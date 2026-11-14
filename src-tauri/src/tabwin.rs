//! Detachable session tabs (Chrome-style): a session tab dragged out of the tab strip opens
//! in its own window (`tabwin-<n>`), and a tab dragged onto another Pulsar window's strip
//! moves there. The session itself (play id, stream, input, renderer) never restarts — only
//! the UI that shows it and the window its native video is embedded in change.
//!
//! * The tab window loads the same `index.html` with `window.__TABWIN__=<n>` injected: the
//!   app renders the session shell only (no Home, no relay bootstrap) and picks its sessions
//!   up with `tab_take_handoff`. Later arrivals come as a `tab-adopt` event.
//! * Each play's video follows its window (`move_video`): Windows re-parents the renderer's
//!   child HWND (`reparent` on its stdin), Linux re-parents the GDK container the renderer
//!   is embedded in, macOS moves the renderer's panel over the other window (`reparent`
//!   with its window number).
//! * Every window reports where its tab strip is (`tab_strip_rect`); a drop is matched
//!   against those rects at the cursor (`tab_drop_target`).
//! * Closing a tab window ends its sessions, like closing those tabs — their video is moved
//!   back under the main window first, so the window's teardown never takes a renderer
//!   child (Windows) or a reusable container (Linux) with it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

pub(crate) const MAIN: &str = "main";
const PREFIX: &str = "tabwin-";

/// Play id → label of the window showing it. Absent = the main window.
static PLAY_WIN: Mutex<Option<HashMap<u64, String>>> = Mutex::new(None);
/// Sessions (the frontend's tab records, opaque here) waiting for a new tab window to load.
static HANDOFF: Mutex<Option<HashMap<String, serde_json::Value>>> = Mutex::new(None);
/// Each window's tab strip, in its client area (CSS px): x, y, w, h.
static STRIPS: Mutex<Option<HashMap<String, (f64, f64, f64, f64)>>> = Mutex::new(None);
/// The window keyboard/mouse capture belongs to: the Pulsar session window focused last.
/// Focus moving to another app (or to the renderer child) doesn't change it.
static INPUT_WIN: Mutex<String> = Mutex::new(String::new());
static NEXT: AtomicU32 = AtomicU32::new(1);

pub(crate) fn is_tab_window(label: &str) -> bool {
	label.starts_with(PREFIX)
}

/// Main window or a tab window — the windows that show sessions.
pub(crate) fn is_session_window(label: &str) -> bool {
	label == MAIN || is_tab_window(label)
}

/// Label of the window showing play `id` (the main window unless it was moved).
pub(crate) fn label_for_play(id: u64) -> String {
	PLAY_WIN
		.lock()
		.unwrap()
		.as_ref()
		.and_then(|m| m.get(&id).cloned())
		.unwrap_or_else(|| MAIN.to_string())
}

/// The window showing play `id`; the main window when that one is gone.
pub(crate) fn window_for_play(app: &AppHandle, id: u64) -> Option<WebviewWindow> {
	app.get_webview_window(&label_for_play(id))
		.or_else(|| app.get_webview_window(MAIN))
}

fn plays_in(label: &str) -> Vec<u64> {
	PLAY_WIN
		.lock()
		.unwrap()
		.as_ref()
		.map(|m| m.iter().filter(|(_, l)| l.as_str() == label).map(|(id, _)| *id).collect())
		.unwrap_or_default()
}

/// Plays whose stream is up (`play-ready` sent). A window that adopts a tab after that event
/// went out (its page still loading) asks `tab_play_ready` instead of waiting for it.
static READY: Mutex<Vec<u64>> = Mutex::new(Vec::new());

pub(crate) fn mark_ready(id: u64) {
	let mut g = READY.lock().unwrap();
	if !g.contains(&id) {
		g.push(id);
	}
}

#[tauri::command]
pub(crate) fn tab_play_ready(id: u64) -> bool {
	READY.lock().unwrap().contains(&id)
}

