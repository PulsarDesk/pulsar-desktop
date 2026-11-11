//! The client's full-duplex hold-loop, extracted verbatim from `start_remote_play`.
//!
//! `hold_session` owns the control `Session` for the lifetime of a remote-play tab:
//! it forwards local input + side-channel data to the host, pings every 500 ms (keepalive —
//! UDP has no disconnect signal — and the RTT the adaptive controller runs on), receives
//! the host's chat/clipboard/stat pushes (surfacing them to the UI), watchdogs host
//! silence, runs the shared adaptive controller (`pulsar_core::adapt`) on 2 s windows, and re-requests
//! the stream when the session menu or the controller changes a stream parameter. On exit
//! it tells the frontend (`play-ended`) and sends a clean `Bye`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use pulsar_core::service::{
	decode_data, media, request_stream, send_data, send_input, send_keepalive, ClientStats,
	DataMsg, InputEvent, LossRecovery, QualityPref, StreamReq,
};
use pulsar_core::Session;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc::Receiver;

use pulsar_core::adapt::{Controller, Sample};
use crate::events::{
	AvatarPayload, DataPayload, FilePayload, FsEntriesPayload, PlayRtt, PlayStats,
};
use crate::state::Restream;

/// Keepalive/RTT ping cadence. 500 ms (was 2 s): the adaptive controller (`pulsar_core::adapt`)
/// decides on 2 s windows and needs a few RTT samples per window to see a queue building
/// (Phase 0.1) and to size the NACK repair window (Phase 0.5). Each Ping is a ~10-byte
/// datagram; the host answers every one with a Pong and refreshes its liveness timer.
const PING_MS: u64 = 500;
/// A Ping still unanswered this long is written off as lost. Pings go out every `PING_MS`
/// whether or not the last one was answered (see `match_pong`): on a 40 %-loss link a lost
/// Ping or Pong used to stall the RTT samples for a second.
const PING_LOST: std::time::Duration = std::time::Duration::from_secs(2);
/// How many recent RTT samples size the repair window (~4 s of Pings).
const RTT_RECENT: usize = 8;
/// How often unanswered NACKs are swept (re-NACKed once, then written off as a failed
/// repair → keyframe request + renderer hold). 50 ms (was 100): the renderer waits one sweep
/// longer than this loop (see `maxdelay` below), so the sweep period is extra head-of-line
/// wait behind every unrepaired hole; a 20 Hz pass over a few dozen entries costs nothing.
const REPAIR_SWEEP_MS: u64 = 50;
/// Bounds of the RTT-derived repair window (ms): how long a missing packet is waited for
/// before the repair is declared failed. The renderer's RTP reorder `max_delay` is this plus
/// one sweep, so both give up together, and a hole holds the frames behind it for 250 ms at
/// most (the renderer's own cap) — a repair slower than that loses to the RFI / keyframe
/// recovery anyway. See `repair_window_ms`.
const REPAIR_MIN_MS: f32 = 60.0;
const REPAIR_MAX_MS: f32 = 250.0 - REPAIR_SWEEP_MS as f32;
/// Allowance on top of the RTT for the host's side of a retransmit (its pacer queue + send).
const REPAIR_SLACK_MS: f32 = 20.0;
/// How long the renderer holds the last good frame after an unrepaired loss while the
/// keyframe is on its way (Phase 0.5). Bounded (a keyframe ends it sooner), and the renderer
/// arms it once per loss episode, so a run of losses can't chain holds into a long freeze.
const HOLD_MS: u32 = 300;

/// The renderer hold after an unrepaired loss in this recovery mode (ms), None = none.
/// Intra refresh: none. No keyframe is coming to end it early, and the decoder conceals the loss
/// while the rolling wave heals the picture within its period — a hold only froze a third of
/// that. Field log 2026-11-09 (Orange Pi client, 9.6 Mbit/s policer + 1 % loss, IR stream): a
/// `hold` on every sweep with a written-off packet, 1-1.7 s freezes, "renderer fps very low"
/// 11-23 times in 80 s.
fn loss_hold_ms(recovery: LossRecovery) -> Option<u32> {
	(recovery != LossRecovery::IntraRefresh).then_some(HOLD_MS)
}

/// Keyframe-request pacing. While losses keep coming, every request within 5 s of the previous
/// one doubles the gap (0.5 s → 1 → 2 → 4 s); 5 s without one resets it. And never sooner than
/// ~2 RTT: the keyframe asked for can't have arrived yet. Without this a lossy link fell into a
/// keyframe storm — 1080p keyframes every ~0.4 s were most of the traffic (2 Mbit/s sent for a
/// 0.3 Mbit/s target), which caused the next losses, which asked for more keyframes.
const KF_GAP_MIN: std::time::Duration = std::time::Duration::from_millis(500);
const KF_GAP_MAX: std::time::Duration = std::time::Duration::from_secs(4);

fn kf_allowed(since_last: std::time::Duration, gap: std::time::Duration, rtt_ms: f32) -> bool {
	since_last >= gap.max(std::time::Duration::from_secs_f32((2.0 * rtt_ms.max(0.0) + 100.0) / 1000.0))
}

fn kf_next_gap(since_last: std::time::Duration, gap: std::time::Duration) -> std::time::Duration {
	if since_last < std::time::Duration::from_secs(5) {
		(gap * 2).min(KF_GAP_MAX)
	} else {
		KF_GAP_MIN
	}
}

/// Reference-frame invalidation is the recovery: the host does it and the codec has the
/// recovery tag (H.264 / HEVC). Whether a reported hole still lies within the encoder's
/// reference buffer is the HOST's call — it knows its DPB depth and sends a keyframe itself
/// when the lost frame is out of reach or the invalidation fails. (The client used to gate on
/// RTT × fps ≤ 4 frames, which a 120 fps stream over any WAN never met: every loss became a
/// keyframe request.)
fn rfi_reach(host_rfi: bool, codec: &str) -> bool {
	host_rfi && codec != "av1"
}

/// Median of the recent RTT samples (`fallback` while there are none).
fn median_ms(v: &std::collections::VecDeque<f32>, fallback: f32) -> f32 {
	if v.is_empty() {
		return fallback;
	}
	let mut s: Vec<f32> = v.iter().copied().collect();
	s.sort_by(f32::total_cmp);
	s[s.len() / 2]
}

/// The repair window in ms (see `REPAIR_MIN_MS`): a NACK retransmit lands ~1 RTT after the
/// NACK, so wait the median RTT plus twice its jitter (the mean absolute deviation of the
/// recent samples) — an answer that is late but normal for this link still makes it, a lost one
/// is not waited out for long. `fec_hold_ms` = how long NACKs are held back for FEC now (see
/// `fec_wait_ms`). The old `1.5 × RTT + 100` (+ a 100 ms sweep) reached ~600 ms on a jittery
/// mobile link, every frame behind a hole held that long. No samples yet: the upper bound.
fn repair_window_ms(rtt_recent: &std::collections::VecDeque<f32>, fec_hold_ms: f32) -> f32 {
	if rtt_recent.is_empty() {
		return REPAIR_MAX_MS;
	}
	let med = median_ms(rtt_recent, 0.0);
	let jitter = rtt_recent.iter().map(|r| (r - med).abs()).sum::<f32>() / rtt_recent.len() as f32;
	(med + 2.0 * jitter + REPAIR_SLACK_MS + fec_hold_ms.max(0.0)).clamp(REPAIR_MIN_MS, REPAIR_MAX_MS)
}

/// Which unanswered Ping a Pong answers (`ages` = time since each was sent, oldest first;
/// `expected_ms` = the latest RTT sample, 0 = none yet). The Pong carries no id: take the Ping
/// whose age is nearest the latest RTT. A lost Ping (or Pong) leaves an old Ping ~`PING_MS`
/// too old, so the next Pong is timed against the newer one; an RTT that grows past `PING_MS`
/// (a filling queue) keeps matching the older one. No sample yet: the oldest. Ties go to the
/// older Ping.
fn match_pong(ages: &[std::time::Duration], expected_ms: f32) -> Option<usize> {
	if ages.is_empty() {
		return None;
	}
	if expected_ms <= 0.0 {
		return Some(0);
	}
	let off = |a: &std::time::Duration| (a.as_secs_f32() * 1000.0 - expected_ms).abs();
	let mut best = 0;
	for (i, a) in ages.iter().enumerate().skip(1) {
		if off(a) < off(&ages[best]) {
			best = i;
		}
	}
	Some(best)
}

/// Pongs a `PongMatcher` vote looks back over.
const PONG_VOTES: usize = 4;
/// A vote needs this many of the last `PONG_VOTES` Pongs behind it; with less evidence the
/// matcher falls back to `match_pong`.
const PONG_VOTE_MIN: i32 = 2;
/// RTT drift (ms per ms) a vote allows for since its Pong: 0.1 = 100 ms a second, more than a
/// filling queue shows, far less than a Ping gap (350+ ms) — wrong candidates still miss.
const PONG_DRIFT: f32 = 0.1;

/// The gap before the Ping after the `n`-th (ms): 350–650 ms, mean `PING_MS`, never periodic
/// (a golden-ratio sequence). With a fixed 500 ms cadence a Pong cannot tell its Ping from the
/// next one: a sustained 600 ms RTT and a 100 ms RTT with one lost Ping give the very same
/// arrival times, and the matcher locked onto RTT − 500 for as long as the RTT stayed over
/// 500 ms (the repair window shrank under the real RTT, the ABR read no queue). Uneven gaps make
/// only the true RTT repeat from Pong to Pong.
fn ping_gap_ms(n: u64) -> u64 {
	let frac = (n as f64 * 0.618_033_988_75).fract();
	PING_MS - 150 + (300.0 * frac) as u64
}

/// Times the host's Pongs against our Pings. A Pong carries no id (the wire format stays as
/// released apps speak it), so each Pong could answer any recent Ping. The RTT that keeps
/// showing up among the candidate ages of the last `PONG_VOTES` Pongs wins (with the uneven
/// `ping_gap_ms` cadence only the true one repeats); a Ping already answered, or older than the
/// last one answered, needs one vote more (it is the "the last match was off by a Ping" fix-up).
/// Without enough votes (a jittery link, the first Pongs) the latest-RTT rule of `match_pong`
/// picks among the unanswered newer Pings, as before.
struct PongMatcher {
	/// Recent Pings, oldest first: when each was sent and whether a Pong was matched to it. Kept
	/// for `PING_LOST` either way — the vote needs the answered ones too.
	sent: std::collections::VecDeque<(std::time::Instant, bool)>,
	/// The last `PONG_VOTES` Pongs: when each arrived and its candidate ages (ms).
	votes: std::collections::VecDeque<(std::time::Instant, Vec<f32>)>,
	last_matched: Option<std::time::Instant>,
	/// The latest RTT reading (ms; 0 = none yet).
	rtt_last: f32,
	/// Pings sent (the `ping_gap_ms` index).
	n: u64,
}

impl PongMatcher {
	fn new() -> Self {
		Self {
			sent: std::collections::VecDeque::with_capacity(8),
			votes: std::collections::VecDeque::with_capacity(PONG_VOTES + 1),
			last_matched: None,
			rtt_last: 0.0,
			n: 0,
		}
	}

	fn forget_old(&mut self, now: std::time::Instant) {
		while self.sent.front().is_some_and(|(t, _)| now.saturating_duration_since(*t) > PING_LOST) {
			self.sent.pop_front();
		}
	}

	/// A Ping went out at `now`; returns the gap before the next one.
	fn on_ping(&mut self, now: std::time::Instant) -> std::time::Duration {
		self.forget_old(now);
		self.sent.push_back((now, false));
		self.n += 1;
		std::time::Duration::from_millis(ping_gap_ms(self.n))
	}

	/// A Pong arrived at `now`: its RTT (ms), or `None` when no Ping is outstanding.
	fn on_pong(&mut self, now: std::time::Instant) -> Option<f32> {
		self.forget_old(now);
		if self.sent.is_empty() {
			return None;
		}
		let ages: Vec<f32> =
			self.sent.iter().map(|(t, _)| now.saturating_duration_since(*t).as_secs_f32() * 1000.0).collect();
		let last = self.last_matched;
		let fresh = |j: usize| {
			let (t, answered) = self.sent[j];
			!answered && last.is_none_or(|m| t > m)
		};
		// A vote counts within the scatter of one reading, plus the drift an RTT can show since
		// that Pong (a filling queue grows it by tens of ms per second).
		let score = |c: f32| {
			self.votes
				.iter()
				.filter(|(at, v)| {
					let tol = (0.05 * c).max(40.0) + PONG_DRIFT * now.saturating_duration_since(*at).as_secs_f32() * 1000.0;
					v.iter().any(|x| (c - x).abs() <= tol)
				})
				.count() as i32
		};
		let near = |c: f32| if self.rtt_last > 0.0 { -(c - self.rtt_last).abs() } else { c };
		// (key, fresh, nearness): ties go to a fresh Ping, then to the age nearest the latest RTT.
		let mut best: Option<(i32, bool, f32, usize)> = None;
		for (j, &c) in ages.iter().enumerate() {
			let f = fresh(j);
			let key = score(c) - i32::from(!f);
			let cand = (key, f, near(c), j);
			if best.is_none_or(|b| (cand.0, cand.1) > (b.0, b.1) || ((cand.0, cand.1) == (b.0, b.1) && cand.2 > b.2)) {
				best = Some(cand);
			}
		}
		let (key, _, _, mut j) = best?;
		if key < PONG_VOTE_MIN {
			// No consistent RTT yet: the latest-RTT rule among the unanswered newer Pings.
			let pool: Vec<usize> = (0..ages.len()).filter(|&j| fresh(j)).collect();
			let pool = if pool.is_empty() { (0..ages.len()).collect() } else { pool };
			let pool_ages: Vec<std::time::Duration> =
				pool.iter().map(|&j| std::time::Duration::from_secs_f32(ages[j] / 1000.0)).collect();
			j = pool[match_pong(&pool_ages, self.rtt_last)?];
		}
		self.sent[j].1 = true;
		self.last_matched = Some(self.sent[j].0);
		self.rtt_last = ages[j];
		if self.votes.len() >= PONG_VOTES {
			self.votes.pop_front();
		}
		self.votes.push_back((now, ages.clone()));
		Some(ages[j])
	}
}

/// The adaptive cap for the path the media runs on now: on the relay its per-session
/// forwarding cap (`relay_cap_raw`, 0 = uncapped; 10 % headroom and ≥ 2 Mbit, as play.rs sizes
/// it at the start) bounds the direct-path cap; direct = `direct_cap` alone.
fn path_cap_kbps(relay: bool, relay_cap_raw: u32, direct_cap: u32) -> u32 {
	if relay && relay_cap_raw > 0 {
		let c = (relay_cap_raw * 9 / 10).max(2_000);
		if direct_cap > 0 { c.min(direct_cap) } else { c }
	} else {
		direct_cap
	}
}

