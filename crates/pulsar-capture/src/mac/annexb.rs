//! VideoToolbox hands out access units in the MP4 layout: every NAL unit is preceded by its
//! length (big-endian, `len_size` bytes — 4 in practice) and the parameter sets live in the
//! format description instead of the stream. The RTP packetizer and every client decoder
//! expect Annex B: `00 00 00 01` before each NAL unit, the parameter sets in-band in front of
//! each keyframe. These helpers do that rewrite.

/// Start code written before every NAL unit.
pub const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Why a length-prefixed buffer could not be rewritten.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnnexBError {
	/// The NAL length field is not 1, 2, 3 or 4 bytes wide.
	BadLengthSize(usize),
	/// A length field or the NAL unit it announces runs past the end of the buffer.
	Truncated { offset: usize },
}

impl std::fmt::Display for AnnexBError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::BadLengthSize(n) => write!(f, "NAL length field of {n} bytes"),
			Self::Truncated { offset } => write!(f, "NAL unit truncated at byte {offset}"),
		}
	}
}

impl std::error::Error for AnnexBError {}

/// Append each parameter set (VPS/SPS/PPS, without start codes) to `out` as Annex B.
pub fn push_parameter_sets(sets: &[&[u8]], out: &mut Vec<u8>) {
	for s in sets.iter().filter(|s| !s.is_empty()) {
		out.extend_from_slice(&START_CODE);
		out.extend_from_slice(s);
	}
}

/// Append the NAL units of the length-prefixed `src` to `out` as Annex B. Empty NAL units are
/// skipped. Returns how many NAL units were written. On an error `out` keeps what was
/// appended before it.
pub fn length_prefixed_to_annexb(src: &[u8], len_size: usize, out: &mut Vec<u8>) -> Result<usize, AnnexBError> {
	if !(1..=4).contains(&len_size) {
		return Err(AnnexBError::BadLengthSize(len_size));
	}
	out.reserve(src.len() + src.len() / 64);
	let mut pos = 0usize;
	let mut count = 0usize;
	while pos < src.len() {
		if src.len() - pos < len_size {
			return Err(AnnexBError::Truncated { offset: pos });
		}
		let len = src[pos..pos + len_size]
			.iter()
			.fold(0usize, |acc, b| (acc << 8) | *b as usize);
		pos += len_size;
		if src.len() - pos < len {
			return Err(AnnexBError::Truncated { offset: pos });
		}
		if len > 0 {
			out.extend_from_slice(&START_CODE);
			out.extend_from_slice(&src[pos..pos + len]);
			count += 1;
		}
		pos += len;
	}
	Ok(count)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn rewrites_four_byte_lengths() {
		let src = [0, 0, 0, 2, 0x65, 0xaa, 0, 0, 0, 3, 0x41, 1, 2];
		let mut out = Vec::new();
		assert_eq!(length_prefixed_to_annexb(&src, 4, &mut out), Ok(2));
		assert_eq!(out, [0, 0, 0, 1, 0x65, 0xaa, 0, 0, 0, 1, 0x41, 1, 2]);
	}

	#[test]
	fn rewrites_two_byte_lengths_and_skips_empty_units() {
		let src = [0, 0, 0, 1, 0x09];
		let mut out = Vec::new();
		assert_eq!(length_prefixed_to_annexb(&src, 2, &mut out), Ok(1));
		assert_eq!(out, [0, 0, 0, 1, 0x09]);
	}

	#[test]
	fn reports_a_truncated_unit() {
		let mut out = Vec::new();
		let src = [0, 0, 0, 9, 0x65, 0xaa];
		assert_eq!(
			length_prefixed_to_annexb(&src, 4, &mut out),
			Err(AnnexBError::Truncated { offset: 4 })
		);
		let src = [0, 0];
		assert_eq!(
			length_prefixed_to_annexb(&src, 4, &mut out),
			Err(AnnexBError::Truncated { offset: 0 })
		);
	}

	#[test]
	fn refuses_a_bad_length_size() {
		let mut out = Vec::new();
		assert_eq!(
			length_prefixed_to_annexb(&[1, 2], 0, &mut out),
			Err(AnnexBError::BadLengthSize(0))
		);
		assert_eq!(
			length_prefixed_to_annexb(&[1, 2], 5, &mut out),
			Err(AnnexBError::BadLengthSize(5))
		);
	}

	#[test]
	fn parameter_sets_come_first_with_start_codes() {
		let mut out = Vec::new();
		push_parameter_sets(&[&[0x67, 1], &[], &[0x68, 2]], &mut out);
		length_prefixed_to_annexb(&[0, 0, 0, 1, 0x65], 4, &mut out).unwrap();
		assert_eq!(out, [0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65]);
	}
}
