//! Picture-in-picture (issue #18): a session shown in a small frameless window of its own
//! (`pip-<n>`), while its tab STAYS in the window it came from as a placeholder ("this session
//! is in a small window — Bring back").
//!
//! * The session itself moves like a torn-off tab (tabwin.rs): its record is handed to the new
//!   window (`tab_take_handoff`), its native video is re-parented there (`move_video`). The PiP
//!   window runs the session (input, menu, side channels); the placeholder only points at it.
//! * Bringing it back (`pip_return`, from the PiP window): the video goes to the placeholder's
//!   window, which swaps the placeholder for the live record (`pip-return`), and the PiP window
//!   closes. The placeholder's "Bring back" asks the PiP window to do that (`pip-restore`).
//! * The placeholder can move between windows like any tab: `rehome` keeps the video in the PiP
//!   window and only changes where it returns to. A torn-off window closing hands its
//!   placeholders to the main window (`home_closed`); the PiP stays open.
//! * Ending the session anywhere ends it everywhere: both windows hear `play-ended`.
//! * Settings → General → "Small window" (`set_pip_enabled`): off refuses new PiPs and brings
//!   every open one back.
//! * Size: 16:9 around 480×270 logical, bottom-right of the source window's monitor (stacked
//!   above other PiPs), or where the last PiP was left. Hover growth (`pip_resize`) keeps the
//!   corner the window is snapped to and grows towards the screen's middle.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;

use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

const PREFIX: &str = "pip-";
/// Default PiP size (logical px) and its gap to the screen edges / other PiPs.
const W: f64 = 480.0;
const H: f64 = 270.0;
const MARGIN: f64 = 16.0;
/// Smallest PiP (logical px).
pub(crate) const MIN_W: f64 = 240.0;
pub(crate) const MIN_H: f64 = 135.0;

struct Pip {
	/// The window holding the placeholder tab (where "Bring back" returns the session).
	home: String,
	play: u64,
	/// The placeholder record (handed to the main window when `home` closes).
	placeholder: serde_json::Value,
}

static PIPS: Mutex<Option<HashMap<String, Pip>>> = Mutex::new(None);
static ENABLED: AtomicBool = AtomicBool::new(true);
static NEXT: AtomicU32 = AtomicU32::new(1);

pub(crate) fn is_pip_window(label: &str) -> bool {
	label.starts_with(PREFIX)
}

/// Is play `id` shown in a PiP window? (Hover never engages input there: a click does.)
pub(crate) fn is_pip_play(id: u64) -> bool {
	is_pip_window(&crate::tabwin::label_for_play(id))
}

fn pip_of_play(id: u64) -> Option<String> {
	PIPS.lock()
		.unwrap()
		.as_ref()
		.and_then(|m| m.iter().find(|(_, p)| p.play == id).map(|(l, _)| l.clone()))
}

/// The placeholder of play `id` moved to window `to`: the PiP now returns there. True = `id`
/// is a PiP session (its video must stay where it is).
pub(crate) fn rehome(id: u64, to: &str) -> bool {
	let mut g = PIPS.lock().unwrap();
	let Some(p) = g.as_mut().and_then(|m| m.values_mut().find(|p| p.play == id)) else {
		return false;
	};
	tracing::info!(id, from = %p.home, %to, "pip placeholder moved");
	p.home = to.to_string();
	true
}

/// Window `label` is closing: the placeholders it holds go to the main window.
pub(crate) fn home_closed(app: &AppHandle, label: &str) {
	let moved: Vec<serde_json::Value> = {
		let mut g = PIPS.lock().unwrap();
		let Some(m) = g.as_mut() else { return };
		m.values_mut()
			.filter(|p| p.home == label)
			.map(|p| {
				p.home = crate::tabwin::MAIN.to_string();
				p.placeholder.clone()
			})
			.collect()
	};
	if !moved.is_empty() {
		tracing::info!(%label, n = moved.len(), "pip placeholders → main window");
		let main = crate::tabwin::MAIN.to_string();
		let _ = app.emit_to(&main, "tab-adopt", (main.clone(), moved));
	}
}

