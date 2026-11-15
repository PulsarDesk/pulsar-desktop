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
//! session is meant to end on the CLIENT side (below), which stops the stream and releases
//! this. That holds for desktop clients only: the Android client keeps `FLAG_KEEP_SCREEN_ON`
//! for the whole stream, has no input-idle expiry and does not end a stream when its screen
//! goes off, so a phone left connected keeps this host awake until the session ends some other
//! way (pulsar-mobile `docs/agents/layout.md`, "Keep-awake").
//!
//! **Client:** held while a session is viewed, Pulsar is the focused app (or "Arka planda kol
//! girdisi" is on), the session is not locked, and its user did something recently
//! ([`ClientAwake`]). A display turning off ends every viewed session by design
//! (`power_watch.rs`: on a Modern Standby laptop the display going off IS the sleep), so a
//! session played only with a gamepad — or only watched — was cut off at the display timeout
//! (2-5 min on battery): pad input never resets the OS idle timer, and the evdev grab hides
//! Linux keyboard/mouse input from it too. The hold lasts [`client_hold`] after the last real
//! input, gamepad included (Settings → Genel → "Ekranı uyanık tut": 30 min by default, 2 h,
//! always or off; `PULSAR_CLIENT_AWAKE_MIN` overrides it, 0 = off); then the normal idle
//! timeout applies again, so a laptop left connected overnight still turns its display off and
//! ends the session (the 2026-11-14 field case `power_watch.rs` describes). A display request
//! also holds back the screen saver and its idle LOCK: while the hold is on, a client whose
//! policy locks it after N idle minutes stays unlocked — hence the focus condition, the expiry
//! and "Kapalı" for machines that must lock on time.
//!
//! Only IDLE sleep is held back. A sleep the user asks for (lid, power button, Start → Sleep,
//! locking the screen) still happens, and `power_watch` ends the viewed sessions on it — on
//! every OS, the lock included: Windows and macOS honour a display request on the lock screen,
//! so `power_watch` watches the lock itself (WTS lock state, `com.apple.screenIsLocked`; Linux:
//! the screen saver's `ActiveChanged`). The client hold is also dropped while locked.
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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
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
/// "Always" (no input expiry): the hold then ends only with focus, lock or the session.
const ALWAYS: u64 = u64::MAX;

/// Settings → Genel → "Ekranı uyanık tut" (`set_client_awake`), in minutes (0 = off,
/// [`ALWAYS`]). The UI syncs it at startup and on every change.
static SETTING_MIN: AtomicU64 = AtomicU64::new(CLIENT_HOLD_DEFAULT_MIN);

/// Settings value: "off", "always" or a number of minutes ("30", "120"). Anything else keeps
/// the default. Live sessions pick it up on their next check (≤ 2 s).
pub(crate) fn set_client_awake(mode: &str) {
	let min = parse_min(mode).unwrap_or(CLIENT_HOLD_DEFAULT_MIN);
	if SETTING_MIN.swap(min, Ordering::SeqCst) != min {
		tracing::info!(mode, "keep-awake: client setting");
	}
}

fn parse_min(v: &str) -> Option<u64> {
	match v.trim() {
		"off" => Some(0),
		"always" => Some(ALWAYS),
		n => n.parse::<u64>().ok(),
	}
}

/// How long the client keeps the display awake after the last input: `PULSAR_CLIENT_AWAKE_MIN`
/// (minutes, 0 = never, or "off" / "always") if set, else the Settings value.
pub(crate) fn client_hold() -> Duration {
	static ENV: OnceLock<Option<u64>> = OnceLock::new();
	let env = *ENV.get_or_init(|| std::env::var("PULSAR_CLIENT_AWAKE_MIN").ok().as_deref().and_then(parse_min));
	hold_from(env, SETTING_MIN.load(Ordering::SeqCst))
}

fn hold_from(env: Option<u64>, setting: u64) -> Duration {
	Duration::from_secs(env.unwrap_or(setting).saturating_mul(60))
}

