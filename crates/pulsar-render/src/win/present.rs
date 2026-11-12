//! NV12 → swapchain RGB present via `ID3D11VideoProcessor` (GPU colour-convert + scale, the
//! reverse of `pulsar-capture`'s BGRA→NV12 Blt). Zero-copy: the decoder's NV12 texture is the
//! VideoProcessor input; the swapchain back buffer is the output. Letterboxed to preserve aspect.

#![allow(dead_code)]

use windows::core::{Interface, Result};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
	ID3D11Device, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor,
	ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
	D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
	D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
	D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
	D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_ROTATION_180,
	D3D11_VIDEO_PROCESSOR_ROTATION_270, D3D11_VIDEO_PROCESSOR_ROTATION_90,
	D3D11_VIDEO_PROCESSOR_ROTATION_IDENTITY, D3D11_VIDEO_PROCESSOR_STREAM,
	D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
	D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_RATIONAL;

/// View-fit mode for presenting the video (AnyDesk-style), mirroring the Linux
/// backend's `video::FIT_MODE`: 0 = FIT (letterbox, default), 1 = STRETCH (fill,
/// may distort), 2 = ORIGINAL (1:1 source pixels, centered — larger streams crop).
/// The overlay's Display section / the frontend set it (`fit` stdin line or a
/// local `ov set fit` echo); `dest_rect` reads it every Blt.
static FIT_MODE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub fn set_fit(mode: &str) {
	let v = match mode {
		"stretch" => 1,
		"original" => 2,
		_ => 0,
	};
	FIT_MODE.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// Host display rotation (degrees CW: 0/90/180/270) the stream still carries — the host's
/// ffmpeg `ddagrab` path streams the UNROTATED Desktop Duplication surface and reports the
/// display orientation over `DataMsg::DisplayRotation` (the app forwards it as `rotate <deg>`).
/// The Blt rotates the frame CW by this — the same convention `pulsar-capture` uses when it
/// bakes the rotation host-side — so a rotated / portrait host shows upright. 0 = identity.
static ROTATION: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub fn set_rotation(deg: u32) {
	let d = match deg % 360 {
		90 => 90,
		180 => 180,
		270 => 270,
		_ => 0,
	};
	ROTATION.store(d, std::sync::atomic::Ordering::Relaxed);
}

pub fn rotation() -> u32 {
	ROTATION.load(std::sync::atomic::Ordering::Relaxed)
}

/// Upright (DISPLAYED) size of an `iw×ih` decoded frame under the current rotation —
/// 90/270 swap the axes. Everything the viewer sees (fit rects, cursor, reported stream
/// dims for the input letterbox) works in this space.
pub fn displayed_dims(iw: u32, ih: u32) -> (u32, u32) {
	match rotation() {
		90 | 270 => (ih, iw),
		_ => (iw, ih),
	}
}

pub fn fit_label() -> &'static str {
	match FIT_MODE.load(std::sync::atomic::Ordering::Relaxed) {
		1 => "stretch",
		2 => "original",
		_ => "fit",
	}
}

/// Fit-mode (source, dest) rect pair of an `iw×ih` source inside `ow×oh` —
/// shared by the GPU Blt below and the side-cursor math in `win/mod.rs` so the
/// cursor always lands where the frame is really drawn. Both rects MUST stay
/// inside their surfaces: a dest rect hanging outside the swapchain makes
/// `VideoProcessorBlt` fail every frame, which froze the whole render loop when
/// 1:1 was picked on a window smaller than the stream — so ORIGINAL is a center
/// CROP (source rect shrinks) rather than an oversized dest rect.
pub fn fit_rects(
	iw: f32,
	ih: f32,
	ow: f32,
	oh: f32,
) -> ((f32, f32, f32, f32), (f32, f32, f32, f32)) {
	if iw <= 0.0 || ih <= 0.0 {
		return ((0.0, 0.0, 1.0, 1.0), (0.0, 0.0, ow, oh));
	}
	match FIT_MODE.load(std::sync::atomic::Ordering::Relaxed) {
		1 => ((0.0, 0.0, iw, ih), (0.0, 0.0, ow, oh)),
		2 => {
			let vw = iw.min(ow);
			let vh = ih.min(oh);
			(
				(((iw - vw) / 2.0).round(), ((ih - vh) / 2.0).round(), vw, vh),
				(((ow - vw) / 2.0).round(), ((oh - vh) / 2.0).round(), vw, vh),
			)
		}
		_ => {
			let scale = (ow / iw).min(oh / ih);
			let w = (iw * scale).round();
			let h = (ih * scale).round();
			(
				(0.0, 0.0, iw, ih),
				(((ow - w) / 2.0).round(), ((oh - h) / 2.0).round(), w, h),
			)
		}
	}
}

