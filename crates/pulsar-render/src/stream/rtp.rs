//! RTP receive + depacketize → Annex-B (H.264/HEVC) / OBU-stream (AV1) access units.
//!
//! Pure logic (no D3D11/windows deps) so it unit-tests on any host. Mirrors the depacketizers
//! in `src/lib/{h264,h265,av1}.ts`, but the consumer here is Media Foundation, not WebCodecs:
//!
//!   - **H.264 / HEVC**: each NAL is emitted as **Annex-B** — prefixed with the 4-byte start
//!     code `00 00 00 01`. The access unit is the concatenation of its start-code-prefixed NALs.
//!   - **AV1**: the raw **OBU stream** (the concatenated OBUs of the Temporal Unit, low-overhead
//!     bitstream with each OBU's own header + LEB128 size). No Annex-B for AV1.
//!
//! `AccessUnit.key` marks an IDR/keyframe AU; `pts_90k` is the 90 kHz RTP timestamp of the AU.
//!
//! Loss handling mirrors the TS players: track the 16-bit RTP sequence number, and on a forward
//! gap set `awaiting_idr` + drop partial state until the next clean keyframe so a corrupt NAL/TU
//! never reaches the decoder. Duplicate (`fwd==0`) and backward/late (`fwd>=0x8000`, e.g. a NACK
//! retransmit that lands after its frame was assembled) packets are DROPPED without touching
//! payload state — splicing them into the in-flight AU would corrupt it.

#![allow(dead_code)]

use super::{AccessUnit, Codec};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

const START_CODE: [u8; 4] = [0, 0, 0, 1];

// ── Reorder / jitter buffer (adaptive streaming Phase 0.5) ─────────────────────────────
//
// The Linux backend gets this from libav's RTP demuxer (`reorder_queue_size` + `max_delay`);
// this depacketizer used to read straight off the socket, so a NACK retransmit — which by
// construction arrives ~1 RTT AFTER the packets that followed the hole — was always "late"
// and dropped, and every single loss was a wait-for-IDR. `Reorder` holds the packets that
// arrive after a gap for up to `max_delay` (the app's RTT-derived value, stdin `maxdelay`)
// and releases them in sequence once the hole is filled, or skips the hole when the wait
// expires / the buffer fills. In-order streams pass straight through (no added latency).

/// App-derived reorder wait (µs). 0 = not received yet → `DEFAULT_MAX_DELAY_US`.
static MAX_DELAY_US: AtomicU64 = AtomicU64::new(0);
const DEFAULT_MAX_DELAY_US: u64 = 100_000;
/// Ceiling on the app's reorder wait (µs). The app sized it 1.5×RTT + 100 ms plus one repair
/// sweep, which reached ~600 ms on a jittery mobile link: every frame behind a hole was held
/// that long (head-of-line blocking) and then released as one burst. A repair that needs more
/// than a quarter second is useless for interactive video anyway — the RFI recovery frame or
/// the keyframe request heals it sooner. The `PULSAR_MAXDELAY` env pin is not capped (debug).
pub const MAX_DELAY_CAP_US: u64 = 250_000;
/// Keepalive RTT from the app (stdin `rtt <ms>`), µs. 0 = unknown → no early give-up.
static RTT_US: AtomicU64 = AtomicU64::new(0);
/// Early give-up on a hole (see `Reorder::early_give_up`): the pre-repair loss above which a
/// hole is treated as probably unrepairable once later frames are complete behind it…
const HIGH_LOSS: f32 = 0.10;
/// …and the floor of its wait (the other term is 2 × RTT: one NACK round trip plus slack).
const EARLY_GIVE_UP_MIN: Duration = Duration::from_millis(60);
/// Sequence numbers per pre-repair loss sample (~0.1 s at 1080p60 / 10 Mbit).
const LOSS_WINDOW: u32 = 128;

