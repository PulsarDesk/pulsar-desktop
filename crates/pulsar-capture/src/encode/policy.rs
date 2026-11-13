//! Encoder decisions that need no GPU: how deep the reference buffer is, whether an RFI request
//! can be met from it or needs a keyframe, when the scheduled keyframes fall, and the per-window
//! encode numbers. Kept apart from the NVENC FFI so they are unit-tested on any machine.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::Codec;

/// Reference frames kept when RFI is on, at least (one is used per frame; the rest are the
/// fallback a recovery frame predicts from). 8 was the depth the 2026-11-08 RFI work shipped
/// and measured with the Windows MFT decoder, so no stream gets a shallower buffer than that.
pub(crate) const RFI_DPB_MIN: u32 = 8;

/// The deepest reference buffer either codec allows (H.264 max_num_ref_frames and HEVC
/// sps_max_dec_pic_buffering both stop at 16). At 120 fps a 16-deep buffer reaches back 14
/// frames ≈ 117 ms: a hole reported over a 78 ms mobile RTT is still inside it, where the old 8
/// (6 frames ≈ 50 ms) was not, so every such loss used to cost a keyframe.
pub(crate) const RFI_DPB_MAX: u32 = 16;

/// Recent frames remembered for RFI. Far beyond `RFI_DPB_MAX`: a lost frame that is logged but
/// older than the buffer is known to be out of reach (→ keyframe), not mistaken for a stranger.
pub(crate) const REF_LOG_LEN: usize = 64;

/// Bits per pixel per frame below which the stream counts as low-budget. A 1080p120 stream
/// crosses it at ~25 Mbit/s, 1080p60 at ~12 Mbit/s, 720p60 at ~5.5 Mbit/s — LAN rates sit
/// above it, the mobile / WAN rates the field test ran at (0.3-15 Mbit/s) far below. A
/// low-budget stream gets the keyframe settings that keep it near its target (keyframe scale
/// 1, only one keyframe per fresh encoder — see `scheduled_idr`). The two-pass rate control
/// has its own, lower line: [`TWO_PASS_BPP`].
pub(crate) const LOW_BUDGET_BPP: f64 = 0.1;

/// Bits per pixel per frame below which the quarter-resolution first pass is worth what it
/// costs: 1080p120 / 1200p120 under ~10-11 Mbit/s, 1080p60 under ~5, 720p60 under ~2.2.
///
/// It used to follow [`LOW_BUDGET_BPP`]. But the remote-desktop bitrate ceiling sits at
/// ~0.06 bpp (16.8 Mbit/s at 1200p120), so every remote session on a LAN ran two-pass for
/// good — and the first pass is not free: measured on an RTX 3080 Laptop at 1200p120 HEVC
/// (2026-11-13) it kept the host process on 84 % of the GPU's 3D engine (0.7 % single-pass:
/// the driver downscales each frame there) and 18 % of a CPU core (5 %). That engine is the
/// one a game renders on. With the band of [`BUDGET_HYSTERESIS`] a stream that started thin
/// leaves the first pass at 0.05 bpp (13.8 Mbit/s at 1200p120) — under every ceiling the
/// client sets (≥ 0.060) with room to spare, so a session that hovers below its ceiling is
/// not left paying for it.
pub(crate) const TWO_PASS_BPP: f64 = 0.04;

/// Highest pixel rate (pixels/s) the two-pass rate control is used at: 1920×1200 at 120 fps.
/// The quarter-resolution first pass adds roughly a quarter of the motion search per frame;
/// below this rate an RTX-class NVENC has throughput to spare (P1 1080p runs at several
/// hundred fps), above it (1440p120, 4K) the extra pass could cost the frame rate itself.
pub(crate) const TWO_PASS_MAX_PIXEL_RATE: f64 = 1920.0 * 1200.0 * 120.0;

/// Bits per pixel per frame of a stream (`bps` bit/s at `w`×`h`@`fps`).
pub(crate) fn bits_per_pixel(bps: u32, w: u32, h: u32, fps: u32) -> f64 {
	let px = w.max(1) as f64 * h.max(1) as f64 * fps.max(1) as f64;
	bps as f64 / px
}

/// The stream is starved for bits (see `LOW_BUDGET_BPP`).
pub(crate) fn low_budget(bps: u32, w: u32, h: u32, fps: u32) -> bool {
	bits_per_pixel(bps, w, h, fps) < LOW_BUDGET_BPP
}

