//! Pulsar's own toast notices (not the OS notification centre): a small borderless,
//! always-on-top window in the bottom-right corner of the primary monitor's work area that
//! stacks short notices — today "a file arrived" (the peer sent one over the session). The
//! window (label `toast`) loads `index.html` with `window.__TOAST__` set, so the Svelte app
//! renders only the toast stack; it is sized to its content, created WITHOUT activation (it
//! never steals focus from the game / remote session) and closed when the last notice expires.
//! Clicking a file notice opens its folder with the file selected.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

const LABEL: &str = "toast";
/// Logical size of one notice + the window chrome around the stack (must match
/// ToastWindow.svelte's layout: 64 px cards, 8 px gaps, 10 px padding).
const WIDTH: f64 = 340.0;
const ITEM_H: f64 = 64.0;
const GAP: f64 = 8.0;
const PAD: f64 = 10.0;
/// Distance from the work-area corner (logical px).
const MARGIN: f64 = 6.0;
/// Older notices make room for new ones beyond this many.
const MAX_VISIBLE: usize = 4;
const LIFETIME: Duration = Duration::from_secs(7);

/// One notice. `kind` picks the title/icon in the UI: `file-in` (a file arrived),
/// `pad-off` (a client controller got no virtual pad here — `name` holds the cause); the
/// file manager has no remote delete/rename yet, so `file-del` / `file-mod` are reserved.
#[derive(Clone, Serialize)]
pub(crate) struct Toast {
	id: u64,
	kind: &'static str,
	/// File name as saved (dedup suffix included).
	name: String,
	/// Who did it (the peer's display name or id), empty when unknown.
	peer: String,
	/// Full path, for "open the folder with the file selected".
	path: Option<String>,
}

static TOASTS: Mutex<Vec<Toast>> = Mutex::new(Vec::new());
static NEXT: AtomicU64 = AtomicU64::new(1);

/// A file from `peer` was saved at `path`.
pub(crate) fn file_received(app: &AppHandle, peer: &str, path: &Path) {
	let name = path
		.file_name()
		.map(|n| n.to_string_lossy().into_owned())
		.unwrap_or_default();
	push(app, "file-in", name, peer.to_string(), Some(path.to_path_buf()));
}

/// A connected client's controller could not get a virtual pad on this host; `text` is the
/// localized cause (crate::pad_notice::host_text).
pub(crate) fn pad_unavailable(app: &AppHandle, text: String) {
	push(app, "pad-off", text, String::new(), None);
}

fn push(app: &AppHandle, kind: &'static str, name: String, peer: String, path: Option<PathBuf>) {
	let id = NEXT.fetch_add(1, Ordering::Relaxed);
	{
		let mut g = TOASTS.lock().unwrap_or_else(|e| e.into_inner());
		g.push(Toast {
			id,
			kind,
			name,
			peer,
			path: path.map(|p| p.to_string_lossy().into_owned()),
		});
		let over = g.len().saturating_sub(MAX_VISIBLE);
		g.drain(..over);
	}
	sync(app);
	let app = app.clone();
	tauri::async_runtime::spawn(async move {
		tokio::time::sleep(LIFETIME).await;
		dismiss(&app, id);
	});
}

fn dismiss(app: &AppHandle, id: u64) {
	let changed = {
		let mut g = TOASTS.lock().unwrap_or_else(|e| e.into_inner());
		let before = g.len();
		g.retain(|t| t.id != id);
		g.len() != before
	};
	if changed {
		sync(app);
	}
}

