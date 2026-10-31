//! Launch splash (design: claude.ai/design "Pulsar Splash.html"): a frameless 520×340 card,
//! created as the very first thing in `setup` so it is on screen while the main window's
//! (heavy) UI bundle is still loading. The main window starts hidden; once its UI is ready
//! the frontend calls `splash_done`, which shows "Ready", reveals the main window and closes
//! the splash.
//!
//! The status line + bar follow REAL local startup milestones ([`Step`]): start → UI → encoder
//! → ready. Progress only moves forward. Network work (relay registration, the update check)
//! runs in parallel after launch and is deliberately not part of the splash.
//!
//! Linux draws the card natively (GTK + cairo, `native` below): a transparent WebKitGTK
//! window with compositing off (`WEBKIT_DISABLE_COMPOSITING_MODE`, the gray-screen fix) loses
//! its first paint and can't keep its rounded corners clear. Windows/macOS load
//! `static/splash.html` in a transparent webview. Both look the same and can be dragged.
//!
//! The last theme is kept in a one-line hint file (`splash-theme` next to config.json),
//! which the frontend updates.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tauri::window::Color;
use tauri::{AppHandle, Manager};

#[cfg(not(target_os = "linux"))]
const LABEL: &str = "splash";
/// The card (the window is exactly the card; only its rounded corners are see-through).
const CARD_W: f64 = 520.0;
const CARD_H: f64 = 340.0;
const W: f64 = CARD_W;
const H: f64 = CARD_H;

/// Set once the main window has been revealed (by the UI or the fallback).
static DONE: AtomicBool = AtomicBool::new(false);
/// The splash's own language (the UI's), fixed at launch.
static ENGLISH: AtomicBool = AtomicBool::new(false);

/// A splash string in the splash's language.
fn tr(key: &str) -> &'static str {
	crate::i18n::t_in(key, ENGLISH.load(Ordering::Relaxed))
}

/// A startup milestone shown on the splash.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Step {
	Start,
	Ui,
	Encoder,
	Ready,
}

impl Step {
	fn pct(self) -> u32 {
		match self {
			Step::Start => 8,
			Step::Ui => 40,
			Step::Encoder => 70,
			Step::Ready => 100,
		}
	}

	/// Where this step's share of the bar ends (the next step's start).
	fn next_pct(self) -> u32 {
		match self {
			Step::Start => Step::Ui.pct(),
			Step::Ui => Step::Encoder.pct(),
			Step::Encoder | Step::Ready => 100,
		}
	}

	/// Roughly how long this step takes (s): the creep's time constant.
	fn typical_secs(self) -> f64 {
		match self {
			Step::Start => 1.0,
			Step::Ui => 0.6,
			Step::Encoder => 3.0,
			Step::Ready => 0.2,
		}
	}

	/// The bar's goal `secs` into this step: it creeps from this step's start toward
	/// (never onto) the next one's — 90 % of the way, easing off exponentially — so the
	/// bar keeps moving while a step runs, and the real next step carries it on.
	fn creep(self, secs: f64) -> f64 {
		let (a, b) = (self.pct() as f64, self.next_pct() as f64);
		if self == Step::Ready {
			return 1.0;
		}
		(a + (b - a) * 0.9 * (1.0 - (-secs / self.typical_secs()).exp())) / 100.0
	}

	fn label(self) -> &'static str {
		tr(match self {
			Step::Start => "splash.start",
			Step::Ui => "splash.ui",
			Step::Encoder => "splash.encoder",
			Step::Ready => "splash.ready",
		})
	}
}

/// The furthest milestone reached so far.
static STEP: Mutex<Step> = Mutex::new(Step::Start);

/// Design aid: `PULSAR_SPLASH_HOLD=1` keeps the splash up (and on top) forever.
fn hold() -> bool {
	std::env::var_os("PULSAR_SPLASH_HOLD").is_some_and(|v| v == "1")
}

fn hint_path(app: &AppHandle) -> std::path::PathBuf {
	crate::util::config_path(app).with_file_name("splash-theme")
}