/// The renderer line for play `id`: `pipwin <available 0|1> <on 0|1>` (the Pulsar menu row).
pub(crate) fn render_line(id: u64) -> String {
	format!("pipwin {} {}", u8::from(ENABLED.load(Ordering::SeqCst)), u8::from(is_pip_play(id)))
}

/// Tell play `id`'s renderer its PiP state (after it moved in / out, or the setting changed).
fn push_line(app: &AppHandle, id: u64) {
	let state = app.state::<crate::state::AppState>();
	let si = state.plays.lock().unwrap().get(&id).map(|p| p.render_stdin.clone());
	if let Some(si) = si {
		use std::io::Write as _;
		if let Some(si) = si.lock().unwrap().as_mut() {
			let _ = writeln!(si, "{}", render_line(id));
		}
	}
}

/// A PiP window is gone: forget it.
pub(crate) fn forget(label: &str) {
	if let Some(m) = PIPS.lock().unwrap().as_mut() {
		m.remove(label);
	}
	if let Some(m) = GEOM.lock().unwrap().as_mut() {
		m.remove(label);
	}
}

/// The work area (physical LTRB) of the monitor holding `w`, else the primary one.
fn work_area(app: &AppHandle, w: Option<&WebviewWindow>) -> Option<(f64, f64, f64, f64, f64)> {
	let m = w
		.and_then(|w| w.current_monitor().ok().flatten())
		.or_else(|| app.primary_monitor().ok().flatten())?;
	let wa = m.work_area();
	let (l, t) = (wa.position.x as f64, wa.position.y as f64);
	Some((l, t, l + wa.size.width as f64, t + wa.size.height as f64, m.scale_factor()))
}

/// Physical rects (LTRB) of the open PiP windows.
fn open_rects(app: &AppHandle) -> Vec<(f64, f64, f64, f64)> {
	let labels: Vec<String> = PIPS.lock().unwrap().as_ref().map(|m| m.keys().cloned().collect()).unwrap_or_default();
	labels
		.iter()
		.filter_map(|l| app.get_webview_window(l))
		.filter_map(|w| {
			let (p, s) = (w.outer_position().ok()?, w.outer_size().ok()?);
			Some((p.x as f64, p.y as f64, p.x as f64 + s.width as f64, p.y as f64 + s.height as f64))
		})
		.collect()
}

fn overlaps(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)) -> bool {
	a.0 < b.2 && b.0 < a.2 && a.1 < b.3 && b.1 < a.3
}

/// The hover watch runs (one task while any PiP is open).
static WATCHING: AtomicBool = AtomicBool::new(false);

/// Every 100 ms: is the pointer over each PiP window? Edges go to that window as `pip-hover`
/// (label, on). Polled because the native video child takes the mouse on Windows (the webview
/// under it never hears the pointer), and the same on every OS keeps one behaviour.
fn watch_hover(app: &AppHandle) {
	if WATCHING.swap(true, Ordering::SeqCst) {
		return;
	}
	let app = app.clone();
	tauri::async_runtime::spawn(async move {
		let mut over: HashMap<String, bool> = HashMap::new();
		loop {
			tokio::time::sleep(std::time::Duration::from_millis(100)).await;
			let labels: Vec<String> =
				PIPS.lock().unwrap().as_ref().map(|m| m.keys().cloned().collect()).unwrap_or_default();
			if labels.is_empty() {
				WATCHING.store(false, Ordering::SeqCst);
				// A PiP opened between the read and the store would have found it running.
				if PIPS.lock().unwrap().as_ref().is_some_and(|m| !m.is_empty())
					&& !WATCHING.swap(true, Ordering::SeqCst)
				{
					continue;
				}
				return;
			}
			over.retain(|l, _| labels.contains(l));
			let Ok(c) = app.cursor_position() else { continue };
			for label in labels {
				let Some(w) = app.get_webview_window(&label) else { continue };
				let (Ok(p), Ok(sz)) = (w.outer_position(), w.outer_size()) else { continue };
				let inside = !w.is_minimized().unwrap_or(false)
					&& c.x >= p.x as f64
					&& c.y >= p.y as f64
					&& c.x < p.x as f64 + sz.width as f64
					&& c.y < p.y as f64 + sz.height as f64;
				if over.get(&label) != Some(&inside) {
					over.insert(label.clone(), inside);
					let _ = app.emit_to(&label, "pip-hover", (label.clone(), inside));
				}
			}
		}
	});
}

