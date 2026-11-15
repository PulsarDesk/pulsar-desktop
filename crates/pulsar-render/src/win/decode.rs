//! Media Foundation DXVA decode: Annex-B access units → NV12 `ID3D11Texture2D` on our D3D11
//! device (zero-copy, no GPU→CPU download). A 4:4:4 stream (HEVC RExt, asked for only when the
//! probe below really decoded the 4:4:4 fixture here) comes out as AYUV instead: the decoder
//! offers no NV12 for it, and the present VideoProcessor takes AYUV the same way.
//!
//! Call sequence (the canonical MF async/hardware decoder dance, kept synchronous here):
//!   MFStartup (once, guarded by Once)
//!   MFTEnumEx(MFT_CATEGORY_VIDEO_DECODER, HARDWARE) filtered by input subtype  → IMFActivate
//!     → IMFTransform   (fall back to SOFTWARE if no HW MFT is registered)
//!   MFCreateDXGIDeviceManager + ResetDevice(our ID3D11Device)   (D3D11 must be MT-protected:
//!     ID3D11Multithread::SetMultithreadProtected(true) on the immediate context)
//!   IMFTransform::ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager)         (zero-copy bind)
//!   SetInputType(MFVideoFormat_{H264,HEVC,AV1} + frame size/rate/progressive)
//!   GetOutputAvailableType loop → pick MFVideoFormat_NV12 (8-bit) or MFVideoFormat_P010
//!     (a 10-bit stream: HEVC Main10 / AV1 10-bit — HDR10 from a host's HDR monitor; the
//!     decoder lists P010 ahead of NV12 for those), else AYUV for 4:4:4 → SetOutputType
//!   ProcessMessage(NOTIFY_BEGIN_STREAMING) + ProcessMessage(NOTIFY_START_OF_STREAM)
//!   per AU: MFCreateSample(MFCreateMemoryBuffer(copy bytes)) → ProcessInput
//!           → ProcessOutput loop:
//!               MF_E_TRANSFORM_NEED_MORE_INPUT → return what we have
//!               MF_E_TRANSFORM_STREAM_CHANGE   → re-pick NV12 SetOutputType, retry
//!               sample → IMFDXGIBuffer::GetResource(ID3D11Texture2D) + GetSubresourceIndex
//!                       → CopyResource the slice into a fresh single-slice NV12 texture
//!                         (SHADER_RESOURCE|RENDER_TARGET) so the VideoProcessor can read it.

#![allow(dead_code)]

use crate::stream::{AccessUnit, Codec};
use std::sync::Once;
use windows::core::{Interface, Result, GUID};
use windows::Win32::Graphics::Direct3D11::{
	ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
	D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
	D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
	DXGI_FORMAT, DXGI_FORMAT_AYUV, DXGI_FORMAT_NV12, DXGI_FORMAT_P010,
};
use windows::Win32::Media::MediaFoundation::{
	IMFActivate, IMFDXGIBuffer, IMFDXGIDeviceManager, IMFSample, IMFTransform,
	MFCreateDXGIDeviceManager, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
	MFMediaType_Video, MFStartup, MFTEnumEx, MFVideoFormat_AV1, MFVideoFormat_H264,
	MFVideoFormat_AYUV, MFVideoFormat_HEVC, MFVideoFormat_NV12, MFVideoFormat_P010,
	MFVideoInterlace_Progressive,
	MFSTARTUP_FULL,
	MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE,
	MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
	MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
	MFT_REGISTER_TYPE_INFO, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
	MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
	MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION,
};

static MF_INIT: Once = Once::new();

// Canned 4:4:4 keyframes (320×180 testsrc2, the same fixtures the Linux / macOS probes decode;
// see `testdata/README.md`). The probe decodes them for real before this client says it can
// show a 4:4:4 stream.
const TEST_444_H264: &[u8] = include_bytes!("../../testdata/test-444.h264");
const TEST_444_H265: &[u8] = include_bytes!("../../testdata/test-444.h265");

/// Pack two u32 into the hi/lo halves of a u64 the way MF stores 2D attributes
/// (`MF_MT_FRAME_SIZE` = width<<32|height, `MF_MT_FRAME_RATE` = num<<32|den).
#[inline]
fn pack_2x32(hi: u32, lo: u32) -> u64 {
	((hi as u64) << 32) | (lo as u64)
}

/// Wraps an `IMFTransform` H.264/HEVC/AV1 decoder bound to our D3D11 device via an
/// `IMFDXGIDeviceManager`, producing NV12 textures.
pub struct Decoder {
	transform: IMFTransform,
	device: ID3D11Device,
	context: ID3D11DeviceContext,
	// Keep the device manager alive for the lifetime of the decoder (the MFT holds it weakly
	// via the ULONG_PTR we passed in SET_D3D_MANAGER).
	_manager: IMFDXGIDeviceManager,
	subtype: GUID,
	/// The picture's VISIBLE size (the stream's display aperture: 1920×1080 of a 1920×1088
	/// surface — decoders pad to the macroblock grid). `None` until the MFT reports it.
	visible: Option<(u32, u32)>,
	/// The negotiated output: NV12 (8-bit) or P010 (10-bit). The decoded textures carry it.
	out_format: DXGI_FORMAT,
	/// The stream's transfer function is SMPTE ST 2084 (PQ): MF's `MF_MT_TRANSFER_FUNCTION`
	/// from the bitstream's colour description; a 10-bit stream that does not say counts as PQ
	/// (the only 10-bit streams a Pulsar host sends are HDR10 — see pulsar-capture `hdr.rs`).
	pq: bool,
	width: u32,
	height: u32,
	fps: u32,
}

// The decoder runs on a dedicated decode thread. The contained COM interfaces are
// apartment-agnostic (MTA, free-threaded) and the D3D11 device is multithread-protected, so it
// is safe to move to that thread.
unsafe impl Send for Decoder {}

