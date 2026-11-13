//! Main window + splash creation that can never block or kill the host core.
//!
//! Tauri builds every `tauri.conf` window on the event-loop thread BEFORE our `setup` closure
//! runs, and panics ("Failed to setup app") when one fails. Inside a LOCKED Windows session
//! (LogonUI up, input desktop = Winlogon — e.g. an unattended Duo seat, autostart, a restart
//! after a crash) WebView2 controller creation hangs and/or fails after ~2 min with
//! `0x800705B4` (timeout). The main window already starts hidden (`visible:false`), so
//! "create it hidden" does not avoid the hang; not creating it while the session is locked does.
//!
//! So the main window is `create:false` in `tauri.conf.json` and built here instead:
//! - session usable (the normal case): built right away in `setup`, exactly where Tauri built
//!   it before, then the splash — normal startup is unchanged;
//! - session locked / input desktop not ours: creation is deferred, the host goes online
//!   without the UI ([`crate::headless`]), and a light poll (2 s) builds the window once the
//!   session unlocks;
//! - creation fails or panics: logged, the host goes online without the UI, and creation is
//!   retried with a backoff (only while the session is usable).
//!
//! Secondary windows that the HOST opens on its own (approval popup, connections window,
//! toasts, remote-cursor label) check [`windows_allowed`] so a client connecting to a locked
//! host doesn't wedge the event loop on a webview that can't be created.
//!
//! Test hooks (debug runs only, harmless in release): `PULSAR_TEST_WEBVIEW_FAIL=1` makes every
//! main-window build fail; `PULSAR_TEST_LOCKED=<secs>` pretends the session is locked for the
//! first `<secs>` seconds of the process.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager, WebviewWindowBuilder};

/// The session's lock state as Windows reports it (`WTSSessionInfoEx`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lock {
	Locked,
	Unlocked,
	Unknown,
}

/// The input desktop as seen from this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Desk {
	/// `Default` — the user's desktop is receiving input.
	Default,
	/// Some other named desktop (`Winlogon` = lock screen / secure desktop).
	Other,
	/// `OpenInputDesktop` refused us (a user process can't open Winlogon; also disconnected).
	Inaccessible,
	/// Not queried / not applicable (non-Windows, or the name query failed).
	Unknown,
}

/// Whether a webview must NOT be created now. An explicit WTS "locked" always blocks; a named
/// non-Default input desktop blocks (lock screen, UAC secure desktop); an inaccessible input
/// desktop blocks only when WTS couldn't tell us the lock state (so a seat type where
/// `OpenInputDesktop` always fails but WTS says "unlocked" still gets its window).
pub(crate) fn blocked(lock: Lock, desk: Desk) -> bool {
	match (lock, desk) {
		(Lock::Locked, _) => true,
		(_, Desk::Other) => true,
		(Lock::Unknown, Desk::Inaccessible) => true,
		_ => false,
	}
}

/// Next retry delay after a failed main-window build: 5 s doubling to a 60 s cap.
pub(crate) fn next_backoff(prev: Option<Duration>) -> Duration {
	match prev {
		None => Duration::from_secs(5),
		Some(d) => (d * 2).min(Duration::from_secs(60)),
	}
}

/// Set once the main window exists.
static UI_UP: AtomicBool = AtomicBool::new(false);
/// Tray "show" / a second launch asked for the window: skip the current backoff.
static WANT_NOW: AtomicBool = AtomicBool::new(false);
static T0: OnceLock<Instant> = OnceLock::new();

fn test_locked() -> bool {
	let Some(secs) = std::env::var("PULSAR_TEST_LOCKED")
		.ok()
		.and_then(|v| v.parse::<u64>().ok())
	else {
		return false;
	};
	T0.get_or_init(Instant::now).elapsed() < Duration::from_secs(secs)
}

fn test_fail() -> bool {
	std::env::var_os("PULSAR_TEST_WEBVIEW_FAIL").is_some_and(|v| v == "1")
}

