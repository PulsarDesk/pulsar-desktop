//! Pure decisions of the in-process libav host (`libav.rs`, Linux X11) for its VA-API / QSV
//! encoders. No libav types here, so the rules are unit-tested on every OS; only Linux runs
//! the engine that uses them.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use pulsar_core::pipeline::HwEncoder;

/// How a running libavcodec encoder takes a new bitrate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RateChange {
	/// Writing `bit_rate` / `rc_max_rate` / `rc_buffer_size` between frames is enough: libx264,
	/// libx265, SVT-AV1 and libavcodec NVENC reconfigure in place, and QSV does from
	/// libavcodec 60 (FFmpeg 6.0).
	Live,
	/// The encoder reads its rate control once, at open: `*_vaapi` (every FFmpeg so far) and
	/// QSV before FFmpeg 6.0. The step re-opens the encoder on the SAME capture, filter graph,
	/// GPU surfaces and RTP muxer (same SSRC, sequence and timestamps), so the client sees one
	/// new IDR instead of a restarted ffmpeg (x11grab reopen, ~1 s of no video).
	Reopen,
}

/// See [`RateChange`]. `lavc_major` = `avcodec_version() >> 16`.
pub(crate) fn rate_change(enc: HwEncoder, lavc_major: u32) -> RateChange {
	match enc {
		HwEncoder::Vaapi => RateChange::Reopen,
		HwEncoder::Qsv if lavc_major < 60 => RateChange::Reopen,
		_ => RateChange::Live,
	}
}

/// A pending bitrate for a [`RateChange::Reopen`] encoder: apply it now?
///
/// A re-open costs an IDR, so it waits for a frame that is an IDR anyway (`key_due`: a
/// client's keyframe request, the recovery cadence, or the encoder's own GOP boundary —
/// every 0.25 s in game mode, ≤ 2 s on the desktop). A cut of 25 % or more does not wait:
/// that step answers congestion, and holding the old rate for up to a GOP keeps the queue
/// growing. Upward steps always wait (the link is fine; no reason to spend an extra IDR).
pub(crate) fn reopen_now(cur_kbps: u32, want_kbps: u32, key_due: bool) -> bool {
	if want_kbps == cur_kbps || want_kbps == 0 {
		return false;
	}
	key_due || (want_kbps as u64) * 4 <= (cur_kbps as u64) * 3
}

/// The rate limits `(rc_max_rate, rc_buffer_size)` (bits/s, bits) for `bit_rate` (bits/s).
///
/// * Software / NVENC (unchanged): peak = target, ~0.5 s VBV — low latency, no spikes.
/// * VA-API / QSV, desktop: the CLI's rate control, which leans on burst headroom for a sharp
///   frame on a scene change / scroll / window switch — the CLI sets only `-b:v`, so
///   `vaapi_encode` runs VBR with a 2x peak and a 1 s HRD buffer, and QSV runs VBR/AVBR. A peak
///   equal to the target would make `*_vaapi` VBR (rc_target_percentage 100 %) and QSV CBR.
/// * VA-API / QSV, game: CBR (peak = target) with the ~0.5 s VBV the in-process NVENC / x264
///   keep. `short_vbv` (`PULSAR_LIBAV_SHORT_VBV=1`) takes ~125 ms instead: a frame never bursts
///   far past the link's rate, but every IDR of the 0.25 s GOP is capped near 5-6 average
///   frames — the IDR size / QP it costs is not measured on radeonsi / iHD yet, so opt-in.
pub(crate) fn rate_limits(bit_rate: i64, enc: HwEncoder, low_latency: bool, short_vbv: bool) -> (i64, i64) {
	let bit_rate = bit_rate.max(1);
	let hw = matches!(enc, HwEncoder::Vaapi | HwEncoder::Qsv);
	match (hw, low_latency) {
		(true, false) => (bit_rate * 2, bit_rate),
		(true, true) if short_vbv => (bit_rate, (bit_rate / 8).max(1)),
		_ => (bit_rate, (bit_rate / 2).max(1)),
	}
}

/// `PULSAR_LIBAV_SHORT_VBV` = `1` / `true` / `on`: the ~125 ms VBV for VA-API / QSV game
/// streams ([`rate_limits`]).
pub(crate) fn short_vbv_enabled(env: Option<&str>) -> bool {
	matches!(env.map(str::trim), Some("1") | Some("true") | Some("on"))
}

/// Watches a VA-API / QSV encode that started fine and then stopped working — the encoder
/// rejecting every frame, or taking frames and handing back no packets — so the engine can
/// end and the stream move to the ffmpeg CLI chain instead of staying alive with no video
/// (an alive engine takes every later request on its fast path). Before it only setup
/// failures fell back. Pure: the caller passes the clock.
#[derive(Debug)]
pub(crate) struct HwGuard {
	rejects: u32,
	sent_since_out: u32,
	last_out: std::time::Instant,
}