/// A play is stopping: shown in a tab window, its video moves back under the main window
/// (hidden) — a Linux resident container is reused by the next session and must not die
/// with that window. Then it's forgotten.
pub(crate) fn play_stopped(app: &AppHandle, id: u64) {
	if label_for_play(id) != MAIN {
		move_video(app, id, MAIN);
	}
	if let Some(m) = PLAY_WIN.lock().unwrap().as_mut() {
		m.remove(&id);
	}
	READY.lock().unwrap().retain(|p| *p != id);
}

/// A session window got the focus: it now owns keyboard/mouse capture. Every window hears
/// it (`input-window`) so only the sessions of THAT window arm the capture.
pub(crate) fn note_focus(app: &AppHandle, label: &str) {
	if !is_session_window(label) {
		return;
	}
	{
		let mut g = INPUT_WIN.lock().unwrap();
		if g.as_str() == label {
			return;
		}
		*g = label.to_string();
	}
	let _ = app.emit("input-window", label);
}

/// Move play `id`'s native video into window `label`. Its new place is hidden until that
/// window's session screen reports a rect (`native_view_rect`).
fn move_video(app: &AppHandle, id: u64, label: &str) {
	{
		let mut g = PLAY_WIN.lock().unwrap();
		let m = g.get_or_insert_with(HashMap::new);
		if label == MAIN {
			m.remove(&id);
		} else {
			m.insert(id, label.to_string());
		}
	}
	#[cfg(any(windows, target_os = "macos"))]
	{
		// Windows: the HWND the render child moves into. macOS: the window number the
		// renderer's panel moves over.
		let Some(hwnd) = app.get_webview_window(label).and_then(|w| crate::process::render_parent(&w)) else {
			return;
		};
		let state = app.state::<crate::state::AppState>();
		let stdin = state.plays.lock().unwrap().get(&id).map(|p| {
			// A respawn (codec / monitor switch) must not replay the OLD window's rect.
			p.render_seed.lock().unwrap().viewrect = None;
			p.render_stdin.clone()
		});
		if let Some(stdin) = stdin {
			use std::io::Write as _;
			if let Some(si) = stdin.lock().unwrap().as_mut() {
				let _ = writeln!(si, "reparent 0x{hwnd:x}");
			}
		}
	}
	#[cfg(all(unix, not(target_os = "macos")))]
	crate::render::reparent_native_container(app, id, label.to_string());
}

/// A tab window is closing: its sessions end (like closing their tabs). Their video goes
/// back under the main window first (hidden), then the window is destroyed.
pub(crate) fn close_tab_window(app: &AppHandle, window: &WebviewWindow) {
	let label = window.label().to_string();
	let plays = plays_in(&label);
	for id in &plays {
		move_video(app, *id, MAIN);
	}
	if let Some(m) = STRIPS.lock().unwrap().as_mut() {
		m.remove(&label);
	}
	if let Some(m) = HANDOFF.lock().unwrap().as_mut() {
		m.remove(&label);
	}
	{
		let mut g = INPUT_WIN.lock().unwrap();
		if g.as_str() == label {
			*g = MAIN.to_string();
			let _ = app.emit("input-window", MAIN);
		}
	}
	let app2 = app.clone();
	let win = window.clone();
	tauri::async_runtime::spawn(async move {
		for id in plays {
			let _ = crate::play::stop_stream(app2.clone(), app2.state(), id).await;
		}
		// Let the re-parent land before the window (and anything still under it) goes.
		tokio::time::sleep(std::time::Duration::from_millis(150)).await;
		let _ = win.destroy();
	});
}

/// The cursor in screen px: the OS cursor, else the frontend's estimate (`fallback`).
fn cursor(app: &AppHandle, fallback: (f64, f64)) -> (f64, f64) {
	match app.cursor_position() {
		Ok(p) => (p.x, p.y),
		Err(_) => fallback,
	}
}

