//! Wire pacer for the media-over-session video forwarder.
//!
//! WHY: the forwarder used to put every datagram on the wire the instant it arrived —
//! a whole keyframe, its FEC parity and any NACK retransmits left back-to-back at NIC line
//! rate. On a thin mobile uplink (~10 Mbit/s, carrier policer with a shallow queue) that
//! burst is what gets dropped: field logs showed 34-41 % loss from the first windows while
//! the AVERAGE rate was far below the link. Pacing spreads the same bytes so the bottleneck
//! queue never sees more than a short burst.
//!
//! MODEL: one token bucket shared by EVERY video-path datagram (video, parity, retransmit),
//! so parity and retransmits can no longer slip out unpaced on top of the video budget.
//! - Rate = `PACE_FACTOR` x the expected wire rate (target bitrate x (1 + FEC ratio)), never
//!   below `PACE_FLOOR_KBPS` and never below `INPUT_HEADROOM` x the measured intake rate
//!   (an encoder overshooting its target must be smoothed, not policed — dropping here would
//!   only turn into loss and keyframe requests).
//! - Bucket depth = `BURST_MS` of the rate (at least a few packets): a frame arriving after an
//!   idle gap leaves at once up to that budget, the rest is dripped at the rate. On a LAN at
//!   tens of Mbit/s a whole P-frame fits the budget, so pacing adds no delay there.
//! - Queue-delay bound: every datagram must leave within `SOFT_DELAY_MS` of being queued. When
//!   the base rate can't meet that for some queued datagram, the drain rate rises just enough
//!   to meet it — a big keyframe can delay the frames behind it by at most that much. Anything
//!   older than `HARD_DELAY` (only reachable when the socket send itself stalls) is dropped, so
//!   the queue holds the latest.
//! - Priority: retransmits go ahead of queued video (the client is already waiting on them);
//!   parity sits in the video FIFO right after the packet that completed its group/frame.
//! - Retransmit budget: retransmits may take at most `RTX_SHARE` of the expected wire rate (a
//!   token bucket `RTX_BURST_MS` deep); a NACKed packet beyond it is not re-sent (the client
//!   NACKs it again after a round-trip, or asks for a keyframe). WHY: in the 2026-11-09 policer
//!   test a loss burst made the client NACK ~1500 packets in one 2 s window — 6 Mbit/s of
//!   retransmits on a ~9 Mbit/s target, on top of the video, into the very policer dropping
//!   them, so the burst fed itself. The host's FEC policy already yields parity to the
//!   retransmits (`fec_policy::budget_cap`); this caps what is left over, so video + parity +
//!   retransmits stay near the client's target even then.
//! - Granularity: a packet may leave while the bucket is short by less than `BORROW_MS` of
//!   rate (the debt is repaid by the refill). Timer wakes are whole milliseconds (and can be
//!   coarser on Windows), so this batches the few packets that fall due within one wake
//!   instead of sleeping per packet.
//!
//! The pacer is pure — every call takes `now` — so the tests drive it with a fake clock.

use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

