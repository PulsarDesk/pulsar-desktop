//! D3D11 / DXGI helpers and the cross-adapter (HYBRID) bridge for the native NVENC
//! encoder. These are the free helpers and RAII guards `Encoder::new`/`submit` rely on:
//! NV12 texture + VideoProcessor creation, the NVIDIA device enumeration, the shared
//! keyed-mutex BGRA bridge, the rtp-dest parser, the NVENC session guard and the
//! NVENC status check. Behaviour is unchanged from the original `encode.rs`.

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
	D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
	D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext,
	ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor, D3D11_VIDEO_PROCESSOR_COLOR_SPACE, ID3D11VideoProcessorEnumerator, D3D11_BIND_RENDER_TARGET,
	D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
	D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
	D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
	D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
};
use windows::Win32::Graphics::Dxgi::Common::{
	DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
	DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
	CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_NOT_FOUND,
};
use windows_core::Interface; // for `.cast::<T>()` and `.as_raw()`

use super::{cap_dbg, VENDOR_NVIDIA};
// The NVENC helpers that need no D3D11 live in `nvsession` (shared with the Linux encoder).
pub(super) use super::nvsession::{chk, parse_rtp_dest, SessionGuard};

/// Create OUR own NV12 `ID3D11Texture2D` (DEFAULT usage, BIND_RENDER_TARGET only —
/// the VideoProcessor output view + NVENC input register both accept that). One
/// non-array, single-mip texture; the encoder Blt's into it every frame.
pub(super) unsafe fn create_nv12_texture(
	device: &ID3D11Device,
	w: u32,
	h: u32,
) -> Result<ID3D11Texture2D, String> {
	let desc = D3D11_TEXTURE2D_DESC {
		Width: w,
		Height: h,
		MipLevels: 1,
		ArraySize: 1,
		Format: DXGI_FORMAT_NV12,
		SampleDesc: DXGI_SAMPLE_DESC {
			Count: 1,
			Quality: 0,
		},
		Usage: D3D11_USAGE_DEFAULT,
		// RENDER_TARGET so the VideoProcessor can write it AND NVENC can read it as an
		// input image. No CPU access, no share flags (same device — Strategy A).
		BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
		CPUAccessFlags: 0,
		MiscFlags: 0,
	};
	let mut tex: Option<ID3D11Texture2D> = None;
	device
		.CreateTexture2D(&desc, None, Some(&mut tex))
		.map_err(|e| format!("CreateTexture2D(NV12): {e}"))?;
	tex.ok_or_else(|| "CreateTexture2D(NV12) returned null".to_string())
}

/// Create an ID3D11VideoProcessor (+ its enumerator) configured for a BGRA→NV12
/// playback conversion at WxH. The colour spaces are set by `set_sdr_colour_spaces` (the
/// processor's defaults convert with the BT.601 matrix).
pub(super) unsafe fn build_video_processor(
	vdevice: &ID3D11VideoDevice,
	in_w: u32,
	in_h: u32,
	out_w: u32,
	out_h: u32,
) -> Result<(ID3D11VideoProcessorEnumerator, ID3D11VideoProcessor), String> {
	// Input = native capture size, Output = (possibly downscaled) encode size — the
	// VideoProcessorBlt scales native→encode when they differ (the 1080p-cap downscale).
	let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
		InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
		InputFrameRate: DXGI_RATIONAL {
			Numerator: 60,
			Denominator: 1,
		},
		InputWidth: in_w,
		InputHeight: in_h,
		OutputFrameRate: DXGI_RATIONAL {
			Numerator: 60,
			Denominator: 1,
		},
		OutputWidth: out_w,
		OutputHeight: out_h,
		Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
	};
	let vp_enum = vdevice
		.CreateVideoProcessorEnumerator(&content)
		.map_err(|e| format!("CreateVideoProcessorEnumerator: {e}"))?;
	// windows ≥0.59 returns the processor directly (no out-param).
	let vproc = vdevice
		.CreateVideoProcessor(&vp_enum, 0)
		.map_err(|e| format!("CreateVideoProcessor: {e}"))?;
	Ok((vp_enum, vproc))
}

