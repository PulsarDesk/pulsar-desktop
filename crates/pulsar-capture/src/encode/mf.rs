//! The display GPU's own hardware encoder through Media Foundation (Qualcomm Adreno, AMD VCN,
//! Intel Quick Sync, …) — the in-process counterpart of the NVENC [`Encoder`](super::Encoder).
//! It serves every host without NVENC (the native path's encoder there, see `lib.rs`
//! `NativeEncoder::MediaFoundation`) and is the third measured candidate on a hybrid NVIDIA
//! laptop (`path.rs`). Zero-copy on the capture device:
//!
//! ```text
//!   capture BGRA (DXGI / WGC texture, display GPU)
//!     │ ID3D11VideoProcessorBlt (BGRA → NV12, scale, rotation) on the capture context
//!     ▼
//!   NV12 surface (ring of 3) → MFCreateDXGISurfaceBuffer      (system-memory copy when the
//!     │ async hardware encoder MFT (DXGI device manager)        MFT is not D3D11-aware)
//!     │   CBR, low-latency mode, no B-frames, keyframe on demand, live bitrate
//!     │   (MeanBitRate), long-term references for loss recovery
//!     ▼
//!   output thread: ProcessOutput the moment the MFT has one → Annex-B (+ parameter sets on
//!   keyframes, + recovery tag) → RtpEgress (the NVENC path's RTP)
//! ```
//!
//! The async MFT contract: `METransformNeedInput` grants one `ProcessInput`,
//! `METransformHaveOutput` one `ProcessOutput`. An output thread blocks on the MFT's event
//! queue: it counts the input grants for the capture thread and takes every output as soon as
//! it is announced, so an access unit leaves the moment it is encoded (emit-on-arrival), and
//! the capture thread goes back to the next frame right after `ProcessInput`. A frame never
//! waits for the previous frame's encode unless all surfaces are in flight — an encode that
//! takes longer than a frame interval (the Qualcomm X Elite MFT at 1440p: p50 8-14 ms, p90
//! up to 30 ms, measured) then costs latency, not frame rate.
//!
//! Loss recovery: when the client asked for RFI and the MFT has long-term references
//! (`CODECAPI_AVEncVideoLTRBufferControl`), frames are marked as LTR periodically and a
//! client-reported loss is healed by predicting the next frame only from an intact LTR
//! (`UseLTRFrame`), tagged with the same recovery SEI as NVENC's RFI frame (`ltr.rs`).
//! Verified on the Qualcomm MFT (H.264 + HEVC): a stream with the lost frames and everything
//! up to the tag removed decodes bit-identical to the full stream from the tag on.
//!
//! Knobs: `PULSAR_MF_LTR=0` (no LTR recovery), `PULSAR_MF_QVS=0..100` (QualityVsSpeed),
//! `PULSAR_MF_SYSMEM=1` (force the system-memory input), `PULSAR_MF_SURFACES=1..8`,
//! `PULSAR_MF_RC=n` (rate-control mode, default CBR), `PULSAR_MF_VBV_FRAMES=n`,
//! `PULSAR_MF_SCENARIO=n`, `PULSAR_IDR_SEC`, `PULSAR_DUMP_BITSTREAM`, `PULSAR_MF_TRACE=1`.
#![cfg(windows)]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
	ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D, ID3D11VideoContext,
	ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
	ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET,
	D3D11_BIND_VIDEO_ENCODER, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
	D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
	D3D11_USAGE_STAGING, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
	D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
	D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
	D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};

use super::cap_dbg;
use super::ltr::LtrPlanner;
use super::mf_probe;
use crate::rtp::RtpEgress;
use crate::{Codec, Frame};

/// How long one frame may wait for the encoder's input slot / a free surface before it fails
/// (the host's stall watchdogs take it from there).
const INPUT_WAIT: Duration = Duration::from_millis(250);
/// NV12 surfaces in rotation: a surface is reused only after the encoder gave its output, so
/// this bounds the frames in flight.
const SURFACES: usize = 3;
/// LTR slots used (the Qualcomm MFT has 4; 2 keep the DPB small).
const LTR_SLOTS: u32 = 2;
/// PCI vendor id of NVIDIA: its GPUs encode through NVENC, never this path.
const VENDOR_NVIDIA: u32 = 0x10DE;

/// A frame inside the encoder.
struct Inflight {
	sample_time: i64,
	rtp_ts: u32,
	frame_no: u64,
	/// A recovery frame: the LTR slot bitmap it predicts from (`UseLTRFrame`).
	recovery: Option<u32>,
	t_in: Instant,
}

/// What the capture thread and the output thread share.
#[derive(Default)]
struct State {
	/// `METransformNeedInput` grants not used yet.
	need_input: u32,
	inflight: VecDeque<Inflight>,
	ltr: Option<LtrPlanner>,
	/// The output thread failed (or the MFT reported an error): every later submit fails.
	error: Option<String>,
	/// Input → output time of each frame since the last log line (µs).
	lat_us: Vec<u32>,
	lat_logged: Option<Instant>,
	/// EWMA of the input → output time (µs), for the host's encode stat.
	lat_ewma_us: u32,
	/// Set when the output thread has left.
	done: bool,
}

struct Shared {
	state: Mutex<State>,
	cv: Condvar,
}