/// Pace = this x the expected wire rate. 2x keeps a frame's packets spread over about half a
/// frame interval at steady state while leaving room for parity/retransmit bursts.
pub(super) const PACE_FACTOR: f64 = 2.0;
/// Never pace slower than this: at very low targets the per-packet overhead and encoder
/// overshoot dominate, and smoothing to 2 Mbit/s is still far gentler than a line-rate burst.
pub(super) const PACE_FLOOR_KBPS: f64 = 2000.0;
/// Burst budget (bucket depth) in ms of the pace rate.
const BURST_MS: f64 = 16.0;
/// ...but at least this many bytes (~4 full packets), so a tiny rate still lets a small frame
/// out in one go.
const BURST_MIN_BYTES: f64 = 4.0 * 1300.0;
/// A packet may leave while the bucket is short by up to this many ms of rate.
const BORROW_MS: f64 = 1.0;
/// Queue-delay bound: the drain rate rises so every datagram leaves within this of queueing.
/// 20 ms, not 50: whatever the pacer holds back is one-way delay the client's delay-gradient
/// estimator sees, and a bursty encoder (MediaFoundation via ffmpeg, frames arriving in
/// clumps) at a low rate kept 25-51 ms in this queue — read as a growing network queue, it
/// cut a clean policed link from 4 to ~1 Mbit (2026-11-09, Snapdragon host → Orange Pi).
/// 20 ms still spreads a P-frame burst over more than a frame interval at 60 fps.
pub(super) const SOFT_DELAY_MS: f64 = 20.0;
/// Datagrams queued longer than this are dropped (stalled send; keep the latest).
pub(super) const HARD_DELAY: Duration = Duration::from_millis(250);
/// Approximate per-datagram cost on the wire beyond the payload handed to the session
/// (IP/UDP headers + the session seal), so many small packets are not under-counted.
pub(super) const WIRE_OVERHEAD: usize = 48;
/// Pace at least this x the measured intake rate.
const INPUT_HEADROOM: f64 = 1.25;
/// Retransmits may use at most this share of the expected wire rate (sustained). Random loss
/// repaired by NACK costs about its loss rate (a few % to ~10 % in the field logs), so only a
/// loss storm reaches it.
pub(super) const RTX_SHARE: f64 = 0.25;
/// Depth of the retransmit bucket in ms of its rate: a frame's worth of NACKs answered at once.
const RTX_BURST_MS: f64 = 200.0;
/// ...but at least this many bytes (~8 full packets), so a low rate still repairs a lost frame.
const RTX_BURST_MIN_BYTES: f64 = 8.0 * 1300.0;
/// Intake-rate measurement window.
const INPUT_WINDOW: Duration = Duration::from_millis(200);

/// What a queued datagram carries (priority + accounting).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Class {
	Video,
	Parity,
	Retransmit,
}

struct Item {
	data: Vec<u8>,
	class: Class,
	/// RTP sequence number of a video/retransmit datagram (dedups NACKs against the queue).
	seq: Option<u16>,
	at: Instant,
}

/// Per-window counters for the forwarder's measurement log (sent = left the pacer).
#[derive(Clone, Debug, Default)]
pub(super) struct Stats {
	pub video_pkts: u64,
	pub video_bytes: u64,
	pub parity_pkts: u64,
	pub parity_bytes: u64,
	pub rtx_pkts: u64,
	pub rtx_bytes: u64,
	/// Datagrams dropped as stale (older than `HARD_DELAY`).
	pub drops: u64,
	/// NACKed packets not re-sent: over the retransmit budget (`RTX_SHARE`).
	pub rtx_drops: u64,
	/// Longest time a datagram waited in the pacer this window.
	pub max_qdelay_ms: f64,
}

pub(super) struct Pacer {
	/// false = pass-through (`PULSAR_MOS_PACE=0` A/B knob): every datagram leaves at once.
	enabled: bool,
	target_kbps: u32,
	fec_ratio: f64,
	/// Base pace rate, bytes per ms.
	base: f64,
	tokens: f64,
	last: Instant,
	rtx: VecDeque<Item>,
	media: VecDeque<Item>,
	in_bytes: f64,
	in_at: Instant,
	/// Measured intake rate (video only), bytes per ms (EWMA).
	in_rate: f64,
	/// Retransmit budget bucket (bytes) and when it was last refilled.
	rtx_tokens: f64,
	rtx_at: Instant,
	stats: Stats,
}

fn cost(len: usize) -> f64 {
	(len + WIRE_OVERHEAD) as f64
}

impl Pacer {
	pub(super) fn new(enabled: bool, now: Instant) -> Self {
		let mut p = Self {
			enabled,
			target_kbps: 0,
			fec_ratio: 0.0,
			base: 0.0,
			tokens: 0.0,
			last: now,
			rtx: VecDeque::new(),
			media: VecDeque::new(),
			in_bytes: 0.0,
			in_at: now,
			in_rate: 0.0,
			// Starts full (clamped to the bucket depth on first use).
			rtx_tokens: f64::MAX,
			rtx_at: now,
			stats: Stats::default(),
		};
		p.recompute();
		p.tokens = p.depth();
		p
	}