/// Where a new PiP goes (logical x, y, w, h): `last` (where the user left the previous one) or
/// the bottom-right corner, stepped up past PiPs already there.
fn place(app: &AppHandle, src: &WebviewWindow, last: Option<[f64; 4]>) -> (f64, f64, f64, f64) {
	let Some((l, t, r, b, s)) = work_area(app, Some(src)) else {
		return (100.0, 100.0, W, H);
	};
	let (mut x, mut y, w, h) = match last {
		Some([x, y, w, h]) if w >= MIN_W && h >= MIN_H => (x * s, y * s, w * s, h * s),
		_ => (r - (W + MARGIN) * s, b - (H + MARGIN) * s, W * s, H * s),
	};
	// Keep it on this screen.
	x = x.clamp(l, (r - w).max(l));
	y = y.clamp(t, (b - h).max(t));
	let others = open_rects(app);
	for _ in 0..8 {
		let me = (x, y, x + w, y + h);
		if !others.iter().any(|o| overlaps(me, *o)) {
			break;
		}
		// Step towards the screen's middle (up from a bottom PiP, down from a top one).
		let up = y + h / 2.0 > (t + b) / 2.0;
		y = if up { y - h - MARGIN * s } else { y + h + MARGIN * s }.clamp(t, (b - h).max(t));
	}
	(x / s, y / s, w / s, h / s)
}

/// Open play `play` in a new PiP window. `session`: its tab record (handed to the new window);
/// `placeholder`: the record the calling window keeps (stored for `home_closed`); `last`: where
/// the previous PiP was left (logical x, y, w, h). Returns the PiP window's label.
#[tauri::command]
pub(crate) async fn pip_open(
	app: AppHandle,
	window: WebviewWindow,
	session: serde_json::Value,
	placeholder: serde_json::Value,
	play: u64,
	last: Option<[f64; 4]>,
) -> Result<String, String> {
	if !ENABLED.load(Ordering::SeqCst) {
		return Err("the small window is turned off in Settings".into());
	}
	if let Some(open) = pip_of_play(play) {
		return Ok(open);
	}
	let n = NEXT.fetch_add(1, Ordering::Relaxed);
	let label = format!("{PREFIX}{n}");
	let (x, y, w, h) = place(&app, &window, last);
	tracing::info!(%label, play, from = window.label(), x, y, w, h, "pip open");
	crate::tabwin::handoff_insert(&label, serde_json::Value::Array(vec![session]));
	PIPS.lock().unwrap().get_or_insert_with(HashMap::new).insert(
		label.clone(),
		Pip { home: window.label().to_string(), play, placeholder },
	);
	let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
	let handle = app.clone();
	let lbl = label.clone();
	let posted = app.run_on_main_thread(move || {
		let built = WebviewWindowBuilder::new(&handle, &lbl, WebviewUrl::App("index.html".into()))
			.initialization_script(format!("window.__TABWIN__={n};window.__PIP__=true;"))
			.background_color(tauri::window::Color(0, 0, 0, 255))
			.title("Pulsar")
			.inner_size(w, h)
			.min_inner_size(MIN_W, MIN_H)
			.position(x, y)
			.decorations(false)
			.resizable(true)
			.maximizable(false)
			// Starts unpinned and passive: it must not take the focus (and with it the input)
			// from what the user is doing.
			.always_on_top(false)
			.focused(false)
			.build();
		let _ = tx.send(built.map(|_| ()).map_err(|e| e.to_string()));
	});
	if posted.is_err() {
		forget(&label);
		return Err("main thread unavailable".into());
	}
	if let Err(e) = rx.await.map_err(|e| e.to_string()).and_then(|r| r) {
		forget(&label);
		return Err(e);
	}
	// Our record of where it is (see GEOM): the builder's position, physical — then settled into
	// its corner with the margins measured on the client (the builder places the outer frame).
	let scale = window.scale_factor().unwrap_or(1.0);
	set_geom(&label, Geom { pos: (x * scale, y * scale), asked: (w, h) });
	if let Some(pw) = app.get_webview_window(&label) {
		let _ = snap_now(&app, &pw);
		// GTK builds a frameless window ~37 px taller than asked and ignores a resize until the
		// window is on screen: give it its asked size (and corner) again once it is.
		let (app2, pw2) = (app.clone(), pw.clone());
		tauri::async_runtime::spawn(async move {
			tokio::time::sleep(std::time::Duration::from_millis(250)).await;
			let p = pw2.clone();
			let _ = pw2.run_on_main_thread(move || {
				let _ = p.set_size(tauri::LogicalSize::new(w, h));
			});
			// The move in its own step: GTK drops one that races the resize.
			tokio::time::sleep(std::time::Duration::from_millis(150)).await;
			let (a, p) = (app2.clone(), pw2.clone());
			let _ = pw2.run_on_main_thread(move || {
				let _ = snap_now(&a, &p);
			});
		});
	}
	crate::tabwin::move_video(&app, play, &label);
	push_line(&app, play);
	crate::winalpha::apply(&app, &label);
	watch_hover(&app);
	Ok(label)
}

