//! Linux: system suspend and display-off / screen-saver detection (untested on Linux hardware).
//!
//! Three independent watchers, each on its own plain thread with a blocking D-Bus / X
//! connection (nothing here touches the GTK main loop):
//!
//! - **Suspend** — logind (`org.freedesktop.login1.Manager`, system bus) `PrepareForSleep(true)`
//!   before a suspend / hibernate, `PrepareForSleep(false)` after the resume. A *delay*
//!   inhibitor lock is held while awake, so the suspend waits (logind's `InhibitDelayMaxSec`,
//!   5 s by default) until the viewed sessions are ended — the host sees a clean stop instead of
//!   a 6 s liveness timeout. The lock is released right after, and taken again on resume.
//! - **Screen saver** — the desktop's screen saver / lock screen `ActiveChanged(bool)` on the
//!   session bus, from `org.freedesktop.ScreenSaver` (KDE, Xfce, Cinnamon, MATE, LXQt …) and
//!   `org.gnome.ScreenSaver` (GNOME Shell). Blanking or locking the screen counts as "display off".
//! - **DPMS** (X11 sessions only) — the X server's DPMS power level, polled every 2 s; a level
//!   other than On (standby / suspend / off) = the display is off. Under Wayland there is no
//!   portable way to read the output power state, so there the screen-saver signal is it
//!   (wlroots compositors without a screen-saver service: suspend only).
//!
//! The evdev grab of a viewed session hides local keyboard/mouse input from the desktop's own
//! idle timer, so without help the screen saver would start (and end the session) while someone
//! is playing. [`note_input`] (called for every forwarded event) reports user activity at most
//! every [`POKE_EVERY`]: `XResetScreenSaver` on X11, `SimulateUserActivity` on
//! `org.freedesktop.ScreenSaver`, and — GNOME has no "user activity" call — a GNOME Session
//! idle inhibitor that is held while input keeps arriving and dropped [`IDLE_HOLD`] after the
//! last event, so the normal idle blanking still happens once nobody touches anything.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tauri::AppHandle;
use zbus::blocking::{Connection, MessageIterator};
use zbus::message::Type as MsgType;
use zbus::MatchRule;

use super::{set, DISPLAY_OFF, SCREEN_SAVER, SUSPENDED};

/// How long a suspend is held back for the sessions to end (logind caps it at its own
/// `InhibitDelayMaxSec`, 5 s by default).
const STOP_WAIT: Duration = Duration::from_secs(3);
/// DPMS poll interval.
const DPMS_POLL: Duration = Duration::from_secs(2);
/// Forwarded input is reported as user activity at most this often.
const POKE_EVERY: Duration = Duration::from_secs(20);
/// The GNOME idle inhibitor stays this long after the last forwarded input.
const IDLE_HOLD: Duration = Duration::from_secs(60);

/// Milliseconds since [`epoch`] of the last forwarded input (0 = never).
static LAST_INPUT_MS: AtomicU64 = AtomicU64::new(0);
/// The activity thread runs (started on the first input).
static ACTIVITY_STARTED: AtomicBool = AtomicBool::new(false);

fn epoch() -> Instant {
	static E: OnceLock<Instant> = OnceLock::new();
	*E.get_or_init(Instant::now)
}

fn now_ms() -> u64 {
	// +1 so a stamp is never 0 ("never").
	epoch().elapsed().as_millis() as u64 + 1
}

pub(super) fn spawn(app: AppHandle) {
	let _ = epoch();
	let a = app.clone();
	thread("pulsar-power-logind", move || {
		if let Err(e) = watch_logind(&a) {
			tracing::warn!("power: logind sleep watch not active: {e}");
		}
	});
	for (iface, label) in [
		("org.freedesktop.ScreenSaver", "freedesktop"),
		("org.gnome.ScreenSaver", "gnome"),
	] {
		let a = app.clone();
		thread("pulsar-power-saver", move || {
			if let Err(e) = watch_screen_saver(&a, iface) {
				tracing::warn!(label, "power: screen saver watch not active: {e}");
			}
		});
	}
	if is_x11_session() {
		thread("pulsar-power-dpms", move || watch_dpms(&app));
	}
}

fn thread(name: &str, f: impl FnOnce() + Send + 'static) {
	if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(f) {
		tracing::warn!("power: {name} thread not started: {e}");
	}
}

/// An X11 desktop session (DPMS is only meaningful there: XWayland reports a fake "On").
fn is_x11_session() -> bool {
	let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
	x11_session_from(&session, std::env::var_os("WAYLAND_DISPLAY").is_some(), std::env::var_os("DISPLAY").is_some())
}