	/// Live target bitrate (kbps, 0 = unknown) and FEC parity ratio (0..1).
	pub(super) fn set_target(&mut self, target_kbps: u32, fec_ratio: f32, now: Instant) {
		let fec = (fec_ratio as f64).clamp(0.0, 1.0);
		if target_kbps != self.target_kbps || (fec - self.fec_ratio).abs() > 1e-4 {
			// Accrue at the old rate up to now before the rate changes.
			self.refill(now);
			self.target_kbps = target_kbps;
			self.fec_ratio = fec;
			self.recompute();
		}
	}

	fn recompute(&mut self) {
		let floor = PACE_FLOOR_KBPS / 8.0;
		let from_target = if self.target_kbps > 0 {
			// kbps / 8 = bytes per ms.
			PACE_FACTOR * self.target_kbps as f64 / 8.0 * (1.0 + self.fec_ratio)
		} else {
			// Unknown target: pace off what actually arrives.
			PACE_FACTOR * self.in_rate
		};
		self.base = from_target.max(floor).max(INPUT_HEADROOM * self.in_rate);
		let d = self.depth();
		if self.tokens > d {
			self.tokens = d;
		}
	}

	fn depth(&self) -> f64 {
		(BURST_MS * self.base).max(BURST_MIN_BYTES)
	}

	/// Drain rate at `now`: the base rate, raised to the least rate at which every queued
	/// datagram (in service order) still leaves by its deadline (`queued + SOFT_DELAY_MS`).
	/// An overdue one counts as due within `BORROW_MS`, i.e. it leaves at the next wake.
	/// O(queue) — the queue is a few hundred datagrams at most.
	fn eff_rate(&self, now: Instant) -> f64 {
		let mut need = self.base;
		let mut cum = -self.tokens.max(0.0);
		for it in self.rtx.iter().chain(self.media.iter()) {
			cum += cost(it.data.len());
			if cum <= 0.0 {
				continue;
			}
			let age = now.saturating_duration_since(it.at).as_secs_f64() * 1000.0;
			let left = (SOFT_DELAY_MS - age).max(BORROW_MS);
			need = need.max(cum / left);
		}
		need
	}

	/// Current drain rate in kbps (for the measurement log).
	pub(super) fn pace_kbps(&self, now: Instant) -> f64 {
		self.eff_rate(now) * 8.0
	}

	/// Tokens after accruing `dt` ms at `rate`. Capped at the burst budget — except while the
	/// deadline bound is boosting the rate, when the whole interval's accrual is kept (a coarse
	/// timer wake must not throttle a drain that has to finish by a deadline).
	fn accrued(&self, dt: f64, rate: f64) -> f64 {
		let depth = self.depth();
		let cap = if rate > self.base { depth.max(rate * dt) } else { depth };
		(self.tokens + rate * dt).min(cap)
	}

	fn refill(&mut self, now: Instant) {
		let dt = now.saturating_duration_since(self.last).as_secs_f64() * 1000.0;
		if dt > 0.0 {
			// The rate that applied over the interval: the queue as it stood at `last`.
			let rate = self.eff_rate(self.last);
			self.tokens = self.accrued(dt, rate);
			self.last = now;
		}
	}

	/// Boost tokens are only for draining a backlog: once the queue is empty (or was emptied by
	/// a stale drop) the budget is back to the normal burst depth.
	fn settle_tokens(&mut self) {
		if self.rtx.is_empty() && self.media.is_empty() {
			self.tokens = self.tokens.min(self.depth());
		}
	}

	/// Whether a retransmit of `len` bytes fits the retransmit budget now (and takes it). No
	/// budget while the target is unknown or the pacer passes through (the A/B knob keeps the
	/// old send-everything behaviour).
	fn rtx_admit(&mut self, len: usize, now: Instant) -> bool {
		if !self.enabled || self.target_kbps == 0 {
			return true;
		}
		// kbps / 8 = bytes per ms, of the wire the client expects (video + parity).
		let rate = RTX_SHARE * self.target_kbps as f64 / 8.0 * (1.0 + self.fec_ratio);
		let depth = (RTX_BURST_MS * rate).max(RTX_BURST_MIN_BYTES);
		let dt = now.saturating_duration_since(self.rtx_at).as_secs_f64() * 1000.0;
		self.rtx_at = now;
		self.rtx_tokens = (self.rtx_tokens.min(depth) + rate * dt).min(depth);
		let c = cost(len);
		if self.rtx_tokens < c {
			self.stats.rtx_drops += 1;
			return false;
		}
		self.rtx_tokens -= c;
		true
	}

