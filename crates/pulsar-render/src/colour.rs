//! Colour of a decoded picture — Pulsar's SDR rule on the client side.
//!
//! Every Pulsar host encodes SDR as **BT.709, limited (16–235) range** and says so in the
//! stream (H.264 / HEVC VUI, AV1 sequence header). Every client decodes what the stream says,
//! and BT.709 limited when it says nothing (an older host, an encoder that writes no colour
//! description — Windows' Media Foundation encoders on ARM64, for one). Each backend reads the
//! description from its decoder (libav `AVFrame`, Media Foundation output type, VideoToolbox
//! buffer attachments) and maps it here:
//!
//! * Linux: the GL shader constants ([`yuv_consts`]) and, for a composed dmabuf (rkmpp, the
//!   Pi), the `EGL_YUV_COLOR_SPACE_HINT_EXT` / `EGL_SAMPLE_RANGE_HINT_EXT` import hints.
//! * Windows: the VideoProcessor's input colour space (`win/present.rs`).
//! * macOS: the Metal shader constants (`mac/metal.rs`) — the same [`yuv_consts`].
//!
//! Pure Rust, unit-tested on every host.

/// The YCbCr → RGB matrix of a decoded picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Matrix {
	Bt601,
	Bt709,
	Bt2020,
}

/// A picture's colour description: matrix + range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Colour {
	pub matrix: Matrix,
	pub full_range: bool,
}

impl Default for Colour {
	/// What a Pulsar host sends, and what a stream that says nothing is taken to be.
	fn default() -> Self {
		Colour { matrix: Matrix::Bt709, full_range: false }
	}
}

impl Matrix {
	/// From an ITU-T H.273 `matrix_coefficients` code (the VUI / AV1 field; FFmpeg's
	/// `AVColorSpace` uses the same numbers): 1 = BT.709; 5 / 6 = BT.601 (BT.470BG /
	/// SMPTE 170M); 4 (FCC) is BT.601-like; 9 / 10 = BT.2020. Unspecified (2), reserved and
	/// anything else = BT.709.
	pub fn from_h273(code: u32) -> Self {
		match code {
			4..=6 => Matrix::Bt601,
			9 | 10 => Matrix::Bt2020,
			_ => Matrix::Bt709,
		}
	}
}

impl Colour {
	/// From FFmpeg's `AVFrame::colorspace` (H.273 numbers) and `AVFrame::color_range`
	/// (0 unspecified, 1 MPEG / limited, 2 JPEG / full). `jpeg_fmt` = the frame's pixel format
	/// is one of the deprecated full-range `yuvj*` formats.
	pub fn from_av(colorspace: i32, color_range: i32, jpeg_fmt: bool) -> Self {
		Colour {
			matrix: Matrix::from_h273(colorspace.max(0) as u32),
			full_range: color_range == 2 || jpeg_fmt,
		}
	}