/// Band around `LOW_BUDGET_BPP` a live bitrate change must cross before the budget class
/// flips (×/÷ this), so an ABR that settles near the threshold does not toggle the
/// rate-control mode every window.
pub(crate) const BUDGET_HYSTERESIS: f64 = 1.25;

/// The budget class after a live bitrate change, from the one the encoder runs in. A build
/// judges the class from its starting rate; a cautious 4 Mbit start at 1080p120 is
/// low-budget, and the LAN it climbs to 30-50 Mbit on must not keep paying the first pass
/// and the 1×-P-frame keyframe cap for the rest of the session.
pub(crate) fn low_budget_live(was_low: bool, bps: u32, w: u32, h: u32, fps: u32) -> bool {
	let bpp = bits_per_pixel(bps, w, h, fps);
	if was_low {
		bpp < LOW_BUDGET_BPP * BUDGET_HYSTERESIS
	} else {
		bpp < LOW_BUDGET_BPP / BUDGET_HYSTERESIS
	}
}

/// The frame budget is thin enough for the first pass (see [`TWO_PASS_BPP`]), judged for a
/// fresh encoder.
pub(crate) fn thin_budget(bps: u32, w: u32, h: u32, fps: u32) -> bool {
	bits_per_pixel(bps, w, h, fps) < TWO_PASS_BPP
}

/// [`thin_budget`] after a live bitrate change, from the class the encoder runs in — the same
/// band as [`low_budget_live`], so a rate near the line does not toggle the first pass.
pub(crate) fn thin_budget_live(was_thin: bool, bps: u32, w: u32, h: u32, fps: u32) -> bool {
	let bpp = bits_per_pixel(bps, w, h, fps);
	if was_thin {
		bpp < TWO_PASS_BPP * BUDGET_HYSTERESIS
	} else {
		bpp < TWO_PASS_BPP / BUDGET_HYSTERESIS
	}
}

/// Use NVENC's quarter-resolution two-pass rate control (`NV_ENC_TWO_PASS_QUARTER_RESOLUTION`,
/// Sunshine's default) for this stream. The first pass measures the frame's complexity before
/// QPs are chosen, so a scene change does not blow through the one-frame VBV and the next
/// frames do not starve to pay it back — what matters when the frame budget is a few KB. It
/// adds no frame of delay (no lookahead) but costs encode time and GPU ([`TWO_PASS_BPP`]), so
/// only a thin stream gets it. `env` = `PULSAR_NVENC_MULTIPASS` ("0" never, "1" always).
pub(crate) fn use_two_pass(bps: u32, w: u32, h: u32, fps: u32, env: Option<&str>) -> bool {
	two_pass_for(thin_budget(bps, w, h, fps), w, h, fps, env)
}

/// `use_two_pass` for a stream whose budget is already judged (`thin`).
pub(crate) fn two_pass_for(thin: bool, w: u32, h: u32, fps: u32, env: Option<&str>) -> bool {
	match env.map(str::trim) {
		Some("0") => false,
		Some("1") => true,
		_ => thin && w as f64 * h as f64 * fps.max(1) as f64 <= TWO_PASS_MAX_PIXEL_RATE,
	}
}

/// The NVENC preset level (1 = fastest … 7 = slowest) for a stream. `env` =
/// `PULSAR_NVENC_PRESET` ("1".."7" forces one).
///
/// Game mode (`low_latency`): P1 — the picture changes every frame and the GPU belongs to the
/// game. Remote desktop leans on quality (text, a picture that sits still): P4 — while that
/// is cheap. The encode time grows with the pixels per second, the quality gain does not, so
/// above [`QUALITY_PRESET_MAX_PIXEL_RATE`] the remote stream takes [`REMOTE_FAST_PRESET`].
///
/// Measured on an RTX 3080 Laptop (2026-11-14, HEVC, CBR, single pass, `testsrc2`): the
/// encoder's utilisation at 1200p120 is 55 % with P4, 47 % P3, 39 % P2, 20 % P1 (26 / 23 /
/// 18 / 11 % at 1200p60; the tuning makes no difference), while P4 is ahead of P1 by 0.03 dB
/// PSNR at 16.8 Mbit/s and 0.04 dB at 8 Mbit/s there — and by a little more where it stays
/// (SSIM 0.9706 vs 0.9686 at 1080p30 / 2 Mbit/s).
pub(crate) fn preset_level(low_latency: bool, w: u32, h: u32, fps: u32, env: Option<&str>) -> u8 {
	if let Some(n) = env.and_then(|v| v.trim().parse::<u8>().ok()).filter(|n| (1..=7).contains(n)) {
		return n;
	}
	if low_latency {
		1
	} else if w as f64 * h as f64 * fps.max(1) as f64 <= QUALITY_PRESET_MAX_PIXEL_RATE {
		4
	} else {
		REMOTE_FAST_PRESET
	}
}