fn x11_session_from(session_type: &str, wayland_display: bool, display: bool) -> bool {
	match session_type {
		"x11" => display,
		"wayland" => false,
		_ => display && !wayland_display,
	}
}

// ---- logind ---------------------------------------------------------------------------------

const LOGIND: &str = "org.freedesktop.login1";
const LOGIND_PATH: &str = "/org/freedesktop/login1";
const LOGIND_MANAGER: &str = "org.freedesktop.login1.Manager";

/// A logind delay lock on "sleep": suspend waits until the fd is closed (dropped).
fn take_delay_lock(conn: &Connection) -> Option<zbus::zvariant::OwnedFd> {
	let reply = conn
		.call_method(
			Some(LOGIND),
			LOGIND_PATH,
			Some(LOGIND_MANAGER),
			"Inhibit",
			&("sleep", "Pulsar", "End the viewed remote sessions before sleeping", "delay"),
		)
		.map_err(|e| tracing::warn!("power: logind delay lock refused: {e}"))
		.ok()?;
	reply.body().deserialize::<zbus::zvariant::OwnedFd>().ok()
}

fn watch_logind(app: &AppHandle) -> zbus::Result<()> {
	let conn = Connection::system()?;
	let rule = MatchRule::builder()
		.msg_type(MsgType::Signal)
		.sender(LOGIND)?
		.interface(LOGIND_MANAGER)?
		.member("PrepareForSleep")?
		.path(LOGIND_PATH)?
		.build();
	let signals = MessageIterator::for_match_rule(rule, &conn, Some(16))?;
	let mut lock = take_delay_lock(&conn);
	tracing::info!(delay_lock = lock.is_some(), "power: logind PrepareForSleep watch active");
	for msg in signals {
		let Ok(msg) = msg else { continue };
		let Ok(start) = msg.body().deserialize::<bool>() else { continue };
		if start {
			let stops = set(app, &SUSPENDED, true, "suspend");
			if !stops.is_empty() {
				let waited = tauri::async_runtime::block_on(async move {
					tokio::time::timeout(STOP_WAIT, async move {
						for s in stops {
							let _ = s.await;
						}
					})
					.await
				});
				if waited.is_err() {
					tracing::warn!("power: sessions still stopping when the suspend proceeds");
				}
			}
			// Let the suspend go on.
			drop(lock.take());
		} else {
			let _ = set(app, &SUSPENDED, false, "resume");
			if lock.is_none() {
				lock = take_delay_lock(&conn);
			}
		}
	}
	Err(zbus::Error::Failure("logind signal stream ended".into()))
}

// ---- screen saver ---------------------------------------------------------------------------

/// `ActiveChanged(bool)` of one screen-saver interface, from any sender / path (KDE emits it on
/// both `/ScreenSaver` and `/org/freedesktop/ScreenSaver`: a repeat is a no-op).
fn watch_screen_saver(app: &AppHandle, iface: &'static str) -> zbus::Result<()> {
	let conn = Connection::session()?;
	let rule = MatchRule::builder().msg_type(MsgType::Signal).interface(iface)?.member("ActiveChanged")?.build();
	let signals = MessageIterator::for_match_rule(rule, &conn, Some(16))?;
	tracing::info!(iface, "power: screen saver watch active");
	for msg in signals {
		let Ok(msg) = msg else { continue };
		let Ok(active) = msg.body().deserialize::<bool>() else { continue };
		let _ = set(app, &SCREEN_SAVER, active, if active { "screen saver on" } else { "screen saver off" });
	}
	Err(zbus::Error::Failure("screen saver signal stream ended".into()))
}

// ---- X11 DPMS -------------------------------------------------------------------------------

/// The display is off from `DPMSInfo`: DPMS enabled and the level is not On.
fn dpms_off(enabled: bool, level: u16) -> bool {
	enabled && level != x11::dpms::DPMSModeOn
}

fn watch_dpms(app: &AppHandle) {
	use x11::{dpms, xlib};
	// SAFETY: a private Display connection used by this thread only; it lives as long as the
	// process except on the early-out paths below, which close it.
	unsafe {
		let dpy = xlib::XOpenDisplay(std::ptr::null());
		if dpy.is_null() {
			tracing::warn!("power: DPMS watch not active (no X display)");
			return;
		}
		let (mut ev, mut err) = (0, 0);
		if dpms::DPMSQueryExtension(dpy, &mut ev, &mut err) == 0 || dpms::DPMSCapable(dpy) == 0 {
			tracing::info!("power: X server has no DPMS — display-off from the screen saver only");
			xlib::XCloseDisplay(dpy);
			return;
		}
		tracing::info!("power: X11 DPMS watch active");
		loop {
			let (mut level, mut enabled) = (0u16, 0u8);
			if dpms::DPMSInfo(dpy, &mut level, &mut enabled) != 0 {
				let off = dpms_off(enabled != 0, level);
				if off != DISPLAY_OFF.load(Ordering::SeqCst) {
					let _ = set(app, &DISPLAY_OFF, off, if off { "display off" } else { "display on" });
				}
			}
			std::thread::sleep(DPMS_POLL);
		}
	}
}