/// Pin the BGRA→NV12 conversion to Pulsar's SDR rule: full-range sRGB-ish (G22 / BT.709
/// primaries) in, **BT.709 limited range** out — the colour description every encoder writes
/// into the stream (NVENC VUI, MF media type) and every client decodes with. Without it the
/// processor uses its defaults, which convert with the BT.601 matrix, while Linux / macOS
/// clients decode BT.709: skin tones and reds / greens shifted.
///
/// `ID3D11VideoContext1` (Windows 8.1+) takes the DXGI colour-space enums; the older
/// `ID3D11VideoContext` call (YCbCr matrix 709 + 16–235 nominal range) covers a context
/// without it. Never fails: a driver that ignores both keeps its default.
pub(super) unsafe fn set_sdr_colour_spaces(vctx: &ID3D11VideoContext, vproc: &ID3D11VideoProcessor) {
	if let Ok(vctx1) = vctx.cast::<ID3D11VideoContext1>() {
		vctx1.VideoProcessorSetStreamColorSpace1(vproc, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
		vctx1.VideoProcessorSetOutputColorSpace1(vproc, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
		return;
	}
	let (input, output) = legacy_sdr_colour_spaces();
	vctx.VideoProcessorSetStreamColorSpace(vproc, 0, &input);
	vctx.VideoProcessorSetOutputColorSpace(vproc, &output);
}

/// The pre-8.1 colour-space words (bitfield: Usage:1, RGB_Range:1, YCbCr_Matrix:1,
/// YCbCr_xvYCC:1, Nominal_Range:2): input = full-range RGB (RGB_Range 0); output = BT.709
/// matrix (bit 2) with the 16–235 nominal range (`D3D11_VIDEO_PROCESSOR_NOMINAL_RANGE_16_235`
/// = 1 at bits 4..5).
fn legacy_sdr_colour_spaces() -> (D3D11_VIDEO_PROCESSOR_COLOR_SPACE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE) {
	let input = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
	let output = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: (1 << 2) | (1 << 4) };
	(input, output)
}

#[cfg(test)]
mod tests {
	#[test]
	fn legacy_output_is_bt709_limited() {
		let (i, o) = super::legacy_sdr_colour_spaces();
		assert_eq!(i._bitfield, 0);
		// YCbCr_Matrix = 1 (BT.709), Nominal_Range = 1 (16–235), RGB_Range / Usage = 0.
		assert_eq!((o._bitfield >> 2) & 1, 1);
		assert_eq!((o._bitfield >> 4) & 3, 1);
		assert_eq!(o._bitfield & 0b11, 0);
	}

	/// Colour bars through the host's real BGRA → NV12 VideoProcessor (the one the NVENC and
	/// Media Foundation paths use), read back: red / green / blue / white / black must land on
	/// their BT.709 limited-range code values. The processor's default (BT.601 on most drivers)
	/// would put red at Y = 81. Skips without a D3D11 video device.
	#[test]
	fn bgra_to_nv12_is_bt709_limited() {
		use super::*;
		use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
		use windows::Win32::Graphics::Direct3D11::{
			D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
			D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
			D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
			D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
		};
		// (R, G, B) → BT.709 limited (Y, Cb, Cr).
		let bars: [([u8; 3], [u8; 3]); 5] = [
			([255, 0, 0], [63, 102, 240]),
			([0, 255, 0], [173, 42, 26]),
			([0, 0, 255], [32, 240, 118]),
			([255, 255, 255], [235, 128, 128]),
			([0, 0, 0], [16, 128, 128]),
		];
		let (bw, h) = (32u32, 32u32);
		let w = bw * bars.len() as u32;
		// SAFETY: D3D11 calls on a device owned by this test; buffers outlive the calls.
		unsafe {
			let mut dev = None;
			let mut ctx = None;
			if D3D11CreateDevice(
				None,
				D3D_DRIVER_TYPE_HARDWARE,
				HMODULE::default(),
				D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
				Some(&[D3D_FEATURE_LEVEL_11_0]),
				D3D11_SDK_VERSION,
				Some(&mut dev),
				None,
				Some(&mut ctx),
			)
			.is_err()
			{
				eprintln!("no D3D11 video device: skipped");
				return;
			}
			let (dev, ctx): (ID3D11Device, ID3D11DeviceContext) = (dev.unwrap(), ctx.unwrap());
			let Ok(vdev) = dev.cast::<ID3D11VideoDevice>() else { return };
			let vctx: ID3D11VideoContext = ctx.cast().unwrap();
			// The BGRA source, one bar per colour.
			let mut px = Vec::with_capacity((w * h * 4) as usize);
			for _ in 0..h {
				for (rgb, _) in bars {
					for _ in 0..bw {
						px.extend_from_slice(&[rgb[2], rgb[1], rgb[0], 255]);
					}
				}
			}
			let desc = D3D11_TEXTURE2D_DESC {
				Width: w,
				Height: h,
				MipLevels: 1,
				ArraySize: 1,
				Format: DXGI_FORMAT_B8G8R8A8_UNORM,
				SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
				Usage: D3D11_USAGE_DEFAULT,
				BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
				CPUAccessFlags: 0,
				MiscFlags: 0,
			};
			let init = D3D11_SUBRESOURCE_DATA { pSysMem: px.as_ptr().cast(), SysMemPitch: w * 4, SysMemSlicePitch: 0 };
			let mut src = None;
			dev.CreateTexture2D(&desc, Some(&init), Some(&mut src)).unwrap();
			let src = src.unwrap();
			let nv12 = create_nv12_texture(&dev, w, h).unwrap();
			let (vp_enum, vproc) = build_video_processor(&vdev, w, h, w, h).unwrap();
			let ovd = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
				ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
				Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
			};
			let mut ov = None;
			vdev.CreateVideoProcessorOutputView(&nv12, &vp_enum, &ovd, Some(&mut ov)).unwrap();
			let ov = ov.unwrap();
			let ivd = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
				FourCC: 0,
				ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
				Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
			};
			let mut iv = None;
			vdev.CreateVideoProcessorInputView(&src, &vp_enum, &ivd, Some(&mut iv)).unwrap();
			let sd = D3D11_TEXTURE2D_DESC {
				Format: DXGI_FORMAT_NV12,
				Usage: D3D11_USAGE_STAGING,
				BindFlags: 0,
				CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
				..desc
			};
			let mut staging = None;
			dev.CreateTexture2D(&sd, None, Some(&mut staging)).unwrap();
			let staging = staging.unwrap();
			// One Blt, then the (Y, Cb, Cr) at each bar's centre.
			let convert = || -> Vec<[u8; 3]> {
				let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
					Enable: true.into(),
					pInputSurface: std::mem::ManuallyDrop::new(iv.clone()),
					..Default::default()
				};
				let r = vctx.VideoProcessorBlt(&vproc, &ov, 0, std::slice::from_ref(&stream));
				std::mem::ManuallyDrop::drop(&mut stream.pInputSurface);
				r.unwrap();
				ctx.CopyResource(&staging, &nv12);
				let mut m = D3D11_MAPPED_SUBRESOURCE::default();
				ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
				let p = m.pData as *const u8;
				let pitch = m.RowPitch as usize;
				let got = (0..bars.len())
					.map(|i| {
						let x = i * bw as usize + bw as usize / 2;
						let y = *p.add(pitch * (h as usize / 2) + x);
						let uv = p.add(pitch * h as usize + pitch * (h as usize / 4) + (x & !1));
						[y, *uv, *uv.add(1)]
					})
					.collect();
				ctx.Unmap(&staging, 0);
				got
			};
			// The setter is honoured by this driver: BT.601 out puts red at Y = 81 …
			if let Ok(v1) = vctx.cast::<ID3D11VideoContext1>() {
				use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601;
				v1.VideoProcessorSetStreamColorSpace1(&vproc, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
				v1.VideoProcessorSetOutputColorSpace1(&vproc, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601);
				let red601 = convert()[0][0];
				assert!(red601.abs_diff(81) <= 2, "BT.601 red Y = {red601}");
			}
			// … and Pulsar's SDR rule puts every bar on its BT.709 limited code values.
			set_sdr_colour_spaces(&vctx, &vproc);
			for (g, (_, want)) in convert().into_iter().zip(bars) {
				assert!(g.iter().zip(want).all(|(a, b)| a.abs_diff(b) <= 2), "NV12 {g:?}, BT.709 limited {want:?}");
			}
		}
	}
}