/// The output side, owned by the output thread.
struct OutputSide {
	transform: IMFTransform,
	events: IMFMediaEventGenerator,
	rtp: Arc<RtpEgress>,
	codec: Codec,
	provides_samples: bool,
	out_size: u32,
	/// The parameter sets (Annex-B), prepended to a keyframe that comes without them.
	seq_header: Vec<u8>,
	trace: bool,
}

// The MFT is an async MFT: free-threaded by contract (its events arrive on MF work queues and
// the client calls ProcessOutput from whichever thread took the event).
unsafe impl Send for OutputSide {}

impl OutputSide {
	/// Until the MFT shuts down: count input grants, send every output.
	unsafe fn run(mut self, shared: Arc<Shared>) {
		let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
		// Its wake-ups are the access units' send times.
		{
			use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL};
			let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
		}
		loop {
			let ev = match self.events.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)) {
				Ok(ev) => ev,
				// MF_E_SHUTDOWN after `flush_and_close`, or a dead MFT.
				Err(_) => break,
			};
			let r = match MF_EVENT_TYPE(ev.GetType().unwrap_or(0) as i32) {
				METransformNeedInput => {
					shared.state.lock().unwrap().need_input += 1;
					Ok(())
				}
				METransformHaveOutput => self.output(&shared),
				MEError => Err(format!("mf: encoder error event ({:?})", ev.GetStatus().err())),
				_ => Ok(()),
			};
			if let Err(e) = r {
				let mut st = shared.state.lock().unwrap();
				if st.error.is_none() {
					cap_dbg(&e);
					st.error = Some(e);
				}
			}
			shared.cv.notify_all();
		}
		shared.state.lock().unwrap().done = true;
		shared.cv.notify_all();
	}

	/// One announced output → RTP.
	unsafe fn output(&mut self, shared: &Shared) -> Result<(), String> {
		let Some((mut au, key, time)) = self.take_output()? else { return Ok(()) };
		// Its frame (outputs come in input order; an entry before it was dropped inside).
		let meta = {
			let mut st = shared.state.lock().unwrap();
			let found = st.inflight.iter().position(|f| f.sample_time == time);
			if self.trace {
				eprintln!("mf out: time={time} key={key} len={} match={found:?} inflight={}", au.len(), st.inflight.len());
			}
			if st.inflight.is_empty() {
				return Ok(());
			}
			let meta = st.inflight.drain(..=found.unwrap_or(0)).last();
			let Some(meta) = meta else { return Ok(()) };
			if au.is_empty() {
				// Skipped by the rate control (after a big keyframe): no reference, no tag.
				if let Some(ltr) = st.ltr.as_mut() {
					ltr.skipped(meta.frame_no, meta.recovery);
				}
				return Ok(());
			}
			meta
		};
		if key && self.codec != Codec::Av1 && !has_parameter_sets(&au, self.codec) {
			if self.seq_header.is_empty() {
				self.seq_header = self.sequence_header();
			}
			if !self.seq_header.is_empty() {
				let mut v = self.seq_header.clone();
				v.extend_from_slice(&au);
				au = v;
			}
		}
		if meta.recovery.is_some() && !key {
			// The client resumes decoding at this tag (same as NVENC's RFI frame).
			let mut v = super::recovery_sei(self.codec);
			v.extend_from_slice(&au);
			au = v;
		}
		super::submit::dump_bitstream(&au);
		self.rtp.send_access_unit(&au, meta.rtp_ts);
		let us = meta.t_in.elapsed().as_micros().min(u32::MAX as u128) as u32;
		let mut st = shared.state.lock().unwrap();
		if let Some(ltr) = st.ltr.as_mut() {
			ltr.sent(meta.frame_no, meta.rtp_ts);
		}
		st.lat_ewma_us = if st.lat_ewma_us == 0 { us } else { st.lat_ewma_us / 8 * 7 + us / 8 };
		st.lat_us.push(us);
		let due = st.lat_logged.is_none_or(|t| t.elapsed() >= Duration::from_secs(1));
		if due {
			let inflight = st.inflight.len();
			let mut v = std::mem::take(&mut st.lat_us);
			st.lat_logged = Some(Instant::now());
			drop(st);
			v.sort_unstable();
			let p = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
			cap_dbg(&format!(
				"mf: encode latency p50 {} us p90 {} us max {} us over {} frames, in flight {inflight}",
				p(0.5),
				p(0.9),
				p(1.0),
				v.len()
			));
		}
		Ok(())
	}

	/// `ProcessOutput` → (Annex-B bytes, keyframe, sample time).
	unsafe fn take_output(&mut self) -> Result<Option<(Vec<u8>, bool, i64)>, String> {
		loop {
			let mut data = [MFT_OUTPUT_DATA_BUFFER::default(); 1];
			if !self.provides_samples {
				let s = MFCreateSample().map_err(|e| e.to_string())?;
				let b = MFCreateMemoryBuffer(self.out_size).map_err(|e| e.to_string())?;
				s.AddBuffer(&b).map_err(|e| e.to_string())?;
				data[0].pSample = std::mem::ManuallyDrop::new(Some(s));
			}
			let mut status = 0u32;
			let r = self.transform.ProcessOutput(0, &mut data, &mut status);
			let sample = std::mem::ManuallyDrop::take(&mut data[0].pSample);
			std::mem::ManuallyDrop::drop(&mut data[0].pEvents);
			match r {
				Ok(()) => {}
				Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
					// Renegotiate the output type the encoder now proposes, then retry.
					let t = self.transform.GetOutputAvailableType(0, 0).map_err(|e| e.to_string())?;
					self.transform.SetOutputType(0, &t, 0).map_err(|e| e.to_string())?;
					self.seq_header.clear();
					continue;
				}
				Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
				Err(e) => return Err(format!("ProcessOutput: {e}")),
			}
			let Some(sample) = sample else { return Ok(None) };
			let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).map(|v| v != 0).unwrap_or(false);
			let time = sample.GetSampleTime().unwrap_or(i64::MIN);
			let buf = sample.ConvertToContiguousBuffer().map_err(|e| e.to_string())?;
			let mut ptr: *mut u8 = std::ptr::null_mut();
			let mut len = 0u32;
			buf.Lock(&mut ptr, None, Some(&mut len)).map_err(|e| e.to_string())?;
			let bytes = std::slice::from_raw_parts(ptr, len as usize).to_vec();
			let _ = buf.Unlock();
			return Ok(Some((bytes, key, time)));
		}
	}

	/// The encoder's parameter sets (Annex-B) from its current output type.
	unsafe fn sequence_header(&self) -> Vec<u8> {
		let Ok(t) = self.transform.GetOutputCurrentType(0) else { return Vec::new() };
		let mut p: *mut u8 = std::ptr::null_mut();
		let mut n = 0u32;
		if t.GetAllocatedBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut p, &mut n).is_err() || p.is_null() {
			return Vec::new();
		}
		let v = std::slice::from_raw_parts(p, n as usize).to_vec();
		windows::Win32::System::Com::CoTaskMemFree(Some(p as *const c_void));
		v
	}
}

