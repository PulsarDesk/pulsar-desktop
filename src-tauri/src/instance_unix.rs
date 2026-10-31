//! Unix (Linux/macOS) analog of the Windows show-event: re-launching Pulsar while it is
//! already running (often hidden in the tray) must bring that instance's window to the
//! front instead of silently exiting — which looked like a launch that hangs.
//!
//! The first instance (it holds the single-instance flock) listens on a per-user Unix
//! socket; a second launch connects, sends one line — its XDG activation token / startup
//! id, if the launcher gave it one — and exits. The token lets the compositor (KWin,
//! Mutter: xdg-activation on Wayland) accept the raise instead of only flashing the
//! taskbar entry.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::Duration;
use tauri::{AppHandle, Manager};

fn socket_path() -> PathBuf {
	let uid = unsafe { libc::getuid() };
	let dir = std::env::var_os("XDG_RUNTIME_DIR")
		.map(PathBuf::from)
		.filter(|p| p.is_dir())
		.unwrap_or_else(std::env::temp_dir);
	dir.join(format!("pulsar-{uid}.sock"))
}

/// The token a desktop launcher handed this (second) process for focusing a window.
fn activation_token() -> String {
	std::env::var("XDG_ACTIVATION_TOKEN")
		.or_else(|_| std::env::var("DESKTOP_STARTUP_ID"))
		.unwrap_or_default()
}

/// Second instance: ask the running one to surface its window. Best effort, never blocks
/// for long (a wedged first instance must not hang this exit path).
pub(crate) fn signal_existing_instance() {
	let Ok(mut s) = UnixStream::connect(socket_path()) else {
		tracing::warn!("couldn't reach the running Pulsar to surface it");
		return;
	};
	let _ = s.set_write_timeout(Some(Duration::from_secs(1)));
	let _ = writeln!(s, "show {}", activation_token());
}

/// First instance: listen for "show" requests. Only called while holding the
/// single-instance lock, so a leftover socket file is always stale and safe to replace.
pub(crate) fn spawn_show_listener(app: AppHandle) {
	let path = socket_path();
	let _ = std::fs::remove_file(&path);
	let listener = match UnixListener::bind(&path) {
		Ok(l) => l,
		Err(e) => {
			tracing::warn!(path = %path.display(), "single-instance socket bind failed: {e}");
			return;
		}
	};
	std::thread::spawn(move || {
		for conn in listener.incoming() {
			let Ok(conn) = conn else { continue };
			let _ = conn.set_read_timeout(Some(Duration::from_secs(1)));
			let mut line = String::new();
			let _ = BufReader::new(conn).read_line(&mut line);
			let Some(rest) = line.trim().strip_prefix("show") else { continue };
			let token = rest.trim().to_string();
			tracing::info!("another launch asked to surface the window");
			let h = app.clone();
			let _ = app.run_on_main_thread(move || surface(&h, &token));
		}
	});
}

/// Show + raise the main window — or the splash, while the app is still booting (the main
/// window appears on its own once the UI is ready).
fn surface(app: &AppHandle, token: &str) {
	if !crate::splash::is_done() {
		crate::splash::present(app, token);
		return;
	}
	let Some(w) = app.get_webview_window("main") else { return };
	#[cfg(target_os = "linux")]
	if !token.is_empty() {
		use gtk::prelude::GtkWindowExt;
		if let Ok(g) = w.gtk_window() {
			g.set_startup_id(token);
		}
	}
	#[cfg(not(target_os = "linux"))]
	let _ = token;
	let _ = w.show();
	let _ = w.unminimize();
	let _ = w.set_focus();
}