/// The PiP window gives its session back (`session`: the live record): the video goes to the
/// placeholder's window, which swaps the placeholder for it, and the PiP window closes.
#[tauri::command]
pub(crate) fn pip_return(app: AppHandle, window: WebviewWindow, session: serde_json::Value, play: u64) -> Result<(), String> {
	let label = window.label().to_string();
	let home = PIPS
		.lock()
		.unwrap()
		.as_mut()
		.and_then(|m| m.remove(&label))
		.map(|p| p.home)
		.filter(|h| app.get_webview_window(h).is_some())
		.unwrap_or_else(|| crate::tabwin::MAIN.to_string());
	tracing::info!(%label, play, %home, "pip return");
	crate::tabwin::move_video(&app, play, &home);
	push_line(&app, play);
	crate::winalpha::apply(&app, &home);
	let _ = app.emit_to(&home, "pip-return", (home.clone(), play, session));
	if let Some(w) = app.get_webview_window(&home) {
		let _ = w.unminimize();
		let _ = w.show();
		let _ = w.set_focus();
	}
	crate::winpin::forget(&label);
	let win = window.clone();
	tauri::async_runtime::spawn(async move {
		// Let the re-parent land before the window (and anything still under it) goes.
		tokio::time::sleep(std::time::Duration::from_millis(150)).await;
		let _ = win.destroy();
	});
	Ok(())
}

/// The placeholder's "Bring back": ask the PiP window showing play `play` to return it.
#[tauri::command]
pub(crate) fn pip_restore(app: AppHandle, play: u64) -> Result<(), String> {
	let Some(label) = pip_of_play(play) else {
		return Err(format!("play {play} is not in a small window"));
	};
	let _ = app.emit_to(&label, "pip-restore", label.clone());
	Ok(())
}

/// Show the PiP window of play `play` (the placeholder was clicked).
#[tauri::command]
pub(crate) fn pip_focus(app: AppHandle, play: u64) {
	if let Some(w) = pip_of_play(play).and_then(|l| app.get_webview_window(&l)) {
		let _ = w.unminimize();
		let _ = w.show();
	}
}

