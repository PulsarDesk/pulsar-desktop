//! Linux / macOS: keep Pulsar's idea of the main window's display scale current, and push a
//! change to the live native renderers — the counterpart of Windows' `dpi_watch.rs`.
//!
//! * **Linux** — tao (Tauri's windowing) never emits `ScaleFactorChanged` on GTK, so a monitor
//!   scale change (GNOME/KDE settings, `Gdk/WindowScalingFactor` XSETTING) or moving the window to
//!   a monitor with another scale went unnoticed: the renderer overlay kept the spawn-time
//!   `PULSAR_UI_SCALE`. Here the GTK window's `scale-factor` property is watched (GTK updates it
//!   from the XSETTINGS manager on X11 and from `wl_surface.preferred_buffer_scale` /
//!   `wl_output.scale` on Wayland). WebKitGTK follows GTK's scale by itself.
//! * **macOS** — tao does emit `ScaleFactorChanged` (`NSWindowDidChangeBackingPropertiesNotification`)
//!   and WKWebView re-rasterizes by itself; on top of that a CoreGraphics display-reconfiguration
//!   callback re-reads the window's scale after any display change (a display added/removed, a
//!   "looks like" resolution change of the display the window is on) — the analog of Windows'
//!   `WM_DISPLAYCHANGE` hook. The macOS renderer (eframe/winit) follows its own window's scale.
//!
//! Untested on Linux and macOS hardware.

use tauri::AppHandle;
#[cfg(target_os = "linux")]
use tauri::Manager;

/// The main window's scale became `scale`: remember it (new renderers get it at spawn) and
/// tell every live renderer (Linux: `uiscale <f>` on its stdin; the overlay re-scales).
pub(crate) fn changed(app: &AppHandle, scale: f64) {
	if !scale.is_finite() || scale <= 0.0 {
		return;
	}
	if crate::process::ui_scale().is_some_and(|s| (s - scale).abs() < 1e-3) {
		return;
	}
	tracing::info!(scale, "scale watch: main window display scale changed");
	crate::process::set_ui_scale(scale);
	#[cfg(target_os = "linux")]
	{
		use std::io::Write;
		let st = app.state::<crate::state::AppState>();
		let stdins: Vec<_> = st.plays.lock().unwrap().values().map(|p| p.render_stdin.clone()).collect();
		for si in stdins {
			if let Some(si) = si.lock().unwrap().as_mut() {
				let _ = writeln!(si, "uiscale {scale}");
				let _ = si.flush();
			}
		}
	}
	#[cfg(not(target_os = "linux"))]
	let _ = app;
}

/// Linux: watch a freshly built main window's GTK `scale-factor` (call after every build of
/// the main window — ui_start may rebuild it; a destroyed window takes its handler with it).
#[cfg(target_os = "linux")]
pub(crate) fn watch_window(app: &AppHandle, w: &tauri::WebviewWindow) {
	// GTK objects live on the main thread; ui_start's retry builds the window from another.
	let (app2, w) = (app.clone(), w.clone());
	let _ = app.run_on_main_thread(move || {
		use gtk::prelude::*;
		let Ok(gw) = w.gtk_window() else {
			tracing::warn!("scale watch: no GTK window for main");
			return;
		};
		changed(&app2, gw.scale_factor() as f64);
		gw.connect_scale_factor_notify(move |gw| changed(&app2, gw.scale_factor() as f64));
	});
}

/// macOS: hear display reconfigurations (call once from the app's setup).
#[cfg(target_os = "macos")]
pub(crate) fn start(app: &AppHandle) {
	static APP: std::sync::OnceLock<AppHandle> = std::sync::OnceLock::new();
	if APP.set(app.clone()).is_err() {
		return;
	}
	fn on_reconfigure() {
		// Runs on the main run loop: read the scale off it (Tauri getters round-trip through
		// the event loop), a moment later so the new layout has settled.
		std::thread::spawn(|| {
			for ms in [0u64, 500, 2000] {
				std::thread::sleep(std::time::Duration::from_millis(ms));
				let Some(app) = APP.get() else { return };
				if let Some(w) = tauri::Manager::get_webview_window(app, "main") {
					if let Ok(s) = w.scale_factor() {
						changed(app, s);
					}
				}
			}
		});
	}
	if !pulsar_capture::display_mode::macos::on_reconfigure(on_reconfigure) {
		tracing::warn!("scale watch: CGDisplayRegisterReconfigurationCallback failed");
	}
}
