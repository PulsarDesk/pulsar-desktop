//! Multi-finger touchpad gestures (client side), shared by every capture path.
//!
//! While the user controls a remote session, a 3- or 4-finger swipe or tap on the local
//! touchpad goes to the host as `InputEvent::Gesture { fingers, kind }`; the host does what
//! ITS user configured for that gesture (pulsar-core `input::gesture`). Only hosts that
//! advertise `FEAT_GESTURE` get it, and only while the setting "Touchpad hareketlerini gönder"
//! is on.
//!
//! The capture paths that feed [`Recognizer`]:
//! - Windows: Raw Input from the precision touchpad's HID reports (`kbdhook::win_gesture`),
//!   which also turns Windows' own 3/4-finger gestures off while control is engaged;
//! - Linux: the evdev touchpad reader (`kbdhook::linux_pinch`);
//! - macOS: the CGEventTap sees the Dock swipe gesture events instead (no raw contacts there)
//!   and swallows them while engaged (`kbdhook::mac`).

// The recognizer is fed by the Windows / Linux readers, the MT slots by the Linux one only.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use pulsar_core::input::gesture::GestureKind;
use pulsar_core::service::InputEvent;
use std::sync::atomic::{AtomicBool, Ordering};

/// The connected host performs touchpad gestures (`FEAT_GESTURE`); set per session by play.rs.
static HOST_GESTURE: AtomicBool = AtomicBool::new(false);
/// Settings → "Touchpad hareketlerini gönder" (default on).
static ENABLED: AtomicBool = AtomicBool::new(true);

pub fn set_host_gesture(on: bool) {
	HOST_GESTURE.store(on, Ordering::SeqCst);
}

pub fn set_enabled(on: bool) {
	ENABLED.store(on, Ordering::SeqCst);
}

pub fn enabled() -> bool {
	ENABLED.load(Ordering::SeqCst)
}

/// Gestures go to the host (setting on + the host performs them). The capture paths also gate
/// on their own "control engaged" state.
pub fn forwarding() -> bool {
	enabled() && HOST_GESTURE.load(Ordering::SeqCst)
}

pub fn event(fingers: u8, kind: GestureKind) -> InputEvent {
	InputEvent::Gesture { fingers, kind }
}

/// Swipe distance, as a fraction of the pad's width (horizontal) / height (vertical).
pub const SWIPE_X: f64 = 0.15;
pub const SWIPE_Y: f64 = 0.15;
/// The swipe must be this much more along its axis than across it.
const DOMINANCE: f64 = 1.3;
/// A tap: all fingers up within this time of the third finger landing, barely moved.
pub const TAP_MAX_MS: u64 = 300;
pub const TAP_MAX_MOVE: f64 = 0.03;

/// 3 / 4-finger swipe + tap recognition from touchpad contacts (positions normalized 0..1 per
/// axis, y grows downwards). One gesture per contact episode (first finger down → all up):
/// a swipe fires once the centroid of 3+ fingers moves past the threshold, a tap fires on the
/// last lift when 3+ fingers were down briefly and did not move. A change in the finger count
/// re-bases the centroid (fingers never land at the same instant).
#[derive(Debug, Default)]
pub struct Recognizer {
	in_episode: bool,
	fired: bool,
	max_fingers: usize,
	/// When 3+ fingers were first down in this episode.
	multi_since: Option<u64>,
	/// Finger count + centroid the current swipe is measured from.
	base: Option<(usize, f64, f64)>,
	/// Largest centroid travel seen with 3+ fingers (tap rejection).
	moved: f64,
}

impl Recognizer {
	pub fn new() -> Self {
		Self::default()
	}

	/// Forget the current episode (device lost, control released mid-gesture).
	pub fn reset(&mut self) {
		*self = Self::default();
	}