/// (dark, game) from the hint file; light remote when it's missing (fresh install = light).
fn read_hint(app: &AppHandle) -> (bool, bool) {
	let s = std::fs::read_to_string(hint_path(app)).unwrap_or_default();
	(s.contains("dark"), s.contains("game"))
}

/// The main window's `--surface` (app.css) as sRGB.
fn surface(dark: bool) -> Color {
	if dark {
		Color(0x16, 0x19, 0x20, 0xff)
	} else {
		Color(0xff, 0xff, 0xff, 0xff)
	}
}

/// Show the splash window right away. Never fails the launch — on any error the main
/// window simply appears when it is ready, as before.
pub(crate) fn show(app: &AppHandle) {
	// The splash text follows the UI's language (from the hint; the UI keeps its own choice,
	// which can differ from config.json's).
	let hint = std::fs::read_to_string(hint_path(app)).unwrap_or_default();
	let english = match hint.split_whitespace().find_map(|w| w.strip_prefix("lang=")) {
		Some(l) => l == "en",
		None => matches!(
			pulsar_core::config::Config::load(crate::util::config_path(app)).language,
			pulsar_core::config::Language::En
		),
	};
	ENGLISH.store(english, Ordering::Relaxed);
	let (dark, game) = read_hint(app);
	let version = app.package_info().version.to_string();
	#[cfg(target_os = "linux")]
	native::show(dark, game, hold(), version);
	#[cfg(not(target_os = "linux"))]
	webview_show(app, dark, game, &version);
	if let Some(main) = app.get_webview_window("main") {
		// The main window's first frame before its UI paints, in the same theme.
		let _ = main.set_background_color(Some(surface(dark)));
	}
	// Safety net: a frontend that never calls `splash_done` must not leave the user
	// staring at a splash with no app behind it.
	let h = app.clone();
	std::thread::spawn(move || {
		std::thread::sleep(Duration::from_secs(12));
		if !DONE.load(Ordering::SeqCst) {
			tracing::warn!("UI never reported ready — revealing the main window anyway");
			reveal(&h);
		}
	});
}

/// Advance the splash to `s` (ignored once past it, or after the splash is gone).
pub(crate) fn step(app: &AppHandle, s: Step) {
	{
		let mut cur = STEP.lock().unwrap();
		if s <= *cur || DONE.load(Ordering::SeqCst) && s != Step::Ready {
			return;
		}
		*cur = s;
	}
	#[cfg(not(target_os = "linux"))]
	if let Some(w) = app.get_webview_window(LABEL) {
		let label = serde_json::to_string(s.label()).unwrap_or_default();
		let js = format!("window.splashStep && window.splashStep({}, {label}, {})", s.pct(), s == Step::Ready);
		let _ = w.eval(js.as_str());
	}
	#[cfg(target_os = "linux")]
	let _ = app; // the native card reads STEP every frame
}

#[cfg(not(target_os = "linux"))]
fn webview_show(app: &AppHandle, dark: bool, game: bool, version: &str) {
	use tauri::{WebviewUrl, WebviewWindowBuilder};
	fn enc(s: &str) -> String {
		s.bytes()
			.map(|b| match b {
				b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' => (b as char).to_string(),
				_ => format!("%{b:02X}"),
			})
			.collect()
	}
	let url = format!(
		"splash.html?theme={}&mode={}&v={}&s={}",
		if dark { "dark" } else { "light" },
		if game { "game" } else { "remote" },
		enc(version),
		enc(Step::Start.label()),
	);
	let built = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App(url.into()))
		.title("Pulsar")
		.inner_size(W, H)
		.min_inner_size(W, H)
		.max_inner_size(W, H)
		.center()
		.resizable(false)
		.maximizable(false)
		.minimizable(false)
		.decorations(false)
		.shadow(false)
		// Transparent window: the page draws the card (rounded corners see-through).
		.transparent(true)
		.background_color(Color(0, 0, 0, 0))
		.focused(true)
		.always_on_top(hold())
		.build();
	if let Err(e) = built {
		tracing::warn!("splash window failed: {e}");
	}
}

/// The main window has been revealed (the splash is gone or going).
pub(crate) fn is_done() -> bool {
	DONE.load(Ordering::SeqCst)
}

