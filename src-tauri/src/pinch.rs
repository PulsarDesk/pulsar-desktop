//! Touchpad pinch forwarding (client side), shared by every capture path.
//!
//! A local touchpad pinch reaches the host in one of two ways:
//! - hosts advertising `FEAT_PINCH` get the gesture itself (`InputEvent::Pinch` begin / update /
//!   end with the cumulative scale) and inject a real pinch (Windows: touch injection; Linux: a
//!   virtual touchpad; macOS: a magnify gesture);
//! - every other host gets Ctrl + wheel steps — what Windows synthesizes for a touchpad pinch
//!   and what browsers, image viewers and office apps treat as zoom.
//!
//! The capture paths that feed this:
//! - Windows: the LL mouse hook sees Windows' synthesized Ctrl + wheel (`kbdhook::imp`);
//! - Linux: [`TouchTracker`] reads the touchpad's multitouch contacts over evdev
//!   (`kbdhook::linux_pinch`);
//! - webview sessions (macOS, and the webview fallback elsewhere): WebKit `gesture*` events or
//!   the Ctrl + wheel Chromium/WebKit synthesize, sent through the `input_pinch` command
//!   (`src/lib/screens/Session/input.svelte.ts`).

// The touch tracker is fed by the Linux reader only; the forwarder by every path.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use pulsar_core::service::InputEvent;
use std::sync::atomic::{AtomicBool, Ordering};

/// The connected host injects real pinch gestures (`FEAT_PINCH`); set per session by play.rs.
static HOST_PINCH: AtomicBool = AtomicBool::new(false);

pub fn set_host_pinch(on: bool) {
	HOST_PINCH.store(on, Ordering::SeqCst);
}

pub fn host_pinch() -> bool {
	HOST_PINCH.load(Ordering::SeqCst)
}

/// Zoom per wheel notch of the Ctrl + wheel fallback (Windows synthesizes one 120-unit notch
/// per ~12 % of pinch; the Windows client uses the same factor the other way round).
pub const PINCH_STEP: f64 = 1.12;
/// Wire scroll units (browser wheel pixels) per notch.
const NOTCH_PX: f64 = 100.0;
/// Smallest Ctrl + wheel step sent (a quarter notch): finer steps would flood the host with
/// Ctrl down/up pairs for no visible zoom.
const MIN_NOTCHES: f64 = 0.25;
/// evdev KEY_LEFTCTRL.
const CTRL: u32 = 29;

/// Sanitize a cumulative scale from any source.
pub fn clamp_scale(s: f64) -> f64 {
	if s.is_finite() && s > 0.0 {
		s.clamp(0.05, 20.0)
	} else {
		1.0
	}
}

/// Turns one pinch (begin, cumulative scale updates, end) into the wire events for the host.
#[derive(Debug, Default)]
pub struct PinchForwarder {
	/// A pinch is open.
	active: bool,
	/// This pinch goes out as `InputEvent::Pinch` (host injects gestures), else Ctrl + wheel.
	real: bool,
	/// Last cumulative scale seen.
	scale: f64,
	/// Fallback: the scale the Ctrl + wheel steps sent so far add up to.
	sent: f64,
}

impl PinchForwarder {
	pub fn new() -> Self {
		Self::default()
	}

	pub fn active(&self) -> bool {
		self.active
	}

	/// Open a pinch. `real` = the host injects pinch gestures. An open pinch is closed first.
	pub fn begin(&mut self, real: bool) -> Vec<InputEvent> {
		let mut out = self.end();
		self.active = true;
		self.real = real;
		self.scale = 1.0;
		self.sent = 1.0;
		if real {
			out.push(InputEvent::Pinch { phase: 0, scale: 1.0 });
		}
		out
	}

	/// The pinch's cumulative scale since begin (1.0 = fingers where they started).
	pub fn update(&mut self, scale: f64) -> Vec<InputEvent> {
		if !self.active {
			return Vec::new();
		}
		let scale = clamp_scale(scale);
		self.scale = scale;
		if self.real {
			return vec![InputEvent::Pinch { phase: 1, scale }];
		}
		let notches = (scale / self.sent).ln() / PINCH_STEP.ln();
		if notches.abs() < MIN_NOTCHES {
			return Vec::new();
		}
		self.sent = scale;
		// Wire scroll: +dy = down; a wheel UP (negative dy) with Ctrl = zoom in.
		vec![
			InputEvent::Key { code: CTRL, down: true },
			InputEvent::Scroll { dx: 0.0, dy: -notches * NOTCH_PX },
			InputEvent::Key { code: CTRL, down: false },
		]
	}

	/// Close the pinch (no-op when none is open).
	pub fn end(&mut self) -> Vec<InputEvent> {
		if !std::mem::take(&mut self.active) {
			return Vec::new();
		}
		if self.real {
			vec![InputEvent::Pinch { phase: 2, scale: self.scale }]
		} else {
			Vec::new()
		}
	}
}

/// What [`TouchTracker`] recognised at a frame boundary.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Gesture {
	Begin,
	/// Cumulative scale since the fingers went down.
	Update(f64),
	End,
}