/// Make the window match the list: create it (unactivated) for the first notice, resize +
/// re-anchor it to the corner as the stack grows/shrinks, close it when empty. Window
/// operations run on the main thread (off-thread ones silently no-op on Windows).
fn sync(app: &AppHandle) {
	let list = TOASTS.lock().unwrap_or_else(|e| e.into_inner()).clone();
	let handle = app.clone();
	let _ = app.run_on_main_thread(move || {
		let existing = handle.get_webview_window(LABEL);
		if list.is_empty() {
			if let Some(w) = existing {
				let _ = w.close();
			}
			return;
		}
		let n = list.len() as f64;
		let (w, h) = (WIDTH + 2.0 * PAD, n * ITEM_H + (n - 1.0) * GAP + 2.0 * PAD);
		// Bottom-right of the primary monitor's work area (above the taskbar), in logical px.
		let (x, y) = match handle.primary_monitor().ok().flatten() {
			Some(m) => {
				let s = m.scale_factor();
				let wa = m.work_area();
				let right = (wa.position.x as f64 + wa.size.width as f64) / s;
				let bottom = (wa.position.y as f64 + wa.size.height as f64) / s;
				(right - w - MARGIN, bottom - h - MARGIN)
			}
			None => (100.0, 100.0),
		};
		match existing {
			Some(win) => {
				let _ = win.set_size(tauri::LogicalSize::new(w, h));
				let _ = win.set_position(tauri::LogicalPosition::new(x, y));
				let _ = win.set_always_on_top(true);
				let _ = handle.emit_to(LABEL, "toasts", &list);
			}
			// Locked session: no new toast window (ui_start.rs).
			None if !crate::ui_start::windows_allowed() => {}
			None => {
				let init = serde_json::to_string(&list).unwrap_or_else(|_| "[]".into());
				let built = WebviewWindowBuilder::new(&handle, LABEL, WebviewUrl::App("index.html".into()))
					.initialization_script(format!("window.__TOAST__={init};"))
					.title("Pulsar")
					.inner_size(w, h)
					.position(x, y)
					.resizable(false)
					.decorations(false)
					.shadow(false)
					.transparent(true)
					.background_color(tauri::window::Color(0, 0, 0, 0))
					.always_on_top(true)
					.skip_taskbar(true)
					// Shown without activation: a notice must never steal the keyboard from the
					// game / remote session the user is in.
					.focused(false)
					.visible(true)
					.build();
				if let Err(e) = built {
					tracing::warn!(%e, "toast window failed to open");
				}
			}
		}
	});
}

/// The current notices (the toast window reads them on mount, then follows `toasts`).
#[tauri::command]
pub(crate) fn toast_list() -> Vec<Toast> {
	TOASTS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Dismiss a notice (its × or after a click).
#[tauri::command]
pub(crate) fn toast_close(app: AppHandle, id: u64) {
	dismiss(&app, id);
}

/// A click on a notice: a file notice opens its folder with the file selected; any notice
/// is dismissed.
#[tauri::command]
pub(crate) fn toast_open(app: AppHandle, id: u64) {
	let path = TOASTS
		.lock()
		.unwrap_or_else(|e| e.into_inner())
		.iter()
		.find(|t| t.id == id)
		.and_then(|t| t.path.clone());
	if let Some(p) = path {
		reveal_in_folder(Path::new(&p));
	}
	dismiss(&app, id);
}

/// Open the platform file manager on `path`'s folder with `path` selected (Explorer
/// `/select,`, Finder `open -R`, the freedesktop FileManager1 `ShowItems` call — with a
/// plain "open the folder" fallback where that isn't available).
pub(crate) fn reveal_in_folder(path: &Path) {
	#[cfg(windows)]
	{
		use std::os::windows::process::CommandExt;
		// Explorer parses `/select,"<path>"` itself; let Rust's own quoting stay out of it.
		let mut cmd = std::process::Command::new("explorer");
		cmd.raw_arg(format!("/select,\"{}\"", path.display()));
		let _ = cmd.spawn();
	}
	#[cfg(target_os = "macos")]
	{
		let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
	}
	#[cfg(all(unix, not(target_os = "macos")))]
	{
		let uri = file_uri(path);
		let shown = std::process::Command::new("gdbus")
			.args([
				"call",
				"--session",
				"--dest",
				"org.freedesktop.FileManager1",
				"--object-path",
				"/org/freedesktop/FileManager1",
				"--method",
				"org.freedesktop.FileManager1.ShowItems",
			])
			.arg(format!("['{uri}']"))
			.arg("")
			.status()
			.map(|s| s.success())
			.unwrap_or(false);
		if !shown {
			if let Some(dir) = path.parent() {
				let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
			}
		}
	}
}

/// `file://` URI with every byte outside the unreserved set percent-encoded.
#[cfg(all(unix, not(target_os = "macos")))]
fn file_uri(path: &Path) -> String {
	use std::os::unix::ffi::OsStrExt;
	let mut s = String::from("file://");
	for &b in path.as_os_str().as_bytes() {
		if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
			s.push(b as char);
		} else {
			s.push_str(&format!("%{b:02X}"));
		}
	}
	s
}
