//! Keep the display (and the system) awake while a session needs it — host and client side.
//!
//! **Host:** held for every session from its first stream request until it ends, on every
//! capture path (Windows NVENC-native / AMF / QSV / software / ffmpeg, macOS ScreenCaptureKit /
//! avfoundation, Linux x11grab / Wayland portal / libav). Before, only the Windows NVENC-native
//! thread (`SetThreadExecutionState`, display only) and macOS ScreenCaptureKit held one: an
//! AMD / Intel / Snapdragon Windows host, an NVIDIA HDR or 4:4:4 stream and every Linux host
//! blanked or locked its screen in a watch-only or gamepad-only session (gamepad input from a
//! virtual pad does not reset any OS idle timer), so the client saw the lock screen and then
//! the machine went to sleep. The host holds it unconditionally while streaming: a forgotten
//! session ends on the CLIENT side (below), which stops the stream and releases this.
//!
//! **Client:** held while a session is viewed AND its user did something recently
//! ([`ClientAwake`]). A display turning off ends every viewed session by design
//! (`power_watch.rs`: on a Modern Standby laptop the display going off IS the sleep), so a
//! session played only with a gamepad — or only watched — was cut off at the display timeout
//! (2-5 min on battery): pad input never resets the OS idle timer, and the evdev grab hides
//! Linux keyboard/mouse input from it too. The hold lasts [`client_hold`] (30 min by default,
//! `PULSAR_CLIENT_AWAKE_MIN`, 0 = off) after the last real input, gamepad included; then the
//! normal idle timeout applies again, so a laptop left connected overnight still turns its
//! display off and ends the session (the 2026-11-14 field case `power_watch.rs` describes).
//!
//! Only IDLE sleep is held back. A sleep the user asks for (lid, power button, Start → Sleep,
//! locking the screen) still happens, and `power_watch` still ends the viewed sessions on it.
//!
//! Per OS (one process-wide request, refcounted over every host and client session):
//! - Windows: `PowerCreateRequest` + `PowerSetRequest(DisplayRequired, SystemRequired)` —
//!   process-wide, not tied to a thread like `SetThreadExecutionState` (visible in
//!   `powercfg /requests`).
//! - macOS: `IOPMAssertion` `PreventUserIdleDisplaySleep` (keeps the system awake too).
//! - Linux (untested on Linux hardware): `org.freedesktop.ScreenSaver.Inhibit` (KDE, Xfce,
//!   Cinnamon, MATE, LXQt; GNOME through gsd's proxy) with the GNOME Session idle inhibitor as
//!   the fallback, `org.freedesktop.PowerManagement.Inhibit` (idle suspend on KDE / Xfce), a
//!   logind `idle` block lock, and on X11 an `XResetScreenSaver` every 20 s (the X server's own
//!   screen saver / DPMS timers, which no D-Bus inhibitor reaches). Bare wlroots compositors
//!   (sway, Hyprland …) only offer the Wayland idle-inhibit protocol, which needs a surface of
//!   ours on screen: not covered there.
//! - Android / iOS: not this crate (pulsar-mobile; Android uses `FLAG_KEEP_SCREEN_ON` and a
//!   screen wake lock in its video plugin).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use pulsar_core::input::GamepadState;
use pulsar_core::service::InputEvent;

/// Keeps the display awake while alive (one per host session / held client session).
pub(crate) struct Guard(());

impl Drop for Guard {
	fn drop(&mut self) {
		release();
	}
}

/// Number of live guards; the OS request is on while it is > 0.
static HELD: Mutex<usize> = Mutex::new(0);

/// Keep the display awake (or keep it awake) for one more holder. `who` is for the log.
pub(crate) fn acquire(who: &'static str) -> Guard {
	let mut n = HELD.lock().unwrap_or_else(|e| e.into_inner());
	if *n == 0 {
		imp::engage(who);
	}
	*n += 1;
	Guard(())
}

fn release() {
	let mut n = HELD.lock().unwrap_or_else(|e| e.into_inner());
	if *n == 0 {
		return;
	}
	*n -= 1;
	if *n == 0 {
		imp::disengage();
	}
}

// ---- client: hold only while the session is in use ------------------------------------------

/// Default client hold after the last input.
const CLIENT_HOLD_DEFAULT_MIN: u64 = 30;