/// Set while the depacketizer drops access units mid-stream waiting for a keyframe or an RFI
/// recovery frame (published by [`pump`]). The renderer shows "recovering" from it instead of
/// leaving a frozen picture unexplained.
pub static WAITING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Packets held behind a gap before flushing regardless (≈ 0.5 s at 8 Mbit / 1200 B —
/// the same bound as the Linux demuxer's `reorder_queue_size`).
pub const REORDER_CAP: usize = 512;
/// Seq distance (either direction) beyond which a jump is an ENCODER RESTART — a fresh
/// ffmpeg on a fresh random RTP base after every ABR resolution/fps/codec restream — not
/// ordinary loss/reorder. Without this, a restart that landed BEHIND the old base (50%
/// odds) made every packet "stale" until the 16-bit seq wrapped past the old cursor —
/// minutes of frozen video at a few hundred packets/s. Mirrors the mobile
/// `REORDER_RESYNC_DISTANCE` (mobile/src/rtp.rs).
pub const RESYNC_DISTANCE: i32 = 256;

/// Set when the depacketizer drops into "wait for a keyframe" mid-stream (a gap its reorder
/// window couldn't wait out, or a resync). The renderer turns it into a `vidsink-needidr`
/// line so the app asks the host for an IDR right away — the app's own NACK bookkeeping can
/// count a late-repaired packet as fine while the decoder already threw the frame away, and
/// the picture then stayed frozen until the host's next periodic keyframe (~10 s).
pub static NEED_IDR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The stream is intra-refreshed (the app writes `ir 1` once the host streams with a rolling
/// intra wave instead of IDRs). Then a mid-stream gap does NOT wait for a keyframe — the
/// stream has none in steady state: the damaged access unit is dropped (and the one the
/// first post-gap packet belongs to, whose head may be gone), decoding resumes at the next
/// whole AU and the wave clears the smear within its period (~1 s). An encoder restart (a
/// far backward jump) and the session start still wait for their IDR.
///
/// Measured 2026-11-08 on Windows (Microsoft HEVC MFT over DXVA, 1200p120): an IR stream with
/// ~1 unrepaired loss/s fed this way overflowed the AU backlog and asked for an IDR ~5×/s, read
/// then as a decoder stall. The RFI work the same day found the likelier cause: the reorder
/// buffer holds every frame behind a hole for the whole repair window and releases them as one
/// burst (~24 at 120 fps), which overflows the 8-AU queue. Not re-measured for IR since, so the
/// Windows app still does not request intra refresh (play.rs `ir_capable`); it uses
/// reference-frame invalidation (`RFI_MODE`) instead.
pub static IR_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The host heals a loss by reference-frame invalidation (the app writes `rfi 1` when the host
/// streams with a deep reference buffer for us). A hole is reported to the host at once —
/// `RFI_REPORT_AFTER` after the first packet behind it, while NACK repair still runs — so the
/// host can still reach the lost frame in its reference buffer: it invalidates it and tags the
/// next frame (predicted from an older, intact one) with a Pulsar SEI. If the repair fails,
/// the depacketizer drops access units until that tag (or a keyframe) instead of waiting for
/// an IDR; no recovery within `RFI_TIMEOUT` → the keyframe request after all. A repaired hole
/// costs nothing but a slightly less efficient recovery frame.
pub static RFI_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Lost sequence range waiting to be sent to the host (the renderer prints `vidsink-rfi`).
static RFI_REQ: std::sync::Mutex<Option<(u16, u16)>> = std::sync::Mutex::new(None);
/// How long a hole must persist before it is reported (plain reordering heals faster).
const RFI_REPORT_AFTER: Duration = Duration::from_millis(3);
/// Waiting longer than this for a recovery frame → ask for a keyframe. The wait starts only
/// when the repair window ran out, by which time a recovery frame (sent ~1 RTT after the
/// early report) has normally arrived already; none = the host could not reach the frame.
/// Every further hole seen while waiting restarts it: that hole was reported too, so a fresh
/// recovery frame is on its way (the previous one may have been the frame the hole hit).
const RFI_TIMEOUT: Duration = Duration::from_millis(150);
/// Upper bound on one recovery wait, however many holes keep restarting `RFI_TIMEOUT`.
const RFI_GIVE_UP: Duration = Duration::from_millis(1000);
/// `pulsar_core::service::media::RFI_RECOVERY_UUID` (this crate has no core dependency).
const RFI_UUID: [u8; 16] = *b"PULSAR-RFI-RECOV";

/// The lost range to report to the host, if any (merged since the last call).
pub fn take_rfi_request() -> Option<(u16, u16)> {
	RFI_REQ.lock().unwrap().take()
}

fn report_lost(first: u16, last: u16) {
	let mut r = RFI_REQ.lock().unwrap();
	*r = Some(match *r {
		Some((f, _)) => (f, last),
		None => (first, last),
	});
}

/// The RTP packet is the host's RFI recovery tag: a single-NAL packet holding the
/// user-data-unregistered SEI with `RFI_UUID` (H.264 type 6 or HEVC prefix SEI 39).
fn is_recovery_tag_packet(p: &[u8]) -> bool {
	if p.len() < 12 {
		return false;
	}
	let mut off = 12 + (p[0] & 0x0f) as usize * 4;
	if p[0] & 0x10 != 0 {
		if off + 4 > p.len() {
			return false;
		}
		off += 4 + (((p[off + 2] as usize) << 8) | p[off + 3] as usize) * 4;
	}
	let Some(pl) = p.get(off..) else { return false };
	let at = |h: usize| pl.len() >= h + 18 && pl[h] == 5 && pl[h + 1] == 16 && pl[h + 2..h + 18] == RFI_UUID;
	(!pl.is_empty() && pl[0] & 0x1f == 6 && at(1)) || (pl.len() > 2 && (pl[0] >> 1) & 0x3f == 39 && at(2))
}

/// The access unit carries the host's RFI recovery tag (a user-data-unregistered SEI).
fn has_recovery_tag(nals: &[Vec<u8>], hevc: bool) -> bool {
	let hdr = if hevc { 2 } else { 1 };
	nals.iter().any(|n| {
		let sei = if hevc {
			n.len() > 2 && (n[0] >> 1) & 0x3f == 39
		} else {
			!n.is_empty() && n[0] & 0x1f == 6
		};
		sei && n.len() >= hdr + 18 && n[hdr] == 5 && n[hdr + 1] == 16 && n[hdr + 2..hdr + 18] == RFI_UUID
	})
}

/// Signed 16-bit sequence distance `seq - from` (wrap-aware, nearest direction).
fn seq_delta(seq: u16, from: u16) -> i32 {
	seq.wrapping_sub(from) as i16 as i32
}

/// App → renderer: the RTT-derived reorder wait (`maxdelay <us>` over stdin).
pub fn set_max_delay_us(us: u64) {
	MAX_DELAY_US.store(us, AtomicOrdering::Relaxed);
}

/// App → renderer: the keepalive RTT (`rtt <ms>` over stdin) — sizes the early give-up.
pub fn set_rtt_ms(ms: f32) {
	if ms.is_finite() && ms > 0.0 {
		RTT_US.store((ms * 1000.0) as u64, AtomicOrdering::Relaxed);
	}
}

fn rtt() -> Option<Duration> {
	Some(RTT_US.load(AtomicOrdering::Relaxed)).filter(|&v| v > 0).map(Duration::from_micros)
}

/// The reorder wait in effect: the `PULSAR_MAXDELAY` env pin, else the app's value capped at
/// `MAX_DELAY_CAP_US`, else the 100 ms default.
pub fn max_delay() -> Duration {
	let us = std::env::var("PULSAR_MAXDELAY")
		.ok()
		.and_then(|v| v.parse::<u64>().ok())
		.or_else(|| {
			Some(MAX_DELAY_US.load(AtomicOrdering::Relaxed))
				.filter(|&v| v > 0)
				.map(|v| v.min(MAX_DELAY_CAP_US))
		})
		.unwrap_or(DEFAULT_MAX_DELAY_US);
	Duration::from_micros(us)
}

/// Sequence-ordered jitter buffer in front of [`Depacketizer`]. Pure (time is passed in).
pub struct Reorder {
	/// Hole starts already reported to the host (RFI); pruned once `next` passes them.
	reported: Vec<u16>,
	/// The next sequence number expected in order. `None` until the first packet.
	next: Option<u16>,
	/// Packets ahead of `next` (a hole precedes them), sorted by forward distance from `next`,
	/// each with its arrival time.
	buf: Vec<(u16, Instant, Vec<u8>)>,
	cap: usize,
	/// Highest sequence number seen so far (pre-repair loss accounting).
	highest: Option<u16>,
	/// Current loss sample: sequence numbers the stream advanced by, and how many of them
	/// were missing on first arrival (later repaired or not).
	win_expected: u32,
	win_missing: u32,
	/// Pre-repair loss fraction (EWMA over `LOSS_WINDOW` samples); < 0 = no sample yet.
	loss: f32,
	/// Tests pin the RTT here instead of the process-global `rtt <ms>` value.
	rtt_pin: Option<Duration>,
	/// Counters since the last [`Reorder::take_stats`] (the pump's 2 s log line).
	stats: ReorderStats,
}

/// What the reorder buffer did in one log window (see [`pump`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ReorderStats {
	/// Holes given up on (released past without their repair) and the packets they spanned.
	pub holes_skipped: u32,
	pub seqs_skipped: u32,
	/// Of those, holes given up early (`early_give_up`) rather than at `max_delay`.
	pub early_skips: u32,
	/// Most packets held behind holes at once.
	pub max_pending: u32,
}

impl Reorder {
	pub fn new(cap: usize) -> Self {
		Self {
			reported: Vec::new(),
			next: None,
			buf: Vec::new(),
			cap: cap.max(2),
			highest: None,
			win_expected: 0,
			win_missing: 0,
			loss: -1.0,
			rtt_pin: None,
			stats: ReorderStats::default(),
		}
	}

	/// The counters since the last call (and reset them).
	pub fn take_stats(&mut self) -> ReorderStats {
		std::mem::take(&mut self.stats)
	}

	/// Pre-repair loss fraction seen here (0 before the first full sample).
	pub fn loss(&self) -> f32 {
		self.loss.max(0.0)
	}

	/// Count a forward arrival for the pre-repair loss estimate. Backward packets (repairs,
	/// duplicates) don't count: their sequence numbers were already counted missing.
	fn note_arrival(&mut self, seq: u16) {
		let Some(h) = self.highest else {
			self.highest = Some(seq);
			return;
		};
		let d = seq.wrapping_sub(h);
		if d == 0 || d >= 0x8000 {
			return;
		}
		self.highest = Some(seq);
		self.win_expected += d as u32;
		self.win_missing += d as u32 - 1;
		if self.win_expected >= LOSS_WINDOW {
			let r = self.win_missing as f32 / self.win_expected as f32;
			self.loss = if self.loss < 0.0 { r } else { self.loss * 0.5 + r * 0.5 };
			self.win_expected = 0;
			self.win_missing = 0;
		}
	}

	/// How long a hole may hold up the stream when it is probably unrepairable: under high
	/// pre-repair loss (≥ `HIGH_LOSS`) with a known RTT, max(2 × RTT, 60 ms) — one NACK round
	/// trip with slack; a retransmit that has not landed by then mostly never will (it was
	/// lost too, or the sender's repair window passed). `None` = wait the full `max_delay`.
	/// Only applied once a later frame is complete behind the hole (`later_frame_complete`):
	/// then waiting costs latency on every frame queued up behind it.
	fn early_give_up(&self) -> Option<Duration> {
		if self.loss < HIGH_LOSS {
			return None;
		}
		let rtt = self.rtt_pin.or_else(rtt)?;
		Some((rtt * 2).max(EARLY_GIVE_UP_MIN))
	}

	/// A whole frame waits behind a hole: a contiguous run of held packets that starts right
	/// after a held marker packet (the end of the previous frame) and ends with a marker.
	fn later_frame_complete(&self) -> bool {
		let mut prev: Option<(u16, bool)> = None;
		let mut open = false;
		for (s, _, p) in &self.buf {
			let marker = p.len() >= 2 && p[1] & 0x80 != 0;
			match prev {
				Some((ps, pm)) if ps.wrapping_add(1) == *s => {
					if pm {
						open = true;
					}
				}
				_ => open = false,
			}
			if open && marker {
				return true;
			}
			prev = Some((*s, marker));
		}
		false
	}

	fn seq_of(p: &[u8]) -> Option<u16> {
		(p.len() >= 12).then(|| ((p[2] as u16) << 8) | p[3] as u16)
	}

	/// Packets currently held behind a hole.
	pub fn pending(&self) -> usize {
		self.buf.len()
	}

	/// Feed one datagram; returns the packets that are deliverable now, in sequence order.
	/// Duplicates and stale (backward) packets pass straight through — the depacketizer
	/// already drops those without touching its state.
	pub fn push(&mut self, pkt: Vec<u8>, now: Instant, max_delay: Duration) -> Vec<Vec<u8>> {
		let mut out = Vec::new();
		let Some(seq) = Self::seq_of(&pkt) else {
			out.push(pkt);
			return out;
		};
		match self.next {
			None => {
				self.next = Some(seq.wrapping_add(1));
				self.highest = Some(seq);
				out.push(pkt);
			}
			Some(next) if seq_delta(seq, next).abs() > RESYNC_DISTANCE => {
				// Encoder restart (fresh RTP base): the held packets belong to the dead stream —
				// drop them and resync to the new base (the depacketizer re-anchors on a key).
				self.buf.clear();
				self.next = Some(seq.wrapping_add(1));
				self.highest = Some(seq);
				out.push(pkt);
			}
			Some(next) => {
				self.note_arrival(seq);
				let d = seq.wrapping_sub(next);
				if d == 0 {
					// The expected packet (possibly the retransmit that fills the hole).
					self.next = Some(seq.wrapping_add(1));
					out.push(pkt);
					self.drain(&mut out);
				} else if d >= 0x8000 {
					// Older than what we already released: stale / duplicate — pass through.
					out.push(pkt);
				} else {
					// Ahead of a hole: hold it, sorted by distance from `next`.
					if !self.buf.iter().any(|(s, _, _)| *s == seq) {
						let pos = self
							.buf
							.iter()
							.position(|(s, _, _)| s.wrapping_sub(next) > d)
							.unwrap_or(self.buf.len());
						self.buf.insert(pos, (seq, now, pkt));
						self.stats.max_pending = self.stats.max_pending.max(self.buf.len() as u32);
					}
					if self.buf.len() >= self.cap {
						self.skip(&mut out);
					}
				}
			}
		}
		self.report_holes(now, false);
		self.release_to_recovery(&mut out);
		self.expire(now, max_delay, &mut out);
		out
	}

	/// Time-based release (call on read timeouts): a head that has waited `max_delay` is
	/// released past its hole.
	pub fn poll(&mut self, now: Instant, max_delay: Duration) -> Vec<Vec<u8>> {
		let mut out = Vec::new();
		self.report_holes(now, false);
		self.expire(now, max_delay, &mut out);
		out
	}

	/// RFI: the host's recovery frame has arrived behind a hole (its tag is a packet of its own,
	/// the first of the frame). Nothing before it is needed any more — every frame from the
	/// hole up to it was predicted from the lost one — so stop waiting for those repairs and
	/// release through it now. Holding them for the whole repair window would release a burst
	/// of frames at once (~24 at 120 fps), which overflows the renderer's AU queue and drops
	/// the recovery frame itself. Holes after the tag keep waiting (they may be its own).
	fn release_to_recovery(&mut self, out: &mut Vec<Vec<u8>>) {
		if !RFI_MODE.load(AtomicOrdering::Relaxed) {
			return;
		}
		let Some(tag) = self.buf.iter().find(|(_, _, p)| is_recovery_tag_packet(p)).map(|(s, _, _)| *s) else {
			return;
		};
		while self.buf.iter().any(|(s, _, _)| *s == tag) {
			self.skip(out);
		}
	}

	/// RFI: report every hole in front of a held packet that has persisted `RFI_REPORT_AFTER`
	/// (`force` = report now, before giving up on it), once each.
	fn report_holes(&mut self, now: Instant, force: bool) {
		if !RFI_MODE.load(AtomicOrdering::Relaxed) {
			return;
		}
		let Some(mut expect) = self.next else { return };
		let next = expect;
		self.reported.retain(|s| s.wrapping_sub(next) < 0x8000);
		for (s, t, _) in &self.buf {
			if *s != expect
				&& (force || now.duration_since(*t) >= RFI_REPORT_AFTER)
				&& !self.reported.contains(&expect)
			{
				self.reported.push(expect);
				report_lost(expect, s.wrapping_sub(1));
			}
			expect = s.wrapping_add(1);
		}
	}

	/// Release past the head hole once it waited `max_delay` — or, when it is probably
	/// unrepairable, `early_give_up` (only with a whole later frame stuck behind it).
	fn expire(&mut self, now: Instant, max_delay: Duration, out: &mut Vec<Vec<u8>>) {
		let early = self.early_give_up().filter(|e| *e < max_delay);
		while let Some((_, t, _)) = self.buf.first() {
			let waited = now.duration_since(*t);
			if waited >= max_delay {
				self.skip(out);
			} else if early.is_some_and(|e| waited >= e && self.later_frame_complete()) {
				self.stats.early_skips += 1;
				self.skip(out);
			} else {
				break;
			}
		}
	}

	/// Give up on the hole in front of the head: jump `next` to it and drain the run.
	fn skip(&mut self, out: &mut Vec<Vec<u8>>) {
		// A hole skipped before it was reported (the buffer filled up) is reported now.
		self.report_holes(Instant::now(), true);
		if let Some((s, _, _)) = self.buf.first() {
			if let Some(next) = self.next {
				self.stats.holes_skipped += 1;
				self.stats.seqs_skipped += s.wrapping_sub(next) as u32;
			}
			self.next = Some(*s);
			self.drain(out);
		}
	}

	/// Release every buffered packet that is now contiguous with `next`.
	fn drain(&mut self, out: &mut Vec<Vec<u8>>) {
		while let (Some(next), Some((s, _, _))) = (self.next, self.buf.first()) {
			if *s != next {
				break;
			}
			let (_, _, p) = self.buf.remove(0);
			self.next = Some(next.wrapping_add(1));
			out.push(p);
		}
	}
}

/// Depacketizer state machine: feed RTP packets, get complete access units.
pub struct Depacketizer {
	codec: Codec,
	// --- RTP sequence / loss tracking (shared across codecs) ---
	last_seq: Option<u16>,
	awaiting_idr: bool,
	// PTS (90 kHz RTP timestamp) of the AU currently being assembled.
	cur_ts: u32,
	// --- H.264 / HEVC: completed NALs of the current AU (raw, no start code yet) ---
	nals: Vec<Vec<u8>>,
	// In-flight fragmentation unit (FU-A / FU) — the reconstructed NAL bytes so far.
	fu: Option<Vec<u8>>,
	// --- AV1: bytes of the Temporal Unit being assembled across packets ---
	av1_tu: Vec<u8>,
	av1_new_seq: bool,
	// Intra-refresh resume (see `IR_MODE`): packets of this AU (RTP timestamp) are dropped —
	// it was hit by a gap — and assembly restarts with the next timestamp.
	skip_ts: Option<u32>,
	// RFI (see `RFI_MODE`): since when access units are dropped until the host's recovery
	// frame (or a keyframe) — (first hole of this wait, latest hole; see `RFI_TIMEOUT`).
	awaiting_recovery: Option<(Instant, Instant)>,
	// RTP timestamp of the access unit the last hole ran into (it may carry the recovery tag,
	// but not whole: it must not end the wait).
	gap_ts: Option<u32>,
	// RFI: the keyframe wait came from a recovery wait that timed out — a whole recovery frame
	// still ends it (see `recovered`).
	rfi_fallback: bool,
	// An access unit has been delivered (the stream was live): a wait from here on is a
	// mid-stream freeze (`WAITING`), not the session start.
	delivered: bool,
}

impl Depacketizer {
	pub fn new(codec: Codec) -> Self {
		Self {
			codec,
			last_seq: None,
			awaiting_idr: true,
			cur_ts: 0,
			nals: Vec::new(),
			fu: None,
			av1_tu: Vec::new(),
			av1_new_seq: false,
			skip_ts: None,
			awaiting_recovery: None,
			gap_ts: None,
			rfi_fallback: false,
			delivered: false,
		}
	}

	/// Access units are being dropped mid-stream until a keyframe or an RFI recovery frame.
	pub fn waiting(&self) -> bool {
		self.delivered && (self.awaiting_idr || self.awaiting_recovery.is_some())
	}

	/// Drop any partial AU/TU state (called on a detected sequence gap).
	fn drop_partial(&mut self) {
		self.fu = None;
		self.nals.clear();
		self.av1_tu.clear();
		self.av1_new_seq = false;
	}

	/// Feed one RTP packet (full UDP payload). Returns a complete access unit when the RTP
	/// marker bit closes one.
	pub fn push(&mut self, rtp_packet: &[u8]) -> Option<AccessUnit> {
		if rtp_packet.len() < 12 {
			return None;
		}
		let b0 = rtp_packet[0];
		let b1 = rtp_packet[1];
		let has_ext = (b0 & 0x10) != 0;
		let cc = (b0 & 0x0f) as usize;
		let marker = (b1 & 0x80) != 0;
		let seq = ((rtp_packet[2] as u16) << 8) | rtp_packet[3] as u16;
		let ts = ((rtp_packet[4] as u32) << 24)
			| ((rtp_packet[5] as u32) << 16)
			| ((rtp_packet[6] as u32) << 8)
			| rtp_packet[7] as u32;

		// Sequence-gap detection: forward distance only. On a gap drop partial state and wait
		// for the next keyframe.
		if let Some(last) = self.last_seq {
			let fwd = seq.wrapping_sub(last);
			// A far backward jump is an encoder restart on a fresh RTP base, not a late
			// retransmit: resync to it and wait for its keyframe (see RESYNC_DISTANCE).
			// (A far FORWARD jump is already handled below as a gap → await keyframe.)
			if seq_delta(seq, last) < -RESYNC_DISTANCE {
				self.awaiting_recovery = None;
				self.rfi_fallback = false;
				self.awaiting_idr = true;
				NEED_IDR.store(true, std::sync::atomic::Ordering::Relaxed);
				self.drop_partial();
				self.last_seq = Some(seq);
			// fwd==1: consecutive (normal). fwd==0: duplicate. fwd>=0x8000: backward/reorder/late.
			} else if fwd == 0 || fwd >= 0x8000 {
				// Duplicate, reordered, or a NACK retransmit that arrives ≥1 RTT after its frame
				// was already assembled: DROP it WITHOUT touching payload state. Splicing its
				// NAL/FU/OBU bytes into the AU currently being assembled would corrupt that
				// (unrelated) AU — during keyframe assembly it poisons the whole next GOP — and a
				// late retransmit can never heal a gap we are already past. Leave last_seq and all
				// partial state untouched. (Mirrors the mobile depacketizer's stale-retransmit
				// guard in mobile/src/rtp.rs.)
				return None;
			}
			// Forward packet (newer than last seen).
			if fwd != 1 {
				// Gap: one or more sequence numbers were skipped.
				self.drop_partial();
				// RFI: an access unit a hole ran through is never a usable recovery frame. A hole
				// that ends right at the recovery tag did not run through it: the tag is a packet
				// of its own, the first of its frame (all of the frames before it lost or
				// skipped — the common case at high loss, where nothing between the hole and
				// the recovery frame survived).
				self.gap_ts = if is_recovery_tag_packet(rtp_packet) { None } else { Some(ts) };
				if IR_MODE.load(std::sync::atomic::Ordering::Relaxed) && !self.awaiting_idr {
					// Intra-refresh stream: no keyframe is coming. Skip the AU this packet
					// belongs to (its head may be in the gap) and resume with the next one.
					self.skip_ts = Some(ts);
				} else if RFI_MODE.load(std::sync::atomic::Ordering::Relaxed)
					&& !self.awaiting_idr
					&& self.codec != Codec::Av1
				{
					// The reorder buffer already reported this hole to the host: drop access
					// units until its recovery frame. A further hole while waiting restarts the
					// timeout (it was reported too: another recovery frame is due).
					let now = Instant::now();
					self.awaiting_recovery = Some(match self.awaiting_recovery {
						Some((first, _)) => (first, now),
						None => (now, now),
					});
				} else {
					self.awaiting_idr = true;
					NEED_IDR.store(true, std::sync::atomic::Ordering::Relaxed);
				}
			}
			self.last_seq = Some(seq);
		} else {
			self.last_seq = Some(seq);
		}

		// Skip the CSRC list and (if present) the RTP header extension.
		let mut off = 12 + cc * 4;
		if has_ext {
			if off + 4 > rtp_packet.len() {
				return None;
			}
			let el = ((rtp_packet[off + 2] as usize) << 8) | rtp_packet[off + 3] as usize;
			off += 4 + el * 4;
		}
		if off > rtp_packet.len() {
			return None;
		}
		let pl = &rtp_packet[off..];
		if pl.is_empty() {
			return None;
		}

		// Intra-refresh resume: still inside the AU a gap damaged → drop; a new timestamp is
		// the first packet of a whole AU again.
		if let Some(skip) = self.skip_ts {
			if ts == skip {
				return None;
			}
			self.skip_ts = None;
		}
		// The timestamp of this packet is the PTS of the AU it belongs to.
		self.cur_ts = ts;

		match self.codec {
			Codec::H264 => self.push_h264(pl, marker),
			Codec::H265 => self.push_h265(pl, marker),
			Codec::Av1 => self.push_av1(pl, marker),
		}
	}

	// ---- H.264 (RFC 6184) ------------------------------------------------------------------

	fn push_h264(&mut self, pl: &[u8], marker: bool) -> Option<AccessUnit> {
		let t = pl[0] & 0x1f;
		if (1..=23).contains(&t) {
			// Single NAL unit.
			self.nals.push(pl.to_vec());
		} else if t == 24 {
			// STAP-A: aggregated NALs, each prefixed by a 2-byte size.
			let mut q = 1usize;
			while q + 2 <= pl.len() {
				let s = ((pl[q] as usize) << 8) | pl[q + 1] as usize;
				q += 2;
				if q + s > pl.len() {
					break;
				}
				self.nals.push(pl[q..q + s].to_vec());
				q += s;
			}
		} else if t == 28 {
			// FU-A: a NAL fragmented across packets.
			if pl.len() < 2 {
				return None;
			}
			let fh = pl[1];
			let start = (fh & 0x80) != 0;
			let end = (fh & 0x40) != 0;
			let orig_type = fh & 0x1f;
			let nri = pl[0] & 0x60;
			if start {
				// Reconstruct the original 1-byte NAL header.
				self.fu = Some(vec![nri | orig_type]);
			}
			if let Some(fu) = self.fu.as_mut() {
				fu.extend_from_slice(&pl[2..]);
				if end {
					let nal = self.fu.take().unwrap();
					self.nals.push(nal);
				}
			}
		}
		if marker {
			self.emit_annexb_h264()
		} else {
			None
		}
	}

	fn emit_annexb_h264(&mut self) -> Option<AccessUnit> {
		if self.nals.is_empty() {
			return None;
		}
		let nals = std::mem::take(&mut self.nals);
		let mut key = false;
		for n in &nals {
			if n.is_empty() {
				continue;
			}
			let t = n[0] & 0x1f;
			if t == 5 || t == 7 {
				// IDR (5) or SPS (7) → keyframe AU.
				key = true;
			}
		}
		if self.awaiting_idr {
			if !key && !self.rfi_fallback_recovers(&nals, false) {
				return None; // wait for a clean keyframe before resuming.
			}
			self.awaiting_idr = false;
			self.rfi_fallback = false;
		}
		if !self.recovered(key, &nals, false) {
			return None;
		}
		self.delivered = true;
		let mut data = Vec::new();
		for n in &nals {
			data.extend_from_slice(&START_CODE);
			data.extend_from_slice(n);
		}
		Some(AccessUnit {
			data,
			pts_90k: self.cur_ts,
			key,
		})
	}

	// ---- HEVC (RFC 7798) -------------------------------------------------------------------

	fn push_h265(&mut self, pl: &[u8], marker: bool) -> Option<AccessUnit> {
		if pl.len() < 2 {
			return None;
		}
		let nal_type = (pl[0] >> 1) & 0x3f;
		if nal_type <= 47 {
			// Single NAL unit — already carries its 2-byte header.
			self.nals.push(pl.to_vec());
		} else if nal_type == 48 {
			// Aggregation Packet: 2-byte PayloadHdr + [16-bit size + NAL]xN. Each aggregated NAL
			// includes its own 2-byte nal_unit_header (no DONL/DOND; matches the TS path).
			let mut q = 2usize;
			while q + 2 <= pl.len() {
				let sz = ((pl[q] as usize) << 8) | pl[q + 1] as usize;
				q += 2;
				if sz == 0 || q + sz > pl.len() {
					break;
				}
				self.nals.push(pl[q..q + sz].to_vec());
				q += sz;
			}
		} else if nal_type == 49 {
			// Fragmentation Unit: 2-byte PayloadHdr + 1-byte FU header + payload.
			if pl.len() < 3 {
				return None;
			}
			let fu_hdr0 = pl[0];
			let fu_hdr1 = pl[1];
			let fuhdr = pl[2];
			let start = (fuhdr & 0x80) != 0;
			let end = (fuhdr & 0x40) != 0;
			let fu_type = fuhdr & 0x3f;
			// nuh_layer_id (6 bits) + nuh_temporal_id_plus1 (3 bits) from the FU PayloadHdr.
			let layer_id = ((fu_hdr0 & 0x01) << 5) | ((fu_hdr1 >> 3) & 0x1f);
			let tid = fu_hdr1 & 0x07;
			if start {
				// Reconstruct the original 2-byte HEVC NAL header.
				let h0 = (fu_type << 1) | ((layer_id >> 5) & 0x01);
				let h1 = ((layer_id & 0x1f) << 3) | (tid & 0x07);
				self.fu = Some(vec![h0, h1]);
			}
			if let Some(fu) = self.fu.as_mut() {
				fu.extend_from_slice(&pl[3..]);
				if end {
					let nal = self.fu.take().unwrap();
					self.nals.push(nal);
				}
			}
		}
		if marker {
			self.emit_annexb_h265()
		} else {
			None
		}
	}

	fn emit_annexb_h265(&mut self) -> Option<AccessUnit> {
		if self.nals.is_empty() {
			return None;
		}
		let nals = std::mem::take(&mut self.nals);
		let mut key = false;
		for n in &nals {
			if n.is_empty() {
				continue;
			}
			let nal_type = (n[0] >> 1) & 0x3f;
			// Key NALs: VPS=32 / SPS=33 / PPS=34, and IRAP VCL types 16..=21 (incl.
			// IDR_W_RADL=19, IDR_N_LP=20, CRA=21).
			if (16..=21).contains(&nal_type) || (32..=34).contains(&nal_type) {
				key = true;
			}
		}
		if self.awaiting_idr {
			if !key && !self.rfi_fallback_recovers(&nals, true) {
				return None;
			}
			self.awaiting_idr = false;
			self.rfi_fallback = false;
		}
		if !self.recovered(key, &nals, true) {
			return None;
		}
		self.delivered = true;
		let mut data = Vec::new();
		for n in &nals {
			data.extend_from_slice(&START_CODE);
			data.extend_from_slice(n);
		}
		Some(AccessUnit {
			data,
			pts_90k: self.cur_ts,
			key,
		})
	}

	/// RFI: may this access unit be decoded? Yes unless waiting for a recovery frame and it is
	/// neither that (the tag) nor a keyframe. A recovery that never comes → keyframe request.
	fn recovered(&mut self, key: bool, nals: &[Vec<u8>], hevc: bool) -> bool {
		let Some((first, last)) = self.awaiting_recovery else {
			return true;
		};
		let whole = self.gap_ts != Some(self.cur_ts);
		if key || (whole && has_recovery_tag(nals, hevc)) {
			self.awaiting_recovery = None;
			return true;
		}
		if last.elapsed() >= RFI_TIMEOUT || first.elapsed() >= RFI_GIVE_UP {
			self.awaiting_recovery = None;
			self.awaiting_idr = true;
			self.rfi_fallback = true;
			NEED_IDR.store(true, std::sync::atomic::Ordering::Relaxed);
		}
		false
	}

	/// RFI: a keyframe wait that a timed-out recovery wait started still ends at a whole
	/// recovery frame. Holes keep being reported while it waits, and the host invalidates
	/// every frame predicted from a lost one, so its recovery frame references a frame from
	/// before the first hole — one this decoder holds. At high loss a small recovery frame
	/// arrives whole long before a large keyframe does (field log: 40 % loss, 1080p IDRs
	/// never whole). Not after a session start or an encoder restart (no `rfi_fallback`).
	fn rfi_fallback_recovers(&self, nals: &[Vec<u8>], hevc: bool) -> bool {
		self.rfi_fallback
			&& RFI_MODE.load(AtomicOrdering::Relaxed)
			&& self.gap_ts != Some(self.cur_ts)
			&& has_recovery_tag(nals, hevc)
	}

	// ---- AV1 (aomedia "RTP Payload Format For AV1") ----------------------------------------

	fn push_av1(&mut self, pl: &[u8], marker: bool) -> Option<AccessUnit> {
		// Aggregation header: Z(7) Y(6) W(5..4) N(3).
		let agg = pl[0];
		let w = (agg >> 4) & 0x03; // element count (0 = all length-prefixed)
		let n = (agg & 0x08) != 0; // start of a new coded video sequence
		if n {
			self.av1_new_seq = true;
		}

		let mut p = 1usize;
		let mut idx = 0u8;
		while p < pl.len() {
			idx += 1;
			let is_last = w != 0 && idx == w;
			let elem_len: usize = if is_last {
				// W>0: the last element has no length prefix; runs to the end of the payload.
				pl.len() - p
			} else {
				let (val, len) = read_leb128(pl, p);
				p += len;
				val
			};
			if p + elem_len > pl.len() {
				// Malformed/truncated — bail rather than splice garbage.
				break;
			}
			// Z/Y continuation is handled transparently by raw byte concatenation: the OBU's own
			// size field (set by the host) spans the reassembled whole.
			self.av1_tu.extend_from_slice(&pl[p..p + elem_len]);
			p += elem_len;
		}

		if marker {
			self.emit_av1()
		} else {
			None
		}
	}

	fn emit_av1(&mut self) -> Option<AccessUnit> {
		if self.av1_tu.is_empty() {
			self.av1_new_seq = false;
			return None;
		}
		let data = std::mem::take(&mut self.av1_tu);
		let new_seq = self.av1_new_seq;
		self.av1_new_seq = false;

		let (has_seq_header, has_frame) = inspect_av1_tu(&data);
		// A key TU carries a sequence header + a frame (Sunshine/Moonlight emit a seq-header OBU
		// before every keyframe). The RTP N bit is a corroborating hint.
		let key = (has_seq_header && has_frame) || new_seq;

		if self.awaiting_idr {
			if !key {
				return None;
			}
			self.awaiting_idr = false;
		}
		self.delivered = true;
		Some(AccessUnit {
			data,
			pts_90k: self.cur_ts,
			key,
		})
	}
}

/// Read a LEB128 unsigned integer (AV1 spec 4.10.5). Returns (value, bytes consumed).
fn read_leb128(data: &[u8], pos: usize) -> (usize, usize) {
	let mut value: usize = 0;
	let mut len = 0usize;
	for i in 0..8 {
		if pos + i >= data.len() {
			break;
		}
		let b = data[pos + i];
		value |= ((b & 0x7f) as usize) << (7 * i);
		len += 1;
		if (b & 0x80) == 0 {
			break;
		}
	}
	(value, len)
}

/// Scan a reassembled Temporal Unit's OBUs for a sequence-header OBU (type 1) and a frame
/// (FRAME=6 / FRAME_HEADER=3). Returns (has_seq_header, has_frame). Walks OBU headers by their
/// LEB128 size fields (low-overhead bitstream).
fn inspect_av1_tu(tu: &[u8]) -> (bool, bool) {
	const OBU_SEQUENCE_HEADER: u8 = 1;
	const OBU_FRAME_HEADER: u8 = 3;
	const OBU_FRAME: u8 = 6;
	let mut has_seq_header = false;
	let mut has_frame = false;
	let mut p = 0usize;
	while p < tu.len() {
		let b = tu[p];
		// obu_header: forbidden(1) type(4) ext(1) has_size(1) reserved(1)
		let obu_type = (b >> 3) & 0x0f;
		let has_ext = (b & 0x04) != 0;
		let has_size = (b & 0x02) != 0;
		let mut q = p + 1;
		if has_ext {
			q += 1; // obu_extension_header
		}
		if q > tu.len() {
			break; // truncated OBU header — bail safely
		}
		let payload_len: usize = if has_size {
			let (val, len) = read_leb128(tu, q);
			q += len;
			val
		} else {
			tu.len() - q // no size field → rest of the TU
		};
		if obu_type == OBU_SEQUENCE_HEADER {
			has_seq_header = true;
		} else if obu_type == OBU_FRAME || obu_type == OBU_FRAME_HEADER {
			has_frame = true;
		}
		if q + payload_len > tu.len() {
			break;
		}
		p = q + payload_len;
		if !has_size && payload_len == 0 {
			break; // guard against a no-size trailing OBU loop
		}
	}
	(has_seq_header, has_frame)
}

/// Bind `0.0.0.0:<port>` (large SO_RCVBUF if easy), receive RTP, depacketize, and push completed
/// `AccessUnit`s into `sink`. Blocks; run on a thread. Stops when `stop` is set. Loss handling
/// (seq gaps → await keyframe) lives in `Depacketizer`.
pub fn recv_loop(
	port: u16,
	codec: Codec,
	sink: impl FnMut(AccessUnit),
	stop: &std::sync::atomic::AtomicBool,
) {
	use std::net::UdpSocket;
	use std::sync::atomic::Ordering;
	use std::time::Duration;

	// BIG receive buffer: the OS default (64 KiB on Windows) overflows on IDR bursts
	// at high fps — the depacketizer then waits for a keyframe that never arrives
	// whole. 4 MiB matches the app's node socket (pulsar-core node.rs).
	//
	// Retry on EADDRINUSE for up to ~800 ms: on a codec/monitor switch the orchestrator
	// (respawn_render_for_codec) kills the old renderer BEFORE spawning this one
	// (kill+wait via stop_render_child), so the port should already be free. But there
	// can be a brief OS-level delay between TerminateProcess returning and the kernel
	// fully releasing the bound UDP socket, and on some Windows versions a killed-but-
	// not-yet-reaped child still holds the port for a tick. Retrying here (instead of
	// returning immediately) turns a transient race into a non-event rather than a
	// permanent black screen for the rest of the session.
	let sock = 'bind: {
		let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
		let mut last_err = None;
		for attempt in 0..8u32 {
			if attempt > 0 {
				std::thread::sleep(Duration::from_millis(100));
			}
			if stop.load(Ordering::SeqCst) {
				return; // session torn down while we were waiting — don't spin longer
			}
			match (|| -> std::io::Result<UdpSocket> {
				let s = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, None)?;
				let _ = s.set_recv_buffer_size(4 * 1024 * 1024);
				s.bind(&addr.into())?;
				Ok(s.into())
			})() {
				Ok(s) => break 'bind s,
				Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
					eprintln!(
						"pulsar-render(win): rtp bind 0.0.0.0:{port} EADDRINUSE (attempt {attempt}), retrying…"
					);
					last_err = Some(e);
				}
				Err(e) => {
					eprintln!("pulsar-render(win): rtp bind 0.0.0.0:{port} failed: {e}");
					return;
				}
			}
		}
		eprintln!(
			"pulsar-render(win): rtp bind 0.0.0.0:{port} still busy after retries: {}",
			last_err.unwrap()
		);
		return;
	};
	// Short read timeout so `stop` is honored promptly between datagrams AND the reorder
	// buffer's time-based release runs while the socket is quiet (a hole with nothing
	// arriving behind it must still be skipped after `max_delay`).
	let _ = sock.set_read_timeout(Some(Duration::from_millis(20)));
	pump(|b| sock.recv(b), codec, sink, || {}, stop);
}