/// Highest pixel rate (pixels/s) a remote-desktop stream keeps the quality preset (P4) at:
/// 1920×1200 at 60 fps. See [`preset_level`].
pub(crate) const QUALITY_PRESET_MAX_PIXEL_RATE: f64 = 1920.0 * 1200.0 * 60.0;
/// The preset of a remote-desktop stream above that rate.
pub(crate) const REMOTE_FAST_PRESET: u8 = 1;

/// Reference-buffer depth for an RFI stream at `w`×`h`: as deep as level 5.1 allows at this
/// size (so the encoder does not have to raise the level — a level-6 stream is one a hardware
/// decoder may refuse), never below `RFI_DPB_MIN`, never above `RFI_DPB_MAX`. 1080p gets 16,
/// 1440p 12, 4K stays at 8.
pub(crate) fn rfi_dpb(codec: Codec, w: u32, h: u32) -> u32 {
	let by_level = match codec {
		// H.264 level 5.1: MaxDpbMbs 184320 macroblocks.
		Codec::H264 => {
			let mbs = (w.div_ceil(16) * h.div_ceil(16)).max(1);
			184_320 / mbs
		}
		// HEVC level 5.1: MaxLumaPs 8912896, maxDpbPicBuf 6 (spec A.4.2: the DPB grows as the
		// picture shrinks relative to the level's maximum).
		Codec::H265 => {
			let ps = w as u64 * h as u64;
			let max = 8_912_896u64;
			if ps <= max >> 2 {
				16
			} else if ps <= max >> 1 {
				12
			} else if ps <= (3 * max) >> 2 {
				8
			} else {
				6
			}
		}
		// No RFI on AV1 (the client's recovery tag is an H.264/HEVC SEI).
		Codec::Av1 => RFI_DPB_MIN,
	};
	by_level.clamp(RFI_DPB_MIN, RFI_DPB_MAX)
}

/// A scheduled keyframe falls on this frame of a fresh encoder: frame 0, every `interval`
/// frames (the safety GOP), and — only when `catch_up` — every ~250 ms in the first second, so
/// a client that missed frame 0's keyframe (demuxer reopen race, a lost packet) has another a
/// quarter second later. `catch_up` is off on a low-budget stream: there each keyframe is a
/// burst many frames' budget large on a link that is already full, four of them per (re)build
/// are what kept the field test's link congested, and the client asks for a keyframe itself
/// when it misses one.
pub(crate) fn scheduled_idr(frame_idx: u64, interval: u64, fps: u32, catch_up: bool) -> bool {
	if frame_idx % interval.max(1) == 0 {
		return true;
	}
	let fast_step = (fps / 4).max(1) as u64; // ~every 250 ms (fps-scaled)
	let fast_window = fps as u64; // first ~1 s after (re)start → ~4 keyframes
	catch_up && frame_idx < fast_window && frame_idx % fast_step == 0
}

/// One encoded frame, as RFI needs it.
#[derive(Clone, Copy, Debug)]
struct RefFrame {
	/// RTP timestamp its packets carried (what the client's lost range maps back to).
	rtp_ts: u32,
	/// NVENC input timestamp (what `nvEncInvalidateRefFrames` takes).
	pts: u64,
	/// Encode order (one per encoded frame — the unit the reference buffer slides by).
	seq: u64,
}

/// What to do with a client-reported loss.
///
/// There is no "force a keyframe" answer. The report is sent ~3 ms after the hole shows,
/// while NACK / FEC repair still runs, so a keyframe forced from it is paid for every hole
/// the repair then fills — at 35 % loss nearly every keyframe has one, so each answered
/// keyframe drew the next (~2 IDR/s on MF hosts, RFI-less NVENC and out-of-reach losses). A
/// loss RFI cannot heal is left to the client: a hole its repair could not fill leaves it
/// waiting for a recovery frame, and after `RFI_TIMEOUT` it asks for a keyframe
/// (`request_idr`, never held back).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RfiPlan {
	/// Invalidate these NVENC timestamps; the next frame is the tagged recovery frame.
	Invalidate(Vec<u64>),
	/// Nothing to do (the reason, for the log).
	Nothing(&'static str),
}

