//! HEVC loss concealment for intra-refresh streams on decoders that stop at a missing
//! reference (pure: no platform deps).
//!
//! An intra-refreshed stream has no keyframe in steady state, so the depacketizer drops the
//! access unit a hole damaged and resumes with the next whole one (`rtp::IR_MODE`). H.264
//! decoders conceal the missing reference (a frame_num gap). The Microsoft HEVC decoder MFT does
//! not: measured 2026-11-15 (Snapdragon X, x265 intra-refresh stream, 1-2 frames lost): after the
//! first hole it accepted every later access unit and output NOTHING, for good — a frozen
//! picture until the next IDR, which an intra-refresh stream never sends.
//!
//! [`Conceal`] keeps the decoder's reference chain whole instead: for every picture order count
//! the stream skipped it hands the decoder a stand-in — the last delivered picture's slices with
//! the missing POC written into their headers (`slice_pic_order_cnt_lsb`, a fixed-width field,
//! so nothing else in the slice moves). Its references are relative (`delta_poc`), so it predicts
//! from the last decoded picture and re-applies that picture's motion — a motion-extrapolated
//! guess at the lost frame. The next real picture then finds its reference, and the rolling intra
//! wave heals the smear within its period. Stand-ins are decoded, never shown.

use std::collections::HashMap;

/// Most stand-ins for one gap; a wider jump is not a short loss (an encoder restart resyncs on
/// its keyframe anyway).
pub const MAX_STAND_INS: u32 = 30;

/// Remove emulation-prevention bytes (`00 00 03` → `00 00`).
fn unescape(nal: &[u8]) -> Vec<u8> {
	let mut out = Vec::with_capacity(nal.len());
	let mut zeros = 0;
	for &b in nal {
		if zeros >= 2 && b == 3 {
			zeros = 0;
			continue;
		}
		zeros = if b == 0 { zeros + 1 } else { 0 };
		out.push(b);
	}
	out
}

/// Insert emulation-prevention bytes (inverse of [`unescape`]).
fn escape(rbsp: &[u8]) -> Vec<u8> {
	let mut out = Vec::with_capacity(rbsp.len() + 8);
	let mut zeros = 0;
	for &b in rbsp {
		if zeros >= 2 && b <= 3 {
			out.push(3);
			zeros = 0;
		}
		zeros = if b == 0 { zeros + 1 } else { 0 };
		out.push(b);
	}
	out
}

struct Bits<'a> {
	d: &'a [u8],
	pos: usize,
}

