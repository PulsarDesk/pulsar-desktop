//! Pure bitstream helpers for the macOS VideoToolbox decoder (`mac/decode.rs`): no platform
//! code, so they are unit-tested on every host (main.rs compiles this file into the test build
//! everywhere).
//!
//! VideoToolbox does not take the Annex-B stream the depacketizer (`stream/rtp.rs`) produces:
//!   - H.264 / HEVC: a `CMVideoFormatDescription` built from the parameter sets (SPS/PPS, plus
//!     the VPS for HEVC), and samples whose NAL units carry a 4-byte big-endian length instead
//!     of a start code (AVCC / HVCC). Parameter sets and access-unit delimiters stay out of the
//!     samples.
//!   - AV1: a format description with an `av1C` box (built from the sequence header OBU), and
//!     samples in the low-overhead OBU format with every OBU sized and without temporal
//!     delimiters (the ISO-BMFF AV1 sample rules).

#![allow(dead_code)]

use crate::stream::Codec;

/// The NAL units of an Annex-B buffer, start codes removed (both 3- and 4-byte start codes;
/// the zero byte that precedes a 4-byte start code is not part of the previous unit).
pub fn nal_units(data: &[u8]) -> Vec<&[u8]> {
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
	let mut out = Vec::with_capacity(starts.len());
	for (k, &s) in starts.iter().enumerate() {
		let mut end = if k + 1 < starts.len() { starts[k + 1] - 3 } else { data.len() };
		// Trailing zero bytes belong to the next (4-byte) start code / trailing_zero_8bits.
		while end > s && data[end - 1] == 0 {
			end -= 1;
		}
		if end > s {
			out.push(&data[s..end]);
		}
	}
	out
}

/// The kinds of NAL unit the decoder treats specially.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NalKind {
	Vps,
	Sps,
	Pps,
	/// Access-unit delimiter (never part of a sample).
	Aud,
	/// Anything else: slices, SEI, ...
	Other,
}

pub fn nal_kind(codec: Codec, nal: &[u8]) -> NalKind {
	let Some(&b0) = nal.first() else { return NalKind::Other };
	match codec {
		Codec::H264 => match b0 & 0x1f {
			7 => NalKind::Sps,
			8 => NalKind::Pps,
			9 => NalKind::Aud,
			_ => NalKind::Other,
		},
		Codec::H265 => match (b0 >> 1) & 0x3f {
			32 => NalKind::Vps,
			33 => NalKind::Sps,
			34 => NalKind::Pps,
			35 => NalKind::Aud,
			_ => NalKind::Other,
		},
		Codec::Av1 => NalKind::Other,
	}
}

/// One H.264 / HEVC access unit split for VideoToolbox: the parameter sets it carried (the
/// first of each kind) and the sample bytes (length-prefixed NAL units without them).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct NalSample {
	pub vps: Option<Vec<u8>>,
	pub sps: Option<Vec<u8>>,
	pub pps: Option<Vec<u8>>,
	/// 4-byte big-endian length + NAL, for every NAL unit that is not a parameter set or AUD.
	pub sample: Vec<u8>,
}

pub fn split_nal_au(codec: Codec, data: &[u8]) -> NalSample {
	let mut out = NalSample::default();
	for nal in nal_units(data) {
		match nal_kind(codec, nal) {
			NalKind::Vps => {
				out.vps.get_or_insert_with(|| nal.to_vec());
			}
			NalKind::Sps => {
				out.sps.get_or_insert_with(|| nal.to_vec());
			}
			NalKind::Pps => {
				out.pps.get_or_insert_with(|| nal.to_vec());
			}
			NalKind::Aud => {}
			NalKind::Other => {
				out.sample.extend_from_slice(&(nal.len() as u32).to_be_bytes());
				out.sample.extend_from_slice(nal);
			}
		}
	}
	out
}

// ---- AV1 -----------------------------------------------------------------------------------

