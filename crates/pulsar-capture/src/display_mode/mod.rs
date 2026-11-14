//! Host "screen adaptation" (Parsec-style) — the parts that are the same on every OS.
//!
//! The app (`src-tauri/src/display_mode.rs`) switches a captured monitor to the display mode
//! that best fits a client's split pane and restores the original mode on teardown. Each OS
//! has its own way to list and set modes (Windows `EnumDisplaySettingsW`, X11 `xrandr`,
//! wlroots `wlr-randr`, KDE `kscreen-doctor`, macOS CoreGraphics), but the CHOICE is shared:
//!
//! * [`score`] / [`plan`] — which mode fits the pane best (aspect first, then area, keep the
//!   refresh rate and the HiDPI scale the monitor runs at now).
//! * [`xrandr`] — a parser for `xrandr --current` (Linux X11).
//! * [`macos`] — the CoreGraphics mode switch (macOS only; the change is made
//!   `kCGConfigureForAppOnly`, so the OS itself restores the mode when the app exits — even on
//!   a crash).
//!
//! Everything except [`macos`] is pure and unit-tested on every target.

pub mod xrandr;

#[cfg(target_os = "macos")]
pub mod macos;

/// Score a candidate mode against the target pane size; LOWER is better. Primary key is the
/// aspect-ratio distance (so the captured frame matches the pane's shape — the whole point of
/// adaptation). Among near-equal aspect we tie-break on area distance to the pane, with a mild
/// bias toward "tall enough" modes (height ≥ 1080) so adaptation doesn't drop a big monitor to
/// a tiny letterbox-equivalent resolution. `native_area` lets us avoid exceeding the monitor's
/// current resolution when a smaller equally-good-aspect mode exists.
pub fn score(mw: u32, mh: u32, target_w: u32, target_h: u32, native_area: u64) -> (i64, i64) {
	// Aspect distance, scaled to an integer so the tuple can be Ord-compared. We compare
	// w*th vs h*tw (cross-multiply) to avoid float division entirely.
	let mode_cross = mw as i64 * target_h as i64;
	let target_cross = mh as i64 * target_w as i64;
	// Normalize the cross-product difference so it's comparable across resolutions: divide by
	// the target dimension product magnitude. Multiply up first to keep integer precision.
	let denom = (target_w as i64 * target_h as i64).max(1);
	let aspect_dist = ((mode_cross - target_cross).abs() * 10_000) / denom;
	// Area distance to the pane (secondary). A mode that EXCEEDS native is penalized so an
	// equally-shaped non-native mode is preferred (don't push the monitor past its panel).
	let area = mw as i64 * mh as i64;
	let target_area = target_w as i64 * target_h as i64;
	let mut area_dist = (area - target_area).abs();
	// Mild preference for ≥1080 lines (keeps adaptation from collapsing to a tiny mode when
	// several share the pane's aspect): bump the area_dist of sub-1080 modes.
	if mh < 1080 {
		area_dist += target_area / 2;
	}
	// Penalize exceeding native area (we'd rather not over-drive the panel).
	if area as u64 > native_area {
		area_dist += area - native_area as i64;
	}
	(aspect_dist, area_dist)
}

/// One selectable display mode, as the platform reports it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cand {
	/// Size of the captured frame in this mode, in PIXELS, already in the monitor's current
	/// orientation (a portrait-rotated output reports `w < h`).
	pub w: u32,
	pub h: u32,
	/// Refresh rate in Hz (0 = unknown).
	pub hz: f64,
	/// Backing scale (pixels per point): 2 for a macOS HiDPI mode, 1 everywhere else.
	pub scale: u32,
}

/// Pick the mode to switch to for a `target_w × target_h` pane. `cands` are every selectable
/// mode of the monitor (interlaced / non-desktop modes already filtered out) and `current` the
/// index of the mode it runs in now.
///
/// Returns the index of the chosen mode, or `None` when there is nothing to do: an empty or
/// zero-sized target, a bad `current`, or the best-fit size equals the current size (no change
/// means nothing to revert later).
///
/// The size comes from [`score`] (first best wins on a tie, like the Windows enumeration);
/// among the modes of that size the one closest to the current refresh rate, with the same
/// HiDPI scale as now, is taken — adaptation changes the shape, not the smoothness or the UI
/// scale.
pub fn plan(cands: &[Cand], current: usize, target_w: u32, target_h: u32) -> Option<usize> {
	if target_w == 0 || target_h == 0 {
		return None;
	}
	let cur = *cands.get(current)?;
	let native_area = cur.w as u64 * cur.h as u64;
	let mut best: Option<((i64, i64), (u32, u32))> = None;
	for c in cands {
		if c.w == 0 || c.h == 0 {
			continue;
		}
		let s = score(c.w, c.h, target_w, target_h, native_area);
		match &best {
			Some((bs, _)) if *bs <= s => {}
			_ => best = Some((s, (c.w, c.h))),
		}
	}
	let (_, (bw, bh)) = best?;
	if (bw, bh) == (cur.w, cur.h) {
		return None;
	}
	let key = |c: &Cand| {
		let scale_diff = (c.scale != cur.scale) as u8;
		// Hz distance in milli-Hz so it orders as an integer.
		let hz_diff = ((c.hz - cur.hz).abs() * 1000.0) as u64;
		(scale_diff, hz_diff)
	};
	cands
		.iter()
		.enumerate()
		.filter(|(_, c)| c.w == bw && c.h == bh)
		.min_by_key(|(_, c)| key(c))
		.map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn c(w: u32, h: u32, hz: f64) -> Cand {
		Cand { w, h, hz, scale: 1 }
	}

	#[test]
	fn picks_the_pane_aspect() {
		// 16:10 pane on a 16:9 4K monitor: 1920x1200 is the only 16:10 mode.
		let modes = [c(3840, 2160, 60.0), c(2560, 1440, 60.0), c(1920, 1200, 60.0), c(1920, 1080, 60.0)];
		assert_eq!(plan(&modes, 0, 1280, 800), Some(2));
	}

	#[test]
	fn no_change_when_already_best() {
		let modes = [c(1920, 1080, 60.0), c(1280, 1024, 60.0)];
		assert_eq!(plan(&modes, 0, 1600, 900), None);
	}

	#[test]
	fn keeps_refresh_and_scale() {
		let modes = [
			c(2560, 1440, 144.0),
			c(1920, 1200, 60.0),
			c(1920, 1200, 143.9),
			Cand { w: 1920, h: 1200, hz: 144.0, scale: 2 },
		];
		assert_eq!(plan(&modes, 0, 1280, 800), Some(2));
	}

	#[test]
	fn rejects_bad_input() {
		let modes = [c(1920, 1080, 60.0)];
		assert_eq!(plan(&modes, 0, 0, 800), None);
		assert_eq!(plan(&modes, 3, 1280, 800), None);
		assert_eq!(plan(&[], 0, 1280, 800), None);
	}

	#[test]
	fn prefers_tall_enough_and_not_past_native() {
		// Same 16:9 aspect everywhere: past-native 3840x2160 and sub-1080 1280x720 both lose
		// to 1920x1080 for a 1600x900 pane on a 2560x1440 monitor.
		let modes = [c(2560, 1440, 60.0), c(3840, 2160, 60.0), c(1920, 1080, 60.0), c(1280, 720, 60.0)];
		assert_eq!(plan(&modes, 0, 1600, 900), Some(2));
	}
}