/// A PiP window is asked to close. Its session still live (Alt+F4, taskbar): bring it back
/// rather than end it, and keep the window until it has — returns true (prevent the close).
/// Session ended or already returned: let it close.
pub(crate) fn close_requested(app: &AppHandle, label: &str) -> bool {
	let play = PIPS.lock().unwrap().as_ref().and_then(|m| m.get(label).map(|p| p.play));
	let Some(play) = play else { return false };
	let live = app.state::<crate::state::AppState>().plays.lock().unwrap().contains_key(&play);
	if !live {
		forget(label);
		return false;
	}
	let _ = app.emit_to(label, "pip-restore", label.to_string());
	true
}

/// The PiP window's session ended (it has nothing left to show): the window goes.
#[tauri::command]
pub(crate) fn pip_close(window: WebviewWindow) {
	if !is_pip_window(window.label()) {
		return;
	}
	forget(window.label());
	crate::winpin::forget(window.label());
	let _ = window.destroy();
}

/// Settings → General → "Small window". Off brings every open PiP back.
#[tauri::command]
pub(crate) fn set_pip_enabled(app: AppHandle, enabled: bool) {
	let was = ENABLED.swap(enabled, Ordering::SeqCst);
	if was == enabled {
		return;
	}
	// Every renderer learns whether the menu row exists.
	let plays: Vec<u64> = app.state::<crate::state::AppState>().plays.lock().unwrap().keys().copied().collect();
	for id in plays {
		push_line(&app, id);
	}
	if enabled {
		return;
	}
	let labels: Vec<String> = PIPS.lock().unwrap().as_ref().map(|m| m.keys().cloned().collect()).unwrap_or_default();
	for l in labels {
		let _ = app.emit_to(&l, "pip-restore", l.clone());
	}
}

/// Which corner of its screen the PiP keeps when it grows: the one its middle is closest to.
fn anchor(rect: (f64, f64, f64, f64), wa: (f64, f64, f64, f64)) -> (bool, bool) {
	let (cx, cy) = ((rect.0 + rect.2) / 2.0, (rect.1 + rect.3) / 2.0);
	(cx > (wa.0 + wa.2) / 2.0, cy > (wa.1 + wa.3) / 2.0)
}

/// New physical rect (x, y, w, h) for `rect` resized to `w`×`h`, keeping its anchored corner and
/// staying inside `wa`.
fn resized(rect: (f64, f64, f64, f64), wa: (f64, f64, f64, f64), w: f64, h: f64) -> (f64, f64, f64, f64) {
	let (right, bottom) = anchor(rect, wa);
	let (w, h) = (w.min(wa.2 - wa.0), h.min(wa.3 - wa.1));
	let x = if right { rect.2 - w } else { rect.0 };
	let y = if bottom { rect.3 - h } else { rect.1 };
	(x.clamp(wa.0, wa.2 - w), y.clamp(wa.1, wa.3 - h), w, h)
}

/// Per PiP window, OUR record of where it is: the rect we last put it at, in `set_position`'s own
/// terms (physical: Windows/macOS = the outer frame's origin; Linux/GTK = the client origin), and
/// the client size we asked for (logical). Hover growth and its way back are laid out from this
/// record, never read back from the window: on GTK the read-backs lag and disagree (the client
/// reports ~37 px taller right after the window is built, `outer_position` sits above the point
/// `set_position` moves), which walked and grew the PiP on every round trip (Orange Pi). Only a
/// move or resize by the USER is read back (`pip_snap`, `pip_size(user)`).
static GEOM: Mutex<Option<HashMap<String, Geom>>> = Mutex::new(None);

#[derive(Clone, Copy, Debug)]
struct Geom {
	/// `set_position` origin (physical).
	pos: (f64, f64),
	/// Client size asked for (logical).
	asked: (f64, f64),
}

fn geom(label: &str) -> Option<Geom> {
	GEOM.lock().unwrap().as_ref()?.get(label).copied()
}