/// How frames reach the MFT.
enum InputMode {
	/// The NV12 texture itself (`MFCreateDXGISurfaceBuffer`).
	Dxgi,
	/// Read back into system memory (an MFT that is not D3D11-aware).
	SysMem(ID3D11Texture2D),
}

pub struct MfEncoder {
	transform: IMFTransform,
	codec_api: ICodecAPI,
	_manager: Option<IMFDXGIDeviceManager>,
	shared: Arc<Shared>,
	out_thread: Option<std::thread::JoinHandle<()>>,
	device: ID3D11Device,
	context: ID3D11DeviceContext,
	vdevice: ID3D11VideoDevice,
	vctx: ID3D11VideoContext,
	vproc: ID3D11VideoProcessor,
	vp_enum: ID3D11VideoProcessorEnumerator,
	surfaces: Vec<(ID3D11Texture2D, Option<ID3D11VideoProcessorOutputView>)>,
	next_surface: usize,
	in_view: Option<(*mut c_void, ID3D11VideoProcessorInputView)>,
	input: InputMode,
	width: u32,
	height: u32,
	rotation: u32,
	rotation_set: bool,
	rtp: Arc<RtpEgress>,
	bitrate_kbps: Arc<AtomicU32>,
	fps: u32,
	vbv_frames: u32,
	force_idr: bool,
	frame_idx: u64,
	idr_interval: u64,
	name: String,
	closed: bool,
}

// Lives on the capture thread only (like the NVENC encoder).
unsafe impl Send for MfEncoder {}

impl MfEncoder {
	/// Open the capture adapter's hardware encoder MFT for `p.codec`. Err when the adapter has
	/// none (or no async hardware one), or is NVIDIA's (NVENC's job) — the caller tries another
	/// path.
	pub unsafe fn new(
		device: &ID3D11Device,
		context: &ID3D11DeviceContext,
		p: &super::EncParams,
	) -> Result<Self, String> {
		// 1. The encoder MFT of THIS adapter (a hybrid host lists NVIDIA's too).
		let vendor = adapter_vendor(device).ok_or("mf: capture adapter unknown")?;
		if vendor == VENDOR_NVIDIA {
			// A wedged NVIDIA driver hangs its MFT exactly like NVENC (2026-11-13), and NVENC is
			// the better encoder there anyway.
			return Err("mf: the capture GPU is NVIDIA's (NVENC path)".into());
		}
		let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
		MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| format!("MFStartup: {e}"))?;
		// Undo the startup on any error below (MFStartup / MFShutdown are counted).
		struct StartGuard(bool);
		impl Drop for StartGuard {
			fn drop(&mut self) {
				if self.0 {
					unsafe {
						let _ = MFShutdown();
					}
				}
			}
		}
		let mut started = StartGuard(true);

		let (pw, ph) = (p.width.max(2) & !1, p.height.max(2) & !1);
		let (w, h) = if p.rotation == 90 || p.rotation == 270 { (ph, pw) } else { (pw, ph) };
		let cap_w = if p.capture_width >= 2 { p.capture_width & !1 } else { w };
		let cap_h = if p.capture_height >= 2 { p.capture_height & !1 } else { h };
		let fps = p.fps.max(1);
		let subtype = mf_probe::subtype(p.codec);