	/// Queue one datagram. `seq` is the RTP sequence number of a video/retransmit packet. A
	/// retransmit over the retransmit budget is dropped here (counted in `Stats::rtx_drops`).
	pub(super) fn push(&mut self, class: Class, data: Vec<u8>, seq: Option<u16>, now: Instant) {
		if class == Class::Retransmit && !self.rtx_admit(data.len(), now) {
			return;
		}
		// Accrue under the queue state the elapsed time actually had.
		self.refill(now);
		if class == Class::Video {
			self.in_bytes += cost(data.len());
			let el = now.saturating_duration_since(self.in_at);
			if el >= INPUT_WINDOW {
				let sample = self.in_bytes / (el.as_secs_f64() * 1000.0);
				self.in_rate = if self.in_rate == 0.0 { sample } else { 0.5 * self.in_rate + 0.5 * sample };
				self.in_bytes = 0.0;
				self.in_at = now;
				self.recompute();
			}
		}
		let item = Item { data, class, seq, at: now };
		if class == Class::Retransmit {
			self.rtx.push_back(item);
		} else {
			self.media.push_back(item);
		}
	}

	fn drop_stale(&mut self, now: Instant) {
		let before = self.stats.drops;
		for q in [&mut self.rtx, &mut self.media] {
			// FIFO per queue → ages are monotone; stop at the first fresh one.
			while let Some(it) = q.front() {
				if now.saturating_duration_since(it.at) <= HARD_DELAY {
					break;
				}
				q.pop_front();
				self.stats.drops += 1;
			}
		}
		if self.stats.drops != before {
			// Tokens a stalled interval accrued at a boosted rate belonged to what was just
			// dropped; the survivors get the normal budget (and their own boost if overdue).
			self.tokens = self.tokens.min(self.depth());
		}
		self.settle_tokens();
	}

	fn head(&self) -> Option<&Item> {
		self.rtx.front().or_else(|| self.media.front())
	}

	/// The next datagram allowed on the wire now (retransmits first), or `None` when the queue
	/// is empty or the bucket must refill first (see `next_wake`).
	pub(super) fn pop(&mut self, now: Instant) -> Option<(Class, Vec<u8>)> {
		self.refill(now);
		self.drop_stale(now);
		let c = cost(self.head()?.data.len());
		if self.enabled && self.tokens + self.eff_rate(now) * BORROW_MS < c {
			return None;
		}
		let it = if self.rtx.is_empty() { self.media.pop_front() } else { self.rtx.pop_front() }?;
		if self.enabled {
			self.tokens -= c;
			self.settle_tokens();
		}
		let waited = now.saturating_duration_since(it.at).as_secs_f64() * 1000.0;
		let s = &mut self.stats;
		if waited > s.max_qdelay_ms {
			s.max_qdelay_ms = waited;
		}
		let n = it.data.len() as u64;
		match it.class {
			Class::Video => {
				s.video_pkts += 1;
				s.video_bytes += n;
			}
			Class::Parity => {
				s.parity_pkts += 1;
				s.parity_bytes += n;
			}
			Class::Retransmit => {
				s.rtx_pkts += 1;
				s.rtx_bytes += n;
			}
		}
		Some((it.class, it.data))
	}

	/// When the head datagram becomes sendable (`None` = queue empty). May be `now`.
	pub(super) fn next_wake(&self, now: Instant) -> Option<Instant> {
		let c = cost(self.head()?.data.len());
		if !self.enabled {
			return Some(now);
		}
		let rate = self.eff_rate(now);
		// Tokens accrue from `self.last`; project from there.
		let since = now.saturating_duration_since(self.last).as_secs_f64() * 1000.0;
		let have = self.accrued(since, rate);
		let deficit = c - have - rate * BORROW_MS;
		if deficit <= 0.0 {
			return Some(now);
		}
		Some(now + Duration::from_secs_f64(deficit / rate / 1000.0))
	}

