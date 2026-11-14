//! Host desktop injector that prefers the in-session SYSTEM helper (`pulsar-sysagent`) and
//! falls back to plain `SendInput` (`DesktopInput`).
//!
//! Why: the host runs at High integrity, and UIPI blocks its SendInput into System-integrity
//! windows — the UAC consent prompt, the lock screen, the secure desktop. The helper runs as
//! SYSTEM in the same session and follows the input desktop, so remote control keeps working
//! there. When the service isn't installed / its helper isn't up, every event goes through
//! `DesktopInput` exactly as before, and the helper is retried every few seconds.
//!
//! Same method set as `DesktopInput`, so the host's dispatch code only changed its type.

#[cfg(not(windows))]
pub(crate) type HostInput = pulsar_core::input::DesktopInput;

#[cfg(windows)]
pub(crate) use imp::HostInput;

#[cfg(windows)]
mod imp {
	use pulsar_core::input::pen_touch::{PenState, PenTouch, PointerBackend, SynthContact, TouchFrame, WinSink};
	use pulsar_core::input::{DesktopInput, MonitorRect};
	use pulsar_sysagent::proto::PointerContact;
	use pulsar_sysagent::{SysInput, SysInputError, SysPointer};
	use std::time::{Duration, Instant};

	const RETRY: Duration = Duration::from_secs(5);

	/// The pen / touch sink's frames → the SYSTEM helper's pipe (protocol v2), which injects
	/// them on its own synthetic devices on the current input desktop.
	struct SysBackend(SysPointer);

	impl PointerBackend for SysBackend {
		fn inject(&mut self, pen: bool, contacts: &[SynthContact]) -> bool {
			let c: Vec<PointerContact> = contacts.iter().map(to_wire).collect();
			self.0.inject(pen, &c).is_ok()
		}
	}

	/// pulsar-core's contact → the helper's wire contact (ranges already clamped by the sink).
	pub(super) fn to_wire(c: &SynthContact) -> PointerContact {
		PointerContact {
			id: c.id.min(255) as u8,
			flags: c.flags,
			x: c.x,
			y: c.y,
			pressure: c.pressure.min(1024) as u16,
			rotation: (c.rotation % 360) as u16,
			tilt_x: c.tilt_x.clamp(-90, 90) as i8,
			tilt_y: c.tilt_y.clamp(-90, 90) as i8,
			pen_flags: (c.pen_flags & 0x7) as u8,
		}
	}

	pub(crate) struct HostInput {
		desk: DesktopInput,
		sys: Option<SysInput>,
		/// Pen / touch driver injecting through the helper (when it speaks protocol v2), so
		/// pen and touch reach UAC prompts and the lock screen like the mouse does.
		sys_pt: Option<PenTouch<Option<MonitorRect>>>,
		next_try: Instant,
		monitor: Option<MonitorRect>,
		/// Log the helper state once per change, not per event.
		logged_missing: bool,
	}

	impl HostInput {
		pub(crate) fn new() -> std::io::Result<Self> {
			let mut s = Self {
				desk: DesktopInput::new()?,
				sys: None,
				sys_pt: None,
				next_try: Instant::now(),
				monitor: None,
				logged_missing: false,
			};
			s.ensure_sys();
			Ok(s)
		}

		fn ensure_sys(&mut self) {
			if self.sys.is_some() || Instant::now() < self.next_try {
				return;
			}
			self.next_try = Instant::now() + RETRY;
			match SysInput::connect(pulsar_core::input::evdev_to_raw_key) {
				Ok(mut s) => {
					s.set_monitor(self.monitor.map(conv));
					tracing::info!(session = s.session(), "input: SYSTEM helper connected (UAC / lock screen reachable)");
					self.logged_missing = false;
					self.sys = Some(s);
				}
				Err(e) => {
					if !self.logged_missing {
						let lvl_warn = matches!(e, SysInputError::Untrusted(_));
						if lvl_warn {
							tracing::warn!(err = %e, "input: SYSTEM helper refused; using SendInput");
						} else {
							tracing::info!(err = %e, "input: SYSTEM helper unavailable; using SendInput");
						}
						self.logged_missing = true;
					}
				}
			}
		}

