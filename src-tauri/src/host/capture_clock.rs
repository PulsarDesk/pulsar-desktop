//! Capture-time RTP timestamps for an encoder that counts frames.
//!
//! The client's delay-gradient estimator compares the spacing of the RTP timestamps with the
//! spacing of the arrivals. An encoder clocked by a frame counter (`pts += 1` at `1/fps`) runs
//! slow whenever the host delivers fewer frames than nominal — a slow software encode, a
//! capture that falls behind — and the client reads the growing difference as a queue on the
//! path (Windows, 2026-11-08: 4-6 % short at 120 fps cut an idle 40 Mbit link down to 1080p30).
//! The Windows and macOS native paths stamp the capture instant; this does the same for the
//! Linux in-process libav host (`libav.rs`): it remembers each frame's capture time under the
//! encoder pts it was given and hands it back for the packet that carries that pts.
//!
//! Pure bookkeeping, built and tested on every OS (only `libav.rs` uses it, Linux).
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::collections::VecDeque;

/// Frames remembered between `capture` and `packet` (an encoder without B-frames emits each
/// frame's packet before it takes many more).
const DEPTH: usize = 64;

#[derive(Default)]
pub(crate) struct CaptureClock {
	pending: VecDeque<(i64, i64)>,
	last_us: Option<i64>,
}

impl CaptureClock {
	/// Frame with encoder pts `pts` was captured `at_us` after the stream start.
	pub(crate) fn capture(&mut self, pts: i64, at_us: i64) {
		if self.pending.len() == DEPTH {
			self.pending.pop_front();
		}
		self.pending.push_back((pts, at_us));
	}

	/// The capture time (µs since the start) for the packet of encoder pts `pts`: strictly
	/// increasing (a muxer refuses anything else); a pts it never saw continues one frame
	/// interval (`frame_us`) after the last.
	pub(crate) fn packet(&mut self, pts: i64, frame_us: i64) -> i64 {
		while self.pending.front().is_some_and(|&(p, _)| p < pts) {
			self.pending.pop_front();
		}
		let at = match self.pending.front() {
			Some(&(p, at)) if p == pts => {
				self.pending.pop_front();
				at
			}
			_ => self.last_us.map_or(0, |l| l + frame_us.max(1)),
		};
		let at = match self.last_us {
			Some(l) if at <= l => l + 1,
			_ => at,
		};
		self.last_us = Some(at);
		at
	}
}

#[cfg(test)]
mod tests {
	use super::CaptureClock;

	#[test]
	fn packets_carry_their_frames_capture_time() {
		let mut c = CaptureClock::default();
		// 60 fps nominal, but the frames come 20 ms apart (the host is behind).
		for n in 0..5 {
			c.capture(n, n * 20_000);
		}
		let got: Vec<i64> = (0..5).map(|n| c.packet(n, 16_667)).collect();
		assert_eq!(got, vec![0, 20_000, 40_000, 60_000, 80_000]);
	}

	#[test]
	fn unknown_or_dropped_frames_stay_monotonic() {
		let mut c = CaptureClock::default();
		c.capture(0, 1_000);
		c.capture(1, 2_000);
		c.capture(2, 2_000); // same instant twice
		assert_eq!(c.packet(0, 16_667), 1_000);
		// pts 1 dropped inside the encoder: its entry is skipped.
		assert_eq!(c.packet(2, 16_667), 2_000);
		// Never seen: one interval on.
		assert_eq!(c.packet(9, 16_667), 18_667);
		c.capture(10, 18_000); // earlier than the last packet
		assert_eq!(c.packet(10, 16_667), 18_668);
	}
}
