//! "Keep on top" (window pin, issue #18): any session window (the main window or a torn-off
//! tab window) can be pinned above other apps. Window-level, not per session — every tab in a
//! pinned window stays on top with it.
//!
//! * The title-bar pin button and the Pulsar menu's "Keep on top" row both land here
//!   (`window_pin_set`); every change is echoed to that window (`window-pin` event) and to the
//!   renderers of the sessions it shows (`pin <available> <on>`), so both controls agree.
//! * Settings → General can turn the feature off (`set_window_pin_enabled`, pushed by the
//!   frontend at start and on change). Off = nothing can pin, and every pinned window is
//!   released on the spot.
//! * Fullscreen keeps the pin: leaving fullscreen re-applies it (`reapply`) instead of the old
//!   unconditional `set_always_on_top(false)` self-heal.
//! * A tab torn off a pinned window opens pinned too (`tabwin::tab_detach` asks `is_pinned`).
//! * Not remembered across restarts: a window that starts on top by itself is a surprise.
//!
//! Linux Wayland: the app runs under XWayland, where most compositors honour keep-above for X
//! clients; the frontend still warns that the desktop may ignore it.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use tauri::{AppHandle, Emitter, Manager, WebviewWindow};

use crate::state::AppState;

/// Settings → General → "Keep on top". On by default (the frontend pushes the saved value).
static ENABLED: AtomicBool = AtomicBool::new(true);
/// Labels of the pinned windows.
static PINNED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

pub(crate) fn enabled() -> bool {
	ENABLED.load(Ordering::SeqCst)
}

pub(crate) fn is_pinned(label: &str) -> bool {
	enabled() && PINNED.lock().unwrap().as_ref().is_some_and(|s| s.contains(label))
}

/// Record `label` as pinned without touching the window (a new window built pinned).
pub(crate) fn mark(label: &str, on: bool) {
	let mut g = PINNED.lock().unwrap();
	let s = g.get_or_insert_with(HashSet::new);
	if on {
		s.insert(label.to_string());
	} else {
		s.remove(label);
	}
}

/// A window is gone: forget it.
pub(crate) fn forget(label: &str) {
	mark(label, false);
}

/// The renderer line for play `id`: `pin <available 0|1> <on 0|1>` (the Pulsar menu row).
pub(crate) fn render_line(id: u64) -> String {
	let label = crate::tabwin::label_for_play(id);
	format!("pin {} {}", u8::from(enabled()), u8::from(is_pinned(&label)))
}

/// Re-apply the window's pin (on the UI thread already): after leaving fullscreen.
pub(crate) fn reapply(w: &WebviewWindow) {
	let _ = w.set_always_on_top(is_pinned(w.label()));
}

/// Tell window `label` and the renderers of its sessions the current pin state.
fn notify(app: &AppHandle, label: &str) {
	let on = is_pinned(label);
	// The payload names the window: frontend global listeners hear other windows' emits too.
	let _ = app.emit_to(label, "window-pin", (label.to_string(), on));
	let state = app.state::<AppState>();
	let stdins: Vec<_> = state
		.plays
		.lock()
		.unwrap()
		.iter()
		.filter(|(id, _)| crate::tabwin::label_for_play(**id) == label)
		.map(|(_, p)| p.render_stdin.clone())
		.collect();
	let line = format!("pin {} {}", u8::from(enabled()), u8::from(on));
	for si in stdins {
		use std::io::Write as _;
		if let Some(si) = si.lock().unwrap().as_mut() {
			let _ = writeln!(si, "{line}");
		}
	}
}

fn apply(app: &AppHandle, label: &str, on: bool) {
	mark(label, on);
	if let Some(w) = app.get_webview_window(label) {
		let w2 = w.clone();
		// Window ops belong on the UI thread (off it they can silently no-op on GTK).
		let _ = w.run_on_main_thread(move || {
			let _ = w2.set_always_on_top(on);
		});
	}
	notify(app, label);
}

/// Pin / unpin the calling window. Refused while the feature is off in Settings.
#[tauri::command]
pub(crate) fn window_pin_set(app: AppHandle, window: WebviewWindow, on: bool) -> Result<bool, String> {
	if on && !enabled() {
		return Err("keep on top is turned off in Settings".into());
	}
	if !crate::tabwin::is_session_window(window.label()) {
		return Err("not a session window".into());
	}
	tracing::info!(window = window.label(), on, "window pin");
	apply(&app, window.label(), on);
	Ok(on)
}

/// Is the calling window pinned?
#[tauri::command]
pub(crate) fn window_pin_get(window: WebviewWindow) -> bool {
	is_pinned(window.label())
}

/// Pin the window showing play `id` (the Pulsar menu's row, routed by the session).
#[tauri::command]
pub(crate) fn play_pin_set(app: AppHandle, id: u64, on: bool) -> Result<bool, String> {
	if on && !enabled() {
		return Err("keep on top is turned off in Settings".into());
	}
	let label = crate::tabwin::label_for_play(id);
	tracing::info!(window = %label, id, on, "window pin (menu)");
	apply(&app, &label, on);
	Ok(on)
}

/// Linux on a Wayland session: the app runs under XWayland and the compositor may ignore
/// keep-above — the title bar's pin says so in its tooltip.
#[tauri::command]
pub(crate) fn window_pin_maybe_ignored() -> bool {
	#[cfg(all(unix, not(target_os = "macos")))]
	{
		std::env::var_os("WAYLAND_DISPLAY").is_some()
			|| std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v.eq_ignore_ascii_case("wayland"))
	}
	#[cfg(not(all(unix, not(target_os = "macos"))))]
	{
		false
	}
}

/// Settings → General → "Keep on top". Turning it off releases every pinned window.
#[tauri::command]
pub(crate) fn set_window_pin_enabled(app: AppHandle, enabled: bool) {
	let was = ENABLED.swap(enabled, Ordering::SeqCst);
	if was == enabled {
		return;
	}
	let pinned: Vec<String> = PINNED.lock().unwrap().as_ref().map(|s| s.iter().cloned().collect()).unwrap_or_default();
	if !enabled {
		for label in &pinned {
			apply(&app, label, false);
		}
	}
	// Every session window's renderers learn whether the menu row exists.
	let labels: Vec<String> = app
		.webview_windows()
		.keys()
		.filter(|l| crate::tabwin::is_session_window(l))
		.cloned()
		.collect();
	for label in labels {
		notify(&app, &label);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn pin_follows_the_setting() {
		ENABLED.store(true, Ordering::SeqCst);
		mark("tabwin-77", true);
		assert!(is_pinned("tabwin-77"));
		ENABLED.store(false, Ordering::SeqCst);
		assert!(!is_pinned("tabwin-77"), "a disabled feature pins nothing");
		ENABLED.store(true, Ordering::SeqCst);
		forget("tabwin-77");
		assert!(!is_pinned("tabwin-77"));
	}
}