pub const OBU_SEQUENCE_HEADER: u8 = 1;
pub const OBU_TEMPORAL_DELIMITER: u8 = 2;
pub const OBU_TILE_LIST: u8 = 8;
pub const OBU_PADDING: u8 = 15;

/// Read a LEB128 value (AV1 spec 4.10.5): (value, bytes used).
pub fn leb128(b: &[u8]) -> Option<(u64, usize)> {
	let mut v = 0u64;
	for i in 0..8 {
		let byte = *b.get(i)?;
		v |= ((byte & 0x7f) as u64) << (7 * i);
		if byte & 0x80 == 0 {
			return Some((v, i + 1));
		}
	}
	None
}

pub fn leb128_encode(mut v: u64, out: &mut Vec<u8>) {
	loop {
		let mut byte = (v & 0x7f) as u8;
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

/// One OBU of a temporal unit: its type, the header bytes (1 or 2, size flag as found), and
/// the payload.
#[derive(Debug, Clone, Copy)]
pub struct Obu<'a> {
	pub kind: u8,
	pub header: &'a [u8],
	pub payload: &'a [u8],
}

/// Walk the OBUs of a low-overhead temporal unit. An OBU without a size field runs to the end
/// of the buffer. Stops at the first malformed OBU.
pub fn obus(tu: &[u8]) -> Vec<Obu<'_>> {
	let mut out = Vec::new();
	let mut p = 0;
	while p < tu.len() {
		let b = tu[p];
		let kind = (b >> 3) & 0x0f;
		let ext = b & 0x04 != 0;
		let has_size = b & 0x02 != 0;
		let hlen = if ext { 2 } else { 1 };
		if p + hlen > tu.len() {
			break;
		}
		let header = &tu[p..p + hlen];
		let mut q = p + hlen;
		let len = if has_size {
			let Some((v, n)) = leb128(&tu[q..]) else { break };
			q += n;
			v as usize
		} else {
			tu.len() - q
		};
		if q + len > tu.len() {
			break;
		}
		out.push(Obu { kind, header, payload: &tu[q..q + len] });
		p = q + len;
	}
	out
}

/// Write one OBU with `obu_has_size_field` set.
fn push_sized_obu(o: &Obu<'_>, out: &mut Vec<u8>) {
	out.push(o.header[0] | 0x02);
	out.extend_from_slice(&o.header[1..]);
	leb128_encode(o.payload.len() as u64, out);
	out.extend_from_slice(o.payload);
}

/// The AV1 sample VideoToolbox takes: every OBU sized, temporal delimiters, tile lists and
/// padding dropped. Also returns the sequence header OBU (re-encoded sized) when the unit
/// carried one.
pub fn av1_sample(tu: &[u8]) -> (Vec<u8>, Option<Vec<u8>>) {
	let mut sample = Vec::with_capacity(tu.len());
	let mut seq = None;
	for o in obus(tu) {
		match o.kind {
			OBU_TEMPORAL_DELIMITER | OBU_TILE_LIST | OBU_PADDING => {}
			_ => {
				if o.kind == OBU_SEQUENCE_HEADER && seq.is_none() {
					let mut s = Vec::new();
					push_sized_obu(&o, &mut s);
					seq = Some(s);
				}
				push_sized_obu(&o, &mut sample);
			}
		}
	}
	(sample, seq)
}

/// MSB-first bit reader for the AV1 sequence header.
struct Bits<'a> {
	b: &'a [u8],
	pos: usize,
}

impl Bits<'_> {
	fn f(&mut self, n: u32) -> Option<u32> {
		let mut v = 0u32;
		for _ in 0..n {
			let byte = *self.b.get(self.pos / 8)?;
			let bit = (byte >> (7 - (self.pos % 8))) & 1;
			v = (v << 1) | bit as u32;
			self.pos += 1;
		}
		Some(v)
	}
	fn flag(&mut self) -> Option<bool> {
		self.f(1).map(|v| v == 1)
	}
	/// uvlc() (AV1 spec 4.10.3).
	fn uvlc(&mut self) -> Option<u32> {
		let mut lz = 0;
		while !self.flag()? {
			lz += 1;
			if lz >= 32 {
				return Some(u32::MAX);
			}
		}
		Some(self.f(lz)? + ((1u64 << lz) - 1) as u32)
	}
}

