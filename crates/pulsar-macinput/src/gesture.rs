//! Trackpad swipes that switch Spaces / open Mission Control or App Exposé (macOS client).
//!
//! macOS gives an app no raw trackpad contacts, but the event tap sees the window server's
//! gesture events. The Spaces / Mission Control swipe arrives as a "Dock swipe" gesture
//! (`kCGSEventGesture` 29 or the Dock-control event 30 with the private
//! `kCGEventGestureHIDType` field = 23, `kIOHIDEventTypeDockSwipe`), carrying its axis
//! (`…SwipeMotion` 1 = horizontal, 2 = vertical), the cumulative progress and the phase
//! (1 began, 2 changed, 4 ended, 8 cancelled) — the same layout tools that post synthetic
//! Space switches use. While control is engaged and gestures go to the host, the tap SWALLOWS
//! these events (the Dock never sees them, so this Mac does not switch Spaces) and the
//! recognised direction goes to the host. The finger count is not in the event: the app takes
//! it from the user's trackpad preferences.
//!
//! The progress sign convention is not documented anywhere; it is one constant here
//! ([`H_POSITIVE`], [`V_POSITIVE`]). Untested on macOS hardware.

use std::sync::atomic::{AtomicBool, Ordering::SeqCst};

/// Fingers' direction (the content follows the fingers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwipeDir {
	Up,
	Down,
	Left,
	Right,
}

/// Gesture event types the tap watches.
pub const EVENT_GESTURE: u32 = 29;
pub const EVENT_DOCK_CONTROL: u32 = 30;
/// Private `CGEventField`s of a gesture event.
pub const FIELD_HID_TYPE: u32 = 110;
pub const FIELD_SWIPE_MOTION: u32 = 123;
pub const FIELD_SWIPE_PROGRESS: u32 = 124;
pub const FIELD_PHASE: u32 = 132;
/// `kIOHIDEventTypeDockSwipe`.
pub const HID_DOCK_SWIPE: i64 = 23;
pub const MOTION_HORIZONTAL: i64 = 1;
pub const MOTION_VERTICAL: i64 = 2;
pub const PHASE_BEGAN: i64 = 1;
pub const PHASE_ENDED: i64 = 4;
pub const PHASE_CANCELLED: i64 = 8;

/// The direction a positive progress means: horizontal = the Space on the right comes in
/// (fingers move left); vertical = fingers move down (App Exposé).
pub const H_POSITIVE: SwipeDir = SwipeDir::Left;
pub const V_POSITIVE: SwipeDir = SwipeDir::Down;
/// Progress (fraction of a full Space switch) at which the swipe counts.
pub const THRESHOLD: f64 = 0.08;

/// Gestures go to the host (the app's setting on + the host performs them).
static FORWARD: AtomicBool = AtomicBool::new(false);

pub fn set_forward_gestures(on: bool) {
	FORWARD.store(on, SeqCst);
}

pub fn forward_gestures() -> bool {
	FORWARD.load(SeqCst)
}

/// One Dock swipe, from its gesture events to one direction.
#[derive(Debug, Default)]
pub struct DockSwipe {
	open: bool,
	fired: bool,
}

impl DockSwipe {
	pub const fn new() -> Self {
		Self { open: false, fired: false }
	}

	/// One Dock-swipe event. Returns the direction once per swipe, as soon as the progress
	/// passes the threshold.
	pub fn event(&mut self, phase: i64, motion: i64, progress: f64) -> Option<SwipeDir> {
		if phase == PHASE_BEGAN || !self.open {
			self.open = true;
			self.fired = false;
		}
		let done = phase & (PHASE_ENDED | PHASE_CANCELLED) != 0;
		let out = if !self.fired && progress.is_finite() && progress.abs() >= THRESHOLD {
			let positive = progress > 0.0;
			let dir = match motion {
				MOTION_HORIZONTAL => Some(if positive { H_POSITIVE } else { opposite(H_POSITIVE) }),
				MOTION_VERTICAL => Some(if positive { V_POSITIVE } else { opposite(V_POSITIVE) }),
				_ => None,
			};
			if dir.is_some() {
				self.fired = true;
			}
			dir
		} else {
			None
		};
		if done {
			self.open = false;
		}
		out
	}
}

fn opposite(d: SwipeDir) -> SwipeDir {
	match d {
		SwipeDir::Up => SwipeDir::Down,
		SwipeDir::Down => SwipeDir::Up,
		SwipeDir::Left => SwipeDir::Right,
		SwipeDir::Right => SwipeDir::Left,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn one_direction_per_swipe() {
		let mut s = DockSwipe::new();
		assert_eq!(s.event(PHASE_BEGAN, MOTION_HORIZONTAL, 0.0), None);
		assert_eq!(s.event(2, MOTION_HORIZONTAL, 0.05), None);
		assert_eq!(s.event(2, MOTION_HORIZONTAL, 0.2), Some(H_POSITIVE));
		assert_eq!(s.event(2, MOTION_HORIZONTAL, 0.6), None, "fired once");
		assert_eq!(s.event(PHASE_ENDED, MOTION_HORIZONTAL, 0.9), None);
		// Next swipe, the other way.
		assert_eq!(s.event(PHASE_BEGAN, MOTION_HORIZONTAL, 0.0), None);
		assert_eq!(s.event(2, MOTION_HORIZONTAL, -0.3), Some(SwipeDir::Right));
		assert_eq!(s.event(PHASE_ENDED, MOTION_HORIZONTAL, -0.3), None);
	}

	#[test]
	fn vertical_swipes_and_a_missing_begin() {
		let mut s = DockSwipe::new();
		// No began seen (the tap came up mid-gesture): still one direction.
		assert_eq!(s.event(2, MOTION_VERTICAL, -0.5), Some(SwipeDir::Up));
		assert_eq!(s.event(PHASE_ENDED, MOTION_VERTICAL, -0.5), None);
		assert_eq!(s.event(PHASE_BEGAN, MOTION_VERTICAL, 0.0), None);
		assert_eq!(s.event(PHASE_ENDED, MOTION_VERTICAL, 0.5), Some(SwipeDir::Down));
	}

	#[test]
	fn small_cancelled_or_unknown_swipes_do_nothing() {
		let mut s = DockSwipe::new();
		assert_eq!(s.event(PHASE_BEGAN, MOTION_HORIZONTAL, 0.0), None);
		assert_eq!(s.event(PHASE_CANCELLED, MOTION_HORIZONTAL, 0.05), None);
		assert_eq!(s.event(PHASE_BEGAN, 4, 0.0), None);
		assert_eq!(s.event(2, 4, 0.9), None, "pinch-style motion");
		assert_eq!(s.event(2, MOTION_HORIZONTAL, f64::NAN), None);
	}

	#[test]
	fn forward_flag() {
		set_forward_gestures(true);
		assert!(forward_gestures());
		set_forward_gestures(false);
		assert!(!forward_gestures());
	}
}