		let only_gpu = mf_probe::hardware_vendors().len() == 1;
		let mft = mf_probe::mft_for_vendor(mf_probe::hardware_mfts(p.codec), vendor, only_gpu)
			.ok_or_else(|| format!("mf: no hardware {:?} encoder for adapter vendor 0x{vendor:04X}", p.codec))?;
		let transform: IMFTransform = mft.activate.ActivateObject().map_err(|e| format!("mf activate: {e}"))?;
		let name = mft.name.clone();
		let attrs = transform.GetAttributes().ok();
		let d3d11_aware = attrs.as_ref().and_then(|a| a.GetUINT32(&MF_SA_D3D11_AWARE).ok()).unwrap_or(0) != 0;
		if let Some(a) = attrs.as_ref() {
			let _ = a.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
			let _ = a.SetUINT32(&MF_LOW_LATENCY, 1);
		}
		let sysmem = !d3d11_aware || env_flag("PULSAR_MF_SYSMEM");

		// 2. Share the capture device with it (multithread-protected: the MFT works on it from
		//    its own threads while the capture context records the colour conversion).
		if let Ok(mt) = context.cast::<ID3D11Multithread>() {
			let _ = mt.SetMultithreadProtected(true);
		}
		let manager = if sysmem {
			None
		} else {
			let mut token = 0u32;
			let mut manager: Option<IMFDXGIDeviceManager> = None;
			MFCreateDXGIDeviceManager(&mut token, &mut manager).map_err(|e| format!("MFCreateDXGIDeviceManager: {e}"))?;
			let manager = manager.ok_or("mf: no device manager")?;
			manager.ResetDevice(device, token).map_err(|e| format!("ResetDevice: {e}"))?;
			transform
				.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
				.map_err(|e| format!("SET_D3D_MANAGER: {e}"))?;
			Some(manager)
		};

		// 3. Rate control + latency knobs before the types (some encoders fix them at
		//    SetOutputType), long-term references for loss recovery.
		let codec_api: ICodecAPI = transform.cast().map_err(|e| format!("ICodecAPI: {e}"))?;
		let bps = p.bitrate_kbps.max(300) * 1000;
		let idr_sec = std::env::var("PULSAR_IDR_SEC").ok().and_then(|s| s.parse::<u32>().ok()).filter(|&v| v > 0).unwrap_or(10);
		let gop = (fps * idr_sec).max(1);
		let vbv_frames = std::env::var("PULSAR_MF_VBV_FRAMES").ok().and_then(|s| s.parse::<u32>().ok()).filter(|&v| v > 0).unwrap_or(1);
		set_rate_control(&codec_api, bps, fps, gop, vbv_frames, p.low_latency);
		let ltr_wanted = p.rfi && p.codec != Codec::Av1 && std::env::var("PULSAR_MF_LTR").map(|v| v != "0").unwrap_or(true);
		let ltr_ok = ltr_wanted
			&& codec_api.IsSupported(&CODECAPI_AVEncVideoLTRBufferControl).is_ok()
			&& codec_api.IsSupported(&CODECAPI_AVEncVideoUseLTRFrame).is_ok()
			&& codec_api.SetValue(&CODECAPI_AVEncVideoLTRBufferControl, &var_u32((1 << 16) | LTR_SLOTS)).is_ok();