/// A video packet seen missing (a seq gap) that has not arrived or been rebuilt yet.
#[derive(Clone, Copy, Debug)]
struct Miss {
	/// When the gap showed it missing (the repair window runs from here).
	t0: std::time::Instant,
	/// When it was NACKed (`None` = held back while FEC may still rebuild it).
	nacked: Option<std::time::Instant>,
	/// The NACK was re-sent once (the first NACK or its answer may itself have been lost).
	renacked: bool,
}

/// How many recently FEC-rebuilt seqs to remember (several frames' worth at 120 fps).
const FEC_REBUILT_KEEP: usize = 256;
/// Parity seen within this long = the host is sending FEC (it turns it off on a clean path).
const FEC_ACTIVE_FOR: std::time::Duration = std::time::Duration::from_secs(1);

/// Whether the host sends FEC parity now (`since_parity` = since the last parity datagram).
fn fec_active(since_parity: Option<std::time::Duration>) -> bool {
	since_parity.is_some_and(|d| d < FEC_ACTIVE_FOR)
}

/// How long a missing packet's NACK waits for FEC at most while the host sends parity (it is
/// sent sooner once its block's parity arrived without rebuilding it, `fec_block_gave_up`):
/// two frame intervals + 8 ms, as a small frame's parity trails it by up to two more frames
/// (`media::RS_GROUP_MAX_FRAMES`) plus the pacing. A NACK sent before FEC had its go asks for a
/// retransmit FEC was about to make redundant — more traffic on the very link losing packets.
/// fps 0 = the host default: assume 60.
fn fec_wait_ms(fps: u32) -> f32 {
	let fps = if fps == 0 { 60 } else { fps };
	(2000.0 / fps as f32 + 8.0).clamp(15.0, 50.0)
}

/// Bounds of the reorder wait (ms): how long a seq gap is given to fill by itself before it is
/// NACKed. Some paths reorder a lot — a Wi-Fi Windows host (MF/ffmpeg) to the Orange Pi showed
/// ~20 % of video packets late by a few ms (2026-11-09) — and an instant NACK on every gap made
/// each of them a retransmit (duplicates) and, worse, a "repair" the controller read as ~20 %
/// pre-repair loss on a clean LAN (it cut to 2 Mbit).
const REORDER_WAIT_MIN_MS: f32 = 3.0;
const REORDER_WAIT_MAX_MS: f32 = 25.0;

/// The reorder wait for the observed reorder lateness (a slowly decaying peak, ms).
fn reorder_wait_ms(reorder_peak_ms: f32) -> f32 {
	(reorder_peak_ms * 1.25).clamp(REORDER_WAIT_MIN_MS, REORDER_WAIT_MAX_MS)
}

/// How long a gap waits before its NACK: the reorder wait, and while the host sends parity
/// also FEC's go (`fec_wait_ms`).
fn nack_wait_ms(fec_on: bool, fps: u32, reorder_peak_ms: f32) -> f32 {
	let reorder = reorder_wait_ms(reorder_peak_ms);
	if fec_on { fec_wait_ms(fps).max(reorder) } else { reorder }
}

/// A missing seq arrived: was it the retransmit our NACK asked for (a repair), or the original
/// arriving late (reordering)? Never NACKed = reordering. NACKed less than half an RTT ago = the
/// original too — a retransmit can't be back that soon.
fn arrival_is_repair(nacked: Option<std::time::Instant>, now: std::time::Instant, rtt_ms: f32) -> bool {
	nacked.is_some_and(|t| now.duration_since(t).as_secs_f32() * 1000.0 >= 0.5 * rtt_ms.max(0.0))
}

/// After a Reed-Solomon parity shard (`idx` of `m`) of a block was applied: FEC can no longer
/// rebuild the block's still-missing packets — more are missing than the block has parity, or
/// its last parity shard is here (the host sends them in order).
fn fec_block_gave_up(missing_in_block: usize, m: u8, idx: u8) -> bool {
	missing_in_block > m as usize || idx.saturating_add(1) >= m
}

/// How many of the block `base..base+k` are missing.
fn missing_in(missing: &std::collections::HashMap<u16, Miss>, base: u16, k: u16) -> usize {
	(0..k).filter(|&i| missing.contains_key(&base.wrapping_add(i))).count()
}

/// The block `base..base+k`'s missing packets whose NACK still waits for FEC: marked NACKed
/// (`now`) and returned, as FEC is done with them.
fn take_block_nacks(
	missing: &mut std::collections::HashMap<u16, Miss>,
	base: u16,
	k: u16,
	now: std::time::Instant,
) -> Vec<u16> {
	let mut out = Vec::new();
	for i in 0..k {
		let seq = base.wrapping_add(i);
		if let Some(e) = missing.get_mut(&seq) {
			if e.nacked.is_none() {
				e.nacked = Some(now);
				out.push(seq);
			}
		}
	}
	out
}

/// The held-back NACKs (`pending`, detection order) whose FEC wait (`wait`) ran out: marked
/// NACKed and returned. Seqs repaired meanwhile, or already NACKed with their block, drop out.
fn take_due_fec_nacks(
	pending: &mut std::collections::VecDeque<(u16, std::time::Instant)>,
	missing: &mut std::collections::HashMap<u16, Miss>,
	wait: std::time::Duration,
	now: std::time::Instant,
) -> Vec<u16> {
	let mut out = Vec::new();
	while let Some(&(seq, t)) = pending.front() {
		if now.duration_since(t) < wait {
			break;
		}
		pending.pop_front();
		if let Some(e) = missing.get_mut(&seq) {
			if e.nacked.is_none() {
				e.nacked = Some(now);
				out.push(seq);
			}
		}
	}
	out
}

/// Remember a seq FEC rebuilt (bounded, oldest out).
fn note_rebuilt(rebuilt: &mut std::collections::VecDeque<u16>, seq: u16) {
	if rebuilt.len() >= FEC_REBUILT_KEEP {
		rebuilt.pop_front();
	}
	rebuilt.push_back(seq);
}

/// Host-silence watchdog bounds. Nothing at all from the host (control OR media) for
/// `SILENT_ANY` = the link is dead. Control alone silent for `SILENT_CONTROL` while media still
/// flows = a dead control channel (a host that stopped answering Pings / a half-dead session):
/// end it eventually, but not after 5 s — on a 40 %-loss mobile link a run of lost Pongs is
/// ordinary, and ending the session on it was one of the field "stream stopped"s.
const SILENT_ANY: std::time::Duration = std::time::Duration::from_secs(5);
const SILENT_CONTROL: std::time::Duration = std::time::Duration::from_secs(15);

/// Whether the host has gone silent long enough to end the session: `since_control` = time since
/// the last control datagram (Pong / DataMsg), `since_media` = since the last media datagram.
fn host_silent(since_control: std::time::Duration, since_media: std::time::Duration) -> bool {
	since_control.min(since_media) > SILENT_ANY || since_control > SILENT_CONTROL
}

/// No video datagram at all for this long while the host still answers = the host stopped
/// sending video (stopped sharing, a capture rebuild that failed, a screen picker): the
/// renderer gets `stopped 1` (its "stream stopped" card + input-capture release). The renderer
/// no longer decides this itself — on a lossy link it saw "no decodable AU" while packets still
/// flowed and stopped a live session; packets still arriving (however mangled) is not a stop.
const VIDEO_STOPPED: std::time::Duration = std::time::Duration::from_secs(5);

/// Whether the stream really stopped (see `VIDEO_STOPPED`): only once video has flowed in this
/// session (`video_seen`; a session that never got video is still starting), and only while the
/// control channel is alive — a dead link is `host_silent`'s call, which ends the session.
fn video_stopped(video_seen: bool, since_video: std::time::Duration, since_control: std::time::Duration) -> bool {
	video_seen && since_video >= VIDEO_STOPPED && since_control < SILENT_ANY
}

/// A ladder point as (width, height, fps) — what the user sees, without the codec-dependent
/// minimum rate.
type PointKey = (u32, u32, u32);

fn point_key(p: &pulsar_core::adapt::Point) -> PointKey {
	(p.width, p.height, p.fps)
}

/// Minimum gap between two "the link can't carry your pick" notices, so a point that flaps
/// between two lighter rungs doesn't toast every window.
const SHORTFALL_NOTICE_GAP: std::time::Duration = std::time::Duration::from_secs(30);

/// Whether to tell the user about the current pick shortfall (`now` = `Controller::
/// pick_shortfall` as (picked, current)): when there is one and it differs from the last one
/// told (`last`, `since_last` ago), at most once per `SHORTFALL_NOTICE_GAP`. The same shortfall
/// coming back after the link recovered is not repeated.
fn shortfall_due(
	last: Option<(PointKey, PointKey)>,
	since_last: Option<std::time::Duration>,
	now: Option<(PointKey, PointKey)>,
) -> bool {
	match now {
		None => false,
		Some(n) => last != Some(n) && since_last.is_none_or(|e| e >= SHORTFALL_NOTICE_GAP),
	}
}

/// The shortfall notice text (Turkish unless the UI runs in English). Inline rather than in
/// `lang/*.json`: it carries two runtime labels and is the only stream notice built here.
fn shortfall_text(picked: &str, current: &str, english: bool) -> String {
	if english {
		format!("The connection can't carry {picked} — using {current}")
	} else {
		format!("Bağlantı {picked} taşımıyor — {current} kullanılıyor")
	}
}

/// Received video payload rate in kbit/s over a `window_ms` window (0 when unknown).
fn goodput_kbps(bytes: u64, window_ms: u32) -> u32 {
	if window_ms == 0 {
		0
	} else {
		(bytes * 8 / window_ms as u64).min(u32::MAX as u64) as u32
	}
}