#[cfg(windows)]
fn query() -> (Lock, Desk) {
	use windows_sys::Win32::System::RemoteDesktop::{
		WTSFreeMemory, WTSQuerySessionInformationW, WTSSessionInfoEx, WTSINFOEXW,
		WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTS_SESSIONSTATE_LOCK,
		WTS_SESSIONSTATE_UNLOCK,
	};
	use windows_sys::Win32::System::StationsAndDesktops::{
		CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, DESKTOP_READOBJECTS, UOI_NAME,
	};
	let lock = unsafe {
		let mut buf: windows_sys::core::PWSTR = std::ptr::null_mut();
		let mut n = 0u32;
		if WTSQuerySessionInformationW(
			WTS_CURRENT_SERVER_HANDLE,
			WTS_CURRENT_SESSION,
			WTSSessionInfoEx,
			&mut buf,
			&mut n,
		) == 0 || buf.is_null()
		{
			Lock::Unknown
		} else {
			let info = &*(buf as *const WTSINFOEXW);
			let l = if info.Level == 1 {
				match info.Data.WTSInfoExLevel1.SessionFlags as u32 {
					WTS_SESSIONSTATE_LOCK => Lock::Locked,
					WTS_SESSIONSTATE_UNLOCK => Lock::Unlocked,
					_ => Lock::Unknown,
				}
			} else {
				Lock::Unknown
			};
			WTSFreeMemory(buf as _);
			l
		}
	};
	let desk = unsafe {
		let h = OpenInputDesktop(0, 0, DESKTOP_READOBJECTS);
		if h.is_null() {
			Desk::Inaccessible
		} else {
			let mut name = [0u16; 64];
			let mut need = 0u32;
			let ok = GetUserObjectInformationW(
				h as _,
				UOI_NAME,
				name.as_mut_ptr() as _,
				(name.len() * 2) as u32,
				&mut need,
			) != 0;
			CloseDesktop(h);
			if ok {
				let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
				if String::from_utf16_lossy(&name[..len]).eq_ignore_ascii_case("Default") {
					Desk::Default
				} else {
					Desk::Other
				}
			} else {
				Desk::Unknown
			}
		}
	};
	(lock, desk)
}

#[cfg(not(windows))]
fn query() -> (Lock, Desk) {
	(Lock::Unknown, Desk::Unknown)
}

/// Live state: `(blocked, lock, desk)`.
fn state_now() -> (bool, Lock, Desk) {
	if test_locked() {
		return (true, Lock::Locked, Desk::Unknown);
	}
	let (l, d) = query();
	(blocked(l, d), l, d)
}

/// `--headless-host` / `--split-seat`: no window, ever (every host-side window builder —
/// approval popup, toasts, remote cursor — checks `windows_allowed`).
static HEADLESS: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_headless() {
	HEADLESS.store(true, Ordering::SeqCst);
}

/// Whether the host may open a window right now (not inside a locked session, not headless).
pub(crate) fn windows_allowed() -> bool {
	!HEADLESS.load(Ordering::SeqCst) && !state_now().0
}

/// Ask for the main window now (tray "show", a second launch) instead of after the backoff.
pub(crate) fn request_now() {
	if !UI_UP.load(Ordering::SeqCst) {
		WANT_NOW.store(true, Ordering::SeqCst);
	}
}

/// Build the main window from its `tauri.conf` entry, then the splash. Never panics.
fn try_create(app: &AppHandle) -> Result<(), String> {
	if test_fail() {
		return Err("simulated failure (PULSAR_TEST_WEBVIEW_FAIL=1)".into());
	}
	// A half-built leftover from a failed attempt would make the label collide.
	if let Some(w) = app.get_webview_window("main") {
		let _ = w.destroy();
	}
	let Some(cfg) = app
		.config()
		.app
		.windows
		.iter()
		.find(|w| w.label == "main")
		.cloned()
	else {
		return Err("no `main` window in tauri.conf".into());
	};
	let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
		WebviewWindowBuilder::from_config(app, &cfg).and_then(|b| b.build())
	}));
	match built {
		Ok(Ok(w)) => {
			UI_UP.store(true, Ordering::SeqCst);
			// The main window's ui scale + the splash, as `setup` did after Tauri's build.
			let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
				crate::splash::show(app);
				if let Ok(scale) = w.scale_factor() {
					crate::process::set_ui_scale(scale);
				}
			}));
			Ok(())
		}
		Ok(Err(e)) => Err(e.to_string()),
		Err(_) => Err("main window build panicked".into()),
	}
}

