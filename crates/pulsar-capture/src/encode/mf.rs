//! The display GPU's own hardware encoder through Media Foundation (AMD VCN, Intel Quick Sync,
//! …) — the in-process counterpart of the NVENC [`Encoder`](super::Encoder), so a hybrid host
//! can MEASURE the iGPU's encoder against NVENC (`path.rs`) instead of only falling back to the
//! ffmpeg CLI. Zero-copy on the capture device:
//!
//! ```text
//!   capture BGRA (DXGI pool texture, display GPU)
//!     │ ID3D11VideoProcessorBlt (BGRA → NV12, scale, rotation) on the capture context
//!     ▼
//!   NV12 texture → MFCreateDXGISurfaceBuffer → IMFSample
//!     │ async hardware encoder MFT (DXGI device manager = the capture device)
//!     │   low-latency CBR, no B-frames, keyframe on demand (CODECAPI_AVEncVideoForceKeyFrame)
//!     ▼
//!   Annex-B access unit (+ parameter sets on keyframes) → RtpEgress (the NVENC path's RTP)
//! ```
//!
//! The async MFT contract: `METransformNeedInput` grants one `ProcessInput`,
//! `METransformHaveOutput` one `ProcessOutput`. A frame waits for its own output (one in, one
//! out in low-latency mode), so latency is the encode time, as with NVENC's synchronous lock.
#![cfg(windows)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
	ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D, ID3D11VideoContext,
	ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
	ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET,
	D3D11_BIND_VIDEO_ENCODER, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC,
	D3D11_USAGE_DEFAULT, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
	D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
	D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
	D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};

use super::cap_dbg;
use crate::rtp::RtpEgress;
use crate::{Codec, Frame};

/// How long one frame may wait for the encoder's input slot / output before the tick errors.
const EVENT_WAIT: Duration = Duration::from_millis(250);

pub struct MfEncoder {
	transform: IMFTransform,
	events: IMFMediaEventGenerator,
	codec_api: ICodecAPI,
	_manager: IMFDXGIDeviceManager,
	device: ID3D11Device,
	context: ID3D11DeviceContext,
	vdevice: ID3D11VideoDevice,
	vctx: ID3D11VideoContext,
	vproc: ID3D11VideoProcessor,
	vp_enum: ID3D11VideoProcessorEnumerator,
	nv12: ID3D11Texture2D,
	out_view: Option<ID3D11VideoProcessorOutputView>,
	in_view: Option<(*mut c_void, ID3D11VideoProcessorInputView)>,
	rotation: u32,
	rotation_set: bool,
	rtp: RtpEgress,
	bitrate_kbps: Arc<AtomicU32>,
	codec: Codec,
	fps: u32,
	/// `METransformNeedInput` / `METransformHaveOutput` events not consumed yet.
	need_input: u32,
	have_output: u32,
	provides_samples: bool,
	out_size: u32,
	/// The parameter sets (Annex-B), prepended to a keyframe that comes without them.
	seq_header: Vec<u8>,
	force_idr: bool,
	frame_idx: u64,
	idr_interval: u64,
	closed: bool,
}

// Lives on the capture thread only (like the NVENC encoder).
unsafe impl Send for MfEncoder {}

impl MfEncoder {
	/// Open the capture adapter's hardware encoder MFT for `p.codec` (H.264 / HEVC). Err when
	/// the adapter has none (or no async hardware one) — the caller tries another path.
	pub unsafe fn new(
		device: &ID3D11Device,
		context: &ID3D11DeviceContext,
		p: &super::EncParams,
	) -> Result<Self, String> {
		let subtype = match p.codec {
			Codec::H264 => MFVideoFormat_H264,
			Codec::H265 => MFVideoFormat_HEVC,
			Codec::Av1 => return Err("mf: AV1 not wired".into()),
		};
		let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
		MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| format!("MFStartup: {e}"))?;

		let (pw, ph) = (p.width.max(2) & !1, p.height.max(2) & !1);
		let (w, h) = if p.rotation == 90 || p.rotation == 270 { (ph, pw) } else { (pw, ph) };
		let cap_w = if p.capture_width >= 2 { p.capture_width & !1 } else { w };
		let cap_h = if p.capture_height >= 2 { p.capture_height & !1 } else { h };
		let fps = p.fps.max(1);

		// 1. The encoder MFT of THIS adapter (a hybrid host lists NVIDIA's too).
		let vendor = adapter_vendor(device).ok_or("mf: capture adapter unknown")?;
		let (transform, name) = find_encoder(subtype, vendor)?;
		cap_dbg(&format!("mf: encoder \"{name}\" vendor=0x{vendor:04X} {w}x{h}@{fps} {:?}", p.codec));
		if let Ok(attrs) = transform.GetAttributes() {
			let _ = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
			let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
		}

