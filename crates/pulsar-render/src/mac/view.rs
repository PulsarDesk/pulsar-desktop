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

/// Picture upscaling (`upscale off|auto|fsr|ai|sgsr`). On macOS every mode but off is AMD
/// FSR 1 (EASU + RCAS, `mac/metal.rs`): there is no driver video super resolution to hand
/// "ai" to, and SGSR is not ported to Metal (it is experimental on Windows too).
static UPSCALE: AtomicU8 = AtomicU8::new(0);

pub fn set_upscale(mode: &str) {
	let forced = std::env::var("PULSAR_UPSCALE").ok();
	let mode = forced.as_deref().unwrap_or(mode);
	let v = match mode.trim() {
		"auto" | "fsr" | "ai" | "sgsr" => 1,
		_ => 0,
	};
	UPSCALE.store(v, Ordering::Relaxed);
}

pub fn upscale_on() -> bool {
	UPSCALE.load(Ordering::Relaxed) != 0
}

/// Whether the placement scales the source crop UP (the only case an upscaler acts on).
pub fn scales_up(src: (f32, f32, f32, f32), dst: (f32, f32, f32, f32)) -> bool {
	dst.2 > src.2.round() || dst.3 > src.3.round()
}

/// The colour matrix of a decoded picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Matrix {
	Bt601,
	Bt709,
	Bt2020,
}

/// Shader constants for YCbCr → RGB: `[ym, ya, cm, ca]` turn a sampled (normalized) luma /
/// chroma value into Y' 0..1 / C −0.5..0.5 (`Y = v·ym + ya`), `[a, b, c, d]` is the matrix
/// (`R = Y + a·Cr`, `G = Y − b·Cb − c·Cr`, `B = Y + d·Cb`). `msb16` = the samples sit in the
/// top bits of 16-bit texels (P010), so a sample reads as `code << (16 − bits)` / 65535.
pub fn yuv_consts(m: Matrix, bits: u32, full_range: bool, msb16: bool) -> ([f32; 4], [f32; 4]) {
	let bits = bits.clamp(8, 16);
	// A normalized texel value → the integer code value.
	let to_code = if msb16 { 65535.0 / (1u32 << (16 - bits)) as f32 } else { ((1u32 << bits) - 1) as f32 };
	let k = (1u32 << (bits - 8)) as f32;
	let max = ((1u32 << bits) - 1) as f32;
	let (ym, ya, cm, ca) = if full_range {
		(to_code / max, 0.0, to_code / max, -(128.0 * k) / max)
	} else {
		(to_code / (219.0 * k), -(16.0 * k) / (219.0 * k), to_code / (224.0 * k), -(128.0 * k) / (224.0 * k))
	};
	let (kr, kb): (f32, f32) = match m {
		Matrix::Bt601 => (0.299, 0.114),
		Matrix::Bt709 => (0.2126, 0.0722),
		Matrix::Bt2020 => (0.2627, 0.0593),
	};
	let kg = 1.0 - kr - kb;
	let a = 2.0 * (1.0 - kr);
	let d = 2.0 * (1.0 - kb);
	let b = d * kb / kg;
	let c = a * kr / kg;
	([ym, ya, cm, ca], [a, b, c, d])
}

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
	fn upscale_modes_all_mean_fsr() {
		set_upscale("ai");
		assert!(upscale_on());
		set_upscale("off");
		assert!(!upscale_on());
		assert!(scales_up((0.0, 0.0, 1280.0, 720.0), (0.0, 0.0, 1920.0, 1080.0)));
		assert!(!scales_up((0.0, 0.0, 1920.0, 1080.0), (0.0, 0.0, 1920.0, 1080.0)));
	}
}