/// Bring the splash to the front (a second launch while this one is still booting).
/// Main thread only.
#[cfg(unix)]
pub(crate) fn present(app: &AppHandle, token: &str) {
	#[cfg(target_os = "linux")]
	{
		let _ = app;
		native::present(token);
	}
	#[cfg(not(target_os = "linux"))]
	{
		let _ = token;
		if let Some(w) = app.get_webview_window(LABEL) {
			let _ = w.set_focus();
		}
	}
}

/// Show "Ready", reveal + focus the main window, then close the splash a beat later (the
/// main window has painted by then — no gap between the two).
pub(crate) fn reveal(app: &AppHandle) {
	if hold() {
		return;
	}
	step(app, Step::Ready);
	DONE.store(true, Ordering::SeqCst);
	if let Some(w) = app.get_webview_window("main") {
		if !w.is_visible().unwrap_or(true) {
			let _ = w.show();
		}
		let _ = w.unminimize();
		let _ = w.set_focus();
	}
	let h = app.clone();
	std::thread::spawn(move || {
		// Long enough for the bar to glide to 100 % and the main window to paint.
		std::thread::sleep(Duration::from_millis(400));
		#[cfg(target_os = "linux")]
		let _ = h.run_on_main_thread(native::close);
		#[cfg(not(target_os = "linux"))]
		if let Some(s) = h.get_webview_window(LABEL) {
			let _ = s.destroy();
		}
	});
}

/// The main UI is ready: swap the splash for the main window.
#[tauri::command]
pub(crate) fn splash_done(app: AppHandle) {
	reveal(&app);
}

/// The UI bundle has loaded and mounted (now loading its language + waiting on caps).
#[tauri::command]
pub(crate) fn splash_ui(app: AppHandle) {
	step(&app, Step::Ui);
}

/// The UI is waiting on the startup encoder probe.
#[tauri::command]
pub(crate) fn splash_encoder(app: AppHandle) {
	step(&app, Step::Encoder);
}

/// Remember the current look for the next launch's splash ("dark"/"light", + " game", + " lang=xx").
#[tauri::command]
pub(crate) fn set_splash_theme(app: AppHandle, theme: String) {
	let p = hint_path(&app);
	if std::fs::read_to_string(&p).ok().as_deref() == Some(theme.as_str()) {
		return;
	}
	if let Some(dir) = p.parent() {
		let _ = std::fs::create_dir_all(dir);
	}
	let _ = std::fs::write(&p, theme);
}

/// The Linux splash: a plain GTK window on an RGBA visual, drawn with cairo/pango — the
/// same card as `static/splash.html` (design tokens from app.css, as sRGB).
#[cfg(target_os = "linux")]
mod native {
	use super::{Step, CARD_H, CARD_W, H, STEP, W};
	use gtk::glib;
	use gtk::pango;
	use gtk::prelude::*;
	use gtk::{cairo, gdk};
	use std::cell::{Cell, RefCell};
	use std::f64::consts::PI;
	use std::time::Instant;

	thread_local! {
		static WIN: RefCell<Option<gtk::Window>> = const { RefCell::new(None) };
	}

	/// sRGB 0..1 from 0xRRGGBB.
	fn rgb(c: u32) -> (f64, f64, f64) {
		(
			((c >> 16) & 0xff) as f64 / 255.0,
			((c >> 8) & 0xff) as f64 / 255.0,
			(c & 0xff) as f64 / 255.0,
		)
	}

	fn set(cr: &cairo::Context, c: u32, a: f64) {
		let (r, g, b) = rgb(c);
		cr.set_source_rgba(r, g, b, a);
	}

	/// The design tokens the card uses, per theme / mode.
	struct Look {
		dark: bool,
		surface: u32,
		surface3: u32,
		border: u32,
		text: u32,
		muted: u32,
		faint: u32,
		accent: u32,
		/// The logo's colour (`--mark`; lifted in dark so it reads on the dark card).
		mark: u32,
		soft: u32,
		glow: f64,
		ring_outer: f64,
		ring_mid: f64,
		ok: u32,
	}

