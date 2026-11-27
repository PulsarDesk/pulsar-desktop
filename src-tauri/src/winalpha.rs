//! Window opacity (issue #18 follow-up): a session window (the main window, a torn-off tab
//! window or a picture-in-picture window) can be made see-through, pinned or not.
//!
//! * Settings → General: "Transparency" on/off (default on) and the default opacity (100 % =
//!   opaque) every window starts with (`set_window_alpha_prefs`, pushed by the frontend).
//! * Per window, for now: the Pulsar menu's Tools row (`opacity <available> <pct>` in,
//!   `ov set opacity <pct>` out → `play_alpha_set`) overrides the default for that window.
//! * Off in Settings: every window goes back to opaque and the row disappears.
//!
//! Per OS: Windows = a layered top-level window (`WS_EX_LAYERED` + `LWA_ALPHA`; removed again at
//! 100 % so an opaque window pays nothing); Linux = GTK's toplevel opacity
//! (`_NET_WM_WINDOW_OPACITY`, needs a compositing WM); macOS = the NSWindow's `alphaValue`, and
//! the renderer's own panel follows through the same `opacity` line.
//!
//! UNTESTED on hardware: whether the native video (a flip-model swap chain in a child window on
//! Windows) turns see-through with its layered parent.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Mutex;

use tauri::{AppHandle, Emitter, Manager, WebviewWindow};

static ENABLED: AtomicBool = AtomicBool::new(true);
/// The default opacity in % (30..=100).
static DEFAULT: AtomicU8 = AtomicU8::new(100);
/// Per-window overrides (label → %).
static OVERRIDE: Mutex<Option<HashMap<String, u8>>> = Mutex::new(None);

const MIN: u8 = 30;

/// The opacity window `label` should have now (%).
pub(crate) fn current(label: &str) -> u8 {
	if !ENABLED.load(Ordering::SeqCst) {
		return 100;
	}
	OVERRIDE
		.lock()
		.unwrap()
		.as_ref()
		.and_then(|m| m.get(label).copied())
		.unwrap_or_else(|| DEFAULT.load(Ordering::SeqCst))
		.clamp(MIN, 100)
}

pub(crate) fn forget(label: &str) {
	if let Some(m) = OVERRIDE.lock().unwrap().as_mut() {
		m.remove(label);
	}
}

/// The renderer line for play `id`: `opacity <available 0|1> <pct>`.
pub(crate) fn render_line(id: u64) -> String {
	let label = crate::tabwin::label_for_play(id);
	format!("opacity {} {}", u8::from(ENABLED.load(Ordering::SeqCst)), current(&label))
}

/// Apply window `label`'s opacity (any thread).
pub(crate) fn apply(app: &AppHandle, label: &str) {
	let Some(w) = app.get_webview_window(label) else { return };
	let pct = current(label);
	let w2 = w.clone();
	let _ = w.run_on_main_thread(move || set_alpha(&w2, pct));
	// Its renderers: the menu row, and the macOS panel's own alpha.
	let state = app.state::<crate::state::AppState>();
	let stdins: Vec<_> = state
		.plays
		.lock()
		.unwrap()
		.iter()
		.filter(|(id, _)| crate::tabwin::label_for_play(**id) == label)
		.map(|(_, p)| p.render_stdin.clone())
		.collect();
	let line = format!("opacity {} {pct}", u8::from(ENABLED.load(Ordering::SeqCst)));
	for si in stdins {
		use std::io::Write as _;
		if let Some(si) = si.lock().unwrap().as_mut() {
			let _ = writeln!(si, "{line}");
		}
	}
	let _ = app.emit_to(label, "window-alpha", (label.to_string(), pct));
}

/// Re-apply window `w`'s opacity alone (on the UI thread already): a window shown after the
/// opacity was first set can come up opaque (GTK applies it to a mapped toplevel only), so session
/// windows re-apply it when they gain the focus. Idempotent and cheap (100 % = opaque, nothing
/// layered).
pub(crate) fn reapply(w: &WebviewWindow) {
	set_alpha(w, current(w.label()));
}