		// 2. Share the capture device with it (multithread-protected: the MFT works on it from
		//    its own threads while the capture context records copies).
		if let Ok(mt) = context.cast::<ID3D11Multithread>() {
			let _ = mt.SetMultithreadProtected(true);
		}
		let mut token = 0u32;
		let mut manager: Option<IMFDXGIDeviceManager> = None;
		MFCreateDXGIDeviceManager(&mut token, &mut manager).map_err(|e| format!("MFCreateDXGIDeviceManager: {e}"))?;
		let manager = manager.ok_or("mf: no device manager")?;
		manager.ResetDevice(device, token).map_err(|e| format!("ResetDevice: {e}"))?;
		transform
			.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
			.map_err(|e| format!("SET_D3D_MANAGER: {e}"))?;

		// 3. Rate control before the types (some encoders fix it at SetOutputType).
		let codec_api: ICodecAPI = transform.cast().map_err(|e| format!("ICodecAPI: {e}"))?;
		let bps = p.bitrate_kbps.max(300) * 1000;
		let idr_sec = std::env::var("PULSAR_IDR_SEC").ok().and_then(|s| s.parse::<u32>().ok()).filter(|&v| v > 0).unwrap_or(10);
		let gop = (fps * idr_sec).max(1);
		set_rate_control(&codec_api, bps, fps, gop);