/// The session window whose tab strip is under screen point `(x, y)`, other than `except`.
/// The session window whose TAB AREA (top chrome + strip) is under `(x, y)` — the drop rule for
/// a whole tab window dragged by its title bar: anywhere else it is just moved, not merged.
fn strip_at(app: &AppHandle, x: f64, y: f64, except: &str) -> Option<String> {
	let strips = STRIPS.lock().unwrap().clone().unwrap_or_default();
	for (label, (sx, sy, sw, sh)) in strips {
		if label == except {
			continue;
		}
		let Some(w) = app.get_webview_window(&label) else { continue };
		if !w.is_visible().unwrap_or(false) || w.is_minimized().unwrap_or(false) {
			continue;
		}
		let Ok(pos) = w.inner_position() else { continue };
		let scale = w.scale_factor().unwrap_or(1.0);
		let (l, t) = (pos.x as f64 + sx * scale, pos.y as f64 + sy * scale);
		let (r, b) = (l + sw * scale, t + (sh + 24.0) * scale);
		if x >= l && x < r && y >= t - 8.0 * scale && y < b {
			return Some(label);
		}
	}
	None
}

/// The session window under screen point `(x, y)` (anywhere in it, like Chrome's drop onto a
/// window), other than `except`. Only windows that accept tabs right now count: the ones that
/// report a tab area (not fullscreen, not split — see `tab_strip_rect`).
fn window_at(app: &AppHandle, x: f64, y: f64, except: &str) -> Option<String> {
	let strips = STRIPS.lock().unwrap().clone().unwrap_or_default();
	// Windows: only the TOPMOST window under the cursor counts — a tab window in front of the
	// main window must not light up the main window behind it (nor another app's window).
	#[cfg(windows)]
	{
		let top = topmost_at(x as i32, y as i32)?;
		for label in strips.keys() {
			if label == except {
				continue;
			}
			let Some(w) = app.get_webview_window(label) else { continue };
			if w.hwnd().ok().map(|h| h.0 as isize) == Some(top) && !w.is_minimized().unwrap_or(false) {
				return Some(label.clone());
			}
		}
		None
	}
	#[cfg(not(windows))]
	{
	for label in strips.keys() {
		if label == except {
			continue;
		}
		let Some(w) = app.get_webview_window(label) else { continue };
		if !w.is_visible().unwrap_or(false) || w.is_minimized().unwrap_or(false) {
			continue;
		}
		let (Ok(pos), Ok(size)) = (w.outer_position(), w.outer_size()) else { continue };
		let (l, t) = (pos.x as f64, pos.y as f64);
		let (r, b) = (l + size.width as f64, t + size.height as f64);
		if x >= l && x < r && y >= t && y < b {
			return Some(label.clone());
		}
	}
	None
	}
}

/// The top-level window in front at screen point `(x, y)`: walks the z-order (EnumWindows is
/// front-to-back) for the first visible, uncloaked window containing the point — skipping the
/// drag ghost, which sits under the cursor by design.
#[cfg(windows)]
fn topmost_at(x: i32, y: i32) -> Option<isize> {
	use windows_sys::core::BOOL;
	use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT};
	use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
	use windows_sys::Win32::UI::WindowsAndMessaging::{
		EnumWindows, GetWindowLongW, GetWindowRect, IsIconic, IsWindowVisible, GWL_EXSTYLE, WS_EX_TRANSPARENT,
	};
	struct Q {
		x: i32,
		y: i32,
		found: isize,
	}
	unsafe extern "system" fn each(h: HWND, lp: LPARAM) -> BOOL {
		let q = &mut *(lp as *mut Q);
		if IsWindowVisible(h) == 0 || IsIconic(h) != 0 {
			return 1;
		}
		// Click-through windows (the drag ghost) are not "under" the cursor.
		if (GetWindowLongW(h, GWL_EXSTYLE) as u32) & WS_EX_TRANSPARENT != 0 {
			return 1;
		}
		let mut cloaked: u32 = 0;
		DwmGetWindowAttribute(h, DWMWA_CLOAKED as u32, &mut cloaked as *mut u32 as *mut _, 4);
		if cloaked != 0 {
			return 1;
		}
		let mut r: RECT = std::mem::zeroed();
		if GetWindowRect(h, &mut r) == 0 {
			return 1;
		}
		if q.x >= r.left && q.x < r.right && q.y >= r.top && q.y < r.bottom {
			q.found = h as isize;
			return 0; // stop: front-most hit
		}
		1
	}
	let mut q = Q { x, y, found: 0 };
	unsafe {
		EnumWindows(Some(each), &mut q as *mut Q as LPARAM);
	}
	(q.found != 0).then_some(q.found)
}