	/// One frame of contacts (only those touching the pad) at `t_ms`.
	pub fn frame(&mut self, t_ms: u64, contacts: &[(f64, f64)]) -> Option<(u8, GestureKind)> {
		let n = contacts.len();
		if n == 0 {
			let tap = self.in_episode
				&& !self.fired
				&& self.max_fingers >= 3
				&& self.moved <= TAP_MAX_MOVE
				&& self.multi_since.is_some_and(|t0| t_ms.saturating_sub(t0) <= TAP_MAX_MS);
			let fingers = self.max_fingers.min(4) as u8;
			self.reset();
			return tap.then_some((fingers, GestureKind::Tap));
		}
		if !self.in_episode {
			self.in_episode = true;
		}
		self.max_fingers = self.max_fingers.max(n);
		if n < 3 {
			self.base = None;
			return None;
		}
		self.multi_since.get_or_insert(t_ms);
		let (sx, sy) = contacts.iter().fold((0.0, 0.0), |(a, b), (x, y)| (a + x, b + y));
		let (cx, cy) = (sx / n as f64, sy / n as f64);
		let Some((_, bx, by)) = self.base.filter(|(bn, _, _)| *bn == n) else {
			self.base = Some((n, cx, cy));
			return None;
		};
		let (dx, dy) = (cx - bx, cy - by);
		self.moved = self.moved.max((dx * dx + dy * dy).sqrt());
		if self.fired {
			return None;
		}
		let kind = if dx.abs() >= SWIPE_X && dx.abs() > dy.abs() * DOMINANCE {
			if dx < 0.0 { GestureKind::SwipeLeft } else { GestureKind::SwipeRight }
		} else if dy.abs() >= SWIPE_Y && dy.abs() > dx.abs() * DOMINANCE {
			if dy < 0.0 { GestureKind::SwipeUp } else { GestureKind::SwipeDown }
		} else {
			return None;
		};
		self.fired = true;
		Some((n.min(4) as u8, kind))
	}
}

/// Multitouch (evdev protocol B) contacts of one touchpad as [`Recognizer`] input: positions
/// normalized by the axes' ranges. Many touchpads track fewer slots than fingers (2 slots +
/// `BTN_TOOL_TRIPLETAP` / `QUADTAP` for the count): the missing fingers are filled in at the
/// tracked fingers' centroid, so the count is right and the centroid unchanged.
#[derive(Debug)]
pub struct MtSlots {
	slots: Vec<(i32, f64, f64)>,
	cur: usize,
	x: (i32, i32),
	y: (i32, i32),
	/// Finger count from the BTN_TOOL_* keys (0 = none reported).
	tool: usize,
}

const MT_SLOTS: usize = 16;