/// Linux: bind the abstract-namespace Unix datagram socket `name` (the app writes it into the
/// SDP as `a=x-pulsar-unix:`) and run [`pump`] on it. The app forwards every video RTP datagram
/// there instead of to a loopback UDP port: no IP stack, no netfilter on the per-packet path.
/// Retries the bind for ~800 ms (a previous decode pass of this process may still be closing
/// the same name). Returns when `stop` is set or the bind fails for good.
#[cfg(target_os = "linux")]
pub fn recv_loop_unix(
	name: &str,
	codec: Codec,
	sink: impl FnMut(AccessUnit),
	tick: impl FnMut(),
	stop: &std::sync::atomic::AtomicBool,
) {
	use std::os::linux::net::SocketAddrExt;
	use std::os::unix::net::{SocketAddr, UnixDatagram};
	use std::sync::atomic::Ordering;
	let addr = match SocketAddr::from_abstract_name(name.as_bytes()) {
		Ok(a) => a,
		Err(e) => {
			eprintln!("pulsar-render: bad unix video socket name {name:?}: {e}");
			return;
		}
	};
	let mut sock = None;
	for attempt in 0..8u32 {
		if attempt > 0 {
			std::thread::sleep(Duration::from_millis(100));
		}
		if stop.load(Ordering::SeqCst) {
			return;
		}
		match UnixDatagram::bind_addr(&addr) {
			Ok(s) => {
				sock = Some(s);
				break;
			}
			Err(e) => eprintln!("pulsar-render: unix video bind @{name} failed (attempt {attempt}): {e}"),
		}
	}
	let Some(sock) = sock else {
		eprintln!("pulsar-render: unix video socket @{name} still unavailable — no video");
		return;
	};
	eprintln!("pulsar-render: video over unix socket @{name} (rust RTP depacketizer)");
	let _ = sock.set_read_timeout(Some(Duration::from_millis(20)));
	pump(|b| sock.recv(b), codec, sink, tick, stop);
}