fn set_geom(label: &str, g: Geom) {
	GEOM.lock().unwrap().get_or_insert_with(HashMap::new).insert(label.to_string(), g);
}

/// What `set_position` moves, read from the window (only after the user moved it).
fn set_position_origin(win: &WebviewWindow) -> Option<(f64, f64)> {
	#[cfg(all(unix, not(target_os = "macos")))]
	let p = win.inner_position().ok()?;
	#[cfg(not(all(unix, not(target_os = "macos"))))]
	let p = win.outer_position().ok()?;
	Some((p.x as f64, p.y as f64))
}

/// From what `set_position` moves to the visible client area's origin (physical): Windows' outer
/// frame starts at the invisible resize border left of the client (and none above it); nothing
/// where `set_position` moves the client origin itself (GTK). Margins and corners are measured on
/// the client, so the PiP sits the same distance from the screen edges on every OS.
fn client_off(win: &WebviewWindow) -> (f64, f64) {
	#[cfg(all(unix, not(target_os = "macos")))]
	{
		let _ = win;
		(0.0, 0.0)
	}
	#[cfg(not(all(unix, not(target_os = "macos"))))]
	{
		match (win.inner_position(), win.outer_position()) {
			(Ok(i), Ok(o)) => ((i.x - o.x) as f64, (i.y - o.y) as f64),
			_ => (0.0, 0.0),
		}
	}
}

/// The PiP's own size (logical, as asked): the frontend's base for hover growth. `user`: the user
/// resized it by hand — take its client size now as the asked one.
#[tauri::command]
pub(crate) fn pip_size(window: WebviewWindow, user: bool) -> Option<[f64; 2]> {
	let label = window.label().to_string();
	if user {
		let s = window.scale_factor().ok()?;
		let i = window.inner_size().ok()?;
		let pos = set_position_origin(&window)?;
		set_geom(&label, Geom { pos, asked: (i.width as f64 / s, i.height as f64 / s) });
	}
	geom(&label).map(|g| [g.asked.0, g.asked.1])
}

/// Resize the calling PiP window to `w`×`h` (logical client size) — hover growth and its way
/// back — keeping the screen corner it sits in.
#[tauri::command]
pub(crate) fn pip_resize(app: AppHandle, window: WebviewWindow, w: f64, h: f64) -> Result<(), String> {
	if !is_pip_window(window.label()) {
		return Err("not a small window".into());
	}
	let win = window.clone();
	let _ = window.run_on_main_thread(move || {
		let label = win.label().to_string();
		let Ok(s) = win.scale_factor() else { return };
		let Some(g) = geom(&label).or_else(|| {
			let i = win.inner_size().ok()?;
			Some(Geom { pos: set_position_origin(&win)?, asked: (i.width as f64 / s, i.height as f64 / s) })
		}) else {
			return;
		};
		let Some((l, t, r, b, _)) = work_area(&app, Some(&win)) else { return };
		let off = client_off(&win);
		let (w, h) = (w.max(MIN_W), h.max(MIN_H));
		// Laid out on the client rect (our record), converted back to `set_position` terms.
		let (cx, cy) = (g.pos.0 + off.0, g.pos.1 + off.1);
		let rect = (cx, cy, cx + g.asked.0 * s, cy + g.asked.1 * s);
		let (nx, ny, _, _) = resized(rect, (l, t, r, b), w * s, h * s);
		let (x, y) = (nx - off.0, ny - off.1);
		tracing::debug!(?g, ?off, to = ?(x, y, w, h), "pip resize");
		set_geom(&label, Geom { pos: (x, y), asked: (w, h) });
		let to = tauri::PhysicalPosition::new(x.round() as i32, y.round() as i32);
		let _ = win.set_position(to);
		let _ = win.set_size(tauri::LogicalSize::new(w, h));
		// GTK drops a move that races the resize (the window then shrinks from its top-left):
		// put it where it belongs once more after the resize landed. Harmless elsewhere.
		let again = win.clone();
		tauri::async_runtime::spawn(async move {
			tokio::time::sleep(std::time::Duration::from_millis(120)).await;
			let w2 = again.clone();
			let _ = again.run_on_main_thread(move || {
				let _ = w2.set_position(to);
			});
		});
	});
	Ok(())
}

