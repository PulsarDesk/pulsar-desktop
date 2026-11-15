//! HDR10 pieces of the native host path that need no graphics API (built and unit-tested on
//! every OS): the mastering-display / content-light metadata a host monitor reports, its
//! bitstream form (HEVC SEI, AV1 metadata OBUs), where it goes in an access unit, and the CPU
//! reference of the scRGB → BT.2020 PQ → 10-bit Y'CbCr conversion the GPU shader performs
//! (`encode/hdr_conv.rs`; its tests compare the GPU result against this).
//!
//! The HDR pipeline (Windows, NVENC): a monitor in HDR mode is captured as FP16 scRGB (linear
//! BT.709 primaries, 1.0 = 80 nits; DXGI `DuplicateOutput1` / WGC `R16G16B16A16Float`), turned
//! into P010 (BT.2020 non-constant luminance, PQ, limited range) on the GPU, encoded as HEVC
//! Main10 / AV1 10-bit with BT.2020 / PQ VUI, and every keyframe carries this metadata.

/// The HDR capability a host monitor reports (DXGI `DXGI_OUTPUT_DESC1`): its primaries and
/// white point (CIE 1931 xy) and luminance range (nits). Used as SMPTE ST 2086 mastering
/// display data + CTA-861.3 content light levels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HdrMeta {
	pub red: (f32, f32),
	pub green: (f32, f32),
	pub blue: (f32, f32),
	pub white: (f32, f32),
	pub min_nits: f32,
	pub max_nits: f32,
	pub max_full_frame_nits: f32,
}

impl Default for HdrMeta {
	/// A typical HDR10 desktop panel when the OS reports nothing usable: DCI-P3 D65 primaries,
	/// 0.005 – 1000 nits, 400 nits full frame.
	fn default() -> Self {
		Self {
			red: (0.680, 0.320),
			green: (0.265, 0.690),
			blue: (0.150, 0.060),
			white: (0.3127, 0.3290),
			min_nits: 0.005,
			max_nits: 1000.0,
			max_full_frame_nits: 400.0,
		}
	}
}

impl HdrMeta {
	/// Replace values the display left empty / out of range (some panels report zeros) with
	/// the defaults, so the stream never carries a 0-nit mastering display.
	pub fn sanitized(self) -> Self {
		let d = Self::default();
		let xy_ok = |p: (f32, f32)| p.0 > 0.0 && p.0 < 1.0 && p.1 > 0.0 && p.1 < 1.0;
		let prim_ok = xy_ok(self.red) && xy_ok(self.green) && xy_ok(self.blue) && xy_ok(self.white);
		let max = if self.max_nits >= 80.0 && self.max_nits <= 10_000.0 { self.max_nits } else { d.max_nits };
		let min = if self.min_nits >= 0.0 && self.min_nits < max { self.min_nits } else { d.min_nits };
		let full = if self.max_full_frame_nits >= 80.0 && self.max_full_frame_nits <= max {
			self.max_full_frame_nits
		} else {
			d.max_full_frame_nits.min(max)
		};
		Self {
			red: if prim_ok { self.red } else { d.red },
			green: if prim_ok { self.green } else { d.green },
			blue: if prim_ok { self.blue } else { d.blue },
			white: if prim_ok { self.white } else { d.white },
			min_nits: min,
			max_nits: max,
			max_full_frame_nits: full,
		}
	}
}

fn clamp_u16(v: f32) -> u16 {
	v.round().clamp(0.0, 65535.0) as u16
}

fn clamp_u32(v: f64) -> u32 {
	v.round().clamp(0.0, u32::MAX as f64) as u32
}