	fn look(dark: bool, game: bool) -> Look {
		let (surface, surface3, border, text, muted, faint) = if dark {
			(0x161920, 0x282b33, 0x2f333a, 0xeceef4, 0xa1a5ac, 0x71747c)
		} else {
			(0xffffff, 0xf0f3f7, 0xe2e4e8, 0x1a1e27, 0x61656d, 0x878b92)
		};
		let (accent, mark, soft) = match (dark, game) {
			(false, false) => (0x4e60e8, 0x4e60e8, 0xe8efff),
			(true, false) => (0x4e60e8, 0x8aa3ff, 0x1f284e),
			(false, true) => (0x009bbe, 0x009bbe, 0xd7f7ff),
			(true, true) => (0x009bbe, 0x2bcceb, 0x003a46),
		};
		Look {
			dark,
			surface,
			surface3,
			border,
			text,
			muted,
			faint,
			accent,
			mark,
			soft,
			glow: if dark { 0.7 } else { 0.9 },
			ring_outer: if dark { 0.32 } else { 0.18 },
			ring_mid: if dark { 0.6 } else { 0.42 },
			ok: 0x00a365,
		}
	}

	/// ≈ the design's `--ease-out` / `--ease` (strong ease-out).
	fn ease(t: f64) -> f64 {
		1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(4)
	}

	fn rounded(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
		cr.new_sub_path();
		cr.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
		cr.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
		cr.arc(x + r, y + h - r, r, PI / 2.0, PI);
		cr.arc(x + r, y + r, r, PI, 1.5 * PI);
		cr.close_path();
	}

	/// A text layout: `family` at `px` pixels, `weight`, letter spacing in px.
	fn text(widget: &gtk::DrawingArea, s: &str, family: &str, px: f64, weight: pango::Weight, spacing: f64) -> pango::Layout {
		let layout = widget.create_pango_layout(Some(s));
		let mut fd = pango::FontDescription::new();
		fd.set_family(family);
		fd.set_weight(weight);
		fd.set_absolute_size(px * pango::SCALE as f64);
		layout.set_font_description(Some(&fd));
		if spacing != 0.0 {
			let attrs = pango::AttrList::new();
			attrs.insert(pango::AttrInt::new_letter_spacing((spacing * pango::SCALE as f64) as i32));
			layout.set_attributes(Some(&attrs));
		}
		layout
	}

	/// pango-cairo's `show_layout` (GTK links pangocairo; no extra crate needed): fills the
	/// layout with the current source.
	fn show_layout(cr: &cairo::Context, layout: &pango::Layout) {
		#[link(name = "pangocairo-1.0")]
		extern "C" {
			fn pango_cairo_show_layout(cr: *mut cairo::ffi::cairo_t, layout: *mut pango::ffi::PangoLayout);
		}
		use gtk::glib::translate::ToGlibPtr;
		unsafe { pango_cairo_show_layout(cr.to_raw_none(), layout.to_glib_none().0) };
	}

	const DISPLAY: &str = "Space Grotesk, Hanken Grotesk, Sans";
	const SANS: &str = "Hanken Grotesk, Sans";
	const MONO: &str = "JetBrains Mono, Monospace";

	struct State {
		look: Look,
		version: String,
		start: Instant,
		last: Cell<f64>,
		/// The bar's drawn width (0..1), easing toward the current step.
		bar: Cell<f64>,
		/// The step being drawn and when it began (s since `start`).
		since: Cell<(Step, f64)>,
	}

