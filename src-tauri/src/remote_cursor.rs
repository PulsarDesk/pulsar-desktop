//! Host: a connected client's pointer, drawn as a labelled "ghost" cursor with the client's
//! name, while that client can NOT control this machine — the host user made it view-only
//! ("Sadece izleme") or is using their own mouse (`host::takeover`). The client can still
//! point at things ("click here") without moving the real cursor. Remote-desktop sessions only.
//!
//! The position comes from the client's held-back pointer input: absolute moves set it,
//! relative ones (the native renderer / evdev capture) move it from where it was. It shows
//! for [`SHOW_FOR`] after the last such input. Off when the host turned it off
//! (`Config::show_remote_cursor`) or the client did (`DataMsg::CursorLabel(false)`).
//!
//! Drawn in a small always-on-top, click-through window. Linux draws it natively (GTK popup +
//! cairo, like the splash — a transparent WebKitGTK window with compositing off can't keep its
//! edges clear), and it ends up in the captured video too: neither x11grab nor the portal can
//! leave a window out. Windows/macOS use a transparent webview (`static/remote-cursor.html`)
//! excluded from capture.

use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};

use pulsar_core::service::InputEvent;

use crate::state::{AppState, ConnMode};

/// How long the ghost stays after the client's last pointer input.
const SHOW_FOR: Duration = Duration::from_secs(3);
const TICK: Duration = Duration::from_millis(16);

/// A part of the virtual desktop (all monitors' bounding box) as 0..1 fractions: x, y, w, h.
pub(crate) type Region = (f64, f64, f64, f64);
const WHOLE: Region = (0.0, 0.0, 1.0, 1.0);

struct Ghost {
	sid: u64,
	/// Position on the streamed screen, 0..1.
	x: f64,
	y: f64,
	/// Where the streamed screen sits in the virtual desktop.
	region: Region,
	at: Instant,
}

static GHOST: Mutex<Option<Ghost>> = Mutex::new(None);
/// The streamed screen's size in pixels, from the drawing side (turns relative moves into 0..1).
static SCREEN_PX: Mutex<(f64, f64)> = Mutex::new((1920.0, 1080.0));

/// A client's pointer input that was NOT injected (view-only / host takeover): move its ghost.
pub(crate) fn held_back(sid: u64, ev: &InputEvent, region: Option<Region>) {
	let mut g = GHOST.lock().unwrap();
	let fresh = g.as_ref().is_none_or(|g| g.sid != sid || g.at.elapsed() > SHOW_FOR);
	let gh = g.get_or_insert(Ghost { sid, x: 0.5, y: 0.5, region: WHOLE, at: Instant::now() });
	if fresh {
		(gh.sid, gh.x, gh.y) = (sid, 0.5, 0.5);
	}
	match *ev {
		InputEvent::PointerMotion { x, y } => (gh.x, gh.y) = (x, y),
		InputEvent::PointerRelative { dx, dy } => {
			let (w, h) = *SCREEN_PX.lock().unwrap();
			gh.x += dx / w.max(1.0);
			gh.y += dy / h.max(1.0);
		}
		_ => {}
	}
	(gh.x, gh.y) = (gh.x.clamp(0.0, 1.0), gh.y.clamp(0.0, 1.0));
	gh.region = region.unwrap_or(WHOLE);
	gh.at = Instant::now();
}

/// The client's input applies again (control returned): drop its ghost at once.
pub(crate) fn applied(sid: u64) {
	let mut g = GHOST.lock().unwrap();
	if g.as_ref().is_some_and(|g| g.sid == sid) {
		*g = None;
	}
}

