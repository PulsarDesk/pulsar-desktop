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
	use pulsar_core::input::{DesktopInput, MonitorRect};
	use pulsar_sysagent::{SysInput, SysInputError};
	use std::time::{Duration, Instant};

	const RETRY: Duration = Duration::from_secs(5);

	pub(crate) struct HostInput {
		desk: DesktopInput,
		sys: Option<SysInput>,
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
		pub(crate) fn flush_held(&mut self) {
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