#[cfg(windows)]
fn set_alpha(w: &WebviewWindow, pct: u8) {
	use windows_sys::Win32::Foundation::HWND;
	use windows_sys::Win32::UI::WindowsAndMessaging::{
		GetWindowLongPtrW, SetLayeredWindowAttributes, SetWindowLongPtrW, GWL_EXSTYLE, LWA_ALPHA, WS_EX_LAYERED,
	};
	let Ok(h) = w.hwnd() else { return };
	let hwnd = h.0 as HWND;
	unsafe {
		let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
		if pct >= 100 {
			if ex & WS_EX_LAYERED as isize != 0 {
				SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex & !(WS_EX_LAYERED as isize));
			}
			return;
		}
		if ex & WS_EX_LAYERED as isize == 0 {
			SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED as isize);
		}
		let a = ((pct as u32 * 255) / 100) as u8;
		SetLayeredWindowAttributes(hwnd, 0, a, LWA_ALPHA);
	}
}

#[cfg(all(unix, not(target_os = "macos")))]
fn set_alpha(w: &WebviewWindow, pct: u8) {
	use gtk::prelude::WidgetExt;
	let Ok(g) = w.gtk_window() else { return };
	let a = pct as f64 / 100.0;
	g.set_opacity(a);
	// Set before the window was mapped, GTK keeps the value but never writes the X property
	// (`_NET_WM_WINDOW_OPACITY`), and setting the same value again is a no-op: write it on the
	// GDK window directly once there is one.
	if let Some(gw) = g.window() {
		gw.set_opacity(a);
	}
}

#[cfg(target_os = "macos")]
fn set_alpha(w: &WebviewWindow, pct: u8) {
	use objc2_app_kit::NSWindow;
	let Ok(p) = w.ns_window() else { return };
	// SAFETY: Tauri hands out the live NSWindow of this window; we only set a property, on the
	// main thread (`apply` runs us there).
	let win: &NSWindow = unsafe { &*(p as *const NSWindow) };
	win.setAlphaValue(pct as f64 / 100.0);
}

/// Settings → General: transparency on/off and the default opacity (%). Re-applied to every
/// session window (a per-window pick stays while the feature is on).
#[tauri::command]
pub(crate) fn set_window_alpha_prefs(app: AppHandle, enabled: bool, default_pct: u8) {
	ENABLED.store(enabled, Ordering::SeqCst);
	DEFAULT.store(default_pct.clamp(MIN, 100), Ordering::SeqCst);
	if !enabled {
		OVERRIDE.lock().unwrap().take();
	}
	let labels: Vec<String> =
		app.webview_windows().keys().filter(|l| crate::tabwin::is_session_window(l)).cloned().collect();
	for l in labels {
		apply(&app, &l);
	}
}

/// The Pulsar menu's row: window showing play `id` → `pct` %.
#[tauri::command]
pub(crate) fn play_alpha_set(app: AppHandle, id: u64, pct: u8) -> Result<(), String> {
	if !ENABLED.load(Ordering::SeqCst) {
		return Err("transparency is turned off in Settings".into());
	}
	let label = crate::tabwin::label_for_play(id);
	OVERRIDE
		.lock()
		.unwrap()
		.get_or_insert_with(HashMap::new)
		.insert(label.clone(), pct.clamp(MIN, 100));
	apply(&app, &label);
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_window_pick_wins_over_the_default_while_on() {
		ENABLED.store(true, Ordering::SeqCst);
		DEFAULT.store(90, Ordering::SeqCst);
		assert_eq!(current("tabwin-88"), 90);
		OVERRIDE.lock().unwrap().get_or_insert_with(HashMap::new).insert("tabwin-88".into(), 10);
		assert_eq!(current("tabwin-88"), MIN, "never fully invisible");
		ENABLED.store(false, Ordering::SeqCst);
		assert_eq!(current("tabwin-88"), 100, "off = opaque");
		ENABLED.store(true, Ordering::SeqCst);
		DEFAULT.store(100, Ordering::SeqCst);
		forget("tabwin-88");
	}
}