/// Picture upscaling (Settings → Görüntü → Görüntü iyileştirme), stdin `upscale <mode>`:
/// 0 off · 1 auto (the GPU's AI upscaler if it has one, else FSR) · 2 FSR · 3 AI (NVIDIA RTX
/// Video Super Resolution / Intel VSR; FSR where the GPU has none).
static UPSCALE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub fn set_upscale(mode: &str) {
	let v = match mode.trim() {
		"auto" => 1,
		"fsr" => 2,
		"ai" => 3,
		_ => 0,
	};
	UPSCALE.store(v, std::sync::atomic::Ordering::Relaxed);
}

const VENDOR_NVIDIA: u32 = 0x10DE;
const VENDOR_INTEL: u32 = 0x8086;

/// The upscaler a frame uses, given the setting and the GPU.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Upscaler {
	None,
	Fsr,
	/// The driver's own (VideoProcessor stream extension).
	Vendor,
}

fn pick(mode: u8, vendor: u32) -> Upscaler {
	let ai = vendor == VENDOR_NVIDIA || vendor == VENDOR_INTEL;
	match mode {
		1 | 3 if ai => Upscaler::Vendor,
		1..=3 => Upscaler::Fsr,
		_ => Upscaler::None,
	}
}

/// GPU NV12→RGB scaler/letterboxer for the present path.
pub struct Present {
	vdevice: ID3D11VideoDevice,
	vctx: ID3D11VideoContext,
	vp_enum: ID3D11VideoProcessorEnumerator,
	vproc: ID3D11VideoProcessor,
	in_w: u32,
	in_h: u32,
	out_w: u32,
	out_h: u32,
	device: ID3D11Device,
	vendor: u32,
	/// The vendor upscaler as last set on `vproc` (re-applied on a rebuild / mode change).
	vendor_on: std::cell::Cell<bool>,
	/// FSR passes (built on first use).
	fsr: std::cell::RefCell<Option<super::fsr::Fsr>>,
	fsr_failed: std::cell::Cell<bool>,
	logged: std::cell::Cell<Option<Upscaler>>,
}

impl Present {
	pub unsafe fn new(
		device: &ID3D11Device,
		in_w: u32,
		in_h: u32,
		out_w: u32,
		out_h: u32,
	) -> Result<Self> {
		let vdevice: ID3D11VideoDevice = device.cast()?;
		let ctx = device.GetImmediateContext()?;
		let vctx: ID3D11VideoContext = ctx.cast()?;
		let (vp_enum, vproc) = Self::build(&vdevice, in_w, in_h, out_w, out_h)?;
		let s = Self {
			vdevice,
			vctx,
			vp_enum,
			vproc,
			in_w,
			in_h,
			out_w,
			out_h,
			device: device.clone(),
			vendor: super::fsr::vendor(device),
			vendor_on: std::cell::Cell::new(false),
			fsr: std::cell::RefCell::new(None),
			fsr_failed: std::cell::Cell::new(false),
			logged: std::cell::Cell::new(None),
		};
		// Black bars for letterboxing.
		s.set_black_background();
		Ok(s)
	}

