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
/// a pad press is not keyboard/mouse input, so a plain SetForegroundWindow just flashes the
/// taskbar button. The usual workaround: a synthetic Alt tap counts as "last input" for us.
#[cfg(windows)]
fn force_foreground(hwnd: windows_sys::Win32::Foundation::HWND) {
	use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
		SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_MENU,
	};
	use windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow;
	let key = |flags| INPUT {
		r#type: INPUT_KEYBOARD,
		Anonymous: INPUT_0 {
			ki: KEYBDINPUT { wVk: VK_MENU, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 },
		},
	};
	let inputs = [key(0), key(KEYEVENTF_KEYUP)];
	unsafe {
		SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32);
		SetForegroundWindow(hwnd);
	}
}
