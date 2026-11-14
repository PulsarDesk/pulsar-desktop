//! The machine going to sleep ends the sessions it is VIEWING, and waking up brings them back.
//!
//! A connected session keeps a client fully awake: its audio stream alone holds Windows out of
//! Modern Standby (sleep study: "Audio Active" 100 %), and the stream keeps arriving. Field
//! case, 2026-11-14: a laptop left connected overnight streamed ~16 Mbit into a dark screen for
//! six hours, never slept, and the host encoded the whole time.
//!
//! So when this machine goes into a sleep-like state every remote-play session ends — here, in
//! Rust, not only in the UI: with the display off the webview may be throttled, and a session
//! that waits for it would be the same bug again. The UI is told first (`system-sleep`): it
//! notes which sessions were open and connects them again on `system-wake`.
//!
//! "Sleep-like" = the system suspends, or its display turns off. On a Modern Standby machine
//! the display turning off IS the sleep (there is no suspend event while audio plays); on any
//! machine nobody is looking at a stream on a dark screen.
//!
//! Per platform (all untested on real hardware except Windows):
//! - Windows: `PowerRegisterSuspendResumeNotification` + `GUID_CONSOLE_DISPLAY_STATE` (below).
//! - Linux (`power_watch/linux.rs`): logind `PrepareForSleep` with a delay inhibitor (the
//!   sessions end BEFORE the suspend), the desktop's screen saver `ActiveChanged`
//!   (org.freedesktop / org.gnome), and the X server's DPMS state on X11.
//! - macOS (`power_watch/macos.rs`): NSWorkspace will-sleep / did-wake and
//!   screens-did-sleep / screens-did-wake, delivered on the main thread's run loop.

use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{AppHandle, Emitter, Manager};

use crate::state::AppState;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

/// The display is off / the system is suspended / the desktop's screen saver (blank or lock
/// screen) is up. Asleep = any of them. `SCREEN_SAVER` is only ever set on Linux.
static DISPLAY_OFF: AtomicBool = AtomicBool::new(false);
static SUSPENDED: AtomicBool = AtomicBool::new(false);
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
static SCREEN_SAVER: AtomicBool = AtomicBool::new(false);
/// What the UI was last told.
static ASLEEP: AtomicBool = AtomicBool::new(false);

/// The sessions sleep is ending (awaitable: Linux holds the suspend until they are done).
type Stops = Vec<tauri::async_runtime::JoinHandle<()>>;

/// Set one of the inputs and act on the resulting edge (if any).
#[cfg_attr(windows, allow(dead_code))]
fn set(app: &AppHandle, flag: &AtomicBool, value: bool, why: &'static str) -> Stops {
	flag.store(value, Ordering::SeqCst);
	update(app, why)
}

/// "Asleep" from the inputs: any one of them.
fn asleep_from(display_off: bool, suspended: bool, screen_saver: bool) -> bool {
	display_off || suspended || screen_saver
}

/// One of the inputs changed: tell the UI (and end the sessions) on an edge of "asleep".
fn update(app: &AppHandle, why: &'static str) -> Stops {
	let asleep = asleep_from(
		DISPLAY_OFF.load(Ordering::SeqCst),
		SUSPENDED.load(Ordering::SeqCst),
		SCREEN_SAVER.load(Ordering::SeqCst),
	);
	if ASLEEP.swap(asleep, Ordering::SeqCst) == asleep {
		return Vec::new();
	}
	if !asleep {
		tracing::info!(why, "power: awake again — the UI reconnects the sessions sleep ended");
		let _ = app.emit("system-wake", why);
		return Vec::new();
	}
	// The UI first (it notes what to bring back), then the sessions.
	let _ = app.emit("system-sleep", why);
	let ids: Vec<u64> = app.state::<AppState>().plays.lock().unwrap().keys().copied().collect();
	tracing::info!(why, sessions = ids.len(), "power: going to sleep — ending the viewed sessions");
	ids.into_iter()
		.map(|id| {
			let app = app.clone();
			tauri::async_runtime::spawn(async move {
				let state = app.state::<AppState>();
				if let Err(e) = crate::play::stop_stream(app.clone(), state, id).await {
					tracing::warn!(id, "power: stopping a session for sleep failed: {e}");
				}
			})
		})
		.collect()
}

/// Local keyboard/mouse input was just forwarded to a session (Linux evdev capture). The
/// grab hides it from the desktop's idle timer; the Linux watcher reports it as user activity
/// so the screen saver / DPMS does not blank a screen someone is playing on (which would
/// end the session). No-op elsewhere.
#[cfg(target_os = "linux")]
pub(crate) fn note_input() {
	linux::note_input();
}

#[cfg(target_os = "linux")]
pub(crate) fn spawn(app: AppHandle) {
	linux::spawn(app);
}