impl HwGuard {
	/// Consecutive frames the encoder refused before the engine gives up (~0.5 s at 60 fps).
	pub(crate) const MAX_REJECTS: u32 = 30;
	/// How long frames may go in with no packet coming out.
	pub(crate) const MAX_SILENCE: std::time::Duration = std::time::Duration::from_secs(3);

	pub(crate) fn new(now: std::time::Instant) -> Self {
		Self { rejects: 0, sent_since_out: 0, last_out: now }
	}

	/// A frame was handed to the encoder (`accepted` = `avcodec_send_frame` took it).
	pub(crate) fn sent(&mut self, accepted: bool) {
		if accepted {
			self.rejects = 0;
			self.sent_since_out = self.sent_since_out.saturating_add(1);
		} else {
			self.rejects = self.rejects.saturating_add(1);
		}
	}

	/// Packets written after a frame (`bytes` > 0 = the encoder works).
	pub(crate) fn output(&mut self, bytes: u64, now: std::time::Instant) {
		if bytes > 0 {
			self.sent_since_out = 0;
			self.last_out = now;
		}
	}

	/// The encoder was re-opened: a fresh start for the silence clock.
	pub(crate) fn reset(&mut self, now: std::time::Instant) {
		*self = Self::new(now);
	}

	/// `Some(why)` once the encode has failed for good.
	pub(crate) fn verdict(&self, now: std::time::Instant) -> Option<String> {
		if self.rejects >= Self::MAX_REJECTS {
			return Some(format!("the encoder rejected {} frames in a row", self.rejects));
		}
		if self.sent_since_out > 0 && now.duration_since(self.last_out) >= Self::MAX_SILENCE {
			return Some(format!(
				"{} frames in, no packet out for {} s",
				self.sent_since_out,
				Self::MAX_SILENCE.as_secs()
			));
		}
		None
	}
}

/// The CPU `scale` of x11grab's RGB into the encoder's YUV with the BT.709 matrix, limited
/// range — what every Pulsar client decodes an untagged stream as (pulsar-render's shader and
/// the macOS decoder assume BT.709). swscale's default would be BT.601 (a hue shift on reds
/// and greens). The VA-API / QSV encoders tag their VUI BT.709 to match.
pub(crate) fn bt709_scale(w: u32, h: u32) -> String {
	format!("scale={w}:{h}:flags=fast_bilinear:out_color_matrix=bt709:out_range=tv")
}

/// The VA-API filter chain behind `buffer` (x11grab's `bgr0`) ending in GPU NV12 surfaces.
/// Both chains convert with BT.709 limited range ([`bt709_scale`]).
///
/// * default — CPU `scale` + `format=nv12`, then `hwupload` (the ffmpeg CLI's VA-API chain,
///   with the matrix the client decodes instead of swscale's BT.601 default).
/// * `vpp` (`PULSAR_VAAPI_VPP=1`) — upload the RGB frame and let the GPU scale + convert
///   (`scale_vaapi`): no CPU colour conversion. Opt-in until measured.
pub(crate) fn vaapi_graph_desc(w: u32, h: u32, vpp: bool) -> String {
	if vpp {
		format!("hwupload,scale_vaapi=w={w}:h={h}:format=nv12:out_color_matrix=bt709:out_range=tv")
	} else {
		format!("{},format=nv12,hwupload", bt709_scale(w, h))
	}
}

/// `PULSAR_LIBAV_HW` = `0` / `false` / `off` keeps VA-API / QSV on the ffmpeg CLI (the
/// in-process engine then takes only software and NVENC plans, as before).
pub(crate) fn hw_in_process_enabled(env: Option<&str>) -> bool {
	!matches!(env.map(str::trim), Some("0") | Some("false") | Some("off"))
}