impl Decoder {
	pub fn new(device: &ID3D11Device, codec: Codec, w: u32, h: u32, fps: u32) -> Result<Self> {
		Self::new_ex(device, codec, w, h, fps, true)
	}

	/// Like `new`, but `allow_async: false` refuses the async-HW-MFT last resort: an async
	/// MFT driven synchronously accepts input forever and never emits a frame, so the
	/// zero-output watchdog rebuilds the decoder through here to force a sync (inbox) MFT.
	pub fn new_ex(
		device: &ID3D11Device,
		codec: Codec,
		w: u32,
		h: u32,
		fps: u32,
		allow_async: bool,
	) -> Result<Self> {
		unsafe {
			// 1. Init Media Foundation (idempotent; guarded so multiple decoders are fine).
			MF_INIT.call_once(|| {
				let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
			});

			let subtype = match codec {
				Codec::H264 => MFVideoFormat_H264,
				Codec::H265 => MFVideoFormat_HEVC,
				Codec::Av1 => MFVideoFormat_AV1,
			};

			// 2. Find the decoder MFT for this input subtype. Prefer a hardware DXVA MFT (sync
			//    or async); fall back to a software MFT so the path works on any GPU.
			let transform = create_decoder_mft(subtype, allow_async)?;
			// Low-latency decoding: without MF_LOW_LATENCY the Microsoft decoders hold frames
			// back (a DPB/reorder output delay of one to several frames) before emitting them —
			// pure added glass-to-glass latency for a bframes=0 live stream. Best effort: a
			// decoder that doesn't know the attribute just ignores it.
			if let Ok(attrs) = transform.GetAttributes() {
				let _ = attrs.SetUINT32(&windows::Win32::Media::MediaFoundation::MF_LOW_LATENCY, 1);
			}

			// 3. Multithread-protect the caller's D3D11 device (it was created WITHOUT it), then
			//    build the DXGI device manager and hand it to the MFT for zero-copy output.
			let context = device.GetImmediateContext()?;
			if let Ok(mt) = context.cast::<ID3D11Multithread>() {
				let _ = mt.SetMultithreadProtected(true);
			}

			let mut reset_token: u32 = 0;
			let mut manager: Option<IMFDXGIDeviceManager> = None;
			MFCreateDXGIDeviceManager(&mut reset_token, &mut manager)?;
			let manager = manager.unwrap();
			manager.ResetDevice(device, reset_token)?;

			// ProcessMessage takes the manager as a ULONG_PTR (usize). The MFT AddRefs it, so we
			// keep our own reference in `_manager` to balance lifetimes.
			transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;

			// 4a. Input type: major=Video, subtype=codec, frame size/rate, progressive.
			let input = MFCreateMediaType()?;
			input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
			input.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
			input.SetUINT64(&MF_MT_FRAME_SIZE, pack_2x32(w, h))?;
			input.SetUINT64(&MF_MT_FRAME_RATE, pack_2x32(fps.max(1), 1))?;
			input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
			transform.SetInputType(0, &input, 0)?;

			// 4b. Output type: walk the available output types and pick NV12 (P010 for a
			//     10-bit stream — re-decided on the stream change once the SPS is known).
			let out_format = set_output(&transform)?;

			// 5. Begin streaming.
			transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
			transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

			Ok(Self {
				transform,
				device: device.clone(),
				context,
				_manager: manager,
				subtype,
				visible: None,
				out_format,
				pq: false,
				width: w,
				height: h,
				fps,
			})
		}
	}

	/// Feed one access unit; drain any decoded NV12 textures (usually 0 or 1).
	pub fn decode(&mut self, au: &AccessUnit) -> Result<Vec<ID3D11Texture2D>> {
		unsafe {
			// 6a. Wrap the Annex-B / OBU bytes in an IMFSample backed by a single memory buffer.
			let sample = MFCreateSample()?;
			let buffer = MFCreateMemoryBuffer(au.data.len().max(1) as u32)?;
			{
				let mut ptr: *mut u8 = std::ptr::null_mut();
				let mut max_len: u32 = 0;
				buffer.Lock(&mut ptr, Some(&mut max_len), None)?;
				if !ptr.is_null() && !au.data.is_empty() {
					std::ptr::copy_nonoverlapping(au.data.as_ptr(), ptr, au.data.len());
				}
				buffer.Unlock()?;
			}
			buffer.SetCurrentLength(au.data.len() as u32)?;
			sample.AddBuffer(&buffer)?;

			// PTS: 90 kHz RTP ticks → 100 ns units. Use widening math to avoid overflow.
			let pts_100ns = (au.pts_90k as i64) * 10_000_000 / 90_000;
			sample.SetSampleTime(pts_100ns)?;

			// 6b. Submit. ProcessInput should normally accept (we drain fully each call).
			self.transform.ProcessInput(0, &sample, 0)?;

			// 6c. Drain all currently-available output frames.
			self.drain()
		}
	}