/// Hold the control session open full-duplex: forward input + side-channel data,
/// keepalive every ~2s (UDP has no disconnect signal), and receive the host's
/// chat/clipboard pushes — surfacing them to the UI.
#[allow(clippy::too_many_arguments)]
pub(super) async fn hold_session(
	mut sess: Session,
	app_ev: AppHandle,
	send_flag: Arc<AtomicBool>,
	mut input_rx: Receiver<InputEvent>,
	mut data_rx: Receiver<DataMsg>,
	mut restream_rx: Receiver<Restream>,
	id: u64,
	video_port: u16,
	audio_port: u16,
	encoder_h: String,
	codec_h: String,
	game_mode: bool,
	decode_codecs: Vec<String>,
	render_stdin: std::sync::Arc<std::sync::Mutex<Option<std::process::ChildStdin>>>,
	mos: bool,
	host_nack: bool,
	// The host does reference-frame invalidation (see play.rs `host_rfi`).
	host_rfi: bool,
	req_w: u32,
	req_h: u32,
	req_fps: u32,
	base_kbps: u32,
	// Relay per-session bitrate cap (kbit/s, 0 = uncapped), already headroom-discounted by
	// play.rs. Caps the adaptive controller's ceiling AND any user-picked manual bitrate so a
	// relayed session never re-overshoots the relay's drop limit after a menu change / ramp-up.
	cap_kbps: u32,
	// The shared adaptive controller (`pulsar_core::adapt`), built by play.rs so the initial
	// request already carries its start point; this loop feeds and actuates it.
	mut ctl: Controller,
	// What identifies the peer for the per-peer memory (`adapt_memory`): id or address.
	peer_key: String,
	// The kind of path the session started on (the memory's second key); followed live below.
	path_class: crate::adapt_memory::PathClass,
	cursor_external: bool,
	req_hdr: bool,
	init_quality: QualityPref,
	// The resolved audio quality the session started with, what "auto" resolved to (a menu
	// pick of auto goes back to it), and whether the host can send Raw.
	init_audio_quality: pulsar_core::audio::AudioQuality,
	auto_audio_quality: pulsar_core::audio::AudioQuality,
	host_audio_raw: bool,
	// Initial host monitor (0 = primary). Seeds `cur_display` so a re-request (adaptive
	// bitrate step / menu change) preserves the monitor the session STARTED on — without
	// this, a same-host second pane that started on a free monitor would snap back to 0 on
	// the first adaptive step, re-colliding with the sibling pane's DXGI capture.
	init_display: u32,
	// Initial per-window capture target (Phase 2b co-op). Seeds `cur_window` so every
	// re-request (adaptive bitrate / menu change) preserves the WGC window the session
	// started on — without this, the first adaptive step would drop `window_hwnd` to None
	// and snap the host back to whole-monitor DXGI capture. `None` = the monitor path.
	init_window: Option<i64>,
	// On-demand host-window-list queries (Phase 2b co-op): the `host_window_list` command
	// sends a oneshot reply-sender here; we service it by asking the host (`query_windows`)
	// over the control session we own and answering on the oneshot. Routed through the hold
	// loop so the held `Session` stays single-owner.
	mut windows_query_rx: Receiver<tokio::sync::oneshot::Sender<Vec<pulsar_core::service::WindowInfo>>>,
	// Host → client rumble: each DataMsg::Rumble is forwarded here for the gilrs reader
	// thread to replay on the physical pad (force-feedback).
	rumble_tx: tokio::sync::mpsc::Sender<(u8, u8, u8)>,
	// Every host monitor `(idx, width, height, refresh Hz)` and the client's own fps target:
	// a monitor switch rebuilds the adaptive ladder for the new monitor (its size, and the fps
	// it can capture at, capped by what this display shows).
	host_displays: Vec<(u32, u32, u32, u32)>,
	client_fps: u32,
	// The adaptive cap for a DIRECT path (the user's configured bitrate, else the auto cap):
	// a relayed session that upgrades to direct mid-stream drops the relay's cap for this.
	direct_cap_kbps: u32,
) {
	// A relayed session keeps punching in the background (core `punch_in_background`) and may
	// upgrade to a direct path mid-stream; the adaptive rate then starts over on it.
	let mut on_relay = matches!(sess.transport(), pulsar_core::Transport::Relay);
	// DualSense pad mic → host: raw TAG_DS5 payloads straight onto the session.
	crate::ds5::client::Ds5Client::set_media_sender(id, sess.sender());
	let mut keep = tokio::time::interval(std::time::Duration::from_secs(2));
	keep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
	// The keepalive Ping fires at uneven gaps (`ping_gap_ms`, mean PING_MS); the first at once.
	let ping = tokio::time::sleep(std::time::Duration::ZERO);
	tokio::pin!(ping);
	let mut repair = tokio::time::interval(std::time::Duration::from_millis(REPAIR_SWEEP_MS));
	repair.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
	// ---- Media-over-session demux state (single-socket transport) ----
	// Media frames arrive INSIDE this session ([tag][rtp…]); forward each datagram to
	// the local consumer: the renderer's RTP port (video) / the viewer's audio port.
	let fwd_sock = if mos {
		tokio::net::UdpSocket::bind("127.0.0.1:0").await.ok()
	} else {
		None
	};
	let v_dest = std::net::SocketAddr::from(([127, 0, 0, 1], video_port));
	// Linux: video goes to the renderer's Unix datagram socket instead (no IP stack per
	// packet); UDP stays the path elsewhere and for consumers that don't bind it.
	let mut v_unix = crate::native_view::VideoSink::new(video_port);
	let a_dest = std::net::SocketAddr::from(([127, 0, 0, 1], audio_port));
	// One-shot diagnostic: did host media actually reach this client + get forwarded?
	let mut first_video_logged = false;
	// Video RTP gap tracking → NACK retransmit requests + the loss% the adaptive
	// controller runs on. `missing` holds requested-but-not-yet-seen seqs (see `Miss`: when
	// first noticed, when NACKed, and whether the NACK was already re-sent once).
	let mut last_vseq: Option<u16> = None;
	let mut missing: std::collections::HashMap<u16, Miss> = std::collections::HashMap::new();
	// Missing seqs whose NACK waits for FEC (detection order; see `fec_wait_ms`), the seqs FEC
	// rebuilt lately (a block's tail can be rebuilt before the next frame's first packet shows
	// the gap — not lost, not to be NACKed), and when the last parity datagram arrived.
	let mut fec_pending: std::collections::VecDeque<(u16, std::time::Instant)> = std::collections::VecDeque::new();
	let mut fec_rebuilt: std::collections::VecDeque<u16> = std::collections::VecDeque::with_capacity(FEC_REBUILT_KEEP);
	let mut last_parity_at: Option<std::time::Instant> = None;
	let (mut win_recv, mut win_lost) = (0u32, 0u32);
	// NACK bookkeeping per window (forwarded to the host in the stats JSON).
	let (mut win_nack_sent, mut win_nack_ok) = (0u32, 0u32);
	// Missing seqs that arrived on their own (reordering, not a repair) this window, and the
	// lateness peak (ms, decays slowly) the reorder wait follows.
	let mut win_reordered = 0u32;
	let mut reorder_peak_ms: f32 = 0.0;
	// Keepalive RTT samples of the current window + a smoothed RTT for the repair window.
	let mut rtt_samples: Vec<f32> = Vec::with_capacity(8);
	let mut rtt_ema: f32 = 0.0;
	// The last few RTT samples: their median and spread size the repair window (the EMA keeps
	// the first, setup-inflated ping for seconds).
	let mut rtt_recent: std::collections::VecDeque<f32> = std::collections::VecDeque::with_capacity(RTT_RECENT);
	// The repair window last pushed to the renderer (ms) — re-sent only on a real change.
	let mut sent_repair_ms: f32 = 0.0;
	// Loss-recovery mode currently requested from the host (flipped once by the controller).
	// DEBUG knob (env-gated): start in a given loss-recovery mode instead of switching on the
	// first sustained loss — `PULSAR_RECOVERY=ir|shortgop` (pairs with the renderer's
	// PULSAR_TEST_DROP to exercise the resume path).
	let mut cur_recovery = match std::env::var("PULSAR_RECOVERY").as_deref() {
		Ok("ir") => LossRecovery::IntraRefresh,
		Ok("shortgop") => LossRecovery::ShortGop,
		_ => LossRecovery::Normal,
	};
	// FEC (Phase 2.1): the host sends XOR parity over the video packets once we report loss;
	// the decoder keeps the recent packets and rebuilds a group's single missing one.
	let mut fec_dec = media::FecDecoder::default();
	// Reed-Solomon parity (preferred: several losses per frame); the XOR decoder above stays
	// for hosts that only have the single-loss groups.
	let mut fec_rs = media::RsFecDecoder::default();
	let mut win_fec_ok = 0u32;
	// Delay-gradient sampling (Phase 3): per video FRAME (RTP timestamp), the arrival time of
	// its last packet on a session-local ms clock feeds the controller's trendline estimator.
	let epoch = std::time::Instant::now();
	let mono_ms = move || epoch.elapsed().as_secs_f64() * 1000.0;
	let mut frame_ts: Option<u32> = None;
	// A Dynamic-refresh panel idles at its low rate for us: run it at its top rate while this
	// session streams faster than that (restored when the guard drops at the session's end).
	#[cfg(windows)]
	let _display_boost = crate::util::DisplayBoost::acquire(
		&app_ev,
		ctl.points().first().map(|p| p.fps).unwrap_or(req_fps),
	);
	// Opening phase (Windows `--hold-reveal`): reveal the video once the bitrate has settled.
	let mut revealed = false;
	let mut first_frames_at: Option<std::time::Instant> = None;
	let mut frame_last_arrival_ms: f64 = 0.0;
	// The host's display name (DataMsg::PeerName), for the "file arrived" toasts.
	let mut host_name = String::new();
	// Frame-clock probe (diagnostics): over ~2 s, how far the host's RTP clock advanced vs our
	// wall clock, how many frames arrived and the worst inter-arrival gap. A host that emits
	// fewer frames than its RTP clock claims shows rtp < wall (a fake, permanent delay trend);
	// a bursty radio link shows rtp ≈ wall with big gaps.
	let mut fc: Option<(u32, f64)> = None; // (first ts, first arrival) of the window
	let mut fc_n = 0u32;
	let mut fc_prev_arr = 0.0f64;
	let mut fc_max_gap = 0.0f64;
	// User pins from the session menu: an explicit resolution or fps switches the ladder off
	// (bitrate-only), "Otomatik" hands that dimension back to the controller.
	// Saved picks applied in play.rs before the first request start out pinned too.
	let mut manual_res = ctl.pins().0.is_some();
	let mut manual_fps = ctl.pins().1.is_some();
	// The last rate remembered for this peer (write-through when it changes).
	let mut remembered_kbps: u32 = 0;
	// Failed-repair keyframe request: when a NACKed packet never arrives (expired from
	// `missing`), the reference chain is broken until the next keyframe — with long host
	// GOPs that is seconds of smear. Ask the host for an immediate IDR instead (rate-limited).
	let mut last_kfreq = std::time::Instant::now();
	let mut kf_gap = KF_GAP_MIN; // see kf_allowed / kf_next_gap
	// When the last menu re-request (esp. a monitor switch) fired. The host rebuilds its encoder
	// for one → a brief packet-loss gap that is NOT congestion; the adaptive controller must
	// ignore it for a moment, or it steps the bitrate (a full-path restream that churns + slows
	// the very switch in progress). Seeded far in the past so startup isn't suppressed.
	let mut last_switch_at = std::time::Instant::now() - std::time::Duration::from_secs(60);
	// When each keepalive Ping was sent, to time the host's Pongs (see `PongMatcher`).
	let mut pings = PongMatcher::new();
	// Watchdog: a host-side kick / network drop doesn't always send a clean close over UDP,
	// so track the last time we heard ANYTHING from the host (incl. keepalive Pongs). If it
	// goes silent past the timeout, end the session — this fires `play-ended` so the client
	// tears down (releases the input grab + drops the tab) instead of freezing on the last
	// frame with the keyboard/mouse still captured.
	let mut last_inbound = std::time::Instant::now();
	// The last MEDIA datagram (video / audio / parity): liveness too — see `host_silent`.
	let mut last_media = std::time::Instant::now();
	// The last VIDEO datagram, and whether the renderer was told the stream stopped
	// (`video_stopped`; edge-triggered `stopped 1|0`).
	let mut last_video = std::time::Instant::now();
	let mut told_stopped = false;
	// Per-window measurement (field diagnostics + the controller's goodput): video RTP payload
	// bytes / packets received as FIRST copies (NACK-repaired ones included, duplicates and
	// parity not), and when the window started.
	let mut win_recv_bytes: u64 = 0;
	let mut win_recv_fresh: u32 = 0;
	let mut win_start = std::time::Instant::now();
	// The kind of path the media runs on now (`adapt_memory` key): the last good rate is
	// remembered under the class the session actually ran cleanly on.
	let mut cur_class = path_class;
	// The last pick shortfall told to the user, and when (see `shortfall_due`).
	let mut shortfall_told: Option<(PointKey, PointKey)> = None;
	let mut shortfall_told_at: Option<std::time::Instant> = None;
	// Current stream state; a session-menu change updates one field and re-requests.
	// Geometry/fps seed from the INITIAL request (the Linux native path negotiates an
	// explicit startup size — e.g. the deliberate 960x540@30 mpv cap): a re-request
	// (adaptive-bitrate step / menu change) must preserve it. Seeding 0 instead meant
	// "host config default" (1080p60), flipping a capped stream to full size exactly
	// when the link was already lossy.
	let (mut cur_w, mut cur_h) = (req_w, req_h);
	let mut cur_encoder = encoder_h;
	let mut cur_codec = codec_h;
	let mut cur_fps = req_fps;
	let mut cur_transmit = true;
	// Host-silent intent: game mode requests it by default (seeds the initial play.rs request);
	// the host honors it by REDIRECTING its default output to a virtual sink (never muting the
	// captured endpoint — see play.rs). Preserved across every re-request; the user can flip it
	// mid-session from the overlay (sets cur_mute below).
	let mut cur_mute = game_mode;
	// Host monitor index (0 = primary). Seeded from the INITIAL request — a same-host
	// second pane starts on a FREE monitor (not 0) — and changed live by the session menu's
	// monitor picker (Restream::Display); preserved across every re-request below.
	let mut cur_display: u32 = init_display;
	// Per-window capture target (Phase 2b co-op). Seeded from the INITIAL request so it
	// survives every re-request; a session-menu "window" pick changes it live via
	// Restream::Window. `Some(hwnd)` makes the host capture that single window via WGC
	// (overriding cur_display); `None` = the cur_display monitor path.
	let mut cur_window: Option<i64> = init_window;
	// Screen-ADAPTATION target (Parsec-style): `Some((w,h))` asks the host to switch the captured
	// monitor to the display mode best fitting a w×h pane (this renderer's window) and to REVERT it
	// on teardown; `None` = no change. Flipped live by the overlay's "Ekran uyarlama" toggle via
	// Restream::Adapt; preserved across every re-request. Ignored host-side when a window target is
	// set (per-window capture has no monitor mode).
	let mut cur_adapt: Option<(u32, u32)> = None;
	// Requested audio channel layout. Stereo today (the client default in play.rs — the
	// audio paths expect opus/48000/2 and the host negotiates surround down anyway);
	// kept as state so it's preserved across every re-request and a future surround
	// picker can flip it without re-plumbing each StreamReq.
	let cur_audio_layout = pulsar_core::audio::ChannelLayout::Stereo;
	let mut cur_audio_quality = init_audio_quality;
	// Starts at the INITIAL request's bitrate (0 = host default) so a menu-driven
	// re-request doesn't silently reset an explicit starting target; the adaptive
	// controller (`pulsar_core::adapt`) moves it within [FLOOR_KBPS, cap].
	let mut cur_bitrate = base_kbps;
	// A user-picked limit (session menu / overlay) disables the adaptive controller;
	// picking "Otomatik" (0) re-enables it.
	// The controller needs the media path's seq/timestamps → media-over-session only.
	let adapt_enabled = mos;
	let _ = (base_kbps, cap_kbps);
	// Quality bias: seeded from the Settings → Display 'Varsayılan kalite' control
	// (already resolved in play.rs: "hq"→Quality, "fast"→Latency, "auto"→mode default).
	// The session overlay's quality picker (Restream::Quality) can override it live.
	let mut cur_quality = init_quality;
	// HDR preference from the UI toggle (Settings → Display). The PULSAR_HDR env var
	// is a debug override: if set it wins regardless of the UI value.
	let cur_hdr = req_hdr || std::env::var_os("PULSAR_HDR").is_some();
	// Inbound file reassembly (file-manager downloads — the host streams an FsGet
	// back as FileBegin/Chunk/End): Begin → state, Chunk → store BY INDEX,
	// End → save in the received-files folder. The session transport is unordered UDP,
	// so chunks can arrive reordered or duplicated; keying by index (not appending
	// in arrival order) lets reorders/dups self-heal — only a genuinely lost chunk
	// fails the transfer. Each transfer carries a per-stream `id`, so concurrent
	// downloads keep separate reassembly state keyed by that id and interleaved
	// messages no longer clobber each other. Mirrors the host's make_on_file.
	/// Idle transfers older than this are swept from the map on the next FileBegin.
	const F_XFER_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
	/// Maximum concurrent in-flight transfers. If a new FileBegin would push us over
	/// this limit we first evict a headless entry (`expected == None` — FileBegin was
	/// lost, can never complete) and only fall back to oldest-by-last_activity among
	/// active entries, to avoid silently killing a slow-but-legitimate transfer.
	const F_MAX_CONCURRENT_XFERS: usize = 8;
	struct FileReasm {
		/// Set by FileBegin. `None` means chunks arrived before FileBegin (UDP
		/// reorder); the entry was lazily created to buffer them. A FileEnd with
		/// `expected == None` is treated as incomplete (Begin never arrived).
		name: String,
		expected: Option<u32>,
		chunks: std::collections::BTreeMap<u32, Vec<u8>>,
		// Running total of buffered bytes (sum of every stored chunk's len, dup-safe).
		// Capped against MAX_XFER_BYTES so a peer can't OOM us by announcing a huge
		// chunk count and streaming distinct-index chunks without ever sending FileEnd.
		received: u64,
		/// Updated on FileBegin and every FileChunk; entries idle beyond
		/// F_XFER_IDLE_TIMEOUT are swept on the next FileBegin so a lost FileEnd
		/// (UDP, no retransmit) can't leak buffered bytes for the session lifetime.
		last_activity: std::time::Instant,
	}
	let mut f_xfers: std::collections::HashMap<u32, FileReasm> = std::collections::HashMap::new();
	// Pending host-window-list query (Phase 2b co-op picker, Bug 4). When the
	// `host_window_list` command hands us a oneshot reply-sender we fire a `QueryWindows`
	// over the session and stash the sender + a deadline here — then resolve it from the
	// inbound demux below when the host's `Windows` reply arrives, instead of a blocking
	// read-loop that consumed (and discarded) the media/RTP racing on the same session. A
	// stale query (old / non-Windows host that never replies) resolves to an empty list at
	// the deadline. At most one is in flight; a superseding query answers the prior empty.
	let mut pending_windows: Option<(
		tokio::sync::oneshot::Sender<Vec<pulsar_core::service::WindowInfo>>,
		tokio::time::Instant,
	)> = None;
	// The current stream state as a request (every re-request — menu or controller — sends
	// the FULL state so nothing snaps back to a default).
	macro_rules! make_req {
		() => {
			StreamReq {
				port: video_port,
				codec: cur_codec.clone(),
				encoder: cur_encoder.clone(),
				width: cur_w,
				height: cur_h,
				fps: cur_fps,
				audio_port,
				transmit_audio: cur_transmit,
				mute_host: cur_mute,
				game_mode,
				bitrate_kbps: cur_bitrate,
				quality: cur_quality,
				hdr: cur_hdr,
				yuv444: std::env::var_os("PULSAR_YUV444").is_some(),
				decode_codecs: decode_codecs.clone(),
				media_over_session: mos,
				cursor_external,
				display_idx: cur_display,
				window_hwnd: cur_window,
				adapt: cur_adapt,
				audio_layout: cur_audio_layout,
				audio_quality: cur_audio_quality,
				loss_recovery: cur_recovery,
				fec: mos,
				fec_rs: mos,
				rfi: host_rfi && cur_codec != "av1",
			}
		};
	}
	loop {
		if !send_flag.load(Ordering::SeqCst) {
			break;
		}
		// Deadline for a still-pending window-list query, so the select's fallback arm can
		// resolve it empty if the host never sends a `Windows` reply. `None` disables that arm.
		let win_deadline = pending_windows.as_ref().map(|(_, d)| *d);
		tokio::select! {
			ev = input_rx.recv() => match ev {
				Some(ev) => if send_input(&mut sess, &ev).await.is_err() { break },
				None => break,
			},
			d = data_rx.recv() => match d {
				// A side-channel send failure must NOT kill the session: an oversized
				// payload (e.g. an avatar past the datagram budget → EMSGSIZE) is a
				// one-message problem, and a genuinely dead link is already caught by
				// the keepalive watchdog below. Breaking here tore the whole session
				// down 0.7 s into the stream ("connected but no video").
				Some(dm) => if let Err(e) = send_data(&sess, &dm).await {
					tracing::warn!(%e, "side-channel send failed (message dropped)");
				},
				None => {}
			},
			inbound = sess.recv() => match inbound {
				Some(bytes) => {
					// Media-over-session fast path: [tag][rtp…] frames → forward the raw
					// RTP datagram to its local consumer (renderer / audio viewer port).
					// Checked FIRST — these are the highest-rate payloads on the session.
					if let Some((tag, rtp)) = media::parse(&bytes) {
						last_media = std::time::Instant::now();
						// DualSense passthrough haptics/speaker slots → the pad threads.
						if tag == pulsar_core::input::ds5::wire::TAG_DS5 {
							crate::ds5::client::on_media(id, &bytes);
							continue;
						}
						// FEC parity (Phase 2.1): rebuild the group's single missing packet — a
						// repair with ZERO round-trips — and hand it to the renderer as if it had
						// arrived (late: the reorder buffer slots it in). Also the only place we
						// learn the host's group size, which the controller deducts from the
						// encoder rate so the wire rate stays at the target.
						// A bandwidth-probe packet arriving after the probe ended (its tail can trail
						// the stream request): nothing to do with it — and it must not reach the
						// audio forward below.
						if tag == media::TAG_PROBE {
							continue;
						}
						if tag == media::TAG_FEC_RS {
							last_parity_at = Some(std::time::Instant::now());
							let block = media::parse_rs_parity(rtp).map(|p| (p.base_seq, p.k as u16, p.m, p.idx));
							for (seq, pkt) in fec_rs.on_parity(rtp) {
								if !v_unix.try_send(&pkt) {
									if let Some(s) = fwd_sock.as_ref() {
										let _ = s.send_to(&pkt, v_dest).await;
									}
								}
								if missing.remove(&seq).is_some() {
									win_lost = win_lost.saturating_sub(1);
								}
								note_rebuilt(&mut fec_rebuilt, seq);
								win_fec_ok += 1;
							}
							// The block's parity is all here (or can't cover what is missing) and
							// packets of it are still missing: FEC is done with them — NACK now.
							if let Some((base, k, m, idx)) = block {
								if fec_block_gave_up(missing_in(&missing, base, k), m, idx) {
									let nacks = take_block_nacks(&mut missing, base, k, std::time::Instant::now());
									if host_nack && !nacks.is_empty() {
										win_nack_sent += nacks.len() as u32;
										let _ = send_data(&sess, &DataMsg::MediaNack(nacks)).await;
									}
								}
							}
							let r = fec_rs.last_ratio();
							if (r - ctl.fec_overhead()).abs() > 1e-3 {
								ctl.set_fec_overhead(r);
							}
							continue;
						}
						if tag == media::TAG_FEC {
							last_parity_at = Some(std::time::Instant::now());
							let group = media::parse_parity(rtp).map(|p| (p.base_seq, p.n as u16));
							if let Some((seq, pkt)) = fec_dec.on_parity(rtp) {
								if !v_unix.try_send(&pkt) {
									if let Some(s) = fwd_sock.as_ref() {
										let _ = s.send_to(&pkt, v_dest).await;
									}
								}
								if missing.remove(&seq).is_some() {
									win_lost = win_lost.saturating_sub(1);
								}
								note_rebuilt(&mut fec_rebuilt, seq);
								win_fec_ok += 1;
							}
							// One parity per XOR group: what it didn't rebuild, it never will.
							if let Some((base, n)) = group {
								let nacks = take_block_nacks(&mut missing, base, n, std::time::Instant::now());
								if host_nack && !nacks.is_empty() {
									win_nack_sent += nacks.len() as u32;
									let _ = send_data(&sess, &DataMsg::MediaNack(nacks)).await;
								}
							}
							let n = fec_dec.last_n();
							let r = if n == 0 { 0.0 } else { 1.0 / n as f32 };
							if (r - ctl.fec_overhead()).abs() > 1e-3 {
								ctl.set_fec_overhead(r);
							}
							continue;
						}
						if let Some(s) = fwd_sock.as_ref() {
							let dest = if tag == media::TAG_VIDEO { v_dest } else { a_dest };
							if tag == media::TAG_VIDEO && !first_video_logged {
								first_video_logged = true;
								tracing::info!(%dest, "first video RTP datagram forwarded to renderer");
							}
							if !(tag == media::TAG_VIDEO && v_unix.try_send(rtp)) {
								let _ = s.send_to(rtp, dest).await;
							}
						} else if !first_video_logged {
							first_video_logged = true;
							tracing::warn!("media arrived but fwd_sock is None — nothing forwarded");
						}
						// Video gap tracking: count loss for the adaptive controller and
						// NACK freshly-missing seqs so the host retransmits them.
						if tag == media::TAG_VIDEO {
							last_video = std::time::Instant::now();
							if let Some(seq) = media::rtp_seq(rtp) {
								fec_dec.on_packet(seq, rtp);
								fec_rs.on_packet(seq, rtp);
								// Frame boundary = a new RTP timestamp: the previous frame is
								// complete, sample its send-vs-arrival delay for the estimator.
								if rtp.len() >= 8 {
									let ts = u32::from_be_bytes([rtp[4], rtp[5], rtp[6], rtp[7]]);
									let now_ms = mono_ms();
									// Only a NEWER timestamp closes the frame. A NACK-retransmitted (or
									// reordered) packet of an older frame used to flip `frame_ts` back
									// and re-sample frames against its late arrival — a steady fake
									// "queue draining" signal that froze the bitrate climb.
									let newer = frame_ts.is_none_or(|cur| (ts.wrapping_sub(cur) as i32) > 0);
									if newer {
										if let Some(prev) = frame_ts {
											ctl.on_frame(prev, frame_last_arrival_ms);
											let arr = frame_last_arrival_ms;
											match fc {
												None => {
													fc = Some((prev, arr));
													fc_n = 0;
													fc_max_gap = 0.0;
												}
												Some((ts0, t0)) => {
													fc_n += 1;
													fc_max_gap = fc_max_gap.max(arr - fc_prev_arr);
													if arr - t0 >= 2000.0 {
														let rtp_ms = (prev.wrapping_sub(ts0) as i32) as f64 / 90.0;
														let wall_ms = arr - t0;
														let drift_pct = (rtp_ms / wall_ms - 1.0) * 100.0;
														let fps = fc_n as f64 * 1000.0 / wall_ms;
														// Info only when it's worth a look: the host clock drifting
														// from ours, or a long hole in the stream. (A stream restart
														// re-bases the host clock — that window is skipped.)
														if rtp_ms > 0.0 && (drift_pct.abs() > 2.0 || fc_max_gap > 250.0) {
															tracing::info!(
																frames = fc_n,
																fps = format!("{fps:.1}"),
																rtp_ms = format!("{rtp_ms:.0}"),
																wall_ms = format!("{wall_ms:.0}"),
																rtp_vs_wall_pct = format!("{drift_pct:.1}"),
																max_gap_ms = format!("{fc_max_gap:.0}"),
																"frame clock: host RTP clock drifts or the stream had a hole"
															);
														} else {
															tracing::debug!(
																frames = fc_n,
																fps = format!("{fps:.1}"),
																rtp_vs_wall_pct = format!("{drift_pct:.1}"),
																max_gap_ms = format!("{fc_max_gap:.0}"),
																"frame clock"
															);
														}
														fc = Some((prev, arr));
														fc_n = 0;
														fc_max_gap = 0.0;
													}
												}
											}
											fc_prev_arr = arr;
										}
										frame_ts = Some(ts);
										frame_last_arrival_ms = now_ms;
									} else if frame_ts == Some(ts) {
										frame_last_arrival_ms = now_ms;
									}
								}
								win_recv += 1;
								// First copy of this seq? Duplicates (and stale reorders of a seq
								// already seen) clear it below; they carry no new payload.
								let mut fresh = true;
								if let Some(e) = missing.remove(&seq) {
									// A requested retransmit or a late reorder arrived — not lost. Only the
									// retransmit counts as a repair (the controller reads repairs as
									// pre-repair loss); a late original feeds the reorder wait instead.
									win_lost = win_lost.saturating_sub(1);
									let now_i = std::time::Instant::now();
									if arrival_is_repair(e.nacked, now_i, median_ms(&rtt_recent, rtt_ema)) {
										win_nack_ok += 1;
									} else {
										win_reordered += 1;
										let late = now_i.duration_since(e.t0).as_secs_f32() * 1000.0;
										reorder_peak_ms = reorder_peak_ms.max(late);
									}
								} else if let Some(last) = last_vseq {
									let d = media::seq_forward(last, seq);
									if d == 0 {
										// Duplicate / retransmit of the last seq — ignore.
										fresh = false;
									} else if d < 0x8000 && d <= 128 {
										// Small forward gap → NACK the missing seqs for retransmit.
										// While the host sends parity the NACK waits until FEC had its
										// go (`fec_pending`, see `fec_wait_ms`).
										if d > 1 {
											let now = std::time::Instant::now();
											// Every gap waits a little first (the reorder wait, plus FEC's
											// go while parity flows); `take_due_fec_nacks` sends it after.
											let defer = true;
											let mut nacks = Vec::with_capacity((d - 1) as usize);
											for i in 1..d {
												let m = last.wrapping_add(i);
												if fec_rebuilt.contains(&m) {
													// FEC rebuilt it before this gap showed: not lost.
													continue;
												}
												missing.insert(m, Miss { t0: now, nacked: (!defer).then_some(now), renacked: false });
												if defer {
													fec_pending.push_back((m, now));
												} else {
													nacks.push(m);
												}
												win_lost += 1;
											}
											if host_nack && !nacks.is_empty() {
												win_nack_sent += nacks.len() as u32;
												let _ = send_data(&sess, &DataMsg::MediaNack(nacks)).await;
											}
										}
										last_vseq = Some(seq);
									} else if d < 0x8000 {
										// Big forward gap (d > 128 but < 0x8000) — encoder restarted
										// with a fresh RTP sequence base (every monitor/codec switch
										// rebuilds it). Resync cleanly to the new base.
										missing.clear();
										fec_pending.clear();
										fec_rebuilt.clear();
										fec_dec.reset();
										fec_rs.reset();
										ctl.on_stream_restart();
										frame_ts = None;
										last_vseq = Some(seq);
									} else {
										// Backward jump (d >= 0x8000). Compute the backward distance:
										// how far behind seq is relative to last.
										let back = 0x10000u32 - d as u32;
										if back > 128 {
											// Large backward jump = the host's encoder RESTARTED with a
											// new sequence base that happens to be numerically lower
											// (roughly half of random bases land here). The old code
											// skipped these — last_vseq stayed STUCK, so every later
											// packet re-tripped the discontinuity and NACK-flooded the
											// session. Resync cleanly to the new base instead.
											missing.clear();
											fec_pending.clear();
											fec_rebuilt.clear();
											fec_dec.reset();
											fec_rs.reset();
											ctl.on_stream_restart();
											frame_ts = None;
											last_vseq = Some(seq);
										}
										// else: small backward distance (≤128 seqs behind) = an
										// ordinary stale reorder or duplicate that arrived late.
										// Leave last_vseq unchanged and do NOT NACK or count loss.
										// (A seq still awaited is in `missing`, handled above —
										// so this one was already seen or written off.)
										else {
											fresh = false;
										}
									}
								} else {
									last_vseq = Some(seq);
								}
								if fresh {
									win_recv_fresh += 1;
									win_recv_bytes += rtp.len() as u64;
								}
								// Held-back NACKs whose FEC wait ran out: send them now.
								if !fec_pending.is_empty() {
									let fec_on = fec_active(last_parity_at.map(|t| t.elapsed()));
									let wait = std::time::Duration::from_secs_f32(nack_wait_ms(fec_on, cur_fps, reorder_peak_ms) / 1000.0);
									let nacks = take_due_fec_nacks(&mut fec_pending, &mut missing, wait, std::time::Instant::now());
									if host_nack && !nacks.is_empty() {
										win_nack_sent += nacks.len() as u32;
										let _ = send_data(&sess, &DataMsg::MediaNack(nacks)).await;
									}
								}
							}
						}
						continue;
					}
						// Reached only by NON-media (control) datagrams -- the media fast-path
						// continue-d above. Reset the grab-release watchdog HERE so it tracks CONTROL
						// liveness (keepalive Pong + any DataMsg), not the video flow: a dead control
						// link with live video must still trip the timeout below and tear the session
						// down instead of leaving it frozen-but-grabbed.
						last_inbound = std::time::Instant::now();
						// Host sent an explicit Bye (it kicked us / went offline / left): end the
						// session NOW. Over a relay our recv never returns None when the host
						// vanishes, so without this the client hangs on the frozen last frame and
						// shows the stall message until the silence watchdog eventually fires.
						// Breaking here runs the `play-ended` emit below — a clean, instant
						// disconnect the UI can act on.
						if pulsar_core::service::is_bye(&bytes) {
							tracing::info!(%id, "host sent Bye — ending play session");
							break;
						}
						if pulsar_core::service::is_pong(&bytes) {
						if let Some(rtt) = pings.on_pong(std::time::Instant::now()) {
							let rtt = rtt as f64;
							let _ = app_ev.emit("play-rtt", PlayRtt { id, rtt });
							rtt_samples.push(rtt as f32);
							rtt_ema = if rtt_ema <= 0.0 { rtt as f32 } else { rtt_ema * 0.7 + rtt as f32 * 0.3 };
							if rtt_recent.len() >= RTT_RECENT {
								rtt_recent.pop_front();
							}
							rtt_recent.push_back(rtt as f32);
							// Feed the native overlay's "Gecikme" tile the real NETWORK
							// latency (it used to show the render present-gap), and size the
							// renderer's RTP reorder wait from it (Phase 0.5): a NACK retransmit
							// needs ~1 RTT to land, so a fixed 100 ms made every repair "too
							// late" on any route slower than that.
							{
								use std::io::Write as _;
								// The renderer waits one repair sweep LONGER than we do, so our
								// `hold` (sent when the sweep writes the repair off) reaches it
								// before its demuxer flushes past the hole and decodes the smear.
								let fec_hold_ms =
									if fec_active(last_parity_at.map(|t| t.elapsed())) { fec_wait_ms(cur_fps) } else { 0.0 };
								let want = repair_window_ms(&rtt_recent, fec_hold_ms) + REPAIR_SWEEP_MS as f32;
								let changed = (want - sent_repair_ms).abs() > sent_repair_ms * 0.15 + 5.0;
								if let Some(si) = render_stdin.lock().unwrap().as_mut() {
									let _ = writeln!(si, "rtt {rtt:.1}");
									if changed {
										let _ = writeln!(si, "maxdelay {}", (want * 1000.0) as u32);
									}
								}
								if changed {
									sent_repair_ms = want;
								}
							}
						}
					} else if let Some(dm) = decode_data(&bytes) {
						match dm {
							DataMsg::Clipboard(text) => {
								// Sync on: written to our clipboard here, silently (it is the
								// host's copy, not something to announce). Off: the UI copies it.
								if !crate::clipboard_sync::apply_remote(&text) {
									let _ = app_ev.emit("data-clip", DataPayload { peer: id.to_string(), text });
								}
							}
							DataMsg::Chat(text) => {
								let _ = app_ev.emit("chat-msg", DataPayload { peer: id.to_string(), text });
							}
							DataMsg::Stats(label) => {
								// Mirror the host's ACTIVE encode summary into the native
								// overlay ("hostenc <label>" stdin line) so it can show what's
								// really in use under the selectors.
								{
									use std::io::Write as _;
									if let Some(si) = render_stdin.lock().unwrap().as_mut() {
										let _ = writeln!(si, "hostenc {label}");
									}
								}
								let _ = app_ev.emit("host-stats", PlayStats { id, label });
							}
							DataMsg::DisplayRotation(deg) => {
								// Render the video upright for a rotated host display (vidsink
								// path on Linux; the Windows pulsar-render rotates in its
								// VideoProcessor Blt via the `rotate <deg>` stdin line).
								#[cfg(all(unix, not(target_os = "macos")))]
								crate::render::apply_vidsink_rotation(&app_ev, id, deg);
								#[cfg(windows)]
								{
									use std::io::Write as _;
									if let Some(si) = render_stdin.lock().unwrap().as_mut() {
										let _ = writeln!(si, "rotate {}", deg % 360);
									}
								}
								#[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
								let _ = deg;
							}
							DataMsg::Avatar(png) => {
								tracing::info!(bytes = png.len(), "host identity image received");
								// The host's identity image for this session, addressed by play
								// id like the other client-side payloads.
								let _ = app_ev.emit("peer-avatar", AvatarPayload {
									peer: id.to_string(),
									data_url: crate::avatar::data_url(&png),
								});
							}
							DataMsg::PeerName(name) => {
								// The host's display name — the UI updates the session target
								// chip and the recents cache. Kept for the file toasts too.
								host_name = name.clone();
								let _ = app_ev.emit("peer-name", (id.to_string(), name));
							}
							DataMsg::CursorPos { x, y } => {
								// Cursor side-channel: the host captured WITHOUT a hardware cursor
								// in the frame (KMS zero-copy) and streams the pointer position
								// out-of-band. Push it to the native renderer so it draws the cursor
								// over the video (Moonlight model). Tiny payload, ~60 Hz.
								use std::io::Write as _;
								if let Some(si) = render_stdin.lock().unwrap().as_mut() {
									let _ = writeln!(si, "cursor {x:.5} {y:.5}");
								}
							}
							DataMsg::CursorShape { w, h, hot_x, hot_y, rgba_png } => {
								// The host pointer's bitmap changed (caret/resize transitions).
								// Base64 the PNG onto one stdin line so the renderer swaps the drawn
								// cursor image + hotspot.
								use base64::Engine as _;
								use std::io::Write as _;
								let b64 = base64::engine::general_purpose::STANDARD.encode(&rgba_png);
								if let Some(si) = render_stdin.lock().unwrap().as_mut() {
									let _ = writeln!(si, "cursorimg {w} {h} {hot_x} {hot_y} {b64}");
								}
							}
							DataMsg::PointerLock(on) => {
								// The host hid its cursor (a game grabbed the mouse): send relative
								// motion while it lasts, absolute again once it shows.
								tracing::info!(on, "host pointer lock");
								crate::kbdhook::set_pointer_lock(on);
							}
							DataMsg::CursorHidden => {
								// The host pointer is hidden / left the screen — stop drawing it.
								use std::io::Write as _;
								if let Some(si) = render_stdin.lock().unwrap().as_mut() {
									let _ = writeln!(si, "cursorhide");
								}
							}
							DataMsg::Ds5Output { slot, report } => {
								crate::ds5::client::on_output(id, slot, &report);
							}
							DataMsg::Ds5Mic { slot, on } => {
								crate::ds5::client::on_mic(id, slot, on);
							}
							DataMsg::Rumble { slot, large, small } => {
								// The host forwarded the game's force-feedback — hand it to the
								// gilrs reader thread to replay on the physical pad at `slot`.
								tracing::info!(slot, large, small, "rumble: client received → reader");
								let _ = rumble_tx.try_send((slot, large, small));
							}
							DataMsg::FsEntries { path, entries } => {
								// A host directory listing for the file panel's remote pane.
								let _ = app_ev.emit("fs-entries", FsEntriesPayload { id, path, entries });
							}
							DataMsg::FileBegin { id: xfer, name, size: _, chunks } => {
								// Evict stale entries (lost FileEnd / lost middle chunk) so a
								// long-lived session over a lossy link can't accumulate unbounded
								// dead reassemblers.
								let now = std::time::Instant::now();
								f_xfers.retain(|_, r| now.duration_since(r.last_activity) < F_XFER_IDLE_TIMEOUT);
								// Hard cap: if still at the concurrent limit, evict the
								// least-harmful entry — but not the lazy placeholder for THIS
								// id (don't evict the entry we're about to complete).
								// Prefer headless entries (expected == None; their FileBegin
								// was lost so they can never complete) over active ones;
								// tie-break by oldest last_activity.
								if f_xfers.len() >= F_MAX_CONCURRENT_XFERS && !f_xfers.contains_key(&xfer) {
									let victim = f_xfers
										.iter()
										.min_by_key(|(_, r)| (r.expected.is_some() as u8, r.last_activity))
										.map(|(k, _)| *k);
									if let Some(vid) = victim {
										f_xfers.remove(&vid);
									}
								}
								// If early FileChunks created a lazy entry (UDP reorder), merge
								// the name + expected into it to preserve buffered chunks.
								// Otherwise insert a fresh entry.
								let sane_name = crate::files::sanitize_filename(&name);
								if let Some(r) = f_xfers.get_mut(&xfer) {
									r.name = sane_name.clone();
									r.expected = Some(chunks);
									r.last_activity = now;
									// Prune any pre-buffered chunks whose index is now >= chunks.
									// They arrived before FileBegin (expected was None) so the
									// in_range guard passed them all through; now that we know the
									// count, out-of-range indices must be removed so they cannot
									// substitute for a genuinely lost in-range chunk.
									r.chunks.retain(|&idx, data| {
										if idx < chunks {
											true
										} else {
											r.received = r.received.saturating_sub(data.len() as u64);
											false
										}
									});
								} else {
									f_xfers.insert(xfer, FileReasm {
										name: sane_name.clone(),
										expected: Some(chunks),
										chunks: std::collections::BTreeMap::new(),
										received: 0,
										last_activity: now,
									});
								}
								// Notify the client UI that the host has started streaming
								// this file — the concurrency guard must NOT release the
								// wire slot on the short wall-clock timeout once a FileBegin
								// has been seen (the transfer is legitimately in flight).
								// xfer_id lets the UI associate this event with the exact
								// download() call that triggered it (keyed by id, not name),
								// preventing a timed-out same-name download from draining a
								// different in-flight slot (C21 fix).
								let _ = app_ev.emit("file-begin", crate::events::FileBeginPayload {
									peer: id.to_string(),
									name: sane_name,
									xfer_id: xfer,
								});
							}
							DataMsg::FileChunk { id: xfer, index, data } => {
								// If no entry exists yet (FileBegin hasn't arrived — UDP reorder),
								// create a lazy placeholder so the chunk is buffered rather than
								// dropped. FileBegin will fill in `name` and `expected` when it
								// arrives, keeping the already-stored chunks intact.
								if !f_xfers.contains_key(&xfer) {
									// Mirror the FileBegin guard: sweep idle entries and enforce
									// the concurrent cap before inserting a new headless entry.
									// Without this a chunk-only flood bypasses both the retain()
									// and the cap, allowing unbounded HashMap growth.
									let now = std::time::Instant::now();
									f_xfers.retain(|_, r| now.duration_since(r.last_activity) < F_XFER_IDLE_TIMEOUT);
									if f_xfers.len() >= F_MAX_CONCURRENT_XFERS {
										let victim = f_xfers
											.iter()
											.min_by_key(|(_, r)| (r.expected.is_some() as u8, r.last_activity))
											.map(|(k, _)| *k);
										if let Some(vid) = victim {
											f_xfers.remove(&vid);
										}
									}
									f_xfers.insert(xfer, FileReasm {
										name: String::new(),
										expected: None,
										chunks: std::collections::BTreeMap::new(),
										received: 0,
										last_activity: now,
									});
								}
								// Ignore an index past the announced count (bogus); a re-sent
								// index just overwrites with the identical bytes. Before FileBegin
								// (expected == None) we buffer all indices — the overflow check
								// runs once expected is known.
								let overflow = if let Some(r) = f_xfers.get_mut(&xfer) {
									r.last_activity = std::time::Instant::now();
									let in_range = r.expected.map_or(true, |e| index < e);
									if in_range {
										let prev_len = r.chunks.get(&index).map(|p| p.len() as u64).unwrap_or(0);
										let projected = r.received - prev_len + data.len() as u64;
										if projected > crate::files::MAX_XFER_BYTES {
											true
										} else {
											r.chunks.insert(index, data);
											r.received = projected;
											false
										}
									} else {
										false
									}
								} else {
									false
								};
								if overflow {
									// Peer is overshooting the sane transfer ceiling — drop the
									// whole transfer so the buffer can't grow unbounded. Emit
									// file-recv{ok:false} BEFORE removing so the download UI
									// learns the transfer was rejected instead of hanging forever
									// on 'indiriliyor…' with no feedback.
									if let Some(r) = f_xfers.remove(&xfer) {
										let _ = app_ev.emit("file-recv", FilePayload {
											peer: id.to_string(),
											name: r.name.clone(),
											bytes: 0,
											ok: false,
											xfer_id: xfer,
										});
									}
								}
							}
							DataMsg::FileEnd { id: xfer } => {
								// End the transfer: a repeated/stray FileEnd (no matching
								// Begin) must not re-save — `remove` drops the state so a
								// second End for the same id is a no-op.
								let Some(r) = f_xfers.remove(&xfer) else { continue };
								// Save only complete transfers (unordered UDP — a missing
								// index means a lost chunk; report a failed transfer instead
								// of corrupting). Reorders/dups already self-healed above.
								// `expected == Some(0)` is a legitimate empty file. If
								// `expected` is still `None`, FileBegin never arrived (lost
								// or extremely delayed); treat that as failed.
								// Contiguous check: len == e is not sufficient — an
								// out-of-range pre-Begin chunk (now pruned on FileBegin)
								// could otherwise substitute for a lost in-range chunk.
								let complete = r.expected.map_or(false, |e| {
									r.chunks.len() == e as usize
										&& (e == 0 || r.chunks.contains_key(&0) && r.chunks.contains_key(&(e - 1)))
										&& (0..e).all(|i| r.chunks.contains_key(&i))
								});
								// Offload the blocking disk write to a thread-pool thread so the
								// session loop (video RTP forwarding + input) is never stalled by a
								// multi-GiB file flush. Move the reassembly state into the closure;
								// clone the Tauri handles before spawning.
								let app_ev_file = app_ev.clone();
								let peer_str = id.to_string();
								let name_fallback = r.name.clone();
								let toast_peer = host_name.clone();
								tokio::task::spawn_blocking(move || {
									// Write chunks directly to disk without building a contiguous
									// intermediate Vec — avoids ~2x peak memory at the MAX_XFER_BYTES
									// ceiling (C24 fix).
									let saved = if complete {
										crate::files::save_received_file_chunks(
											&r.name,
											r.chunks.values(),
											r.received,
										)
									} else {
										None
									};
									let written = saved.as_ref().map(|(_, b)| *b).unwrap_or(0);
									// Pulsar's own toast: "file arrived", click = its folder.
									if let Some((p, _)) = saved.as_ref() {
										crate::toast::file_received(&app_ev_file, &toast_peer, p);
									}
									// Use the actual on-disk filename (may have a dedup suffix like
									// " (1)") so the UI flash names the file the user can actually open.
									let saved_name = saved
										.as_ref()
										.and_then(|(p, _)| p.file_name())
										.and_then(|n| n.to_str())
										.unwrap_or(&name_fallback)
										.to_string();
									let _ = app_ev_file.emit("file-recv", FilePayload {
										peer: peer_str,
										name: saved_name,
										bytes: written,
										ok: saved.is_some(),
										xfer_id: xfer,
									});
								});
							}
							_ => {}
						}
					} else if let Some(list) = pulsar_core::service::decode_windows(&bytes) {
						// Host reply to our QueryWindows (Bug 4): resolve the pending
						// window-list picker query from the inbound demux instead of a
						// blocking read-loop that consumed the interleaved media/RTP.
						if let Some((reply, _)) = pending_windows.take() {
							let _ = reply.send(list);
						}
					}
				}
				None => break, // host closed the session
			},
			// Keepalive + RTT probe (~PING_MS, uneven gaps), sent whether or not the last one was
			// answered: a lost Ping or Pong must not stall the RTT samples (and the repair window
			// they size). Pongs are matched to Pings by `PongMatcher`; one older than `PING_LOST`
			// is dropped.
			_ = &mut ping => {
				if send_keepalive(&mut sess).await.is_err() { break }
				let gap = pings.on_ping(std::time::Instant::now());
				ping.as_mut().reset(tokio::time::Instant::now() + gap);
			},
			// Repair sweep (REPAIR_SWEEP_MS): re-NACK once after ~1 RTT, then write the seq off
			// as a FAILED repair after the RTT-derived window — the decoder's reference chain
			// is broken until a keyframe / refresh, so (a) ask the host for an IDR now (the
			// MediaNack([0]) sentinel, ≤ 1/s) and (b) tell the renderer to HOLD its last good
			// frame for a bounded moment instead of showing the smear (Phase 0.5) — neither on
			// an intra-refreshed stream, which heals by itself.
			_ = repair.tick() => {
				// Fast reflex (Phase 3): a queue that keeps filling between windows cuts the
				// rate now, without waiting for the 2 s tick.
				if adapt_enabled {
					if let Some(d) = ctl.poll_fast(mono_ms()) {
						let mut changed = false;
						if let Some(kbps) = d.bitrate {
							cur_bitrate = kbps;
							changed = true;
						}
						if let Some(p) = d.point {
							// The controller's point already honours the user's picks (a pick is a
							// preferred maximum it only goes under when the link can't carry it).
							cur_w = p.width;
							cur_h = p.height;
							cur_fps = p.fps;
							last_switch_at = std::time::Instant::now();
							changed = true;
						}
						if changed {
							tracing::info!(
								kbps = cur_bitrate,
								point = %ctl.point().label(),
								reason = d.reason,
								"abr fast step"
							);
							if request_stream(&mut sess, &make_req!()).await.is_err() {
								break;
							}
						}
					}
				}
				// Opening phase: the renderer has decoded its first frames but stays hidden behind
				// the preparing screen while the controller finds the path's rate; reveal once it
				// has settled (no cut for 2 windows, rate + point steady, clean) or after 8 s at most.
				if !revealed
					&& crate::render_stats::HOLD_REVEAL_ID.load(std::sync::atomic::Ordering::SeqCst) == id
					&& crate::render_stats::FIRST_FRAMES_ID.load(std::sync::atomic::Ordering::SeqCst) == id
				{
					let t = *first_frames_at.get_or_insert_with(std::time::Instant::now);
					let settled = ctl.startup_settled();
					if settled || t.elapsed() >= std::time::Duration::from_secs(8) {
						revealed = true;
						{
							use std::io::Write as _;
							if let Some(si) = render_stdin.lock().unwrap().as_mut() {
								let _ = writeln!(si, "reveal");
								let _ = si.flush();
							}
						}
						let _ = app_ev.emit("play-ready", id);
						tracing::info!(
							settled,
							waited_ms = t.elapsed().as_millis() as u64,
							kbps = ctl.target_kbps(),
							point = %ctl.point().label(),
							"stream settled → revealed"
						);
					}
				}
				// The renderer fell back to waiting for a keyframe (a gap its reorder window could
				// not wait out, or a resync) while this loop may count the loss as repaired: ask now.
				if crate::render_stats::RENDER_WANTS_IDR.swap(false, std::sync::atomic::Ordering::Relaxed)
					&& host_nack
					&& kf_allowed(last_kfreq.elapsed(), kf_gap, rtt_ema)
				{
					kf_gap = kf_next_gap(last_kfreq.elapsed(), kf_gap);
					last_kfreq = std::time::Instant::now();
					let _ = send_data(&sess, &DataMsg::MediaNack(vec![0])).await;
					tracing::info!("renderer lost its reference picture → keyframe request");
				}
				if !missing.is_empty() {
					let now = std::time::Instant::now();
					let fec_hold = fec_active(last_parity_at.map(|t| t.elapsed()));
					let fec_hold_ms = if fec_hold { fec_wait_ms(cur_fps) } else { 0.0 };
					// Held-back NACKs whose FEC wait ran out while no packet came to send them —
					// all of them once the parity stopped (nothing will rebuild them).
					let nack_hold_ms = nack_wait_ms(fec_hold, cur_fps, reorder_peak_ms);
					let wait = std::time::Duration::from_secs_f32(nack_hold_ms / 1000.0);
					let mut renack = take_due_fec_nacks(&mut fec_pending, &mut missing, wait, now);
					let window = std::time::Duration::from_secs_f32(repair_window_ms(&rtt_recent, nack_hold_ms) / 1000.0);
					let rtt_med = median_ms(&rtt_recent, rtt_ema);
					let renack_after = std::time::Duration::from_secs_f32((rtt_med * 1.2 + 20.0).max(40.0) / 1000.0);
					let mut expired = 0u32;
					missing.retain(|&seq, e| {
						let age = now.duration_since(e.t0);
						if age >= window {
							expired += 1;
							false
						} else {
							// ~1 RTT after the NACK went out (a NACK held for FEC left later
							// than the gap showed).
							if !e.renacked && e.nacked.is_some_and(|t| now.duration_since(t) >= renack_after) {
								e.renacked = true;
								renack.push(seq);
							}
							true
						}
					});
					if host_nack && !renack.is_empty() {
						win_nack_sent += renack.len() as u32;
						let _ = send_data(&sess, &DataMsg::MediaNack(renack)).await;
					}
					if expired > 0 {
						// An intra-refreshed stream heals itself within its refresh period: no
						// keyframe (whose size spike is what IR avoids) and no renderer hold
						// either (see `loss_hold_ms`) — it keeps decoding and presenting the
						// concealed, healing picture. (If the renderer itself loses the thread it
						// still asks, via RENDER_WANTS_IDR.)
						let ir = cur_recovery == LossRecovery::IntraRefresh;
						// RFI: the renderer already reported the hole and recovers without one (or
						// asks for one itself when the host could not reach the lost frame).
						let rfi = rfi_reach(host_rfi, &cur_codec);
						if !ir
							&& !rfi && host_nack
							&& kf_allowed(last_kfreq.elapsed(), kf_gap.max(std::time::Duration::from_secs(1)), rtt_ema)
						{
							kf_gap = kf_next_gap(last_kfreq.elapsed(), kf_gap);
							last_kfreq = std::time::Instant::now();
							let _ = send_data(&sess, &DataMsg::MediaNack(vec![0])).await;
						}
						// Sent on every sweep that writes a packet off: the renderer arms one hold per
						// loss episode and treats the rest as that episode going on.
						let hold = loss_hold_ms(cur_recovery);
						if let Some(ms) = hold {
							use std::io::Write as _;
							if let Some(si) = render_stdin.lock().unwrap().as_mut() {
								let _ = writeln!(si, "hold {ms}");
							}
						}
						tracing::info!(
							expired,
							ir,
							hold = hold.is_some(),
							"unrepaired video loss → renderer hold + keyframe request (neither on intra-refresh; no keyframe on RFI)"
						);
					}
				}
			},
			// The renderer saw a hole (reference-frame invalidation mode): tell the host at once,
			// while it can still reach the lost frame — it invalidates it and tags its recovery
			// frame, so no keyframe is needed (the NACK repair keeps running meanwhile).
			_ = crate::render_stats::RFI_WAKE.notified() => {
				let rfi_lost = crate::render_stats::RENDER_WANTS_RFI.lock().unwrap().take();
				if let Some((first, last)) = rfi_lost {
					let _ = send_data(&sess, &DataMsg::MediaRfi { first, last }).await;
					tracing::debug!(first, last, "renderer hole → reference-frame invalidation request");
				}
			},
			_ = keep.tick() => {
				// Host silent too long (a kick / drop without a clean UDP close) → end the
				// session so the client tears down instead of freezing with input grabbed.
				// Media counts as liveness (a lossy link drops Pongs long before it drops the
				// stream); a control channel dead for longer still ends it (`host_silent`).
				if host_silent(last_inbound.elapsed(), last_media.elapsed()) {
					tracing::info!(
						control_silent_ms = last_inbound.elapsed().as_millis() as u64,
						media_silent_ms = last_media.elapsed().as_millis() as u64,
						"host silent → ending play session"
					);
					break;
				}
				let stopped = video_stopped(first_video_logged, last_video.elapsed(), last_inbound.elapsed());
				if stopped != told_stopped {
					told_stopped = stopped;
					tracing::info!(
						stopped,
						video_silent_ms = last_video.elapsed().as_millis() as u64,
						"host video stream {}",
						if stopped { "stopped" } else { "is back" }
					);
					if let Some(si) = render_stdin.lock().unwrap().as_mut() {
						use std::io::Write as _;
						let _ = writeln!(si, "stopped {}", stopped as u8);
						let _ = si.flush();
					}
				}
				// Notices from outside this loop (DualSense auto-degrade, link hint).
				for text in crate::play_notice::take(id) {
					tracing::info!(%text, "play notice");
					let mut native = false;
					if let Some(si) = render_stdin.lock().unwrap().as_mut() {
						use std::io::Write as _;
						native = true;
						let _ = writeln!(si, "toast {text}");
						let _ = si.flush();
					}
					if !native {
						let _ = app_ev.emit("play-notice", serde_json::json!({ "id": id, "text": text }));
					}
				}
				let live = sess.live_transport().await;
				// Relay → direct upgrade, or a direct path that died and fell back to the relay:
				// what the controller learned on the old path (a queue that collapsed it to the
				// floor, a relay-sized cap) does not describe the new one. Each path gets its own
				// cap (`path_cap_kbps`): the relay's forwarding limit only while relayed.
				let now_relay = matches!(live, pulsar_core::Transport::Relay);
				if now_relay != on_relay {
					on_relay = now_relay;
					ctl.path_changed(path_cap_kbps(on_relay, sess.rate_cap_kbps(), direct_cap_kbps));
					tracing::info!(
						to = if on_relay { "relay" } else { "direct" },
						cap_kbps = ctl.cap(),
						kbps = ctl.target_kbps(),
						"media path changed: the adaptive rate starts over on the new path"
					);
					// Onto the relay: a rate above its cap is dropped there from the next packet —
					// come under it now, not at the window's decision.
					if adapt_enabled && on_relay && ctl.encoder_kbps() < cur_bitrate {
						cur_bitrate = ctl.encoder_kbps();
						if request_stream(&mut sess, &make_req!()).await.is_err() {
							break;
						}
					}
				}
				// The path class the media runs on now (the transport may switch mid-session).
				// On a change, the last good rate learned so far belongs to the OLD path: mark it
				// as already remembered, so only a rate found clean on the new path is stored
				// under the new class.
				let class = crate::adapt_memory::PathClass::of(
					live,
					if matches!(live, pulsar_core::Transport::Direct) {
						sess.peer_addr().await.map(|a| a.ip())
					} else {
						None
					},
				);
				if class != cur_class {
					tracing::info!(from = cur_class.as_str(), to = class.as_str(), "media path class changed");
					cur_class = class;
					remembered_kbps = ctl.last_good_kbps();
				}
				// The real window length (the interval skips missed ticks, and a busy loop can run
				// a tick late): the controller turns `recv_bytes` into a delivered rate with it.
				let window_ms = win_start.elapsed().as_millis().min(u32::MAX as u128) as u32;
				win_start = std::time::Instant::now();
				// Field measurement, one line per window: what actually arrived (first copies),
				// what is still missing, and the delivered video rate vs the target — tells
				// congestion (goodput pinned under the target, loss rising with the rate) from
				// policing / MTU trouble (loss at any rate) in the next field log.
				tracing::info!(
					recv = win_recv,
					fresh = win_recv_fresh,
					reordered = win_reordered,
					reorder_wait_ms = reorder_wait_ms(reorder_peak_ms),
					lost = win_lost,
					recv_bytes = win_recv_bytes,
					window_ms,
					goodput_kbps = goodput_kbps(win_recv_bytes, window_ms),
					target = ctl.target_kbps(),
					kbps = cur_bitrate,
					point = %ctl.point().label(),
					path = cur_class.as_str(),
					"net window"
				);
				// The FEC share is learned from parity packets as they arrive, so a host that turns
				// parity OFF sends nothing that would say so: without this the controller kept
				// deducting the last share for the rest of the session (2026-11-09 LAN run: a
				// startup WAN guess turned 15 % parity on for one window, the host turned it off,
				// and the encoder sat at 12 of the 15 Mbit target for good).
				if !fec_active(last_parity_at.map(|t| t.elapsed())) && ctl.fec_overhead() > 0.0 {
					ctl.set_fec_overhead(0.0);
					tracing::info!("no parity from the host lately → FEC share released");
				}
				// ---- Adaptive controller (`pulsar_core::adapt`): one decision per 2 s window from
				// loss (seq gaps after NACK repair) + the window's RTT samples. Actuates a
				// bitrate step and/or the loss-recovery mode flip by re-requesting the stream
				// (live NVENC reconfigure on the native path; an encoder restart elsewhere).
				let sample = Sample {
					recv: win_recv,
					lost: win_lost,
					rtt_ms: std::mem::take(&mut rtt_samples),
					nack_sent: win_nack_sent,
					nack_ok: win_nack_ok,
					fec_ok: win_fec_ok,
					recv_bytes: win_recv_bytes,
					window_ms,
				};
				if last_switch_at.elapsed() < std::time::Duration::from_millis(2500) {
					// A recent menu switch / point change caused an encoder-rebuild gap — skip
					// this window so that loss isn't mistaken for congestion (counters are
					// zeroed below).
				} else if adapt_enabled {
					let mut d = ctl.tick(&sample);
					// Windows: the client's own decoder is the limit when it can't decode the
					// current fps in real time — step the fps down instead of freezing.
					if cfg!(windows) && d.point.is_none() {
						let dec_ms = f32::from_bits(
							crate::render_stats::DECODE_MS_BITS.load(std::sync::atomic::Ordering::Relaxed),
						);
						let cb = ctl.client_decode_bound(dec_ms);
						if let Some(p) = cb.point {
							tracing::info!(decode_ms = dec_ms, to = %p.label(), "client decoder can't keep up → lower fps");
							d.point = Some(p);
							d.reason = cb.reason;
						}
					}
					let sig = ctl.last();
					let mut rerequest = false;
					if let Some(r) = d.recovery {
						cur_recovery = r;
						rerequest = true;
					}
					if let Some(kbps) = d.bitrate {
						cur_bitrate = kbps;
						rerequest = true;
					}
					if let Some(p) = d.point {
						// The controller's point already honours the user's picks (a pick is a
						// preferred maximum it only goes under when the link can't carry it).
						cur_w = p.width;
						cur_h = p.height;
						cur_fps = p.fps;
						// The host rebuilds its encoder for a size/fps change: ignore the gap.
						last_switch_at = std::time::Instant::now();
						rerequest = true;
					}
					if d.is_change() {
						tracing::info!(
							loss_pct = sig.loss * 100.0,
							eff_pct = sig.eff_loss * 100.0,
							rtt = sig.rtt_ms,
							excess = sig.excess_ms,
							jitter = sig.jitter_ms,
							delay = ?sig.delay,
							trend = sig.trend,
							target = ctl.target_kbps(),
							kbps = cur_bitrate,
							point = %ctl.point().label(),
							ceiling = ctl.ceiling(),
							probe_wait = ctl.probe_wait(),
							noise_pct = ctl.noise_loss() * 100.0,
							fec_pct = ctl.fec_overhead() * 100.0,
							recovery = ?cur_recovery,
							user_cap = ctl.user_cap(),
							reason = d.reason,
							"abr step"
						);
					} else {
						tracing::debug!(
							loss_pct = sig.loss * 100.0,
							rtt = sig.rtt_ms,
							excess = sig.excess_ms,
							jitter = sig.jitter_ms,
							delay = ?sig.delay,
							trend = sig.trend,
							target = ctl.target_kbps(),
							kbps = cur_bitrate,
							point = %ctl.point().label(),
							ceiling = ctl.ceiling(),
							nack_sent = sample.nack_sent,
							nack_ok = sample.nack_ok,
							fec_ok = sample.fec_ok,
							"abr window"
						);
					}
					// Per-peer memory: the rate that stayed clean for 30 s seeds the next
					// session to this peer (60 % of it, then a fast startup probe).
					let good = ctl.last_good_kbps();
					if good > 0 && good != remembered_kbps {
						remembered_kbps = good;
						crate::adapt_memory::remember(&app_ev, &peer_key, cur_class, good);
					}
					// The saved picks are a preferred maximum: when the link can't carry them and
					// the controller runs a lighter point, tell the user once (renderer toast; the
					// webview gets a `play-notice` when there is no native renderer).
					let shortfall = ctl.pick_shortfall();
					let shortfall_key = shortfall.map(|(a, b)| (point_key(&a), point_key(&b)));
					if shortfall_due(shortfall_told, shortfall_told_at.map(|t| t.elapsed()), shortfall_key) {
						if let Some((picked, current)) = shortfall {
							shortfall_told = shortfall_key;
							shortfall_told_at = Some(std::time::Instant::now());
							let text = shortfall_text(&picked.label(), &current.label(), crate::i18n::lang() == "en");
							tracing::info!(picked = %picked.label(), current = %current.label(), "pick shortfall → notice");
							use std::io::Write as _;
							let mut native = false;
							if let Some(si) = render_stdin.lock().unwrap().as_mut() {
								native = true;
								let _ = writeln!(si, "toast {text}");
								let _ = si.flush();
							}
							if !native {
								let _ = app_ev.emit("play-notice", serde_json::json!({ "id": id, "text": text }));
							}
						}
					}
					// Client → host feedback (Phase 0.3 / 2.4): what we measured and what we
					// settled on, as JSON in the existing Stats side channel. Old hosts ignore
					// it; a new host sizes its FEC parity from the loss.
					let stats = ClientStats {
						v: ClientStats::VERSION,
						loss: sig.loss,
						rtt_ms: sig.rtt_ms,
						jitter_ms: sig.jitter_ms,
						kbps: cur_bitrate,
						recovery: cur_recovery,
						nack_sent: sample.nack_sent,
						nack_ok: sample.nack_ok,
						fec_ok: sample.fec_ok,
						// The controller's point even under user picks (a pick is a preferred
						// maximum it goes under when the link can't carry it): the host sizes its
						// FEC from the point's fps.
						point: ctl.point().label(),
					};
					let _ = send_data(&sess, &DataMsg::Stats(stats.to_json())).await;
					// Keep the renderer's resume mode in step with the stream's recovery mode
					// (every window: a respawned renderer starts in the safe wait-for-IDR mode).
					{
						use std::io::Write as _;
						if let Some(si) = render_stdin.lock().unwrap().as_mut() {
							let _ = writeln!(si, "ir {}", u8::from(cur_recovery == LossRecovery::IntraRefresh));
							// The stream was asked with `rfi` (deep reference buffer, see make_req!).
							// The host decides per hole whether its reference buffer still reaches
							// the lost frame, and sends a keyframe itself when it doesn't.
							let _ = writeln!(si, "rfi {}", u8::from(rfi_reach(host_rfi, &cur_codec)));
						}
					}
					if rerequest {
						if request_stream(&mut sess, &make_req!()).await.is_err() {
							break;
						}
					}
				}
				(win_recv, win_lost) = (0, 0);
				(win_nack_sent, win_nack_ok) = (0, 0);
				win_reordered = 0;
				// The peak decays so one burst of reordering doesn't slow every later NACK.
				reorder_peak_ms *= 0.8;
				win_fec_ok = 0;
				(win_recv_bytes, win_recv_fresh) = (0, 0);
			},
			q = restream_rx.recv() => match q {
				// Session menu changed the resolution or encoder: merge it into the
				// current state and re-request — the host kills the old ffmpeg and
				// restarts capture/encode with the new setting.
				Some(cmd) => {
					// A menu re-request (esp. a monitor switch) → the host rebuilds its encoder
					// (a brief gap). Mark it so the adaptive controller ignores that gap's loss.
					last_switch_at = std::time::Instant::now();
					match cmd {
						// An explicit size pins the ladder's resolution; (0, 0) = "Otomatik"
						// hands it back to the controller (which re-picks for its target).
						Restream::Resolution(w, h) => {
							manual_res = w != 0 && h != 0;
							if manual_res {
								cur_w = w;
								cur_h = h;
							}
							// The ladder keeps adapting the fps (and the rate) under a pinned size.
							ctl.set_pins(manual_res.then_some((cur_w, cur_h)), manual_fps.then_some(cur_fps));
							// With the controller running, a pick is a preferred maximum: stream its
							// point (lighter than the pick while the link can't carry it), as its
							// own decisions do.
							let p = ctl.point();
							if adapt_enabled || !manual_res {
								cur_w = p.width;
								cur_h = p.height;
							}
							if adapt_enabled || !manual_fps {
								cur_fps = p.fps;
							}
						}
						Restream::Encoder(e) => { cur_encoder = e; }
						Restream::Codec(c) => {
							// The rungs' minimum bitrates follow the codec's efficiency.
							ctl.set_codec(pulsar_core::adapt::codec_from_wire(&c));
							cur_codec = c;
						}
						Restream::Fps(f) => {
							manual_fps = f != 0;
							if manual_fps {
								cur_fps = f;
							}
							// The ladder keeps adapting the size (and the rate) under a pinned fps.
							ctl.set_pins(manual_res.then_some((cur_w, cur_h)), manual_fps.then_some(cur_fps));
							// A preferred maximum under the controller (see Resolution above).
							let p = ctl.point();
							if adapt_enabled || !manual_fps {
								cur_fps = p.fps;
							}
							if adapt_enabled || !manual_res {
								cur_w = p.width;
								cur_h = p.height;
							}
						}
						// A user-picked bandwidth limit (b > 0) is the adaptive controller's
						// CEILING, not a fixed rate: it keeps adapting under it (a 10 Mbit limit on a
						// 50 Mbit line still backs off when the line congests) and the ladder picks
						// the best point the limit buys. "Otomatik" (0) lifts it. A relayed
						// session's per-session cap still bounds it (the controller's own cap).
						Restream::Bitrate(b) => {
							ctl.set_user_cap(b);
							cur_bitrate = ctl.encoder_kbps();
							// Picks are a preferred maximum under the controller (see Resolution).
							let p = ctl.point();
							if adapt_enabled || !manual_res {
								cur_w = p.width;
								cur_h = p.height;
							}
							if adapt_enabled || (!manual_res && !manual_fps) {
								cur_fps = p.fps;
							}
							tracing::info!(limit_kbps = b, cap = ctl.cap(), rate = cur_bitrate, point = %ctl.point().label(), "bandwidth limit set");
						}
						Restream::Quality(q) => { cur_quality = q; }
						Restream::AudioQuality(pref) => {
							cur_audio_quality =
								crate::play::audio_quality_for(&pref, auto_audio_quality, host_audio_raw);
							tracing::info!(pref, quality = ?cur_audio_quality, "audio quality change requested");
						}
						Restream::Audio(t, m) => {
							tracing::info!(transmit = t, mute = m, "audio restream requested");
							cur_transmit = t;
							cur_mute = m;
						}
						Restream::Display(d) => {
							tracing::info!(display = d, "host monitor switch requested");
							cur_display = d;
							// Rebuild the ladder for the new monitor: its native size and the fps it
							// can capture at (its refresh, capped by the client's own target).
							if let Some(&(_, w, h, hz)) = host_displays.iter().find(|m| m.0 == d) {
								let fps = if hz > 1 { client_fps.min(crate::util::nearest_fps(hz)) } else { client_fps };
								ctl.set_native(w, h, fps);
								// Picks are a preferred maximum under the controller (see Resolution).
								let p = ctl.point();
								if adapt_enabled || !manual_res {
									cur_w = p.width;
									cur_h = p.height;
								}
								if adapt_enabled || !manual_fps {
									cur_fps = p.fps;
								}
								tracing::info!(display = d, native = %format!("{w}x{h}@{fps}"), point = %p.label(), "ladder rebuilt for the new monitor");
							}
							// Picking a monitor clears any per-window target: the two are
							// mutually exclusive (the host ignores display_idx when window_hwnd
							// is Some), so a Display switch means "go back to whole-monitor".
							cur_window = None;
						}
						Restream::Window(w) => {
							tracing::info!(hwnd = ?w, "per-window capture target requested");
							cur_window = w;
						}
						Restream::Adapt(a) => {
							tracing::info!(adapt = ?a, "screen adaptation toggle");
							cur_adapt = a;
						}
					}
					let req = make_req!();
					let rs = request_stream(&mut sess, &req).await;
					tracing::info!(display_idx = cur_display, ok = rs.is_ok(), "restream request_stream sent");
					if rs.is_err() {
						break;
					}
				}
				None => {}
			},
			// On-demand host-window-list query (Phase 2b co-op "window" capture picker, Bug 4):
			// FIRE the QueryWindows request and stash the reply-sender; the host's `Windows`
			// answer is resolved from the inbound demux above (out of the media/RTP racing on
			// this same session), NOT via a blocking read-loop whose 16-message budget the video
			// stream would eat. A still-pending prior query is superseded → answer it empty so its
			// caller never hangs. A send failure answers empty (picker shows no windows).
			q = windows_query_rx.recv() => match q {
				Some(reply) => {
					if let Some((old, _)) = pending_windows.take() {
						let _ = old.send(Vec::new());
					}
					if pulsar_core::service::send_query_windows(&sess).await.is_err() {
						let _ = reply.send(Vec::new());
					} else {
						pending_windows = Some((
							reply,
							tokio::time::Instant::now() + std::time::Duration::from_secs(3),
						));
					}
				}
				None => {}
			},
			// Fallback deadline for a pending window-list query the host never answered (old /
			// non-Windows host → no `Windows` reply): resolve it to an empty list so the picker
			// just shows no windows instead of the caller hanging. Disabled when nothing pends.
			_ = tokio::time::sleep_until(win_deadline.unwrap_or_else(tokio::time::Instant::now)),
				if win_deadline.is_some() =>
			{
				if let Some((reply, _)) = pending_windows.take() {
					let _ = reply.send(Vec::new());
				}
			},
		}
	}
	// Tell the host we're leaving so it tears down immediately (kills ffmpeg +
	// releases held input) instead of waiting out PEER_TIMEOUT.
	// Hold-loop ended (host closed the session, a network error, or we're leaving): tell the
	// frontend so it tears the session down — release the evdev/input grab + drop the tab —
	// instead of freezing on mpv's last frame with the keyboard/mouse still captured.
	let _ = app_ev.emit("play-ended", id);
	// The session's file-manager window (if open) dies with the session.
	crate::files_window::close(&app_ev, id);
	let _ = pulsar_core::service::send_bye(&mut sess).await;
	drop(sess);
}

