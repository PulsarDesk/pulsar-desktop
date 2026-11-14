//! Loss-recovery presentation policy of the Linux renderer (`video.rs`), kept pure — time is
//! passed in — so it unit-tests on every host: the bounded "hold the last good frame" after an
//! unrepaired loss (stdin `hold <ms>`) and the bound on the access-unit backlog in front of the
//! decoder.

use super::AccessUnit;
use std::collections::VecDeque;

/// A `hold` request starts a new hold only after this long without one: until then it belongs
/// to the loss episode the current (or last) hold was armed for. The app sends a request on
/// every repair sweep (50 ms) that writes a packet off, so under sustained loss they come
/// several times a second — and the old rule (re-arm as soon as the previous deadline had
/// passed, even before a single frame was shown) chained them into one long freeze: field log
/// 2026-11-09 (Orange Pi, 9.6 Mbit/s policer + 1 % loss, intra-refresh stream), holds of up to
/// 1.7 s, 14-38 s of 80 s frozen, "renderer fps very low" 11-23 times. One hold per episode
/// bounds a freeze to the hold's own length however long the losses go on.
pub const HOLD_EPISODE_GAP_MS: u64 = 1_000;

/// How a hold ended — reported as `vidsink-hold <how> <frames> <ms>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HoldEnd {
	/// `keyframe` (a clean picture ended it early) or `deadline` (it ran its length).
	pub how: &'static str,
	/// Decoded frames it withheld.
	pub frames: u32,
	/// How long presentation was held: from arming to the keyframe / the deadline.
	pub ms: u64,
}

/// The renderer's loss hold: after an unrepaired loss the decoder is kept fed (its state stays
/// continuous) but its frames are not presented for a bounded moment, so the reference-less
/// smear isn't shown while the requested keyframe is on its way.
#[derive(Debug, Default)]
pub struct LossHold {
	/// Deadline of the current hold (renderer ms clock); `None` = presenting.
	until: Option<u64>,
	/// When the current hold was armed.
	since: u64,
	/// Decoded frames the current hold withheld so far.
	frames: u32,
	/// The latest request, armed or not: the loss episode lasts while they keep coming.
	last_request: Option<u64>,
}

impl LossHold {
	pub const fn new() -> Self {
		Self { until: None, since: 0, frames: 0, last_request: None }
	}

	/// The app wrote a NACK repair off (`hold <ms>`). Arms a hold of `ms` at the start of a loss
	/// episode only (see `HOLD_EPISODE_GAP_MS`); an active hold is never extended. Never on an
	/// intra-refreshed stream: nothing would end it early (no keyframe comes), and the decoder
	/// conceals the loss while the rolling intra wave heals the picture within its period —
	/// showing that beats freezing a third of it. Returns a hold that had run out unobserved (no
	/// frame decoded since its deadline), for the caller to report.
	pub fn request(&mut self, now: u64, ms: u64, intra_refresh: bool) -> Option<HoldEnd> {
		let new_episode = self.last_request.map_or(true, |t| now.saturating_sub(t) >= HOLD_EPISODE_GAP_MS);
		self.last_request = Some(now);
		let ended = self.expire(now);
		if !intra_refresh && new_episode && self.until.is_none() {
			self.until = Some(now + ms.max(1));
			self.since = now;
			self.frames = 0;
		}
		ended
	}

	/// A keyframe reached the decoder: everything decoded from here is clean, so a hold ends.
	pub fn on_keyframe(&mut self, now: u64) -> Option<HoldEnd> {
		if let Some(e) = self.expire(now) {
			return Some(e);
		}
		self.until.take()?;
		Some(HoldEnd { how: "keyframe", frames: self.frames, ms: now.saturating_sub(self.since) })
	}

	/// A decoded frame: `(present it, a hold that just ran out)`.
	pub fn on_frame(&mut self, now: u64) -> (bool, Option<HoldEnd>) {
		let ended = self.expire(now);
		if self.until.is_some() {
			self.frames += 1;
			return (false, None);
		}
		(true, ended)
	}

	/// Holding at `now`?
	pub fn holding(&self, now: u64) -> bool {
		self.until.is_some_and(|u| now < u)
	}