	/// Pull every ready output sample, converting each to a fresh single-slice NV12 texture.
	unsafe fn drain(&mut self) -> Result<Vec<ID3D11Texture2D>> {
		let mut out = Vec::new();
		loop {
			let mut data = [MFT_OUTPUT_DATA_BUFFER::default(); 1];
			// For a DXGI/D3D11 MFT, the transform allocates the output samples itself
			// (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES), so we leave pSample = null.
			let mut status: u32 = 0;
			let hr = self.transform.ProcessOutput(0, &mut data, &mut status);

			match hr {
				Ok(()) => {
					if let Some(sample) = data[0].pSample.take() {
						if let Ok(tex) = self.sample_to_texture(&sample) {
							out.push(tex);
						}
					}
				}
				Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
					// No more output for now; hand back what we decoded.
					break;
				}
				Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
					// The decoder discovered the real stream geometry (and bit depth);
					// renegotiate NV12 / P010 and retry.
					self.out_format = set_output(&self.transform)?;
					self.pq = output_is_pq(&self.transform, self.out_format);
					self.visible = visible_size(&self.transform);
					continue;
				}
				Err(e) => return Err(e),
			}
		}
		Ok(out)
	}

	/// Extract the D3D11 NV12 (or AYUV, 4:4:4) texture from an output sample (zero-copy via
	/// IMFDXGIBuffer) and CopyResource the correct array slice into a fresh single-slice texture
	/// of the same format the downstream VideoProcessor can bind as a shader/render resource.
	unsafe fn sample_to_texture(&self, sample: &IMFSample) -> Result<ID3D11Texture2D> {
		let buffer = sample.GetBufferByIndex(0)?;
		let dxgi: IMFDXGIBuffer = buffer.cast()?;

		// The MFT-owned texture is often a texture-array; GetSubresourceIndex tells us our slice.
		let mut src: Option<ID3D11Texture2D> = None;
		dxgi.GetResource(&ID3D11Texture2D::IID, &mut src as *mut _ as *mut _)?;
		let src = src.unwrap();
		let slice = dxgi.GetSubresourceIndex()?;

		// Describe a single-slice copy with SHADER_RESOURCE|RENDER_TARGET bind flags, in the
		// decoder's own format (NV12; AYUV for a 4:4:4 stream — anything else stays NV12, the
		// format this path always used).
		let mut desc = D3D11_TEXTURE2D_DESC::default();
		src.GetDesc(&mut desc);
		desc.ArraySize = 1;
		desc.MipLevels = 1;
		// NV12 / AYUV (4:4:4) for the VideoProcessor present; P010 is read by the HDR shader
		// (hdr.rs) through two plane views, so it needs SHADER_RESOURCE only.
		if desc.Format != DXGI_FORMAT_P010 && desc.Format != DXGI_FORMAT_AYUV {
			desc.Format = DXGI_FORMAT_NV12;
		}
		desc.Usage = D3D11_USAGE_DEFAULT;
		desc.BindFlags = if desc.Format == DXGI_FORMAT_P010 {
			D3D11_BIND_SHADER_RESOURCE.0 as u32
		} else {
			(D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32
		};
		desc.CPUAccessFlags = 0;
		desc.MiscFlags = 0;

		let mut dst: Option<ID3D11Texture2D> = None;
		self.device.CreateTexture2D(&desc, None, Some(&mut dst))?;
		let dst = dst.unwrap();

		// Copy just our array slice (dst subresource 0 ← src subresource `slice`).
		self.context
			.CopySubresourceRegion(&dst, 0, 0, 0, 0, &src, slice, None);

		Ok(dst)
	}
}

impl Decoder {
	/// The visible picture size (see `visible`), once the decoder knows it.
	pub fn visible(&self) -> Option<(u32, u32)> {
		self.visible
	}

	/// The stream is 10-bit PQ (HDR10): its pictures are P010 and need the HDR present.
	pub fn is_hdr(&self) -> bool {
		self.out_format == DXGI_FORMAT_P010 && self.pq
	}
}

/// The output type's visible area: `MF_MT_MINIMUM_DISPLAY_APERTURE` (an `MFVideoArea`; the
/// SPS conformance window), else its `MF_MT_FRAME_SIZE`. `None` when neither is set.
unsafe fn visible_size(transform: &IMFTransform) -> Option<(u32, u32)> {
	use windows::Win32::Media::MediaFoundation::{MFVideoArea, MF_MT_MINIMUM_DISPLAY_APERTURE};
	let t = transform.GetOutputCurrentType(0).ok()?;
	let mut area = MFVideoArea::default();
	let mut got = 0u32;
	let blob = std::slice::from_raw_parts_mut(&mut area as *mut MFVideoArea as *mut u8, std::mem::size_of::<MFVideoArea>());
	if t.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, blob, Some(&mut got)).is_ok()
		&& got as usize == std::mem::size_of::<MFVideoArea>()
		&& area.Area.cx > 0
		&& area.Area.cy > 0
	{
		return Some((area.Area.cx as u32, area.Area.cy as u32));
	}
	let v = t.GetUINT64(&MF_MT_FRAME_SIZE).ok()?;
	let (w, h) = ((v >> 32) as u32, v as u32);
	(w > 0 && h > 0).then_some((w, h))
}

/// Create the decoder `IMFTransform` for `subtype`. This decoder is driven SYNCHRONOUSLY
/// (`ProcessInput` → `ProcessOutput` drain), so it PREFERS a synchronous MFT — the Microsoft
/// inbox decoder. A hardware MFT is ALWAYS asynchronous and requires the full event-driven
/// `IMFMediaEventGenerator` model; driven synchronously it accepts input but never yields a
/// frame (permanent black screen). We still activate + UNLOCK hardware/async MFTs
/// (`MF_TRANSFORM_ASYNC_UNLOCK` is mandatory before any call, else every call fails
/// `MF_E_TRANSFORM_ASYNC_LOCKED`) and keep the first as a LAST-RESORT fallback for hardware that
/// registers only async decoders (e.g. Qualcomm ARM64), but a sync MFT wins whenever one exists.
unsafe fn create_decoder_mft(subtype: GUID, allow_async: bool) -> Result<IMFTransform> {
	let in_info = MFT_REGISTER_TYPE_INFO {
		guidMajorType: MFMediaType_Video,
		guidSubtype: subtype,
	};

	// Try HW (async + sync), then SW. Each returns a list of IMFActivate.
	let attempts = [
		MFT_ENUM_FLAG_HARDWARE
			| MFT_ENUM_FLAG_ASYNCMFT
			| MFT_ENUM_FLAG_SYNCMFT
			| MFT_ENUM_FLAG_SORTANDFILTER,
		// Software fallback: no HARDWARE flag (there is no SOFTWARE flag in this crate version;
		// omitting HARDWARE yields software/registered MFTs).
		MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
	];

	// An unlocked async hardware MFT, kept only in case no synchronous MFT is found at all.
	let mut async_fallback: Option<IMFTransform> = None;

	for flags in attempts {
		let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
		let mut count: u32 = 0;
		let hr = MFTEnumEx(
			MFT_CATEGORY_VIDEO_DECODER,
			flags,
			Some(&in_info),
			None, // any output (we negotiate NV12 ourselves)
			&mut activates,
			&mut count,
		);
		if hr.is_err() || count == 0 || activates.is_null() {
			if !activates.is_null() {
				windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _));
			}
			continue;
		}

		// Prefer the first SYNCHRONOUS MFT in the (sorted/filtered) list; unlock + stash the
		// first async one as a fallback and keep scanning.
		let list = std::slice::from_raw_parts(activates, count as usize);
		let mut sync_pick: Option<IMFTransform> = None;
		for activate in list {
			if let Some(act) = activate {
				if let Ok(t) = act.ActivateObject::<IMFTransform>() {
					if is_async_mft(&t) {
						unlock_async_mft(&t);
						if async_fallback.is_none() {
							async_fallback = Some(t);
						}
					} else {
						sync_pick = Some(t);
						break;
					}
				}
			}
		}
		windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _));

		if let Some(t) = sync_pick {
			return Ok(t);
		}
	}

	// No synchronous MFT anywhere — fall back to the unlocked async HW MFT (hardware-only
	// clients). It may still not decode under the synchronous drive, but it is the only option.
	if allow_async {
		if let Some(t) = async_fallback {
			return Ok(t);
		}
	}

	Err(windows::core::Error::from(
		windows::Win32::Foundation::E_FAIL,
	))
}