#[derive(Debug, Clone, Copy)]
struct Slot {
	id: i32,
	x: f64,
	y: f64,
}

/// Distance between two contacts and their midpoint, in mm.
#[derive(Debug, Clone, Copy)]
struct Pair {
	dist: f64,
	cx: f64,
	cy: f64,
}

/// Two-finger pinch recognition from raw multitouch (evdev protocol B) contacts — the Linux
/// client's touchpad is not grabbed, so the pinch is read alongside the compositor.
///
/// A pinch starts once two fingers change their distance by `PINCH_MM` more than they move
/// together (a two-finger scroll moves both fingers the same way and keeps the distance); a
/// pair that first moves together by `SCROLL_MM` is a scroll and never becomes a pinch. The
/// scale is the finger distance over the distance when the second finger landed, so no motion
/// before recognition is lost. Lifting or adding a finger ends the pinch.
#[derive(Debug)]
pub struct TouchTracker {
	slots: Vec<Slot>,
	cur: usize,
	/// Units per mm of x / y (the axes' resolution).
	res: (f64, f64),
	/// Contacts at the moment there were exactly two.
	base: Option<Pair>,
	/// The current pair was classified as a two-finger scroll.
	scrolling: bool,
	pinching: bool,
	last_scale: f64,
}

const PINCH_MM: f64 = 2.5;
const SCROLL_MM: f64 = 4.0;
const MAX_SLOTS: usize = 16;

impl TouchTracker {
	/// `res_x` / `res_y`: the touchpad's ABS_MT_POSITION_* resolution (units per mm; 0 =
	/// unknown, then 1 unit is taken as 1/10 mm).
	pub fn new(res_x: i32, res_y: i32) -> Self {
		let r = |v: i32| if v > 0 { v as f64 } else { 10.0 };
		Self {
			slots: vec![Slot { id: -1, x: 0.0, y: 0.0 }; MAX_SLOTS],
			cur: 0,
			res: (r(res_x), r(res_y)),
			base: None,
			scrolling: false,
			pinching: false,
			last_scale: 1.0,
		}
	}

	pub fn set_slot(&mut self, slot: i32) {
		self.cur = (slot.max(0) as usize).min(MAX_SLOTS - 1);
	}

	pub fn set_tracking_id(&mut self, id: i32) {
		self.slots[self.cur].id = id;
	}

	pub fn set_x(&mut self, v: i32) {
		self.slots[self.cur].x = v as f64 / self.res.0;
	}

	pub fn set_y(&mut self, v: i32) {
		self.slots[self.cur].y = v as f64 / self.res.1;
	}

	/// The kernel dropped events (SYN_DROPPED): contact state is unknown — forget it all.
	pub fn reset(&mut self) -> Option<Gesture> {
		for s in &mut self.slots {
			s.id = -1;
		}
		self.base = None;
		self.scrolling = false;
		std::mem::take(&mut self.pinching).then_some(Gesture::End)
	}

	/// End of one frame (SYN_REPORT): classify the current contacts.
	pub fn frame(&mut self) -> Option<Gesture> {
		let mut down = self.slots.iter().filter(|s| s.id >= 0);
		let pair = match (down.next(), down.next(), down.next()) {
			(Some(a), Some(b), None) => {
				let (dx, dy) = (b.x - a.x, b.y - a.y);
				Some(Pair {
					dist: (dx * dx + dy * dy).sqrt(),
					cx: (a.x + b.x) / 2.0,
					cy: (a.y + b.y) / 2.0,
				})
			}
			_ => None,
		};
		let Some(p) = pair else {
			self.base = None;
			self.scrolling = false;
			return std::mem::take(&mut self.pinching).then_some(Gesture::End);
		};
		let Some(b) = self.base else {
			self.base = Some(p);
			return None;
		};
		let scale = if b.dist > 0.5 { p.dist / b.dist } else { 1.0 };
		if self.pinching {
			if (scale - self.last_scale).abs() < 0.002 {
				return None;
			}
			self.last_scale = scale;
			return Some(Gesture::Update(scale));
		}
		if self.scrolling {
			return None;
		}
		let spread = (p.dist - b.dist).abs();
		let moved = ((p.cx - b.cx).powi(2) + (p.cy - b.cy).powi(2)).sqrt();
		if spread >= PINCH_MM && spread > moved {
			self.pinching = true;
			self.last_scale = scale;
			return Some(Gesture::Begin);
		}
		if moved >= SCROLL_MM && moved > spread {
			self.scrolling = true;
		}
		None
	}