/// What the decoder needs from an AV1 sequence header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Av1Seq {
	pub profile: u8,
	pub level0: u8,
	pub tier0: u8,
	pub bit_depth: u8,
	pub mono: bool,
	pub subsampling_x: bool,
	pub subsampling_y: bool,
	pub chroma_sample_position: u8,
	pub max_width: u32,
	pub max_height: u32,
	/// matrix_coefficients (1 = BT.709, 6 = BT.601, 9 = BT.2020 NCL, 2 = unspecified).
	pub matrix: u8,
	pub full_range: bool,
}

/// Parse a sequence header OBU payload (AV1 spec 5.5).
pub fn parse_av1_seq(p: &[u8]) -> Option<Av1Seq> {
	let mut r = Bits { b: p, pos: 0 };
	let profile = r.f(3)? as u8;
	let _still = r.flag()?;
	let reduced = r.flag()?;
	let (level0, tier0);
	if reduced {
		level0 = r.f(5)? as u8;
		tier0 = 0;
	} else {
		let timing = r.flag()?;
		let mut decoder_model = false;
		let mut buffer_delay_len = 0;
		if timing {
			r.f(32)?;
			r.f(32)?;
			if r.flag()? {
				r.uvlc()?;
			}
			decoder_model = r.flag()?;
			if decoder_model {
				buffer_delay_len = r.f(5)? + 1;
				r.f(32)?;
				r.f(5)?;
				r.f(5)?;
			}
		}
		let initial_display_delay = r.flag()?;
		let ops = r.f(5)? + 1;
		let (mut l0, mut t0) = (0u8, 0u8);
		for i in 0..ops {
			r.f(12)?;
			let level = r.f(5)? as u8;
			let tier = if level > 7 { r.f(1)? as u8 } else { 0 };
			if decoder_model && r.flag()? {
				r.f(buffer_delay_len)?;
				r.f(buffer_delay_len)?;
				r.f(1)?;
			}
			if initial_display_delay && r.flag()? {
				r.f(4)?;
			}
			if i == 0 {
				l0 = level;
				t0 = tier;
			}
		}
		level0 = l0;
		tier0 = t0;
	}
	let wbits = r.f(4)? + 1;
	let hbits = r.f(4)? + 1;
	let max_width = r.f(wbits)? + 1;
	let max_height = r.f(hbits)? + 1;
	let frame_ids = if reduced { false } else { r.flag()? };
	if frame_ids {
		r.f(4)?;
		r.f(3)?;
	}
	r.f(3)?; // use_128x128_superblock, enable_filter_intra, enable_intra_edge_filter
	if !reduced {
		r.f(4)?; // interintra_compound, masked_compound, warped_motion, dual_filter
		let order_hint = r.flag()?;
		if order_hint {
			r.f(2)?; // jnt_comp, ref_frame_mvs
		}
		let choose_sct = r.flag()?;
		let force_sct = if choose_sct { 2 } else { r.f(1)? };
		if force_sct > 0 {
			let choose_imv = r.flag()?;
			if !choose_imv {
				r.f(1)?;
			}
		}
		if order_hint {
			r.f(3)?;
		}
	}
	r.f(3)?; // enable_superres, enable_cdef, enable_restoration
	// color_config()
	let high = r.flag()?;
	let bit_depth = if profile == 2 && high {
		if r.flag()? {
			12
		} else {
			10
		}
	} else if high {
		10
	} else {
		8
	};
	let mono = if profile == 1 { false } else { r.flag()? };
	let (mut cp, mut tc, mut mc) = (2u8, 2u8, 2u8);
	if r.flag()? {
		cp = r.f(8)? as u8;
		tc = r.f(8)? as u8;
		mc = r.f(8)? as u8;
	}
	let (full_range, ssx, ssy, csp);
	if mono {
		full_range = r.flag()?;
		ssx = true;
		ssy = true;
		csp = 0;
	} else if cp == 1 && tc == 13 && mc == 0 {
		full_range = true;
		ssx = false;
		ssy = false;
		csp = 0;
	} else {
		full_range = r.flag()?;
		let (x, y) = match profile {
			0 => (true, true),
			1 => (false, false),
			_ => {
				if bit_depth == 12 {
					let x = r.flag()?;
					let y = if x { r.flag()? } else { false };
					(x, y)
				} else {
					(true, false)
				}
			}
		};
		ssx = x;
		ssy = y;
		csp = if x && y { r.f(2)? as u8 } else { 0 };
	}
	Some(Av1Seq {
		profile,
		level0,
		tier0,
		bit_depth,
		mono,
		subsampling_x: ssx,
		subsampling_y: ssy,
		chroma_sample_position: csp,
		max_width,
		max_height,
		matrix: mc,
		full_range,
	})
}