/// Headless capability probe — the Windows analogue of the Linux tiered probe (main.rs
/// `--probe`). Reports, per codec, whether a decoder MFT this renderer can actually drive
/// exists: `ok` requires a SYNCHRONOUS MFT (the drive model is ProcessInput→ProcessOutput;
/// an async-only MFT accepts input forever and never emits a frame — the black-screen
/// trap). JSON schema matches the Linux probe so the app's caps parser needs no changes.
///
/// `hw` / `tier` are the REAL state, not a blanket claim: the synchronous MFTs this probe
/// picks are the inbox / Store decoders (msmpeg2vdec, the HEVC / AV1 Video Extensions),
/// which decode on the GPU through DXVA only when they are D3D11-aware (`MF_SA_D3D11_AWARE`)
/// AND the GPU the renderer opens (the default hardware adapter, as in `win::mod`) exposes a
/// DXVA decoder profile for the codec that outputs NV12. Otherwise the same MFT decodes on
/// the CPU, which is `hw:false, tier:"software"` (still `ok`: it plays, just slower).
///
/// `ten_bit` (HEVC / AV1): a canned 10-bit HDR10 keyframe (`testdata/test-hdr10.*`) REALLY
/// decodes to P010 on a hardware D3D11 device — the precondition for asking a host for HDR
/// (the present path then shows it on an HDR monitor or tone-maps it, `win/hdr.rs`).
pub fn probe_json() -> String {
	unsafe {
		MF_INIT.call_once(|| {
			let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
		});
	}
	// 1. The base caps: MFT enumeration only (fast, no device), exactly as before HDR.
	let codecs = [
		("h264", MFVideoFormat_H264, Codec::H264, None),
		("h265", MFVideoFormat_HEVC, Codec::H265, Some(TEST_HDR10_H265)),
		("av1", MFVideoFormat_AV1, Codec::Av1, Some(TEST_HDR10_AV1)),
	];
	let mfts: Vec<Result<IMFTransform>> = codecs.iter().map(|c| unsafe { create_decoder_mft(c.1, false) }).collect();
	// 2. The 10-bit checks need a hardware D3D11 device and real decodes. A wedged GPU driver
	// can block D3D11CreateDevice for good (seen on an NVIDIA host, 2026-11-13); the app gives
	// the whole probe 15 s and then drops to `probe_unavailable`, which would cost every
	// session its real codec caps. So they run on their own thread under a short deadline: a
	// hang or a slow driver only clears `ten_bit` (the process exits after printing, taking a
	// stuck thread with it — `probe_abandoned_worker`). Same adapter as the renderer (the
	// default one, `Renderer::new`). Started first so it overlaps the DXVA query below.
	let want: Vec<(usize, Codec, &'static [u8])> = codecs
		.iter()
		.enumerate()
		.filter_map(|(i, c)| c.3.filter(|_| mfts[i].is_ok()).map(|s| (i, c.2, s)))
		.collect();
	let pending = want.len();
	let (tx, rx) = std::sync::mpsc::channel();
	std::thread::spawn(move || {
		// Media Foundation / D3D on this thread: join the process MTA.
		let _ = unsafe {
			windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED)
		};
		for (i, codec, sample) in want {
			let _ = tx.send((i, decodes_to_p010(codec, sample)));
		}
	});
	let deadline = std::time::Instant::now() + TEN_BIT_PROBE_BUDGET;
	// 3. The real hw / sw tier per codec: a D3D11-aware MFT and a DXVA profile on the GPU
	// (bounded query on its own thread, see `dxva_nv12_profiles_bounded`).
	let dxva = dxva_nv12_profiles_bounded(DXVA_QUERY_TIMEOUT);
	let mut ten = [false; 3];
	let mut answered = 0;
	while let Ok((i, ok)) = rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
		ten[i] = ok;
		answered += 1;
	}
	if answered < pending {
		// The 10-bit worker is still inside the driver: end the process hard after printing.
		DXVA_QUERY_ABANDONED.store(true, std::sync::atomic::Ordering::SeqCst);
	}
	let mut entries = Vec::new();
	for (i, (name, subtype, ..)) in codecs.iter().enumerate() {
		let ok = mfts[i].is_ok();
		let hw = match &mfts[i] {
			Ok(t) => unsafe { is_d3d11_aware(t) && gpu_decodes(*subtype, &dxva) },
			Err(_) => false,
		};
		entries.push(probe_entry(name, ok, hw, ten[i]));
	}
	format!("[{}]", entries.join(","))
}

