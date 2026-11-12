//! "Wake on controller": pressing a pad's Guide / PS / Home button while Pulsar is hidden
//! (tray), minimized or behind another app brings the main window to the front and asks
//! the UI to switch to gaming mode (`pad-wake` event). Opt-in from Settings → General.
//!
//! Driven by the always-on Guide watcher (`commands::spawn_guide_watcher`), which calls
//! [`on_guide_press`] on every press edge. While Pulsar is the focused window the press keeps
//! its old meaning (`guide-toggle`: the shell toggles gaming mode when idle); in the
//! background it never toggles anything silently any more.

use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{AppHandle, Emitter, Manager};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Turn the feature on/off (synced from the UI at startup and on every toggle).
pub fn set_enabled(on: bool) {
	ENABLED.store(on, Ordering::Relaxed);
}

/// A Guide/PS/Home press edge from any connected pad.
pub fn on_guide_press(app: &AppHandle) {
	let Some(w) = app.get_webview_window("main") else { return };
	let visible = w.is_visible().unwrap_or(false);
	let minimized = w.is_minimized().unwrap_or(false);
	if visible && !minimized && w.is_focused().unwrap_or(false) {
		let _ = app.emit("guide-toggle", ());
	} else if ENABLED.load(Ordering::Relaxed) {
		tracing::info!(visible, minimized, "pad wake: Guide pressed — bringing Pulsar forward in gaming mode");
		wake(app, &w);
	}
}

/// Bring the main window to the front (shown, restored, focused) — e.g. a reverse-direction
/// request: this device is about to become the client, so its Pulsar must be on screen.
pub(crate) fn bring_forward(app: &AppHandle) {
	let app2 = app.clone();
	let _ = app.run_on_main_thread(move || {
		let Some(w) = app2.get_webview_window("main") else { return };
		let _ = w.show();
		let _ = w.unminimize();
		#[cfg(windows)]
		if let Ok(hwnd) = w.hwnd() {
			force_foreground(hwnd.0 as _);
		}
		let _ = w.set_focus();
	});
}

fn wake(app: &AppHandle, w: &tauri::WebviewWindow) {
	let _ = w.show();
	let _ = w.unminimize();
	#[cfg(windows)]
	if let Ok(hwnd) = w.hwnd() {
		force_foreground(hwnd.0 as _);
	}
	let _ = w.set_focus();
	let _ = app.emit("pad-wake", ());
}

/// Windows only lets the process that owns the foreground (or got the last input) take it;
/// a pad press / a network request is not our input, so a plain SetForegroundWindow just
/// flashes the taskbar button. Join the foreground thread's input queue for the call (the
/// documented way to hand focus over), then detach.
///
/// NOT a synthetic Alt tap (the old trick): Alt down+up on our frameless window turned into
/// WM_SYSCOMMAND SC_KEYMENU — Windows' menu loop then held the UI thread until the next key or
/// click, so no event reached the webview (2026-11-13: after a reverse-direction connect the
/// UI froze — the video covered the window, the session never ended).
#[cfg(windows)]
fn force_foreground(hwnd: windows_sys::Win32::Foundation::HWND) {
	use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
	use windows_sys::Win32::UI::WindowsAndMessaging::{
		BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow,
	};
	unsafe {
		let fg = GetForegroundWindow();
		let me = GetCurrentThreadId();
		let fg_tid = if fg.is_null() { 0 } else { GetWindowThreadProcessId(fg, std::ptr::null_mut()) };
		let attached = fg_tid != 0 && fg_tid != me && AttachThreadInput(me, fg_tid, 1) != 0;
		BringWindowToTop(hwnd);
		let ok = SetForegroundWindow(hwnd) != 0;
		if attached {
			AttachThreadInput(me, fg_tid, 0);
		}
		if !ok {
			tracing::info!("bring forward: Windows kept the foreground elsewhere (taskbar flash)");
		}
	}
}