/// HEVC `mastering_display_colour_volume` (payload 137, 24 bytes) + `content_light_level_info`
/// (payload 144, 4 bytes) in one prefix SEI NAL unit (Annex B, start code included).
/// Chromaticities in 0.00002 units, primaries in G, B, R order (ST 2086 as HEVC carries it),
/// luminance in 0.0001 nit units.
pub fn hevc_hdr_sei(m: &HdrMeta) -> Vec<u8> {
	let m = m.sanitized();
	let xy = |p: (f32, f32)| [clamp_u16(p.0 * 50_000.0), clamp_u16(p.1 * 50_000.0)];
	let mut mdcv = Vec::with_capacity(24);
	for p in [m.green, m.blue, m.red, m.white] {
		for v in xy(p) {
			mdcv.extend_from_slice(&v.to_be_bytes());
		}
	}
	mdcv.extend_from_slice(&clamp_u32(m.max_nits as f64 * 10_000.0).to_be_bytes());
	mdcv.extend_from_slice(&clamp_u32(m.min_nits as f64 * 10_000.0).to_be_bytes());
	let mut cll = Vec::with_capacity(4);
	cll.extend_from_slice(&clamp_u16(m.max_nits).to_be_bytes());
	cll.extend_from_slice(&clamp_u16(m.max_full_frame_nits).to_be_bytes());

	let mut rbsp = Vec::with_capacity(40);
	for (ty, body) in [(137u8, &mdcv), (144u8, &cll)] {
		rbsp.push(ty);
		rbsp.push(body.len() as u8);
		rbsp.extend_from_slice(body);
	}
	rbsp.push(0x80); // rbsp_trailing_bits
	let mut nal = vec![0, 0, 0, 1, 0x4E, 0x01]; // prefix SEI (type 39), layer 0, temporal id 1
	push_emulation_prevented(&mut nal, &rbsp);
	nal
}

/// Copy `rbsp` into `out` inserting the 0x03 emulation-prevention byte wherever two zero
/// bytes would be followed by a byte ≤ 3 (H.264 / HEVC NAL rule).
fn push_emulation_prevented(out: &mut Vec<u8>, rbsp: &[u8]) {
	let mut zeros = 0;
	for &b in rbsp {
		if zeros >= 2 && b <= 3 {
			out.push(3);
			zeros = 0;
		}
		out.push(b);
		zeros = if b == 0 { zeros + 1 } else { 0 };
	}
}

fn leb128(mut v: usize, out: &mut Vec<u8>) {
	loop {
		let mut byte = (v & 0x7F) as u8;
		v >>= 7;
		if v != 0 {
			byte |= 0x80;
		}
		out.push(byte);
		if v == 0 {
			break;
		}
	}
}

fn read_leb128(b: &[u8]) -> Option<(usize, usize)> {
	let mut v: usize = 0;
	for (i, &byte) in b.iter().enumerate().take(8) {
		v |= ((byte & 0x7F) as usize) << (7 * i);
		if byte & 0x80 == 0 {
			return Some((v, i + 1));
		}
	}
	None
}

/// AV1 `METADATA_TYPE_HDR_MDCV` + `METADATA_TYPE_HDR_CLL` as two metadata OBUs (low-overhead
/// format, with size fields). Primaries in R, G, B order as 0.16 fixed point; max luminance
/// 24.8 fixed point, min 18.14 fixed point (AV1 spec 6.7.3 / 6.7.4).
pub fn av1_hdr_obus(m: &HdrMeta) -> Vec<u8> {
	let m = m.sanitized();
	let fx16 = |v: f32| clamp_u16(v * 65_536.0);
	let mut mdcv = vec![];
	leb128(2, &mut mdcv); // METADATA_TYPE_HDR_MDCV
	for p in [m.red, m.green, m.blue, m.white] {
		mdcv.extend_from_slice(&fx16(p.0).to_be_bytes());
		mdcv.extend_from_slice(&fx16(p.1).to_be_bytes());
	}
	mdcv.extend_from_slice(&clamp_u32(m.max_nits as f64 * 256.0).to_be_bytes());
	mdcv.extend_from_slice(&clamp_u32(m.min_nits as f64 * 16_384.0).to_be_bytes());
	mdcv.push(0x80); // trailing bits
	let mut cll = vec![];
	leb128(1, &mut cll); // METADATA_TYPE_HDR_CLL
	cll.extend_from_slice(&clamp_u16(m.max_nits).to_be_bytes());
	cll.extend_from_slice(&clamp_u16(m.max_full_frame_nits).to_be_bytes());
	cll.push(0x80);
	let mut out = Vec::with_capacity(48);
	for body in [mdcv, cll] {
		out.push((5 << 3) | 0x02); // OBU_METADATA, obu_has_size_field
		leb128(body.len(), &mut out);
		out.extend_from_slice(&body);
	}
	out
}