/// A stick must move this far from where it was at the last counted input (i16 axes, ~8 %):
/// a drifting or noisy stick is not a person.
const PAD_AXIS: i32 = 2600;
/// Same for the 8-bit triggers and the DualSense's 8-bit sticks.
const PAD_U8: i32 = 10;
/// Leading DualSense input state bytes kept: sticks 0..4, triggers 4..6, the report counter 6
/// (ignored), buttons 7..10, then gyro / accel / timestamp (11..32, ignored: they always
/// change) and the two touch points 32..36 / 36..40.
const DS5_HEAD: usize = 40;
/// The DualSense touch points (`[contact, x lo, x hi | y lo, y hi]`; contact bit 7 SET = no
/// finger, the low 7 bits = a touch id that changes on every new touch; X 0..1919, Y 0..1079).
const DS5_TOUCH: [usize; 2] = [32, 36];
/// A finger must move this far on the touchpad (12-bit units, ~1 %) to count.
const DS5_TOUCH_MOVE: i32 = 20;

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
	a[7..10] != b[7..10]
		|| (0..6).any(|i| (a[i] as i32 - b[i] as i32).abs() > PAD_U8)
		|| DS5_TOUCH.iter().any(|&o| touch_moved(&a[o..o + 4], &b[o..o + 4]))
}

/// A finger touched / lifted, a new touch began, or a finger moved on the touchpad.
fn touch_moved(a: &[u8], b: &[u8]) -> bool {
	let down = |p: &[u8]| p[0] & 0x80 == 0;
	let xy = |p: &[u8]| (p[1] as i32 | ((p[2] & 0x0F) as i32) << 8, (p[2] >> 4) as i32 | (p[3] as i32) << 4);
	if down(a) != down(b) {
		return true;
	}
	if !down(b) {
		return false; // no finger: the stale position bytes mean nothing
	}
	let ((ax, ay), (bx, by)) = (xy(a), xy(b));
	a[0] != b[0] || (ax - bx).abs() > DS5_TOUCH_MOVE || (ay - by).abs() > DS5_TOUCH_MOVE
}

/// The client side of one viewed session: holds a [`Guard`] while the session is in use —
/// Pulsar focused (or background controller input on), the session unlocked, and an input
/// within [`client_hold`] (connecting counts as one). Lives in the session's hold loop
/// (`play/hold.rs`), which sees every input the session forwards (webview, native keyboard
/// hooks, renderer pen/touch, pads, DualSense) and calls [`ClientAwake::tick`] every 2 s.
pub(crate) struct ClientAwake {
	last: Instant,
	guard: Option<Guard>,
	activity: Activity,
}

/// Why the client holds the display, or why not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Want {
	Hold,
	Off,
	Idle,
	Unfocused,
	Locked,
}

fn want(hold: Duration, last: Instant, now: Instant, viewed: bool, locked: bool) -> Want {
	if hold.is_zero() {
		Want::Off
	} else if locked {
		Want::Locked
	} else if !viewed {
		Want::Unfocused
	} else if expired(last, now, hold) {
		Want::Idle
	} else {
		Want::Hold
	}
}

impl ClientAwake {
	/// A session starts being viewed (connecting counts as input).
	pub(crate) fn start(now: Instant) -> Self {
		let mut s = Self { last: now, guard: None, activity: Activity::default() };
		s.tick(now);
		s
	}

	/// An input event this session forwards. A released hold comes back on the next
	/// [`ClientAwake::tick`] (≤ 2 s; display timeouts are minutes).
	pub(crate) fn on_input(&mut self, ev: &InputEvent, now: Instant) {
		if self.activity.is_user(ev) {
			self.last = now;
		}
	}