		/// Run `f` on the helper; on failure drop it and replay the event through SendInput.
		fn with(
			&mut self,
			f: impl FnOnce(&mut SysInput) -> Result<(), SysInputError>,
			fallback: impl FnOnce(&mut DesktopInput),
		) {
			self.ensure_sys();
			if let Some(s) = self.sys.as_mut() {
				match f(s) {
					Ok(()) => return,
					Err(e) => {
						tracing::warn!(err = %e, "input: SYSTEM helper lost; falling back to SendInput");
						self.sys = None;
						self.sys_pt = None;
						self.next_try = Instant::now() + RETRY;
					}
				}
			}
			fallback(&mut self.desk);
		}

		pub(crate) fn set_monitor(&mut self, rect: Option<MonitorRect>) {
			self.monitor = rect;
			self.desk.set_monitor(rect);
			if let Some(s) = self.sys.as_mut() {
				s.set_monitor(rect.map(conv));
			}
			if let Some(w) = &self.sys_pt {
				w.set_geom(rect);
			}
		}
		pub(crate) fn pointer(&mut self, x: f64, y: f64) {
			self.with(|s| s.pointer(x, y), |d| d.pointer(x, y));
		}
		pub(crate) fn pointer_relative(&mut self, dx: f64, dy: f64) {
			self.with(|s| s.pointer_relative(dx, dy), |d| d.pointer_relative(dx, dy));
		}
		pub(crate) fn button(&mut self, button: u8, down: bool) {
			self.with(|s| s.button(button, down), |d| d.button(button, down));
		}
		pub(crate) fn scroll(&mut self, dx: f64, dy: f64) {
			self.with(|s| s.scroll(dx, dy), |d| d.scroll(dx, dy));
		}
		pub(crate) fn key(&mut self, code: u32, down: bool) {
			self.with(|s| s.key(code, down), |d| d.key(code, down));
		}
		pub(crate) fn type_char(&mut self, c: char) {
			self.with(|s| s.type_char(c), |d| d.type_char(c));
		}
		pub(crate) fn pinch(&mut self, phase: u8, scale: f64) {
			self.with(|s| s.pinch(phase, scale), |d| d.pinch(phase, scale));
		}
		/// Pen / touch through the helper when it is up and speaks protocol v2 (the pen /
		/// touch driver runs here, the helper only injects its frames on the input desktop);
		/// otherwise the in-process synthetic devices (which UIPI keeps out of UAC / the
		/// lock screen).
		fn sys_pointer(&mut self) -> bool {
			self.ensure_sys();
			if self.sys.as_ref().is_some_and(|s| !s.is_alive()) {
				tracing::warn!("input: SYSTEM helper lost; falling back to SendInput");
				self.sys = None;
				self.next_try = Instant::now() + RETRY;
			}
			let Some(ptr) = self.sys.as_ref().and_then(|s| s.pen_touch()) else {
				self.sys_pt = None;
				return false;
			};
			if self.sys_pt.is_none() {
				let w = PenTouch::spawn("pulsar-sys-pen-touch", move || Some(WinSink::with_backend(SysBackend(ptr))));
				w.set_geom(self.monitor);
				self.sys_pt = Some(w);
			}
			true
		}
		pub(crate) fn pen(&mut self, s: PenState) {
			if self.sys_pointer() {
				if let Some(w) = &self.sys_pt {
					w.pen(s);
				}
			} else {
				self.desk.pen(s);
			}
		}
		pub(crate) fn touch(&mut self, f: TouchFrame) {
			if self.sys_pointer() {
				if let Some(w) = &self.sys_pt {
					w.touch(f);
				}
			} else {
				self.desk.touch(f);
			}
		}
		pub(crate) fn flush_held(&mut self) {
			if let Some(w) = &self.sys_pt {
				w.release();
			}
			if let Some(s) = self.sys.as_mut() {
				let _ = s.flush_held();
			}
			self.desk.flush_held();
		}
	}

	fn conv(r: MonitorRect) -> pulsar_sysagent::MonitorRect {
		pulsar_sysagent::MonitorRect {
			mon_left: r.mon_left,
			mon_top: r.mon_top,
			mon_width: r.mon_width,
			mon_height: r.mon_height,
			virt_left: r.virt_left,
			virt_top: r.virt_top,
			virt_width: r.virt_width,
			virt_height: r.virt_height,
		}
	}
}