/// Where the metadata goes inside one encoded access unit: HEVC = before the first VCL NAL
/// (after the parameter sets the encoder repeats on keyframes); AV1 = before the first frame /
/// frame header / tile group OBU (after the temporal delimiter and sequence header). `None`
/// when the access unit has no such unit (nothing to attach to).
pub fn metadata_insert_offset(codec: crate::Codec, au: &[u8]) -> Option<usize> {
	match codec {
		crate::Codec::H265 => {
			let mut i = 0;
			while i + 4 < au.len() {
				let sc = if au[i..].starts_with(&[0, 0, 1]) {
					3
				} else if au[i..].starts_with(&[0, 0, 0, 1]) {
					4
				} else {
					i += 1;
					continue;
				};
				let ty = (au[i + sc] >> 1) & 0x3F;
				if ty < 32 {
					return Some(i);
				}
				i += sc + 1;
			}
			None
		}
		crate::Codec::Av1 => {
			let mut i = 0;
			while i < au.len() {
				let hdr = au[i];
				let ty = (hdr >> 3) & 0x0F;
				let ext = hdr & 0x04 != 0;
				let has_size = hdr & 0x02 != 0;
				if matches!(ty, 3 | 4 | 6) {
					return Some(i);
				}
				if !has_size {
					return None;
				}
				let h = 1 + ext as usize;
				let (size, n) = read_leb128(au.get(i + h..)?)?;
				i += h + n + size;
			}
			None
		}
		crate::Codec::H264 => None,
	}
}

/// `au` with the HDR metadata for `codec` inserted at [`metadata_insert_offset`] (unchanged
/// when there is no place for it or the codec carries none here).
pub fn with_hdr_metadata(codec: crate::Codec, au: &[u8], m: &HdrMeta) -> Vec<u8> {
	let Some(at) = metadata_insert_offset(codec, au) else {
		return au.to_vec();
	};
	let meta = match codec {
		crate::Codec::H265 => hevc_hdr_sei(m),
		crate::Codec::Av1 => av1_hdr_obus(m),
		crate::Codec::H264 => return au.to_vec(),
	};
	let mut out = Vec::with_capacity(au.len() + meta.len());
	out.extend_from_slice(&au[..at]);
	out.extend_from_slice(&meta);
	out.extend_from_slice(&au[at..]);
	out
}

// ── CPU reference of the GPU conversion (encode/hdr_conv.rs) ─────────────────────────────

/// SMPTE ST 2084 inverse EOTF: linear light in nits (0 – 10 000) → PQ signal 0 – 1.
pub fn pq_from_nits(nits: f32) -> f32 {
	let (m1, m2) = (0.159_301_76_f32, 78.843_75_f32);
	let (c1, c2, c3) = (0.835_937_5_f32, 18.851_562_f32, 18.6875_f32);
	let y = (nits / 10_000.0).clamp(0.0, 1.0).powf(m1);
	((c1 + c2 * y) / (1.0 + c3 * y)).powf(m2)
}

/// SMPTE ST 2084 EOTF: PQ signal 0 – 1 → nits.
pub fn nits_from_pq(e: f32) -> f32 {
	let (m1, m2) = (0.159_301_76_f32, 78.843_75_f32);
	let (c1, c2, c3) = (0.835_937_5_f32, 18.851_562_f32, 18.6875_f32);
	let p = e.clamp(0.0, 1.0).powf(1.0 / m2);
	10_000.0 * ((p - c1).max(0.0) / (c2 - c3 * p)).powf(1.0 / m1)
}