		// 4. Output (codec) then input (NV12) type.
		let out = MFCreateMediaType().map_err(|e| format!("MFCreateMediaType: {e}"))?;
		out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| e.to_string())?;
		out.SetGUID(&MF_MT_SUBTYPE, &subtype).map_err(|e| e.to_string())?;
		out.SetUINT32(&MF_MT_AVG_BITRATE, bps).map_err(|e| e.to_string())?;
		out.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h)).map_err(|e| e.to_string())?;
		out.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1)).map_err(|e| e.to_string())?;
		out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)).map_err(|e| e.to_string())?;
		out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(|e| e.to_string())?;
		// H.264 Main / HEVC Main (8-bit 4:2:0) — what every client decodes. AV1: the MFT's
		// default (Main).
		match p.codec {
			Codec::H264 => {
				let _ = out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32);
			}
			Codec::H265 => {
				let _ = out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH265VProfile_Main_420_8.0 as u32);
			}
			Codec::Av1 => {}
		}
		transform.SetOutputType(0, &out, 0).map_err(|e| format!("SetOutputType: {e}"))?;
		let inp = MFCreateMediaType().map_err(|e| e.to_string())?;
		inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| e.to_string())?;
		inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).map_err(|e| e.to_string())?;
		inp.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h)).map_err(|e| e.to_string())?;
		inp.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1)).map_err(|e| e.to_string())?;
		inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(|e| e.to_string())?;
		transform.SetInputType(0, &inp, 0).map_err(|e| format!("SetInputType: {e}"))?;
		// Again with the types set: some encoders (AMD's HEVC one) reset the rate control when
		// the output type lands and otherwise run at their own default rate (measured: 60 Mbit
		// for a 20 Mbit request).
		set_rate_control(&codec_api, bps, fps, gop, vbv_frames, p.low_latency);

		// 5. BGRA → NV12 on the capture device.
		let vdevice: ID3D11VideoDevice = device.cast().map_err(|e| format!("mf: no video device ({e}) — capture device lacks VIDEO_SUPPORT"))?;
		let vctx: ID3D11VideoContext = context.cast().map_err(|e| format!("mf: no video context: {e}"))?;
		let (vp_enum, vproc) = super::d3d::build_video_processor(&vdevice, cap_w, cap_h, w, h)?;
		let nsurf = std::env::var("PULSAR_MF_SURFACES").ok().and_then(|s| s.parse::<usize>().ok()).filter(|n| (1..=8).contains(n)).unwrap_or(SURFACES);
		let mut surfaces = Vec::with_capacity(nsurf);
		for _ in 0..if sysmem { 1 } else { nsurf } {
			surfaces.push((nv12_texture(device, w, h)?, None));
		}
		let input = if sysmem { InputMode::SysMem(staging_texture(device, w, h)?) } else { InputMode::Dxgi };

		let bitrate_kbps = Arc::new(AtomicU32::new(p.bitrate_kbps.max(1)));
		let dest = super::d3d::parse_rtp_dest(&p.dest)?;
		let rtp = Arc::new(RtpEgress::spawn(dest, p.codec, fps, bitrate_kbps.clone()).map_err(|e| format!("rtp: {e}"))?);

		// Long-term references: mark one every ~quarter second (a recovery frame then
		// predicts from at most that far back).
		let ltr = ltr_ok.then(|| LtrPlanner::new(LTR_SLOTS as usize, (fps / 4).max(2) as u64));
		let ltr_on = ltr.is_some();
		let shared = Arc::new(Shared { state: Mutex::new(State { ltr, ..Default::default() }), cv: Condvar::new() });

		// 6. The output thread, then start streaming.
		let info = transform.GetOutputStreamInfo(0).map_err(|e| format!("GetOutputStreamInfo: {e}"))?;
		let side = OutputSide {
			transform: transform.clone(),
			events: transform.cast().map_err(|e| format!("event generator: {e}"))?,
			rtp: rtp.clone(),
			codec: p.codec,
			provides_samples: info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0,
			out_size: info.cbSize.max(1 << 20),
			seq_header: Vec::new(),
			trace: std::env::var_os("PULSAR_MF_TRACE").is_some(),
		};
		let sh = shared.clone();
		let out_thread = std::thread::Builder::new()
			.name("pulsar-mf-output".into())
			.spawn(move || unsafe { side.run(sh) })
			.map_err(|e| format!("mf output thread: {e}"))?;
		let enc = MfEncoder {
			transform,
			codec_api,
			_manager: manager,
			shared,
			out_thread: Some(out_thread),
			device: device.clone(),
			context: context.clone(),
			vdevice,
			vctx,
			vproc,
			vp_enum,
			surfaces,
			next_surface: 0,
			in_view: None,
			input,
			width: w,
			height: h,
			rotation: p.rotation,
			rotation_set: false,
			rtp,
			bitrate_kbps,
			fps,
			vbv_frames,
			force_idr: true,
			frame_idx: 0,
			idr_interval: gop as u64,
			name,
			closed: false,
		};
		// From here `enc`'s Drop shuts the MFT down (and joins the thread) on an error.
		started.0 = false;
		enc.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).map_err(|e| e.to_string())?;
		enc.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).map_err(|e| e.to_string())?;
		cap_dbg(&format!(
			"mf: encoder \"{}\" vendor=0x{vendor:04X} {w}x{h}@{fps} {:?} {} kbps input={} ltr={ltr_on} (asked rfi={})",
			enc.name,
			p.codec,
			bps / 1000,
			if sysmem { "sysmem" } else { "dxgi" },
			p.rfi
		));
		Ok(enc)
	}

	/// The MFT's friendly name (e.g. "QCOM Hardware Encoder - H264").
	pub fn name(&self) -> &str {
		&self.name
	}

	/// The RTP sequence numbers frame `ts` went out on (benchmarks).
	pub(crate) fn rtp_seq_range_of(&self, ts: u32) -> Option<(u16, u16)> {
		self.rtp.seq_range_of(ts)
	}

	/// Smoothed input → output time of the encoder (µs), 0 before the first output. `submit`
	/// returns right after handing the frame over, so its own duration is not the encode time.
	pub fn encode_latency_us(&self) -> u32 {
		self.shared.state.lock().unwrap().lat_ewma_us
	}

	pub fn request_idr(&mut self) {
		self.force_idr = true;
	}

	/// Live bitrate change: mean (= peak in CBR) and the VBV; the MFT applies it from the next
	/// frame — no restart, no keyframe (measured on the Qualcomm MFT: the new rate holds within
	/// the next 500 ms window).
	pub unsafe fn reconfigure_bitrate(&mut self, kbps: u32) -> Result<(), String> {
		let bps = kbps.max(300) * 1000;
		self.codec_api
			.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &var_u32(bps))
			.map_err(|e| format!("mf bitrate {kbps}: {e}"))?;
		let _ = self.codec_api.SetValue(&CODECAPI_AVEncCommonMaxBitRate, &var_u32(bps));
		let _ = self.codec_api.SetValue(&CODECAPI_AVEncCommonBufferSize, &var_u32(vbv_bits(bps, self.fps, self.vbv_frames)));
		self.bitrate_kbps.store(kbps, Ordering::Relaxed);
		Ok(())
	}

	/// One tick: convert and hand the frame to the encoder. Its access unit leaves from the
	/// output thread as soon as it is encoded. Waits only for an input grant and a free surface.
	pub unsafe fn submit(&mut self, frame: &Frame, pts: i64, rtp_ts_90k: u32) -> Result<(), String> {
		if self.closed {
			return Err("submit after close".into());
		}
		// Keyframes: on request, every GOP, and one more half a second in (a client that missed
		// the first one catches it without asking). NVENC sends four in the first second; an MF
		// encoder's keyframe is ~13 frames' budget at 1080p (215 KB at 8 Mbit/s, Qualcomm HEVC)
		// and its CBR then skipped every other frame for most of a second (measured) — the
		// client's keyframe request answers on the very next frame here anyway.
		let n = self.frame_idx;
		let fast = n == (self.fps / 2).max(1) as u64;
		let key = self.force_idr || fast || n % self.idr_interval.max(1) == 0;

		// An input grant and a free surface (all in flight = the encoder is behind).
		let cap = self.surfaces.len();
		let ltr_ops = {
			let deadline = Instant::now() + INPUT_WAIT;
			let mut st = self.shared.state.lock().unwrap();
			loop {
				if let Some(e) = st.error.as_ref() {
					return Err(e.clone());
				}
				if st.done {
					return Err("mf: encoder output thread ended".into());
				}
				if st.need_input > 0 && st.inflight.len() < cap {
					break;
				}
				let left = deadline.saturating_duration_since(Instant::now());
				if left.is_zero() {
					return Err(format!(
						"mf: no encoder input slot for {} ms ({} frames in flight)",
						INPUT_WAIT.as_millis(),
						st.inflight.len()
					));
				}
				st = self.shared.cv.wait_timeout(st, left).unwrap().0;
			}
			st.need_input -= 1;
			st.ltr.as_mut().map(|l| l.before_encode(n, key)).unwrap_or_default()
		};

		let t_blt = Instant::now();
		let surface = self.next_surface;
		self.next_surface = (self.next_surface + 1) % self.surfaces.len();
		self.blt(frame.texture, surface)?;
		let buffer = self.input_buffer(surface)?;
		let blt_ms = t_blt.elapsed().as_millis();

		if key {
			let _ = self.codec_api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &var_u32(1));
			self.force_idr = false;
		}
		if let Some(slot) = ltr_ops.mark {
			let _ = self.codec_api.SetValue(&CODECAPI_AVEncVideoMarkLTRFrame, &var_u32(slot));
		}
		let mut recovery = None;
		if let Some(bitmap) = ltr_ops.use_bitmap {
			// Mode 1: predict this frame only from these LTR frames (later frames do not reach
			// back past it).
			if self.codec_api.SetValue(&CODECAPI_AVEncVideoUseLTRFrame, &var_u32((1 << 16) | bitmap)).is_ok() {
				recovery = Some(bitmap);
			}
		}
		self.frame_idx += 1;

		let sample = MFCreateSample().map_err(|e| e.to_string())?;
		sample.AddBuffer(&buffer).map_err(|e| e.to_string())?;
		let dur = 10_000_000i64 / self.fps as i64;
		let time = pts * dur;
		let _ = sample.SetSampleTime(time);
		let _ = sample.SetSampleDuration(dur);
		// In flight before ProcessInput: the output can be announced before it returns.
		self.shared.state.lock().unwrap().inflight.push_back(Inflight {
			sample_time: time,
			rtp_ts: rtp_ts_90k,
			frame_no: n,
			recovery,
			t_in: Instant::now(),
		});
		let t_in = Instant::now();
		if let Err(e) = self.transform.ProcessInput(0, &sample, 0) {
			let mut st = self.shared.state.lock().unwrap();
			if let Some(i) = st.inflight.iter().position(|f| f.frame_no == n) {
				st.inflight.remove(i);
			}
			return Err(format!("ProcessInput: {e}"));
		}
		let in_ms = t_in.elapsed().as_millis();
		if blt_ms + in_ms >= 100 {
			cap_dbg(&format!("mf: slow call — convert+input buffer {blt_ms} ms, ProcessInput {in_ms} ms"));
		}
		Ok(())
	}

	/// The client lost the packets `first..=last` (reported at once, while its NACK repair may
	/// still fill the hole). With long-term references the next frame predicts only from an
	/// intact LTR older than the loss and carries the recovery tag; without (or when no LTR is
	/// intact) nothing is forced — the client asks for a keyframe if its repair fails, as with
	/// NVENC (`policy::RfiPlan`). `Ok(1)` = a recovery frame is planned.
	pub unsafe fn invalidate_lost(&mut self, first: u16, last: u16) -> Result<usize, &'static str> {
		let lost = self.rtp.frames_in_seq_range(first, last);
		let mut st = self.shared.state.lock().unwrap();
		let Some(ltr) = st.ltr.as_mut() else {
			return Err("rfi off (media foundation encoder without long-term references; the client asks for a keyframe if its repair fails)");
		};
		ltr.on_loss(&lost).map(|_| 1)
	}

	pub unsafe fn flush_and_close(&mut self) {
		if self.closed {
			return;
		}
		self.closed = true;
		let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
		let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
		if let Ok(sd) = self.transform.cast::<IMFShutdown>() {
			let _ = sd.Shutdown();
		}
		// The output thread leaves on the shutdown (GetEvent fails). Bounded: one stuck in the
		// driver is detached rather than holding the capture thread.
		if let Some(t) = self.out_thread.take() {
			let t0 = Instant::now();
			while !t.is_finished() && t0.elapsed() < Duration::from_millis(500) {
				std::thread::sleep(Duration::from_millis(2));
			}
			if t.is_finished() {
				let _ = t.join();
				let _ = MFShutdown();
			}
			// else: its MFT reference keeps Media Foundation up until it returns.
		}
	}

	// ---- internals ---------------------------------------------------------------------

	/// The input buffer for surface `i`: the texture itself, or its system-memory copy.
	unsafe fn input_buffer(&mut self, i: usize) -> Result<IMFMediaBuffer, String> {
		let tex = &self.surfaces[i].0;
		match &self.input {
			InputMode::Dxgi => MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)
				.map_err(|e| format!("MFCreateDXGISurfaceBuffer: {e}")),
			InputMode::SysMem(staging) => {
				self.context.CopyResource(staging, tex);
				let mut m = D3D11_MAPPED_SUBRESOURCE::default();
				self.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut m)).map_err(|e| format!("Map: {e}"))?;
				let (w, h) = (self.width as usize, self.height as usize);
				let size = w * h * 3 / 2;
				let r = (|| -> Result<IMFMediaBuffer, String> {
					let b = MFCreateMemoryBuffer(size as u32).map_err(|e| e.to_string())?;
					let mut dst: *mut u8 = std::ptr::null_mut();
					b.Lock(&mut dst, None, None).map_err(|e| e.to_string())?;
					let src = m.pData as *const u8;
					let pitch = m.RowPitch as usize;
					// Y plane, then the interleaved UV plane (it starts `h` rows down).
					for row in 0..h + h / 2 {
						std::ptr::copy_nonoverlapping(src.add(row * pitch), dst.add(row * w), w);
					}
					let _ = b.Unlock();
					b.SetCurrentLength(size as u32).map_err(|e| e.to_string())?;
					Ok(b)
				})();
				self.context.Unmap(staging, 0);
				r
			}
		}
	}

	/// BGRA capture → NV12 surface `i` (scale + rotation), on the capture context.
	unsafe fn blt(&mut self, src: &ID3D11Texture2D, i: usize) -> Result<(), String> {
		if self.surfaces[i].1.is_none() {
			let d = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
				ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
				Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
			};
			let mut v = None;
			self.vdevice
				.CreateVideoProcessorOutputView(&self.surfaces[i].0, &self.vp_enum, &d, Some(&mut v))
				.map_err(|e| format!("CreateVideoProcessorOutputView: {e}"))?;
			self.surfaces[i].1 = v;
		}
		let raw = src.as_raw();
		if self.in_view.as_ref().map(|(k, _)| *k) != Some(raw) {
			let d = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
				FourCC: 0,
				ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
				Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
			};
			let mut v = None;
			self.vdevice
				.CreateVideoProcessorInputView(src, &self.vp_enum, &d, Some(&mut v))
				.map_err(|e| format!("CreateVideoProcessorInputView: {e}"))?;
			self.in_view = v.map(|v| (raw, v));
		}
		if !self.rotation_set {
			use windows::Win32::Graphics::Direct3D11::{
				D3D11_VIDEO_PROCESSOR_ROTATION_180, D3D11_VIDEO_PROCESSOR_ROTATION_270,
				D3D11_VIDEO_PROCESSOR_ROTATION_90, D3D11_VIDEO_PROCESSOR_ROTATION_IDENTITY,
			};
			let rot = match self.rotation {
				90 => D3D11_VIDEO_PROCESSOR_ROTATION_90,
				180 => D3D11_VIDEO_PROCESSOR_ROTATION_180,
				270 => D3D11_VIDEO_PROCESSOR_ROTATION_270,
				_ => D3D11_VIDEO_PROCESSOR_ROTATION_IDENTITY,
			};
			self.vctx.VideoProcessorSetStreamRotation(&self.vproc, 0, true, rot);
			self.rotation_set = true;
		}
		let out_view = self.surfaces[i].1.clone().ok_or("no output view")?;
		let in_view = self.in_view.as_ref().map(|(_, v)| v.clone()).ok_or("no input view")?;
		let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
			Enable: true.into(),
			pInputSurface: std::mem::ManuallyDrop::new(Some(in_view)),
			..Default::default()
		};
		let r = self.vctx.VideoProcessorBlt(&self.vproc, &out_view, 0, std::slice::from_ref(&stream));
		std::mem::ManuallyDrop::drop(&mut stream.pInputSurface);
		r.map_err(|e| format!("VideoProcessorBlt: {e}"))
	}
}