// ===========================================================================
// Cross-adapter (HYBRID) support: NVIDIA device + shared keyed-mutex bridge
// ===========================================================================

/// Read the PCI `VendorId` of the adapter backing a D3D11 device. We walk
/// device → IDXGIDevice → IDXGIAdapter → GetDesc. Used to decide fast path
/// (capture already on NVIDIA) vs hybrid (AMD/Intel display owner + NVIDIA NVENC).
pub(super) unsafe fn device_vendor_id(device: &ID3D11Device) -> Option<u32> {
	use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice};
	// ID3D11Device QIs to IDXGIDevice; GetAdapter → GetDesc carries the VendorId.
	let dxgi_dev: IDXGIDevice = device.cast().ok()?;
	let adapter: IDXGIAdapter = dxgi_dev.GetAdapter().ok()?;
	let desc = adapter.GetDesc().ok()?;
	Some(desc.VendorId)
}

/// PCI vendor ids of the hardware GPUs in this machine (0x10DE NVIDIA, 0x1002 AMD, 0x8086
/// Intel), in adapter order; the software (WARP) adapter is left out. Enumeration only — no
/// device is created, so it answers at once even while a driver is not responding.
pub(crate) fn hardware_gpu_vendors() -> Vec<u32> {
	let mut out = Vec::new();
	// SAFETY: plain DXGI enumeration on a factory owned by this function.
	unsafe {
		let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else { return out };
		let mut i = 0u32;
		while let Ok(adapter) = factory.EnumAdapters1(i) {
			i += 1;
			let Ok(desc) = adapter.GetDesc1() else { continue };
			if (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0 {
				continue;
			}
			if !out.contains(&desc.VendorId) {
				out.push(desc.VendorId);
			}
		}
	}
	out
}

/// Enumerate adapters and create a D3D11 device on the FIRST hardware NVIDIA adapter
/// (VendorId 0x10DE), with `BGRA_SUPPORT | VIDEO_SUPPORT` (the VideoProcessor needs
/// VIDEO_SUPPORT — the AMD capture device in dxgi.rs does NOT set it). This is the
/// device the whole NVENC + VideoProcessor + NV12 chain runs on in the hybrid case.
/// Returns Err (→ host falls back to ffmpeg) if there is no usable NVIDIA adapter.
pub(super) unsafe fn create_nvidia_device() -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
	let factory: IDXGIFactory1 =
		CreateDXGIFactory1().map_err(|e| format!("CreateDXGIFactory1: {e}"))?;
	let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];

	let mut i = 0u32;
	loop {
		let adapter = match factory.EnumAdapters1(i) {
			Ok(a) => a,
			// DXGI_ERROR_NOT_FOUND = enumerated all adapters without finding NVIDIA.
			Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => {
				return Err("no NVIDIA adapter found for NVENC".into());
			}
			Err(e) => return Err(format!("EnumAdapters1({i}): {e}")),
		};
		i += 1;

		let desc = match adapter.GetDesc1() {
			Ok(d) => d,
			Err(_) => continue,
		};
		let name = String::from_utf16_lossy(
			&desc.Description[..desc.Description.iter().position(|&c| c == 0).unwrap_or(0)],
		);
		cap_dbg(&format!(
			"adapter[{}] vendor=0x{:04X} device=0x{:04X} flags=0x{:X} \"{}\"",
			i - 1,
			desc.VendorId,
			desc.DeviceId,
			desc.Flags,
			name
		));
		// Skip the WARP/software adapter (DXGI_ADAPTER_FLAG_SOFTWARE bit) — NVENC needs a
		// real NVIDIA GPU. `.0` is i32; mask against the flags u32.
		if (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0 {
			continue;
		}
		if desc.VendorId != VENDOR_NVIDIA {
			continue;
		}
		cap_dbg(&format!("→ picked NVIDIA adapter \"{}\" for NVENC", name));

		// Found an NVIDIA adapter — create the device. DRIVER_TYPE_UNKNOWN is mandatory
		// when an explicit adapter is passed. VIDEO_SUPPORT is REQUIRED (VideoProcessor).
		let mut dev: Option<ID3D11Device> = None;
		let mut ctx: Option<ID3D11DeviceContext> = None;
		match D3D11CreateDevice(
			&adapter,
			D3D_DRIVER_TYPE_UNKNOWN,
			HMODULE::default(),
			D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
			Some(&levels),
			D3D11_SDK_VERSION,
			Some(&mut dev),
			None,
			Some(&mut ctx),
		) {
			Ok(()) => {
				let dev = dev.ok_or("D3D11CreateDevice(NVIDIA) returned null device")?;
				let ctx = ctx.ok_or("D3D11CreateDevice(NVIDIA) returned null context")?;
				return Ok((dev, ctx));
			}
			// This NVIDIA adapter rejected the device (e.g. no video support) — keep
			// scanning in case a second NVIDIA adapter works, else the loop returns Err.
			Err(_) => continue,
		}
	}
}

