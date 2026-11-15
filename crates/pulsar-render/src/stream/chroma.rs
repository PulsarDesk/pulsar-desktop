//! Which chroma format a stream carries, read from its sequence parameter set — so a renderer
//! can pick a 4:4:4-capable decode path for a 4:4:4 stream (Linux: libavcodec in software;
//! macOS: a 4:4:4 VideoToolbox output format) before the first picture, and keep its fastest
//! path for everything else. Pure, unit-tested on every host.

use super::Codec;

/// `chroma_format_idc` of the first SPS in the Annex-B access unit `au` (0 = monochrome,
/// 1 = 4:2:0, 2 = 4:2:2, 3 = 4:4:4). `None`: no SPS in it (a delta frame), AV1, or one we
/// cannot parse.
pub fn au_chroma_format(codec: Codec, au: &[u8]) -> Option<u32> {
	nal_units(au).find_map(|nal| match codec {
		Codec::H264 if nal.first().map(|b| b & 0x1F) == Some(7) => sps_chroma_format(codec, nal),
		Codec::H265 if nal.first().map(|b| (b >> 1) & 0x3F) == Some(33) => sps_chroma_format(codec, nal),
		_ => None,
	})
}

/// True when `au` carries an SPS that says 4:4:4.
pub fn au_is_444(codec: Codec, au: &[u8]) -> bool {
	au_chroma_format(codec, au) == Some(3)
}

/// `chroma_format_idc` of one SPS NAL unit (header included, no start code).
pub fn sps_chroma_format(codec: Codec, nal: &[u8]) -> Option<u32> {
	match codec {
		Codec::H264 => {
			let rbsp = unescape(nal.get(1..)?);
			let mut r = Bits::new(&rbsp);
			let profile_idc = r.u(8)?;
			r.skip(16)?; // constraint flags + reserved, level_idc
			r.ue()?; // seq_parameter_set_id
			// Only the high profiles code chroma_format_idc; every other one is 4:2:0.
			if matches!(profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
				r.ue()
			} else {
				Some(1)
			}
		}
		Codec::H265 => {
			let rbsp = unescape(nal.get(2..)?);
			let mut r = Bits::new(&rbsp);
			r.skip(4)?; // sps_video_parameter_set_id
			let max_sub_layers_minus1 = r.u(3)?;
			r.skip(1)?; // sps_temporal_id_nesting_flag
			// profile_tier_level(1, max_sub_layers_minus1)
			r.skip(88 + 8)?; // general profile (88 bits) + general_level_idc
			let mut present = Vec::new();
			for _ in 0..max_sub_layers_minus1 {
				present.push((r.u(1)? == 1, r.u(1)? == 1));
			}
			if max_sub_layers_minus1 > 0 {
				r.skip(2 * (8 - max_sub_layers_minus1))?; // reserved_zero_2bits
			}
			for (profile, level) in present {
				if profile {
					r.skip(88)?;
				}
				if level {
					r.skip(8)?;
				}
			}
			r.ue()?; // sps_seq_parameter_set_id
			r.ue()
		}
		Codec::Av1 => None,
	}
}

/// The NAL units of an Annex-B chunk (start codes stripped).
fn nal_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
	let mut starts = Vec::new();
	let mut i = 0;
	while i + 3 <= data.len() {
		if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
			starts.push(i + 3);
			i += 3;
		} else {
			i += 1;
		}
	}
	let ends: Vec<usize> = starts
		.iter()
		.skip(1)
		.map(|&s| {
			// The next start code begins 3 bytes before its payload (4 with a leading zero).
			let mut e = s - 3;
			while e > 0 && data[e - 1] == 0 {
				e -= 1;
			}
			e
		})
		.chain(std::iter::once(data.len()))
		.collect();
	starts.into_iter().zip(ends).filter(|(s, e)| s < e).map(move |(s, e)| &data[s..e])
}

/// RBSP from an escaped NAL payload (drops the emulation-prevention byte of each 00 00 03).
fn unescape(p: &[u8]) -> Vec<u8> {
	let mut out = Vec::with_capacity(p.len());
	let mut zeros = 0;
	for &b in p {
		if zeros >= 2 && b == 3 {
			zeros = 0;
			continue;
		}
		zeros = if b == 0 { zeros + 1 } else { 0 };
		out.push(b);
	}
	out
}

struct Bits<'a> {
	b: &'a [u8],
	pos: usize,
}

impl<'a> Bits<'a> {
	fn new(b: &'a [u8]) -> Self {
		Bits { b, pos: 0 }
	}
	fn u(&mut self, n: u32) -> Option<u32> {
		let mut v = 0u32;
		for _ in 0..n {
			let byte = *self.b.get(self.pos / 8)?;
			v = (v << 1) | ((byte >> (7 - self.pos % 8)) & 1) as u32;
			self.pos += 1;
		}
		Some(v)
	}
	fn skip(&mut self, n: u32) -> Option<()> {
		self.pos += n as usize;
		(self.pos <= self.b.len() * 8).then_some(())
	}
	/// Exp-Golomb `ue(v)`.
	fn ue(&mut self) -> Option<u32> {
		let mut zeros = 0;
		while self.u(1)? == 0 {
			zeros += 1;
			if zeros > 31 {
				return None;
			}
		}
		Some((1u32 << zeros) - 1 + self.u(zeros)?)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const H264_420: &[u8] = include_bytes!("../../testdata/test.h264");
	const H264_444: &[u8] = include_bytes!("../../testdata/test-444.h264");
	const H265_420: &[u8] = include_bytes!("../../testdata/test.h265");
	const H265_444: &[u8] = include_bytes!("../../testdata/test-444.h265");

	#[test]
	fn reads_the_chroma_format_of_the_fixtures() {
		assert_eq!(au_chroma_format(Codec::H264, H264_420), Some(1));
		assert_eq!(au_chroma_format(Codec::H264, H264_444), Some(3));
		assert_eq!(au_chroma_format(Codec::H265, H265_420), Some(1));
		assert_eq!(au_chroma_format(Codec::H265, H265_444), Some(3));
		assert!(au_is_444(Codec::H265, H265_444));
		assert!(!au_is_444(Codec::H264, H264_420));
	}

	#[test]
	fn no_sps_no_answer() {
		// A delta frame (no parameter sets), garbage, AV1.
		assert_eq!(au_chroma_format(Codec::H265, &[0, 0, 0, 1, 0x02, 0x01, 0xAA]), None);
		assert_eq!(au_chroma_format(Codec::H264, &[1, 2, 3]), None);
		assert_eq!(au_chroma_format(Codec::Av1, H265_444), None);
		// A truncated SPS.
		assert_eq!(au_chroma_format(Codec::H265, &[0, 0, 1, 0x42, 0x01, 0x01]), None);
	}

	#[test]
	fn unescapes_emulation_prevention() {
		assert_eq!(unescape(&[0, 0, 3, 1, 0, 0, 3, 0]), vec![0, 0, 1, 0, 0, 0]);
	}

	#[test]
	fn baseline_h264_is_420() {
		// profile_idc 66 (Baseline) codes no chroma_format_idc: 4:2:0. SPS: 67 42 00 1e + ue(0)=1.
		assert_eq!(sps_chroma_format(Codec::H264, &[0x67, 66, 0, 30, 0x80]), Some(1));
	}
}
