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

/// Resize the calling PiP window to `w`×`h` (logical) — hover growth and its way back — keeping
/// the corner it sits in.
#[tauri::command]
pub(crate) fn pip_resize(app: AppHandle, window: WebviewWindow, w: f64, h: f64) -> Result<(), String> {
	if !is_pip_window(window.label()) {
		return Err("not a small window".into());
	}
	let win = window.clone();
	let _ = window.run_on_main_thread(move || {
		let (Ok(p), Ok(sz), Ok(inner)) = (win.outer_position(), win.outer_size(), win.inner_size()) else { return };
		let Some((l, t, r, b, s)) = work_area(&app, Some(&win)) else { return };
		// The outer rect carries an invisible frame (Windows' resize border) around the client
		// area `set_size` sets: lay out the OUTER rect, size the inner one — or every grow/shrink
		// round trip walks the window by the frame's width.
		let (fw, fh) = (sz.width as f64 - inner.width as f64, sz.height as f64 - inner.height as f64);
		let rect = (p.x as f64, p.y as f64, p.x as f64 + sz.width as f64, p.y as f64 + sz.height as f64);
		let (x, y, nw, nh) = resized(rect, (l, t, r, b), w.max(MIN_W) * s + fw, h.max(MIN_H) * s + fh);
		let _ = win.set_size(tauri::PhysicalSize::new((nw - fw).round() as u32, (nh - fh).round() as u32));
		let _ = win.set_position(tauri::PhysicalPosition::new(x.round() as i32, y.round() as i32));
	});
	Ok(())
}

/// A PiP was dragged and let go: within `SNAP` of a corner, it snaps into it. Returns where it
/// ended up (logical x, y, w, h) so the next PiP can open there.
#[tauri::command]
pub(crate) fn pip_snap(app: AppHandle, window: WebviewWindow) -> Option<[f64; 4]> {
	const SNAP: f64 = 48.0;
	let (p, sz) = (window.outer_position().ok()?, window.outer_size().ok()?);
	let (l, t, r, b, s) = work_area(&app, Some(&window))?;
	let (w, h) = (sz.width as f64, sz.height as f64);
	let (mut x, mut y) = (p.x as f64, p.y as f64);
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
	if (x, y) != (p.x as f64, p.y as f64) {
		let _ = window.set_position(tauri::PhysicalPosition::new(x.round() as i32, y.round() as i32));
	}
	// The next PiP is built with this as its INNER size (and outer position): hand back the
	// client size, or each reopen grows it by the invisible frame.
	let inner = window.inner_size().ok()?;
	Some([x / s, y / s, inner.width as f64 / s, inner.height as f64 / s])
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
