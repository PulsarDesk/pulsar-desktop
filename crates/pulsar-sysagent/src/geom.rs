//! Pure input math shared with `pulsar_core::input::DesktopInput` (same formulas, so a
//! session behaves identically whichever injector it uses). Cross-platform + tested.

use crate::proto::{
	Rect, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
	MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
	MOUSEEVENTF_VIRTUALDESK,
};

/// Same fields as `pulsar_core::input::MonitorRect` (virtual-desktop pixels), so the host
/// fills it from `pulsar_capture::display_rect()` exactly like the SendInput path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MonitorRect {
	pub mon_left: i32,
	pub mon_top: i32,
	pub mon_width: i32,
	pub mon_height: i32,
	pub virt_left: i32,
	pub virt_top: i32,
	pub virt_width: i32,
	pub virt_height: i32,
}

impl MonitorRect {
	/// The streamed monitor's own rect (the cursor clamp target after relative moves).
	pub fn monitor(&self) -> Option<Rect> {
		(self.mon_width > 0 && self.mon_height > 0).then_some(Rect {
			left: self.mon_left,
			top: self.mon_top,
			width: self.mon_width,
			height: self.mon_height,
		})
	}
}

pub const WHEEL_DELTA: i32 = 120;

/// Normalized (0..1) pointer → `(dx, dy, flags)` for an absolute `MOUSEINPUT`.
/// With a monitor rect: placed inside that monitor, normalized across the whole virtual
/// desktop (`MOUSEEVENTF_VIRTUALDESK`). Without: primary-display mapping.
pub fn absolute_move(x: f64, y: f64, monitor: Option<MonitorRect>) -> (i32, i32, u32) {
	let x = if x.is_finite() {
		x.clamp(0.0, 1.0)
	} else {
		0.0
	};
	let y = if y.is_finite() {
		y.clamp(0.0, 1.0)
	} else {
		0.0
	};
	match monitor {
		Some(m) if m.virt_width > 0 && m.virt_height > 0 => {
			let px = m.mon_left as f64 + x * m.mon_width as f64;
			let py = m.mon_top as f64 + y * m.mon_height as f64;
			let nx = (px - m.virt_left as f64) / m.virt_width as f64;
			let ny = (py - m.virt_top as f64) / m.virt_height as f64;
			(
				(nx * 65535.0).round() as i32,
				(ny * 65535.0).round() as i32,
				MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK | MOUSEEVENTF_MOVE,
			)
		}
		_ => (
			(x * 65535.0).round() as i32,
			(y * 65535.0).round() as i32,
			MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_MOVE,
		),
	}
}

/// 0=left, 1=right, 2=middle. Anything else (the side buttons 3 / 4 included: the helper
/// protocol carries no XBUTTON flags) is `None` and ignored, never a left click.
pub fn button_flag(button: u8, down: bool) -> Option<u32> {
	Some(match (button, down) {
		(0, true) => MOUSEEVENTF_LEFTDOWN,
		(0, false) => MOUSEEVENTF_LEFTUP,
		(1, true) => MOUSEEVENTF_RIGHTDOWN,
		(1, false) => MOUSEEVENTF_RIGHTUP,
		(2, true) => MOUSEEVENTF_MIDDLEDOWN,
		(2, false) => MOUSEEVENTF_MIDDLEUP,
		_ => return None,
	})
}

/// Hi-res wheel accumulator: browser pixels (100 per notch) → `mouseData` units of 1/120
/// notch, sub-unit remainder carried. Windows takes sub-`WHEEL_DELTA` wheel data (precision
/// touchpads send it), so smooth client scrolling stays smooth on the secure desktop too —
/// the same conversion as pulsar-core's `input::wheel::HiResScroll` for the user desktop.
///
/// Whole-notch mode ([`set_whole_notches`](Self::set_whole_notches)) emits only completed
/// notches × `WHEEL_DELTA` instead — the helper injects ALL of a session's input while it is
/// up, games included, and games often do `delta / WHEEL_DELTA` as an integer (a 15-unit step
/// = a dead wheel). The host turns it on for a game session / a relative pointer, as it does
/// for its own `DesktopInput`.
#[derive(Default, Debug)]
pub struct ScrollAcc {
	v: f64,
	h: f64,
	nv: NotchCounter,
	nh: NotchCounter,
	whole_notches: bool,
}

/// Legacy-notch counter over hi-res units: a notch fires once the remainder passes HALF a
/// notch in one direction, a reversal voids the remainder (pulsar-core
/// `input::wheel::HiResScroll`, the kernel's hid scroll counter).
#[derive(Default, Debug)]
struct NotchCounter {
	rem: i32,
	dir: i8,
}

impl NotchCounter {
	fn push(&mut self, units: i32) -> i32 {
		if units == 0 {
			return 0;
		}
		let dir: i8 = if units > 0 { 1 } else { -1 };
		if dir != self.dir {
			self.dir = dir;
			self.rem = 0;
		}
		self.rem = self.rem.saturating_add(units);
		if self.rem.abs() < WHEEL_DELTA / 2 {
			return 0;
		}
		let mut n = self.rem / WHEEL_DELTA;
		if n == 0 {
			n = dir as i32;
		}
		self.rem -= n * WHEEL_DELTA;
		n
	}
}