#[cfg(test)]
mod reorder_tests {
	use super::*;

	#[test]
	fn a_late_original_is_reordering_not_a_repair() {
		let t0 = std::time::Instant::now();
		let ms = |n: u64| t0 + std::time::Duration::from_millis(n);
		// Never NACKed: it filled the gap by itself.
		assert!(!arrival_is_repair(None, ms(10), 8.0));
		// NACKed 2 ms ago on an 8 ms path: the retransmit can't be back yet — the original.
		assert!(!arrival_is_repair(Some(ms(0)), ms(2), 8.0));
		// NACKed a full RTT ago: the retransmit.
		assert!(arrival_is_repair(Some(ms(0)), ms(9), 8.0));
	}

	#[test]
	fn the_nack_waits_for_reordering_and_for_fec() {
		assert_eq!(nack_wait_ms(false, 60, 0.0), REORDER_WAIT_MIN_MS);
		assert_eq!(nack_wait_ms(false, 60, 8.0), 10.0);
		assert_eq!(nack_wait_ms(false, 60, 400.0), REORDER_WAIT_MAX_MS);
		// FEC's go is longer than a small reorder wait.
		assert_eq!(nack_wait_ms(true, 120, 2.0), fec_wait_ms(120));
	}
}

#[cfg(test)]
mod kf_tests {
	use super::*;
	use std::time::Duration;

