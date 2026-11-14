//! Rectangle math for the macOS capture, in Quartz global display coordinates: points, origin
//! at the top-left of the main display, y growing down — the space `CGEventGetLocation`,
//! `CGWarpMouseCursorPosition` and `kCGWindowBounds` all use. Pure, so it is unit-tested on
//! every platform. Same rules as src-tauri's `kbdhook/confine.rs` (the Windows pixel version).
//!
//! A rectangle is `(left, top, right, bottom)`; right/bottom are exclusive, and the cursor is
//! kept at least one point inside them.

pub type Rect = (f64, f64, f64, f64);

/// Smallest side a confine rect may have — anything thinner is a parked or minimized window
/// and must not trap the cursor.
pub const MIN_SIDE: f64 = 8.0;

fn valid(r: Rect) -> bool {
	r.2 - r.0 >= MIN_SIDE && r.3 - r.1 >= MIN_SIDE
}

/// The picture area inside `outer` (the renderer window), from the renderer's video
/// fractions `(x0, y0, x1, y1)` of that window (letterbox bars excluded). `None` or
/// degenerate fractions mean the whole window. `None` when the result is too small.
pub fn video_rect(outer: Rect, frac: Option<(f32, f32, f32, f32)>) -> Option<Rect> {
	if !valid(outer) {
		return None;
	}
	let (w, h) = (outer.2 - outer.0, outer.3 - outer.1);
	let (fx0, fy0, fx1, fy1) = match frac {
		Some((a, b, c, d)) if c > a && d > b => (
			a.clamp(0.0, 1.0) as f64,
			b.clamp(0.0, 1.0) as f64,
			c.clamp(0.0, 1.0) as f64,
			d.clamp(0.0, 1.0) as f64,
		),
		_ => (0.0, 0.0, 1.0, 1.0),
	};
	let r = (
		(outer.0 + w * fx0).round(),
		(outer.1 + h * fy0).round(),
		(outer.0 + w * fx1).round(),
		(outer.1 + h * fy1).round(),
	);
	valid(r).then_some(r)
}

/// A sub-rectangle given relative to `outer`'s top-left corner (`x, y, w, h`, e.g. the
/// session area inside the Pulsar window), clipped to `outer`. `None` when empty/too small.
pub fn sub_rect(outer: Rect, x: f64, y: f64, w: f64, h: f64) -> Option<Rect> {
	if !(w > 0.0 && h > 0.0) {
		return None;
	}
	let r = (
		(outer.0 + x).max(outer.0),
		(outer.1 + y).max(outer.1),
		(outer.0 + x + w).min(outer.2),
		(outer.1 + y + h).min(outer.3),
	);
	valid(r).then_some(r)
}

/// Point inside the rect (right/bottom exclusive).
pub fn contains(r: Rect, x: f64, y: f64) -> bool {
	x >= r.0 && x < r.2 && y >= r.1 && y < r.3
}

/// The nearest point at least one point inside the rect — where the cursor goes when a move
/// overshoots the confine.
pub fn clamp_into(r: Rect, x: f64, y: f64) -> (f64, f64) {
	(x.clamp(r.0, (r.2 - 1.0).max(r.0)), y.clamp(r.1, (r.3 - 1.0).max(r.1)))
}

/// Absolute pointer position (0..=1 on both axes) of a point within the rect — the value
/// `InputEvent::PointerMotion` carries. Points outside are clamped to the edge.
pub fn normalize(r: Rect, x: f64, y: f64) -> (f64, f64) {
	let (cx, cy) = clamp_into(r, x, y);
	let span = |lo: f64, hi: f64, v: f64| {
		let d = hi - lo - 1.0;
		if d <= 0.0 {
			0.0
		} else {
			((v - lo) / d).clamp(0.0, 1.0)
		}
	};
	(span(r.0, r.2, cx), span(r.1, r.3, cy))
}