/// What to draw now: the name and the position as 0..1 of the virtual desktop; `None` = hide.
fn wanted(app: &AppHandle) -> Option<(String, f64, f64, Region)> {
	let (sid, x, y, region) = {
		let g = GHOST.lock().unwrap();
		let g = g.as_ref().filter(|g| g.at.elapsed() <= SHOW_FOR)?;
		(g.sid, g.x, g.y, g.region)
	};
	let st = app.state::<AppState>();
	if !st.config.lock().unwrap().show_remote_cursor {
		return None;
	}
	let peer = {
		let active = st.active.lock().unwrap();
		let ci = active.get(&sid)?;
		if ci.mode == ConnMode::Game || !ci.cursor_label {
			return None;
		}
		ci.peer.clone()
	};
	let name = st.peer_meta.lock().unwrap().get(&peer).and_then(|m| m.0.clone());
	let name = name
		.filter(|n| !n.trim().is_empty())
		.or_else(|| st.peer_ids.lock().unwrap().get(&peer).cloned())
		.unwrap_or(peer);
	Some((name, region.0 + x * region.2, region.1 + y * region.3, region))
}

/// Map a virtual-desktop fraction onto pixels of the bounding box `(x, y, w, h)`, and record
/// the streamed screen's pixel size for relative moves.
fn place(vx: f64, vy: f64, region: Region, bbox: (f64, f64, f64, f64)) -> (f64, f64) {
	*SCREEN_PX.lock().unwrap() = (region.2 * bbox.2, region.3 * bbox.3);
	(bbox.0 + vx * bbox.2, bbox.1 + vy * bbox.3)
}

/// Start the ghost's driver (once, from `setup`).
pub(crate) fn start(app: &AppHandle) {
	#[cfg(target_os = "linux")]
	{
		let h = app.clone();
		let _ = app.run_on_main_thread(move || native::start(h));
	}
	#[cfg(not(target_os = "linux"))]
	{
		let h = app.clone();
		std::thread::Builder::new()
			.name("pulsar-remote-cursor".into())
			.spawn(move || webview::run(h))
			.ok();
	}
}

#[cfg(not(target_os = "linux"))]
mod webview {
	use super::*;
	use tauri::window::Color;
	use tauri::{PhysicalPosition, WebviewUrl, WebviewWindowBuilder};

	const LABEL: &str = "remote-cursor";
	/// The window is a fixed box; the page draws the arrow at its top-left (the hotspot) and
	/// the name pill beside it, the rest is clear.
	const W: f64 = 300.0;
	const H: f64 = 48.0;

	/// All monitors' bounding box, physical pixels.
	fn bbox(app: &AppHandle) -> Option<(f64, f64, f64, f64)> {
		let ms = app.available_monitors().ok()?;
		let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
		for m in &ms {
			let (p, s) = (m.position(), m.size());
			x0 = x0.min(p.x as f64);
			y0 = y0.min(p.y as f64);
			x1 = x1.max(p.x as f64 + s.width as f64);
			y1 = y1.max(p.y as f64 + s.height as f64);
		}
		(x1 > x0 && y1 > y0).then_some((x0, y0, x1 - x0, y1 - y0))
	}

	pub(super) fn run(app: AppHandle) {
		let mut shown: Option<String> = None;
		let mut last_pos: Option<(i32, i32)> = None;
		let mut screens = None;
		let mut checked = Instant::now() - Duration::from_secs(10);
		loop {
			let want = wanted(&app);
			match (&want, app.get_webview_window(LABEL)) {
				(None, Some(w)) => {
					if shown.take().is_some() {
						let _ = w.hide();
					}
				}
				(None, None) => {}
				(Some((name, vx, vy, region)), win) => {
					let w = match win {
						Some(w) => w,
						None => match build(&app) {
							Some(w) => w,
							None => {
								std::thread::sleep(Duration::from_secs(1));
								continue;
							}
						},
					};
					// The monitor layout rarely changes; re-read it every few seconds.
					if checked.elapsed() > Duration::from_secs(3) {
						screens = bbox(&app);
						checked = Instant::now();
					}
					if let Some(b) = screens {
						let (px, py) = place(*vx, *vy, *region, b);
						let pos = (px.round() as i32, py.round() as i32);
						if last_pos != Some(pos) {
							last_pos = Some(pos);
							let _ = w.set_position(PhysicalPosition::new(pos.0, pos.1));
						}
					}
					if shown.as_deref() != Some(name.as_str()) {
						let js = format!(
							"window.setName && window.setName({})",
							serde_json::to_string(name).unwrap_or_default()
						);
						let _ = w.eval(&js);
						if shown.is_none() {
							let _ = w.show();
						}
						shown = Some(name.clone());
					}
				}
			}
			std::thread::sleep(if shown.is_some() { TICK } else { Duration::from_millis(100) });
		}
	}

