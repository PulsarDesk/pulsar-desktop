//! Loss recovery with long-term references (LTR) for encoders that cannot invalidate a
//! reference frame but can keep a few frames as long-term references and be told to predict
//! the next frame from one of them — the Media Foundation hardware encoders
//! (`CODECAPI_AVEncVideoLTRBufferControl` / `MarkLTRFrame` / `UseLTRFrame`; Qualcomm, Intel,
//! AMD MFTs expose them). Pure bookkeeping, no GPU: it decides which frame to mark into which
//! slot and, after a client-reported loss, which intact long-term reference the recovery frame
//! predicts from.
//!
//! The rules (same contract as NVENC's `policy::RefTracker`):
//! - every keyframe clears the slots and is marked into slot 0 (the most stable reference);
//! - after that a frame is marked every `every` frames, round-robin over the slots;
//! - a loss is healed from the NEWEST slot older than the oldest lost frame; slots at or after
//!   it are dropped (they were the lost frame or predicted from it);
//! - nothing intact → `Err`, NO keyframe here: the report arrives before the client's NACK
//!   repair, and the client asks for a keyframe itself when the repair fails. The unhealed
//!   loss then also spoils every later slot until a keyframe or a recovery from before it.

use std::collections::VecDeque;

/// What to tell the encoder for the frame about to be encoded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LtrOps {
	/// `MarkLTRFrame` = this slot index.
	pub(crate) mark: Option<u32>,
	/// `UseLTRFrame` with this slot bitmap: the frame is a recovery frame (tag it).
	pub(crate) use_bitmap: Option<u32>,
}

/// Frames remembered for mapping a lost RTP timestamp back to its frame number.
const RECENT: usize = 512;

pub(crate) struct LtrPlanner {
	/// Frame number held in each slot.
	slots: Vec<Option<u64>>,
	next_slot: usize,
	every: u64,
	last_mark: Option<u64>,
	/// Slot the next frame predicts from (a planned recovery).
	pending: Option<usize>,
	/// The oldest frame of a loss no recovery healed: slots from it on are spoiled.
	unhealed_from: Option<u64>,
	recent: VecDeque<(u32, u64)>,
}

impl LtrPlanner {
	pub(crate) fn new(slots: usize, every: u64) -> Self {
		LtrPlanner {
			slots: vec![None; slots.max(1)],
			next_slot: 0,
			every: every.max(1),
			last_mark: None,
			pending: None,
			unhealed_from: None,
			recent: VecDeque::with_capacity(RECENT),
		}
	}

	/// Frame `n` is about to be encoded (`key` = it will be a keyframe).
	pub(crate) fn before_encode(&mut self, n: u64, key: bool) -> LtrOps {
		if key {
			self.slots.iter_mut().for_each(|s| *s = None);
			self.pending = None;
			self.unhealed_from = None;
			self.slots[0] = Some(n);
			self.next_slot = 1 % self.slots.len();
			self.last_mark = Some(n);
			return LtrOps { mark: Some(0), use_bitmap: None };
		}
		if let Some(slot) = self.pending.take() {
			// The recovery frame. A mark waits for the next frame.
			return LtrOps { mark: None, use_bitmap: Some(1 << slot) };
		}
		let due = self.last_mark.is_none_or(|m| n.saturating_sub(m) >= self.every);
		if !due || self.unhealed_from.is_some() {
			return LtrOps::default();
		}
		let slot = self.next_slot;
		self.slots[slot] = Some(n);
		self.next_slot = (slot + 1) % self.slots.len();
		self.last_mark = Some(n);
		LtrOps { mark: Some(slot as u32), use_bitmap: None }
	}

	/// Frame `n` went out with RTP timestamp `ts`.
	pub(crate) fn sent(&mut self, n: u64, ts: u32) {
		if self.recent.len() == RECENT {
			self.recent.pop_front();
		}
		self.recent.push_back((ts, n));
	}

	/// Frame `n` produced no access unit (the encoder's rate control skipped it): it is no
	/// reference, so a slot meant to hold it holds nothing; a recovery frame that was skipped
	/// is planned again for the next frame (the slot it predicted from is still intact).
	pub(crate) fn skipped(&mut self, n: u64, was_recovery_from: Option<u32>) {
		for s in self.slots.iter_mut() {
			if *s == Some(n) {
				*s = None;
			}
		}
		if let Some(bitmap) = was_recovery_from {
			let slot = bitmap.trailing_zeros() as usize;
			if self.slots.get(slot).is_some_and(|s| s.is_some()) {
				self.pending = Some(slot);
			}
		}
	}