impl<'a> Bits<'a> {
	fn new(d: &'a [u8], pos: usize) -> Self {
		Self { d, pos }
	}
	fn u(&mut self, n: u32) -> Option<u32> {
		let mut v = 0u32;
		for _ in 0..n {
			let byte = *self.d.get(self.pos / 8)?;
			v = (v << 1) | ((byte >> (7 - self.pos % 8)) & 1) as u32;
			self.pos += 1;
		}
		Some(v)
	}
	fn skip(&mut self, n: usize) -> Option<()> {
		self.pos += n;
		(self.pos <= self.d.len() * 8).then_some(())
	}
	fn ue(&mut self) -> Option<u32> {
		let mut lz = 0;
		while self.u(1)? == 0 {
			lz += 1;
			if lz > 31 {
				return None;
			}
		}
		Some((1u32 << lz) - 1 + self.u(lz)?)
	}
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Sps {
	log2_max_poc_lsb: u32,
	separate_colour_plane: bool,
	pic_size_in_ctbs: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Pps {
	sps_id: u32,
	dependent_slices: bool,
	output_flag_present: bool,
	extra_bits: u32,
}

fn parse_sps(rbsp: &[u8]) -> Option<(u32, Sps)> {
	let mut b = Bits::new(rbsp, 16);
	b.skip(4)?; // sps_video_parameter_set_id
	let max_sub = b.u(3)?;
	b.skip(1)?;
	// profile_tier_level(1, max_sub)
	b.skip(96)?;
	let mut sub = [(false, false); 8];
	for s in sub.iter_mut().take(max_sub as usize) {
		*s = (b.u(1)? == 1, b.u(1)? == 1);
	}
	if max_sub > 0 {
		b.skip(2 * (8 - max_sub as usize))?;
	}
	for &(p, l) in sub.iter().take(max_sub as usize) {
		if p {
			b.skip(88)?;
		}
		if l {
			b.skip(8)?;
		}
	}
	let id = b.ue()?;
	let chroma = b.ue()?;
	let separate_colour_plane = chroma == 3 && b.u(1)? == 1;
	let w = b.ue()?;
	let h = b.ue()?;
	if b.u(1)? == 1 {
		for _ in 0..4 {
			b.ue()?;
		}
	}
	b.ue()?;
	b.ue()?;
	let log2_max_poc_lsb = b.ue()? + 4;
	let all = b.u(1)? == 1;
	for _ in (if all { 0 } else { max_sub })..=max_sub {
		b.ue()?;
		b.ue()?;
		b.ue()?;
	}
	let min_cb = b.ue()? + 3;
	let ctb = 1u32 << (min_cb + b.ue()?).min(16);
	let pic_size_in_ctbs = w.div_ceil(ctb) * h.div_ceil(ctb);
	(log2_max_poc_lsb <= 16).then_some((id, Sps { log2_max_poc_lsb, separate_colour_plane, pic_size_in_ctbs }))
}

fn parse_pps(rbsp: &[u8]) -> Option<(u32, Pps)> {
	let mut b = Bits::new(rbsp, 16);
	let id = b.ue()?;
	let sps_id = b.ue()?;
	let dependent_slices = b.u(1)? == 1;
	let output_flag_present = b.u(1)? == 1;
	let extra_bits = b.u(3)?;
	Some((id, Pps { sps_id, dependent_slices, output_flag_present, extra_bits }))
}

fn nal_type(nal: &[u8]) -> u8 {
	(nal[0] >> 1) & 0x3f
}

/// The parameter sets seen so far and the last delivered picture (see the module doc).
#[derive(Default)]
pub struct Conceal {
	sps: HashMap<u32, Sps>,
	pps: HashMap<u32, Pps>,
	/// Slices (VCL NALs, no start code) of the last delivered non-IRAP picture, its POC LSB, and
	/// its RTP timestamp.
	last: Option<(Vec<Vec<u8>>, u32, u32)>,
	/// POC LSB of the last delivered picture of any kind (an IRAP's too: the next gap is counted
	/// from it even though it cannot stand in).
	last_poc: Option<u32>,
}

impl Conceal {
	pub fn new() -> Self {
		Self::default()
	}

	/// Forget the last picture (a resync: the decoder restarts from a keyframe).
	pub fn reset(&mut self) {
		self.last = None;
		self.last_poc = None;
	}

	/// Where `slice_pic_order_cnt_lsb` sits in the RBSP of this slice NAL: (bit offset, width,
	/// value). None for an IDR, a dependent slice segment or unknown parameter sets.
	fn poc_field(&self, rbsp: &[u8]) -> Option<(usize, u32, u32)> {
		let t = nal_type(rbsp);
		if t > 31 || t == 19 || t == 20 {
			return None;
		}
		let mut b = Bits::new(rbsp, 16);
		let first = b.u(1)? == 1;
		if (16..=23).contains(&t) {
			b.skip(1)?;
		}
		let pps = *self.pps.get(&b.ue()?)?;
		let sps = *self.sps.get(&pps.sps_id)?;
		if !first {
			if pps.dependent_slices && b.u(1)? == 1 {
				return None; // dependent segment: its header is the previous segment's
			}
			let bits = 32 - (sps.pic_size_in_ctbs.max(2) - 1).leading_zeros();
			b.skip(bits as usize)?;
		}
		b.skip(pps.extra_bits as usize)?;
		b.ue()?; // slice_type
		if pps.output_flag_present {
			b.skip(1)?;
		}
		if sps.separate_colour_plane {
			b.skip(2)?;
		}
		let at = b.pos;
		Some((at, sps.log2_max_poc_lsb, b.u(sps.log2_max_poc_lsb)?))
	}

	/// The slice with its POC LSB replaced by `poc` (None if it has no POC field).
	fn with_poc(&self, nal: &[u8], poc: u32) -> Option<Vec<u8>> {
		let mut rbsp = unescape(nal);
		let (at, width, _) = self.poc_field(&rbsp)?;
		for i in 0..width as usize {
			let bit = (poc >> (width as usize - 1 - i)) & 1;
			let (byte, sh) = ((at + i) / 8, 7 - (at + i) % 8);
			rbsp[byte] = (rbsp[byte] & !(1 << sh)) | ((bit as u8) << sh);
		}
		Some(escape(&rbsp))
	}

	/// A picture about to be delivered (`nals` without start codes). Updates the parameter sets;
	/// with `gap` (the stream skipped access units since the last delivered one) returns the
	/// stand-ins for the picture order counts in between, oldest first, each as the Annex-B
	/// slices of one access unit.
	pub fn deliver(&mut self, nals: &[Vec<u8>], pts: u32, gap: bool) -> Vec<Vec<Vec<u8>>> {
		for n in nals.iter().filter(|n| n.len() > 2) {
			match nal_type(n) {
				33 => {
					if let Some((id, s)) = parse_sps(&unescape(n)) {
						self.sps.insert(id, s);
					}
				}
				34 => {
					if let Some((id, p)) = parse_pps(&unescape(n)) {
						self.pps.insert(id, p);
					}
				}
				_ => {}
			}
		}
		let vcl: Vec<&Vec<u8>> = nals.iter().filter(|n| n.len() > 2 && nal_type(n) <= 31).collect();
		let Some(first) = vcl.first() else { return Vec::new() };
		let irap = (16..=23).contains(&nal_type(first));
		let poc = if matches!(nal_type(first), 19 | 20) {
			Some(0)
		} else {
			self.poc_field(&unescape(first)).map(|(_, _, v)| v)
		};
		let mut out = Vec::new();
		if let (true, Some(poc), Some((slices, last_poc, _)), Some(width)) =
			(gap && !irap, poc, self.last.as_ref(), self.width())
		{
			let mask = (1u32 << width) - 1;
			let missing = poc.wrapping_sub(*last_poc).wrapping_sub(1) & mask;
			if (1..=MAX_STAND_INS).contains(&missing) && self.last_poc == Some(*last_poc) {
				for k in 1..=missing {
					let p = last_poc.wrapping_add(k) & mask;
					let au: Option<Vec<Vec<u8>>> = slices.iter().map(|s| self.with_poc(s, p)).collect();
					match au {
						Some(au) => out.push(au),
						None => {
							out.clear();
							break;
						}
					}
				}
			}
		}
		self.last_poc = poc;
		self.last = match (irap, poc) {
			(false, Some(p)) => Some((vcl.into_iter().cloned().collect(), p, pts)),
			_ => None,
		};
		out
	}

	/// POC LSB width of the (only, in practice) active SPS.
	fn width(&self) -> Option<u32> {
		let w = self.sps.values().next()?.log2_max_poc_lsb;
		self.sps.values().all(|s| s.log2_max_poc_lsb == w).then_some(w)
	}
}

/// Bitstream builders for the tests here and in `rtp`.
#[cfg(test)]
pub(crate) mod gen {
	use super::escape;

	pub(crate) struct W {
		v: Vec<u8>,
		n: usize,
	}
	impl W {
		pub(crate) fn new(hdr: [u8; 2]) -> Self {
			Self { v: hdr.to_vec(), n: 16 }
		}
		pub(crate) fn u(&mut self, bits: u32, val: u32) {
			for i in (0..bits).rev() {
				if self.n % 8 == 0 {
					self.v.push(0);
				}
				let last = self.v.len() - 1;
				self.v[last] |= (((val >> i) & 1) as u8) << (7 - self.n % 8);
				self.n += 1;
			}
		}
		pub(crate) fn ue(&mut self, val: u32) {
			let x = val + 1;
			let len = 32 - x.leading_zeros();
			self.u(len - 1, 0);
			self.u(len, x);
		}
		pub(crate) fn done(mut self) -> Vec<u8> {
			self.u(1, 1); // rbsp stop bit
			escape(&self.v)
		}
	}

	/// An SPS for 1280x720, 64x64 CTBs, `log2_max_poc_lsb` bits of POC LSB.
	pub(crate) fn sps(log2_max_poc_lsb: u32) -> Vec<u8> {
		let mut w = W::new([33 << 1, 1]);
		w.u(4, 0);
		w.u(3, 0);
		w.u(1, 1);
		w.u(32, 0x0160_0000);
		w.u(32, 0x9000_0000);
		w.u(32, 0x0000_005d); // rest of the general PTL + level 93
		w.ue(0); // sps id
		w.ue(1); // 4:2:0
		w.ue(1280);
		w.ue(720);
		w.u(1, 0);
		w.ue(0);
		w.ue(0);
		w.ue(log2_max_poc_lsb - 4);
		w.u(1, 1);
		w.ue(1);
		w.ue(0);
		w.ue(0);
		w.ue(0); // log2_min_cb 8
		w.ue(3); // CTB 64
		w.done()
	}

	pub(crate) fn pps(dependent: bool) -> Vec<u8> {
		let mut w = W::new([34 << 1, 1]);
		w.ue(0);
		w.ue(0);
		w.u(1, dependent as u32);
		w.u(1, 0);
		w.u(3, 0);
		w.done()
	}

	/// A TRAIL_R slice (`first` segment or at `addr`) with POC LSB `poc` and some payload.
	pub(crate) fn slice(first: bool, addr: u32, poc: u32, width: u32, payload: &[u8]) -> Vec<u8> {
		let mut w = W::new([1 << 1, 1]);
		w.u(1, first as u32);
		w.ue(0);
		if !first {
			w.u(1, 0); // dependent_slice_segment_flag
			w.u(8, addr); // sps(): 240 CTBs → an 8-bit slice_segment_address
		}
		w.ue(1); // P
		w.u(width, poc);
		for &b in payload {
			w.u(8, b as u32);
		}
		w.done()
	}

}

#[cfg(test)]
mod tests {
	use super::gen::*;
	use super::*;

	#[test]
	fn escape_round_trips() {
		let raw = [0u8, 0, 1, 0, 0, 0, 0, 0, 3, 7, 0, 0];
		assert_eq!(unescape(&escape(&raw)), raw);
		assert_eq!(escape(&[0, 0, 1]), vec![0, 0, 3, 1]);
	}

	#[test]
	fn parses_the_parameter_sets() {
		let (id, s) = parse_sps(&unescape(&sps(8))).unwrap();
		assert_eq!(id, 0);
		assert_eq!(s.log2_max_poc_lsb, 8);
		assert_eq!(s.pic_size_in_ctbs, 20 * 12);
		let (_, p) = parse_pps(&unescape(&pps(true))).unwrap();
		assert!(p.dependent_slices);
	}

	#[test]
	fn stand_ins_fill_the_skipped_picture_order_counts() {
		let mut c = Conceal::new();
		let ps = vec![sps(8), pps(false)];
		let s = |poc| slice(true, 0, poc, 8, &[0, 0, 0, 1, 0xAB, 0xCD]);
		let mut first = ps.clone();
		first.push(s(10));
		assert!(c.deliver(&first, 0, false).is_empty());
		assert!(c.deliver(&[s(11)], 1500, false).is_empty(), "no gap, no stand-in");
		// 12 and 13 were lost.
		let ins = c.deliver(&[s(14)], 6000, true);
		assert_eq!(ins.len(), 2);
		for (au, poc) in ins.iter().zip([12u32, 13]) {
			assert_eq!(au.len(), 1);
			assert_eq!(au[0], s(poc), "picture 11's slice with the missing POC, nothing else changed");
		}
	}

	#[test]
	fn the_poc_wraps() {
		let mut c = Conceal::new();
		let s = |poc| slice(true, 0, poc, 4, &[0x55]);
		assert!(c.deliver(&[sps(4), pps(false), s(14)], 0, false).is_empty());
		let ins = c.deliver(&[s(1)], 0, true);
		assert_eq!(ins.len(), 2, "15 and 0 are missing");
		assert_eq!(ins[1][0], s(0));
	}

	#[test]
	fn no_stand_in_without_a_gap_flag_or_parameter_sets_or_for_a_wide_jump() {
		let mut c = Conceal::new();
		let s = |poc| slice(true, 0, poc, 8, &[1]);
		assert!(c.deliver(&[s(1)], 0, false).is_empty());
		assert!(c.deliver(&[s(3)], 0, true).is_empty(), "no SPS/PPS seen");
		assert!(c.deliver(&[sps(8), pps(false), s(4)], 0, false).is_empty());
		assert!(c.deliver(&[s(6)], 0, false).is_empty(), "a POC step without a hole (not ours to fill)");
		assert!(c.deliver(&[s(100)], 0, true).is_empty(), "more than MAX_STAND_INS");
	}

	#[test]
	fn a_multi_slice_picture_gets_every_slice_patched() {
		let mut c = Conceal::new();
		// 1280x720 at 64x64 CTBs = 240 CTBs → 8-bit slice_segment_address.
		let pic = |poc| vec![slice(true, 0, poc, 8, &[9, 9]), slice(false, 120, poc, 8, &[7, 7])];
		let mut au = vec![sps(8), pps(true)];
		au.extend(pic(20));
		c.deliver(&au, 0, false);
		let ins = c.deliver(&pic(22), 0, true);
		assert_eq!(ins, vec![pic(21)]);
	}

	#[test]
	fn an_idr_cannot_stand_in_but_counts() {
		let mut c = Conceal::new();
		let mut idr = W::new([19 << 1, 1]);
		idr.u(1, 1);
		idr.u(1, 0);
		idr.ue(0);
		idr.ue(2);
		let idr = idr.done();
		assert!(c.deliver(&[sps(8), pps(false), idr], 0, false).is_empty());
		assert!(c.deliver(&[slice(true, 0, 2, 8, &[1])], 0, true).is_empty(), "nothing to copy after an IDR");
		let ins = c.deliver(&[slice(true, 0, 4, 8, &[1])], 0, true);
		assert_eq!(ins.len(), 1);
	}
}
