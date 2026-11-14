//! macOS capture: the Tauri side of `pulsar-macinput` (a CGEventTap — see that crate's docs
//! for what is captured and when). Same public surface and semantics as the Windows
//! `imp.rs`: armed for the whole native session, inert until engaged (a click on the video,
//! game mode's connect-time engage, a hover with the free cursor), every key and the mouse go
//! to the host while engaged, Ctrl+Shift+M / Q / F11, Ctrl+Alt+Z and 3×RightCtrl behave the
//! same, focus loss releases and every held key/button is flushed on release.
//!
//! Needs the Accessibility permission. Without it `enable` arms nothing: the session keeps its
//! webview input path (the canvas forwards pointer + keys, OS shortcuts stay local), the user
//! gets macOS's own prompt once and a Pulsar notice that opens the right Settings pane.
//!
//! Untested on macOS hardware.

use super::*;
use pulsar_macinput as mi;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tauri::Emitter;
use tokio::sync::mpsc::Sender;

/// Hand an event to the forwarder thread: never blocks (the tap callback must return fast),
/// keeps the order, drops only coalescible motion when the session channel is full — the same
/// discipline as the Windows hook (`imp.rs` `fwd`).
fn fwd(tx: &Sender<InputEvent>, ev: InputEvent) {
	use std::sync::mpsc;
	static Q: OnceLock<Mutex<mpsc::Sender<(Sender<InputEvent>, InputEvent)>>> = OnceLock::new();
	let q = Q.get_or_init(|| {
		let (qtx, qrx) = mpsc::channel::<(Sender<InputEvent>, InputEvent)>();
		std::thread::Builder::new()
			.name("macinput-fwd".into())
			.spawn(move || {
				for (tx, ev) in qrx {
					if crate::util::is_coalescible_input(&ev) {
						let _ = tx.try_send(ev);
					} else if tx.blocking_send(ev).is_err() {
						tracing::warn!("mac input forwarder: session channel closed");
					}
				}
			})
			.expect("spawn mac input forwarder");
		Mutex::new(qtx)
	});
	let _ = q.lock().unwrap_or_else(|e| e.into_inner()).send((tx.clone(), ev));
}

/// The capture's output: input events to the armed session, edges as the app's Tauri events.
struct TauriSink {
	app: AppHandle,
	tx: Sender<InputEvent>,
}

impl mi::Sink for TauriSink {
	fn send(&self, ev: mi::Out) {
		let ev = match ev {
			mi::Out::Key { code, down } => InputEvent::Key { code, down },
			mi::Out::Motion { x, y } => InputEvent::PointerMotion { x, y },
			mi::Out::Relative { dx, dy } => InputEvent::PointerRelative { dx, dy },
			mi::Out::Button { button, down } => InputEvent::PointerButton { button, down },
			mi::Out::Scroll { dx, dy } => InputEvent::Scroll { dx, dy },
		};
		fwd(&self.tx, ev);
	}

	fn signal(&self, s: mi::Signal) {
		let _ = match s {
			mi::Signal::Engaged => self.app.emit("kbd-engaged", ()),
			mi::Signal::Released => self.app.emit("kbd-released", ()),
			mi::Signal::Detach => self.app.emit("kbd-detach", ()),
			mi::Signal::OverlayToggle(kept) => self.app.emit("overlay-toggle", kept),
			mi::Signal::Leave => self.app.emit("kbd-leave", ()),
			mi::Signal::FullscreenToggle => self.app.emit("fullscreen-toggle", ()),
		};
	}

	fn scale(&self, dx: f64, dy: f64) -> (f64, f64) {
		super::scale_relative(dx, dy)
	}
}