impl Drop for MfEncoder {
	fn drop(&mut self) {
		unsafe { self.flush_and_close() };
		let _ = (&self.device, &self.context);
	}
}

/// VBV size in bits: `frames` frames of the mean rate.
fn vbv_bits(bps: u32, fps: u32, frames: u32) -> u32 {
	(bps / fps.max(1)).saturating_mul(frames.max(1))
}

/// Low-latency CBR: mean = peak = `bps`, a `vbv_frames`-frame buffer (no frame may burst past
/// the link's per-frame budget), no B-frames, `gop`-frame keyframe period, low-latency mode.
unsafe fn set_rate_control(api: &ICodecAPI, bps: u32, fps: u32, gop: u32, vbv_frames: u32, low_latency: bool) {
	let rc = std::env::var("PULSAR_MF_RC").ok().and_then(|s| s.parse::<u32>().ok()).unwrap_or(eAVEncCommonRateControlMode_CBR.0 as u32);
	let _ = api.SetValue(&CODECAPI_AVEncCommonRateControlMode, &var_u32(rc));
	let _ = api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &var_u32(bps));
	let _ = api.SetValue(&CODECAPI_AVEncCommonMaxBitRate, &var_u32(bps));
	let _ = api.SetValue(&CODECAPI_AVEncCommonBufferSize, &var_u32(vbv_bits(bps, fps, vbv_frames)));
	let _ = api.SetValue(&CODECAPI_AVLowLatencyMode, &var_bool(true));
	let _ = api.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &var_u32(0));
	let _ = api.SetValue(&CODECAPI_AVEncMPVGOPSize, &var_u32(gop));
	// Speed over quality only where it is asked for (game mode); the remote desktop keeps the
	// encoder's own balance. PULSAR_MF_QVS overrides.
	let qvs = std::env::var("PULSAR_MF_QVS").ok().and_then(|s| s.parse::<u32>().ok()).map(|v| v.min(100));
	if let Some(q) = qvs.or(low_latency.then_some(0)) {
		let _ = api.SetValue(&CODECAPI_AVEncCommonQualityVsSpeed, &var_u32(q));
	}
	if let Some(sc) = std::env::var("PULSAR_MF_SCENARIO").ok().and_then(|s| s.parse::<u32>().ok()) {
		let _ = api.SetValue(&CODECAPI_AVScenarioInfo, &var_u32(sc));
	}
}