	/// From a Media Foundation output type: `MF_MT_YUV_MATRIX` (`MFVideoTransferMatrix`:
	/// 1 = BT.709, 2 = BT.601, 3 = SMPTE 240M, 4 / 5 = BT.2020) and `MF_MT_VIDEO_NOMINAL_RANGE`
	/// (`MFNominalRange`: 1 = 0–255, 2 = 16–235). `None` = the attribute is not set.
	pub fn from_mf(matrix: Option<u32>, nominal_range: Option<u32>) -> Self {
		Colour {
			matrix: match matrix {
				Some(2) => Matrix::Bt601,
				Some(4) | Some(5) => Matrix::Bt2020,
				_ => Matrix::Bt709,
			},
			full_range: nominal_range == Some(1),
		}
	}
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

/// EGL `EGL_EXT_image_dma_buf_import` hint values for a YUV dmabuf: `(colour space, range)`
/// for `EGL_YUV_COLOR_SPACE_HINT_EXT` / `EGL_SAMPLE_RANGE_HINT_EXT`. Without them the driver
/// converts with its default — BT.601 on Mesa / the Mali blob (the rkmpp / Pi picture).
pub fn egl_hints(c: Colour) -> (i32, i32) {
	const EGL_ITU_REC601_EXT: i32 = 0x327F;
	const EGL_ITU_REC709_EXT: i32 = 0x3280;
	const EGL_ITU_REC2020_EXT: i32 = 0x3281;
	const EGL_YUV_FULL_RANGE_EXT: i32 = 0x3282;
	const EGL_YUV_NARROW_RANGE_EXT: i32 = 0x3283;
	let space = match c.matrix {
		Matrix::Bt601 => EGL_ITU_REC601_EXT,
		Matrix::Bt709 => EGL_ITU_REC709_EXT,
		Matrix::Bt2020 => EGL_ITU_REC2020_EXT,
	};
	(space, if c.full_range { EGL_YUV_FULL_RANGE_EXT } else { EGL_YUV_NARROW_RANGE_EXT })
}

/// KMS plane properties `COLOR_ENCODING` / `COLOR_RANGE` (the kernel's `drm_color_encoding`
/// BT.601 = 0, BT.709 = 1, BT.2020 = 2; `drm_color_range` limited = 0, full = 1) for a YUV
/// framebuffer scanned out directly (the Pi's DRM-lease path).
pub fn kms_props(c: Colour) -> (u64, u64) {
	let enc = match c.matrix {
		Matrix::Bt601 => 0,
		Matrix::Bt709 => 1,
		Matrix::Bt2020 => 2,
	};
	(enc, u64::from(c.full_range))
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Apply the shader maths to 8-bit code values (what the GL / Metal shaders do).
	fn to_rgb(c: Colour, y: u8, cb: u8, cr: u8) -> [f32; 3] {
		let (r, m) = yuv_consts(c.matrix, 8, c.full_range, false);
		let n = |v: u8| v as f32 / 255.0;
		let yy = n(y) * r[0] + r[1];
		let u = n(cb) * r[2] + r[3];
		let v = n(cr) * r[2] + r[3];
		[yy + m[0] * v, yy - m[1] * u - m[2] * v, yy + m[3] * u]
	}

	fn close(a: [f32; 3], b: [f32; 3]) -> bool {
		a.iter().zip(b).all(|(x, y)| (x - y).abs() < 0.01)
	}

	/// Colour bars: the BT.709 limited-range code values a Pulsar host sends (what the host
	/// test in pulsar-core `sdr_colour_bars_through_ffmpeg` measures out of the real encoder)
	/// come back as the RGB the host captured.
	#[test]
	fn bt709_limited_bars_round_trip() {
		let c = Colour::default();
		for (yuv, rgb) in [
			((63, 102, 240), [1.0, 0.0, 0.0]),
			((173, 42, 26), [0.0, 1.0, 0.0]),
			((32, 240, 118), [0.0, 0.0, 1.0]),
			((235, 128, 128), [1.0, 1.0, 1.0]),
			((16, 128, 128), [0.0, 0.0, 0.0]),
		] {
			let got = to_rgb(c, yuv.0, yuv.1, yuv.2);
			assert!(close(got, rgb), "{yuv:?} → {got:?}, want {rgb:?}");
		}
		// The same red decoded with the BT.601 matrix is visibly off — the bug this fixes.
		let wrong = to_rgb(Colour { matrix: Matrix::Bt601, full_range: false }, 63, 102, 240);
		assert!(!close(wrong, [1.0, 0.0, 0.0]), "{wrong:?}");
		// BT.601 bars decode with the BT.601 matrix (a host that tags BT.601 still shows right).
		let c601 = Colour { matrix: Matrix::Bt601, full_range: false };
		assert!(close(to_rgb(c601, 81, 90, 240), [1.0, 0.0, 0.0]));
		// Full range: red = (54, 99, 255) in BT.709.
		let full = Colour { matrix: Matrix::Bt709, full_range: true };
		assert!(close(to_rgb(full, 54, 99, 255), [1.0, 0.0, 0.0]));
	}

	#[test]
	fn descriptions_map_to_the_rule() {
		// Unspecified / missing = BT.709 limited.
		assert_eq!(Colour::from_av(2, 0, false), Colour::default());
		assert_eq!(Colour::from_mf(None, None), Colour::default());
		assert_eq!(Colour::from_av(1, 1, false), Colour::default());
		assert_eq!(Colour::from_av(5, 1, false).matrix, Matrix::Bt601);
		assert_eq!(Colour::from_av(6, 2, false), Colour { matrix: Matrix::Bt601, full_range: true });
		assert_eq!(Colour::from_av(9, 1, false).matrix, Matrix::Bt2020);
		assert!(Colour::from_av(1, 0, true).full_range);
		assert_eq!(Colour::from_mf(Some(2), Some(2)), Colour { matrix: Matrix::Bt601, full_range: false });
		assert_eq!(Colour::from_mf(Some(1), Some(1)), Colour { matrix: Matrix::Bt709, full_range: true });
		assert_eq!(Colour::from_mf(Some(4), None).matrix, Matrix::Bt2020);
		assert_eq!(egl_hints(Colour::default()), (0x3280, 0x3283));
		assert_eq!(egl_hints(Colour { matrix: Matrix::Bt601, full_range: true }), (0x327F, 0x3282));
		assert_eq!(kms_props(Colour::default()), (1, 0));
		assert_eq!(kms_props(Colour { matrix: Matrix::Bt2020, full_range: true }), (2, 1));
	}
}