/// The frames the encoder produced lately and the reference buffer they sit in — decides
/// whether a lost frame can be invalidated (an older intact reference is still buffered) or
/// the client needs a keyframe. The host decides, because only it knows the buffer: the client
/// just reports the hole.
pub(crate) struct RefTracker {
	frames: VecDeque<RefFrame>,
	next_seq: u64,
	/// Encode order of the newest IDR (an IDR empties the reference buffer).
	last_idr: Option<u64>,
	/// Encode order of the oldest loss since the newest IDR that was NOT invalidated (out of
	/// reach, the IDR itself, the driver refused). Every frame after it may stand on a
	/// reference the client never decoded: a recovery frame for a LATER loss would be
	/// predicted from that chain, and the client — still waiting for a recovery point —
	/// would take it and show a corrupt picture until the next IDR. So while it is set a
	/// later loss is only invalidated together with it (when it is still in reach); otherwise
	/// nothing, and the client's keyframe request heals both. Cleared by the next IDR.
	unhealed: Option<u64>,
	/// Reference-buffer depth the encoder was configured with.
	dpb: u32,
}

impl RefTracker {
	pub(crate) fn new(dpb: u32) -> Self {
		Self {
			frames: VecDeque::with_capacity(REF_LOG_LEN),
			next_seq: 0,
			last_idr: None,
			unhealed: None,
			dpb: dpb.max(1),
		}
	}

	/// A frame was encoded and handed to the RTP sender.
	pub(crate) fn record(&mut self, rtp_ts: u32, pts: u64, idr: bool) {
		if self.frames.len() >= REF_LOG_LEN {
			self.frames.pop_front();
		}
		let seq = self.next_seq;
		self.next_seq += 1;
		self.frames.push_back(RefFrame { rtp_ts, pts, seq });
		if idr {
			self.last_idr = Some(seq);
			self.unhealed = None;
		}
	}

	/// A loss was not invalidated: remember the oldest such frame (see `unhealed`).
	fn decline(&mut self, seq: u64, why: &'static str) -> RfiPlan {
		self.unhealed = Some(self.unhealed.map_or(seq, |u| u.min(seq)));
		RfiPlan::Nothing(why)
	}

	/// The driver refused to invalidate `pts` (a plan's `Invalidate`): those losses stay
	/// unhealed.
	pub(crate) fn invalidate_failed(&mut self, pts: &[u64]) {
		if let Some(seq) = self.frames.iter().filter(|f| pts.contains(&f.pts)).map(|f| f.seq).min() {
			self.decline(seq, "");
		}
	}

	/// The frames carried by the lost packets (`lost_ts`, their RTP timestamps from the send
	/// log) — invalidate them, or nothing when the buffer no longer holds an intact reference
	/// older than all of them (and older than any earlier unhealed loss, see `unhealed`).
	pub(crate) fn plan(&mut self, lost_ts: &[u32]) -> RfiPlan {
		if lost_ts.is_empty() {
			// Older than the send log (seconds ago — the client has asked for a keyframe since)
			// or from a previous encoder (this one opened on a keyframe).
			return RfiPlan::Nothing("lost packets not in the send log");
		}
		let Some(newest) = self.frames.back().map(|f| f.seq) else {
			return RfiPlan::Nothing("nothing encoded yet");
		};
		let mut lost: Vec<RefFrame> = Vec::with_capacity(lost_ts.len());
		// In the send log but no longer in our frame log = older than every logged frame.
		let mut too_old = false;
		for ts in lost_ts {
			match self.frames.iter().find(|f| f.rtp_ts == *ts) {
				Some(f) => lost.push(*f),
				None => too_old = true,
			}
		}
		if let Some(idr) = self.last_idr {
			// The decoder restarts at the newest IDR: only losses after it matter. Losing the
			// IDR itself leaves nothing to predict from (it emptied the buffer).
			if lost.iter().any(|f| f.seq == idr) {
				return self.decline(idr, "the keyframe itself was lost");
			}
			lost.retain(|f| f.seq > idr);
			if self.frames.front().is_some_and(|f| f.seq <= idr) {
				too_old = false; // the unlogged frames predate the IDR
			}
		}
		if too_old {
			let front = self.frames.front().map_or(0, |f| f.seq);
			return self.decline(front, "lost frame already left the reference buffer");
		}
		let Some(oldest) = lost.iter().map(|f| f.seq).min() else {
			return RfiPlan::Nothing("a keyframe after the loss already heals it");
		};
		// An earlier loss left unhealed: the recovery frame must reach back past it too.
		let from = self.unhealed.map_or(oldest, |u| u.min(oldest));
		if self.last_idr.is_some_and(|idr| from <= idr) {
			return self.decline(from, "an earlier loss (the keyframe) is unhealed");
		}
		// The recovery frame (encoded next) needs the frame before the oldest lost one still in
		// the buffer, which then holds the last `dpb` frames up to `newest`.
		if newest - from + 2 > self.dpb as u64 {
			return self.decline(from, if from < oldest {
				"an earlier unhealed loss already left the reference buffer"
			} else {
				"lost frame already left the reference buffer"
			});
		}
		let mut pts: Vec<u64> = lost.iter().map(|f| f.pts).collect();
		// NVENC also invalidates every frame reconstructed from an invalidated one, so the
		// unhealed frame alone takes its whole chain out of the prediction.
		if from < oldest {
			if let Some(f) = self.frames.iter().find(|f| f.seq == from) {
				pts.insert(0, f.pts);
			}
		}
		self.unhealed = None;
		RfiPlan::Invalidate(pts)
	}
}

