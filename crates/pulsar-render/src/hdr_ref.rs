//! The brightness reference an HDR10 (PQ) picture is tone-mapped to SDR around, shared by the
//! three renderers (Windows `win/hdr.rs`, Linux `video.rs` `FRAG_TONEMAP`, macOS `mac/metal.rs`).
//!
//! - **SDR white** (nits): where the HOST's SDR content sits on its HDR monitor — Windows
//!   "SDR content brightness", about 80–480 nits. Most of a remote desktop is SDR content at
//!   that level, so mapping it to SDR 1.0 keeps the desktop as bright as on an SDR stream. A
//!   fixed 203 nits (ITU-R BT.2408) made an 80-nit host look dim and pushed a 300+-nit host's
//!   UI whites into the roll-off.
//! - **Peak** (nits): the host monitor's maximum (its mastering-display max) — the highlight
//!   roll-off ends there.
//!
//! The host sends both (`DataMsg::HdrMeta` → the app's `hdrref <sdr> <peak>` stdin line); until
//! then (or from a host that does not send them) the defaults below apply.

use std::sync::atomic::{AtomicU32, Ordering};

/// SDR reference white when the host said nothing (ITU-R BT.2408).
pub const DEFAULT_SDR_WHITE_NITS: f32 = 203.0;
/// Roll-off peak when the host said nothing (a typical HDR10 desktop panel).
pub const DEFAULT_PEAK_NITS: f32 = 1000.0;
/// Below this share of reference white the tone curve is the identity.
pub const KNEE: f32 = 0.8;

static SDR_WHITE: AtomicU32 = AtomicU32::new(0);
static PEAK: AtomicU32 = AtomicU32::new(0);

/// `(SDR white, peak)` in nits, sanitised: SDR white 80–1000, peak at least SDR white.
pub fn get() -> (f32, f32) {
	let s = SDR_WHITE.load(Ordering::Relaxed);
	let p = PEAK.load(Ordering::Relaxed);
	sanitize(s as f32, p as f32)
}

/// Pure part of [`get`] (0 = unknown → default).
pub fn sanitize(sdr: f32, peak: f32) -> (f32, f32) {
	let sdr = if sdr > 0.0 { sdr.clamp(80.0, 1000.0) } else { DEFAULT_SDR_WHITE_NITS };
	let peak = if peak > 0.0 { peak.clamp(80.0, 10_000.0) } else { DEFAULT_PEAK_NITS };
	(sdr, peak.max(sdr))
}

/// The `hdrref <sdr_white_nits> <peak_nits>` stdin line's arguments (0 = unknown).
pub fn set_from_args<'a>(it: impl Iterator<Item = &'a str>) {
	let (s, p) = parse(it);
	SDR_WHITE.store(s, Ordering::Relaxed);
	PEAK.store(p, Ordering::Relaxed);
}

fn parse<'a>(mut it: impl Iterator<Item = &'a str>) -> (u32, u32) {
	let s = it.next().and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
	let p = it.next().and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
	(s, p)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn unknown_values_fall_back_and_odd_ones_are_clamped() {
		assert_eq!(sanitize(0.0, 0.0), (DEFAULT_SDR_WHITE_NITS, DEFAULT_PEAK_NITS));
		assert_eq!(sanitize(80.0, 600.0), (80.0, 600.0));
		assert_eq!(sanitize(20.0, 0.0), (80.0, DEFAULT_PEAK_NITS));
		// A peak under the SDR white (bogus report) never makes the roll-off start above it.
		assert_eq!(sanitize(400.0, 300.0), (400.0, 400.0));
	}

	#[test]
	fn the_stdin_line_parses() {
		// (The global is not touched here: the Windows shader tests read it in parallel.)
		assert_eq!(parse("240 1500".split_whitespace()), (240, 1500));
		assert_eq!(parse("x".split_whitespace()), (0, 0));
		assert_eq!(sanitize(240.0, 1500.0), (240.0, 1500.0));
	}
}
