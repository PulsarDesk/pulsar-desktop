//! Picture placement for the macOS renderer — the same rules as the Windows backend
//! (`win/present.rs`): view-fit mode, host rotation, upscaler choice, and the YUV→RGB
//! constants. Pure (no platform code), so main.rs also compiles it into the test build on every
//! host.

#![allow(dead_code)]

use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};

/// View-fit mode (`fit <mode>` stdin / `ov set fit`): 0 = fit (letterbox, default),
/// 1 = stretch, 2 = original (1:1, a centre crop when the window is smaller).
static FIT_MODE: AtomicU8 = AtomicU8::new(0);

pub fn set_fit(mode: &str) {
	let v = match mode.trim() {
		"stretch" => 1,
		"original" => 2,
		_ => 0,
	};
	FIT_MODE.store(v, Ordering::Relaxed);
}

pub fn fit_label() -> &'static str {
	match FIT_MODE.load(Ordering::Relaxed) {
		1 => "stretch",
		2 => "original",
		_ => "fit",
	}
}

/// Host display rotation (degrees CW) the stream still carries (`rotate <deg>`); the
/// renderer turns the picture upright by it, like the Windows VideoProcessor rotation.
static ROTATION: AtomicU32 = AtomicU32::new(0);

pub fn set_rotation(deg: u32) {
	let d = match deg % 360 {
		90 => 90,
		180 => 180,
		270 => 270,
		_ => 0,
	};
	ROTATION.store(d, Ordering::Relaxed);
}

pub fn rotation() -> u32 {
	ROTATION.load(Ordering::Relaxed)
}

/// Upright (displayed) size of an `iw×ih` decoded frame: 90/270 swap the axes.
pub fn displayed_dims(iw: u32, ih: u32) -> (u32, u32) {
	match rotation() {
		90 | 270 => (ih, iw),
		_ => (iw, ih),
	}
}

/// Fit-mode (source, dest) rects `(x, y, w, h)` of an `iw×ih` displayed picture in an
/// `ow×oh` surface (pixels) — identical to `win/present.rs::fit_rects`.
pub fn fit_rects(iw: f32, ih: f32, ow: f32, oh: f32) -> ((f32, f32, f32, f32), (f32, f32, f32, f32)) {
	if iw <= 0.0 || ih <= 0.0 {
		return ((0.0, 0.0, 1.0, 1.0), (0.0, 0.0, ow, oh));
	}
	match FIT_MODE.load(Ordering::Relaxed) {
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
			((0.0, 0.0, iw, ih), (((ow - w) / 2.0).round(), ((oh - h) / 2.0).round(), w, h))
		}
	}
}

/// Texture coordinates of the four dest-quad corners (top-left, top-right, bottom-left,
/// bottom-right) for the displayed-space source crop `s` of an `iw×ih` decoded (unrotated)
/// frame under rotation `rot`. Inverse of the CW rotation per corner, displayed (X, Y) → input:
/// 90: (Y, ih − X) · 180: (iw − X, ih − Y) · 270: (iw − Y, X).
pub fn quad_uvs(rot: u32, iw: f32, ih: f32, s: (f32, f32, f32, f32)) -> [[f32; 2]; 4] {
	let (x, y, w, h) = s;
	let corners = [(x, y), (x + w, y), (x, y + h), (x + w, y + h)];
	let (iw, ih) = (iw.max(1.0), ih.max(1.0));
	corners.map(|(dx, dy)| {
		let (px, py) = match rot {
			90 => (dy, ih - dx),
			180 => (iw - dx, ih - dy),
			270 => (iw - dy, dx),
			_ => (dx, dy),
		};
		[px / iw, py / ih]
	})
}

/// Picture upscaling (`upscale off|auto|fsr|ai|sgsr`), stored as on Windows: 0 off, 1 auto,
/// 2 fsr, 3 ai, 4 sgsr. See [`pick`] for what each one runs on macOS.
static UPSCALE: AtomicU8 = AtomicU8::new(0);

pub fn set_upscale(mode: &str) {
	// Diagnostics: `PULSAR_UPSCALE=off|auto|fsr|ai|sgsr` pins the mode over the app's setting.
	let forced = std::env::var("PULSAR_UPSCALE").ok();
	let mode = forced.as_deref().unwrap_or(mode);
	let v = match mode.trim() {
		"auto" => 1,
		"fsr" => 2,
		"ai" => 3,
		"sgsr" => 4,
		_ => 0,
	};
	UPSCALE.store(v, Ordering::Relaxed);
}

pub fn upscale_mode() -> u8 {
	UPSCALE.load(Ordering::Relaxed)
}

pub fn upscale_on() -> bool {
	upscale_mode() != 0
}

/// The upscaler a frame uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Upscaler {
	None,
	/// AMD FSR 1 (EASU + RCAS, the shaders of `win/fsr.rs` in MSL).
	Fsr,
	/// Apple's MetalFX spatial scaler (macOS 13+, where the GPU supports it) — the analogue of
	/// the driver video super resolution "ai" / "auto" use on Windows.
	MetalFx,
}