	/// The scale of the open pinch (valid right after `Gesture::Begin`).
	pub fn scale(&self) -> f64 {
		self.last_scale
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn ctrl_wheel_dy(ev: &[InputEvent]) -> Option<f64> {
		match ev {
			[InputEvent::Key { code: 29, down: true }, InputEvent::Scroll { dx, dy }, InputEvent::Key { code: 29, down: false }] =>
			{
				assert_eq!(*dx, 0.0);
				Some(*dy)
			}
			[] => None,
			other => panic!("unexpected {other:?}"),
		}
	}

	#[test]
	fn real_pinch_forwards_the_gesture() {
		let mut f = PinchForwarder::new();
		assert!(matches!(f.begin(true)[..], [InputEvent::Pinch { phase: 0, scale }] if scale == 1.0));
		assert!(matches!(f.update(1.3)[..], [InputEvent::Pinch { phase: 1, scale }] if scale == 1.3));
		assert!(matches!(f.end()[..], [InputEvent::Pinch { phase: 2, scale }] if scale == 1.3));
		assert!(f.end().is_empty());
		assert!(f.update(2.0).is_empty(), "no update without a begin");
	}

	#[test]
	fn fallback_sends_ctrl_wheel_steps_that_add_up_to_the_scale() {
		let mut f = PinchForwarder::new();
		assert!(f.begin(false).is_empty());
		// Below a quarter notch: nothing yet.
		assert_eq!(ctrl_wheel_dy(&f.update(1.01)), None);
		// One notch of zoom-in = wheel up (negative dy) by 100 px.
		let dy = ctrl_wheel_dy(&f.update(PINCH_STEP)).unwrap();
		assert!((dy + 100.0).abs() < 1e-6, "{dy}");
		// Back to 1.0 = one notch out.
		let dy = ctrl_wheel_dy(&f.update(1.0)).unwrap();
		assert!((dy - 100.0).abs() < 1e-6, "{dy}");
		assert!(f.end().is_empty());
		assert!(!f.active());
	}

	#[test]
	fn begin_closes_an_open_pinch() {
		let mut f = PinchForwarder::new();
		f.begin(true);
		f.update(0.5);
		let ev = f.begin(true);
		assert!(matches!(
			ev[..],
			[InputEvent::Pinch { phase: 2, scale: a }, InputEvent::Pinch { phase: 0, scale: b }] if a == 0.5 && b == 1.0
		));
	}

	#[test]
	fn scales_are_sanitized() {
		assert_eq!(clamp_scale(f64::NAN), 1.0);
		assert_eq!(clamp_scale(-1.0), 1.0);
		assert_eq!(clamp_scale(1000.0), 20.0);
	}

	/// Two contacts at x0/x1 (mm, resolution 1) on one row at y, moved by `dy` together.
	fn two(t: &mut TouchTracker, x0: i32, x1: i32, y: i32) -> Option<Gesture> {
		t.set_slot(0);
		t.set_x(x0);
		t.set_y(y);
		t.set_slot(1);
		t.set_x(x1);
		t.set_y(y);
		t.frame()
	}

	fn down(t: &mut TouchTracker) {
		t.set_slot(0);
		t.set_tracking_id(10);
		t.set_slot(1);
		t.set_tracking_id(11);
	}

	#[test]
	fn spreading_fingers_is_a_pinch() {
		let mut t = TouchTracker::new(1, 1);
		down(&mut t);
		assert_eq!(two(&mut t, 40, 60, 30), None); // base: 20 mm apart
		assert_eq!(two(&mut t, 39, 61, 30), None); // 2 mm: not yet
		assert_eq!(two(&mut t, 38, 62, 30), Some(Gesture::Begin));
		assert!((t.scale() - 1.2).abs() < 1e-9);
		assert_eq!(two(&mut t, 30, 70, 30), Some(Gesture::Update(2.0)));
		assert_eq!(two(&mut t, 30, 70, 30), None, "no motion, no update");
		t.set_slot(1);
		t.set_tracking_id(-1);
		assert_eq!(t.frame(), Some(Gesture::End));
		assert_eq!(t.frame(), None);
	}

	#[test]
	fn two_finger_scroll_never_becomes_a_pinch() {
		let mut t = TouchTracker::new(1, 1);
		down(&mut t);
		assert_eq!(two(&mut t, 40, 60, 30), None);
		assert_eq!(two(&mut t, 40, 60, 35), None); // both fingers 5 mm down = scroll
		assert_eq!(two(&mut t, 30, 70, 40), None, "a scroll stays a scroll");
	}

	#[test]
	fn resolution_makes_units_millimetres() {
		// 40 units/mm: a 3 mm spread is 120 units.
		let mut t = TouchTracker::new(40, 40);
		down(&mut t);
		assert_eq!(two(&mut t, 1600, 2400, 1000), None);
		assert_eq!(two(&mut t, 1540, 2460, 1000), Some(Gesture::Begin));
	}

	#[test]
	fn dropped_events_end_the_pinch() {
		let mut t = TouchTracker::new(1, 1);
		down(&mut t);
		two(&mut t, 40, 60, 30);
		assert_eq!(two(&mut t, 35, 65, 30), Some(Gesture::Begin));
		assert_eq!(t.reset(), Some(Gesture::End));
		assert_eq!(t.reset(), None);
	}

	#[test]
	fn a_third_finger_ends_the_pinch() {
		let mut t = TouchTracker::new(1, 1);
		down(&mut t);
		two(&mut t, 40, 60, 30);
		assert_eq!(two(&mut t, 35, 65, 30), Some(Gesture::Begin));
		t.set_slot(2);
		t.set_tracking_id(12);
		assert_eq!(t.frame(), Some(Gesture::End));
	}
}