/// Linear BT.709 → linear BT.2020 primaries (ITU-R BT.2087).
pub const BT709_TO_BT2020: [[f32; 3]; 3] = [
	[0.627_403_9, 0.329_283_0, 0.043_313_1],
	[0.069_097_3, 0.919_540_4, 0.011_362_3],
	[0.016_391_4, 0.088_013_3, 0.895_595_3],
];

/// One scRGB pixel (linear BT.709, 1.0 = 80 nits) → 10-bit limited-range BT.2020 PQ Y'CbCr
/// codes `(Y, Cb, Cr)` — what the shader writes into P010 (before the 6-bit left shift).
pub fn scrgb_to_ycbcr10(rgb: [f32; 3]) -> (u16, u16, u16) {
	let m = BT709_TO_BT2020;
	let lin = |r: usize| (m[r][0] * rgb[0] + m[r][1] * rgb[1] + m[r][2] * rgb[2]).max(0.0) * 80.0;
	let (r, g, b) = (pq_from_nits(lin(0)), pq_from_nits(lin(1)), pq_from_nits(lin(2)));
	let y = 0.2627 * r + 0.6780 * g + 0.0593 * b;
	let cb = (b - y) / 1.8814;
	let cr = (r - y) / 1.4746;
	let q = |v: f32| v.round().clamp(0.0, 1023.0) as u16;
	(q(64.0 + 876.0 * y), q(512.0 + 896.0 * cb), q(512.0 + 896.0 * cr))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::Codec;

	#[test]
	fn pq_round_trips_and_hits_reference_points() {
		// 100 nits ≈ 0.508, 1000 nits ≈ 0.752, 10 000 nits = 1 (ST 2084 tables).
		assert!((pq_from_nits(100.0) - 0.508).abs() < 0.002);
		assert!((pq_from_nits(1000.0) - 0.752).abs() < 0.002);
		assert!((pq_from_nits(10_000.0) - 1.0).abs() < 1e-4);
		assert_eq!(pq_from_nits(0.0), pq_from_nits(-5.0));
		for n in [0.1f32, 1.0, 80.0, 203.0, 600.0, 4000.0] {
			assert!((nits_from_pq(pq_from_nits(n)) - n).abs() / n < 0.002, "{n}");
		}
	}

	#[test]
	fn scrgb_white_and_black_land_on_the_expected_codes() {
		// scRGB black → Y 64 (limited black, PQ 0 ≈ code 64), neutral chroma.
		let (y, cb, cr) = scrgb_to_ycbcr10([0.0, 0.0, 0.0]);
		assert_eq!((cb, cr), (512, 512));
		assert!(y <= 65, "{y}");
		// SDR reference white in HDR mode (203 nits = 2.5375 in scRGB) → PQ 0.58 → Y ≈ 572.
		let (y, cb, cr) = scrgb_to_ycbcr10([203.0 / 80.0; 3]);
		assert!((y as i32 - 572).abs() <= 2, "{y}");
		assert!((cb as i32 - 512).abs() <= 1 && (cr as i32 - 512).abs() <= 1, "{cb} {cr}");
		// Pure BT.709 red is inside BT.2020: Cr above neutral, Cb below.
		let (_, cb, cr) = scrgb_to_ycbcr10([1.0, 0.0, 0.0]);
		assert!(cr > 560 && cb < 512, "{cb} {cr}");
	}

	#[test]
	fn hevc_sei_has_both_payloads_and_no_start_code_inside() {
		let sei = hevc_hdr_sei(&HdrMeta::default());
		assert_eq!(&sei[..6], &[0, 0, 0, 1, 0x4E, 0x01]);
		// payload 137 size 24 first, then 144 size 4, then the trailing bits.
		let body = &sei[6..];
		assert_eq!(body[0], 137);
		assert_eq!(body[1], 24);
		assert_eq!(*body.last().unwrap(), 0x80);
		for w in body.windows(3) {
			// 00 00 03 is the escape itself; 00 00 00..02 would be a start code / forbidden.
			assert!(!(w[0] == 0 && w[1] == 0 && w[2] <= 2), "emulation prevention missing: {body:02x?}");
		}
		// Green x of P3 (0.265) in 0.00002 units = 13250 = 0x33C2 first.
		assert_eq!(&body[2..4], &13_250u16.to_be_bytes());
	}

	#[test]
	fn emulation_prevention_escapes_zero_runs() {
		let mut v = vec![];
		push_emulation_prevented(&mut v, &[0, 0, 1, 0, 0, 0, 0, 0, 4]);
		assert_eq!(v, vec![0, 0, 3, 1, 0, 0, 3, 0, 0, 3, 0, 4]);
	}

	#[test]
	fn sanitized_replaces_empty_reports() {
		let z = HdrMeta { red: (0.0, 0.0), green: (0.0, 0.0), blue: (0.0, 0.0), white: (0.0, 0.0), min_nits: 0.0, max_nits: 0.0, max_full_frame_nits: 0.0 };
		let s = z.sanitized();
		assert_eq!(s.max_nits, 1000.0);
		assert_eq!(s.red, HdrMeta::default().red);
		let real = HdrMeta { max_nits: 600.0, max_full_frame_nits: 350.0, ..Default::default() }.sanitized();
		assert_eq!((real.max_nits, real.max_full_frame_nits), (600.0, 350.0));
	}

	#[test]
	fn hevc_metadata_goes_after_the_parameter_sets() {
		// VPS(32) SPS(33) PPS(34) then an IDR slice (19).
		let au = [
			0, 0, 0, 1, 0x40, 0x01, 0xAA, //
			0, 0, 0, 1, 0x42, 0x01, 0xBB, //
			0, 0, 0, 1, 0x44, 0x01, 0xCC, //
			0, 0, 1, 0x26, 0x01, 0xDD, 0xEE,
		];
		assert_eq!(metadata_insert_offset(Codec::H265, &au), Some(21));
		let out = with_hdr_metadata(Codec::H265, &au, &HdrMeta::default());
		assert_eq!(&out[..21], &au[..21]);
		assert_eq!(&out[21..27], &[0, 0, 0, 1, 0x4E, 0x01]);
		assert!(out.ends_with(&au[21..]));
		// No VCL unit: unchanged.
		assert_eq!(with_hdr_metadata(Codec::H265, &au[..21], &HdrMeta::default()), au[..21].to_vec());
	}

	#[test]
	fn av1_metadata_goes_before_the_frame_obu() {
		// Temporal delimiter (2, size 0), sequence header (1, size 2), frame (6, size 3).
		let au = [0x12, 0x00, 0x0A, 0x02, 0xAA, 0xBB, 0x32, 0x03, 1, 2, 3];
		assert_eq!(metadata_insert_offset(Codec::Av1, &au), Some(6));
		let out = with_hdr_metadata(Codec::Av1, &au, &HdrMeta::default());
		// Walk the result: TD, SEQ, META, META, FRAME.
		let mut types = vec![];
		let mut i = 0;
		while i < out.len() {
			let ty = (out[i] >> 3) & 0x0F;
			types.push(ty);
			let (size, n) = read_leb128(&out[i + 1..]).unwrap();
			i += 1 + n + size;
		}
		assert_eq!(types, vec![2, 1, 5, 5, 6]);
		assert_eq!(i, out.len());
	}

	#[test]
	fn av1_mdcv_fixed_point() {
		let obus = av1_hdr_obus(&HdrMeta::default());
		assert_eq!(obus[0], 0x2A);
		let (size, n) = read_leb128(&obus[1..]).unwrap();
		let body = &obus[1 + n..1 + n + size];
		assert_eq!(body[0], 2); // MDCV
		// red x 0.680 in 0.16 = 44564
		assert_eq!(&body[1..3], &44_564u16.to_be_bytes());
		// max luminance 1000 nits in 24.8 = 256000
		assert_eq!(&body[17..21], &256_000u32.to_be_bytes());
		assert_eq!(*body.last().unwrap(), 0x80);
	}

	#[test]
	fn h264_gets_no_metadata() {
		let au = [0, 0, 0, 1, 0x65, 1, 2];
		assert_eq!(with_hdr_metadata(Codec::H264, &au, &HdrMeta::default()), au.to_vec());
	}
}