	fn build(app: &AppHandle) -> Option<tauri::WebviewWindow> {
		let built = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("remote-cursor.html".into()))
			.title("Pulsar")
			.inner_size(W, H)
			.resizable(false)
			.decorations(false)
			.shadow(false)
			.transparent(true)
			.background_color(Color(0, 0, 0, 0))
			.always_on_top(true)
			.skip_taskbar(true)
			.focused(false)
			.focusable(false)
			// Kept out of the captured video: the client draws its own pointer already.
			.content_protected(true)
			.visible(false)
			.build();
		match built {
			Ok(w) => {
				let _ = w.set_ignore_cursor_events(true);
				Some(w)
			}
			Err(e) => {
				tracing::warn!("remote cursor window failed: {e}");
				None
			}
		}
	}
}

#[cfg(target_os = "linux")]
mod native {
	use super::*;
	use gtk::glib;
	use gtk::pango;
	use gtk::prelude::*;
	use gtk::{cairo, gdk};
	use std::cell::RefCell;
	use std::f64::consts::PI;
	use std::rc::Rc;

	/// The name pill's top-left, relative to the arrow's tip (the hotspot = window origin).
	const PILL: (f64, f64) = (14.0, 18.0);
	const PAD_X: f64 = 10.0;
	const PAD_Y: f64 = 5.0;
	const FONT_PX: f64 = 13.0;
	/// The Pulsar accent (the splash's `--accent`); white text/outline on it reads in both themes.
	const ACCENT: (f64, f64, f64) = (0x4e as f64 / 255.0, 0x60 as f64 / 255.0, 0xe8 as f64 / 255.0);
	/// A standard arrow cursor outline, tip at (0, 0).
	const ARROW: [(f64, f64); 7] =
		[(1.0, 1.0), (1.0, 18.0), (5.5, 14.0), (8.5, 21.0), (11.0, 20.0), (8.0, 13.5), (13.5, 13.5)];

	struct Pointer {
		win: gtk::Window,
		area: gtk::DrawingArea,
		name: Rc<RefCell<String>>,
		shown: bool,
		pos: (i32, i32),
	}

	fn layout(widget: &gtk::DrawingArea, s: &str) -> pango::Layout {
		let l = widget.create_pango_layout(Some(s));
		let mut fd = pango::FontDescription::new();
		fd.set_family("Hanken Grotesk, Sans");
		fd.set_weight(pango::Weight::Semibold);
		fd.set_absolute_size(FONT_PX * pango::SCALE as f64);
		l.set_font_description(Some(&fd));
		l
	}

	fn show_layout(cr: &cairo::Context, layout: &pango::Layout) {
		#[link(name = "pangocairo-1.0")]
		extern "C" {
			fn pango_cairo_show_layout(cr: *mut cairo::ffi::cairo_t, layout: *mut pango::ffi::PangoLayout);
		}
		use gtk::glib::translate::ToGlibPtr;
		unsafe { pango_cairo_show_layout(cr.to_raw_none(), layout.to_glib_none().0) };
	}

	fn draw(a: &gtk::DrawingArea, cr: &cairo::Context, name: &str) {
		cr.set_operator(cairo::Operator::Source);
		cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
		let _ = cr.paint();
		cr.set_operator(cairo::Operator::Over);
		// The arrow: accent fill, white outline.
		cr.move_to(ARROW[0].0, ARROW[0].1);
		for &(x, y) in &ARROW[1..] {
			cr.line_to(x, y);
		}
		cr.close_path();
		cr.set_source_rgba(ACCENT.0, ACCENT.1, ACCENT.2, 1.0);
		let _ = cr.fill_preserve();
		cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
		cr.set_line_width(1.5);
		let _ = cr.stroke();
		// The name pill.
		let l = layout(a, name);
		let (tw, th) = l.pixel_size();
		let (x, y) = PILL;
		let (w, h) = (tw as f64 + PAD_X * 2.0, th as f64 + PAD_Y * 2.0);
		let r = h / 2.0;
		cr.new_sub_path();
		cr.arc(x + w - r, y + r, r, -PI / 2.0, PI / 2.0);
		cr.arc(x + r, y + r, r, PI / 2.0, 1.5 * PI);
		cr.close_path();
		cr.set_source_rgba(ACCENT.0, ACCENT.1, ACCENT.2, 0.96);
		let _ = cr.fill();
		cr.move_to(x + PAD_X, y + PAD_Y);
		cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
		show_layout(cr, &l);
	}