/// Called from `setup` (event-loop thread). Normal session: builds the main window + splash
/// right here. Locked session or a failed build: the host goes online without the UI and a
/// background thread builds the window once the session can show it.
pub(crate) fn start(app: &AppHandle) {
	let _ = T0.set(Instant::now());
	// Watchdog, armed BEFORE the build: a build that hangs on this thread, or a webview that
	// got built but never runs its UI (wedged renderer), must not keep the host offline.
	// A normal UI calls go_online within seconds, so this never fires on a healthy start.
	let h = app.clone();
	std::thread::spawn(move || {
		std::thread::sleep(Duration::from_secs(60));
		if !crate::headless::ui_called() {
			crate::headless::start(h, "UI never called go_online");
		}
	});
	let (blocked_now, lock, desk) = state_now();
	if !blocked_now {
		match try_create(app) {
			Ok(()) => return,
			Err(e) => {
				tracing::error!(
					"main window could not be created: {e} — hosting without the UI, will retry"
				);
				crate::headless::start(app.clone(), "main window failed");
				spawn_retry(app.clone(), Some(next_backoff(None)));
			}
		}
	} else {
		tracing::warn!(
			?lock,
			?desk,
			"session is locked — deferring the main window until it unlocks; hosting without the UI"
		);
		crate::headless::start(app.clone(), "session locked");
		spawn_retry(app.clone(), None);
	}
}

fn spawn_retry(app: AppHandle, first_backoff: Option<Duration>) {
	let _ = std::thread::Builder::new()
		.name("pulsar-ui-start".into())
		.spawn(move || {
			const POLL: Duration = Duration::from_secs(2);
			let mut backoff = first_backoff;
			let mut next_try = Instant::now() + first_backoff.unwrap_or_default();
			let mut was_blocked: Option<bool> = None;
			let mut attempts = 0u32;
			loop {
				std::thread::sleep(POLL);
				let (b, lock, desk) = state_now();
				if was_blocked != Some(b) {
					tracing::info!(blocked = b, ?lock, ?desk, "main window: session state");
				}
				let just_unlocked = was_blocked == Some(true) && !b;
				was_blocked = Some(b);
				// Wait one more poll after an unlock so the desktop switch settles.
				if b || just_unlocked {
					continue;
				}
				if Instant::now() < next_try && !WANT_NOW.swap(false, Ordering::SeqCst) {
					continue;
				}
				attempts += 1;
				tracing::info!(attempts, "main window: creating");
				match try_create(&app) {
					Ok(()) => {
						tracing::info!(attempts, "main window created");
						return;
					}
					Err(e) => {
						let d = next_backoff(backoff);
						backoff = Some(d);
						next_try = Instant::now() + d;
						tracing::warn!(attempts, retry_in = ?d, "main window could not be created: {e}");
					}
				}
			}
		});
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn blocked_matrix() {
		use Desk::{Default, Inaccessible, Other};
		use Lock::{Locked, Unlocked};
		assert!(blocked(Locked, Default));
		assert!(blocked(Locked, Desk::Unknown));
		assert!(blocked(Unlocked, Other));
		assert!(blocked(Lock::Unknown, Other));
		assert!(blocked(Lock::Unknown, Inaccessible));
		assert!(!blocked(Unlocked, Default));
		assert!(!blocked(Unlocked, Inaccessible));
		assert!(!blocked(Unlocked, Desk::Unknown));
		assert!(!blocked(Lock::Unknown, Default));
		// Non-Windows (nothing known) never blocks: unchanged startup there.
		assert!(!blocked(Lock::Unknown, Desk::Unknown));
	}

	#[test]
	fn backoff_doubles_to_cap() {
		let mut d = None;
		let mut seen = vec![];
		for _ in 0..6 {
			let n = next_backoff(d);
			seen.push(n.as_secs());
			d = Some(n);
		}
		assert_eq!(seen, vec![5, 10, 20, 40, 60, 60]);
	}
}