/// One probe JSON entry (same keys as the Linux / macOS probes).
/// `ten_bit`: the 10-bit HDR10 decode check passed (never without `ok`).
fn probe_entry(codec: &str, ok: bool, hw: bool, ten_bit: bool) -> String {
	format!(
		concat!(
			"{{\"codec\":\"{}\",\"ok\":{},\"decoder\":\"mediafoundation\",",
			"\"hw\":{},\"tier\":\"{}\",\"incompatible_with\":[],\"ten_bit\":{}}}"
		),
		codec,
		ok,
		hw && ok,
		if hw && ok { "hwaccel" } else { "software" },
		ten_bit && ok
	)
}

/// How long the probe waits for the DXVA profile query before it treats the GPU as having
/// none (`hw:false`).
const DXVA_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(2500);

/// `dxva_nv12_profiles` on a worker thread, bounded by `timeout`. The query is informational
/// (it only sets `hw`/`tier`); `ok` comes from the MFT enumeration alone. `D3D11CreateDevice`
/// can block for good on a GPU whose driver is wedged by another process (seen on NVIDIA:
/// every device creation blocks while one hung encoder holds the driver). Without the bound
/// the whole `--probe` would hang until caps.rs kills it after 15 s and falls back to H.264
/// only, so one informational flag would cost HEVC/AV1 and a 15 s delay. On a timeout the
/// worker is left behind (the process exits right after printing) and the list is empty.
fn dxva_nv12_profiles_bounded(timeout: std::time::Duration) -> Vec<GUID> {
	let (tx, rx) = std::sync::mpsc::channel();
	let spawned = std::thread::Builder::new()
		.name("dxva-probe".into())
		.spawn(move || {
			let _ = tx.send(unsafe { dxva_nv12_profiles() });
		});
	if spawned.is_err() {
		return Vec::new();
	}
	rx.recv_timeout(timeout).unwrap_or_else(|_| {
		DXVA_QUERY_ABANDONED.store(true, std::sync::atomic::Ordering::SeqCst);
		Vec::new()
	})
}

static DXVA_QUERY_ABANDONED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Did the probe leave a worker blocked in the driver (the DXVA query, or the 10-bit HDR10
/// decode check past `TEN_BIT_PROBE_BUDGET`)? Then a normal exit could
/// hang in DLL detach (the thread may hold the loader lock), so `main` ends the process
/// with `TerminateProcess` once the JSON is flushed.
pub fn probe_abandoned_worker() -> bool {
	DXVA_QUERY_ABANDONED.load(std::sync::atomic::Ordering::SeqCst)
}

/// The DXVA (D3D11 video) decoder profiles of one codec subtype, 8-bit 4:2:0 — what the
/// stream is unless HDR is on (HEVC Main10 is not needed for the SDR stream).
fn dxva_profiles_for(subtype: GUID) -> &'static [GUID] {
	use windows::Win32::Graphics::Direct3D11::{
		D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0, D3D11_DECODER_PROFILE_H264_VLD_NOFGT,
		D3D11_DECODER_PROFILE_HEVC_VLD_MAIN,
	};
	const H264: [GUID; 1] = [D3D11_DECODER_PROFILE_H264_VLD_NOFGT];
	const HEVC: [GUID; 1] = [D3D11_DECODER_PROFILE_HEVC_VLD_MAIN];
	const AV1: [GUID; 1] = [D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0];
	if subtype == MFVideoFormat_H264 {
		&H264
	} else if subtype == MFVideoFormat_HEVC {
		&HEVC
	} else if subtype == MFVideoFormat_AV1 {
		&AV1
	} else {
		&[]
	}
}

/// Does the GPU (its NV12-output DXVA profiles in `dxva`) decode this codec subtype?
fn gpu_decodes(subtype: GUID, dxva: &[GUID]) -> bool {
	dxva_profiles_for(subtype).iter().any(|p| dxva.contains(p))
}

/// The DXVA decoder profiles of the default hardware adapter (the one the renderer's
/// `D3D11CreateDevice(None, HARDWARE, …, VIDEO_SUPPORT)` opens) that can output NV12.
/// Empty when there is no D3D11 video device (WARP / basic display driver / no GPU).
unsafe fn dxva_nv12_profiles() -> Vec<GUID> {
	use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
	use windows::Win32::Graphics::Direct3D11::{
		D3D11CreateDevice, ID3D11VideoDevice, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
	};
	let mut device: Option<ID3D11Device> = None;
	if D3D11CreateDevice(
		None,
		D3D_DRIVER_TYPE_HARDWARE,
		Default::default(),
		D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
		Some(&[D3D_FEATURE_LEVEL_11_0]),
		D3D11_SDK_VERSION,
		Some(&mut device),
		None,
		None,
	)
	.is_err()
	{
		return Vec::new();
	}
	let Some(video) = device.and_then(|d| d.cast::<ID3D11VideoDevice>().ok()) else {
		return Vec::new();
	};
	let mut out = Vec::new();
	for i in 0..video.GetVideoDecoderProfileCount() {
		let Ok(p) = video.GetVideoDecoderProfile(i) else {
			continue;
		};
		if video.CheckVideoDecoderFormat(&p, DXGI_FORMAT_NV12).is_ok_and(|b| b.as_bool()) {
			out.push(p);
		}
	}
	out
}

/// Is this MFT D3D11-aware (`MF_SA_D3D11_AWARE`)? Only such an MFT decodes through DXVA on
/// the device manager `Decoder::new_ex` hands it; any other decodes on the CPU.
unsafe fn is_d3d11_aware(t: &IMFTransform) -> bool {
	t.GetAttributes()
		.and_then(|a| a.GetUINT32(&windows::Win32::Media::MediaFoundation::MF_SA_D3D11_AWARE))
		.map(|v| v != 0)
		.unwrap_or(false)
}