/// The calling window's tab strip moved / resized (client CSS px). Zero size = no strip
/// (no tabs, fullscreen, split): nothing can be dropped there.
#[tauri::command]
pub(crate) fn tab_strip_rect(window: WebviewWindow, x: f64, y: f64, w: f64, h: f64) {
	let mut g = STRIPS.lock().unwrap();
	let m = g.get_or_insert_with(HashMap::new);
	if w > 0.0 && h > 0.0 {
		m.insert(window.label().to_string(), (x, y, w, h));
	} else {
		m.remove(window.label());
	}
}

/// Which other window's tab strip a dragged tab is over right now (None = none). `sx`/`sy`:
/// the frontend's screen-px estimate of the cursor, used only when the OS can't tell.
#[tauri::command]
pub(crate) fn tab_drop_target(app: AppHandle, window: WebviewWindow, sx: f64, sy: f64) -> Option<String> {
	let (x, y) = cursor(&app, (sx, sy));
	let to = window_at(&app, x, y, window.label());
	tracing::info!(from = window.label(), x, y, ?to, strips = ?STRIPS.lock().unwrap().as_ref().map(|m| m.len()), "tab drop target");
	to
}

/// Per tab window: bumped on every move, so only the last move's check acts.
static MOVE_GEN: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

/// Is the primary mouse button still down? (Windows; elsewhere unknown → treated as up.)
fn button_down() -> bool {
	#[cfg(windows)]
	unsafe {
		use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
		(GetAsyncKeyState(0x01) as u16 & 0x8000) != 0
	}
	#[cfg(not(windows))]
	false
}

/// A tab window moved (dragged by its title bar or its only tab, like Chrome): once the drag
/// ends with the cursor over another window's tab area, the window's tabs merge into it.
pub(crate) fn note_moved(app: &AppHandle, label: &str) {
	if !is_tab_window(label) {
		return;
	}
	let gen = {
		let mut g = MOVE_GEN.lock().unwrap();
		let e = g.get_or_insert_with(HashMap::new).entry(label.to_string()).or_insert(0);
		*e += 1;
		*e
	};
	let app = app.clone();
	let label = label.to_string();
	tauri::async_runtime::spawn(async move {
		let current = || MOVE_GEN.lock().unwrap().as_ref().and_then(|m| m.get(&label).copied()) == Some(gen);
		tokio::time::sleep(std::time::Duration::from_millis(250)).await;
		// Still held (paused mid-drag): wait for the release, unless it moves again.
		while current() && button_down() {
			tokio::time::sleep(std::time::Duration::from_millis(60)).await;
		}
		if !current() {
			return;
		}
		let Ok(p) = app.cursor_position() else { return };
		let to = strip_at(&app, p.x, p.y, &label);
		tracing::info!(%label, x = p.x, y = p.y, ?to, "tab window drag ended");
		if let Some(to) = to {
			let _ = app.emit_to(&label, "tab-merge-into", (label.clone(), to));
		}
	});
}

const GHOST: &str = "tabghost";
/// Ghost size (logical px) and where the cursor holds it.
const GHOST_W: f64 = 220.0;
const GHOST_H: f64 = 34.0;
const GHOST_GRAB: (f64, f64) = (24.0, 17.0);

fn ghost_pos(app: &AppHandle, scale: f64) -> Option<tauri::PhysicalPosition<i32>> {
	let p = app.cursor_position().ok()?;
	Some(tauri::PhysicalPosition::new(
		(p.x - GHOST_GRAB.0 * scale).round() as i32,
		(p.y - GHOST_GRAB.1 * scale).round() as i32,
	))
}