/// The upscaler for `mode` (see [`set_upscale`]), as `win/present.rs` `pick` does with the
/// vendor upscaler: auto and ai take MetalFX when it is available, else FSR; fsr takes FSR;
/// sgsr (not ported to Metal) takes FSR too. Nothing when the picture is not scaled up or the
/// chosen pass is unavailable.
pub fn pick(mode: u8, scales_up: bool, metalfx: bool, fsr: bool) -> Upscaler {
	if !scales_up {
		return Upscaler::None;
	}
	match mode {
		1 | 3 if metalfx => Upscaler::MetalFx,
		1..=4 if fsr => Upscaler::Fsr,
		_ => Upscaler::None,
	}
}

/// The name the overlay shows for the active upscaler ("" = none).
pub fn upscaler_label(u: Upscaler) -> &'static str {
	match u {
		Upscaler::None => "",
		Upscaler::Fsr => "FSR",
		Upscaler::MetalFx => "MetalFX",
	}
}

/// Whether the placement scales the source crop UP (the only case an upscaler acts on).
pub fn scales_up(src: (f32, f32, f32, f32), dst: (f32, f32, f32, f32)) -> bool {
	dst.2 > src.2.round() || dst.3 > src.3.round()
}

// The YCbCr → RGB maths are shared with the Linux renderer (`colour.rs`).
pub use crate::colour::{yuv_consts, Matrix};

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn quad_uvs_follow_the_rotation() {
		let full = (0.0, 0.0, 1920.0, 1080.0);
		assert_eq!(quad_uvs(0, 1920.0, 1080.0, full), [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]]);
		// 90° CW: the displayed picture is 1080 wide; its top-left is the input's bottom-left.
		let rot = (0.0, 0.0, 1080.0, 1920.0);
		assert_eq!(quad_uvs(90, 1920.0, 1080.0, rot), [[0.0, 1.0], [0.0, 0.0], [1.0, 1.0], [1.0, 0.0]]);
		assert_eq!(quad_uvs(180, 1920.0, 1080.0, full), [[1.0, 1.0], [0.0, 1.0], [1.0, 0.0], [0.0, 0.0]]);
		assert_eq!(quad_uvs(270, 1920.0, 1080.0, rot), [[1.0, 0.0], [1.0, 1.0], [0.0, 0.0], [0.0, 1.0]]);
	}

	#[test]
	fn limited_range_bt709_maps_black_and_white() {
		let (r, m) = yuv_consts(Matrix::Bt709, 8, false, false);
		let y = |code: f32| (code / 255.0) * r[0] + r[1];
		assert!(y(16.0).abs() < 1e-5 && (y(235.0) - 1.0).abs() < 1e-5);
		let c = |code: f32| (code / 255.0) * r[2] + r[3];
		assert!(c(128.0).abs() < 1e-5);
		assert!((m[0] - 1.5748).abs() < 1e-3 && (m[3] - 1.8556).abs() < 1e-3);
		assert!((m[1] - 0.1873).abs() < 1e-3 && (m[2] - 0.4681).abs() < 1e-3);
	}

	#[test]
	fn p010_white_is_white() {
		let (r, _) = yuv_consts(Matrix::Bt2020, 10, false, true);
		// Code 940 in the top 10 bits of a 16-bit texel.
		let v = (940u32 << 6) as f32 / 65535.0;
		assert!((v * r[0] + r[1] - 1.0).abs() < 1e-4);
	}

	#[test]
	fn upscale_modes_pick_metalfx_or_fsr() {
		set_upscale("ai");
		assert!(upscale_on());
		assert_eq!(upscale_mode(), 3);
		set_upscale("off");
		assert!(!upscale_on());
		// auto / ai: MetalFX when available, else FSR.
		assert_eq!(pick(1, true, true, true), Upscaler::MetalFx);
		assert_eq!(pick(3, true, true, true), Upscaler::MetalFx);
		assert_eq!(pick(1, true, false, true), Upscaler::Fsr);
		assert_eq!(pick(3, true, false, true), Upscaler::Fsr);
		// fsr / sgsr: FSR even when MetalFX exists.
		assert_eq!(pick(2, true, true, true), Upscaler::Fsr);
		assert_eq!(pick(4, true, true, true), Upscaler::Fsr);
		// Off, not scaled up, or nothing available: none.
		assert_eq!(pick(0, true, true, true), Upscaler::None);
		assert_eq!(pick(1, false, true, true), Upscaler::None);
		assert_eq!(pick(2, true, true, false), Upscaler::None);
		assert_eq!(upscaler_label(Upscaler::MetalFx), "MetalFX");
		assert!(scales_up((0.0, 0.0, 1280.0, 720.0), (0.0, 0.0, 1920.0, 1080.0)));
		assert!(!scales_up((0.0, 0.0, 1920.0, 1080.0), (0.0, 0.0, 1920.0, 1080.0)));
	}
}