	/// Sequence numbers still waiting in the pacer (not yet on the wire): a NACK for one of
	/// these is premature — the original is about to leave — so it must not be duplicated.
	pub(super) fn unsent_seqs(&self) -> HashSet<u16> {
		self.rtx.iter().chain(self.media.iter()).filter_map(|it| it.seq).collect()
	}

	/// The window's counters, reset for the next window.
	pub(super) fn take_stats(&mut self) -> Stats {
		std::mem::take(&mut self.stats)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const PKT: usize = 1200;

	fn ms(t0: Instant, t: Instant) -> f64 {
		t.saturating_duration_since(t0).as_secs_f64() * 1000.0
	}

	/// Drive the pacer like the forwarder loop does: send all that is due, then sleep to the
	/// next wake rounded UP to whole milliseconds (a tokio timer), until `until`.
	/// Returns the send times (ms since `t0`) with each datagram's class and length.
	fn drain(p: &mut Pacer, t0: Instant, now: &mut Instant, until: Instant) -> Vec<(f64, Class, usize)> {
		let mut out = Vec::new();
		loop {
			while let Some((c, d)) = p.pop(*now) {
				out.push((ms(t0, *now), c, d.len()));
			}
			let Some(w) = p.next_wake(*now) else { break };
			let wait_ms = ms(*now, w).ceil().max(1.0);
			let next = *now + Duration::from_millis(wait_ms as u64);
			if next > until {
				break;
			}
			*now = next;
		}
		out
	}

	fn push_frame(p: &mut Pacer, bytes: usize, seq: &mut u16, now: Instant) {
		let mut left = bytes;
		while left > 0 {
			let n = left.min(PKT);
			p.push(Class::Video, vec![0u8; n], Some(*seq), now);
			*seq = seq.wrapping_add(1);
			left -= n;
		}
	}

	#[test]
	fn keyframe_at_3_mbit_is_spread_not_burst() {
		let t0 = Instant::now();
		let mut now = t0;
		let mut p = Pacer::new(true, t0);
		p.set_target(3000, 0.0, t0);
		let mut seq = 0u16;
		push_frame(&mut p, 40_000, &mut seq, now);
		let sent = drain(&mut p, t0, &mut now, t0 + Duration::from_secs(1));
		assert_eq!(sent.len(), 34, "every packet leaves");
		let first = sent.first().unwrap().0;
		let last = sent.last().unwrap().0;
		// 2 x 3 Mbit = 750 B/ms; the 16 ms budget (12 KB) leaves at once, the remaining ~29 KB
		// would drip at the rate for ~38 ms, but the queue-delay bound (SOFT_DELAY_MS) drains
		// it faster so it is out within the bound. The old pacer let it all out at once.
		assert!(last - first >= SOFT_DELAY_MS * 0.7, "keyframe spread {:.1} ms", last - first);
		assert!(last - first <= SOFT_DELAY_MS + 2.0, "bounded by the drain bound: {:.1} ms", last - first);
		let at_once = sent.iter().filter(|s| s.0 == first).map(|s| s.2 + WIRE_OVERHEAD).sum::<usize>();
		assert!(at_once as f64 <= 16.0 * 750.0 + 750.0 + PKT as f64, "initial burst {at_once} B within the budget");
		// After the burst: no 5 ms span carries more than the drain rate allows (the base rate,
		// raised to empty the rest within the bound; + one packet + borrow).
		let rest = (40_000 + 34 * WIRE_OVERHEAD) as f64 - 16.0 * 750.0;
		let rate = (rest / SOFT_DELAY_MS).max(750.0);
		for w in 0..60 {
			let lo = first + 1.0 + w as f64;
			let b: usize = sent.iter().filter(|s| s.0 >= lo && s.0 < lo + 5.0).map(|s| s.2 + WIRE_OVERHEAD).sum();
			assert!(b as f64 <= 5.0 * rate + rate + 2.0 * PKT as f64, "5 ms span from {lo} ms carries {b} B");
		}
	}

	#[test]
	fn lan_50_mbit_adds_at_most_2_ms_per_frame() {
		let t0 = Instant::now();
		let mut now = t0;
		let mut p = Pacer::new(true, t0);
		p.set_target(50_000, 0.1, t0);
		let mut seq = 0u16;
		// 120 fps at 50 Mbit = ~52 KB a frame; vary sizes ±40 % for 3 s.
		let interval = Duration::from_micros(8333);
		let mut worst = 0.0f64;
		for i in 0..360u32 {
			let at = t0 + interval * i;
			// Drain anything pending up to this frame's arrival.
			drain(&mut p, t0, &mut now, at);
			now = at;
			let size = 52_000 * (60 + (i * 37 % 80)) as usize / 100;
			push_frame(&mut p, size, &mut seq, now);
			let sent = drain(&mut p, t0, &mut now, at + interval);
			let done = sent.last().map(|s| s.0).unwrap_or(ms(t0, at));
			worst = worst.max(done - ms(t0, at));
		}
		assert!(worst <= 2.0, "worst added delay per frame {worst:.2} ms");
	}

	#[test]
	fn retransmits_jump_the_video_queue() {
		let t0 = Instant::now();
		let mut p = Pacer::new(true, t0);
		p.set_target(1000, 0.0, t0);
		let mut seq = 100u16;
		push_frame(&mut p, 30_000, &mut seq, t0);
		// Drain the burst budget.
		while p.pop(t0).is_some() {}
		assert!(!p.media.is_empty(), "video still queued behind the budget");
		p.push(Class::Retransmit, vec![1u8; 900], Some(7), t0);
		let wake = p.next_wake(t0).unwrap();
		let (c, d) = p.pop(wake + Duration::from_millis(1)).unwrap();
		assert_eq!(c, Class::Retransmit, "the retransmit leaves before queued video");
		assert_eq!(d.len(), 900);
		// Queued seqs are reported so a premature NACK is not duplicated.
		let unsent = p.unsent_seqs();
		assert!(unsent.contains(&(seq - 1)) && !unsent.contains(&7));
	}

	#[test]
	fn parity_leaves_right_after_its_frame() {
		let t0 = Instant::now();
		let mut now = t0;
		let mut p = Pacer::new(true, t0);
		p.set_target(3000, 0.2, t0);
		let mut seq = 0u16;
		push_frame(&mut p, 20_000, &mut seq, now);
		p.push(Class::Parity, vec![2u8; 1200], None, now);
		push_frame(&mut p, 3_000, &mut seq, now);
		let sent = drain(&mut p, t0, &mut now, t0 + Duration::from_secs(1));
		let pos = sent.iter().position(|s| s.1 == Class::Parity).unwrap();
		assert_eq!(pos, 17, "parity follows the 17 packets of its frame, before the next frame");
	}

	#[test]
	fn backlog_drains_within_the_soft_bound() {
		let t0 = Instant::now();
		let mut now = t0;
		let mut p = Pacer::new(true, t0);
		// The encoder dumps 300 KB on a 1 Mbit target (pace floor 2 Mbit = 250 B/ms → 1.2 s
		// at the base rate): the drain bound must cap the wait instead.
		p.set_target(1000, 0.0, t0);
		let mut seq = 0u16;
		push_frame(&mut p, 300_000, &mut seq, now);
		let sent = drain(&mut p, t0, &mut now, t0 + Duration::from_secs(2));
		assert_eq!(sent.len(), 250);
		let last = sent.last().unwrap().0;
		assert!(last <= SOFT_DELAY_MS + 5.0, "backlog drained in {last:.1} ms");
		let st = p.take_stats();
		assert!(st.max_qdelay_ms <= SOFT_DELAY_MS + 5.0, "max queue delay {:.1}", st.max_qdelay_ms);
		assert_eq!(st.drops, 0);
		assert_eq!(st.video_pkts, 250);
	}

	#[test]
	fn a_stalled_send_drops_stale_datagrams_and_keeps_the_latest() {
		let t0 = Instant::now();
		let mut p = Pacer::new(true, t0);
		p.set_target(3000, 0.0, t0);
		let mut seq = 0u16;
		push_frame(&mut p, 60_000, &mut seq, t0); // old frame (50 pkts)
		let late = t0 + Duration::from_millis(300); // the socket send stalled this long
		push_frame(&mut p, 2_400, &mut seq, late); // newest frame (2 pkts)
		let (c, d) = p.pop(late).unwrap();
		assert_eq!((c, d.len()), (Class::Video, PKT));
		assert_eq!(p.take_stats().drops, 50, "the stale frame is dropped");
		assert!(p.unsent_seqs().contains(&51), "the newest frame is still queued/sent");
	}

	#[test]
	fn overshooting_encoder_raises_the_pace() {
		let t0 = Instant::now();
		let mut now = t0;
		let mut p = Pacer::new(true, t0);
		// 300 kbit target but the encoder emits ~4 Mbit/s (500 B/ms) — smooth, don't police.
		p.set_target(300, 0.0, t0);
		let mut seq = 0u16;
		for i in 0..200u32 {
			let at = t0 + Duration::from_millis(10 * i as u64);
			drain(&mut p, t0, &mut now, at);
			now = at;
			push_frame(&mut p, 5_000, &mut seq, now);
		}
		let until = now + Duration::from_secs(1);
		drain(&mut p, t0, &mut now, until);
		let pace = p.pace_kbps(now);
		assert!(pace >= 1.25 * 4000.0 * 0.95, "pace {pace} kbps tracks the intake");
		let st = p.take_stats();
		assert_eq!(st.drops, 0);
		assert!(st.max_qdelay_ms <= SOFT_DELAY_MS + 5.0, "max queue delay {:.1}", st.max_qdelay_ms);
	}

	#[test]
	fn retransmits_beyond_their_share_are_dropped() {
		let t0 = Instant::now();
		let mut p = Pacer::new(true, t0);
		// 4 Mbit target, 15 % parity: retransmits get 25 % of 4.6 Mbit = ~144 B/ms, a 200 ms
		// (~29 KB) bucket.
		p.set_target(4000, 0.15, t0);
		let rate = RTX_SHARE * 4000.0 / 8.0 * 1.15;
		let per = cost(PKT);
		// A NACK storm: 100 packets at once. The bucket's worth goes, the rest is dropped.
		for s in 0..100u16 {
			p.push(Class::Retransmit, vec![1u8; PKT], Some(s), t0);
		}
		let admitted = p.rtx.len();
		assert_eq!(admitted, (200.0 * rate / per).floor() as usize, "one bucket");
		assert_eq!(p.take_stats().rtx_drops, 100 - admitted as u64);
		// Sustained: over 2 s the retransmits get their share (+ the bucket), no more.
		let mut sent = 0usize;
		for i in 0..400u32 {
			let now = t0 + Duration::from_millis(5 * i as u64 + 5);
			p.push(Class::Retransmit, vec![1u8; PKT], Some(i as u16), now);
			while p.pop(now).is_some() {
				sent += 1;
			}
		}
		let bytes = (sent + p.rtx.len()) as f64 * per;
		assert!(bytes <= 2000.0 * rate + 200.0 * rate + per, "{bytes} B of retransmits in 2 s");
		assert!(bytes >= 2000.0 * rate * 0.9, "the share is usable: {bytes} B");
		// Pass-through (the A/B knob) and an unknown target keep every retransmit.
		let mut q = Pacer::new(false, t0);
		q.set_target(4000, 0.15, t0);
		let mut u = Pacer::new(true, t0);
		for s in 0..100u16 {
			q.push(Class::Retransmit, vec![1u8; PKT], Some(s), t0);
			u.push(Class::Retransmit, vec![1u8; PKT], Some(s), t0);
		}
		assert_eq!((q.rtx.len(), u.rtx.len()), (100, 100));
	}

	#[test]
	fn disabled_pacer_passes_through() {
		let t0 = Instant::now();
		let mut p = Pacer::new(false, t0);
		p.set_target(300, 0.0, t0);
		let mut seq = 0u16;
		push_frame(&mut p, 100_000, &mut seq, t0);
		let mut n = 0;
		while p.pop(t0).is_some() {
			n += 1;
		}
		assert_eq!(n, 84);
	}
}