/// The point lies inside the fractional sub-area `frac` (x0, y0, x1, y1) of `outer` — the
/// renderer's overlay-open button, reported as fractions of the renderer window.
pub fn in_frac(outer: Rect, frac: (f32, f32, f32, f32), x: f64, y: f64) -> bool {
	let (w, h) = (outer.2 - outer.0, outer.3 - outer.1);
	if w <= 0.0 || h <= 0.0 {
		return false;
	}
	let fx = (x - outer.0) / w;
	let fy = (y - outer.1) / h;
	fx >= frac.0 as f64 && fx < frac.2 as f64 && fy >= frac.1 as f64 && fy < frac.3 as f64
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn video_rect_whole_window_and_letterbox() {
		assert_eq!(video_rect((100.0, 50.0, 740.0, 410.0), None), Some((100.0, 50.0, 740.0, 410.0)));
		// 16:9 video in a 16:10 window: bars top and bottom.
		assert_eq!(
			video_rect((0.0, 0.0, 1440.0, 900.0), Some((0.0, 0.05, 1.0, 0.95))),
			Some((0.0, 45.0, 1440.0, 855.0))
		);
		// Degenerate fractions → whole window.
		assert_eq!(
			video_rect((0.0, 0.0, 800.0, 600.0), Some((0.5, 0.0, 0.5, 1.0))),
			Some((0.0, 0.0, 800.0, 600.0))
		);
		// A display left of the main one has negative x.
		assert_eq!(
			video_rect((-1920.0, 0.0, 0.0, 1080.0), Some((0.25, 0.0, 0.75, 1.0))),
			Some((-1440.0, 0.0, -480.0, 1080.0))
		);
		assert_eq!(video_rect((0.0, 0.0, 4.0, 400.0), None), None);
	}

	#[test]
	fn sub_rect_offsets_and_clips() {
		let win = (200.0, 100.0, 1400.0, 900.0);
		// Session area below a 40-point tab bar.
		assert_eq!(sub_rect(win, 0.0, 40.0, 1200.0, 760.0), Some((200.0, 140.0, 1400.0, 900.0)));
		// Overhang is clipped to the window.
		assert_eq!(sub_rect(win, -10.0, 0.0, 5000.0, 50.0), Some((200.0, 100.0, 1400.0, 150.0)));
		assert_eq!(sub_rect(win, 0.0, 0.0, 0.0, 10.0), None);
		assert_eq!(sub_rect(win, 0.0, 0.0, 3.0, 10.0), None);
	}

	#[test]
	fn contains_clamp_normalize() {
		let r = (10.0, 20.0, 110.0, 220.0);
		assert!(contains(r, 10.0, 20.0));
		assert!(contains(r, 109.5, 219.5));
		assert!(!contains(r, 110.0, 100.0));
		assert!(!contains(r, 50.0, 19.9));
		assert_eq!(clamp_into(r, 5000.0, 100.0), (109.0, 100.0));
		assert_eq!(clamp_into(r, -5000.0, -3.0), (10.0, 20.0));
		assert_eq!(clamp_into(r, 60.5, 60.25), (60.5, 60.25));
		let n = (100.0, 100.0, 201.0, 301.0);
		assert_eq!(normalize(n, 100.0, 100.0), (0.0, 0.0));
		assert_eq!(normalize(n, 200.0, 300.0), (1.0, 1.0));
		assert_eq!(normalize(n, 150.0, 200.0), (0.5, 0.5));
		assert_eq!(normalize(n, 9999.0, -9999.0), (1.0, 0.0));
		assert_eq!(normalize((5.0, 5.0, 6.0, 6.0), 5.0, 5.0), (0.0, 0.0));
	}

	#[test]
	fn frac_hit_test() {
		let win = (0.0, 0.0, 1000.0, 500.0);
		let btn = (0.9, 0.0, 1.0, 0.1);
		assert!(in_frac(win, btn, 950.0, 10.0));
		assert!(!in_frac(win, btn, 850.0, 10.0));
		assert!(!in_frac(win, btn, 950.0, 60.0));
		assert!(!in_frac((0.0, 0.0, 0.0, 0.0), btn, 0.0, 0.0));
	}
}