	#[test]
	fn the_stream_stops_only_when_video_packets_stop_on_a_live_session() {
		let s = Duration::from_secs;
		// Packets still arriving (however mangled — a lossy link with no decodable AU): not stopped.
		assert!(!video_stopped(true, Duration::from_millis(300), s(1)));
		assert!(!video_stopped(true, s(4), s(1)));
		// No video datagram for 5 s while the host still answers: the host stopped sending.
		assert!(video_stopped(true, s(5), s(1)));
		// A session that never had video is still starting (screen picker, encoder start).
		assert!(!video_stopped(false, s(30), s(1)));
		// Control dead too: that is the session watchdog's call, not a "stopped" card.
		assert!(!video_stopped(true, s(30), s(6)));
	}

	#[test]
	fn media_keeps_the_session_alive_but_not_forever() {
		let s = Duration::from_secs;
		// Both flowing.
		assert!(!host_silent(s(1), Duration::ZERO));
		// Pongs lost on a lossy link while video still arrives: not silent...
		assert!(!host_silent(s(6), Duration::ZERO));
		assert!(!host_silent(s(14), Duration::from_millis(200)));
		// ...but a control channel dead for 15 s+ ends it even with live video.
		assert!(host_silent(s(16), Duration::ZERO));
		// Nothing at all for 5 s+ = dead link.
		assert!(host_silent(s(6), s(6)));
		// Control alive, media silent (host paused / choosing a screen): not silent.
		assert!(!host_silent(s(1), s(30)));
	}