/// A line from a window's frontend into the app log (tab moves are spread over several
/// windows' JS; this is how their steps show up next to the Rust side).
#[tauri::command]
pub(crate) fn ui_log(window: WebviewWindow, msg: String) {
	tracing::info!(window = window.label(), "ui: {msg}");
}

/// Bumped by every ghost show/hide: a show whose window is built after a later hide (the
/// build runs on the UI thread, a hide can overtake it) destroys it instead of leaving it up.
static GHOST_GEN: AtomicU32 = AtomicU32::new(0);

/// A dragged tab left its window: show a picture of it under the cursor (a tiny borderless,
/// always-on-top window — the webview can't draw outside its own window). Never focused.
#[tauri::command]
pub(crate) async fn tab_ghost_show(app: AppHandle, title: String) -> Result<(), String> {
	let gen = GHOST_GEN.fetch_add(1, Ordering::SeqCst) + 1;
	let handle = app.clone();
	let _ = app.run_on_main_thread(move || {
		if GHOST_GEN.load(Ordering::SeqCst) != gen {
			return; // hidden again before we got here
		}
		if let Some(w) = handle.get_webview_window(GHOST) {
			let _ = w.show();
			return;
		}
		let title_js = serde_json::to_string(&title).unwrap_or_else(|_| "\"\"".into());
		let built = WebviewWindowBuilder::new(&handle, GHOST, WebviewUrl::App("index.html".into()))
			.initialization_script(format!("window.__TABGHOST__={title_js};"))
			.title("Pulsar")
			.inner_size(GHOST_W, GHOST_H)
			.decorations(false)
			.transparent(true)
			.shadow(false)
			.resizable(false)
			.always_on_top(true)
			.skip_taskbar(true)
			.focused(false)
			.build();
		match built {
			Ok(w) => {
				if GHOST_GEN.load(Ordering::SeqCst) != gen {
					let _ = w.destroy();
					return;
				}
				let s = w.scale_factor().unwrap_or(1.0);
				if let Some(p) = ghost_pos(&handle, s) {
					let _ = w.set_position(p);
				}
				let _ = w.set_ignore_cursor_events(true);
			}
			Err(e) => tracing::warn!(%e, "tab ghost window failed"),
		}
	});
	Ok(())
}

/// The window whose tab area the dragged tab is over (it shows "drop here").
static HOVER: Mutex<Option<String>> = Mutex::new(None);

fn set_hover(app: &AppHandle, to: Option<String>) {
	let mut g = HOVER.lock().unwrap();
	if *g == to {
		return;
	}
	// The payload names the window it is for: the frontend's global listeners hear an
	// emit_to meant for another window too.
	if let Some(old) = g.take() {
		let _ = app.emit_to(&old, "tab-drop-hover", (old.clone(), false));
	}
	if let Some(new) = &to {
		let _ = app.emit_to(new, "tab-drop-hover", (new.clone(), true));
	}
	*g = to;
}

/// Keep the ghost under the cursor (called on each pointer move while the tab is outside its
/// window), and light up the tab area of the window it would drop into.
#[tauri::command]
pub(crate) fn tab_ghost_move(app: AppHandle, window: WebviewWindow) {
	if let Some(w) = app.get_webview_window(GHOST) {
		let s = w.scale_factor().unwrap_or(1.0);
		if let Some(p) = ghost_pos(&app, s) {
			let _ = w.set_position(p);
		}
	}
	let target = app.cursor_position().ok().and_then(|p| window_at(&app, p.x, p.y, window.label()));
	set_hover(&app, target);
}

/// The drag ended or came back inside: the ghost (and any "drop here") goes.
#[tauri::command]
pub(crate) fn tab_ghost_hide(app: AppHandle) {
	GHOST_GEN.fetch_add(1, Ordering::SeqCst);
	set_hover(&app, None);
	let handle = app.clone();
	// On the UI thread too, so it runs after any show already queued there.
	let _ = app.run_on_main_thread(move || {
		if let Some(w) = handle.get_webview_window(GHOST) {
			let _ = w.destroy();
		}
	});
}