/// Ask for the permission at most once per run (macOS shows its own prompt only once per
/// install anyway) and show Pulsar's notice with the way to grant it.
fn permission_missing(app: &AppHandle) {
	static ASKED: AtomicBool = AtomicBool::new(false);
	if mi::trusted() || ASKED.swap(true, Ordering::SeqCst) {
		return;
	}
	tracing::warn!("macOS: Accessibility permission missing — OS shortcuts stay local (webview input path)");
	let _ = mi::request_trust();
	crate::toast::kbd_permission(app);
}

pub fn enable(app: AppHandle, tx: Sender<InputEvent>, mouse: bool, id: u64, _start_suspended: bool) {
	let arm = mi::Arm {
		id,
		mouse,
		remote: super::SESSION_REMOTE.load(Ordering::SeqCst),
		free_cursor: super::pointer_free(),
	};
	if !mi::enable(Arc::new(TauriSink { app: app.clone(), tx }), arm) {
		permission_missing(&app);
	}
}

pub fn disable() {
	mi::disable();
}

/// Session menu open/close: the overlay is drawn by the renderer over the live video, there is
/// no grab to suspend — only the free-cursor handover reads it (as on Windows).
pub fn overlay_suspend(suspend: bool) {
	mi::overlay_open(suspend);
}

pub fn set_focused(focused: bool) {
	mi::set_focused(focused);
}

pub fn engage(_app: &AppHandle) {
	mi::engage();
}

pub fn engage_auto(_app: &AppHandle) {
	mi::engage_auto();
}

pub fn release(_app: &AppHandle) {
	mi::release("overlay");
}

pub fn arm_kiosk() {
	mi::arm_kiosk();
}

pub fn set_stream_stalled(stalled: bool) {
	mi::set_stream_stalled(stalled);
}

pub fn web_key(evdev: u32, down: bool) {
	mi::web_key(evdev, down);
}

pub fn set_video_frac(x0: f32, y0: f32, x1: f32, y1: f32) {
	mi::set_video_frac(x0, y0, x1, y1);
}

pub fn set_ovbtn_frac(f: Option<(f32, f32, f32, f32)>) {
	mi::set_ovbtn_frac(f);
}

pub fn set_pointer_lock(id: u64, on: bool) {
	mi::set_pointer_lock(id, on);
}

pub fn pointer_free_changed(on: bool) {
	mi::set_pointer_free(on);
}

pub fn is_engaged() -> bool {
	mi::is_engaged()
}

pub fn hover_edge() -> bool {
	mi::hover_edge()
}

/// Pulsar or one of its renderer children is the frontmost app — read fresh at most every
/// 100 ms (the controller gate asks on every pad report).
pub fn app_focused() -> bool {
	static CACHE: Mutex<Option<(std::time::Instant, bool)>> = Mutex::new(None);
	let mut c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
	if let Some((at, v)) = *c {
		if at.elapsed() < std::time::Duration::from_millis(100) {
			return v;
		}
	}
	let v = mi::app_focused_now();
	*c = Some((std::time::Instant::now(), v));
	v
}

/// The session area of play `id` (CSS px = points, relative to its window): the confine /
/// normalization rect while no renderer window of ours is on screen. Zero area = hidden.
pub fn set_view_rect(app: &AppHandle, id: u64, x: i32, y: i32, w: i32, h: i32) {
	if w <= 0 || h <= 0 {
		mi::set_view(id, None);
		return;
	}
	let window = crate::tabwin::window_for_play(app, id)
		.and_then(|win| win.ns_window().ok())
		.and_then(mi::ns_window_number);
	mi::set_view(
		id,
		window.map(|window| mi::View { window, rect: (x as f64, y as f64, w as f64, h as f64) }),
	);
}

/// The OS-level capture can run (Accessibility granted): the session UI then takes control
/// through it instead of the webview canvas path.
pub fn os_capture_available() -> bool {
	mi::trusted()
}

/// "Grant the permission": macOS's prompt (first time) and the Accessibility settings pane.
pub fn request_permission() {
	if !mi::request_trust() {
		mi::open_accessibility_settings();
	}
}