#[cfg(test)]
mod probe_tests {
	use super::*;

	#[test]
	fn entry_reports_the_real_tier() {
		let hw = probe_entry("h264", true, true, false);
		assert!(hw.contains("\"hw\":true") && hw.contains("\"tier\":\"hwaccel\""), "{hw}");
		let sw = probe_entry("h265", true, false, true);
		assert!(sw.contains("\"hw\":false") && sw.contains("\"tier\":\"software\""), "{sw}");
		// No decoder at all is never "hardware".
		let none = probe_entry("av1", false, true, true);
		assert!(none.contains("\"ok\":false") && none.contains("\"hw\":false"), "{none}");
		// No decoder: no 10-bit either; a 10-bit pass is reported as such.
		assert!(none.contains("\"ten_bit\":false") && sw.contains("\"ten_bit\":true"), "{none} {sw}");
		for e in [hw, sw, none] {
			let v: serde_json::Value = serde_json::from_str(&e).expect("valid JSON");
			assert!(v["incompatible_with"].is_array());
		}
	}

	#[test]
	fn gpu_profile_match_is_per_codec() {
		use windows::Win32::Graphics::Direct3D11::D3D11_DECODER_PROFILE_H264_VLD_NOFGT;
		let only_h264 = [D3D11_DECODER_PROFILE_H264_VLD_NOFGT];
		assert!(gpu_decodes(MFVideoFormat_H264, &only_h264));
		assert!(!gpu_decodes(MFVideoFormat_HEVC, &only_h264));
		assert!(!gpu_decodes(MFVideoFormat_AV1, &only_h264));
		assert!(!gpu_decodes(MFVideoFormat_H264, &[]));
	}

	/// The real probe on this machine: valid JSON, three codecs, `hw` only where `ok`.
	#[test]
	fn probe_json_is_well_formed() {
		let v: serde_json::Value = serde_json::from_str(&probe_json()).expect("valid JSON");
		let arr = v.as_array().expect("array");
		assert_eq!(arr.len(), 3);
		for e in arr {
			if e["hw"].as_bool() == Some(true) {
				assert_eq!(e["ok"].as_bool(), Some(true));
				assert_eq!(e["tier"].as_str(), Some("hwaccel"));
			}
		}
	}
}

/// `--probe-444`: the codecs this machine REALLY decodes and presents in 4:4:4, as
/// `[{"codec":"h265"}]`. A separate invocation from `--probe` on purpose: it creates a hardware
/// D3D11 device and runs real decodes (a wedged driver can hang there), so the app runs it only
/// when 4:4:4 is wanted (`PULSAR_YUV444`) and under its own timeout — a hang here can never cost
/// the 4:2:0 caps `--probe` reports.
pub fn probe_444_json() -> String {
	unsafe {
		MF_INIT.call_once(|| {
			let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
		});
	}
	let mut entries = Vec::new();
	// No AV1: no fixture, and no host encoder we drive makes AV1 High (4:4:4).
	for (name, subtype, codec) in [("h264", MFVideoFormat_H264, Codec::H264), ("h265", MFVideoFormat_HEVC, Codec::H265)] {
		let ok = unsafe { create_decoder_mft(subtype, false).is_ok() && probe_yuv444(codec) };
		if ok {
			entries.push(format!("{{\"codec\":\"{name}\"}}"));
		}
	}
	format!("[{}]", entries.join(","))
}

/// Can this machine show a 4:4:4 `codec` stream? Decodes the canned 4:4:4 keyframe with the
/// same decoder the session uses (a synchronous MFT on a hardware D3D11 device), wants an AYUV
/// picture out (the Microsoft H.264 decoder has no 4:4:4 at all; HEVC RExt depends on the
/// GPU's DXVA profiles), and presents it once through the same VideoProcessor the session
/// presents with. Any failure → `false`, and the app never asks for 4:4:4.
unsafe fn probe_yuv444(codec: Codec) -> bool {
	use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
	use windows::Win32::Graphics::Direct3D11::{
		D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
		D3D11_SDK_VERSION,
	};
	use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
	use windows::Win32::Media::MediaFoundation::MFT_MESSAGE_COMMAND_DRAIN;
	let fixture = match codec {
		Codec::H264 => TEST_444_H264,
		Codec::H265 => TEST_444_H265,
		Codec::Av1 => return false,
	};
	let mut device: Option<ID3D11Device> = None;
	if D3D11CreateDevice(
		None,
		D3D_DRIVER_TYPE_HARDWARE,
		Default::default(),
		D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
		Some(&[D3D_FEATURE_LEVEL_11_0]),
		D3D11_SDK_VERSION,
		Some(&mut device),
		None,
		None,
	)
	.is_err()
	{
		return false;
	}
	let Some(device) = device else { return false };
	let Ok(mut dec) = Decoder::new_ex(&device, codec, 320, 180, 30, false) else {
		return false;
	};
	let au = AccessUnit { data: fixture.to_vec(), pts_90k: 0, key: true, conceal: false };
	let mut frames = dec.decode(&au).unwrap_or_default();
	if frames.is_empty() {
		// A single keyframe may sit in the decoder until the stream is drained.
		if dec.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0).is_ok() {
			frames = dec.drain().unwrap_or_default();
		}
	}
	let Some(tex) = frames.into_iter().next() else { return false };
	let mut td = D3D11_TEXTURE2D_DESC::default();
	tex.GetDesc(&mut td);
	if td.Format != DXGI_FORMAT_AYUV {
		// A decoder that turned 4:4:4 into NV12 would stream sharp text only to blur it here.
		return false;
	}
	// Present it once, like a session does (VideoProcessor AYUV → BGRA).
	let Ok(present) = super::present::Present::new(&device, td.Width, td.Height, td.Width, td.Height) else {
		return false;
	};
	let desc = D3D11_TEXTURE2D_DESC {
		Width: td.Width,
		Height: td.Height,
		MipLevels: 1,
		ArraySize: 1,
		Format: DXGI_FORMAT_B8G8R8A8_UNORM,
		SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
		Usage: D3D11_USAGE_DEFAULT,
		BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
		CPUAccessFlags: 0,
		MiscFlags: 0,
	};
	let mut back: Option<ID3D11Texture2D> = None;
	if device.CreateTexture2D(&desc, None, Some(&mut back)).is_err() {
		return false;
	}
	let Some(back) = back else { return false };
	present.blt(&tex, &back).is_ok()
}