#[cfg(target_os = "macos")]
pub(crate) fn spawn(app: AppHandle) {
	macos::watch(move |ev| {
		use macos::Event::*;
		let _ = match ev {
			WillSleep => set(&app, &SUSPENDED, true, "suspend"),
			DidWake => set(&app, &SUSPENDED, false, "resume"),
			ScreensDidSleep => set(&app, &DISPLAY_OFF, true, "display off"),
			ScreensDidWake => set(&app, &DISPLAY_OFF, false, "display on"),
		};
	});
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub(crate) fn spawn(_app: AppHandle) {}

#[cfg(windows)]
pub(crate) fn spawn(app: AppHandle) {
	use std::ffi::c_void;

	#[repr(C)]
	struct Guid(u32, u16, u16, [u8; 8]);
	/// `DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS`.
	#[repr(C)]
	struct Subscribe {
		callback: unsafe extern "system" fn(*mut c_void, u32, *mut c_void) -> u32,
		context: *mut c_void,
	}
	/// `POWERBROADCAST_SETTING` (the data follows the header).
	#[repr(C)]
	struct Setting {
		power_setting: Guid,
		data_length: u32,
		data: [u8; 4],
	}
	#[link(name = "powrprof")]
	extern "system" {
		fn PowerSettingRegisterNotification(
			setting: *const Guid,
			flags: u32,
			recipient: *mut c_void,
			registration: *mut *mut c_void,
		) -> u32;
		fn PowerRegisterSuspendResumeNotification(
			flags: u32,
			recipient: *mut c_void,
			registration: *mut *mut c_void,
		) -> u32;
	}
	const DEVICE_NOTIFY_CALLBACK: u32 = 2;
	const PBT_APMSUSPEND: u32 = 0x0004;
	const PBT_APMRESUMESUSPEND: u32 = 0x0007;
	const PBT_APMRESUMEAUTOMATIC: u32 = 0x0012;
	const PBT_POWERSETTINGCHANGE: u32 = 0x8013;
	/// `GUID_CONSOLE_DISPLAY_STATE`: 0 = off, 1 = on, 2 = dimmed.
	const CONSOLE_DISPLAY_STATE: Guid =
		Guid(0x6fe6_9556, 0x704a, 0x47a0, [0x8f, 0x24, 0xc2, 0x8d, 0x93, 0x6f, 0xda, 0x47]);

	/// Runs on a system worker thread: only flips the flags and emits.
	unsafe extern "system" fn on_power(context: *mut c_void, kind: u32, setting: *mut c_void) -> u32 {
		// SAFETY: `context` is the leaked `AppHandle` registered below; it lives for the process.
		let app = unsafe { &*(context as *const AppHandle) };
		match kind {
			PBT_APMSUSPEND => {
				SUSPENDED.store(true, Ordering::SeqCst);
				let _ = update(app, "suspend");
			}
			// An automatic resume (a wake for background work) comes with the display still
			// off: `DISPLAY_OFF` keeps the sessions closed until someone is there.
			PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => {
				SUSPENDED.store(false, Ordering::SeqCst);
				let _ = update(app, "resume");
			}
			PBT_POWERSETTINGCHANGE if !setting.is_null() => {
				// SAFETY: for this notification `setting` points at a POWERBROADCAST_SETTING.
				let s = unsafe { &*(setting as *const Setting) };
				if s.data_length >= 4 {
					// Registered for the display state only, so no need to compare the GUID.
					let state = u32::from_ne_bytes(s.data);
					// The registration answers at once with the current state: one log line
					// that shows the notifications work on this machine.
					static FIRST: AtomicBool = AtomicBool::new(true);
					if FIRST.swap(false, Ordering::SeqCst) {
						tracing::info!(state, "power: display state notifications active (0 off, 1 on, 2 dimmed)");
					}
					match state {
						0 => {
							DISPLAY_OFF.store(true, Ordering::SeqCst);
							let _ = update(app, "display off");
						}
						1 => {
							DISPLAY_OFF.store(false, Ordering::SeqCst);
							let _ = update(app, "display on");
						}
						_ => {} // dimmed
					}
				}
			}
			_ => {}
		}
		0
	}

	// Both registrations live as long as the process: the handle and the parameters leak.
	let context = Box::into_raw(Box::new(app)) as *mut c_void;
	let params = Box::into_raw(Box::new(Subscribe { callback: on_power, context })) as *mut c_void;
	let mut reg: *mut c_void = std::ptr::null_mut();
	// SAFETY: valid pointers that outlive the registrations; the callback is `extern "system"`
	// with the DEVICE_NOTIFY_CALLBACK_ROUTINE signature. (Registering the display state calls
	// back at once with the current state — "on" — which is no edge.)
	let (a, b) = unsafe {
		(
			PowerRegisterSuspendResumeNotification(DEVICE_NOTIFY_CALLBACK, params, &mut reg),
			PowerSettingRegisterNotification(&CONSOLE_DISPLAY_STATE, DEVICE_NOTIFY_CALLBACK, params, &mut reg),
		)
	};
	if a != 0 || b != 0 {
		tracing::warn!(suspend = a, display = b, "power: sleep notifications not registered");
	}
}

#[cfg(test)]
mod tests {
	use super::asleep_from;

	#[test]
	fn any_input_means_asleep() {
		assert!(!asleep_from(false, false, false));
		assert!(asleep_from(true, false, false));
		assert!(asleep_from(false, true, false));
		assert!(asleep_from(false, false, true));
	}
}