	#[test]
	fn shortfall_notice_is_told_once_per_change() {
		let pick = (1920, 1080, 120);
		let a = Some((pick, (1920, 1080, 60)));
		let b = Some((pick, (1280, 720, 60)));
		let s = Duration::from_secs;
		// Nothing to tell.
		assert!(!shortfall_due(None, None, None));
		// First shortfall: told at once.
		assert!(shortfall_due(None, None, a));
		// Same one again: not repeated, however long ago.
		assert!(!shortfall_due(a, Some(s(1)), a));
		assert!(!shortfall_due(a, Some(s(600)), a));
		// A different point: told, but not within the gap of the last notice.
		assert!(!shortfall_due(a, Some(s(5)), b));
		assert!(shortfall_due(a, Some(SHORTFALL_NOTICE_GAP), b));
		// The link carries the pick again: nothing to tell.
		assert!(!shortfall_due(b, Some(s(60)), None));
	}

	#[test]
	fn shortfall_text_is_localized() {
		assert_eq!(shortfall_text("1080p120", "720p60", false), "Bağlantı 1080p120 taşımıyor — 720p60 kullanılıyor");
		assert_eq!(shortfall_text("1080p120", "720p60", true), "The connection can't carry 1080p120 — using 720p60");
	}

	#[test]
	fn goodput_from_window_bytes() {
		assert_eq!(goodput_kbps(250_000, 2000), 1000);
		assert_eq!(goodput_kbps(250_000, 0), 0);
	}

