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

/// 0=left, 1=right, 2=middle (anything else = left, as in DesktopInput).
pub fn button_flag(button: u8, down: bool) -> u32 {
	match (button, down) {
		(1, true) => MOUSEEVENTF_RIGHTDOWN,
		(1, false) => MOUSEEVENTF_RIGHTUP,
		(2, true) => MOUSEEVENTF_MIDDLEDOWN,
		(2, false) => MOUSEEVENTF_MIDDLEUP,
		(_, true) => MOUSEEVENTF_LEFTDOWN,
		(_, false) => MOUSEEVENTF_LEFTUP,
	}
}

/// Fractional wheel accumulator (browser pixels → notches, remainder carried).
#[derive(Default, Debug)]
pub struct ScrollAcc {
	v: f64,
	h: f64,
}

impl ScrollAcc {
	/// Returns whole `(vertical, horizontal)` notches to emit now. Browser down(+) → wheel
	/// down (negative), so dy is negated.
	pub fn push(&mut self, dx: f64, dy: f64) -> (i32, i32) {
		if dx.is_finite() {
			self.h += dx / 100.0;
		}
		if dy.is_finite() {
			self.v += -dy / 100.0;
		}
		let v = self.v.trunc() as i32;
		let h = self.h.trunc() as i32;
		self.v -= v as f64;
		self.h -= h as f64;
		(v, h)
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
		assert_eq!(s.push(0.0, 40.0), (0, 0));
		assert_eq!(s.push(0.0, 40.0), (0, 0));
		assert_eq!(s.push(0.0, 40.0), (-1, 0)); // 120 px down → one notch down
		assert_eq!(s.push(250.0, 0.0), (0, 2));
		assert_eq!(s.push(60.0, 0.0), (0, 1)); // 0.5 + 0.6 carried
	}

	#[test]
	fn buttons() {
		assert_eq!(button_flag(0, true), MOUSEEVENTF_LEFTDOWN);
		assert_eq!(button_flag(1, false), MOUSEEVENTF_RIGHTUP);
		assert_eq!(button_flag(2, true), MOUSEEVENTF_MIDDLEDOWN);
		assert_eq!(button_flag(9, false), MOUSEEVENTF_LEFTUP);
	}
}