	unsafe fn build(
		vdevice: &ID3D11VideoDevice,
		in_w: u32,
		in_h: u32,
		out_w: u32,
		out_h: u32,
	) -> Result<(ID3D11VideoProcessorEnumerator, ID3D11VideoProcessor)> {
		let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
			InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
			InputFrameRate: DXGI_RATIONAL {
				Numerator: 60,
				Denominator: 1,
			},
			InputWidth: in_w.max(1),
			InputHeight: in_h.max(1),
			OutputFrameRate: DXGI_RATIONAL {
				Numerator: 60,
				Denominator: 1,
			},
			OutputWidth: out_w.max(1),
			OutputHeight: out_h.max(1),
			Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
		};
		let vp_enum = vdevice.CreateVideoProcessorEnumerator(&content)?;
		let vproc = vdevice.CreateVideoProcessor(&vp_enum, 0)?;
		Ok((vp_enum, vproc))
	}

	/// Set the video processor's output background color to opaque black (for letterbox bars).
	/// Must be called on `self` (uses `self.vctx` + `self.vproc`) — not inside `build()` which
	/// has no access to `vctx`.
	unsafe fn set_black_background(&self) {
		self.vctx.VideoProcessorSetOutputBackgroundColor(
			&self.vproc,
			false,
			&windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_COLOR {
				Anonymous: windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_COLOR_0 {
					RGBA: windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_COLOR_RGBA {
						R: 0.0,
						G: 0.0,
						B: 0.0,
						A: 1.0,
					},
				},
			},
		);
	}

	/// Rebuild the processor if the input (decoded) or output (window) size changed.
	pub unsafe fn resize(&mut self, in_w: u32, in_h: u32, out_w: u32, out_h: u32) -> Result<()> {
		if in_w == self.in_w && in_h == self.in_h && out_w == self.out_w && out_h == self.out_h {
			return Ok(());
		}
		let (e, p) = Self::build(&self.vdevice, in_w, in_h, out_w, out_h)?;
		self.vp_enum = e;
		self.vproc = p;
		// Re-apply the black background color: build() creates a fresh ID3D11VideoProcessor
		// which resets all state, so the letterbox bars would show swapchain garbage
		// (FLIP_DISCARD = back buffer undefined) without this call.
		self.set_black_background();
		// A fresh processor has no stream extension set.
		self.vendor_on.set(false);
		self.in_w = in_w;
		self.in_h = in_h;
		self.out_w = out_w;
		self.out_h = out_h;
		Ok(())
	}

	/// Fit-mode (source, dest) rects for the current sizes (see `fit_rects`). `fit_rects` works
	/// in the DISPLAYED (upright) space; the VideoProcessor's source rect is pre-rotation, so
	/// map the (displayed-space) source crop back onto the decoded frame.
	fn blt_rects(&self) -> (RECT, RECT) {
		let (s, d, _) = self.blt_rects_crop();
		(s, d)
	}

	/// `blt_rects` plus the source crop's DISPLAYED (upright) size — what an upscaler gets.
	fn blt_rects_crop(&self) -> (RECT, RECT, (u32, u32)) {
		let (iw, ih) = (self.in_w as f32, self.in_h as f32);
		let (vw, vh) = displayed_dims(self.in_w, self.in_h);
		let (s, d) = fit_rects(vw as f32, vh as f32, self.out_w as f32, self.out_h as f32);
		let crop = (s.2.round().max(1.0) as u32, s.3.round().max(1.0) as u32);
		let (x, y, w, h) = s;
		// Inverse of the CW rotation, per corner: displayed (X,Y) → input
		//   90:  (Y, ih − X)   180: (iw − X, ih − Y)   270: (iw − Y, X)
		let s = match rotation() {
			90 => (y, ih - (x + w), h, w),
			180 => (iw - (x + w), ih - (y + h), w, h),
			270 => (iw - (y + h), x, h, w),
			_ => s,
		};
		let to_rect = |(x, y, w, h): (f32, f32, f32, f32)| RECT {
			left: x as i32,
			top: y as i32,
			right: (x + w) as i32,
			bottom: (y + h) as i32,
		};
		(to_rect(s), to_rect(d), crop)
	}

	/// NVIDIA RTX Video Super Resolution / Intel VSR on this processor's stream (on or off).
	/// Driver extensions of the VideoProcessor (as Moonlight uses them): they only act when the
	/// Blt scales UP. Ignored by drivers / GPUs without them.
	unsafe fn set_vendor_upscale(&self, on: bool) {
		use windows::core::GUID;
		if self.vendor == VENDOR_NVIDIA {
			const NV_PPE: GUID = GUID::from_u128(0xd43ce1b3_1f4b_48ac_baee_c3c25375e6f7);
			// { version, method (2 = super resolution), enable }
			let data: [u32; 3] = [1, 2, on as u32];
			let _ = self.vctx.VideoProcessorSetStreamExtension(
				&self.vproc,
				0,
				&NV_PPE,
				std::mem::size_of_val(&data) as u32,
				data.as_ptr() as *const _,
			);
		} else if self.vendor == VENDOR_INTEL {
			const INTEL_VPE: GUID = GUID::from_u128(0xedd1d4b9_8659_4cbc_a4d6_9831a2163ac3);
			#[repr(C)]
			struct Ext {
				function: u32,
				param: *mut core::ffi::c_void,
			}
			let mut version: u32 = 3; // kIntelVpeVersion3
			let mut mode: u32 = if on { 1 } else { 0 }; // kIntelVpeModePreproc / none
			let mut scaling: u32 = if on { 2 } else { 0 }; // kIntelVpeScalingSuperResolution / default
			let ext = |function: u32, param: *mut u32| Ext { function, param: param as *mut _ };
			let v = ext(0x01, &mut version); // kIntelVpeFnVersion
			let _ = self.vctx.VideoProcessorSetOutputExtension(
				&self.vproc,
				&INTEL_VPE,
				std::mem::size_of::<Ext>() as u32,
				&v as *const Ext as *const _,
			);
			let m = ext(0x20, &mut mode); // kIntelVpeFnMode
			let _ = self.vctx.VideoProcessorSetOutputExtension(
				&self.vproc,
				&INTEL_VPE,
				std::mem::size_of::<Ext>() as u32,
				&m as *const Ext as *const _,
			);
			let sc = ext(0x37, &mut scaling); // kIntelVpeFnScaling
			let _ = self.vctx.VideoProcessorSetStreamExtension(
				&self.vproc,
				0,
				&INTEL_VPE,
				std::mem::size_of::<Ext>() as u32,
				&sc as *const Ext as *const _,
			);
		}
		self.vendor_on.set(on);
	}

	/// Where the video lands in the window, as fractions of the client area
	/// `(x0, y0, x1, y1)` (fit letterbox / stretch / rotation all applied). The app confines
	/// the cursor to exactly this area while control is engaged.
	pub fn dest_frac(&self) -> (f32, f32, f32, f32) {
		let (_, d) = self.blt_rects();
		let (w, h) = (self.out_w.max(1) as f32, self.out_h.max(1) as f32);
		(d.left as f32 / w, d.top as f32 / h, d.right as f32 / w, d.bottom as f32 / h)
	}

	/// Convert+scale the NV12 `src` onto the swapchain `back` buffer (RGB): one GPU Blt — with
	/// the GPU's own upscaler on it, or into a source-sized texture that FSR scales up.
	pub unsafe fn blt(&self, src: &ID3D11Texture2D, back: &ID3D11Texture2D) -> Result<()> {
		let (_, dst, crop) = self.blt_rects_crop();
		let up = (dst.right - dst.left) as u32 > crop.0 || (dst.bottom - dst.top) as u32 > crop.1;
		let mut which = pick(UPSCALE.load(std::sync::atomic::Ordering::Relaxed), self.vendor);
		if !up || (which == Upscaler::Fsr && self.fsr_failed.get()) {
			which = Upscaler::None;
		}
		if self.logged.get() != Some(which) {
			self.logged.set(Some(which));
			eprintln!(
				"pulsar-render(win): upscaler {which:?} (gpu vendor 0x{:04x}, {}x{} -> {}x{})",
				self.vendor,
				crop.0,
				crop.1,
				dst.right - dst.left,
				dst.bottom - dst.top
			);
		}
		let vendor_want = which == Upscaler::Vendor;
		if vendor_want != self.vendor_on.get() {
			self.set_vendor_upscale(vendor_want);
		}
		if which == Upscaler::Fsr {
			let mut slot = self.fsr.borrow_mut();
			if slot.is_none() {
				match super::fsr::Fsr::new(&self.device) {
					Ok(f) => *slot = Some(f),
					Err(e) => {
						eprintln!("pulsar-render(win): FSR unavailable ({e}) — plain scaling");
						self.fsr_failed.set(true);
						drop(slot);
						return self.blt_to(src, back, None);
					}
				}
			}
			let fsr = slot.as_mut().unwrap();
			let r = fsr
				.source(crop.0, crop.1)
				.and_then(|mid| self.blt_to(src, &mid, Some(crop)))
				.and_then(|_| fsr.run(back, dst));
			match r {
				Ok(()) => return Ok(()),
				Err(e) => {
					// Never a black picture over an upscaler: plain scaling from now on.
					eprintln!("pulsar-render(win): FSR pass failed ({e}) — plain scaling");
					self.fsr_failed.set(true);
				}
			}
		}
		self.blt_to(src, back, None)
	}

	/// One VideoProcessor Blt of `src` onto `out`: the fit/letterbox placement on the window, or
	/// (`crop` given) the source crop filling a crop-sized texture 1:1 (FSR's input).
	unsafe fn blt_to(&self, src: &ID3D11Texture2D, back: &ID3D11Texture2D, crop: Option<(u32, u32)>) -> Result<()> {
		// Input view onto the NV12 source.
		let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
			FourCC: 0,
			ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
			Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
				Texture2D: D3D11_TEX2D_VPIV {
					MipSlice: 0,
					ArraySlice: 0,
				},
			},
		};
		let mut iv: Option<ID3D11VideoProcessorInputView> = None;
		self.vdevice
			.CreateVideoProcessorInputView(src, &self.vp_enum, &in_desc, Some(&mut iv))?;
		let iv = iv.unwrap();

		// Output view onto the swapchain back buffer.
		let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
			ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
			Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
				Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
			},
		};
		let mut ov: Option<ID3D11VideoProcessorOutputView> = None;
		self.vdevice.CreateVideoProcessorOutputView(
			back,
			&self.vp_enum,
			&out_desc,
			Some(&mut ov),
		)?;
		let ov = ov.unwrap();

		// Fit-mode rects: source crop (1:1 on a smaller window) + dest placement.
		let (src_r, mut dst) = self.blt_rects();
		if let Some((w, h)) = crop {
			dst = RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
		}
		self.vctx
			.VideoProcessorSetStreamSourceRect(&self.vproc, 0, true, Some(&src_r));
		self.vctx
			.VideoProcessorSetStreamDestRect(&self.vproc, 0, true, Some(&dst));
		// Host-rotation correction (see ROTATION). Set every Blt: it's a cheap state write and
		// a rebuilt processor (resize) starts from IDENTITY.
		let rot = match rotation() {
			90 => D3D11_VIDEO_PROCESSOR_ROTATION_90,
			180 => D3D11_VIDEO_PROCESSOR_ROTATION_180,
			270 => D3D11_VIDEO_PROCESSOR_ROTATION_270,
			_ => D3D11_VIDEO_PROCESSOR_ROTATION_IDENTITY,
		};
		self.vctx
			.VideoProcessorSetStreamRotation(&self.vproc, 0, rot != D3D11_VIDEO_PROCESSOR_ROTATION_IDENTITY, rot);

		let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
			Enable: true.into(),
			OutputIndex: 0,
			InputFrameOrField: 0,
			PastFrames: 0,
			FutureFrames: 0,
			ppPastSurfaces: std::ptr::null_mut(),
			pInputSurface: std::mem::ManuallyDrop::new(Some(iv)),
			ppFutureSurfaces: std::ptr::null_mut(),
			ppPastSurfacesRight: std::ptr::null_mut(),
			pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
			ppFutureSurfacesRight: std::ptr::null_mut(),
		};
		let r = self
			.vctx
			.VideoProcessorBlt(&self.vproc, &ov, 0, std::slice::from_ref(&stream));
		std::mem::ManuallyDrop::drop(&mut stream.pInputSurface);
		std::mem::ManuallyDrop::drop(&mut stream.pInputSurfaceRight);
		r
	}
}