impl MtSlots {
	/// `x` / `y`: ABS_MT_POSITION_X / _Y (min, max).
	pub fn new(x: (i32, i32), y: (i32, i32)) -> Self {
		let fix = |(lo, hi): (i32, i32)| if hi > lo { (lo, hi) } else { (0, 1) };
		Self { slots: vec![(-1, 0.0, 0.0); MT_SLOTS], cur: 0, x: fix(x), y: fix(y), tool: 0 }
	}
	pub fn set_slot(&mut self, slot: i32) {
		self.cur = (slot.max(0) as usize).min(MT_SLOTS - 1);
	}
	pub fn set_tracking_id(&mut self, id: i32) {
		self.slots[self.cur].0 = id;
	}
	pub fn set_x(&mut self, v: i32) {
		self.slots[self.cur].1 = ((v - self.x.0) as f64 / (self.x.1 - self.x.0) as f64).clamp(0.0, 1.0);
	}
	pub fn set_y(&mut self, v: i32) {
		self.slots[self.cur].2 = ((v - self.y.0) as f64 / (self.y.1 - self.y.0) as f64).clamp(0.0, 1.0);
	}
	/// A BTN_TOOL_* key edge: `fingers` = 1 (FINGER) .. 5 (QUINTTAP).
	pub fn set_tool(&mut self, fingers: usize, down: bool) {
		if down {
			self.tool = fingers;
		} else if self.tool == fingers {
			self.tool = 0;
		}
	}
	/// SYN_DROPPED: the contact state is unknown.
	pub fn reset(&mut self) {
		for s in &mut self.slots {
			s.0 = -1;
		}
		self.tool = 0;
	}
	/// The contacts at a SYN_REPORT.
	pub fn contacts(&self) -> Vec<(f64, f64)> {
		let mut c: Vec<(f64, f64)> = self.slots.iter().filter(|s| s.0 >= 0).map(|s| (s.1, s.2)).collect();
		if !c.is_empty() && self.tool > c.len() {
			let n = c.len() as f64;
			let (sx, sy) = c.iter().fold((0.0, 0.0), |(a, b), (x, y)| (a + x, b + y));
			c.resize(self.tool, (sx / n, sy / n));
		}
		c
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use GestureKind::*;

	#[test]
	fn mt_slots_normalize_and_fill_untracked_fingers() {
		let mut m = MtSlots::new((0, 1000), (0, 500));
		m.set_slot(0);
		m.set_tracking_id(5);
		m.set_x(200);
		m.set_y(250);
		m.set_slot(1);
		m.set_tracking_id(6);
		m.set_x(400);
		m.set_y(250);
		assert_eq!(m.contacts(), vec![(0.2, 0.5), (0.4, 0.5)]);
		// A 2-slot pad reporting three fingers.
		m.set_tool(2, false);
		m.set_tool(3, true);
		let c = m.contacts();
		assert_eq!(c.len(), 3);
		assert!((c[2].0 - 0.3).abs() < 1e-9 && (c[2].1 - 0.5).abs() < 1e-9);
		m.set_slot(1);
		m.set_tracking_id(-1);
		m.set_slot(0);
		m.set_tracking_id(-1);
		assert!(m.contacts().is_empty(), "no tracked finger, no contacts");
		m.set_slot(99); // clamped
		m.set_x(5000);
		m.reset();
		assert!(m.contacts().is_empty());
	}

	#[test]
	fn mt_slots_feed_a_swipe() {
		let mut m = MtSlots::new((0, 100), (0, 100));
		let mut r = Recognizer::new();
		let mut out = Vec::new();
		for (i, slot) in [0, 1, 2].iter().enumerate() {
			m.set_slot(*slot);
			m.set_tracking_id(10 + i as i32);
			m.set_x(20 + 10 * i as i32);
		}
		for step in 0..=10 {
			for slot in 0..3 {
				m.set_slot(slot);
				m.set_y(80 - step * 5);
			}
			out.extend(r.frame(step as u64 * 8, &m.contacts()));
		}
		assert_eq!(out, vec![(3, SwipeUp)]);
	}

	/// `n` fingers in a row around (x, y), 0.08 apart.
	fn fingers(n: usize, x: f64, y: f64) -> Vec<(f64, f64)> {
		(0..n).map(|i| (x + i as f64 * 0.08, y)).collect()
	}

	/// Feed a straight move of `n` fingers from (x0,y0) to (x1,y1) in `steps` frames 8 ms apart,
	/// then lift. Returns every recognised gesture.
	fn swipe(r: &mut Recognizer, n: usize, from: (f64, f64), to: (f64, f64), steps: u32) -> Vec<(u8, GestureKind)> {
		let mut out = Vec::new();
		let mut t = 1000;
		for i in 0..=steps {
			let f = i as f64 / steps as f64;
			let (x, y) = (from.0 + (to.0 - from.0) * f, from.1 + (to.1 - from.1) * f);
			out.extend(r.frame(t, &fingers(n, x, y)));
			t += 8;
		}
		out.extend(r.frame(t, &[]));
		out
	}

	#[test]
	fn three_finger_swipes_in_four_directions() {
		let mut r = Recognizer::new();
		assert_eq!(swipe(&mut r, 3, (0.3, 0.7), (0.3, 0.3), 20), vec![(3, SwipeUp)]);
		assert_eq!(swipe(&mut r, 3, (0.3, 0.3), (0.3, 0.7), 20), vec![(3, SwipeDown)]);
		assert_eq!(swipe(&mut r, 3, (0.6, 0.5), (0.2, 0.5), 20), vec![(3, SwipeLeft)]);
		assert_eq!(swipe(&mut r, 3, (0.2, 0.5), (0.6, 0.5), 20), vec![(3, SwipeRight)]);
	}

	#[test]
	fn four_and_five_fingers() {
		let mut r = Recognizer::new();
		assert_eq!(swipe(&mut r, 4, (0.2, 0.3), (0.2, 0.8), 20), vec![(4, SwipeDown)]);
		assert_eq!(swipe(&mut r, 5, (0.1, 0.5), (0.5, 0.5), 20), vec![(4, SwipeRight)]);
	}

	#[test]
	fn one_gesture_per_episode() {
		let mut r = Recognizer::new();
		// Down then all the way back up without lifting: only the first direction fires.
		let mut out = swipe_no_lift(&mut r, 3, (0.3, 0.2), (0.3, 0.8));
		out.extend(swipe_no_lift(&mut r, 3, (0.3, 0.8), (0.3, 0.1)));
		out.extend(r.frame(9999, &[]));
		assert_eq!(out, vec![(3, SwipeDown)]);
	}

	fn swipe_no_lift(r: &mut Recognizer, n: usize, from: (f64, f64), to: (f64, f64)) -> Vec<(u8, GestureKind)> {
		(0..=20)
			.filter_map(|i| {
				let f = i as f64 / 20.0;
				r.frame(1000 + i * 8, &fingers(n, from.0 + (to.0 - from.0) * f, from.1 + (to.1 - from.1) * f))
			})
			.collect()
	}

	#[test]
	fn small_or_diagonal_moves_are_not_swipes() {
		let mut r = Recognizer::new();
		assert!(swipe(&mut r, 3, (0.3, 0.5), (0.4, 0.55), 20).is_empty(), "10 % is below the threshold");
		assert!(swipe(&mut r, 3, (0.2, 0.2), (0.5, 0.5), 20).is_empty(), "45° is neither axis");
	}

	#[test]
	fn two_fingers_never_gesture() {
		let mut r = Recognizer::new();
		assert!(swipe(&mut r, 2, (0.3, 0.8), (0.3, 0.1), 20).is_empty());
		// A quick two-finger tap is not a gesture either.
		assert_eq!(r.frame(0, &fingers(2, 0.3, 0.3)), None);
		assert_eq!(r.frame(50, &[]), None);
	}

	#[test]
	fn taps() {
		let mut r = Recognizer::new();
		// Fingers land one after another, lift 120 ms later.
		assert_eq!(r.frame(0, &fingers(1, 0.4, 0.4)), None);
		assert_eq!(r.frame(10, &fingers(2, 0.4, 0.4)), None);
		assert_eq!(r.frame(20, &fingers(3, 0.4, 0.4)), None);
		assert_eq!(r.frame(80, &fingers(3, 0.401, 0.4)), None);
		assert_eq!(r.frame(140, &fingers(1, 0.4, 0.4)), None);
		assert_eq!(r.frame(150, &[]), Some((3, Tap)));
		// Four-finger tap.
		assert_eq!(r.frame(1000, &fingers(4, 0.3, 0.3)), None);
		assert_eq!(r.frame(1100, &[]), Some((4, Tap)));
		// Held too long: a rest, not a tap.
		assert_eq!(r.frame(2000, &fingers(3, 0.3, 0.3)), None);
		assert_eq!(r.frame(2600, &[]), None);
		// Moved: not a tap (and too short for a swipe).
		assert_eq!(r.frame(3000, &fingers(3, 0.3, 0.3)), None);
		assert_eq!(r.frame(3050, &fingers(3, 0.36, 0.3)), None);
		assert_eq!(r.frame(3100, &[]), None);
		// A swipe never also taps.
		assert_eq!(swipe(&mut r, 3, (0.3, 0.7), (0.3, 0.3), 5), vec![(3, SwipeUp)]);
	}

	#[test]
	fn a_landing_finger_rebases_instead_of_jumping() {
		let mut r = Recognizer::new();
		// Three fingers left of centre, a fourth lands far right: the centroid jumps by 0.15
		// but that is a re-base, not a swipe right.
		assert_eq!(r.frame(0, &fingers(3, 0.1, 0.5)), None);
		assert_eq!(r.frame(8, &fingers(3, 0.1, 0.5)), None);
		let mut four = fingers(3, 0.1, 0.5);
		four.push((0.9, 0.5));
		assert_eq!(r.frame(16, &four), None);
		assert_eq!(r.frame(24, &four), None);
	}

	#[test]
	fn reset_forgets_the_episode() {
		let mut r = Recognizer::new();
		r.frame(0, &fingers(3, 0.3, 0.3));
		r.reset();
		assert_eq!(r.frame(50, &[]), None, "no tap after a reset");
	}

	#[test]
	fn forwarding_needs_the_setting_and_the_host() {
		set_host_gesture(false);
		set_enabled(true);
		assert!(!forwarding());
		set_host_gesture(true);
		assert!(forwarding());
		set_enabled(false);
		assert!(!forwarding());
		set_enabled(true);
		set_host_gesture(false);
	}
}