// ---- user activity (evdev-grabbed input) ----------------------------------------------------

/// A forwarded input event: cheap (one atomic store); the activity thread does the reporting.
pub(super) fn note_input() {
	LAST_INPUT_MS.store(now_ms(), Ordering::Relaxed);
	if !ACTIVITY_STARTED.swap(true, Ordering::SeqCst) {
		thread("pulsar-power-activity", activity_loop);
	}
}

/// Input arrived within the last `window` (as of `now`).
fn recent(last_ms: u64, now_ms: u64, window: Duration) -> bool {
	last_ms != 0 && now_ms.saturating_sub(last_ms) <= window.as_millis() as u64
}

fn activity_loop() {
	let session = Connection::session().map_err(|e| tracing::warn!("power: no session bus for activity: {e}")).ok();
	let x11 = is_x11_session();
	// GNOME Session inhibit cookie while held.
	let mut gnome_cookie: Option<u32> = None;
	loop {
		let last = LAST_INPUT_MS.load(Ordering::Relaxed);
		let now = now_ms();
		if recent(last, now, POKE_EVERY) {
			if x11 {
				reset_x_screen_saver();
			}
			if let Some(conn) = &session {
				let _ = conn.call_method(
					Some("org.freedesktop.ScreenSaver"),
					"/org/freedesktop/ScreenSaver",
					Some("org.freedesktop.ScreenSaver"),
					"SimulateUserActivity",
					&(),
				);
				if gnome_cookie.is_none() {
					gnome_cookie = gnome_inhibit_idle(conn);
				}
			}
		}
		if let (Some(cookie), Some(conn)) = (gnome_cookie, &session) {
			if !recent(last, now, IDLE_HOLD) {
				let _ = conn.call_method(
					Some("org.gnome.SessionManager"),
					"/org/gnome/SessionManager",
					Some("org.gnome.SessionManager"),
					"Uninhibit",
					&(cookie,),
				);
				gnome_cookie = None;
			}
		}
		std::thread::sleep(POKE_EVERY);
	}
}

/// GNOME Session "idle" inhibitor (flag 8); `None` where GNOME Session is not running.
fn gnome_inhibit_idle(conn: &Connection) -> Option<u32> {
	let reply = conn
		.call_method(
			Some("org.gnome.SessionManager"),
			"/org/gnome/SessionManager",
			Some("org.gnome.SessionManager"),
			"Inhibit",
			&("pulsar", 0u32, "A remote session is being used", 8u32),
		)
		.ok()?;
	reply.body().deserialize::<u32>().ok()
}

fn reset_x_screen_saver() {
	use x11::xlib;
	thread_local! {
		static DPY: std::cell::Cell<*mut xlib::Display> = const { std::cell::Cell::new(std::ptr::null_mut()) };
	}
	DPY.with(|d| {
		// SAFETY: this thread's own Display connection, opened once and kept.
		unsafe {
			if d.get().is_null() {
				d.set(xlib::XOpenDisplay(std::ptr::null()));
			}
			let dpy = d.get();
			if !dpy.is_null() {
				// Resets the X screen saver AND the DPMS timers (both run off the last input time).
				xlib::XResetScreenSaver(dpy);
				xlib::XFlush(dpy);
			}
		}
	});
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn dpms_level_maps_to_display_off() {
		assert!(!dpms_off(true, 0)); // On
		assert!(dpms_off(true, 1)); // Standby
		assert!(dpms_off(true, 2)); // Suspend
		assert!(dpms_off(true, 3)); // Off
		assert!(!dpms_off(false, 3)); // DPMS disabled: the level means nothing
	}

	#[test]
	fn x11_session_detection() {
		assert!(x11_session_from("x11", false, true));
		assert!(!x11_session_from("wayland", true, true));
		assert!(!x11_session_from("", true, true)); // XWayland
		assert!(x11_session_from("", false, true));
		assert!(!x11_session_from("tty", false, false));
	}

	#[test]
	fn recent_input_window() {
		let w = Duration::from_secs(20);
		assert!(!recent(0, 5_000, w)); // never
		assert!(recent(1_000, 21_000, w));
		assert!(!recent(1_000, 21_001, w));
		assert!(recent(5_000, 4_000, w)); // clock skew between threads: still recent
	}
}