	fn draw(widget: &gtk::DrawingArea, cr: &cairo::Context, st: &State) {
		let t = st.start.elapsed().as_secs_f64();
		let dt = (t - st.last.replace(t)).clamp(0.0, 0.1);
		let l = &st.look;
		let step = *STEP.lock().unwrap();
		if st.since.get().0 != step {
			st.since.set((step, t));
		}
		let target = step.creep(t - st.since.get().1);
		// Glide toward the goal (never backwards); faster when a real step jumped ahead.
		let rate = if step == Step::Ready { 10.0 } else { 4.0 };
		let bar = (st.bar.get() + (target - st.bar.get()) * (dt * rate).min(1.0)).max(st.bar.get());
		st.bar.set(bar);

		// Fully transparent outside the card's rounded corners.
		cr.set_operator(cairo::Operator::Source);
		cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
		let _ = cr.paint();
		cr.set_operator(cairo::Operator::Over);

		cr.push_group();
		let (x0, y0) = (0.0, 0.0);
		let r = 26.0; // --r-xl

		// Card.
		rounded(cr, x0 + 0.5, y0 + 0.5, CARD_W - 1.0, CARD_H - 1.0, r);
		set(cr, l.surface, 1.0);
		let _ = cr.fill_preserve();
		cr.save().ok();
		cr.clip_preserve();
		// Accent glow: radial-gradient(60% 55% at 50% 38%, --accent-soft, transparent 70%).
		{
			cr.save().ok();
			cr.translate(x0 + CARD_W * 0.5, y0 + CARD_H * 0.38);
			cr.scale(CARD_W * 0.6, CARD_H * 0.55);
			let g = cairo::RadialGradient::new(0.0, 0.0, 0.0, 0.0, 0.0, 1.0);
			let (sr, sg, sb) = rgb(l.soft);
			g.add_color_stop_rgba(0.0, sr, sg, sb, l.glow);
			g.add_color_stop_rgba(0.7, sr, sg, sb, 0.0);
			let _ = cr.set_source(&g);
			let _ = cr.paint();
			cr.restore().ok();
		}

		// Mark: three ripples (2.4 s, staggered 0.8 s) behind a beating logo.
		// The mark (88) + 22 + the wordmark (~38) as one block, centred like the flex column.
		let (cx, cy) = (x0 + CARD_W / 2.0, y0 + (CARD_H - 148.0) / 2.0 + 44.0);
		for d in [0.0, 0.8, 1.6] {
			if t < d {
				continue;
			}
			let p = ease(((t - d) % 2.4) / 2.4);
			let scale = 0.6 + (3.2 - 0.6) * p;
			cr.new_path();
			cr.arc(cx, cy, 44.0 * scale, 0.0, 2.0 * PI);
			set(cr, l.accent, 0.45 * (1.0 - p));
			cr.set_line_width(1.5);
			let _ = cr.stroke();
		}
		let bp = (t % 2.4) / 2.4;
		let beat = if bp < 0.12 {
			1.0 + 0.06 * ease(bp / 0.12)
		} else if bp < 0.24 {
			1.06 - 0.06 * ease((bp - 0.12) / 0.12)
		} else {
			1.0
		};
		cr.save().ok();
		cr.translate(cx, cy);
		let k = beat * 72.0 / 52.0; // PulsarMark size 72 in its 52-unit viewBox
		cr.scale(k, k);
		for (rad, lw, a) in [(24.0, 1.5, l.ring_outer), (16.0, 1.75, l.ring_mid), (8.5, 2.0, 1.0)] {
			cr.new_path();
			cr.arc(0.0, 0.0, rad, 0.0, 2.0 * PI);
			set(cr, l.mark, a);
			cr.set_line_width(lw);
			let _ = cr.stroke();
		}
		cr.new_path();
		cr.arc(0.0, 0.0, 3.5, 0.0, 2.0 * PI);
		set(cr, l.mark, 1.0);
		let _ = cr.fill();
		cr.restore().ok();

		// Wordmark, centered under the mark.
		let word = text(widget, "Pulsar", DISPLAY, 30.0, pango::Weight::Semibold, -1.05);
		let (ww, _) = word.pixel_size();
		let wy = cy + 44.0 + 22.0;
		cr.move_to(cx - ww as f64 / 2.0, wy);
		set(cr, l.text, 1.0);
		show_layout(cr, &word);

		// Version, top-right.
		let ver = text(widget, &format!("v{}", st.version), MONO, 11.0, pango::Weight::Normal, 0.0);
		let (vw, _) = ver.pixel_size();
		cr.move_to(x0 + CARD_W - 18.0 - vw as f64, y0 + 16.0);
		set(cr, l.faint, 1.0);
		show_layout(cr, &ver);

		// Status row + progress bar, bottom.
		let (lx, rx) = (x0 + 36.0, x0 + CARD_W - 36.0);
		let bar_y = y0 + CARD_H - 30.0 - 3.0;
		rounded(cr, lx, bar_y, rx - lx, 3.0, 1.5);
		set(cr, l.surface3, 1.0);
		let _ = cr.fill();
		if bar > 0.004 {
			rounded(cr, lx, bar_y, ((rx - lx) * bar).max(3.0), 3.0, 1.5);
			set(cr, l.accent, 1.0);
			let _ = cr.fill();
		}
		let status = text(widget, step.label(), SANS, 12.0, pango::Weight::Normal, 0.0);
		let (_, sh) = status.pixel_size();
		let row_y = bar_y - 10.0 - sh as f64;
		cr.new_path();
		cr.arc(lx + 3.0, row_y + sh as f64 / 2.0, 3.0, 0.0, 2.0 * PI);
		set(cr, if step == Step::Ready { l.ok } else { l.accent }, 1.0);
		let _ = cr.fill();
		cr.move_to(lx + 6.0 + 8.0, row_y);
		set(cr, l.muted, 1.0);
		show_layout(cr, &status);
		let pct = text(widget, &format!("{}%", (bar * 100.0).round() as u32), MONO, 11.0, pango::Weight::Normal, 0.0);
		let (pw, ph) = pct.pixel_size();
		cr.move_to(rx - pw as f64, row_y + (sh - ph) as f64 / 2.0);
		set(cr, l.faint, 1.0);
		show_layout(cr, &pct);
		cr.restore().ok(); // card clip

		// Border on top.
		rounded(cr, x0 + 0.5, y0 + 0.5, CARD_W - 1.0, CARD_H - 1.0, r);
		set(cr, l.border, 1.0);
		cr.set_line_width(1.0);
		let _ = cr.stroke();

		let _ = cr.pop_group_to_source();
		let _ = cr.paint_with_alpha(ease(t / 0.3));
	}