		// 4. Output (codec) then input (NV12) type.
		let out = MFCreateMediaType().map_err(|e| format!("MFCreateMediaType: {e}"))?;
		out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| e.to_string())?;
		out.SetGUID(&MF_MT_SUBTYPE, &subtype).map_err(|e| e.to_string())?;
		out.SetUINT32(&MF_MT_AVG_BITRATE, bps).map_err(|e| e.to_string())?;
		out.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h)).map_err(|e| e.to_string())?;
		out.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1)).map_err(|e| e.to_string())?;
		out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)).map_err(|e| e.to_string())?;
		out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(|e| e.to_string())?;
		// H.264 Main / HEVC Main (8-bit 4:2:0) — what every client decodes.
		let profile = if p.codec == Codec::H264 { 77 } else { 1 };
		let _ = out.SetUINT32(&MF_MT_MPEG2_PROFILE, profile);
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
		set_rate_control(&codec_api, bps, fps, gop);

		let info = transform.GetOutputStreamInfo(0).map_err(|e| format!("GetOutputStreamInfo: {e}"))?;
		let provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
		let events: IMFMediaEventGenerator = transform.cast().map_err(|e| format!("event generator: {e}"))?;
		transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).map_err(|e| e.to_string())?;
		transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).map_err(|e| e.to_string())?;

		// 5. BGRA → NV12 on the capture device.
		let vdevice: ID3D11VideoDevice = device.cast().map_err(|e| format!("mf: no video device ({e}) — capture device lacks VIDEO_SUPPORT"))?;
		let vctx: ID3D11VideoContext = context.cast().map_err(|e| format!("mf: no video context: {e}"))?;
		let (vp_enum, vproc) = super::d3d::build_video_processor(&vdevice, cap_w, cap_h, w, h)?;
		let nv12 = nv12_texture(device, w, h)?;

		let bitrate_kbps = Arc::new(AtomicU32::new(p.bitrate_kbps.max(1)));
		let dest = super::d3d::parse_rtp_dest(&p.dest)?;
		let rtp = RtpEgress::spawn(dest, p.codec, fps, bitrate_kbps.clone()).map_err(|e| format!("rtp: {e}"))?;

		Ok(MfEncoder {
			transform,
			events,
			codec_api,
			_manager: manager,
			device: device.clone(),
			context: context.clone(),
			vdevice,
			vctx,
			vproc,
			vp_enum,
			nv12,
			out_view: None,
			in_view: None,
			rotation: p.rotation,
			rotation_set: false,
			rtp,
			bitrate_kbps,
			codec: p.codec,
			fps,
			need_input: 0,
			have_output: 0,
			provides_samples,
			out_size: info.cbSize.max(1 << 20),
			seq_header: Vec::new(),
			force_idr: true,
			frame_idx: 0,
			idr_interval: gop as u64,
			closed: false,
		})
	}

	pub fn request_idr(&mut self) {
		self.force_idr = true;
	}

	/// Live bitrate change (CBR mean bitrate; a hardware MFT applies it on the next frames).
	pub unsafe fn reconfigure_bitrate(&mut self, kbps: u32) -> Result<(), String> {
		let bps = kbps.max(300) * 1000;
		self.codec_api
			.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &var_u32(bps))
			.map_err(|e| format!("mf bitrate {kbps}: {e}"))?;
		let _ = self.codec_api.SetValue(&CODECAPI_AVEncCommonMaxBitRate, &var_u32(bps));
		let _ = self.codec_api.SetValue(&CODECAPI_AVEncCommonBufferSize, &var_u32(bps / self.fps.max(1)));
		self.bitrate_kbps.store(kbps, Ordering::Relaxed);
		Ok(())
	}

	/// One paced tick: convert, encode, packetize. Waits for this frame's output.
	pub unsafe fn submit(&mut self, frame: &Frame, pts: i64, rtp_ts_90k: u32) -> Result<(), String> {
		if self.closed {
			return Err("submit after close".into());
		}
		self.blt(frame.texture)?;
		// Keyframes: on request, every GOP, and a few in the first second (the client must
		// catch one fast after a (re)start — same policy as the NVENC path).
		let fast = self.frame_idx < self.fps as u64 && self.frame_idx % (self.fps / 4).max(1) as u64 == 0;
		if self.force_idr || fast || self.frame_idx % self.idr_interval.max(1) == 0 {
			let _ = self.codec_api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &var_u32(1));
			self.force_idr = false;
		}
		self.frame_idx += 1;

		self.wait(|s| s.need_input > 0)?;
		let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &self.nv12, 0, false)
			.map_err(|e| format!("MFCreateDXGISurfaceBuffer: {e}"))?;
		let sample = MFCreateSample().map_err(|e| e.to_string())?;
		sample.AddBuffer(&buffer).map_err(|e| e.to_string())?;
		let dur = 10_000_000i64 / self.fps as i64;
		let _ = sample.SetSampleTime(pts * dur);
		let _ = sample.SetSampleDuration(dur);
		self.transform.ProcessInput(0, &sample, 0).map_err(|e| format!("ProcessInput: {e}"))?;
		self.need_input -= 1;

		self.wait(|s| s.have_output > 0)?;
		self.have_output -= 1;
		let (mut au, key) = self.take_output()?;
		if au.is_empty() {
			return Ok(());
		}
		if key && !has_parameter_sets(&au, self.codec) {
			if self.seq_header.is_empty() {
				self.seq_header = self.sequence_header();
			}
			if !self.seq_header.is_empty() {
				let mut v = self.seq_header.clone();
				v.extend_from_slice(&au);
				au = v;
			}
		}
		self.rtp.send_access_unit(&au, rtp_ts_90k);
		Ok(())
	}

	/// No reference-frame invalidation here. The host still advertises RFI (the NVENC path does
	/// it), so an RFI client reports the hole (before its repair) and waits for a recovery
	/// frame; when the repair fails and none comes it asks for a keyframe (`request_idr`).
	/// Forcing one from the report itself cost a keyframe for every hole the repair filled —
	/// ~2 IDR/s on a lossy link (see `policy::RfiPlan`).
	pub unsafe fn invalidate_lost(&mut self, _first: u16, _last: u16) -> Result<usize, &'static str> {
		Err("rfi off (media foundation encoder; the client asks for a keyframe if its repair fails)")
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
		let _ = MFShutdown();
	}

	// ---- internals ---------------------------------------------------------------------

	/// Pump the MFT's events until `ready` holds (or `EVENT_WAIT` runs out).
	unsafe fn wait(&mut self, ready: impl Fn(&Self) -> bool) -> Result<(), String> {
		let deadline = Instant::now() + EVENT_WAIT;
		while !ready(self) {
			match self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
				Ok(ev) => match MF_EVENT_TYPE(ev.GetType().map_err(|e| e.to_string())? as i32) {
					METransformNeedInput => self.need_input += 1,
					METransformHaveOutput => self.have_output += 1,
					_ => {}
				},
				Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
					if Instant::now() >= deadline {
						return Err("mf: encoder event timeout".into());
					}
					std::thread::sleep(Duration::from_micros(100));
				}
				Err(e) => return Err(format!("GetEvent: {e}")),
			}
		}
		Ok(())
	}

	/// `ProcessOutput` → (Annex-B bytes, keyframe).
	unsafe fn take_output(&mut self) -> Result<(Vec<u8>, bool), String> {
		loop {
			let mut data = [MFT_OUTPUT_DATA_BUFFER::default(); 1];
			if !self.provides_samples {
				let s = MFCreateSample().map_err(|e| e.to_string())?;
				let b = MFCreateMemoryBuffer(self.out_size).map_err(|e| e.to_string())?;
				s.AddBuffer(&b).map_err(|e| e.to_string())?;
				data[0].pSample = std::mem::ManuallyDrop::new(Some(s));
			}
			let mut status = 0u32;
			match self.transform.ProcessOutput(0, &mut data, &mut status) {
				Ok(()) => {}
				Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
					// Renegotiate the output type the encoder now proposes, then retry.
					std::mem::ManuallyDrop::drop(&mut data[0].pSample);
					std::mem::ManuallyDrop::drop(&mut data[0].pEvents);
					let t = self.transform.GetOutputAvailableType(0, 0).map_err(|e| e.to_string())?;
					self.transform.SetOutputType(0, &t, 0).map_err(|e| e.to_string())?;
					continue;
				}
				Err(e) => {
					std::mem::ManuallyDrop::drop(&mut data[0].pSample);
					std::mem::ManuallyDrop::drop(&mut data[0].pEvents);
					return Err(format!("ProcessOutput: {e}"));
				}
			}
			let sample = std::mem::ManuallyDrop::take(&mut data[0].pSample);
			std::mem::ManuallyDrop::drop(&mut data[0].pEvents);
			let Some(sample) = sample else { return Ok((Vec::new(), false)) };
			let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).map(|v| v != 0).unwrap_or(false);
			let buf = sample.ConvertToContiguousBuffer().map_err(|e| e.to_string())?;
			let mut ptr: *mut u8 = std::ptr::null_mut();
			let mut len = 0u32;
			buf.Lock(&mut ptr, None, Some(&mut len)).map_err(|e| e.to_string())?;
			let bytes = std::slice::from_raw_parts(ptr, len as usize).to_vec();
			let _ = buf.Unlock();
			return Ok((bytes, key));
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

	/// BGRA capture → our NV12 texture (scale + rotation), on the capture context.
	unsafe fn blt(&mut self, src: &ID3D11Texture2D) -> Result<(), String> {
		if self.out_view.is_none() {
			let d = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
				ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
				Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
			};
			let mut v = None;
			self.vdevice
				.CreateVideoProcessorOutputView(&self.nv12, &self.vp_enum, &d, Some(&mut v))
				.map_err(|e| format!("CreateVideoProcessorOutputView: {e}"))?;
			self.out_view = v;
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
		let out_view = self.out_view.clone().ok_or("no output view")?;
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

/// Low-latency CBR: mean = peak = `bps`, a one-frame buffer (no frame may burst past the
/// link's per-frame budget), no B-frames, `gop`-frame keyframe period.
unsafe fn set_rate_control(api: &ICodecAPI, bps: u32, fps: u32, gop: u32) {
	let _ = api.SetValue(&CODECAPI_AVEncCommonRateControlMode, &var_u32(eAVEncCommonRateControlMode_CBR.0 as u32));
	let _ = api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &var_u32(bps));
	let _ = api.SetValue(&CODECAPI_AVEncCommonMaxBitRate, &var_u32(bps));
	let _ = api.SetValue(&CODECAPI_AVEncCommonBufferSize, &var_u32(bps / fps.max(1)));
	let _ = api.SetValue(&CODECAPI_AVLowLatencyMode, &var_bool(true));
	let _ = api.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &var_u32(0));
	let _ = api.SetValue(&CODECAPI_AVEncMPVGOPSize, &var_u32(gop));
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