fn env_flag(name: &str) -> bool {
	std::env::var(name).map(|v| v == "1").unwrap_or(false)
}

fn pack(hi: u32, lo: u32) -> u64 {
	((hi as u64) << 32) | lo as u64
}

fn var_u32(v: u32) -> VARIANT {
	let mut var = VARIANT::default();
	unsafe {
		let inner = &mut *var.Anonymous.Anonymous;
		inner.vt = VT_UI4;
		inner.Anonymous.ulVal = v;
	}
	var
}

fn var_bool(v: bool) -> VARIANT {
	let mut var = VARIANT::default();
	unsafe {
		let inner = &mut *var.Anonymous.Anonymous;
		inner.vt = VT_BOOL;
		inner.Anonymous.boolVal = windows::Win32::Foundation::VARIANT_BOOL(if v { -1 } else { 0 });
	}
	var
}

unsafe fn adapter_vendor(device: &ID3D11Device) -> Option<u32> {
	let d: IDXGIDevice = device.cast().ok()?;
	Some(d.GetAdapter().ok()?.GetDesc().ok()?.VendorId)
}

/// The encoder's NV12 input: a render target for the VideoProcessor, and (when the driver allows
/// it) an encoder input.
unsafe fn nv12_texture(device: &ID3D11Device, w: u32, h: u32) -> Result<ID3D11Texture2D, String> {
	for bind in [D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0, D3D11_BIND_RENDER_TARGET.0] {
		let desc = D3D11_TEXTURE2D_DESC {
			Width: w,
			Height: h,
			MipLevels: 1,
			ArraySize: 1,
			Format: DXGI_FORMAT_NV12,
			SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
			Usage: D3D11_USAGE_DEFAULT,
			BindFlags: bind as u32,
			CPUAccessFlags: 0,
			MiscFlags: 0,
		};
		let mut t = None;
		if device.CreateTexture2D(&desc, None, Some(&mut t)).is_ok() {
			if let Some(t) = t {
				return Ok(t);
			}
		}
	}
	Err("mf: NV12 texture".into())
}