/// Interval of the encoder's window log line.
pub(crate) const ENC_WINDOW: Duration = Duration::from_secs(2);

/// Encode numbers over one `ENC_WINDOW`: tells a keyframe-driven overshoot (the field test's
/// "host sent 6× target") apart from FEC / retransmit overhead in the pacer's own window line.
#[derive(Default)]
pub(crate) struct EncWindow {
	start: Option<Instant>,
	frames: u32,
	bytes: u64,
	idrs: u32,
	idr_bytes: u64,
	idr_max: u32,
}

impl EncWindow {
	/// One encoded access unit of `size` bytes.
	pub(crate) fn add(&mut self, now: Instant, size: usize, idr: bool) {
		self.start.get_or_insert(now);
		self.frames += 1;
		self.bytes += size as u64;
		if idr {
			self.idrs += 1;
			self.idr_bytes += size as u64;
			self.idr_max = self.idr_max.max(size as u32);
		}
	}

	/// The window's log line once it is `ENC_WINDOW` old — only when it is worth reading (it
	/// held a keyframe, or the encoder ran > 25 % over `target_bps`), so a steady LAN stream
	/// logs nothing. The window restarts either way.
	pub(crate) fn take(&mut self, now: Instant, target_bps: u32, fps: u32) -> Option<String> {
		let start = self.start?;
		let el = now.saturating_duration_since(start);
		if el < ENC_WINDOW {
			return None;
		}
		let w = std::mem::take(self);
		let secs = el.as_secs_f64().max(0.001);
		let kbps = (w.bytes as f64 * 8.0 / secs / 1000.0) as u64;
		let target_kbps = target_bps as u64 / 1000;
		if w.idrs == 0 && kbps * 4 <= target_kbps * 5 {
			return None;
		}
		let budget = target_bps as u64 / 8 / fps.max(1) as u64;
		Some(format!(
			"nvenc window {:.1}s: {} frames {} B = {kbps} kbit/s (target {target_kbps}, {budget} B/frame) \
			 idr {} ({} B, max {} B)",
			secs, w.frames, w.bytes, w.idrs, w.idr_bytes, w.idr_max
		))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn tracker_with(dpb: u32, n: u32, idr_at: &[u32]) -> RefTracker {
		let mut t = RefTracker::new(dpb);
		for i in 0..n {
			// rtp_ts = 1000 + i, pts = 500 + i: distinct from seq so a mix-up shows.
			t.record(1000 + i, 500 + i as u64, idr_at.contains(&i));
		}
		t
	}

	#[test]
	fn dpb_follows_level_limits() {
		assert_eq!(rfi_dpb(Codec::H265, 1920, 1080), 16);
		assert_eq!(rfi_dpb(Codec::H264, 1920, 1080), 16);
		assert_eq!(rfi_dpb(Codec::H265, 2560, 1440), 12);
		assert_eq!(rfi_dpb(Codec::H264, 2560, 1440), 12);
		// 4K: level 5.1 would allow fewer than the proven 8 — keep 8, as before.
		assert_eq!(rfi_dpb(Codec::H265, 3840, 2160), RFI_DPB_MIN);
		assert_eq!(rfi_dpb(Codec::H264, 3840, 2160), RFI_DPB_MIN);
		assert_eq!(rfi_dpb(Codec::H265, 1280, 720), RFI_DPB_MAX);
		// Portrait (rotated host) has the same pixel count.
		assert_eq!(rfi_dpb(Codec::H265, 1080, 1920), 16);
	}

	#[test]
	fn reachable_loss_is_invalidated() {
		// 16-deep buffer, 30 frames, the IDR at 0. Frame 20 lost, newest 29: 29-20+2 = 11 ≤ 16.
		let mut t = tracker_with(16, 30, &[0]);
		assert_eq!(t.plan(&[1020]), RfiPlan::Invalidate(vec![520]));
		// Two frames of one hole: both invalidated.
		assert_eq!(t.plan(&[1021, 1022]), RfiPlan::Invalidate(vec![521, 522]));
	}

	#[test]
	fn reach_edge_matches_the_buffer() {
		// newest 29; with dpb 16 the oldest reachable lost frame is 15 (29-15+2 = 16).
		let mut t = tracker_with(16, 30, &[0]);
		assert_eq!(t.plan(&[1015]), RfiPlan::Invalidate(vec![515]));
		let mut t = tracker_with(16, 30, &[0]);
		assert!(matches!(t.plan(&[1014]), RfiPlan::Nothing(_)));
		// The old 8-deep buffer at 120 fps reached 6 frames (50 ms) — a 78 ms RTT (~10 frames)
		// was out of reach; 16 reaches it.
		let mut t8 = tracker_with(8, 30, &[0]);
		assert!(matches!(t8.plan(&[1019]), RfiPlan::Nothing(_)));
		let mut t16 = tracker_with(16, 30, &[0]);
		assert_eq!(t16.plan(&[1019]), RfiPlan::Invalidate(vec![519]));
	}

	/// No keyframe is ever forced from the (pre-repair) report: a hole the repair fills must
	/// cost nothing, one it can't fill is the client's keyframe request.
	#[test]
	fn a_declined_loss_forces_no_keyframe() {
		let mut t = tracker_with(16, 60, &[0]);
		assert!(matches!(t.plan(&[1010]), RfiPlan::Nothing(_)), "out of reach");
		let mut t = tracker_with(16, 30, &[0, 25]);
		assert!(matches!(t.plan(&[1025, 1026]), RfiPlan::Nothing(_)), "the IDR itself");
	}

	#[test]
	fn loss_before_the_newest_idr_is_already_healed() {
		// IDR at frame 25 (a forced one); a late report for frame 20 needs nothing.
		let mut t = tracker_with(16, 30, &[0, 25]);
		assert!(matches!(t.plan(&[1020]), RfiPlan::Nothing(_)));
		// A hole spanning the IDR's predecessors and a later frame: only the later one counts.
		assert_eq!(t.plan(&[1024, 1027]), RfiPlan::Invalidate(vec![527]));
	}

	/// The lost IDR stays unhealed: a later reachable loss must not get a recovery frame (it
	/// would be predicted from frames the client never decoded) — until the next IDR.
	#[test]
	fn a_lost_idr_blocks_rfi_for_later_losses_until_the_next_idr() {
		let mut t = tracker_with(16, 30, &[0, 25]);
		assert!(matches!(t.plan(&[1025]), RfiPlan::Nothing(_)));
		t.record(1030, 530, false);
		t.record(1031, 531, false);
		assert!(matches!(t.plan(&[1030]), RfiPlan::Nothing(_)), "chain from the lost IDR");
		// The client's keyframe request: a fresh IDR clears it.
		t.record(1032, 532, true);
		for i in 33..40 {
			t.record(1000 + i, 500 + i as u64, false);
		}
		assert_eq!(t.plan(&[1035]), RfiPlan::Invalidate(vec![535]));
	}

	/// An out-of-reach loss also blocks later recoveries; an unhealed loss still in reach is
	/// invalidated together with the later one.
	#[test]
	fn an_unhealed_loss_is_reached_back_to_or_blocks() {
		let mut t = tracker_with(16, 60, &[0]);
		assert!(matches!(t.plan(&[1010]), RfiPlan::Nothing(_)));
		assert!(matches!(t.plan(&[1058]), RfiPlan::Nothing(_)), "frame 10 still unhealed");
		// Driver refusal on a reachable loss: the next loss reaches back to it.
		let mut t = tracker_with(16, 30, &[0]);
		assert_eq!(t.plan(&[1020]), RfiPlan::Invalidate(vec![520]));
		t.invalidate_failed(&[520]);
		t.record(1030, 530, false);
		assert_eq!(t.plan(&[1027]), RfiPlan::Invalidate(vec![520, 527]));
		// …and that invalidation healed it.
		t.record(1031, 531, false);
		assert_eq!(t.plan(&[1029]), RfiPlan::Invalidate(vec![529]));
	}

	#[test]
	fn unknown_frames() {
		let mut t = tracker_with(16, 30, &[0]);
		assert!(matches!(t.plan(&[]), RfiPlan::Nothing(_)));
		// 100 frames: the log keeps the last 64 (36..99), the IDR at 0 fell out. A send-log
		// frame older than the log is out of reach.
		let mut t = tracker_with(16, 100, &[0]);
		assert!(matches!(t.plan(&[1010]), RfiPlan::Nothing(_)));
		assert_eq!(t.unhealed, Some(36));
		// …unless a logged IDR is newer than it: the decoder restarted there.
		let mut t = tracker_with(16, 100, &[0, 90]);
		assert!(matches!(t.plan(&[1010]), RfiPlan::Nothing(_)));
		assert_eq!(t.unhealed, None, "healed by the IDR, not unhealed");
		let mut empty = RefTracker::new(16);
		assert!(matches!(empty.plan(&[5]), RfiPlan::Nothing(_)));
	}

	#[test]
	fn keyframe_schedule() {
		// Generous budget: frame 0 plus the ~250 ms catch-ups in the first second.
		let idrs: Vec<u64> = (0..240).filter(|&i| scheduled_idr(i, 1200, 120, true)).collect();
		assert_eq!(idrs, vec![0, 30, 60, 90]);
		// Low budget: one keyframe per fresh encoder.
		let idrs: Vec<u64> = (0..240).filter(|&i| scheduled_idr(i, 1200, 120, false)).collect();
		assert_eq!(idrs, vec![0]);
		// The safety GOP fires either way.
		assert!(scheduled_idr(1200, 1200, 120, false));
		assert!(scheduled_idr(2400, 1200, 120, true));
	}

	#[test]
	fn budget_classes() {
		// LAN: 1080p120 at 50 Mbit/s, 1080p60 at 20 Mbit/s — not low-budget, single pass.
		assert!(!low_budget(50_000_000, 1920, 1080, 120));
		assert!(!low_budget(20_000_000, 1920, 1080, 60));
		assert!(!use_two_pass(50_000_000, 1920, 1080, 120, None));
		// Field test: 1080p120 at 4 Mbit/s and 15 Mbit/s — low-budget (keyframe settings)…
		assert!(low_budget(4_000_000, 1920, 1080, 120));
		assert!(low_budget(15_000_000, 1920, 1080, 120));
		// …and the first pass only where the frame budget is really thin.
		assert!(use_two_pass(4_000_000, 1920, 1080, 120, None));
		assert!(use_two_pass(3_000_000, 1920, 1080, 60, None));
		assert!(use_two_pass(2_000_000, 1280, 720, 60, None));
		assert!(!use_two_pass(15_000_000, 1920, 1080, 120, None));
		// The remote-desktop ceilings (2026-11-13: 16.8 Mbit at 1200p120 ran two-pass for good
		// and kept the host on 84 % of the GPU's 3D engine).
		assert!(!use_two_pass(16_817_000, 1920, 1200, 120, None));
		assert!(!use_two_pass(15_136_000, 1920, 1080, 120, None));
		assert!(!use_two_pass(15_000_000, 1920, 1080, 60, None));
		// 4K120 at 40 Mbit/s is low-budget but too many pixels for the extra pass.
		assert!(low_budget(40_000_000, 3840, 2160, 120));
		assert!(!use_two_pass(8_000_000, 3840, 2160, 120, None));
		// The env knob wins both ways.
		assert!(!use_two_pass(4_000_000, 1920, 1080, 120, Some("0")));
		assert!(use_two_pass(50_000_000, 1920, 1080, 120, Some("1")));
		assert!(use_two_pass(4_000_000, 1920, 1080, 120, Some("auto")));
	}

	/// A stream that started thin (a cautious start, a bad probe) leaves the first pass on its
	/// way up — below the ceiling, or it would keep paying for it all session.
	#[test]
	fn the_first_pass_ends_under_every_ceiling() {
		let (w, h, fps) = (1920, 1200, 120);
		assert!(thin_budget(8_857_000, w, h, fps), "a thin start");
		assert!(thin_budget_live(true, 12_000_000, w, h, fps), "inside the band: stays");
		assert!(!thin_budget_live(true, 16_817_000, w, h, fps), "the remote ceiling: off");
		assert!(!thin_budget_live(false, 11_000_000, w, h, fps), "inside the band: stays off");
		assert!(thin_budget_live(false, 8_000_000, w, h, fps), "a thin link: on again");
		// Game mode's ceiling at the same size is far above.
		assert!(!thin_budget_live(true, 28_000_000, w, h, fps));
	}

	#[test]
	fn the_preset_follows_the_mode_and_the_pixel_rate() {
		// Game: the fastest, whatever the size.
		assert_eq!(preset_level(true, 1920, 1080, 60, None), 1);
		assert_eq!(preset_level(true, 3840, 2160, 120, None), 1);
		// Remote: the quality preset while it is cheap…
		assert_eq!(preset_level(false, 1920, 1080, 30, None), 4);
		assert_eq!(preset_level(false, 1920, 1200, 60, None), 4);
		assert_eq!(preset_level(false, 2560, 1440, 30, None), 4);
		// …a fast one where every frame costs the most.
		assert_eq!(preset_level(false, 1920, 1200, 120, None), REMOTE_FAST_PRESET);
		assert_eq!(preset_level(false, 1920, 1080, 90, None), REMOTE_FAST_PRESET);
		assert_eq!(preset_level(false, 3840, 2160, 60, None), REMOTE_FAST_PRESET);
		// The env knob.
		assert_eq!(preset_level(false, 1920, 1200, 120, Some("4")), 4);
		assert_eq!(preset_level(true, 1920, 1080, 60, Some(" 3 ")), 3);
		assert_eq!(preset_level(true, 1920, 1080, 60, Some("9")), 1, "out of range: ignored");
		assert_eq!(preset_level(false, 1920, 1080, 60, Some("auto")), 4);
	}

	/// A live bitrate change re-judges the class with hysteresis: the cautious-start build
	/// (1080p120 at 4 Mbit, low-budget) leaves it at a LAN rate, not just over the line.
	#[test]
	fn budget_class_follows_the_live_rate_with_hysteresis() {
		let (w, h, fps) = (1920, 1080, 120);
		// The threshold is ~24.9 Mbit at 1080p120.
		assert!(low_budget_live(true, 4_000_000, w, h, fps));
		assert!(low_budget_live(true, 28_000_000, w, h, fps), "inside the band: stays low");
		assert!(!low_budget_live(true, 33_600_000, w, h, fps), "the LAN cap: leaves it");
		assert!(!low_budget_live(false, 22_000_000, w, h, fps), "inside the band: stays LAN");
		assert!(low_budget_live(false, 15_000_000, w, h, fps), "a WAN rate: low again");
		assert!(two_pass_for(true, w, h, fps, None));
		assert!(!two_pass_for(false, w, h, fps, None));
		assert!(!two_pass_for(true, 3840, 2160, 120, None), "too many pixels");
		assert!(two_pass_for(false, w, h, fps, Some("1")));
	}

	#[test]
	fn window_logs_only_when_worth_it() {
		let t0 = Instant::now();
		let mut w = EncWindow::default();
		// Steady stream at target (10 Mbit/s, 120 fps ≈ 10416 B/frame): no line.
		for i in 0..240u32 {
			w.add(t0 + Duration::from_micros(i as u64 * 8333), 10_416, false);
		}
		assert!(w.take(t0 + Duration::from_millis(1000), 10_000_000, 120).is_none(), "not due yet");
		assert!(w.take(t0 + ENC_WINDOW, 10_000_000, 120).is_none());
		// A window with a keyframe: logged, with its size.
		w.add(t0 + ENC_WINDOW, 40_000, true);
		w.add(t0 + ENC_WINDOW, 300, false);
		let line = w.take(t0 + ENC_WINDOW * 2, 300_000, 120).expect("keyframe window logs");
		assert!(line.contains("idr 1 (40000 B, max 40000 B)"), "{line}");
		// Overshoot without a keyframe: logged too.
		for _ in 0..240 {
			w.add(t0, 2_000, false);
		}
		assert!(w.take(t0 + ENC_WINDOW, 300_000, 120).is_some());
	}
}