/// `PULSAR_VAAPI_VPP` = `1` / `true` / `on` turns the GPU colour conversion on.
pub(crate) fn vpp_enabled(env: Option<&str>) -> bool {
	matches!(env.map(str::trim), Some("1") | Some("true") | Some("on"))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn vaapi_always_reopens_qsv_only_before_ffmpeg_6() {
		assert_eq!(rate_change(HwEncoder::Vaapi, 58), RateChange::Reopen);
		assert_eq!(rate_change(HwEncoder::Vaapi, 61), RateChange::Reopen);
		assert_eq!(rate_change(HwEncoder::Qsv, 58), RateChange::Reopen);
		assert_eq!(rate_change(HwEncoder::Qsv, 60), RateChange::Live);
		assert_eq!(rate_change(HwEncoder::Software, 58), RateChange::Live);
		assert_eq!(rate_change(HwEncoder::Nvenc, 58), RateChange::Live);
	}

	#[test]
	fn reopen_waits_for_an_idr_unless_the_cut_is_large() {
		// Same rate / no rate: nothing to do.
		assert!(!reopen_now(8000, 8000, true));
		assert!(!reopen_now(8000, 0, true));
		// Small steps, either way, wait for the next IDR.
		assert!(!reopen_now(8000, 7000, false));
		assert!(!reopen_now(8000, 12_000, false));
		assert!(reopen_now(8000, 7000, true));
		assert!(reopen_now(8000, 12_000, true));
		// A 25 % cut (or more) goes at once.
		assert!(reopen_now(8000, 6000, false));
		assert!(reopen_now(8000, 2000, false));
		assert!(!reopen_now(8000, 6001, false));
	}

	#[test]
	fn hw_desktop_keeps_the_clis_burst_headroom() {
		// VA-API / QSV desktop: 2x peak, 1 s buffer (the CLI's -b:v-only VBR), never CBR.
		assert_eq!(rate_limits(8_000_000, HwEncoder::Vaapi, false, false), (16_000_000, 8_000_000));
		assert_eq!(rate_limits(8_000_000, HwEncoder::Qsv, false, true), (16_000_000, 8_000_000));
		// Game: CBR with the 0.5 s VBV; ~125 ms only on the opt-in.
		assert_eq!(rate_limits(8_000_000, HwEncoder::Vaapi, true, false), (8_000_000, 4_000_000));
		assert_eq!(rate_limits(8_000_000, HwEncoder::Qsv, true, false), (8_000_000, 4_000_000));
		assert_eq!(rate_limits(8_000_000, HwEncoder::Vaapi, true, true), (8_000_000, 1_000_000));
		// Unchanged for the encoders the engine already had, in both modes.
		for ll in [false, true] {
			assert_eq!(rate_limits(8_000_000, HwEncoder::Software, ll, true), (8_000_000, 4_000_000));
			assert_eq!(rate_limits(8_000_000, HwEncoder::Nvenc, ll, true), (8_000_000, 4_000_000));
		}
	}

	#[test]
	fn vaapi_chains_end_on_the_gpu_in_bt709() {
		assert_eq!(
			vaapi_graph_desc(1920, 1080, false),
			"scale=1920:1080:flags=fast_bilinear:out_color_matrix=bt709:out_range=tv,format=nv12,hwupload"
		);
		let vpp = vaapi_graph_desc(1280, 720, true);
		assert!(vpp.starts_with("hwupload,scale_vaapi=w=1280:h=720:format=nv12"));
		assert!(vpp.contains("out_color_matrix=bt709"));
	}

	#[test]
	fn hw_guard_ends_a_dead_encode_only() {
		let t0 = std::time::Instant::now();
		let s = |ms: u64| t0 + std::time::Duration::from_millis(ms);
		let mut g = HwGuard::new(t0);
		// A healthy encode: frames in, packets out.
		for i in 0..200u64 {
			g.sent(true);
			g.output(5000, s(i * 16));
		}
		assert_eq!(g.verdict(s(10_000)), None);
		// A few rejects in a row are tolerated; the counter resets on an accepted frame.
		for _ in 0..(HwGuard::MAX_REJECTS - 1) {
			g.sent(false);
		}
		assert_eq!(g.verdict(s(10_000)), None);
		g.sent(true);
		g.output(1, s(10_000));
		for _ in 0..HwGuard::MAX_REJECTS {
			g.sent(false);
		}
		assert!(g.verdict(s(10_000)).is_some());
		// Frames accepted but nothing comes out for 3 s.
		let mut g = HwGuard::new(t0);
		g.sent(true);
		assert_eq!(g.verdict(s(2_999)), None);
		assert!(g.verdict(s(3_000)).is_some());
		// No frames in at all (a still capture) is not the encoder's fault.
		let g = HwGuard::new(t0);
		assert_eq!(g.verdict(s(60_000)), None);
		// A re-open restarts the clock.
		let mut g = HwGuard::new(t0);
		g.sent(true);
		g.reset(s(2_900));
		g.sent(true);
		assert_eq!(g.verdict(s(5_000)), None);
	}

	#[test]
	fn env_switches() {
		assert!(hw_in_process_enabled(None));
		assert!(hw_in_process_enabled(Some("1")));
		assert!(!hw_in_process_enabled(Some(" 0 ")));
		assert!(!hw_in_process_enabled(Some("off")));
		assert!(!vpp_enabled(None));
		assert!(vpp_enabled(Some("1")));
		assert!(!vpp_enabled(Some("0")));
		assert!(!short_vbv_enabled(None));
		assert!(short_vbv_enabled(Some("1")));
		assert!(!short_vbv_enabled(Some("0")));
	}
}