/// The `av1C` box body (AV1-ISOBMFF 2.3.3) for a sequence header OBU (sized, as
/// [`av1_sample`] returns it): 4 bytes of config, then the OBU itself as `configOBUs`.
pub fn av1c(seq: &Av1Seq, seq_obu: &[u8]) -> Vec<u8> {
	let mut v = Vec::with_capacity(4 + seq_obu.len());
	v.push(0x81); // marker 1, version 1
	v.push((seq.profile << 5) | (seq.level0 & 0x1f));
	v.push(
		(seq.tier0 << 7)
			| (((seq.bit_depth > 8) as u8) << 6)
			| (((seq.bit_depth == 12) as u8) << 5)
			| ((seq.mono as u8) << 4)
			| ((seq.subsampling_x as u8) << 3)
			| ((seq.subsampling_y as u8) << 2)
			| (seq.chroma_sample_position & 3),
	);
	v.push(0); // no initial_presentation_delay
	v.extend_from_slice(seq_obu);
	v
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn annexb_splits_on_both_start_code_lengths() {
		let d = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9, 0];
		let n = nal_units(&d);
		assert_eq!(n, vec![&[0x67u8, 1, 2][..], &[0x68, 3][..], &[0x65, 9, 9][..]]);
	}

	#[test]
	fn h264_au_becomes_params_and_a_length_prefixed_sample() {
		let d = [0, 0, 0, 1, 0x09, 0xf0, 0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 9, 9];
		let s = split_nal_au(Codec::H264, &d);
		assert_eq!(s.sps.as_deref(), Some(&[0x67u8, 1, 2][..]));
		assert_eq!(s.pps.as_deref(), Some(&[0x68u8, 3][..]));
		assert_eq!(s.vps, None);
		assert_eq!(s.sample, vec![0, 0, 0, 3, 0x65, 9, 9]);
	}

	#[test]
	fn hevc_nal_types_come_from_the_six_bit_field() {
		assert_eq!(nal_kind(Codec::H265, &[32 << 1, 1]), NalKind::Vps);
		assert_eq!(nal_kind(Codec::H265, &[33 << 1, 1]), NalKind::Sps);
		assert_eq!(nal_kind(Codec::H265, &[34 << 1, 1]), NalKind::Pps);
		assert_eq!(nal_kind(Codec::H265, &[35 << 1, 1]), NalKind::Aud);
		assert_eq!(nal_kind(Codec::H265, &[19 << 1, 1]), NalKind::Other);
	}

	#[test]
	fn leb128_round_trips() {
		for v in [0u64, 1, 127, 128, 300, 1 << 20] {
			let mut b = Vec::new();
			leb128_encode(v, &mut b);
			assert_eq!(leb128(&b), Some((v, b.len())));
		}
	}

	/// A real 1080p 8-bit 4:2:0 sequence header (libaom/SVT style, profile 0, level 4.0 = 8).
	fn seq_payload_1080p() -> Vec<u8> {
		// Build it with a bit writer so the test documents the layout.
		struct W(Vec<u8>, usize);
		impl W {
			fn put(&mut self, v: u32, n: u32) {
				for i in (0..n).rev() {
					if self.1 % 8 == 0 {
						self.0.push(0);
					}
					let bit = ((v >> i) & 1) as u8;
					let last = self.0.len() - 1;
					self.0[last] |= bit << (7 - (self.1 % 8));
					self.1 += 1;
				}
			}
		}
		let mut w = W(Vec::new(), 0);
		w.put(0, 3); // profile
		w.put(0, 1); // still
		w.put(0, 1); // reduced
		w.put(0, 1); // timing_info_present
		w.put(0, 1); // initial_display_delay_present
		w.put(0, 5); // operating_points_cnt_minus_1
		w.put(0, 12); // idc
		w.put(8, 5); // seq_level_idx (4.0)
		w.put(0, 1); // seq_tier (level > 7)
		w.put(10, 4); // frame_width_bits_minus_1 (11 bits)
		w.put(10, 4); // frame_height_bits_minus_1
		w.put(1919, 11);
		w.put(1079, 11);
		w.put(0, 1); // frame_id_numbers_present
		w.put(0, 3); // sb128, filter_intra, intra_edge
		w.put(0, 4); // interintra, masked, warped, dual
		w.put(1, 1); // enable_order_hint
		w.put(0, 2); // jnt_comp, ref_frame_mvs
		w.put(1, 1); // seq_choose_screen_content_tools → force = 2
		w.put(1, 1); // seq_choose_integer_mv
		w.put(6, 3); // order_hint_bits_minus_1
		w.put(0, 3); // superres, cdef, restoration
		w.put(0, 1); // high_bitdepth
		w.put(0, 1); // mono_chrome
		w.put(1, 1); // color_description_present
		w.put(1, 8); // BT.709 primaries
		w.put(1, 8); // BT.709 transfer
		w.put(1, 8); // BT.709 matrix
		w.put(0, 1); // color_range (limited)
		w.put(0, 2); // chroma_sample_position
		w.put(0, 1); // film_grain_params_present
		w.0
	}

	#[test]
	fn av1_sequence_header_parses() {
		let s = parse_av1_seq(&seq_payload_1080p()).unwrap();
		assert_eq!((s.profile, s.level0, s.tier0, s.bit_depth), (0, 8, 0, 8));
		assert_eq!((s.max_width, s.max_height), (1920, 1080));
		assert!(s.subsampling_x && s.subsampling_y && !s.mono);
		assert_eq!((s.matrix, s.full_range), (1, false));
	}

	#[test]
	fn av1_sample_drops_delimiters_and_sizes_every_obu() {
		let seq = seq_payload_1080p();
		let mut tu = vec![0x12, 0x00]; // temporal delimiter, sized, empty
		tu.push(0x0a); // sequence header, sized
		leb128_encode(seq.len() as u64, &mut tu);
		tu.extend_from_slice(&seq);
		tu.push(0x30); // OBU_FRAME (6), NO size field: runs to the end
		tu.extend_from_slice(&[1, 2, 3]);
		let (sample, seq_obu) = av1_sample(&tu);
		let seq_obu = seq_obu.unwrap();
		assert_eq!(seq_obu[0], 0x0a);
		assert!(sample.starts_with(&seq_obu));
		assert_eq!(&sample[seq_obu.len()..], &[0x32, 3, 1, 2, 3]);
		let c = av1c(&parse_av1_seq(&seq).unwrap(), &seq_obu);
		assert_eq!(&c[..4], &[0x81, 0x08, 0x0c, 0x00]);
		assert_eq!(&c[4..], &seq_obu[..]);
	}
}
