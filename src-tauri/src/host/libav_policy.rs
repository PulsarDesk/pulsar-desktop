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

/// The VBV (`rc_buffer_size`, bits) for `bit_rate` (bits/s). VA-API / QSV in game mode get a
/// short buffer (~125 ms: a frame never bursts far past the link's rate, yet the 0.25 s GOP's
/// IDRs keep their quality); everything else keeps the ~0.5 s the engine always used.
pub(crate) fn vbv_bits(bit_rate: i64, enc: HwEncoder, low_latency: bool) -> i64 {
	let div = if low_latency && matches!(enc, HwEncoder::Vaapi | HwEncoder::Qsv) { 8 } else { 2 };
	(bit_rate / div).max(1)
}

/// The VA-API filter chain behind `buffer` (x11grab's `bgr0`) ending in GPU NV12 surfaces.
///
/// * default — CPU `scale` + `format=nv12`, then `hwupload`: exactly the ffmpeg CLI's VA-API
///   chain (`format=nv12,hwupload`), so the colours (swscale's BT.601 matrix) match what the
///   client has always decoded.
/// * `vpp` (`PULSAR_VAAPI_VPP=1`) — upload the RGB frame and let the GPU scale + convert
///   (`scale_vaapi`): no CPU colour conversion, but the matrix is the driver's (BT.709 for HD
///   on Mesa / iHD), so the encoder's VUI is tagged BT.709 to match. Opt-in until measured.
pub(crate) fn vaapi_graph_desc(w: u32, h: u32, vpp: bool) -> String {
	if vpp {
		format!("hwupload,scale_vaapi=w={w}:h={h}:format=nv12:out_color_matrix=bt709:out_range=tv")
	} else {
		format!("scale={w}:{h}:flags=fast_bilinear,format=nv12,hwupload")
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
	fn short_vbv_only_for_hw_game_streams() {
		assert_eq!(vbv_bits(8_000_000, HwEncoder::Vaapi, true), 1_000_000);
		assert_eq!(vbv_bits(8_000_000, HwEncoder::Qsv, true), 1_000_000);
		assert_eq!(vbv_bits(8_000_000, HwEncoder::Vaapi, false), 4_000_000);
		// Unchanged for the encoders the engine already had.
		assert_eq!(vbv_bits(8_000_000, HwEncoder::Software, true), 4_000_000);
		assert_eq!(vbv_bits(8_000_000, HwEncoder::Nvenc, true), 4_000_000);
	}

	#[test]
	fn vaapi_chains_end_on_the_gpu() {
		assert_eq!(vaapi_graph_desc(1920, 1080, false), "scale=1920:1080:flags=fast_bilinear,format=nv12,hwupload");
		let vpp = vaapi_graph_desc(1280, 720, true);
		assert!(vpp.starts_with("hwupload,scale_vaapi=w=1280:h=720:format=nv12"));
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
	}
}