	/// End a hold whose deadline has passed.
	fn expire(&mut self, now: u64) -> Option<HoldEnd> {
		let u = self.until.filter(|&u| now >= u)?;
		self.until = None;
		Some(HoldEnd { how: "deadline", frames: self.frames, ms: u.saturating_sub(self.since) })
	}
}

/// What [`bound_backlog`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backlog {
	/// Nothing dropped.
	Kept,
	/// The stale lead-in up to the newest queued keyframe was dropped (that many AUs).
	DroppedLeadIn(usize),
	/// No keyframe to skip to and the hard bound reached: the oldest AU was shed — the
	/// reference chain is broken, the caller asks for a keyframe.
	ShedOldest,
}

/// Bound the AU backlog `q` before one more is queued — the Windows renderer's rule
/// (win/mod.rs). Never cut a delta AU out of the middle of the P-chain: past `soft` drop the
/// stale lead-in up to (and keeping) the newest queued keyframe. Without one an intact chain may
/// grow to `hard`: the reorder buffer releases everything that waited behind a hole at once
/// (~30 AUs per 250 ms at 120 fps), and the decoder catches up on such a burst — shedding from
/// it broke the chain into a keyframe request, which an intra-refreshed stream (no keyframes in
/// steady state) only gets by asking. Only a decoder that cannot keep up at all reaches `hard`.
pub fn bound_backlog(q: &mut VecDeque<AccessUnit>, soft: usize, hard: usize) -> Backlog {
	if q.len() < soft {
		return Backlog::Kept;
	}
	match q.iter().rposition(|a| a.key) {
		Some(k) if k > 0 => {
			q.drain(..k);
			Backlog::DroppedLeadIn(k)
		}
		_ if q.len() < hard => Backlog::Kept,
		_ => {
			q.pop_front();
			Backlog::ShedOldest
		}
	}
}

/// AU-backlog overflow reports, at most one line per [`OVERFLOW_LOG_EVERY`] with what happened
/// since the last one (Windows and Linux renderers). Every overflow used to print its own line:
/// a Windows renderer that decoded nothing (its D3D device gone while the display was off)
/// wrote one per arriving frame — 2.6 million lines, 415 MB of app log, in one night
/// (2026-11-14). The Linux renderer printed the same line per overflow.
pub struct OverflowLog {
	/// Line prefix (`pulsar-render(win)`, `pulsar-render`).
	tag: &'static str,
	/// Stale AUs dropped ahead of a queued keyframe since the last line.
	skipped: u32,
	/// Oldest AUs shed with no keyframe to skip to (each asks for an IDR) since the last line.
	shed: u32,
	last: Option<std::time::Instant>,
}

pub const OVERFLOW_LOG_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

impl OverflowLog {
	pub fn new(tag: &'static str) -> OverflowLog {
		OverflowLog { tag, skipped: 0, shed: 0, last: None }
	}

	/// Count one overflow — `dropped` AUs, `shed` = the oldest one went with no keyframe queued
	/// — and return the line to print when one is due (the first at once).
	pub fn note(&mut self, dropped: u32, shed: bool, now: std::time::Instant) -> Option<String> {
		if shed {
			self.shed += dropped;
		} else {
			self.skipped += dropped;
		}
		if self.last.is_some_and(|t| now.duration_since(t) < OVERFLOW_LOG_EVERY) {
			return None;
		}
		self.last = Some(now);
		let (skipped, shed) = (std::mem::take(&mut self.skipped), std::mem::take(&mut self.shed));
		let mut parts = Vec::new();
		if skipped > 0 {
			parts.push(format!("dropped {skipped} stale AU(s) ahead of newest keyframe"));
		}
		if shed > 0 {
			parts.push(format!("no keyframe queued; dropped the oldest AU {shed} time(s), keyframe requested"));
		}
		Some(format!("{}: AU backlog overflow — {}", self.tag, parts.join("; ")))
	}
}

/// A log line that can repeat every frame (a present that stalls, a failing GPU call): the
/// first one is printed, then at most one per `every`, carrying how many were held back.
pub struct RateLog {
	every: std::time::Duration,
	last: Option<std::time::Instant>,
	held: u32,
}

impl RateLog {
	pub const fn new(every: std::time::Duration) -> RateLog {
		RateLog { every, last: None, held: 0 }
	}