/// How long the 10-bit decode checks may take in `--probe` before `ten_bit` is reported false.
const TEN_BIT_PROBE_BUDGET: std::time::Duration = std::time::Duration::from_secs(4);

/// 64×64 HDR10 keyframes (HEVC Main10 / AV1 10-bit, BT.2020 PQ) for the `ten_bit` probe.
const TEST_HDR10_H265: &[u8] = include_bytes!("../../testdata/test-hdr10.h265");
const TEST_HDR10_AV1: &[u8] = include_bytes!("../../testdata/test-hdr10.av1.obu");

/// A hardware D3D11 device with video support (what the renderer decodes on).
fn probe_device() -> Option<ID3D11Device> {
	use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
	use windows::Win32::Graphics::Direct3D11::{
		D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
		D3D11_SDK_VERSION,
	};
	let mut d = None;
	unsafe {
		D3D11CreateDevice(
			None,
			D3D_DRIVER_TYPE_HARDWARE,
			Default::default(),
			D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
			Some(&[D3D_FEATURE_LEVEL_11_0]),
			D3D11_SDK_VERSION,
			Some(&mut d),
			None,
			None,
		)
		.ok()?;
	}
	d
}

/// Decode one canned keyframe the way the renderer does; true when a picture comes out as
/// P010 flagged PQ.
fn decodes_to_p010(codec: Codec, sample: &[u8]) -> bool {
	let Some(dev) = probe_device() else { return false };
	decode_sample(&dev, codec, sample).is_some_and(|(fmt, pq)| fmt == DXGI_FORMAT_P010 && pq)
}

/// Feed `sample` as one access unit (then drain) and report the first picture's format.
fn decode_sample(dev: &ID3D11Device, codec: Codec, sample: &[u8]) -> Option<(DXGI_FORMAT, bool)> {
	use windows::Win32::Media::MediaFoundation::MFT_MESSAGE_COMMAND_DRAIN;
	let mut d = Decoder::new_ex(dev, codec, 64, 64, 30, false).ok()?;
	let au = AccessUnit { data: sample.to_vec(), pts_90k: 0, key: true, conceal: false };
	let mut out = d.decode(&au).ok()?;
	if out.is_empty() {
		unsafe {
			d.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0).ok()?;
			out = d.drain().ok()?;
		}
	}
	let tex = out.first()?;
	let mut desc = D3D11_TEXTURE2D_DESC::default();
	unsafe { tex.GetDesc(&mut desc) };
	Some((desc.Format, d.pq))
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Split an HEVC Annex B stream into access units (parameter sets / prefix SEI open a new
	/// one after a picture; a slice with first_slice_segment_in_pic_flag starts a picture).
	fn hevc_aus(bs: &[u8]) -> Vec<Vec<u8>> {
		let mut starts = vec![];
		let mut i = 0;
		while i + 3 < bs.len() {
			if bs[i..].starts_with(&[0, 0, 1]) {
				let s = if i > 0 && bs[i - 1] == 0 { i - 1 } else { i };
				starts.push((s, i + 3));
				i += 3;
			} else {
				i += 1;
			}
		}
		let mut aus: Vec<Vec<u8>> = vec![];
		let mut cur: Vec<u8> = vec![];
		let mut has_vcl = false;
		for (k, &(s, h)) in starts.iter().enumerate() {
			let end = starts.get(k + 1).map(|n| n.0).unwrap_or(bs.len());
			let ty = (bs[h] >> 1) & 0x3F;
			let vcl = ty < 32;
			let first_slice = vcl && bs.get(h + 2).is_some_and(|b| b & 0x80 != 0);
			if has_vcl && (matches!(ty, 32..=35 | 39) || first_slice) {
				aus.push(std::mem::take(&mut cur));
				has_vcl = false;
			}
			cur.extend_from_slice(&bs[s..end]);
			has_vcl |= vcl;
		}
		if !cur.is_empty() {
			aus.push(cur);
		}
		aus
	}

	/// A REAL HDR10 stream from the host path (pulsar-capture's ignored NVENC test writes one):
	/// `PULSAR_HDR_STREAM=<file.h265> cargo test -p pulsar-render real_hdr10 -- --ignored`.
	/// Every picture decodes to P010 PQ, and the tone-mapped SDR present of the last one is not
	/// black and not one flat colour.
	#[test]
	#[ignore]
	fn real_hdr10_stream_decodes_and_tone_maps() {
		let path = std::env::var("PULSAR_HDR_STREAM").expect("PULSAR_HDR_STREAM");
		let bs = std::fs::read(path).unwrap();
		unsafe {
			MF_INIT.call_once(|| {
				let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
			});
		}
		let dev = probe_device().expect("device");
		let mut d = Decoder::new_ex(&dev, Codec::H265, 1280, 720, 60, false).unwrap();
		let mut pics = vec![];
		for (n, au) in hevc_aus(&bs).into_iter().enumerate() {
			let au = AccessUnit { data: au, pts_90k: n as u32 * 1500, key: n == 0, conceal: false };
			pics.extend(d.decode(&au).unwrap());
		}
		assert!(pics.len() >= 20, "{} pictures", pics.len());
		assert!(d.is_hdr(), "decoder: P010 + PQ");
		let last = pics.last().unwrap();
		let mut desc = D3D11_TEXTURE2D_DESC::default();
		unsafe { last.GetDesc(&mut desc) };
		assert_eq!(desc.Format, DXGI_FORMAT_P010);
		let (w, h) = d.visible().unwrap_or((desc.Width, desc.Height));
		let row = unsafe { super::super::hdr::tests_support::sdr_row(&dev, last, desc.Width, desc.Height, w, h) };
		let lum: Vec<u32> = row.iter().map(|p| (p >> 8) & 0xFF).collect();
		let (mn, mx) = (*lum.iter().min().unwrap(), *lum.iter().max().unwrap());
		eprintln!("{} pictures {w}x{h}; tone-mapped row luma {mn}..{mx}", pics.len());
		assert!(mx > 100 && mx - mn > 50, "a gradient, not black / flat: {mn}..{mx}");
	}

	#[test]
	fn hdr10_samples_decode_to_p010_pq_and_sdr_stays_nv12() {
		unsafe {
			MF_INIT.call_once(|| {
				let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
			});
		}
		let Some(dev) = probe_device() else {
			eprintln!("skip: no hardware D3D11 device");
			return;
		};
		// 8-bit stays NV12, not PQ.
		let sdr = decode_sample(&dev, Codec::H265, include_bytes!("../../testdata/test.h265"));
		eprintln!("8-bit HEVC → {sdr:?}");
		if let Some((fmt, pq)) = sdr {
			assert_eq!(fmt, DXGI_FORMAT_NV12);
			assert!(!pq);
		}
		for (codec, s) in [(Codec::H265, TEST_HDR10_H265), (Codec::Av1, TEST_HDR10_AV1)] {
			let got = decode_sample(&dev, codec, s);
			eprintln!("10-bit {codec:?} → {got:?}");
			// A machine without the codec's decoder (no HEVC extension / AV1) yields None.
			if let Some((fmt, pq)) = got {
				assert_eq!(fmt, DXGI_FORMAT_P010, "{codec:?}");
				assert!(pq, "{codec:?}");
			}
		}
	}
}