/// How long the client keeps the display awake after the last input: `PULSAR_CLIENT_AWAKE_MIN`
/// minutes (0 = never), else [`CLIENT_HOLD_DEFAULT_MIN`].
pub(crate) fn client_hold() -> Duration {
	hold_from(std::env::var("PULSAR_CLIENT_AWAKE_MIN").ok().as_deref())
}

fn hold_from(env: Option<&str>) -> Duration {
	let min = env.and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(CLIENT_HOLD_DEFAULT_MIN);
	Duration::from_secs(min.saturating_mul(60))
}

/// A stick must move this far from where it was at the last counted input (i16 axes, ~8 %):
/// a drifting or noisy stick is not a person.
const PAD_AXIS: i32 = 2600;
/// Same for the 8-bit triggers and the DualSense's 8-bit sticks.
const PAD_U8: i32 = 10;
/// Leading DualSense input state bytes compared: sticks 0..4, triggers 4..6, the report
/// counter 6 (ignored), buttons 7..10. Gyro / touchpad / timestamps follow and always change.
const DS5_HEAD: usize = 10;

/// Which input events are a person using the session. Pads repeat their state about once a
/// second (a lost datagram would leave a button stuck) and a DualSense streams its sensors
/// all the time, so pad events count only when a button or a trigger changed or a stick
/// really moved since the last counted one.
#[derive(Default)]
pub(crate) struct Activity {
	pads: Vec<(u8, GamepadState)>,
	ds5: Vec<(u8, [u8; DS5_HEAD])>,
}

impl Activity {
	pub(crate) fn is_user(&mut self, ev: &InputEvent) -> bool {
		match ev {
			InputEvent::Gamepad(s) => self.pad(u8::MAX, s),
			InputEvent::GamepadSlot { slot, state, .. } => self.pad(*slot, state),
			InputEvent::GamepadDisconnect { .. } => false,
			InputEvent::Ds5Input { slot, report } => {
				let mut head = [0u8; DS5_HEAD];
				head.copy_from_slice(&report.0[..DS5_HEAD]);
				self.ds5(*slot, head)
			}
			_ => true,
		}
	}

	fn pad(&mut self, slot: u8, s: &GamepadState) -> bool {
		match self.pads.iter_mut().find(|(k, _)| *k == slot) {
			Some((_, last)) => {
				let moved = pad_moved(last, s);
				if moved {
					*last = *s;
				}
				moved
			}
			None => {
				self.pads.push((slot, *s));
				true
			}
		}
	}

	fn ds5(&mut self, slot: u8, head: [u8; DS5_HEAD]) -> bool {
		match self.ds5.iter_mut().find(|(k, _)| *k == slot) {
			Some((_, last)) => {
				let moved = ds5_moved(last, &head);
				if moved {
					*last = head;
				}
				moved
			}
			None => {
				self.ds5.push((slot, head));
				true
			}
		}
	}
}

fn pad_moved(a: &GamepadState, b: &GamepadState) -> bool {
	let axis = |x: i16, y: i16| (x as i32 - y as i32).abs() > PAD_AXIS;
	let trig = |x: u8, y: u8| (x as i32 - y as i32).abs() > PAD_U8;
	a.buttons != b.buttons
		|| axis(a.left_x, b.left_x)
		|| axis(a.left_y, b.left_y)
		|| axis(a.right_x, b.right_x)
		|| axis(a.right_y, b.right_y)
		|| trig(a.left_trigger, b.left_trigger)
		|| trig(a.right_trigger, b.right_trigger)
}

fn ds5_moved(a: &[u8; DS5_HEAD], b: &[u8; DS5_HEAD]) -> bool {
	a[7..] != b[7..] || (0..6).any(|i| (a[i] as i32 - b[i] as i32).abs() > PAD_U8)
}

/// The client side of one viewed session: holds a [`Guard`] from the start of the session and
/// while input keeps arriving, drops it [`client_hold`] after the last input and takes it again
/// on the next one. Lives in the session's hold loop (`play/hold.rs`), which sees every input
/// the session forwards (webview, native keyboard hooks, renderer pen/touch, pads, DualSense).
pub(crate) struct ClientAwake {
	hold: Duration,
	last: Instant,
	guard: Option<Guard>,
	activity: Activity,
}

