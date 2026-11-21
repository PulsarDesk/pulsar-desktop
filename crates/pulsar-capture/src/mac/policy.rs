//! Pure decisions of the macOS native path (sizes, rates, timestamps, stats), kept apart from
//! the framework calls so they build and are unit-tested on every target.

/// The output size for a requested `want_w × want_h` on a display of `native_w × native_h`
/// pixels: the display's own aspect fitted into the request (`crate::fit_aspect`: 0 = no limit
/// on that axis, never larger than the display, even, at least 2×2). The whole display stays in
/// the frame (ScreenCaptureKit `preservesAspectRatio = NO` is then a pure scale), so a
/// normalized pointer position maps onto all of it. Each axis used to be clamped on its own,
/// which squashed a display whose aspect differs from the client's ladder point.
pub fn output_size(want_w: u32, want_h: u32, native_w: u32, native_h: u32) -> (u32, u32) {
	crate::fit_aspect(native_w, native_h, want_w, want_h)
}

/// The output size of a WINDOW stream: the host already fitted the window's aspect into the
/// client's box (`want_w × want_h`); a 0 axis means "the window's own size" (`window_px`, asked
/// only then). Even, at least 2×2. ScreenCaptureKit keeps the window's aspect inside it.
pub fn window_output_size(want_w: u32, want_h: u32, window_px: impl FnOnce() -> Option<(u32, u32)>) -> (u32, u32) {
	let (w, h) = if want_w == 0 || want_h == 0 {
		let (pw, ph) = window_px().unwrap_or((1280, 720));
		(if want_w == 0 { pw } else { want_w }, if want_h == 0 { ph } else { want_h })
	} else {
		(want_w, want_h)
	};
	(w.max(2) & !1, h.max(2) & !1)
}

/// `DataRateLimits` byte budget per second for a `kbps` average: 1.5 × the average, so a
/// keyframe may overshoot but cannot flood the link.
pub fn data_rate_limit_bytes(kbps: u32) -> i64 {
	kbps.max(100) as i64 * 1000 / 8 * 3 / 2
}

/// RTP 90 kHz timestamp of a µs capture time (wraps like RTP does).
pub fn rtp_ts_90k(pts_us: i64) -> u32 {
	(pts_us.max(0) as u128 * 9 / 100) as u32
}

/// The next presentation time: the frame's own capture time, but always after the previous one
/// (VideoToolbox wants strictly increasing timestamps; a repeated frame is stamped "now").
pub fn next_pts(capture_us: i64, prev_us: Option<i64>) -> i64 {
	match prev_us {
		Some(p) if capture_us <= p => p + 1,
		_ => capture_us,
	}
}

/// Median and 90th percentile of `samples` (sorted in place); (0, 0) when empty.
pub fn p50_p90(samples: &mut [u32]) -> (u32, u32) {
	if samples.is_empty() {
		return (0, 0);
	}
	samples.sort_unstable();
	let at = |q: usize| samples[((samples.len() - 1) * q) / 100];
	(at(50), at(90))
}

/// One step of the encode-time EWMA (1/8 weight for the new sample; the first sample seeds it).
pub fn ewma_us(prev: u32, sample: u32) -> u32 {
	if prev == 0 {
		sample
	} else {
		((prev as u64 * 7 + sample as u64) / 8) as u32
	}
}

/// Where an `rtp://host:port` destination points.
pub fn rtp_dest(dest: &str) -> Option<std::net::SocketAddr> {
	dest.strip_prefix("rtp://").unwrap_or(dest).split('?').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn output_size_defaults_clamps_and_evens() {
		assert_eq!(output_size(0, 0, 3024, 1964), (3024, 1964));
		assert_eq!(output_size(2560, 1440, 3024, 1964), (2216, 1440));
		assert_eq!(output_size(4000, 3000, 2560, 1440), (2560, 1440));
		assert_eq!(output_size(1281, 721, 2560, 1440), (1280, 720));
		assert_eq!(output_size(1, 1, 0, 0), (2, 2));
		assert_eq!(output_size(1920, 1080, 0, 0), (1920, 1080));
	}

	#[test]
	fn output_size_keeps_the_display_aspect() {
		// A 3024×1964 (≈1.54) panel asked for 16:9: the whole panel, scaled to fit (no squash).
		assert_eq!(output_size(1920, 1080, 3024, 1964), (1662, 1080));
	}

	#[test]
	fn window_output_size_keeps_the_fitted_box_and_evens() {
		// Fitted by the host: taken as is (made even); the window is not asked.
		assert_eq!(window_output_size(1280, 801, || panic!("not asked")), (1280, 800));
		assert_eq!(window_output_size(1, 1, || None), (2, 2));
		// 0 = the window's own size.
		assert_eq!(window_output_size(0, 0, || Some((1513, 983))), (1512, 982));
		assert_eq!(window_output_size(0, 720, || Some((1000, 500))), (1000, 720));
		// The window is gone meanwhile: a sane default (the start then fails on the window).
		assert_eq!(window_output_size(0, 0, || None), (1280, 720));
	}

	#[test]
	fn rate_limit_is_one_and_a_half_times_the_average() {
		assert_eq!(data_rate_limit_bytes(8000), 1_500_000);
		assert_eq!(data_rate_limit_bytes(0), data_rate_limit_bytes(100));
	}

	#[test]
	fn rtp_ts_is_90khz() {
		assert_eq!(rtp_ts_90k(0), 0);
		assert_eq!(rtp_ts_90k(1_000_000), 90_000);
		assert_eq!(rtp_ts_90k(-5), 0);
		// Wraps after 2^32 ticks (~13.25 h) like an RTP clock.
		assert_eq!(rtp_ts_90k((1u64 << 32) as i64 * 100 / 9 + 12), 1);
	}

	#[test]
	fn pts_always_moves_forward() {
		assert_eq!(next_pts(100, None), 100);
		assert_eq!(next_pts(200, Some(100)), 200);
		assert_eq!(next_pts(100, Some(100)), 101);
		assert_eq!(next_pts(50, Some(100)), 101);
	}

	#[test]
	fn percentiles() {
		assert_eq!(p50_p90(&mut []), (0, 0));
		let mut v: Vec<u32> = (1..=10).rev().collect();
		assert_eq!(p50_p90(&mut v), (5, 9));
	}

	#[test]
	fn ewma_seeds_then_smooths() {
		assert_eq!(ewma_us(0, 800), 800);
		assert_eq!(ewma_us(800, 1600), 900);
	}

	#[test]
	fn parses_rtp_destinations() {
		assert_eq!(rtp_dest("rtp://127.0.0.1:5004"), Some("127.0.0.1:5004".parse().unwrap()));
		assert_eq!(rtp_dest("rtp://10.0.0.2:9000?pkt_size=1200"), Some("10.0.0.2:9000".parse().unwrap()));
		assert_eq!(rtp_dest("nonsense"), None);
	}
}