/// Cross-adapter bridge via a CPU-staging roundtrip (GPU-AGNOSTIC). AMD copies the captured
/// frame into a CPU-readable STAGING texture; `submit` maps it and uploads the pixels to the
/// NVIDIA BGRA (DEFAULT) the VideoProcessor reads. Replaces the keyed-mutex shared-texture path:
/// D3D11 cross-adapter shared keyed-mutex resources are NOT supported on all AMD↔NVIDIA combos
/// (`OpenSharedResource1` → E_INVALIDARG, e.g. this RTX3080 laptop's iGPU↔dGPU), so the CPU hop is
/// the reliable universal path for ANY display-owner GPU. Costs a per-frame readback+upload (a few
/// ms at 1080p); the same-GPU fast path avoids the bridge entirely.
pub(super) struct CrossAdapterBridge {
	/// AMD STAGING (CPU_READ) — the captured `frame.texture` is CopyResource'd here, then mapped.
	pub(super) amd_shared: ID3D11Texture2D,
	pub(super) amd_context: ID3D11DeviceContext,
	/// NVIDIA DEFAULT BGRA — the mapped pixels are uploaded here; the VideoProcessor's input.
	pub(super) nvidia_bgra: ID3D11Texture2D,
	pub(super) kept_amd_device: ID3D11Device,
	pub(super) kept_amd_context: ID3D11DeviceContext,
}