impl ClientAwake {
	/// How often the hold loop should call [`ClientAwake::tick`].
	pub(crate) const TICK: Duration = Duration::from_secs(15);

	/// A session starts being viewed (connecting counts as input).
	pub(crate) fn start(now: Instant) -> Self {
		let hold = client_hold();
		let guard = (!hold.is_zero()).then(|| acquire("client session"));
		Self { hold, last: now, guard, activity: Activity::default() }
	}

	/// An input event this session forwards.
	pub(crate) fn on_input(&mut self, ev: &InputEvent, now: Instant) {
		if !self.activity.is_user(ev) {
			return;
		}
		self.last = now;
		if self.guard.is_none() && !self.hold.is_zero() {
			tracing::info!("keep-awake: input again — the display is held awake for this session");
			self.guard = Some(acquire("client session"));
		}
	}

	/// Periodic check: release the hold once nobody used the session for [`client_hold`].
	pub(crate) fn tick(&mut self, now: Instant) {
		if self.guard.is_some() && expired(self.last, now, self.hold) {
			tracing::info!(
				idle_min = self.hold.as_secs() / 60,
				"keep-awake: no input on this session — the display may turn off again (that ends the session)"
			);
			self.guard = None;
		}
	}
}

fn expired(last: Instant, now: Instant, hold: Duration) -> bool {
	now.saturating_duration_since(last) >= hold
}

// ---- OS requests ----------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
	use std::sync::Mutex;

	use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
	use windows_sys::Win32::System::Power::{
		PowerClearRequest, PowerCreateRequest, PowerRequestDisplayRequired, PowerRequestSystemRequired,
		PowerSetRequest,
	};
	use windows_sys::Win32::System::Threading::{
		POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
	};

	/// The live power request handle (as usize: HANDLE is a raw pointer, not Send).
	static REQUEST: Mutex<usize> = Mutex::new(0);

	pub(super) fn engage(who: &'static str) {
		let mut reason: Vec<u16> = "Pulsar: a remote session is active\0".encode_utf16().collect();
		let ctx = REASON_CONTEXT {
			Version: 0, // POWER_REQUEST_CONTEXT_VERSION
			Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
			Reason: REASON_CONTEXT_0 { SimpleReasonString: reason.as_mut_ptr() },
		};
		// SAFETY: `ctx` and the NUL-terminated reason outlive the call (the system copies it).
		let h: HANDLE = unsafe { PowerCreateRequest(&ctx) };
		if h.is_null() || h == INVALID_HANDLE_VALUE {
			tracing::warn!(who, "keep-awake: PowerCreateRequest failed — the display may turn off");
			return;
		}
		// SAFETY: a valid power request handle, cleared + closed in `disengage`.
		let (display_ok, system_ok) = unsafe {
			(
				PowerSetRequest(h, PowerRequestDisplayRequired) != 0,
				PowerSetRequest(h, PowerRequestSystemRequired) != 0,
			)
		};
		*REQUEST.lock().unwrap_or_else(|e| e.into_inner()) = h as usize;
		tracing::info!(who, display_ok, system_ok, "keep-awake: on (display + system required)");
	}

	pub(super) fn disengage() {
		let h = std::mem::take(&mut *REQUEST.lock().unwrap_or_else(|e| e.into_inner())) as HANDLE;
		if h.is_null() {
			return;
		}
		// SAFETY: the handle `engage` created; nothing else uses it.
		unsafe {
			PowerClearRequest(h, PowerRequestDisplayRequired);
			PowerClearRequest(h, PowerRequestSystemRequired);
			CloseHandle(h);
		}
		tracing::info!("keep-awake: off");
	}

	/// The request is live (tests).
	#[cfg(test)]
	pub(super) fn engaged() -> bool {
		*REQUEST.lock().unwrap() != 0
	}
}

#[cfg(target_os = "macos")]
mod imp {
	use std::ffi::c_void;
	use std::sync::Mutex;

	#[link(name = "IOKit", kind = "framework")]
	extern "C" {
		fn IOPMAssertionCreateWithName(kind: *const c_void, level: u32, name: *const c_void, id: *mut u32) -> i32;
		fn IOPMAssertionRelease(id: u32) -> i32;
	}

	#[link(name = "CoreFoundation", kind = "framework")]
	extern "C" {
		fn CFStringCreateWithCString(alloc: *const c_void, s: *const std::ffi::c_char, encoding: u32) -> *const c_void;
		fn CFRelease(cf: *const c_void);
	}