	#[test]
	fn keyframe_requests_back_off_during_a_loss_streak() {
		let ms = Duration::from_millis;
		// A lone loss is answered fast...
		assert!(kf_allowed(ms(600), KF_GAP_MIN, 20.0));
		// ...but not before ~2 RTT (the requested keyframe can't be here yet).
		assert!(!kf_allowed(ms(600), KF_GAP_MIN, 300.0));
		// Requests in a streak double the gap up to the cap.
		let mut gap = KF_GAP_MIN;
		for want in [1000, 2000, 4000, 4000] {
			gap = kf_next_gap(ms(600), gap);
			assert_eq!(gap, ms(want));
		}
		assert!(!kf_allowed(ms(3000), gap, 20.0));
		// A quiet spell resets it.
		assert_eq!(kf_next_gap(Duration::from_secs(6), gap), KF_GAP_MIN);
	}

	fn rtts(v: &[f32]) -> std::collections::VecDeque<f32> {
		v.iter().copied().collect()
	}

	#[test]
	fn repair_window_follows_median_rtt_and_jitter() {
		// No samples yet: the upper bound; the renderer then waits the full 250 ms.
		assert_eq!(repair_window_ms(&rtts(&[]), 0.0), REPAIR_MAX_MS);
		assert_eq!(REPAIR_MAX_MS + REPAIR_SWEEP_MS as f32, 250.0);
		// A steady 78 ms mobile path: median + slack, no jitter term.
		assert_eq!(repair_window_ms(&rtts(&[78.0; 8]), 0.0), 78.0 + REPAIR_SLACK_MS);
		// Jitter widens it: median 80, mean |dev| 8 → 80 + 2 × 8 + slack.
		let w = repair_window_ms(&rtts(&[70.0, 90.0, 70.0, 90.0, 80.0]), 0.0);
		assert!((w - (80.0 + 2.0 * 8.0 + REPAIR_SLACK_MS)).abs() < 0.01, "{w}");
		// The field link (avg 78, spikes to 248) no longer reaches ~600 ms: capped.
		let field = repair_window_ms(&rtts(&[60.0, 248.0, 75.0, 230.0, 90.0, 210.0, 70.0, 240.0]), 0.0);
		assert_eq!(field, REPAIR_MAX_MS);
		// A LAN: the lower bound (a retransmit still has room to land).
		assert_eq!(repair_window_ms(&rtts(&[2.0, 3.0, 2.0]), 0.0), REPAIR_MIN_MS);
		// NACKs held for FEC add their wait.
		assert_eq!(repair_window_ms(&rtts(&[50.0; 4]), 25.0), 50.0 + REPAIR_SLACK_MS + 25.0);
	}

