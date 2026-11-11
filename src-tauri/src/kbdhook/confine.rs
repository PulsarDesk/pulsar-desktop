//! Pure rectangle math for the Windows cursor confine (`imp.rs`): where the video sits on
//! screen, whether a point is inside it, and how a point outside is pulled back in. Kept free
//! of Win32 calls so it is unit-tested on every platform.
//!
//! All rectangles are screen pixels, `(left, top, right, bottom)`, right/bottom EXCLUSIVE —
//! the `RECT` convention `ClipCursor` uses (the cursor can reach `right - 1`, never `right`).

pub type Rect = (i32, i32, i32, i32);

/// Smallest side, in px, a confine rect may have — anything thinner is a parked/minimized
/// window and must not trap the cursor.
pub const MIN_SIDE: i32 = 8;

/// The picture area inside `outer` (the renderer window), given the renderer's reported
/// video fractions `(x0, y0, x1, y1)` of its client area (letterbox bars excluded). `None`
/// fractions, or a degenerate set, mean the whole window. `None` when the result is too
/// small to confine to.
pub fn video_rect(outer: Rect, frac: Option<(f32, f32, f32, f32)>) -> Option<Rect> {
	let (l, t, r, b) = outer;
	let (w, h) = ((r - l) as f32, (b - t) as f32);
	if w < MIN_SIDE as f32 || h < MIN_SIDE as f32 {
		return None;
	}
	let (fx0, fy0, fx1, fy1) = match frac {
		Some((a, b2, c, d)) if c > a && d > b2 => (
			a.clamp(0.0, 1.0),
			b2.clamp(0.0, 1.0),
			c.clamp(0.0, 1.0),
			d.clamp(0.0, 1.0),
		),
		_ => (0.0, 0.0, 1.0, 1.0),
	};
	let rect = (
		l + (w * fx0).round() as i32,
		t + (h * fy0).round() as i32,
		l + (w * fx1).round() as i32,
		t + (h * fy1).round() as i32,
	);
	if rect.2 - rect.0 < MIN_SIDE || rect.3 - rect.1 < MIN_SIDE {
		return None;
	}
	Some(rect)
}

/// `a ∩ b`, `None` when they don't overlap. Used to trim the confine rect to the virtual
/// desktop: Windows stores the clip already intersected with the screen, so an untrimmed
/// rect (a maximized window's frame hangs ~8 px off-screen) would never compare equal to
/// `GetClipCursor` and get re-applied on every tick.
pub fn intersect(a: Rect, b: Rect) -> Option<Rect> {
	let r = (a.0.max(b.0), a.1.max(b.1), a.2.min(b.2), a.3.min(b.3));
	(r.2 > r.0 && r.3 > r.1).then_some(r)
}

/// Point inside the rect (right/bottom exclusive).
pub fn contains(r: Rect, x: i32, y: i32) -> bool {
	x >= r.0 && x < r.2 && y >= r.1 && y < r.3
}

/// The nearest point inside the rect — where the cursor goes when a move overshoots the
/// confine (a fast swipe while the clip was momentarily dropped).
pub fn clamp_into(r: Rect, x: i32, y: i32) -> (i32, i32) {
	(
		x.clamp(r.0, (r.2 - 1).max(r.0)),
		y.clamp(r.1, (r.3 - 1).max(r.1)),
	)
}

/// Absolute pointer position (0..=1 on both axes) of a screen point within the rect, the
/// value `InputEvent::PointerMotion` carries. Points outside are clamped to the edge.
pub fn normalize(r: Rect, x: i32, y: i32) -> (f64, f64) {
	let (cx, cy) = clamp_into(r, x, y);
	let span = |lo: i32, hi: i32, v: i32| {
		let d = (hi - lo - 1) as f64;
		if d <= 0.0 {
			0.0
		} else {
			((v - lo) as f64 / d).clamp(0.0, 1.0)
		}
	};
	(span(r.0, r.2, cx), span(r.1, r.3, cy))
}

/// A screen point packed into one u64 (for an atomic hand-off between threads); negative
/// coordinates (monitors left of / above the primary) survive the round trip.
pub fn pack_pos(x: i32, y: i32) -> u64 {
	((x as u32 as u64) << 32) | (y as u32 as u64)
}

