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
		if let Some(w) = app2.get_webview_window("main") {
			raise(&w);
		}
	});
}

fn wake(app: &AppHandle, w: &tauri::WebviewWindow) {
	// The platform raise calls (GTK, AppKit) must run on the main thread; the Guide watcher
	// calls us from its own thread.
	let w2 = w.clone();
	let _ = app.run_on_main_thread(move || raise(&w2));
	let _ = app.emit("pad-wake", ());
}

/// Show + restore + the platform's "take the foreground from another app" + focus.
fn raise(w: &tauri::WebviewWindow) {
	let _ = w.show();
	let _ = w.unminimize();
	#[cfg(windows)]
	if let Ok(hwnd) = w.hwnd() {
		force_foreground(hwnd.0 as _);
	}
	#[cfg(target_os = "linux")]
	linux_raise::force_foreground(w);
	#[cfg(target_os = "macos")]
	if let Ok(ns_window) = w.ns_window() {
		crate::mac_activate::activate(ns_window);
	}
	let _ = w.set_focus();
}

/// Linux: a plain `gtk_window_present()` from the background loses to focus-stealing
/// prevention — the WM only flags the window "demands attention" because the request carries
/// no fresh user timestamp (the pad press is not X input).
///
/// * X11: present with the X server's CURRENT time (`gdk_x11_get_server_time`), and send the
///   EWMH `_NET_ACTIVE_WINDOW` client message with source indication 2 ("pager"): window
///   managers (mutter, KWin, xfwm4, openbox…) honour pager requests unconditionally — the
///   same request `wmctrl -a` makes. The window is moved to the current workspace first.
/// * Wayland: there is no way for a background client to take focus — xdg-activation needs a
///   token minted from a focused surface or a launcher. `present()` is all the protocol
///   allows; GNOME shows "Pulsar is ready", KWin flashes the task entry.
///
/// Untested on Linux hardware (written 2026-11-14).
#[cfg(target_os = "linux")]
mod linux_raise {
	use gtk::glib::Cast;
	use gtk::prelude::{GtkWindowExt, WidgetExt};

	pub(super) fn force_foreground(w: &tauri::WebviewWindow) {
		let Ok(win) = w.gtk_window() else { return };
		let Some(gdk_win) = win.window() else {
			win.present();
			return;
		};
		match gdk_win.downcast::<gdkx11::X11Window>() {
			Ok(x) => {
				let ts = gdkx11::x11_get_server_time(&x);
				x.move_to_current_desktop();
				win.present_with_time(ts);
				net_active_window(x.xid(), ts);
			}
			Err(_) => {
				win.present();
				tracing::info!("bring forward: Wayland — the compositor decides (no activation token)");
			}
		}
	}

	/// EWMH `_NET_ACTIVE_WINDOW` (source = pager) to the root window, on our own short-lived
	/// Xlib connection (GDK's is not exposed safely; the request is server-side anyway).
	fn net_active_window(xid: x11::xlib::Window, ts: u32) {
		use x11::xlib;
		unsafe {
			let dpy = xlib::XOpenDisplay(std::ptr::null());
			if dpy.is_null() {
				return;
			}
			let root = xlib::XDefaultRootWindow(dpy);
			let mut msg: xlib::XClientMessageEvent = std::mem::zeroed();
			msg.type_ = xlib::ClientMessage;
			msg.display = dpy;
			msg.window = xid;
			msg.message_type = xlib::XInternAtom(dpy, c"_NET_ACTIVE_WINDOW".as_ptr(), xlib::False);
			msg.format = 32;
			msg.data.set_long(0, 2); // source indication: pager
			msg.data.set_long(1, ts as std::os::raw::c_long);
			msg.data.set_long(2, 0); // our currently active window: none
			let mut ev = xlib::XEvent { client_message: msg };
			xlib::XSendEvent(
				dpy,
				root,
				xlib::False,
				xlib::SubstructureRedirectMask | xlib::SubstructureNotifyMask,
				&mut ev,
			);
			xlib::XFlush(dpy);
			xlib::XCloseDisplay(dpy);
		}
	}
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