/// This window's label (the frontend can't know it before `getCurrentWindow` loads).
#[tauri::command]
pub(crate) fn tab_window_label(window: WebviewWindow) -> String {
	window.label().to_string()
}

/// The window input capture currently belongs to.
#[tauri::command]
pub(crate) fn tab_input_window() -> String {
	let g = INPUT_WIN.lock().unwrap();
	if g.is_empty() {
		MAIN.to_string()
	} else {
		g.clone()
	}
}

/// A new tab window picks up the sessions it was opened for (once).
#[tauri::command]
pub(crate) fn tab_take_handoff(window: WebviewWindow) -> serde_json::Value {
	HANDOFF
		.lock()
		.unwrap()
		.as_mut()
		.and_then(|m| m.remove(window.label()))
		.unwrap_or(serde_json::Value::Array(vec![]))
}

/// Tear tabs off into a NEW window under the cursor. `sessions`: the frontend's tab records
/// (handed to the new window as they are); `plays`: their play ids (video to move).
/// `grab`: where on the tab the cursor holds it (client CSS px of the new window), so the
/// window lands with its tab under the cursor. Returns the new window's label.
#[tauri::command]
pub(crate) async fn tab_detach(
	app: AppHandle,
	window: WebviewWindow,
	sessions: serde_json::Value,
	plays: Vec<u64>,
	grab: (f64, f64),
	sx: f64,
	sy: f64,
) -> Result<String, String> {
	let n = NEXT.fetch_add(1, Ordering::Relaxed);
	let label = format!("{PREFIX}{n}");
	HANDOFF
		.lock()
		.unwrap()
		.get_or_insert_with(HashMap::new)
		.insert(label.clone(), sessions);
	let scale = window.scale_factor().unwrap_or(1.0);
	let (w, h) = window
		.inner_size()
		.map(|s| (s.width as f64 / scale, s.height as f64 / scale))
		.unwrap_or((1200.0, 780.0));
	let (w, h) = (w.max(640.0), h.max(400.0));
	let (cx, cy) = cursor(&app, (sx, sy));
	// Logical position of the new window: the grab point sits under the cursor.
	let (x, y) = (cx / scale - grab.0, cy / scale - grab.1);
	let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
	let handle = app.clone();
	let lbl = label.clone();
	let posted = app.run_on_main_thread(move || {
		let built = WebviewWindowBuilder::new(&handle, &lbl, WebviewUrl::App("index.html".into()))
			.initialization_script(format!("window.__TABWIN__={n};"))
			.background_color(crate::splash::window_bg(&handle))
			.title("Pulsar")
			.inner_size(w, h)
			.min_inner_size(640.0, 400.0)
			.position(x, y)
			.decorations(false)
			.resizable(true)
			.maximizable(true)
			.focused(true)
			.build();
		let _ = tx.send(built.map(|_| ()).map_err(|e| e.to_string()));
	});
	if posted.is_err() {
		return Err("main thread unavailable".into());
	}
	rx.await.map_err(|e| e.to_string())??;
	for id in plays {
		move_video(&app, id, &label);
	}
	Ok(label)
}

/// Move tabs into an EXISTING window (`to`): their video moves there and that window adds
/// the tabs (`tab-adopt`) and comes to the front.
#[tauri::command]
pub(crate) fn tab_move(app: AppHandle, to: String, sessions: serde_json::Value, plays: Vec<u64>) -> Result<(), String> {
	let Some(win) = app.get_webview_window(&to).filter(|_| is_session_window(&to)) else {
		return Err(format!("no window {to}"));
	};
	tracing::info!(%to, ?plays, n = sessions.as_array().map(|a| a.len()).unwrap_or(0), "tab move");
	for id in plays {
		move_video(&app, id, &to);
	}
	let _ = app.emit_to(&to, "tab-adopt", (to.clone(), sessions));
	let _ = win.unminimize();
	let _ = win.show();
	let _ = win.set_focus();
	Ok(())
}
