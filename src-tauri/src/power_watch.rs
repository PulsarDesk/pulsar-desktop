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
//! Windows only for now. Elsewhere a real suspend freezes the app, the host times the session
//! out by itself (its 6 s liveness), and a display that only turns off is not noticed.

use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{AppHandle, Emitter, Manager};

use crate::state::AppState;

/// The display is off / the system is suspended. Asleep = either.
static DISPLAY_OFF: AtomicBool = AtomicBool::new(false);
static SUSPENDED: AtomicBool = AtomicBool::new(false);
/// What the UI was last told.
static ASLEEP: AtomicBool = AtomicBool::new(false);

/// One of the two inputs changed: tell the UI (and end the sessions) on an edge of "asleep".
fn update(app: &AppHandle, why: &'static str) {
	let asleep = DISPLAY_OFF.load(Ordering::SeqCst) || SUSPENDED.load(Ordering::SeqCst);
	if ASLEEP.swap(asleep, Ordering::SeqCst) == asleep {
		return;
	}
	if !asleep {
		tracing::info!(why, "power: awake again — the UI reconnects the sessions sleep ended");
		let _ = app.emit("system-wake", why);
		return;
	}
	// The UI first (it notes what to bring back), then the sessions.
	let _ = app.emit("system-sleep", why);
	let ids: Vec<u64> = app.state::<AppState>().plays.lock().unwrap().keys().copied().collect();
	tracing::info!(why, sessions = ids.len(), "power: going to sleep — ending the viewed sessions");
	for id in ids {
		let app = app.clone();
		tauri::async_runtime::spawn(async move {
			let state = app.state::<AppState>();
			if let Err(e) = crate::play::stop_stream(app.clone(), state, id).await {
				tracing::warn!(id, "power: stopping a session for sleep failed: {e}");
			}
		});
	}
}

#[cfg(not(windows))]
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
				update(app, "suspend");
			}
			// An automatic resume (a wake for background work) comes with the display still
			// off: `DISPLAY_OFF` keeps the sessions closed until someone is there.
			PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => {
				SUSPENDED.store(false, Ordering::SeqCst);
				update(app, "resume");
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
							update(app, "display off");
						}
						1 => {
							DISPLAY_OFF.store(false, Ordering::SeqCst);
							update(app, "display on");
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