/// The receive → reorder → depacketize loop behind [`recv_loop`], over any datagram source
/// (`recv` must time out every ~20 ms with `WouldBlock`/`TimedOut` so `stop` and the reorder
/// buffer's time-based release run while the source is quiet). `tick` runs after every
/// datagram and every timeout — the Linux backend reports RFI holes / keyframe requests
/// from it the moment the reorder buffer raises them. Blocks until `stop` or a hard error.
pub fn pump(
	mut recv: impl FnMut(&mut [u8]) -> std::io::Result<usize>,
	codec: Codec,
	mut sink: impl FnMut(AccessUnit),
	mut tick: impl FnMut(),
	stop: &std::sync::atomic::AtomicBool,
) {
	use std::sync::atomic::Ordering;

	let mut depacketizer = Depacketizer::new(codec);
	let mut reorder = Reorder::new(REORDER_CAP);
	WAITING.store(false, Ordering::Relaxed);
	let mut buf = [0u8; 65536];
	// DEBUG knob (env-gated, default off): drop one of every N video packets AFTER the app's
	// NACK/FEC repair — a deterministic unrepaired loss to exercise the resume path.
	let test_drop: u64 = std::env::var("PULSAR_TEST_DROP")
		.ok()
		.and_then(|v| v.parse().ok())
		.filter(|&n: &u64| n >= 2)
		.unwrap_or(0);
	let mut rx_count: u64 = 0;
	// One stderr line (→ the app log) per 2 s window in which a hole was given up on, so a
	// field log tells repairable from unrepairable loss. Quiet on a clean stream.
	let mut stats_at = Instant::now();
	while !stop.load(Ordering::SeqCst) {
		if stats_at.elapsed() >= Duration::from_secs(2) {
			stats_at = Instant::now();
			let st = reorder.take_stats();
			if st.holes_skipped > 0 {
				eprintln!(
					"pulsar-render: rtp 2s: pre-repair loss {:.1} %, {} hole(s) given up ({} pkt, {} early), max held {}, max delay {} ms, waiting {}",
					reorder.loss() * 100.0,
					st.holes_skipped,
					st.seqs_skipped,
					st.early_skips,
					st.max_pending,
					max_delay().as_millis(),
					depacketizer.waiting() as u8,
				);
			}
		}
		match recv(&mut buf) {
			Ok(n) => {
				rx_count += 1;
				if test_drop > 0 && rx_count % test_drop == 0 {
					continue;
				}
				for p in reorder.push(buf[..n].to_vec(), Instant::now(), max_delay()) {
					if let Some(au) = depacketizer.push(&p) {
						sink(au);
					}
				}
				WAITING.store(depacketizer.waiting(), Ordering::Relaxed);
				tick();
			}
			Err(e) => match e.kind() {
				std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
					for p in reorder.poll(Instant::now(), max_delay()) {
						if let Some(au) = depacketizer.push(&p) {
							sink(au);
						}
					}
					WAITING.store(depacketizer.waiting(), Ordering::Relaxed);
					tick();
					continue;
				}
				std::io::ErrorKind::Interrupted => continue,
				_ => {
					eprintln!("pulsar-render: rtp recv error: {e}");
					break;
				}
			},
		}
	}
}