	#[test]
	fn an_intra_refreshed_stream_gets_no_loss_hold() {
		// IR: keep decoding + presenting through the loss — the wave heals it.
		assert_eq!(loss_hold_ms(LossRecovery::IntraRefresh), None);
		// The keyframe-recovered modes keep the bounded hold.
		assert_eq!(loss_hold_ms(LossRecovery::Normal), Some(HOLD_MS));
		assert_eq!(loss_hold_ms(LossRecovery::ShortGop), Some(HOLD_MS));
	}

	#[test]
	fn rfi_is_the_hosts_call_at_any_rtt_and_fps() {
		// No RTT × fps gate any more: the host knows its reference buffer.
		assert!(rfi_reach(true, "h265"));
		assert!(rfi_reach(true, "h264"));
		// AV1 has no recovery tag; a host without RFI keeps the keyframe recovery.
		assert!(!rfi_reach(true, "av1"));
		assert!(!rfi_reach(false, "h265"));
	}

	#[test]
	fn pongs_match_the_right_ping() {
		let ms = Duration::from_millis;
		// Nothing outstanding: a stray Pong times nothing.
		assert_eq!(match_pong(&[], 80.0), None);
		// No sample yet: the oldest Ping.
		assert_eq!(match_pong(&[ms(120), ms(20)], 0.0), Some(0));
		// The previous Ping (or its Pong) was lost: time the Pong against the newer Ping, not
		// +500 ms against the lost one.
		assert_eq!(match_pong(&[ms(580), ms(80)], 78.0), Some(1));
		// A queue filling past PING_MS: the RTT keeps growing, the older Ping is the match.
		assert_eq!(match_pong(&[ms(560), ms(60)], 450.0), Some(0));
		// One outstanding: that one.
		assert_eq!(match_pong(&[ms(300)], 78.0), Some(0));
	}

	/// Drive a `PongMatcher` with the real ping cadence over a path whose RTT is `rtt(t_ms)`;
	/// `lost(i)` drops the round trip of Ping i. Returns (arrival ms, true RTT, reading).
	fn pong_run(secs: u64, rtt: impl Fn(u64) -> u64, lost: impl Fn(u64) -> bool) -> Vec<(u64, u64, f32)> {
		let base = std::time::Instant::now();
		let at = |ms: u64| base + Duration::from_millis(ms);
		let mut m = PongMatcher::new();
		// (time, is_pong, ping index)
		let mut events: Vec<(u64, bool, u64)> = Vec::new();
		let (mut t, mut i) = (0u64, 0u64);
		let mut gaps = PongMatcher::new();
		while t < secs * 1000 {
			events.push((t, false, i));
			if !lost(i) {
				events.push((t + rtt(t), true, i));
			}
			t += gaps.on_ping(at(0)).as_millis() as u64;
			i += 1;
		}
		events.sort();
		let mut sent_at = std::collections::HashMap::new();
		let mut out = Vec::new();
		for (t, pong, i) in events {
			if pong {
				if let Some(r) = m.on_pong(at(t)) {
					out.push((t, t - sent_at[&i], r));
				}
			} else {
				sent_at.insert(i, t);
				m.on_ping(at(t));
			}
		}
		out
	}

	#[test]
	fn ping_gaps_average_the_cadence_and_never_repeat() {
		let gaps: Vec<u64> = (1..=40).map(ping_gap_ms).collect();
		assert!(gaps.iter().all(|g| (350..=650).contains(g)), "{gaps:?}");
		let mean = gaps.iter().sum::<u64>() as f64 / gaps.len() as f64;
		assert!((mean - PING_MS as f64).abs() < 25.0, "mean {mean}");
		assert!(gaps.windows(2).all(|w| w[0] != w[1]), "uneven: {gaps:?}");
	}

	/// The RTT jumps from 78 ms to 600 ms and stays there: the readings follow it within a few
	/// Pongs instead of locking onto RTT − 500 (a fixed 500 ms cadence read a steady 100 ms).
	#[test]
	fn a_sustained_rtt_over_the_ping_cadence_is_read_right() {
		for jump in [600u64, 900, 1300] {
			let out = pong_run(40, |t| if t < 3000 { 78 } else { jump }, |_| false);
			let late: Vec<_> = out.iter().filter(|(t, _, _)| *t > 12_000).collect();
			assert!(late.len() > 40);
			let wrong = late.iter().filter(|(_, truth, r)| (*r - *truth as f32).abs() > 5.0).count();
			assert!(wrong == 0, "{jump} ms: {wrong} of {} wrong, e.g. {:?}", late.len(), &late[..6]);
		}
	}

	/// A short RTT with lost Pings: the Pong after a loss is timed against its own Ping, not
	/// +500 ms against the lost one; and a growing queue is followed.
	#[test]
	fn lost_pings_and_a_growing_queue_keep_the_readings_right() {
		let out = pong_run(40, |_| 78, |i| i % 3 == 1 || i % 7 == 0);
		// (The very first Pongs, before any sample, go to the oldest Ping as ever; Pings 0 and 1
		// are both lost here.)
		let wrong: Vec<_> =
			out.iter().filter(|(t, truth, r)| *t > 3000 && (*r - *truth as f32).abs() > 5.0).collect();
		assert!(wrong.is_empty(), "{} of {} wrong: {wrong:?}", wrong.len(), out.len());
		// 78 ms → 1 s over 20 s and back.
		let ramp = |t: u64| 78 + (t.saturating_sub(5000).min(20_000) * 922 / 20_000);
		let out = pong_run(40, ramp, |i| i % 5 == 2);
		let wrong = out.iter().filter(|(_, truth, r)| (*r - *truth as f32).abs() > 5.0).count();
		assert!(wrong * 10 <= out.len(), "{wrong} of {} wrong", out.len());
	}

	#[test]
	fn path_change_picks_the_cap_of_the_new_path() {
		// Onto the relay: its forwarding cap with 10 % headroom bounds the direct cap.
		assert_eq!(path_cap_kbps(true, 10_000, 40_000), 9_000);
		// …never under 2 Mbit, and never above what the direct cap allows.
		assert_eq!(path_cap_kbps(true, 1_000, 40_000), 2_000);
		assert_eq!(path_cap_kbps(true, 50_000, 15_000), 15_000);
		// An uncapped relay, or the direct path: the direct cap.
		assert_eq!(path_cap_kbps(true, 0, 40_000), 40_000);
		assert_eq!(path_cap_kbps(false, 10_000, 40_000), 40_000);
	}

	#[test]
	fn fec_wait_is_two_frames_bounded() {
		assert!((fec_wait_ms(120) - (2000.0 / 120.0 + 8.0)).abs() < 0.01);
		assert!((fec_wait_ms(0) - (2000.0 / 60.0 + 8.0)).abs() < 0.01, "host default = 60 fps");
		assert_eq!(fec_wait_ms(30), 50.0);
		assert_eq!(fec_wait_ms(1000), 15.0);
		// FEC counts as on while parity arrived within the last second.
		assert!(!fec_active(None));
		assert!(fec_active(Some(Duration::from_millis(300))));
		assert!(!fec_active(Some(Duration::from_secs(2))));
	}

	#[test]
	fn nack_waits_for_fec_until_it_gave_up() {
		// A block of 10 packets with 2 parity shards.
		assert!(!fec_block_gave_up(1, 2, 0), "first shard, more to come: wait");
		assert!(fec_block_gave_up(1, 2, 1), "last shard here, still missing: FEC is done");
		assert!(fec_block_gave_up(3, 2, 0), "3 missing > 2 parity: never repairable");
		assert!(!fec_block_gave_up(2, 2, 0));

		let t0 = std::time::Instant::now();
		let miss = |nacked: Option<std::time::Instant>| Miss { t0, nacked, renacked: false };
		let mut missing = std::collections::HashMap::new();
		// 101 and 103 held for FEC, 200 already NACKed (FEC was off when it went missing).
		missing.insert(101u16, miss(None));
		missing.insert(103u16, miss(None));
		missing.insert(200u16, miss(Some(t0)));
		assert_eq!(missing_in(&missing, 100, 10), 2);
		let mut pending: std::collections::VecDeque<(u16, std::time::Instant)> =
			[(101u16, t0), (103u16, t0)].into_iter().collect();
		let wait = Duration::from_millis(25);
		// Within the wait nothing is NACKed.
		assert!(take_due_fec_nacks(&mut pending, &mut missing, wait, t0 + Duration::from_millis(10)).is_empty());
		assert_eq!(pending.len(), 2);
		// The block's parity gave up on 103's block (100..105): only its held seqs go, once.
		let now = t0 + Duration::from_millis(12);
		assert_eq!(take_block_nacks(&mut missing, 100, 5, now), vec![101, 103]);
		assert!(take_block_nacks(&mut missing, 100, 5, now).is_empty(), "never twice");
		assert_eq!(missing[&103].nacked, Some(now));
		// A held seq FEC rebuilt meanwhile (gone from `missing`) is never NACKed; a due one is.
		let t1 = t0 + Duration::from_millis(30);
		missing.insert(300u16, miss(None));
		missing.insert(301u16, miss(None));
		missing.remove(&300);
		pending.clear();
		pending.extend([(300u16, t0), (301u16, t0)]);
		assert_eq!(take_due_fec_nacks(&mut pending, &mut missing, wait, t1), vec![301]);
		assert!(pending.is_empty());
		// FEC off (zero wait): every held NACK goes at once.
		missing.insert(400u16, miss(None));
		pending.push_back((400, t1));
		assert_eq!(take_due_fec_nacks(&mut pending, &mut missing, Duration::ZERO, t1), vec![400]);
	}

	#[test]
	fn rebuilt_seqs_are_remembered_bounded() {
		let mut r = std::collections::VecDeque::new();
		for s in 0..(FEC_REBUILT_KEEP as u16 + 10) {
			note_rebuilt(&mut r, s);
		}
		assert_eq!(r.len(), FEC_REBUILT_KEEP);
		assert!(!r.contains(&0));
		assert!(r.contains(&(FEC_REBUILT_KEEP as u16 + 9)));
	}
}