	/// The client lost the frames with these RTP timestamps: plan a recovery frame
	/// (`Ok(slot)`, applied by the next `before_encode`), or say why not.
	pub(crate) fn on_loss(&mut self, lost_ts: &[u32]) -> Result<usize, &'static str> {
		let oldest = lost_ts
			.iter()
			.filter_map(|ts| self.recent.iter().rev().find(|(t, _)| t == ts).map(|(_, n)| *n))
			.min()
			.ok_or("lost frame out of reach")?;
		// Spoiled: the lost frame itself and anything marked after it.
		for s in self.slots.iter_mut() {
			if s.is_some_and(|f| f >= oldest) {
				*s = None;
			}
		}
		let limit = self.unhealed_from.map_or(oldest, |u| u.min(oldest));
		let best = self
			.slots
			.iter()
			.enumerate()
			.filter_map(|(i, s)| s.filter(|f| *f < limit).map(|f| (i, f)))
			.max_by_key(|(_, f)| *f);
		match best {
			Some((i, _)) => {
				self.pending = Some(i);
				self.unhealed_from = None;
				Ok(i)
			}
			None => {
				self.pending = None;
				self.unhealed_from = Some(limit);
				Err("no intact long-term reference older than the loss")
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn run(p: &mut LtrPlanner, from: u64, to: u64) -> Vec<(u64, LtrOps)> {
		(from..to)
			.map(|n| {
				let ops = p.before_encode(n, n == 0);
				p.sent(n, n as u32 * 1000);
				(n, ops)
			})
			.collect()
	}

	#[test]
	fn keyframe_marks_slot_zero_then_round_robin() {
		let mut p = LtrPlanner::new(2, 10);
		let ops = run(&mut p, 0, 31);
		let marks: Vec<(u64, u32)> = ops.iter().filter_map(|(n, o)| o.mark.map(|m| (*n, m))).collect();
		assert_eq!(marks, vec![(0, 0), (10, 1), (20, 0), (30, 1)]);
	}

	#[test]
	fn loss_heals_from_newest_slot_before_it() {
		let mut p = LtrPlanner::new(2, 10);
		run(&mut p, 0, 25); // slots: 20 in 0, 10 in 1
		assert_eq!(p.on_loss(&[22_000, 23_000]), Ok(0));
		let ops = p.before_encode(25, false);
		assert_eq!(ops.use_bitmap, Some(1));
		assert_eq!(ops.mark, None);
	}

	#[test]
	fn loss_of_the_marked_frame_uses_the_older_slot() {
		let mut p = LtrPlanner::new(2, 10);
		run(&mut p, 0, 25);
		assert_eq!(p.on_loss(&[20_000]), Ok(1)); // frame 20 (slot 0) lost → frame 10 (slot 1)
		assert_eq!(p.before_encode(25, false).use_bitmap, Some(2));
	}

	#[test]
	fn nothing_intact_is_declined_and_spoils_later_marks() {
		let mut p = LtrPlanner::new(2, 10);
		run(&mut p, 0, 5);
		assert!(p.on_loss(&[0]).is_err()); // the keyframe itself
		// No marks while the loss is unhealed (they would be predicted from it).
		let ops = run(&mut p, 5, 40);
		assert!(ops.iter().all(|(_, o)| *o == LtrOps::default()));
		assert!(p.on_loss(&[35_000]).is_err());
		// A keyframe starts over.
		assert_eq!(p.before_encode(40, true).mark, Some(0));
	}

	#[test]
	fn a_skipped_frame_is_no_reference_and_a_skipped_recovery_is_retried() {
		let mut p = LtrPlanner::new(2, 10);
		run(&mut p, 0, 25); // slot 0 = 20, slot 1 = 10
		p.skipped(20, None);
		assert_eq!(p.on_loss(&[22_000]), Ok(1)); // frame 20 was never a reference → frame 10
		let ops = p.before_encode(25, false);
		assert_eq!(ops.use_bitmap, Some(2));
		p.skipped(25, ops.use_bitmap);
		assert_eq!(p.before_encode(26, false).use_bitmap, Some(2));
	}

	#[test]
	fn unknown_frames_are_out_of_reach() {
		let mut p = LtrPlanner::new(2, 10);
		run(&mut p, 0, 5);
		assert_eq!(p.on_loss(&[123_456_789]), Err("lost frame out of reach"));
	}
}