// ===========================================================================================
#[cfg(test)]
mod tests {
	use super::*;

	// --- RTP packet builder (12-byte header, no CSRC/extension) ---
	fn rtp(seq: u16, ts: u32, marker: bool, payload: &[u8]) -> Vec<u8> {
		let mut p = Vec::with_capacity(12 + payload.len());
		p.push(0x80); // V=2, P=0, X=0, CC=0
		p.push(if marker { 0x80 } else { 0x00 }); // M + PT=0
		p.push((seq >> 8) as u8);
		p.push(seq as u8);
		p.extend_from_slice(&ts.to_be_bytes());
		p.extend_from_slice(&[0, 0, 0, 0]); // SSRC
		p.extend_from_slice(payload);
		p
	}

	fn leb128(mut v: usize) -> Vec<u8> {
		let mut out = Vec::new();
		loop {
			let mut b = (v & 0x7f) as u8;
			v >>= 7;
			if v != 0 {
				b |= 0x80;
			}
			out.push(b);
			if v == 0 {
				break;
			}
		}
		out
	}

	#[test]
	fn h264_fu_a_three_fragments() {
		// Original NAL: header (type 5 = IDR, nri=3 → 0x65) + payload bytes.
		let nal_header = 0x65u8;
		let body: Vec<u8> = (0..30u8).collect();
		// Split body across 3 FU-A fragments.
		let f0 = &body[0..10];
		let f1 = &body[10..20];
		let f2 = &body[20..30];
		let nri = nal_header & 0x60; // 0x60
		let orig = nal_header & 0x1f; // 5

		let mut d = Depacketizer::new(Codec::H264);
		// Start fragment.
		let mut p0 = vec![0x60 | 28u8, 0x80 | orig]; // FU indicator (nri|28), FU header S=1
		p0.extend_from_slice(f0);
		assert!(d.push(&rtp(1, 9000, false, &p0)).is_none());
		// Middle.
		let mut p1 = vec![nri | 28u8, orig];
		p1.extend_from_slice(f1);
		assert!(d.push(&rtp(2, 9000, false, &p1)).is_none());
		// End fragment with marker.
		let mut p2 = vec![nri | 28u8, 0x40 | orig]; // E=1
		p2.extend_from_slice(f2);
		let au = d.push(&rtp(3, 9000, true, &p2)).expect("AU on marker");

		// Annex-B: start code + reconstructed NAL header + full body.
		let mut expect = vec![0, 0, 0, 1, nal_header];
		expect.extend_from_slice(&body);
		assert_eq!(au.data, expect);
		assert_eq!(au.pts_90k, 9000);
		assert!(au.key, "IDR (type 5) → key");
	}

	#[test]
	fn h264_single_nal_and_stap_a() {
		let mut d = Depacketizer::new(Codec::H264);
		// STAP-A bundling an SPS (type 7) and PPS (type 8).
		let sps = [0x67u8, 0x42, 0x00, 0x1e];
		let pps = [0x68u8, 0xce, 0x3c, 0x80];
		let mut stap = vec![24u8];
		stap.push((sps.len() >> 8) as u8);
		stap.push(sps.len() as u8);
		stap.extend_from_slice(&sps);
		stap.push((pps.len() >> 8) as u8);
		stap.push(pps.len() as u8);
		stap.extend_from_slice(&pps);
		let au = d.push(&rtp(1, 100, true, &stap)).expect("AU");
		let mut expect = vec![0, 0, 0, 1];
		expect.extend_from_slice(&sps);
		expect.extend_from_slice(&[0, 0, 0, 1]);
		expect.extend_from_slice(&pps);
		assert_eq!(au.data, expect);
		assert!(au.key, "SPS (type 7) → key");
	}

	#[test]
	fn hevc_fu_reassembly() {
		// IDR_W_RADL = 19 (key). 2-byte HEVC header reconstructed from FU header + PayloadHdr.
		let fu_type = 19u8;
		let layer_id = 0u8;
		let tid = 1u8; // nuh_temporal_id_plus1
				 // FU PayloadHdr bytes: byte0 carries nal_type=49 in bits 6..1.
		let ph0 = (49u8 << 1) | ((layer_id >> 5) & 0x01);
		let ph1 = ((layer_id & 0x1f) << 3) | (tid & 0x07);
		let body: Vec<u8> = (50..80u8).collect();

		let mut d = Depacketizer::new(Codec::H265);
		// Start.
		let mut p0 = vec![ph0, ph1, 0x80 | fu_type];
		p0.extend_from_slice(&body[0..15]);
		assert!(d.push(&rtp(10, 7, false, &p0)).is_none());
		// End + marker.
		let mut p1 = vec![ph0, ph1, 0x40 | fu_type];
		p1.extend_from_slice(&body[15..30]);
		let au = d.push(&rtp(11, 7, true, &p1)).expect("AU");

		// Reconstructed 2-byte header.
		let h0 = (fu_type << 1) | ((layer_id >> 5) & 0x01);
		let h1 = ((layer_id & 0x1f) << 3) | (tid & 0x07);
		let mut expect = vec![0, 0, 0, 1, h0, h1];
		expect.extend_from_slice(&body);
		assert_eq!(au.data, expect);
		assert!(au.key, "IDR_W_RADL (19) → key");
	}

	#[test]
	fn av1_w0_and_w2_with_continuation() {
		// Build a TU: a sequence-header OBU (type 1) + a frame OBU (type 6).
		// OBU header: forbidden0 type(4) ext0 has_size1 reserved0.
		let seq_obu_payload = [0xaau8, 0xbb];
		let mut seq_obu = vec![(1u8 << 3) | 0x02]; // type=1, has_size=1
		seq_obu.extend_from_slice(&leb128(seq_obu_payload.len()));
		seq_obu.extend_from_slice(&seq_obu_payload);

		let frame_payload: Vec<u8> = (0..20u8).collect();
		let mut frame_obu = vec![(6u8 << 3) | 0x02]; // type=6, has_size=1
		frame_obu.extend_from_slice(&leb128(frame_payload.len()));
		frame_obu.extend_from_slice(&frame_payload);

		let full_tu: Vec<u8> = seq_obu.iter().chain(frame_obu.iter()).copied().collect();

		// --- W=2 single packet: first element LEB-prefixed, last runs to end. N=1 (new seq). ---
		{
			let mut d = Depacketizer::new(Codec::Av1);
			let agg = (2u8 << 4) | 0x08; // W=2, N=1
			let mut pl = vec![agg];
			pl.extend_from_slice(&leb128(seq_obu.len()));
			pl.extend_from_slice(&seq_obu);
			pl.extend_from_slice(&frame_obu); // last element, no prefix
			let au = d.push(&rtp(1, 42, true, &pl)).expect("AU");
			assert_eq!(au.data, full_tu);
			assert_eq!(au.pts_90k, 42);
			assert!(au.key, "seq header + frame → key");
		}

		// --- W=0 across two packets with Z/Y continuation. ---
		{
			let mut d = Depacketizer::new(Codec::Av1);
			// Packet 1: W=0, all elements LEB-prefixed. Carry the seq OBU whole, then half the
			// frame OBU as a length-prefixed element with Y=1 (continues into next packet).
			let frame_first = &frame_obu[0..5];
			let frame_rest = &frame_obu[5..];
			let agg1 = 0x40 | 0x08; // Y=1, W=0, N=1
			let mut p1 = vec![agg1];
			p1.extend_from_slice(&leb128(seq_obu.len()));
			p1.extend_from_slice(&seq_obu);
			p1.extend_from_slice(&leb128(frame_first.len()));
			p1.extend_from_slice(frame_first);
			assert!(d.push(&rtp(5, 99, false, &p1)).is_none());

			// Packet 2: W=0, Z=1 (first element continues prev OBU). Marker ends the TU.
			let agg2 = 0x80; // Z=1, W=0
			let mut p2 = vec![agg2];
			p2.extend_from_slice(&leb128(frame_rest.len()));
			p2.extend_from_slice(frame_rest);
			let au = d.push(&rtp(6, 99, true, &p2)).expect("AU");
			assert_eq!(au.data, full_tu, "continuation stitched back to whole TU");
			assert!(au.key);
		}
	}

	#[test]
	fn seq_gap_awaits_keyframe() {
		let mut d = Depacketizer::new(Codec::H264);
		// Prime the depacketizer with a keyframe so awaiting_idr is cleared and last_seq is
		// established (rtp-3: new depacketizer starts with awaiting_idr=true).
		let idr0 = [0x65u8, 0xff]; // type=5 (IDR) → key
		assert!(d.push(&rtp(99, 0, true, &idr0)).is_some(), "initial IDR clears awaiting_idr");
		// Now a clean delta AU (type 1) so last_seq advances.
		let delta = [0x41u8, 1, 2, 3]; // nri=2, type=1
		let au = d.push(&rtp(100, 1, true, &delta)).expect("first AU");
		assert!(!au.key);

		// Now SKIP seq 101 → gap. seq 102 is a delta NAL: must be dropped (awaiting IDR).
		let delta2 = [0x41u8, 4, 5, 6];
		assert!(
			d.push(&rtp(102, 2, true, &delta2)).is_none(),
			"delta after a gap is dropped until a keyframe"
		);

		// seq 103: still a delta → still dropped.
		let delta3 = [0x41u8, 7, 8];
		assert!(d.push(&rtp(103, 3, true, &delta3)).is_none());

		// seq 104: an IDR (type 5) → resumes.
		let idr = [0x65u8, 9, 10];
		let au = d.push(&rtp(104, 4, true, &idr)).expect("resumes on key");
		assert!(au.key);
		assert_eq!(au.data, vec![0, 0, 0, 1, 0x65, 9, 10]);
	}

	/// Intra-refresh mode: a gap inside an AU drops that AU (and doesn't wait for a key);
	/// the next whole AU — a plain delta frame — is delivered, and no IDR is requested.
	#[test]
	fn intra_refresh_mode_resumes_on_the_next_whole_au_after_a_gap() {
		let _g = IR_LOCK.lock().unwrap();
		IR_MODE.store(true, std::sync::atomic::Ordering::Relaxed);
		NEED_IDR.store(false, std::sync::atomic::Ordering::Relaxed);
		let mut d = Depacketizer::new(Codec::H264);
		let idr = [0x65u8, 1, 2, 3];
		let delta = [0x41u8, 9, 9, 9];
		assert!(d.push(&rtp(100, 0, true, &idr)).is_some(), "first key decodes");
		assert!(d.push(&rtp(101, 1, true, &delta)).is_some());
		// AU ts=2 is two packets (101+1 lost): seq 103 is its tail → skipped.
		assert!(d.push(&rtp(103, 2, true, &delta)).is_none(), "the damaged AU is dropped");
		// The next AU is whole: delivered even though it's not a keyframe.
		let au = d.push(&rtp(104, 3, true, &delta)).expect("resumes without a keyframe");
		assert!(!au.key);
		assert!(!NEED_IDR.load(std::sync::atomic::Ordering::Relaxed), "no keyframe asked for");
		IR_MODE.store(false, std::sync::atomic::Ordering::Relaxed);
	}