	/// One occurrence at `now`. `Some(n)` = print it now, `n` = occurrences held back since
	/// the last printed one; `None` = stay quiet (counted).
	pub fn hit(&mut self, now: std::time::Instant) -> Option<u32> {
		if self.last.is_some_and(|t| now.duration_since(t) < self.every) {
			self.held += 1;
			return None;
		}
		self.last = Some(now);
		Some(std::mem::take(&mut self.held))
	}
}

/// ` (+N more since the last line)` for a [`RateLog`] count, empty for 0.
pub fn held_suffix(n: u32) -> String {
	if n == 0 {
		String::new()
	} else {
		format!(" (+{n} more since the last line)")
	}
}

/// Test hook `PULSAR_TEST_DEVICE_LOSS=<after_s>,<failed_attempts>` (both renderers): once,
/// `<after_s>` seconds in, the renderer acts as if its GPU device / GL context was lost, and
/// the first `<failed_attempts>` rebuilds fail like they do while the display is off.
pub fn parse_device_loss_test(v: &str) -> Option<(std::time::Duration, u32)> {
	let (after, fails) = v.split_once(',')?;
	let after: f32 = after.trim().parse().ok()?;
	if !after.is_finite() || after < 0.0 {
		return None;
	}
	Some((std::time::Duration::from_secs_f32(after), fails.trim().parse().ok()?))
}

/// Pause between two attempts to rebuild a lost GPU device / GL context.
pub const REBUILD_EVERY: std::time::Duration = std::time::Duration::from_millis(500);

/// A failing rebuild is logged at the first attempt, then every 120th (= once a minute at
/// [`REBUILD_EVERY`]): a display that stays off can refuse it for hours.
pub fn rebuild_fail_logged(attempt: u32) -> bool {
	attempt == 1 || attempt % 120 == 0
}

#[cfg(test)]
mod tests {
	use super::*;

	const HOLD: u64 = 300;
	/// One decoded frame every ~11 ms (90 fps).
	const FRAME: u64 = 11;

	/// Drive `h` from `from` to `to` (ms) with a decoded frame every `FRAME` ms and a request at
	/// each of `requests`; returns (frames presented, frames withheld, the ends reported).
	fn run(h: &mut LossHold, from: u64, to: u64, requests: &[u64], ir: bool) -> (u32, u32, Vec<HoldEnd>) {
		let (mut shown, mut held, mut ends) = (0, 0, Vec::new());
		for t in from..to {
			if requests.contains(&t) {
				ends.extend(h.request(t, HOLD, ir));
			}
			if (t - from) % FRAME == 0 {
				let (show, end) = h.on_frame(t);
				ends.extend(end);
				if show {
					shown += 1;
				} else {
					held += 1;
				}
			}
		}
		(shown, held, ends)
	}

	#[test]
	fn a_hold_withholds_frames_for_its_length_then_reports_the_deadline() {
		let mut h = LossHold::new();
		assert_eq!(h.request(1_000, HOLD, false), None);
		assert!(h.holding(1_000) && h.holding(1_299) && !h.holding(1_300));
		assert_eq!(h.on_frame(1_010), (false, None));
		assert_eq!(h.on_frame(1_200), (false, None));
		let end = HoldEnd { how: "deadline", frames: 2, ms: HOLD };
		assert_eq!(h.on_frame(1_310), (true, Some(end)));
		assert_eq!(h.on_frame(1_320), (true, None));
	}

	/// The field-log pattern: a request on every 50 ms sweep for 3 s. One hold at the start of
	/// the episode, then presentation goes on — it used to re-arm every time a deadline passed.
	#[test]
	fn sustained_loss_freezes_once_not_for_the_whole_episode() {
		let mut h = LossHold::new();
		let requests: Vec<u64> = (0..60).map(|i| 10_000 + i * 50).collect();
		let (shown, held, ends) = run(&mut h, 10_000, 13_500, &requests, false);
		assert_eq!(ends, vec![HoldEnd { how: "deadline", frames: held, ms: HOLD }]);
		assert!(u64::from(held) <= HOLD / FRAME + 1, "held {held} frames");
		assert!(shown > 250, "presentation resumed after one hold (shown {shown})");
	}