/// The first async hardware encoder MFT for `subtype` whose vendor is `vendor` (PCI id).
unsafe fn find_encoder(subtype: windows::core::GUID, vendor: u32) -> Result<(IMFTransform, String), String> {
	let out_info = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: subtype };
	let in_info = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
	let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
	let mut count = 0u32;
	MFTEnumEx(
		MFT_CATEGORY_VIDEO_ENCODER,
		MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
		Some(&in_info),
		Some(&out_info),
		&mut list,
		&mut count,
	)
	.map_err(|e| format!("MFTEnumEx: {e}"))?;
	let want = format!("VEN_{vendor:04X}");
	let mut found: Result<(IMFTransform, String), String> = Err(format!("mf: no {want} hardware encoder"));
	if !list.is_null() {
		for act in std::slice::from_raw_parts(list, count as usize).iter().flatten() {
			let ven = attr_string(act, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute).unwrap_or_default();
			let name = attr_string(act, &MFT_FRIENDLY_NAME_Attribute).unwrap_or_default();
			if found.is_err() && ven.eq_ignore_ascii_case(&want) {
				if let Ok(t) = act.ActivateObject::<IMFTransform>() {
					found = Ok((t, name));
				}
			}
		}
		windows::Win32::System::Com::CoTaskMemFree(Some(list as *const c_void));
	}
	found
}

unsafe fn attr_string(a: &IMFActivate, key: &windows::core::GUID) -> Option<String> {
	let mut p = windows::core::PWSTR::null();
	let mut n = 0u32;
	a.GetAllocatedString(key, &mut p, &mut n).ok()?;
	let s = p.to_string().ok();
	windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const c_void));
	s
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