	/// Without intra refresh the old rule holds: a gap waits for the next keyframe.
	#[test]
	fn without_intra_refresh_a_gap_still_waits_for_a_keyframe() {
		let _g = IR_LOCK.lock().unwrap();
		IR_MODE.store(false, std::sync::atomic::Ordering::Relaxed);
		let mut d = Depacketizer::new(Codec::H264);
		let idr = [0x65u8, 1, 2, 3];
		let delta = [0x41u8, 9, 9, 9];
		assert!(d.push(&rtp(100, 0, true, &idr)).is_some());
		assert!(d.push(&rtp(102, 1, true, &delta)).is_none());
		assert!(d.push(&rtp(103, 2, true, &delta)).is_none(), "still waiting for a key");
		assert!(d.push(&rtp(104, 3, true, &idr)).is_some());
	}

	/// RFI: a hole is reported to the host right away (while NACK repair still runs); if the
	/// repair fails the depacketizer drops access units until the host's tagged recovery frame,
	/// which decodes although it is not a keyframe.
	#[test]
	fn rfi_mode_reports_the_hole_and_resumes_at_the_recovery_tag() {
		let _g = IR_LOCK.lock().unwrap();
		RFI_MODE.store(true, std::sync::atomic::Ordering::Relaxed);
		NEED_IDR.store(false, std::sync::atomic::Ordering::Relaxed);
		let _ = take_rfi_request();
		let md = Duration::from_millis(200);
		let t0 = Instant::now();
		let mut r = Reorder::new(64);
		let mut d = Depacketizer::new(Codec::H265);
		let idr = [0x26u8, 0x01, 7, 7]; // IDR_W_RADL
		let p = [0x02u8, 0x01, 9, 9]; // TRAIL_R
		let feed = |r: &mut Reorder, d: &mut Depacketizer, pkt: Vec<u8>, now: Instant| {
			r.push(pkt, now, md).into_iter().filter_map(|x| d.push(&x)).collect::<Vec<_>>()
		};
		assert_eq!(feed(&mut r, &mut d, rtp(10, 0, true, &idr), t0).len(), 1);
		assert_eq!(feed(&mut r, &mut d, rtp(11, 1, true, &p), t0).len(), 1);
		// 12 and 13 are lost; 14 waits behind the hole.
		assert!(feed(&mut r, &mut d, rtp(14, 4, true, &p), t0).is_empty());
		assert_eq!(take_rfi_request(), None, "not before RFI_REPORT_AFTER (reordering)");
		let t1 = t0 + Duration::from_millis(5);
		assert!(feed(&mut r, &mut d, rtp(15, 5, true, &p), t1).is_empty());
		assert_eq!(take_rfi_request(), Some((12, 13)), "reported while repair still runs");
		// The host's recovery frame (tagged) arrives: the wait for the repair ends at once.
		let mut rec = vec![0x4Eu8, 0x01, 0x05, 0x10];
		rec.extend_from_slice(b"PULSAR-RFI-RECOV");
		rec.push(0x80);
		let t2 = t0 + Duration::from_millis(8);
		assert!(feed(&mut r, &mut d, rtp(16, 6, false, &rec), t2).is_empty());
		let out = feed(&mut r, &mut d, rtp(17, 6, true, &p), t2);
		assert_eq!(out.len(), 1, "the recovery frame decodes at once: {out:?}");
		assert!(!out[0].key && out[0].pts_90k == 6);
		assert!(r.poll(t0 + Duration::from_millis(300), md).is_empty(), "nothing left behind");
		assert_eq!(take_rfi_request(), None, "a reported hole is not reported twice");
		assert!(!NEED_IDR.load(std::sync::atomic::Ordering::Relaxed), "no keyframe asked for");
		// And the stream simply continues.
		assert_eq!(feed(&mut r, &mut d, rtp(18, 7, true, &p), t0 + Duration::from_millis(301)).len(), 1);
		RFI_MODE.store(false, std::sync::atomic::Ordering::Relaxed);
	}

	/// RFI: the recovery frame ends the wait for the hole's repair at once — the frames in
	/// between are dropped and the recovery frame decodes, with no burst behind it.
	#[test]
	fn rfi_mode_releases_at_the_recovery_tag_without_waiting_out_the_repair() {
		let _g = IR_LOCK.lock().unwrap();
		RFI_MODE.store(true, std::sync::atomic::Ordering::Relaxed);
		NEED_IDR.store(false, std::sync::atomic::Ordering::Relaxed);
		let _ = take_rfi_request();
		let md = Duration::from_millis(200);
		let t0 = Instant::now();
		let mut r = Reorder::new(64);
		let mut d = Depacketizer::new(Codec::H264);
		let idr = [0x65u8, 1, 2];
		let p = [0x41u8, 9, 9];
		let mut sei = vec![0x06u8, 0x05, 0x10];
		sei.extend_from_slice(b"PULSAR-RFI-RECOV");
		sei.push(0x80);
		let mut got: Vec<u32> = Vec::new();
		let mut feed = |pkt: Vec<u8>, at: u64| -> Vec<u32> {
			r.push(pkt, t0 + Duration::from_millis(at), md)
				.into_iter()
				.filter_map(|x| d.push(&x))
				.map(|au| au.pts_90k)
				.collect()
		};
		got.extend(feed(rtp(1, 0, true, &idr), 0));
		// 2 is lost; frames 3 and 4 wait behind the hole.
		got.extend(feed(rtp(3, 2, true, &p), 1));
		got.extend(feed(rtp(4, 3, true, &p), 5));
		// The recovery frame: its SEI packet, then its slice.
		got.extend(feed(rtp(5, 4, false, &sei), 9));
		got.extend(feed(rtp(6, 4, true, &p), 9));
		assert_eq!(got, vec![0, 4], "the in-between frames are dropped, the recovery decodes at once");
		got.extend(feed(rtp(7, 5, true, &p), 10));
		assert_eq!(got, vec![0, 4, 5]);
		assert!(!NEED_IDR.load(std::sync::atomic::Ordering::Relaxed));
		RFI_MODE.store(false, std::sync::atomic::Ordering::Relaxed);
	}

	/// RFI: a recovery frame a hole ran through is not accepted.
	#[test]
	fn rfi_mode_rejects_a_damaged_recovery_frame() {
		let _g = IR_LOCK.lock().unwrap();
		RFI_MODE.store(true, std::sync::atomic::Ordering::Relaxed);
		let mut d = Depacketizer::new(Codec::H264);
		let idr = [0x65u8, 1, 2];
		let p = [0x41u8, 9, 9];
		let mut sei = vec![0x06u8, 0x05, 0x10];
		sei.extend_from_slice(b"PULSAR-RFI-RECOV");
		sei.push(0x80);
		assert!(d.push(&rtp(1, 0, true, &idr)).is_some());
		assert!(d.push(&rtp(3, 2, true, &p)).is_none(), "hole → waiting");
		// Recovery frame ts=4: tag at 4, slice 5 lost, tail at 6.
		assert!(d.push(&rtp(4, 4, false, &sei)).is_none());
		assert!(d.push(&rtp(6, 4, true, &p)).is_none(), "damaged recovery frame rejected");
		RFI_MODE.store(false, std::sync::atomic::Ordering::Relaxed);
	}

	/// RFI: a repaired hole costs nothing — every frame decodes, no waiting for a recovery.
	#[test]
	fn rfi_mode_a_repaired_hole_decodes_everything() {
		let _g = IR_LOCK.lock().unwrap();
		RFI_MODE.store(true, std::sync::atomic::Ordering::Relaxed);
		let _ = take_rfi_request();
		let md = Duration::from_millis(200);
		let t0 = Instant::now();
		let mut r = Reorder::new(64);
		let mut d = Depacketizer::new(Codec::H264);
		let idr = [0x65u8, 1, 2];
		let p = [0x41u8, 9, 9];
		let mut n = 0;
		for x in r.push(rtp(100, 0, true, &idr), t0, md) { n += d.push(&x).is_some() as u32; }
		for x in r.push(rtp(102, 2, true, &p), t0, md) { n += d.push(&x).is_some() as u32; }
		for x in r.push(rtp(103, 3, true, &p), t0 + Duration::from_millis(4), md) { n += d.push(&x).is_some() as u32; }
		assert_eq!(take_rfi_request(), Some((101, 101)));
		// The NACK retransmit fills it.
		for x in r.push(rtp(101, 1, true, &p), t0 + Duration::from_millis(10), md) { n += d.push(&x).is_some() as u32; }
		assert_eq!(n, 4, "all four frames decode");
		RFI_MODE.store(false, std::sync::atomic::Ordering::Relaxed);
	}

	/// RFI: no recovery frame within RFI_TIMEOUT → the keyframe request after all.
	#[test]
	fn rfi_mode_falls_back_to_a_keyframe() {
		let _g = IR_LOCK.lock().unwrap();
		RFI_MODE.store(true, std::sync::atomic::Ordering::Relaxed);
		NEED_IDR.store(false, std::sync::atomic::Ordering::Relaxed);
		let mut d = Depacketizer::new(Codec::H264);
		let idr = [0x65u8, 1, 2];
		let p = [0x41u8, 9, 9];
		assert!(d.push(&rtp(1, 0, true, &idr)).is_some());
		assert!(d.push(&rtp(3, 2, true, &p)).is_none(), "gap → waiting for the recovery");
		d.awaiting_recovery = Some((Instant::now(), Instant::now() - RFI_TIMEOUT));
		assert!(d.push(&rtp(4, 3, true, &p)).is_none());
		assert!(NEED_IDR.load(std::sync::atomic::Ordering::Relaxed), "keyframe requested");
		assert!(d.push(&rtp(5, 4, true, &idr)).is_some(), "and the keyframe resumes");
		RFI_MODE.store(false, std::sync::atomic::Ordering::Relaxed);
		NEED_IDR.store(false, std::sync::atomic::Ordering::Relaxed);
	}

	fn rfi_tag_h264() -> Vec<u8> {
		let mut sei = vec![0x06u8, 0x05, 0x10];
		sei.extend_from_slice(b"PULSAR-RFI-RECOV");
		sei.push(0x80);
		sei
	}

	/// RFI at high loss: every packet between the hole and the recovery frame was lost, so the
	/// first packet after the hole IS the tag. The recovery frame is still whole (the tag is
	/// its first packet) and must end the wait — it used to be rejected as "hit by the hole".
	#[test]
	fn rfi_mode_a_hole_ending_at_the_tag_still_recovers() {
		let _g = IR_LOCK.lock().unwrap();
		RFI_MODE.store(true, std::sync::atomic::Ordering::Relaxed);
		NEED_IDR.store(false, std::sync::atomic::Ordering::Relaxed);
		let mut d = Depacketizer::new(Codec::H264);
		let idr = [0x65u8, 1, 2];
		let p = [0x41u8, 9, 9];
		assert!(d.push(&rtp(1, 0, true, &idr)).is_some());
		// 2..=4 lost; 5 is the recovery frame's tag, 6 its slice.
		assert!(d.push(&rtp(5, 4, false, &rfi_tag_h264())).is_none());
		let au = d.push(&rtp(6, 4, true, &p)).expect("the whole recovery frame decodes");
		assert_eq!(au.pts_90k, 4);
		assert!(!d.waiting());
		assert!(!NEED_IDR.load(std::sync::atomic::Ordering::Relaxed));
		RFI_MODE.store(false, std::sync::atomic::Ordering::Relaxed);
	}