	fn build() -> Pointer {
		// A popup (override-redirect on X11): no decorations, no focus, placed exactly.
		let win = gtk::Window::new(gtk::WindowType::Popup);
		win.set_title("Pulsar");
		win.set_decorated(false);
		win.set_resizable(false);
		win.set_accept_focus(false);
		win.set_skip_taskbar_hint(true);
		win.set_skip_pager_hint(true);
		win.set_keep_above(true);
		win.set_type_hint(gdk::WindowTypeHint::Tooltip);
		if let Some(v) = WidgetExt::screen(&win).and_then(|s| s.rgba_visual()) {
			win.set_visual(Some(&v));
		}
		win.set_app_paintable(true);
		let area = gtk::DrawingArea::new();
		let name = Rc::new(RefCell::new(String::new()));
		let n2 = name.clone();
		area.connect_draw(move |a, cr| {
			draw(a, cr, &n2.borrow());
			glib::Propagation::Stop
		});
		win.add(&area);
		// Click-through: an empty input shape once realized.
		win.connect_realize(|w| {
			if let Some(gw) = w.window() {
				gw.input_shape_combine_region(&cairo::Region::create(), 0, 0);
			}
		});
		Pointer { win, area, name, shown: false, pos: (i32::MIN, i32::MIN) }
	}

	/// All monitors' bounding box (GDK logical pixels).
	fn bbox() -> Option<(f64, f64, f64, f64)> {
		let d = gdk::Display::default()?;
		let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
		for i in 0..d.n_monitors() {
			let g = d.monitor(i)?.geometry();
			x0 = x0.min(g.x());
			y0 = y0.min(g.y());
			x1 = x1.max(g.x() + g.width());
			y1 = y1.max(g.y() + g.height());
		}
		(x1 > x0 && y1 > y0).then_some((x0 as f64, y0 as f64, (x1 - x0) as f64, (y1 - y0) as f64))
	}

	pub(super) fn start(app: AppHandle) {
		let mut ptr: Option<Pointer> = None;
		let mut idle = 0u32;
		glib::timeout_add_local(TICK, move || {
			// Idle: re-check ~10×/s instead of every frame.
			if ptr.as_ref().is_none_or(|p| !p.shown) {
				idle += 1;
				if idle % 6 != 0 {
					return glib::ControlFlow::Continue;
				}
			}
			match wanted(&app) {
				None => {
					if let Some(p) = ptr.as_mut().filter(|p| p.shown) {
						p.win.hide();
						p.shown = false;
					}
				}
				Some((name, vx, vy, region)) => {
					let p = ptr.get_or_insert_with(build);
					if *p.name.borrow() != name {
						let (tw, th) = layout(&p.area, &name).pixel_size();
						let w = (PILL.0 + PAD_X * 2.0) as i32 + tw + 2;
						let h = (PILL.1 + PAD_Y * 2.0) as i32 + th + 2;
						p.win.set_size_request(w, h);
						p.win.resize(w, h);
						*p.name.borrow_mut() = name;
						p.area.queue_draw();
					}
					if let Some(b) = bbox() {
						let (x, y) = place(vx, vy, region, b);
						let pos = (x.round() as i32, y.round() as i32);
						if pos != p.pos {
							p.pos = pos;
							p.win.move_(pos.0, pos.1);
						}
					}
					if !p.shown {
						p.win.show_all();
						p.shown = true;
					}
				}
			}
			glib::ControlFlow::Continue
		});
	}
}