	/// Frames that only come in bursts (the reorder buffer releasing what waited behind a hole):
	/// an expired hold no frame saw end must not merge with the next request into a chain.
	#[test]
	fn a_hold_that_ran_out_unobserved_does_not_chain() {
		let mut h = LossHold::new();
		let mut ends: Vec<HoldEnd> = h.request(0, HOLD, false).into_iter().collect();
		for t in (50..=600).step_by(50) {
			ends.extend(h.request(t, HOLD, false));
		}
		assert!(!h.holding(600), "not re-armed once its deadline passed");
		assert_eq!(ends, vec![HoldEnd { how: "deadline", frames: 0, ms: HOLD }]);
		assert_eq!(h.on_frame(610), (true, None));
	}

	#[test]
	fn an_active_hold_is_never_extended() {
		let mut h = LossHold::new();
		h.request(0, HOLD, false);
		h.request(250, HOLD, false);
		assert!(h.holding(299) && !h.holding(300));
	}

	#[test]
	fn a_new_loss_episode_holds_again() {
		let mut h = LossHold::new();
		h.request(0, HOLD, false);
		assert!(h.on_frame(400).0);
		// Still the same episode (a request within the gap of the previous one)…
		h.request(900, HOLD, false);
		assert!(!h.holding(900));
		// …a quiet second later a new one.
		h.request(900 + HOLD_EPISODE_GAP_MS, HOLD, false);
		assert!(h.holding(900 + HOLD_EPISODE_GAP_MS));
	}

	/// A new episode while an earlier hold ran out unobserved (no frame since): the old one is
	/// reported with its own length and a fresh hold starts.
	#[test]
	fn a_new_episode_reports_the_unobserved_hold_and_arms() {
		let mut h = LossHold::new();
		h.request(0, HOLD, false);
		let end = h.request(5_000, HOLD, false);
		assert_eq!(end, Some(HoldEnd { how: "deadline", frames: 0, ms: HOLD }));
		assert!(h.holding(5_000));
	}

	#[test]
	fn a_keyframe_ends_the_hold_early() {
		let mut h = LossHold::new();
		h.request(100, HOLD, false);
		assert_eq!(h.on_frame(150), (false, None));
		assert_eq!(h.on_keyframe(220), Some(HoldEnd { how: "keyframe", frames: 1, ms: 120 }));
		assert_eq!(h.on_frame(225), (true, None));
		// No hold, nothing to report.
		assert_eq!(h.on_keyframe(230), None);
		// A keyframe after the deadline reports the deadline it ran into.
		h.request(5_000, HOLD, false);
		assert_eq!(h.on_keyframe(5_400), Some(HoldEnd { how: "deadline", frames: 0, ms: HOLD }));
	}

	/// Intra refresh: presentation never stops for a loss — the decoder conceals it and the wave
	/// heals the picture.
	#[test]
	fn an_intra_refreshed_stream_is_never_held() {
		let mut h = LossHold::new();
		let requests: Vec<u64> = (0..20).map(|i| 1_000 + i * 50).collect();
		let (shown, held, ends) = run(&mut h, 1_000, 3_000, &requests, true);
		assert_eq!((held, ends), (0, vec![]));
		assert!(shown > 150, "shown {shown}");
	}

	fn au(key: bool) -> AccessUnit {
		AccessUnit { data: vec![0, 0, 0, 1, 0x41], pts_90k: 0, key }
	}

	fn queue(keys: &[bool]) -> VecDeque<AccessUnit> {
		keys.iter().map(|&k| au(k)).collect()
	}

	#[test]
	fn a_short_backlog_is_kept() {
		let mut q = queue(&[true, false, false]);
		assert_eq!(bound_backlog(&mut q, 8, 64), Backlog::Kept);
		assert_eq!(q.len(), 3);
	}

	#[test]
	fn a_full_backlog_skips_to_its_newest_keyframe() {
		let mut keys = vec![false; 32];
		keys[5] = true;
		keys[20] = true;
		let mut q = queue(&keys);
		assert_eq!(bound_backlog(&mut q, 32, 64), Backlog::DroppedLeadIn(20));
		assert_eq!(q.len(), 12);
		assert!(q.front().unwrap().key);
	}