	/// RFI: the recovery frame itself lost a packet that NACK repaired late. The tag releases
	/// the hole in front of it at once; the recovery frame's own hole keeps waiting and, once
	/// repaired, the recovery frame decodes whole.
	#[test]
	fn rfi_mode_recovery_frame_with_a_late_repaired_loss_decodes() {
		let _g = IR_LOCK.lock().unwrap();
		RFI_MODE.store(true, std::sync::atomic::Ordering::Relaxed);
		NEED_IDR.store(false, std::sync::atomic::Ordering::Relaxed);
		let _ = take_rfi_request();
		let md = Duration::from_millis(200);
		let t0 = Instant::now();
		let mut r = Reorder::new(64);
		let mut d = Depacketizer::new(Codec::H264);
		let idr = [0x65u8, 1, 2];
		let p = [0x41u8, 9, 9];
		let mut got: Vec<u32> = Vec::new();
		let mut feed = |pkt: Vec<u8>, at: u64| -> Vec<u32> {
			r.push(pkt, t0 + Duration::from_millis(at), md)
				.into_iter()
				.filter_map(|x| d.push(&x))
				.map(|au| au.pts_90k)
				.collect()
		};
		got.extend(feed(rtp(1, 0, true, &idr), 0));
		// 2 lost (frame ts 1); frame ts 2 waits behind it.
		got.extend(feed(rtp(3, 2, true, &p), 1));
		// Recovery frame ts 3: tag 4, slice 5 LOST, slice 6 (marker).
		got.extend(feed(rtp(4, 3, false, &rfi_tag_h264()), 40));
		got.extend(feed(rtp(6, 3, true, &p), 41));
		assert_eq!(got, vec![0], "the recovery frame waits for its own repair");
		// The NACK retransmit of 5 lands 90 ms later (inside the repair window).
		got.extend(feed(rtp(5, 3, false, &p), 130));
		assert_eq!(got, vec![0, 3], "repaired recovery frame decodes");
		got.extend(feed(rtp(7, 4, true, &p), 131));
		assert_eq!(got, vec![0, 3, 4]);
		assert!(!NEED_IDR.load(std::sync::atomic::Ordering::Relaxed));
		RFI_MODE.store(false, std::sync::atomic::Ordering::Relaxed);
	}

	/// RFI: a further hole while waiting for the recovery frame restarts the timeout (that hole
	/// was reported too) — bounded by RFI_GIVE_UP from the first hole.
	#[test]
	fn rfi_mode_a_new_hole_restarts_the_recovery_timeout_up_to_the_give_up() {
		let _g = IR_LOCK.lock().unwrap();
		RFI_MODE.store(true, std::sync::atomic::Ordering::Relaxed);
		NEED_IDR.store(false, std::sync::atomic::Ordering::Relaxed);
		let mut d = Depacketizer::new(Codec::H264);
		let idr = [0x65u8, 1, 2];
		let p = [0x41u8, 9, 9];
		assert!(d.push(&rtp(1, 0, true, &idr)).is_some());
		assert!(d.push(&rtp(3, 2, true, &p)).is_none(), "hole → waiting");
		assert!(d.waiting(), "a mid-stream recovery wait shows as waiting");
		// The first hole is older than the timeout, but a new hole just happened.
		d.awaiting_recovery = Some((Instant::now() - RFI_TIMEOUT, Instant::now() - RFI_TIMEOUT));
		assert!(d.push(&rtp(5, 4, true, &p)).is_none());
		let (_, last) = d.awaiting_recovery.expect("still waiting for the recovery frame");
		assert!(last.elapsed() < RFI_TIMEOUT, "the new hole restarted the timeout");
		assert!(!NEED_IDR.load(std::sync::atomic::Ordering::Relaxed));
		// The overall bound still asks for a keyframe.
		d.awaiting_recovery = Some((Instant::now() - RFI_GIVE_UP, Instant::now()));
		assert!(d.push(&rtp(6, 5, true, &p)).is_none());
		assert!(d.awaiting_recovery.is_none() && d.awaiting_idr);
		assert!(NEED_IDR.load(std::sync::atomic::Ordering::Relaxed), "keyframe requested");
		RFI_MODE.store(false, std::sync::atomic::Ordering::Relaxed);
		NEED_IDR.store(false, std::sync::atomic::Ordering::Relaxed);
	}

	/// RFI: after the recovery wait timed out into a keyframe wait, a whole recovery frame still
	/// resumes (at high loss it arrives long before a whole keyframe); a damaged one doesn't.
	#[test]
	fn rfi_mode_a_recovery_frame_ends_the_fallback_keyframe_wait() {
		let _g = IR_LOCK.lock().unwrap();
		RFI_MODE.store(true, std::sync::atomic::Ordering::Relaxed);
		NEED_IDR.store(false, std::sync::atomic::Ordering::Relaxed);
		let mut d = Depacketizer::new(Codec::H264);
		let idr = [0x65u8, 1, 2];
		let p = [0x41u8, 9, 9];
		assert!(d.push(&rtp(1, 0, true, &idr)).is_some());
		assert!(d.push(&rtp(3, 2, true, &p)).is_none());
		d.awaiting_recovery = Some((Instant::now() - RFI_TIMEOUT, Instant::now() - RFI_TIMEOUT));
		assert!(d.push(&rtp(4, 3, true, &p)).is_none());
		assert!(d.awaiting_idr && d.waiting(), "timed out into a keyframe wait");
		// A damaged recovery frame (its slice 6 lost) does not end it…
		assert!(d.push(&rtp(5, 4, false, &rfi_tag_h264())).is_none());
		assert!(d.push(&rtp(7, 4, true, &p)).is_none());
		// …a whole one does.
		assert!(d.push(&rtp(8, 5, false, &rfi_tag_h264())).is_none());
		let au = d.push(&rtp(9, 5, true, &p)).expect("whole recovery frame resumes");
		assert_eq!(au.pts_90k, 5);
		assert!(!d.waiting());
		RFI_MODE.store(false, std::sync::atomic::Ordering::Relaxed);
		NEED_IDR.store(false, std::sync::atomic::Ordering::Relaxed);
	}

	/// The keyframe fallback only ends at a recovery frame when it came from a recovery wait:
	/// at the session start a tagged frame is not an entry point.
	#[test]
	fn a_recovery_tag_is_no_entry_point_at_the_session_start() {
		let _g = IR_LOCK.lock().unwrap();
		RFI_MODE.store(true, std::sync::atomic::Ordering::Relaxed);
		let mut d = Depacketizer::new(Codec::H264);
		let p = [0x41u8, 9, 9];
		assert!(d.push(&rtp(1, 0, false, &rfi_tag_h264())).is_none());
		assert!(d.push(&rtp(2, 0, true, &p)).is_none(), "session start waits for the keyframe");
		assert!(!d.waiting(), "nothing delivered yet: the session start is not a freeze");
		RFI_MODE.store(false, std::sync::atomic::Ordering::Relaxed);
	}

	/// A plain keyframe wait after a gap counts as waiting (the renderer shows "recovering").
	#[test]
	fn a_mid_stream_keyframe_wait_is_waiting() {
		let _g = IR_LOCK.lock().unwrap();
		RFI_MODE.store(false, std::sync::atomic::Ordering::Relaxed);
		IR_MODE.store(false, std::sync::atomic::Ordering::Relaxed);
		let mut d = Depacketizer::new(Codec::H264);
		assert!(!d.waiting());
		assert!(d.push(&rtp(1, 0, true, &[0x65u8, 1])).is_some());
		assert!(!d.waiting());
		assert!(d.push(&rtp(3, 2, true, &[0x41u8, 1])).is_none());
		assert!(d.waiting());
		assert!(d.push(&rtp(4, 3, true, &[0x65u8, 1])).is_some());
		assert!(!d.waiting());
		NEED_IDR.store(false, std::sync::atomic::Ordering::Relaxed);
	}

	/// IR_MODE is process-global: the two tests above must not run interleaved.
	static IR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

	#[test]
	fn encoder_restart_behind_old_base_resyncs() {
		// A restarted ffmpeg lands on a fresh random RTP base that can be BEHIND the old one.
		// That must resync on the new stream's keyframe — not be dropped as "stale" until the
		// 16-bit seq wraps past the old cursor (the frozen-video bug).
		let mut d = Depacketizer::new(Codec::H264);
		let idr = [0x65u8, 0xaa];
		assert!(d.push(&rtp(40_000, 0, true, &idr)).is_some(), "initial IDR");
		let delta = [0x41u8, 1];
		assert!(d.push(&rtp(40_001, 1, true, &delta)).is_some());
		// New stream, base 1_000 (far behind 40_001): its first delta is not decodable…
		assert!(d.push(&rtp(1_000, 0, true, &delta)).is_none(), "delta before the new key");
		// …but its keyframe and what follows flow again.
		let au = d.push(&rtp(1_001, 0, true, &idr)).expect("resyncs on the new stream's key");
		assert!(au.key);
		assert!(d.push(&rtp(1_002, 1, true, &delta)).is_some(), "new stream keeps flowing");
		// A genuinely late retransmit from just behind the cursor is still dropped.
		assert!(d.push(&rtp(990, 0, true, &delta)).is_none());
	}

	#[test]
	fn reorder_resyncs_on_encoder_restart() {
		let now = Instant::now();
		let md = Duration::from_millis(100);
		let mut r = Reorder::new(REORDER_CAP);
		assert_eq!(r.push(rtp(40_000, 0, true, &[1]), now, md).len(), 1);
		// Hole at 40_001; 40_002 is held.
		assert!(r.push(rtp(40_002, 0, true, &[1]), now, md).is_empty());
		assert_eq!(r.pending(), 1);
		// Restart far behind: released immediately, the dead stream's hold is discarded, and
		// the new stream then passes in order.
		assert_eq!(r.push(rtp(1_000, 0, true, &[1]), now, md).len(), 1);
		assert_eq!(r.pending(), 0);
		assert_eq!(r.push(rtp(1_001, 0, true, &[1]), now, md).len(), 1);
	}

	#[test]
	fn dup_and_stale_retransmit_dropped() {
		// Mirrors mobile/src/rtp.rs::stale_retransmit_dropped_but_restart_accepted: a duplicate
		// or a late (backward) NACK retransmit must be dropped without emitting an AU and without
		// disturbing the assembly of the next frame.
		let mut d = Depacketizer::new(Codec::H264);
		// Prime with an IDR (clears awaiting_idr, establishes last_seq=100).
		let idr = [0x65u8, 0xaa];
		assert!(d.push(&rtp(100, 0, true, &idr)).is_some(), "initial IDR");
		// A clean delta AU advances last_seq to 101.
		let delta = [0x41u8, 1, 2, 3];
		let au = d.push(&rtp(101, 1, true, &delta)).expect("delta AU");
		assert!(!au.key);

		// DUPLICATE of seq 101 (fwd==0): dropped, no AU, state untouched.
		assert!(
			d.push(&rtp(101, 1, true, &delta)).is_none(),
			"duplicate packet dropped, not re-emitted"
		);
		// STALE/backward NACK retransmit (seq 99 < 101 → fwd>=0x8000): dropped, not spliced, and
		// must NOT trigger a gap (awaiting_idr stays clear).
		let stale = [0x41u8, 9, 9];
		assert!(
			d.push(&rtp(99, 0, true, &stale)).is_none(),
			"stale retransmit dropped"
		);

		// The next genuine forward delta (seq 102) is unaffected — last_seq is still 101, so 102
		// is consecutive (no gap) and emits a clean AU.
		let delta2 = [0x41u8, 4, 5, 6];
		let au = d.push(&rtp(102, 2, true, &delta2)).expect("next AU unaffected");
		assert!(!au.key);
		assert_eq!(au.data, vec![0, 0, 0, 1, 0x41, 4, 5, 6]);
	}