/// True if an activated MFT is asynchronous. All hardware MFTs are async; they must be unlocked
/// before use and require the event-driven drive model this decoder does not implement.
unsafe fn is_async_mft(t: &IMFTransform) -> bool {
	t.GetAttributes()
		.and_then(|a| a.GetUINT32(&MF_TRANSFORM_ASYNC))
		.map(|v| v == 1)
		.unwrap_or(false)
}

/// Set `MF_TRANSFORM_ASYNC_UNLOCK` on a hardware/async MFT so its `IMFTransform` methods stop
/// failing with `MF_E_TRANSFORM_ASYNC_LOCKED` (mandatory per the MF async-MFT contract).
unsafe fn unlock_async_mft(t: &IMFTransform) {
	if let Ok(attrs) = t.GetAttributes() {
		let _ = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
	}
}

/// Pick the output: P010 when the decoder lists it AHEAD of NV12 (its native format for a
/// 10-bit stream — HEVC Main10 / AV1 10-bit), else NV12. Before the first stream change the
/// decoder lists NV12 first (it does not know the depth yet); the change renegotiates.
/// `PULSAR_DEC_10BIT=0` keeps NV12 whenever the decoder offers it (the decoder then dithers
/// 10-bit down; colours stay PQ-encoded — diagnostics only).
/// A 4:4:4 stream offers no NV12 (the decoder does not subsample): then take its 8-bit 4:4:4
/// AYUV, which the present VideoProcessor reads like NV12.
unsafe fn set_output(transform: &IMFTransform) -> Result<DXGI_FORMAT> {
	let allow_p010 = std::env::var("PULSAR_DEC_10BIT").map(|v| v != "0").unwrap_or(true);
	let mut i = 0u32;
	let mut nv12 = None;
	let mut ayuv = None;
	while let Ok(t) = transform.GetOutputAvailableType(0, i) {
		match t.GetGUID(&MF_MT_SUBTYPE) {
			Ok(s) if s == MFVideoFormat_P010 && allow_p010 && nv12.is_none() => {
				transform.SetOutputType(0, &t, 0)?;
				return Ok(DXGI_FORMAT_P010);
			}
			Ok(s) if s == MFVideoFormat_NV12 && nv12.is_none() => nv12 = Some(t),
			Ok(s) if s == MFVideoFormat_AYUV && ayuv.is_none() => ayuv = Some(t),
			_ => {}
		}
		i += 1;
	}
	if let Some(t) = nv12 {
		transform.SetOutputType(0, &t, 0)?;
		return Ok(DXGI_FORMAT_NV12);
	}
	if let Some(t) = ayuv {
		transform.SetOutputType(0, &t, 0)?;
		return Ok(DXGI_FORMAT_AYUV);
	}

	// Fallback: explicitly request NV12 (some MFTs accept a constructed type even if they don't
	// enumerate it before the input is fully known).
	let t = MFCreateMediaType()?;
	t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
	t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
	transform.SetOutputType(0, &t, 0)?;
	Ok(DXGI_FORMAT_NV12)
}

/// Is the negotiated output PQ? MF's `MF_MT_TRANSFER_FUNCTION` when the decoder copied the
/// bitstream's colour description into it (`MFVideoTransFunc_2084` = 15; any other stated
/// function = not PQ); unstated on a 10-bit output = PQ (see `Decoder::pq`).
unsafe fn output_is_pq(transform: &IMFTransform, fmt: DXGI_FORMAT) -> bool {
	use windows::Win32::Media::MediaFoundation::MF_MT_TRANSFER_FUNCTION;
	if fmt != DXGI_FORMAT_P010 {
		return false;
	}
	match transform.GetOutputCurrentType(0).and_then(|t| t.GetUINT32(&MF_MT_TRANSFER_FUNCTION)) {
		Ok(0) | Err(_) => true, // MFVideoTransFunc_Unknown / not set
		Ok(v) => v == 15,
	}
}