	/// The live assertion id.
	static ASSERTION: Mutex<Option<u32>> = Mutex::new(None);

	pub(super) fn engage(who: &'static str) {
		const UTF8: u32 = 0x0800_0100;
		const ON: u32 = 255; // kIOPMAssertionLevelOn
		// SAFETY: CF strings created and released here; the assertion id is released in
		// `disengage`.
		let id = unsafe {
			let kind = CFStringCreateWithCString(std::ptr::null(), c"PreventUserIdleDisplaySleep".as_ptr(), UTF8);
			let name = CFStringCreateWithCString(std::ptr::null(), c"Pulsar: a remote session is active".as_ptr(), UTF8);
			let mut id = 0u32;
			let r = if kind.is_null() || name.is_null() { -1 } else { IOPMAssertionCreateWithName(kind, ON, name, &mut id) };
			if !kind.is_null() {
				CFRelease(kind);
			}
			if !name.is_null() {
				CFRelease(name);
			}
			(r == 0).then_some(id)
		};
		match id {
			Some(id) => {
				*ASSERTION.lock().unwrap_or_else(|e| e.into_inner()) = Some(id);
				tracing::info!(who, "keep-awake: on (PreventUserIdleDisplaySleep)");
			}
			None => tracing::warn!(who, "keep-awake: IOPMAssertion refused — the display may sleep"),
		}
	}

	pub(super) fn disengage() {
		if let Some(id) = ASSERTION.lock().unwrap_or_else(|e| e.into_inner()).take() {
			// SAFETY: releases the assertion created in `engage`.
			unsafe { IOPMAssertionRelease(id) };
			tracing::info!("keep-awake: off");
		}
	}

	#[cfg(test)]
	pub(super) fn engaged() -> bool {
		ASSERTION.lock().unwrap().is_some()
	}
}

#[cfg(target_os = "linux")]
mod imp {
	use std::sync::mpsc::{self, RecvTimeoutError, Sender};
	use std::sync::Mutex;
	use std::time::Duration;

	use zbus::blocking::Connection;

	/// The X server's screen-saver / DPMS timers are reset this often while held.
	const X_POKE: Duration = Duration::from_secs(20);
	const REASON: &str = "A remote session is active";

	/// Stops the holder thread (dropping it is enough).
	static STOP: Mutex<Option<Sender<()>>> = Mutex::new(None);