/// CPU-readable NV12 copy for an MFT that only takes system memory.
unsafe fn staging_texture(device: &ID3D11Device, w: u32, h: u32) -> Result<ID3D11Texture2D, String> {
	let desc = D3D11_TEXTURE2D_DESC {
		Width: w,
		Height: h,
		MipLevels: 1,
		ArraySize: 1,
		Format: DXGI_FORMAT_NV12,
		SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
		Usage: D3D11_USAGE_STAGING,
		BindFlags: 0,
		CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
		MiscFlags: 0,
	};
	let mut t = None;
	device.CreateTexture2D(&desc, None, Some(&mut t)).map_err(|e| format!("mf: NV12 staging: {e}"))?;
	t.ok_or_else(|| "mf: NV12 staging".into())
}

/// The access unit already carries its parameter sets (H.264 SPS / HEVC VPS or SPS).
fn has_parameter_sets(au: &[u8], codec: Codec) -> bool {
	let mut i = 0;
	while i + 3 < au.len() {
		if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
			let b = au[i + 3];
			let hit = match codec {
				Codec::H264 => b & 0x1f == 7,
				_ => matches!((b >> 1) & 0x3f, 32 | 33),
			};
			if hit {
				return true;
			}
			i += 3;
		} else {
			i += 1;
		}
	}
	false
}
