//! Media Foundation DXVA decode: Annex-B access units → NV12 `ID3D11Texture2D` on our D3D11
//! device (zero-copy, no GPU→CPU download).
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
//!     decoder lists P010 ahead of NV12 for those) → SetOutputType
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
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_NV12, DXGI_FORMAT_P010};
use windows::Win32::Media::MediaFoundation::{
	IMFActivate, IMFDXGIBuffer, IMFDXGIDeviceManager, IMFSample, IMFTransform,
	MFCreateDXGIDeviceManager, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
	MFMediaType_Video, MFStartup, MFTEnumEx, MFVideoFormat_AV1, MFVideoFormat_H264,
	MFVideoFormat_HEVC, MFVideoFormat_NV12, MFVideoFormat_P010, MFVideoInterlace_Progressive,
	MFSTARTUP_FULL,
	MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE,
	MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
	MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
	MFT_REGISTER_TYPE_INFO, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
	MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
	MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION,
};

static MF_INIT: Once = Once::new();

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

	/// Extract the D3D11 NV12 texture from an output sample (zero-copy via IMFDXGIBuffer) and
	/// CopyResource the correct array slice into a fresh single-slice NV12 texture the
	/// downstream VideoProcessor can bind as a shader/render resource.
	unsafe fn sample_to_texture(&self, sample: &IMFSample) -> Result<ID3D11Texture2D> {
		let buffer = sample.GetBufferByIndex(0)?;
		let dxgi: IMFDXGIBuffer = buffer.cast()?;

		// The MFT-owned texture is often a texture-array; GetSubresourceIndex tells us our slice.
		let mut src: Option<ID3D11Texture2D> = None;
		dxgi.GetResource(&ID3D11Texture2D::IID, &mut src as *mut _ as *mut _)?;
		let src = src.unwrap();
		let slice = dxgi.GetSubresourceIndex()?;

		// Describe a single-slice copy with NV12 + SHADER_RESOURCE|RENDER_TARGET bind flags.
		let mut desc = D3D11_TEXTURE2D_DESC::default();
		src.GetDesc(&mut desc);
		desc.ArraySize = 1;
		desc.MipLevels = 1;
		// NV12 for the VideoProcessor present; P010 is read by the HDR shader (hdr.rs) through
		// two plane views, so it needs SHADER_RESOURCE only.
		desc.Format = if desc.Format == DXGI_FORMAT_P010 { DXGI_FORMAT_P010 } else { DXGI_FORMAT_NV12 };
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
/// `ten_bit` (HEVC / AV1): a canned 10-bit HDR10 keyframe (`testdata/test-hdr10.*`) REALLY
/// decodes to P010 on a hardware D3D11 device — the precondition for asking a host for HDR
/// (the present path then shows it on an HDR monitor or tone-maps it, `win/hdr.rs`).
pub fn probe_json() -> String {
	unsafe {
		MF_INIT.call_once(|| {
			let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
		});
	}
	let mut entries = Vec::new();
	for (name, subtype, codec, hdr_sample) in [
		("h264", MFVideoFormat_H264, Codec::H264, None),
		("h265", MFVideoFormat_HEVC, Codec::H265, Some(TEST_HDR10_H265)),
		("av1", MFVideoFormat_AV1, Codec::Av1, Some(TEST_HDR10_AV1)),
	] {
		let sync_ok = unsafe { create_decoder_mft(subtype, false).is_ok() };
		let ten_bit = sync_ok && hdr_sample.is_some_and(|s| decodes_to_p010(codec, s));
		entries.push(format!(
			concat!(
				"{{\"codec\":\"{}\",\"ok\":{},\"decoder\":\"mediafoundation\",",
				"\"hw\":true,\"tier\":\"hwaccel\",\"incompatible_with\":[],\"ten_bit\":{}}}"
			),
			name, sync_ok, ten_bit
		));
	}
	format!("[{}]", entries.join(","))
}

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
unsafe fn set_output(transform: &IMFTransform) -> Result<DXGI_FORMAT> {
	let allow_p010 = std::env::var("PULSAR_DEC_10BIT").map(|v| v != "0").unwrap_or(true);
	let mut i = 0u32;
	let mut nv12 = None;
	while let Ok(t) = transform.GetOutputAvailableType(0, i) {
		match t.GetGUID(&MF_MT_SUBTYPE) {
			Ok(s) if s == MFVideoFormat_P010 && allow_p010 && nv12.is_none() => {
				transform.SetOutputType(0, &t, 0)?;
				return Ok(DXGI_FORMAT_P010);
			}
			Ok(s) if s == MFVideoFormat_NV12 && nv12.is_none() => nv12 = Some(t),
			_ => {}
		}
		i += 1;
	}
	if let Some(t) = nv12 {
		transform.SetOutputType(0, &t, 0)?;
		return Ok(DXGI_FORMAT_NV12);
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