	/// The D-Bus calls can block (a hung bus waits for zbus' timeout), so they never run on
	/// the caller's (async) thread: a plain thread takes the inhibitors, keeps them until
	/// `disengage`, then gives them back.
	pub(super) fn engage(who: &'static str) {
		let (tx, rx) = mpsc::channel::<()>();
		match std::thread::Builder::new().name("pulsar-keep-awake".into()).spawn(move || hold(who, rx)) {
			Ok(_) => *STOP.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx),
			Err(e) => tracing::warn!(who, "keep-awake: thread not started: {e}"),
		}
	}

	pub(super) fn disengage() {
		// Dropping the sender wakes the thread (`Disconnected`).
		STOP.lock().unwrap_or_else(|e| e.into_inner()).take();
	}

	#[cfg(test)]
	pub(super) fn engaged() -> bool {
		STOP.lock().unwrap().is_some()
	}

	/// An `Inhibit` call that answers with a cookie.
	fn call_u32<B>(conn: &Connection, dest: &str, path: &str, iface: &str, args: &B) -> Option<u32>
	where
		B: serde::Serialize + zbus::zvariant::DynamicType,
	{
		conn.call_method(Some(dest), path, Some(iface), "Inhibit", args).ok()?.body().deserialize::<u32>().ok()
	}

	fn hold(who: &'static str, stop: mpsc::Receiver<()>) {
		let session = Connection::session().ok();
		let system = Connection::system().ok();
		// Display: the desktop's screen saver (blank / lock), or GNOME Session's idle flag.
		let saver = session.as_ref().and_then(|c| {
			call_u32(c, "org.freedesktop.ScreenSaver", "/org/freedesktop/ScreenSaver", "org.freedesktop.ScreenSaver", &("Pulsar", REASON))
		});
		let gnome = match (&saver, &session) {
			(None, Some(c)) => call_u32(
				c,
				"org.gnome.SessionManager",
				"/org/gnome/SessionManager",
				"org.gnome.SessionManager",
				// toplevel xid 0, flag 8 = idle (NOT 4 = suspend: that would block a suspend the
				// user asks for).
				&("pulsar", 0u32, REASON, 8u32),
			),
			_ => None,
		};
		// System: idle suspend on KDE / Xfce (their power managers honour this), and logind's
		// own idle action. Neither blocks a suspend the user asks for.
		let pm = session.as_ref().and_then(|c| {
			call_u32(
				c,
				"org.freedesktop.PowerManagement",
				"/org/freedesktop/PowerManagement/Inhibit",
				"org.freedesktop.PowerManagement.Inhibit",
				&("Pulsar", REASON),
			)
		});
		let logind_idle = system.as_ref().and_then(|c| {
			c.call_method(
				Some("org.freedesktop.login1"),
				"/org/freedesktop/login1",
				Some("org.freedesktop.login1.Manager"),
				"Inhibit",
				&("idle", "Pulsar", REASON, "block"),
			)
			.ok()?
			.body()
			.deserialize::<zbus::zvariant::OwnedFd>()
			.ok()
		});
		let x11 = XScreenSaver::open();
		tracing::info!(
			who,
			screensaver = saver.is_some(),
			gnome_idle = gnome.is_some(),
			power_management = pm.is_some(),
			logind_idle = logind_idle.is_some(),
			x11 = x11.is_some(),
			"keep-awake: on"
		);
		loop {
			if let Some(x) = &x11 {
				x.reset();
			}
			match stop.recv_timeout(X_POKE) {
				Err(RecvTimeoutError::Timeout) => {}
				_ => break,
			}
		}
		let un = |conn: &Connection, dest: &str, path: &str, iface: &str, method: &str, cookie: u32| {
			let _ = conn.call_method(Some(dest), path, Some(iface), method, &(cookie,));
		};
		if let Some(c) = &session {
			if let Some(k) = saver {
				un(c, "org.freedesktop.ScreenSaver", "/org/freedesktop/ScreenSaver", "org.freedesktop.ScreenSaver", "UnInhibit", k);
			}
			if let Some(k) = gnome {
				un(c, "org.gnome.SessionManager", "/org/gnome/SessionManager", "org.gnome.SessionManager", "Uninhibit", k);
			}
			if let Some(k) = pm {
				un(
					c,
					"org.freedesktop.PowerManagement",
					"/org/freedesktop/PowerManagement/Inhibit",
					"org.freedesktop.PowerManagement.Inhibit",
					"UnInhibit",
					k,
				);
			}
		}
		// Closing the fd releases the logind lock; the connections dropping releases anything
		// the calls above missed (the services tie inhibitors to the caller's bus name).
		drop(logind_idle);
		drop(x11);
		tracing::info!("keep-awake: off");
	}

	/// This thread's own X connection (X11 sessions only: XWayland has no real screen saver).
	struct XScreenSaver(*mut x11::xlib::Display);

	impl XScreenSaver {
		fn open() -> Option<Self> {
			if !crate::power_watch::is_x11_session() {
				return None;
			}
			// SAFETY: a private Display connection, closed on drop.
			let dpy = unsafe { x11::xlib::XOpenDisplay(std::ptr::null()) };
			(!dpy.is_null()).then_some(Self(dpy))
		}

		fn reset(&self) {
			// SAFETY: this thread's open Display. Resets the X screen saver AND the DPMS timers
			// (both run off the last input time).
			unsafe {
				x11::xlib::XResetScreenSaver(self.0);
				x11::xlib::XFlush(self.0);
			}
		}
	}

	impl Drop for XScreenSaver {
		fn drop(&mut self) {
			// SAFETY: the Display opened in `open`.
			unsafe { x11::xlib::XCloseDisplay(self.0) };
		}
	}
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
mod imp {
	pub(super) fn engage(_who: &'static str) {}
	pub(super) fn disengage() {}
	#[cfg(test)]
	pub(super) fn engaged() -> bool {
		false
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use pulsar_core::input::GamepadKind;

	fn slot(slot: u8, state: GamepadState) -> InputEvent {
		InputEvent::GamepadSlot { slot, kind: GamepadKind::Xbox, target: Default::default(), state }
	}

	#[test]
	fn hold_from_env() {
		assert_eq!(hold_from(None), Duration::from_secs(30 * 60));
		assert_eq!(hold_from(Some("5")), Duration::from_secs(5 * 60));
		assert_eq!(hold_from(Some(" 0 ")), Duration::ZERO);
		assert_eq!(hold_from(Some("x")), Duration::from_secs(30 * 60));
	}

	#[test]
	fn keyboard_mouse_touch_always_count() {
		let mut a = Activity::default();
		assert!(a.is_user(&InputEvent::PointerMotion { x: 0.5, y: 0.5 }));
		assert!(a.is_user(&InputEvent::Key { code: 30, down: true }));
		assert!(a.is_user(&InputEvent::Scroll { dx: 0.0, dy: 1.0 }));
		assert!(!a.is_user(&InputEvent::GamepadDisconnect { slot: 0 }));
	}

	#[test]
	fn pad_keepalive_and_drift_do_not_count() {
		let mut a = Activity::default();
		let idle = GamepadState::default();
		assert!(a.is_user(&slot(0, idle)), "first sight of a pad counts");
		assert!(!a.is_user(&slot(0, idle)), "a 1 Hz keepalive repeat is not input");
		let drift = GamepadState { left_x: 900, right_y: -1200, left_trigger: 4, ..idle };
		assert!(!a.is_user(&slot(0, drift)), "a noisy stick is not input");
		let pressed = GamepadState { buttons: 1, ..idle };
		assert!(a.is_user(&slot(0, pressed)));
		assert!(!a.is_user(&slot(0, pressed)));
		let moved = GamepadState { left_x: 20_000, ..pressed };
		assert!(a.is_user(&slot(0, moved)));
		let pulled = GamepadState { right_trigger: 200, ..moved };
		assert!(a.is_user(&slot(0, pulled)));
		// Another slot has its own reference state.
		assert!(a.is_user(&slot(1, idle)));
		assert!(!a.is_user(&slot(1, idle)));
		// A slow deliberate stick move adds up against the last COUNTED state.
		let mut s = idle;
		let mut counted = false;
		for _ in 0..10 {
			s.left_y += 1000;
			counted |= a.is_user(&slot(1, s));
		}
		assert!(counted);
	}

	#[test]
	fn ds5_sensors_and_counter_do_not_count() {
		use pulsar_core::input::ds5::wire::Ds5Report;
		let mut a = Activity::default();
		let mut r = [0u8; 63];
		r[..4].copy_from_slice(&[0x80; 4]);
		r[7] = 0x08;
		let ev = |r: [u8; 63]| InputEvent::Ds5Input { slot: 0, report: Ds5Report(r) };
		assert!(a.is_user(&ev(r)));
		let mut noise = r;
		noise[6] = 0x33; // report counter
		noise[1] = 0x83; // stick jitter
		noise[20] = 0x7f; // gyro
		noise[40] = 0x12; // timestamp / touchpad area
		assert!(!a.is_user(&ev(noise)));
		let mut cross = noise;
		cross[7] = 0x28; // cross button
		assert!(a.is_user(&ev(cross)));
		let mut stick = cross;
		stick[2] = 0xF0;
		assert!(a.is_user(&ev(stick)));
	}

	#[test]
	fn hold_expiry() {
		let t0 = Instant::now();
		let hold = Duration::from_secs(60);
		assert!(!expired(t0, t0 + Duration::from_secs(59), hold));
		assert!(expired(t0, t0 + Duration::from_secs(60), hold));
		assert!(!expired(t0 + Duration::from_secs(5), t0, hold), "clock skew: not expired");
	}

	/// The OS request follows the refcount: on with the first guard, off with the last.
	/// Holds the real request for a moment on the test machine (no window, no input).
	#[test]
	fn refcount_engages_once() {
		let a = acquire("test");
		let b = acquire("test");
		// Windows always grants a power request; macOS / Linux may refuse in a sandbox / CI.
		#[cfg(any(windows, target_os = "linux"))]
		assert!(imp::engaged(), "request taken");
		drop(a);
		assert_eq!(*HELD.lock().unwrap(), 1);
		drop(b);
		assert_eq!(*HELD.lock().unwrap(), 0);
		assert!(!imp::engaged());
	}
}