	/// Periodic check (every 2 s): take or release the hold (see [`want`]).
	pub(crate) fn tick(&mut self, now: Instant) {
		let hold = client_hold();
		let viewed = crate::focus_gate::app_focused() || crate::focus_gate::background_input();
		let w = want(hold, self.last, now, viewed, crate::power_watch::locked());
		match (w, self.guard.is_some()) {
			(Want::Hold, false) => {
				tracing::info!("keep-awake: client session in use — the display is held awake");
				self.guard = Some(acquire("client session"));
			}
			(Want::Hold, true) | (_, false) => {}
			(why, true) => {
				tracing::info!(
					?why,
					"keep-awake: client hold released — the display may turn off again (that ends the session)"
				);
				self.guard = None;
			}
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
	fn hold_from_env_and_setting() {
		assert_eq!(hold_from(None, CLIENT_HOLD_DEFAULT_MIN), Duration::from_secs(30 * 60));
		assert_eq!(hold_from(parse_min("5"), 30), Duration::from_secs(5 * 60), "env wins");
		assert_eq!(hold_from(parse_min(" 0 "), 120), Duration::ZERO);
		assert_eq!(hold_from(parse_min("x"), 120), Duration::from_secs(120 * 60), "bad env: setting");
		assert_eq!(hold_from(None, parse_min("off").unwrap()), Duration::ZERO);
		let always = hold_from(None, parse_min("always").unwrap());
		let t0 = Instant::now();
		assert!(!expired(t0, t0 + Duration::from_secs(365 * 24 * 3600), always));
	}

	#[test]
	fn hold_needs_focus_unlock_and_recent_input() {
		let t0 = Instant::now();
		let h = Duration::from_secs(60);
		let soon = t0 + Duration::from_secs(10);
		assert_eq!(want(h, t0, soon, true, false), Want::Hold);
		assert_eq!(want(h, t0, t0 + h, true, false), Want::Idle);
		assert_eq!(want(h, t0, soon, false, false), Want::Unfocused);
		assert_eq!(want(h, t0, soon, true, true), Want::Locked);
		assert_eq!(want(Duration::ZERO, t0, soon, true, false), Want::Off);
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
		noise[28] = 0x12; // sensor timestamp
		noise[40] = 0x12; // past the touch points
		assert!(!a.is_user(&ev(noise)));
		let mut cross = noise;
		cross[7] = 0x28; // cross button
		assert!(a.is_user(&ev(cross)));
		let mut stick = cross;
		stick[2] = 0xF0;
		assert!(a.is_user(&ev(stick)));
	}

	#[test]
	fn ds5_touchpad_counts() {
		use pulsar_core::input::ds5::wire::Ds5Report;
		let mut a = Activity::default();
		let mut r = [0u8; 63];
		r[..4].copy_from_slice(&[0x80; 4]);
		r[7] = 0x08;
		r[32] = 0x80; // no finger
		r[36] = 0x80;
		r[33] = 0x55; // stale position bytes of a lifted finger
		let ev = |r: [u8; 63]| InputEvent::Ds5Input { slot: 0, report: Ds5Report(r) };
		assert!(a.is_user(&ev(r)));
		let mut lifted_noise = r;
		lifted_noise[34] = 0x3c;
		assert!(!a.is_user(&ev(lifted_noise)), "position bytes of a lifted finger are not input");
		// Finger down at x 0x100, y 0x200: x lo 0x00, x hi 1 | y lo 0 << 4, y hi 0x20.
		let mut down = lifted_noise;
		down[32] = 0x05;
		down[33] = 0x00;
		down[34] = 0x01;
		down[35] = 0x20;
		assert!(a.is_user(&ev(down)), "a finger touching counts");
		let mut jitter = down;
		jitter[33] = 0x05; // x + 5
		assert!(!a.is_user(&ev(jitter)), "a resting finger's jitter is not input");
		let mut slide = jitter;
		slide[33] = 0x40; // x + 64
		assert!(a.is_user(&ev(slide)), "a finger moving counts");
		let mut slide_y = slide;
		slide_y[35] = 0x24; // y + 64
		assert!(a.is_user(&ev(slide_y)));
		let mut second = slide_y;
		second[36] = 0x06; // second finger down
		assert!(a.is_user(&ev(second)));
		let mut lift = second;
		lift[32] |= 0x80;
		assert!(a.is_user(&ev(lift)), "a finger lifting counts");
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