	pub(super) fn show(dark: bool, game: bool, keep_above: bool, version: String) {
		let win = gtk::Window::new(gtk::WindowType::Toplevel);
		win.set_title("Pulsar");
		win.set_icon_name(Some("pulsar"));
		win.set_decorated(false);
		win.set_resizable(false);
		win.set_size_request(W as i32, H as i32);
		win.set_default_size(W as i32, H as i32);
		win.set_position(gtk::WindowPosition::Center);
		win.set_keep_above(keep_above);
		// RGBA visual + app-paintable: the corners outside the card stay see-through.
		if let Some(v) = WidgetExt::screen(&win).and_then(|s| s.rgba_visual()) {
			win.set_visual(Some(&v));
		}
		win.set_app_paintable(true);

		let area = gtk::DrawingArea::new();
		let st = State {
			look: look(dark, game),
			version,
			start: Instant::now(),
			last: Cell::new(0.0),
			bar: Cell::new(0.0),
			since: Cell::new((Step::Start, 0.0)),
		};
		area.connect_draw(move |a, cr| {
			draw(a, cr, &st);
			glib::Propagation::Stop
		});
		// Drag the card to move the window.
		area.add_events(gdk::EventMask::BUTTON_PRESS_MASK);
		let w2 = win.clone();
		area.connect_button_press_event(move |_, e| {
			if e.button() == 1 {
				let (x, y) = e.root();
				w2.begin_move_drag(1, x as i32, y as i32, e.time());
			}
			glib::Propagation::Stop
		});
		area.add_tick_callback(|a, _| {
			a.queue_draw();
			glib::ControlFlow::Continue
		});
		win.add(&area);
		win.show_all();
		WIN.with(|c| *c.borrow_mut() = Some(win));
	}

	pub(super) fn present(token: &str) {
		WIN.with(|c| {
			if let Some(w) = c.borrow().as_ref() {
				if !token.is_empty() {
					w.set_startup_id(token);
				}
				w.present();
			}
		});
	}

	pub(super) fn close() {
		WIN.with(|c| {
			if let Some(w) = c.borrow_mut().take() {
				w.close();
			}
		});
	}
}