	/// A reorder-buffer burst on a keyframe-less (intra-refreshed) stream: kept whole, no
	/// keyframe request — Linux used to shed + ask for an IDR at 32.
	#[test]
	fn a_keyframe_less_burst_is_decoded_whole_up_to_the_hard_bound() {
		let mut q = queue(&[false; 40]);
		assert_eq!(bound_backlog(&mut q, 32, 64), Backlog::Kept);
		assert_eq!(q.len(), 40);
		let mut q = queue(&[false; 64]);
		assert_eq!(bound_backlog(&mut q, 32, 64), Backlog::ShedOldest);
		assert_eq!(q.len(), 63);
	}

	/// A keyframe at the front is no drop point; at the hard bound even it goes (the decoder is
	/// stuck — a fresh keyframe is the way out), as on Windows.
	#[test]
	fn a_keyframe_at_the_front_is_shed_only_at_the_hard_bound() {
		let mut keys = vec![false; 40];
		keys[0] = true;
		let mut q = queue(&keys);
		assert_eq!(bound_backlog(&mut q, 32, 64), Backlog::Kept);
		let mut keys = vec![false; 64];
		keys[0] = true;
		let mut q = queue(&keys);
		assert_eq!(bound_backlog(&mut q, 32, 64), Backlog::ShedOldest);
	}

	#[test]
	fn overflow_log_one_line_per_two_seconds_with_the_counts_since_the_last() {
		let t0 = std::time::Instant::now();
		let at = |ms: u64| t0 + std::time::Duration::from_millis(ms);
		let mut log = OverflowLog::new("pulsar-render(win)");
		// The first overflow is reported at once.
		assert_eq!(
			log.note(40, false, at(0)).as_deref(),
			Some("pulsar-render(win): AU backlog overflow — dropped 40 stale AU(s) ahead of newest keyframe")
		);
		// A frame every 8 ms shed for two seconds: silent, then one line with the count.
		let mut lines = Vec::new();
		for i in 1..=250u64 {
			lines.extend(log.note(1, true, at(i * 8)));
		}
		assert_eq!(lines.len(), 1, "{lines:?}");
		assert!(lines[0].contains("dropped the oldest AU 250 time(s), keyframe requested"), "{lines:?}");
		// Both kinds within one window share a line.
		assert_eq!(log.note(63, false, at(2100)), None);
		assert_eq!(log.note(1, true, at(2200)), None);
		let line = log.note(1, true, at(4100)).unwrap();
		assert!(line.contains("dropped 63 stale AU(s)") && line.contains("2 time(s)"), "{line}");
	}

	#[test]
	fn overflow_log_uses_its_tag() {
		let mut log = OverflowLog::new("pulsar-render");
		let line = log.note(1, true, std::time::Instant::now()).unwrap();
		assert!(line.starts_with("pulsar-render: AU backlog overflow — no keyframe"), "{line}");
	}

	#[test]
	fn rate_log_prints_first_then_once_per_window_with_the_held_count() {
		let t0 = std::time::Instant::now();
		let at = |ms: u64| t0 + std::time::Duration::from_millis(ms);
		let mut log = RateLog::new(std::time::Duration::from_secs(5));
		assert_eq!(log.hit(at(0)), Some(0));
		// A stall every frame for 5 s: quiet, counted.
		for i in 1..300u64 {
			assert_eq!(log.hit(at(i * 16)), None);
		}
		assert_eq!(log.hit(at(5000)), Some(299));
		assert_eq!(log.hit(at(5001)), None);
		assert_eq!(held_suffix(0), "");
		assert_eq!(held_suffix(3), " (+3 more since the last line)");
	}

	#[test]
	fn device_loss_test_hook_parses() {
		use std::time::Duration;
		assert_eq!(parse_device_loss_test("6,8"), Some((Duration::from_secs(6), 8)));
		assert_eq!(parse_device_loss_test(" 1.5 , 0 "), Some((Duration::from_millis(1500), 0)));
		assert_eq!(parse_device_loss_test("6"), None);
		assert_eq!(parse_device_loss_test("x,1"), None);
		assert_eq!(parse_device_loss_test("-1,1"), None);
		assert_eq!(parse_device_loss_test("2,-1"), None);
	}

	#[test]
	fn failing_rebuilds_log_once_a_minute() {
		let logged: Vec<u32> = (1..=400).filter(|&n| rebuild_fail_logged(n)).collect();
		assert_eq!(logged, vec![1, 120, 240, 360]);
	}
}