/// Snap window `window` into a corner it is near (within `SNAP` of two edges, margins measured on
/// the client area) and record where it is. Returns where it ended up (logical x, y in
/// `set_position` terms; w, h = the asked client size).
fn snap_now(app: &AppHandle, window: &WebviewWindow) -> Option<[f64; 4]> {
	const SNAP: f64 = 48.0;
	let label = window.label().to_string();
	let (l, t, r, b, s) = work_area(app, Some(window))?;
	let (px, py) = set_position_origin(window)?;
	let asked = geom(&label).map(|g| g.asked).or_else(|| {
		let i = window.inner_size().ok()?;
		Some((i.width as f64 / s, i.height as f64 / s))
	})?;
	let off = client_off(window);
	let (w, h) = (asked.0 * s, asked.1 * s);
	let (mut x, mut y) = (px + off.0, py + off.1);
	let (x0, y0) = (x, y);
	let m = MARGIN * s;
	let near = |a: f64, b: f64| (a - b).abs() <= SNAP * s;
	if near(x, l + m) || x < l + m {
		x = l + m;
	} else if near(x + w, r - m) || x + w > r - m {
		x = r - m - w;
	}
	if near(y, t + m) || y < t + m {
		y = t + m;
	} else if near(y + h, b - m) || y + h > b - m {
		y = b - m - h;
	}
	let (x, y) = (x - off.0, y - off.1);
	if (x, y) != (x0 - off.0, y0 - off.1) {
		let _ = window.set_position(tauri::PhysicalPosition::new(x.round() as i32, y.round() as i32));
	}
	set_geom(&label, Geom { pos: (x, y), asked });
	Some([x / s, y / s, asked.0, asked.1])
}

/// A PiP was dragged and let go: it snaps into a near corner; the result is where the next PiP
/// opens.
#[tauri::command]
pub(crate) fn pip_snap(app: AppHandle, window: WebviewWindow) -> Option<[f64; 4]> {
	snap_now(&app, &window)
}

#[cfg(test)]
mod tests {
	use super::*;

	const WA: (f64, f64, f64, f64) = (0.0, 0.0, 1920.0, 1040.0);

	#[test]
	fn growth_keeps_the_corner_it_sits_in() {
		// Bottom-right PiP grows up and left.
		let br = (1424.0, 754.0, 1904.0, 1024.0);
		assert_eq!(resized(br, WA, 720.0, 405.0), (1184.0, 619.0, 720.0, 405.0));
		// Top-left PiP grows down and right.
		let tl = (16.0, 16.0, 496.0, 286.0);
		assert_eq!(resized(tl, WA, 720.0, 405.0), (16.0, 16.0, 720.0, 405.0));
		// Never past the screen.
		let (x, y, w, h) = resized(br, WA, 4000.0, 3000.0);
		assert!(x >= 0.0 && y >= 0.0 && x + w <= 1920.0 && y + h <= 1040.0);
	}

	#[test]
	fn rehome_only_moves_pip_sessions() {
		PIPS.lock().unwrap().get_or_insert_with(HashMap::new).insert(
			"pip-900".into(),
			Pip { home: "main".into(), play: 9001, placeholder: serde_json::Value::Null },
		);
		assert!(rehome(9001, "tabwin-3"));
		assert!(!rehome(9002, "tabwin-3"), "an ordinary tab moves its video");
		assert_eq!(PIPS.lock().unwrap().as_ref().unwrap()["pip-900"].home, "tabwin-3");
		forget("pip-900");
		assert!(pip_of_play(9001).is_none());
	}
}