pub fn unpack_pos(v: u64) -> (i32, i32) {
	((v >> 32) as u32 as i32, v as u32 as i32)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn video_rect_whole_window_without_frac() {
		assert_eq!(
			video_rect((100, 50, 1380, 770), None),
			Some((100, 50, 1380, 770))
		);
		// Degenerate fractions fall back to the whole window too.
		assert_eq!(
			video_rect((0, 0, 800, 600), Some((0.5, 0.0, 0.5, 1.0))),
			Some((0, 0, 800, 600))
		);
	}

	#[test]
	fn video_rect_letterbox() {
		// 1920x1200 window showing 16:9 video: bars top and bottom.
		let r = video_rect((0, 0, 1920, 1200), Some((0.0, 0.05, 1.0, 0.95))).unwrap();
		assert_eq!(r, (0, 60, 1920, 1140));
	}

	#[test]
	fn video_rect_negative_origin_second_monitor() {
		// A monitor left of the primary has negative screen x.
		let r = video_rect((-2560, 0, 0, 1440), Some((0.125, 0.0, 0.875, 1.0))).unwrap();
		assert_eq!(r, (-2240, 0, -320, 1440));
	}

	#[test]
	fn video_rect_rejects_tiny() {
		assert_eq!(video_rect((-32000, -32000, -31999, -31999), None), None); // parked 1x1 child
		assert_eq!(
			video_rect((0, 0, 800, 600), Some((0.0, 0.0, 0.005, 1.0))),
			None
		);
	}

	#[test]
	fn video_rect_clamps_out_of_range_frac() {
		assert_eq!(
			video_rect((0, 0, 100, 100), Some((-0.5, -1.0, 2.0, 3.0))),
			Some((0, 0, 100, 100))
		);
	}

	#[test]
	fn intersect_trims_offscreen_frame() {
		let maximized = (-8, -8, 1928, 1088);
		let screen = (0, 0, 1920, 1080);
		assert_eq!(intersect(maximized, screen), Some((0, 0, 1920, 1080)));
		assert_eq!(intersect((0, 0, 10, 10), (10, 0, 20, 10)), None);
		// Spanning two monitors side by side: the virtual screen covers both.
		assert_eq!(
			intersect((1800, 100, 2100, 400), (0, 0, 3840, 1080)),
			Some((1800, 100, 2100, 400))
		);
	}

	#[test]
	fn contains_is_right_bottom_exclusive() {
		let r = (10, 20, 110, 220);
		assert!(contains(r, 10, 20));
		assert!(contains(r, 109, 219));
		assert!(!contains(r, 110, 100));
		assert!(!contains(r, 50, 220));
		assert!(!contains(r, 9, 100));
		assert!(!contains(r, 50, 19));
	}

	#[test]
	fn clamp_into_pulls_overshoot_back() {
		let r = (10, 20, 110, 220);
		assert_eq!(clamp_into(r, 5000, 100), (109, 100)); // fast swipe right
		assert_eq!(clamp_into(r, -5000, 100), (10, 100)); // fast swipe left
		assert_eq!(clamp_into(r, 50, -3), (50, 20));
		assert_eq!(clamp_into(r, 300, 300), (109, 219));
		assert_eq!(clamp_into(r, 60, 60), (60, 60));
		// Negative coordinates (monitor left of / above the primary).
		assert_eq!(clamp_into((-1920, -200, 0, 880), 40, -900), (-1, -200));
	}

	#[test]
	fn normalize_maps_edges_to_unit() {
		let r = (100, 100, 201, 301);
		assert_eq!(normalize(r, 100, 100), (0.0, 0.0));
		assert_eq!(normalize(r, 200, 300), (1.0, 1.0));
		assert_eq!(normalize(r, 150, 200), (0.5, 0.5));
		// Overshoot clamps to the edge it crossed.
		assert_eq!(normalize(r, 9999, -9999), (1.0, 0.0));
		// 1 px wide: no division by zero.
		assert_eq!(normalize((5, 5, 6, 6), 5, 5), (0.0, 0.0));
	}

	#[test]
	fn pack_round_trips_negative() {
		for (x, y) in [
			(0, 0),
			(-1, -1),
			(-2560, 1439),
			(3839, -200),
			(i32::MIN, i32::MAX),
		] {
			assert_eq!(unpack_pos(pack_pos(x, y)), (x, y));
		}
	}
}