impl CrossAdapterBridge {
	/// Create the shared BGRA on the AMD (capture) device, open it on the NVIDIA device,
	/// and allocate the NVIDIA-side staging. `amd_device`/`amd_ctx` are the capture device;
	/// `nv_device` is the NVENC device; `w`/`h` is the bridge surface size.
	pub(super) unsafe fn create(
		amd_device: &ID3D11Device,
		amd_ctx: &ID3D11DeviceContext,
		nv_device: &ID3D11Device,
		w: u32,
		h: u32,
	) -> Result<Self, String> {
		// -- 1. AMD STAGING texture (CPU-readable). Each tick the captured BGRA `frame.texture`
		//    is CopyResource'd into this on the AMD device, then mapped for readback. STAGING
		//    can carry NO bind flags. This is the cross-adapter hop's source-of-pixels.
		let staging_desc = D3D11_TEXTURE2D_DESC {
			Width: w,
			Height: h,
			MipLevels: 1,
			ArraySize: 1,
			Format: DXGI_FORMAT_B8G8R8A8_UNORM,
			SampleDesc: DXGI_SAMPLE_DESC {
				Count: 1,
				Quality: 0,
			},
			Usage: D3D11_USAGE_STAGING,
			BindFlags: 0,
			CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
			MiscFlags: 0,
		};
		let mut amd_staging: Option<ID3D11Texture2D> = None;
		amd_device
			.CreateTexture2D(&staging_desc, None, Some(&mut amd_staging))
			.map_err(|e| format!("CreateTexture2D(AMD staging): {e}"))?;
		let amd_staging = amd_staging.ok_or("AMD staging texture null")?;

		// -- 2. NVIDIA BGRA (DEFAULT, RT|SRV). `submit` uploads the AMD-mapped pixels here
		//    (UpdateSubresource on the NVIDIA context) and the VideoProcessor reads it as the
		//    BGRA→NV12 source. No share flags — it lives entirely on the NVIDIA device.
		let nv_desc = D3D11_TEXTURE2D_DESC {
			Width: w,
			Height: h,
			MipLevels: 1,
			ArraySize: 1,
			Format: DXGI_FORMAT_B8G8R8A8_UNORM,
			SampleDesc: DXGI_SAMPLE_DESC {
				Count: 1,
				Quality: 0,
			},
			Usage: D3D11_USAGE_DEFAULT,
			BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
			CPUAccessFlags: 0,
			MiscFlags: 0,
		};
		let mut nvidia_bgra: Option<ID3D11Texture2D> = None;
		nv_device
			.CreateTexture2D(&nv_desc, None, Some(&mut nvidia_bgra))
			.map_err(|e| format!("CreateTexture2D(NVIDIA BGRA): {e}"))?;
		let nvidia_bgra = nvidia_bgra.ok_or("NVIDIA BGRA null")?;

		Ok(CrossAdapterBridge {
			amd_shared: amd_staging,
			// AddRef the AMD capture device/context so the staging stays valid for the
			// encoder's whole life (balanced in flush_and_close).
			amd_context: amd_ctx.clone(),
			nvidia_bgra,
			kept_amd_device: amd_device.clone(),
			kept_amd_context: amd_ctx.clone(),
		})
	}
}