impl ScrollAcc {
	/// Whole `WHEEL_DELTA` notches (`true`) or hi-res units (`false`, the default).
	pub fn set_whole_notches(&mut self, on: bool) {
		self.whole_notches = on;
	}

	/// Returns the `(vertical, horizontal)` `mouseData` to emit now (0 = nothing). Browser
	/// down(+) → wheel down (negative), so dy is negated.
	pub fn push(&mut self, dx: f64, dy: f64) -> (i32, i32) {
		let (v, h) = self.push_units(dx, dy);
		// The notch counters always run, so a mode switch mid-scroll keeps its place.
		let (nv, nh) = (self.nv.push(v), self.nh.push(h));
		if self.whole_notches {
			(nv.saturating_mul(WHEEL_DELTA), nh.saturating_mul(WHEEL_DELTA))
		} else {
			(v, h)
		}
	}

	fn push_units(&mut self, dx: f64, dy: f64) -> (i32, i32) {
		let per_px = WHEEL_DELTA as f64 / 100.0;
		if dx.is_finite() {
			self.h += dx * per_px;
		}
		if dy.is_finite() {
			self.v += -dy * per_px;
		}
		// The 1e-9 nudge keeps float drift (0.6 × 10 = 5.999…) from holding a unit back.
		let whole = |x: f64| (x + 1e-9_f64.copysign(x)).trunc();
		let v = whole(self.v);
		let h = whole(self.h);
		self.v -= v;
		self.h -= h;
		(v.clamp(i32::MIN as f64, i32::MAX as f64) as i32, h.clamp(i32::MIN as f64, i32::MAX as f64) as i32)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn absolute_primary_mapping() {
		assert_eq!(
			absolute_move(0.0, 0.0, None),
			(0, 0, MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_MOVE)
		);
		assert_eq!(absolute_move(1.0, 1.0, None).0, 65535);
		assert_eq!(absolute_move(2.0, -1.0, None).0, 65535, "clamped");
		assert_eq!(absolute_move(f64::NAN, 0.5, None).0, 0);
	}

	#[test]
	fn absolute_second_monitor_mapping() {
		// Two 1920x1080 monitors side by side, streaming the RIGHT one.
		let m = MonitorRect {
			mon_left: 1920,
			mon_top: 0,
			mon_width: 1920,
			mon_height: 1080,
			virt_left: 0,
			virt_top: 0,
			virt_width: 3840,
			virt_height: 1080,
		};
		let (dx, dy, f) = absolute_move(0.0, 0.5, Some(m));
		assert_eq!(
			f,
			MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK | MOUSEEVENTF_MOVE
		);
		assert_eq!(dx, 32768); // halfway across the virtual desktop
		assert_eq!(dy, 32768);
		assert_eq!(absolute_move(1.0, 1.0, Some(m)).0, 65535);
		assert_eq!(
			m.monitor(),
			Some(Rect {
				left: 1920,
				top: 0,
				width: 1920,
				height: 1080
			})
		);
	}

	#[test]
	fn scroll_accumulates_fractions() {
		let mut s = ScrollAcc::default();
		assert_eq!(s.push(0.0, 100.0), (-120, 0)); // one notch down
		assert_eq!(s.push(0.0, 12.5), (-15, 0)); // a hi-res 1/8 notch is sent, not rounded away
		assert_eq!(s.push(250.0, 0.0), (0, 300));
		assert_eq!(s.push(0.0, -0.5), (0, 0)); // 0.6 unit carried…
		assert_eq!(s.push(0.0, -0.5), (1, 0)); // …1.2 → 1, 0.2 carried
	}

	#[test]
	fn whole_notch_mode_for_games() {
		let mut s = ScrollAcc::default();
		s.set_whole_notches(true);
		assert_eq!(s.push(0.0, 100.0), (-120, 0));
		// 1/8-notch steps: nothing until half a notch built up, then one whole notch.
		assert_eq!(s.push(0.0, 12.5), (0, 0));
		assert_eq!(s.push(0.0, 12.5), (0, 0));
		assert_eq!(s.push(0.0, 12.5), (0, 0));
		assert_eq!(s.push(0.0, 12.5), (-120, 0));
		for _ in 0..4 {
			assert_eq!(s.push(0.0, 12.5), (0, 0)); // the rest of that detent
		}
		assert_eq!(s.push(-300.0, 0.0), (0, -360));
		// Back to hi-res units.
		s.set_whole_notches(false);
		assert_eq!(s.push(0.0, 12.5), (-15, 0));
	}

	#[test]
	fn buttons() {
		assert_eq!(button_flag(0, true), Some(MOUSEEVENTF_LEFTDOWN));
		assert_eq!(button_flag(1, false), Some(MOUSEEVENTF_RIGHTUP));
		assert_eq!(button_flag(2, true), Some(MOUSEEVENTF_MIDDLEDOWN));
		assert_eq!(button_flag(3, true), None);
		assert_eq!(button_flag(9, false), None);
	}
}