	#[test]
	fn stale_packet_not_spliced_into_open_au() {
		let mut d = Depacketizer::new(Codec::H264);
		// Prime with an IDR so we are not awaiting a keyframe (last_seq=50).
		assert!(d.push(&rtp(50, 0, true, &[0x65u8, 0x00])).is_some());
		// Begin a 2-NAL delta AU (first packet has no marker), seq 51.
		let n1 = [0x41u8, 1, 1];
		assert!(
			d.push(&rtp(51, 5, false, &n1)).is_none(),
			"first NAL — AU still open"
		);
		// A STALE retransmit (seq 40, backward) arrives mid-assembly: it must be DROPPED, not
		// appended to the open AU.
		let stale = [0x41u8, 7, 7, 7];
		assert!(
			d.push(&rtp(40, 0, false, &stale)).is_none(),
			"stale packet dropped mid-AU"
		);
		// Second NAL + marker closes the AU (seq 52 — consecutive after 51).
		let n2 = [0x41u8, 2, 2];
		let au = d.push(&rtp(52, 5, true, &n2)).expect("AU closes on marker");
		// The AU contains ONLY n1 and n2 — the stale packet's bytes are absent.
		let expect = vec![0, 0, 0, 1, 0x41, 1, 1, 0, 0, 0, 1, 0x41, 2, 2];
		assert_eq!(au.data, expect, "stale bytes not spliced into the AU");
	}
}

// ===========================================================================================
#[cfg(test)]
mod reorder_tests {
	use super::*;

	fn pkt(seq: u16) -> Vec<u8> {
		let mut p = vec![0x80, 0x60, (seq >> 8) as u8, seq as u8, 0, 0, 0, 0, 0, 0, 0, 1];
		p.push(seq as u8); // payload marker so we can tell packets apart
		p
	}
	fn seqs(v: &[Vec<u8>]) -> Vec<u16> {
		v.iter().map(|p| ((p[2] as u16) << 8) | p[3] as u16).collect()
	}
	const MD: Duration = Duration::from_millis(100);

	#[test]
	fn in_order_is_passthrough_with_no_buffering() {
		let mut r = Reorder::new(REORDER_CAP);
		let t = Instant::now();
		for s in 0u16..50 {
			let out = r.push(pkt(s), t, MD);
			assert_eq!(seqs(&out), vec![s]);
			assert_eq!(r.pending(), 0);
		}
	}

	#[test]
	fn gap_waits_for_the_retransmit_then_releases_in_order() {
		let mut r = Reorder::new(REORDER_CAP);
		let t = Instant::now();
		assert_eq!(seqs(&r.push(pkt(10), t, MD)), vec![10]);
		// 11 is lost; 12..15 arrive and are held.
		for s in 12u16..=15 {
			assert!(r.push(pkt(s), t + Duration::from_millis(5), MD).is_empty());
		}
		assert_eq!(r.pending(), 4);
		// Nothing released while the wait is still running.
		assert!(r.poll(t + Duration::from_millis(60), MD).is_empty());
		// The NACK retransmit lands at +80 ms: the whole run comes out in sequence.
		let out = r.push(pkt(11), t + Duration::from_millis(80), MD);
		assert_eq!(seqs(&out), vec![11, 12, 13, 14, 15]);
		assert_eq!(r.pending(), 0);
		assert_eq!(seqs(&r.push(pkt(16), t + Duration::from_millis(81), MD)), vec![16]);
	}

	#[test]
	fn gap_times_out_then_skips_and_the_late_packet_passes_as_stale() {
		let mut r = Reorder::new(REORDER_CAP);
		let t = Instant::now();
		r.push(pkt(0), t, MD);
		assert!(r.push(pkt(2), t, MD).is_empty());
		assert!(r.push(pkt(3), t + Duration::from_millis(10), MD).is_empty());
		// max_delay elapsed for the head (2): skip the hole, release 2,3.
		let out = r.poll(t + MD, MD);
		assert_eq!(seqs(&out), vec![2, 3]);
		// The retransmit of 1 arrives too late: passed through (the depacketizer drops it).
		assert_eq!(seqs(&r.push(pkt(1), t + Duration::from_millis(150), MD)), vec![1]);
		// And the stream continues in order behind the skip.
		assert_eq!(seqs(&r.push(pkt(4), t + Duration::from_millis(151), MD)), vec![4]);
	}

	#[test]
	fn a_second_hole_behind_the_first_waits_its_own_turn() {
		let mut r = Reorder::new(REORDER_CAP);
		let t = Instant::now();
		r.push(pkt(0), t, MD);
		assert!(r.push(pkt(2), t, MD).is_empty()); // hole at 1
		assert!(r.push(pkt(4), t + Duration::from_millis(50), MD).is_empty()); // hole at 3
		// Filling 1 releases 2 only; 4 still waits for 3.
		assert_eq!(seqs(&r.push(pkt(1), t + Duration::from_millis(60), MD)), vec![1, 2]);
		assert_eq!(r.pending(), 1);
		// 4 arrived at +50 ms → released past its hole at +150 ms, not before.
		assert!(r.poll(t + Duration::from_millis(140), MD).is_empty());
		assert_eq!(seqs(&r.poll(t + Duration::from_millis(150), MD)), vec![4]);
	}

	#[test]
	fn buffer_cap_flushes_past_the_hole() {
		let mut r = Reorder::new(8);
		let t = Instant::now();
		r.push(pkt(0), t, MD);
		let mut out = Vec::new();
		for s in 2u16..=9 {
			out.extend(r.push(pkt(s), t, MD));
		}
		assert_eq!(seqs(&out), (2u16..=9).collect::<Vec<_>>(), "8 held packets → flushed");
		assert_eq!(r.pending(), 0);
	}

	#[test]
	fn sequence_wraparound_is_ordered_correctly() {
		let mut r = Reorder::new(REORDER_CAP);
		let t = Instant::now();
		r.push(pkt(65533), t, MD);
		r.push(pkt(65534), t, MD);
		// 65535 lost; 0, 1 arrive (across the wrap) and wait; the retransmit releases all.
		assert!(r.push(pkt(0), t, MD).is_empty());
		assert!(r.push(pkt(1), t, MD).is_empty());
		assert_eq!(seqs(&r.push(pkt(65535), t, MD)), vec![65535, 0, 1]);
	}

	/// A packet with the RTP marker bit (end of a frame).
	fn pktm(seq: u16) -> Vec<u8> {
		let mut p = pkt(seq);
		p[1] |= 0x80;
		p
	}

	/// A Reorder that has measured `loss_pct` pre-repair loss and knows the RTT.
	fn lossy(loss_pct: u32, rtt_ms: u64) -> Reorder {
		let mut r = Reorder::new(REORDER_CAP);
		r.loss = loss_pct as f32 / 100.0;
		r.rtt_pin = Some(Duration::from_millis(rtt_ms));
		r
	}

	#[test]
	fn the_app_reorder_wait_is_capped() {
		if std::env::var_os("PULSAR_MAXDELAY").is_some() {
			return; // the debug pin overrides the app value (and is not capped)
		}
		set_max_delay_us(600_000);
		assert_eq!(max_delay(), Duration::from_micros(MAX_DELAY_CAP_US));
		set_max_delay_us(120_000);
		assert_eq!(max_delay(), Duration::from_millis(120));
		set_max_delay_us(0);
		assert_eq!(max_delay(), Duration::from_micros(DEFAULT_MAX_DELAY_US));
	}

	#[test]
	fn pre_repair_loss_counts_first_arrival_gaps_only() {
		let mut r = Reorder::new(REORDER_CAP);
		let t = Instant::now();
		// 256 sequence numbers, every 5th missing on first arrival (20 %); the late repairs
		// (backward arrivals) don't count against it.
		let mut s = 0u16;
		r.push(pkt(s), t, MD);
		while s < 256 {
			s += 1;
			if s % 5 == 0 {
				continue;
			}
			r.push(pkt(s), t, MD);
			if s % 5 == 1 && s > 1 {
				r.push(pkt(s - 1), t, MD); // the retransmit of the missing one
			}
		}
		assert!((r.loss() - 0.2).abs() < 0.02, "loss {}", r.loss());
	}

	#[test]
	fn a_later_complete_frame_is_detected() {
		let mut r = Reorder::new(REORDER_CAP);
		let t = Instant::now();
		r.push(pkt(0), t, MD);
		// hole at 1; 2 (end of the damaged frame, marker), then 3,4 (a whole frame).
		r.push(pktm(2), t, MD);
		r.push(pkt(3), t, MD);
		assert!(!r.later_frame_complete(), "the later frame is not complete yet");
		r.push(pktm(4), t, MD);
		assert!(r.later_frame_complete());
		// A hole inside the later frame: not complete.
		let mut r = Reorder::new(REORDER_CAP);
		r.push(pkt(0), t, MD);
		r.push(pktm(2), t, MD);
		r.push(pktm(4), t, MD); // 3 missing
		assert!(!r.later_frame_complete());
	}

	/// High loss + a whole frame stuck behind the hole: give up at max(2×RTT, 60 ms) instead of
	/// the full repair window.
	#[test]
	fn an_unrepairable_hole_is_given_up_early_under_high_loss() {
		let md = Duration::from_millis(250);
		let mut r = lossy(30, 40);
		let t = Instant::now();
		r.push(pkt(0), t, md);
		assert!(r.push(pktm(2), t, md).is_empty());
		assert!(r.push(pktm(3), t + Duration::from_millis(8), md).is_empty());
		// 2×RTT = 80 ms: still waiting at 79 ms…
		assert!(r.poll(t + Duration::from_millis(79), md).is_empty());
		// …released at 80 ms, far before the 250 ms window.
		assert_eq!(seqs(&r.poll(t + Duration::from_millis(80), md)), vec![2, 3]);
	}

	#[test]
	fn stats_count_given_up_holes() {
		let md = Duration::from_millis(250);
		let mut r = lossy(30, 40);
		let t = Instant::now();
		r.push(pkt(0), t, md);
		r.push(pktm(3), t, md); // hole 1..=2
		r.push(pktm(4), t, md);
		r.poll(t + Duration::from_millis(80), md); // early
		r.push(pkt(6), t + Duration::from_millis(100), md); // hole 5
		r.poll(t + Duration::from_millis(350), md); // full window
		let st = r.take_stats();
		assert_eq!(st, ReorderStats { holes_skipped: 2, seqs_skipped: 3, early_skips: 1, max_pending: 2 });
		assert_eq!(r.take_stats(), ReorderStats::default(), "reset after take");
	}

	#[test]
	fn the_early_give_up_has_a_floor() {
		let md = Duration::from_millis(250);
		let mut r = lossy(30, 10);
		let t = Instant::now();
		r.push(pkt(0), t, md);
		r.push(pktm(2), t, md);
		r.push(pktm(3), t, md);
		assert!(r.poll(t + Duration::from_millis(59), md).is_empty(), "60 ms floor (2×RTT = 20)");
		assert_eq!(seqs(&r.poll(t + Duration::from_millis(60), md)), vec![2, 3]);
	}

	#[test]
	fn no_early_give_up_at_low_loss_or_without_a_complete_frame() {
		let md = Duration::from_millis(250);
		let t = Instant::now();
		let at = t + Duration::from_millis(200);
		// Low loss: the full window.
		let mut r = lossy(5, 40);
		r.push(pkt(0), t, md);
		r.push(pktm(2), t, md);
		r.push(pktm(3), t, md);
		assert!(r.poll(at, md).is_empty());
		// High loss, but only the damaged frame's tail behind the hole.
		let mut r = lossy(30, 40);
		r.push(pkt(0), t, md);
		r.push(pkt(2), t, md);
		r.push(pktm(3), t, md);
		assert!(r.poll(at, md).is_empty());
		// The full window still applies.
		assert_eq!(seqs(&r.poll(t + md, md)), vec![2, 3]);
	}

	#[test]
	fn duplicates_of_held_packets_are_absorbed_and_stale_ones_pass_through() {
		let mut r = Reorder::new(REORDER_CAP);
		let t = Instant::now();
		r.push(pkt(5), t, MD);
		assert!(r.push(pkt(7), t, MD).is_empty());
		assert!(r.push(pkt(7), t, MD).is_empty(), "duplicate of a held packet");
		assert_eq!(r.pending(), 1);
		assert_eq!(seqs(&r.push(pkt(3), t, MD)), vec![3], "stale packet passes through");
		assert_eq!(seqs(&r.push(pkt(6), t, MD)), vec![6, 7]);
	}
}
