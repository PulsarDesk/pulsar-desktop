//! The installer's look: design tokens, fonts and the hand-drawn pieces (icons, the Pulsar
//! mark, buttons, boxes). Source: the "Pulsar Kurulum" design (Pulsar DS, converted to sRGB
//! for egui). Light and dark follow the Windows app theme. No animation anywhere.

use std::sync::Arc;

use eframe::egui::{
	self, epaint::Mesh, text::LayoutJob, Align2, Color32, CornerRadius, FontData, FontDefinitions, FontFamily,
	FontId, Painter, Pos2, Rect, Response, Sense, Shape, Stroke, StrokeKind, TextFormat, Ui, Vec2,
};

/// Window size (logical px).
pub const W: f32 = 520.0;
pub const H: f32 = 380.0;
/// Horizontal content margin.
pub const PAD: f32 = 28.0;

/// One theme's colors.
#[derive(Clone, Copy)]
pub struct Tok {
	pub bg: Color32,
	pub bar: Color32,
	pub sunken: Color32,
	pub track: Color32,
	pub border: Color32,
	pub border_strong: Color32,
	pub text: Color32,
	pub muted: Color32,
	pub faint: Color32,
	pub accent: Color32,
	pub accent_hover: Color32,
	pub accent_press: Color32,
	pub on_accent: Color32,
	pub accent_soft: Color32,
	pub accent_text: Color32,
	pub ok: Color32,
	pub warn_soft: Color32,
	pub warn_text: Color32,
	pub danger: Color32,
	pub danger_hover: Color32,
	pub danger_soft: Color32,
	pub danger_text: Color32,
}

const fn hex(v: u32) -> Color32 {
	Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

pub const LIGHT: Tok = Tok {
	bg: hex(0xFFFFFF),
	bar: hex(0xF6F8FB),
	sunken: hex(0xF6F8FB),
	track: hex(0xF0F3F7),
	border: hex(0xE2E4E8),
	border_strong: hex(0xCED2D8),
	text: hex(0x1A1E27),
	muted: hex(0x61656D),
	faint: hex(0x71757C),
	accent: hex(0x4E60E8),
	accent_hover: hex(0x404DDC),
	accent_press: hex(0x363FC6),
	on_accent: hex(0xF9FCFF),
	accent_soft: hex(0xE8EFFF),
	accent_text: hex(0x3A45C8),
	ok: hex(0x00A365),
	warn_soft: hex(0xFFEECD),
	warn_text: hex(0x915200),
	danger: hex(0xD72D34),
	danger_hover: hex(0xC51224),
	danger_soft: hex(0xFFE8E4),
	danger_text: hex(0xBE222A),
};

pub const DARK: Tok = Tok {
	bg: hex(0x161920),
	bar: hex(0x1D2027),
	sunken: hex(0x1D2027),
	track: hex(0x282B33),
	border: hex(0x2F333A),
	border_strong: hex(0x494D56),
	text: hex(0xECEEF4),
	muted: hex(0xA1A5AC),
	faint: hex(0x888C94),
	accent: hex(0x4E60E8),
	accent_hover: hex(0x404DDC),
	accent_press: hex(0x363FC6),
	on_accent: hex(0xF9FCFF),
	accent_soft: hex(0x1F284E),
	accent_text: hex(0x8EA5FD),
	ok: hex(0x00A365),
	warn_soft: hex(0x422B06),
	warn_text: hex(0xF0BA59),
	danger: hex(0xD72D34),
	danger_hover: hex(0xC51224),
	danger_soft: hex(0x4F1A18),
	danger_text: hex(0xFB817A),
};

// ── Fonts ───────────────────────────────────────────────────────────────────────

/// Brand fonts embedded (Hanken Grotesk 400/600, Space Grotesk 600 — Latin + Latin Extended
/// subsets, OFL, see fonts/); Segoe UI from the system as the fallback (Cyrillic, arrows), and
/// the system monospace (Cascadia Mono on Windows 11, else Consolas).
pub fn fonts() -> FontDefinitions {
	let mut f = FontDefinitions::default();
	let embed = |f: &mut FontDefinitions, k: &str, b: &'static [u8]| {
		f.font_data.insert(k.to_string(), Arc::new(FontData::from_static(b)));
	};
	embed(&mut f, "hk4", include_bytes!("../fonts/HankenGrotesk-400-latin.ttf"));
	embed(&mut f, "hk4x", include_bytes!("../fonts/HankenGrotesk-400-latin-ext.ttf"));
	embed(&mut f, "hk6", include_bytes!("../fonts/HankenGrotesk-600-latin.ttf"));
	embed(&mut f, "hk6x", include_bytes!("../fonts/HankenGrotesk-600-latin-ext.ttf"));
	embed(&mut f, "sg6", include_bytes!("../fonts/SpaceGrotesk-600-latin.ttf"));
	embed(&mut f, "sg6x", include_bytes!("../fonts/SpaceGrotesk-600-latin-ext.ttf"));
	let mut sys = |k: &str, path: &str| -> bool {
		match std::fs::read(path) {
			Ok(b) => {
				f.font_data.insert(k.to_string(), Arc::new(FontData::from_owned(b)));
				true
			}
			Err(_) => false,
		}
	};
	let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".to_string());
	let segoe = sys("segoe", &format!(r"{windir}\Fonts\segoeui.ttf"));
	let segoe6 = sys("segoe6", &format!(r"{windir}\Fonts\seguisb.ttf"));
	let mono = sys("mono", &format!(r"{windir}\Fonts\CascadiaMono.ttf"))
		|| sys("mono", &format!(r"{windir}\Fonts\consola.ttf"));

	let defaults_prop = f.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
	let defaults_mono = f.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
	let chain = |keys: &[&str], on: &[bool], tail: &[String]| -> Vec<String> {
		keys.iter()
			.zip(on)
			.filter(|(_, ok)| **ok)
			.map(|(k, _)| k.to_string())
			.chain(tail.iter().cloned())
			.collect()
	};
	f.families.insert(
		FontFamily::Proportional,
		chain(&["hk4", "hk4x", "segoe"], &[true, true, segoe], &defaults_prop),
	);
	f.families.insert(
		FontFamily::Name("semibold".into()),
		chain(&["hk6", "hk6x", "segoe6", "segoe"], &[true, true, segoe6, segoe], &defaults_prop),
	);
	f.families.insert(
		FontFamily::Name("display".into()),
		chain(&["sg6", "sg6x", "hk6", "hk6x", "segoe6"], &[true, true, true, true, segoe6], &defaults_prop),
	);
	f.families.insert(FontFamily::Monospace, chain(&["mono", "segoe"], &[mono, segoe], &defaults_mono));
	f
}

pub fn regular(size: f32) -> FontId {
	FontId::new(size, FontFamily::Proportional)
}
pub fn semibold(size: f32) -> FontId {
	FontId::new(size, FontFamily::Name("semibold".into()))
}
pub fn display(size: f32) -> FontId {
	FontId::new(size, FontFamily::Name("display".into()))
}
pub fn mono(size: f32) -> FontId {
	FontId::new(size, FontFamily::Monospace)
}

/// Lay out text with a fixed line height, wrapped at `width` (`max_rows` 0 = unlimited;
/// `anywhere` breaks inside words, for paths).
pub fn text_job(text: &str, font: FontId, color: Color32, line: f32, width: f32, max_rows: usize, anywhere: bool) -> LayoutJob {
	let mut job = LayoutJob::single_section(
		text.to_string(),
		TextFormat { font_id: font, color, line_height: Some(line), ..Default::default() },
	);
	job.wrap.max_width = width;
	if max_rows > 0 {
		job.wrap.max_rows = max_rows;
	}
	job.wrap.break_anywhere = anywhere;
	job
}

/// Draw `job` at `pos` (top-left); returns its size.
pub fn draw_job(p: &Painter, pos: Pos2, job: LayoutJob) -> Vec2 {
	let g = p.layout_job(job);
	let size = g.size();
	p.galley(pos, g, Color32::PLACEHOLDER);
	size
}

/// Single-line text width.
pub fn text_width(p: &Painter, text: &str, font: FontId) -> f32 {
	p.layout_no_wrap(text.to_string(), font, Color32::WHITE).size().x
}

/// Shorten `text` in the middle (keeping its end — the file / folder name) to fit `width`.
pub fn ellipsize_middle(p: &Painter, text: &str, font: &FontId, width: f32) -> String {
	if text_width(p, text, font.clone()) <= width {
		return text.to_string();
	}
	let chars: Vec<char> = text.chars().collect();
	let keep_end = chars.len() * 3 / 5;
	let mut head = chars.len().saturating_sub(keep_end).min(chars.len() / 3);
	let tail_start = chars.len() - keep_end;
	loop {
		let s: String = chars[..head].iter().chain(['…'].iter()).chain(chars[tail_start..].iter()).collect();
		if head == 0 || text_width(p, &s, font.clone()) <= width {
			if text_width(p, &s, font.clone()) <= width {
				return s;
			}
			// Even the tail alone is too long: keep only its end.
			let mut t = tail_start;
			while t < chars.len() {
				let s: String = ['…'].iter().chain(chars[t..].iter()).collect();
				if text_width(p, &s, font.clone()) <= width {
					return s;
				}
				t += 1;
			}
			return "…".to_string();
		}
		head -= 1;
	}
}

// ── Icons (16 px grid, 1.5 px strokes — the design's SVGs as lines and circles) ───

#[derive(Clone, Copy, PartialEq)]
pub enum Icon {
	Close,
	Check,
	Folder,
	Info,
	Warn,
	User,
	Lock,
	Arrow,
	Down,
	Retry,
	Copy,
	Minus,
}

/// Draw `icon` with its top-left at `at`, `size` px (the 16-grid scaled), in `color`.
pub fn icon(p: &Painter, icon: Icon, at: Pos2, size: f32, color: Color32, width: f32) {
	let s = size / 16.0;
	let pt = |x: f32, y: f32| at + Vec2::new(x * s, y * s);
	let st = Stroke::new(width, color);
	let line = |pts: &[(f32, f32)]| {
		p.add(Shape::line(pts.iter().map(|&(x, y)| pt(x, y)).collect(), st));
	};
	let closed = |pts: &[(f32, f32)]| {
		p.add(Shape::closed_line(pts.iter().map(|&(x, y)| pt(x, y)).collect(), st));
	};
	let arc = |cx: f32, cy: f32, r: f32, a0: f32, a1: f32| -> Vec<Pos2> {
		let n = 24;
		(0..=n)
			.map(|i| {
				let a = (a0 + (a1 - a0) * i as f32 / n as f32).to_radians();
				pt(cx + r * a.cos(), cy + r * a.sin())
			})
			.collect()
	};
	match icon {
		Icon::Close => {
			// 10-grid in the design: map onto 16 with a 3 px inset.
			line(&[(3.0, 3.0), (13.0, 13.0)]);
			line(&[(13.0, 3.0), (3.0, 13.0)]);
		}
		Icon::Check => line(&[(3.5, 8.4), (6.5, 11.4), (12.5, 4.8)]),
		Icon::Folder => closed(&[
			(1.75, 4.25),
			(2.4, 3.1),
			(3.25, 2.75),
			(6.25, 2.75),
			(7.75, 4.5),
			(12.75, 4.5),
			(13.9, 5.1),
			(14.25, 6.0),
			(14.25, 12.25),
			(13.9, 13.3),
			(12.75, 13.75),
			(3.25, 13.75),
			(2.1, 13.3),
			(1.75, 12.25),
		]),
		Icon::Info => {
			p.circle_stroke(pt(8.0, 8.0), 6.25 * s, st);
			line(&[(8.0, 7.25), (8.0, 11.0)]);
			p.circle_filled(pt(8.0, 5.0), 0.9 * s, color);
		}
		Icon::Warn => {
			closed(&[(8.0, 1.9), (14.4, 13.1), (1.6, 13.1)]);
			line(&[(8.0, 6.5), (8.0, 9.5)]);
			p.circle_filled(pt(8.0, 11.3), 0.9 * s, color);
		}
		Icon::User => {
			p.circle_stroke(pt(8.0, 5.5), 2.75 * s, st);
			p.add(Shape::line(arc(8.0, 14.6, 5.25, 190.0, 350.0), st));
		}
		Icon::Lock => {
			p.rect_stroke(
				Rect::from_min_size(pt(3.0, 7.0), Vec2::new(10.0 * s, 7.0 * s)),
				CornerRadius::same((1.75 * s) as u8),
				st,
				StrokeKind::Middle,
			);
			let mut sh = vec![pt(5.25, 7.0)];
			sh.extend(arc(8.0, 5.25, 2.75, 180.0, 360.0));
			sh.push(pt(10.75, 7.0));
			p.add(Shape::line(sh, st));
		}
		Icon::Arrow => {
			line(&[(3.0, 8.0), (13.0, 8.0)]);
			line(&[(9.0, 4.0), (13.0, 8.0), (9.0, 12.0)]);
		}
		Icon::Down => {
			line(&[(8.0, 2.5), (8.0, 10.5)]);
			line(&[(4.5, 7.5), (8.0, 11.0), (11.5, 7.5)]);
			line(&[(3.0, 13.5), (13.0, 13.5)]);
		}
		Icon::Retry => {
			p.add(Shape::line(arc(8.0, 8.0, 5.0, 0.0, 312.0), st));
			line(&[(13.0, 2.5), (13.0, 5.5), (10.0, 5.5)]);
		}
		Icon::Copy => {
			p.rect_stroke(
				Rect::from_min_size(pt(5.0, 5.0), Vec2::new(8.5 * s, 8.5 * s)),
				CornerRadius::same((1.75 * s) as u8),
				st,
				StrokeKind::Middle,
			);
			line(&[(11.0, 5.0), (11.0, 3.5), (10.25, 2.5), (3.5, 2.5), (2.5, 3.5), (2.5, 10.25), (3.5, 11.0), (5.0, 11.0)]);
		}
		Icon::Minus => line(&[(4.0, 8.0), (12.0, 8.0)]),
	}
}

// ── The Pulsar mark (static: no rings, no beat) ──────────────────────────────────

/// The mark, 52 px, centred in `rect`. `neutral` = the faint grey of the failed / removed
/// screens instead of the accent.
pub fn mark(p: &Painter, rect: Rect, t: &Tok, neutral: bool) {
	let c = rect.center();
	let col = if neutral { t.faint } else { t.accent };
	let s = rect.width() / 52.0;
	p.circle_stroke(c, 24.0 * s, Stroke::new(1.5, col.gamma_multiply(0.18)));
	p.circle_stroke(c, 16.0 * s, Stroke::new(1.75, col.gamma_multiply(0.42)));
	p.circle_stroke(c, 8.5 * s, Stroke::new(2.0, col));
	p.circle_filled(c, 3.5 * s, col);
}

/// The soft halo behind the mark: a radial fade from `accent_soft` to transparent.
pub fn glow(p: &Painter, center: Pos2, r: f32, color: Color32) {
	let mut m = Mesh::default();
	m.colored_vertex(center, color);
	let n = 48;
	for i in 0..=n {
		let a = std::f32::consts::TAU * i as f32 / n as f32;
		m.colored_vertex(center + Vec2::new(a.cos(), a.sin()) * r, Color32::TRANSPARENT);
	}
	for i in 1..=n as u32 {
		m.add_triangle(0, i, i + 1);
	}
	p.add(Shape::mesh(m));
}

// ── Buttons ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
	Primary,
	Ghost,
	Quiet,
	Danger,
}

/// A footer button. `icon_first` puts the icon before the label (otherwise after it).
pub struct Btn<'a> {
	pub label: &'a str,
	pub kind: Kind,
	pub icon: Option<Icon>,
	pub icon_after: bool,
	pub enabled: bool,
}

impl<'a> Btn<'a> {
	pub fn new(label: &'a str, kind: Kind) -> Self {
		Self { label, kind, icon: None, icon_after: false, enabled: true }
	}
	pub fn icon(mut self, i: Icon) -> Self {
		self.icon = Some(i);
		self
	}
	pub fn icon_after(mut self, i: Icon) -> Self {
		self.icon = Some(i);
		self.icon_after = true;
		self
	}
	pub fn enabled(mut self, on: bool) -> Self {
		self.enabled = on;
		self
	}

	/// The width it needs.
	pub fn width(&self, p: &Painter) -> f32 {
		let pad = if self.kind == Kind::Quiet { 12.0 } else { 16.0 };
		let min = if self.kind == Kind::Quiet { 0.0 } else { 88.0 };
		let icon = if self.icon.is_some() { 14.0 + 8.0 } else { 0.0 };
		(text_width(p, self.label, semibold(13.5)) + icon + pad * 2.0).max(min)
	}

	/// Draw at `rect` (h 34) and return its response.
	pub fn show(&self, ui: &mut Ui, rect: Rect, t: &Tok, id: egui::Id) -> Response {
		let sense = if self.enabled { Sense::click() } else { Sense::hover() };
		let r = ui.interact(rect, id, sense);
		let p = ui.painter();
		let (hov, press) = (self.enabled && r.hovered(), self.enabled && r.is_pointer_button_down_on());
		let (fill, fg, border) = match (self.kind, self.enabled) {
			(Kind::Primary, true) => (
				if press {
					t.accent_press
				} else if hov {
					t.accent_hover
				} else {
					t.accent
				},
				t.on_accent,
				None,
			),
			(Kind::Primary, false) => (t.accent_soft, t.accent_text, None),
			(Kind::Ghost, true) => (if hov { t.track } else { t.bg }, t.text, Some(t.border_strong)),
			(Kind::Ghost, false) => (t.bg, t.faint, Some(t.border)),
			(Kind::Quiet, _) => (if hov { t.track } else { Color32::TRANSPARENT }, if hov { t.text } else { t.muted }, None),
			(Kind::Danger, _) => (if hov { t.danger_hover } else { t.danger }, Color32::WHITE, None),
		};
		if r.has_focus() {
			p.rect_stroke(rect.expand(2.0), CornerRadius::same(10), Stroke::new(2.0, t.accent), StrokeKind::Outside);
		}
		p.rect_filled(rect, CornerRadius::same(8), fill);
		if let Some(b) = border {
			p.rect_stroke(rect, CornerRadius::same(8), Stroke::new(1.0, b), StrokeKind::Inside);
		}
		let tw = text_width(p, self.label, semibold(13.5));
		let iw = if self.icon.is_some() { 14.0 + 8.0 } else { 0.0 };
		let mut x = rect.center().x - (tw + iw) / 2.0;
		let cy = rect.center().y;
		if let (Some(i), false) = (self.icon, self.icon_after) {
			icon(p, i, Pos2::new(x, cy - 7.0), 14.0, fg, 1.7);
			x += iw;
		}
		p.text(Pos2::new(x, cy), Align2::LEFT_CENTER, self.label, semibold(13.5), fg);
		if let (Some(i), true) = (self.icon, self.icon_after) {
			icon(p, i, Pos2::new(x + tw + 8.0, cy - 7.0), 14.0, fg, 1.7);
		}
		if self.enabled {
			r.on_hover_cursor(egui::CursorIcon::PointingHand)
		} else {
			r
		}
	}
}

// ── Body pieces (each returns the height it used) ─────────────────────────────────

/// "Kurulum yeri" + the path box.
pub fn path_box(p: &Painter, top_left: Pos2, width: f32, t: &Tok, path: &str) -> f32 {
	let lh = draw_job(p, top_left, text_job("Kurulum yeri", semibold(12.0), t.muted, 16.0, width, 1, false)).y;
	let y0 = top_left.y + lh + 6.0;
	let inner_w = width - 24.0 - 16.0 - 10.0;
	let job = text_job(path, mono(12.0), t.text, 17.0, inner_w, 2, true);
	let g = p.layout_job(job);
	let h = g.size().y.max(17.0) + 18.0;
	let rect = Rect::from_min_size(Pos2::new(top_left.x, y0), Vec2::new(width, h));
	p.rect_filled(rect, CornerRadius::same(8), t.sunken);
	p.rect_stroke(rect, CornerRadius::same(8), Stroke::new(1.0, t.border), StrokeKind::Inside);
	icon(p, Icon::Folder, Pos2::new(rect.min.x + 12.0, rect.min.y + 9.5), 16.0, t.muted, 1.5);
	p.galley(Pos2::new(rect.min.x + 12.0 + 16.0 + 10.0, rect.min.y + 9.0), g, Color32::PLACEHOLDER);
	y0 + h - top_left.y
}

/// `path_box` with a "Değiştir" button at its right edge (fresh installs; an update keeps the
/// registered folder, so it passes `editable = false` and the button is not drawn).
/// Returns (height, change clicked).
pub fn path_box_edit(ui: &mut Ui, top_left: Pos2, width: f32, t: &Tok, path: &str, editable: bool, id: egui::Id) -> (f32, bool) {
	if !editable {
		return (path_box(ui.painter(), top_left, width, t, path), false);
	}
	let p = ui.painter().clone();
	let label = "Değiştir";
	let bw = text_width(&p, label, semibold(12.5)) + 20.0;
	let lh = draw_job(&p, top_left, text_job("Kurulum yeri", semibold(12.0), t.muted, 16.0, width, 1, false)).y;
	let y0 = top_left.y + lh + 6.0;
	let inner_w = width - 24.0 - 16.0 - 10.0 - bw - 8.0;
	let job = text_job(path, mono(12.0), t.text, 17.0, inner_w, 2, true);
	let g = p.layout_job(job);
	let h = g.size().y.max(17.0) + 18.0;
	let rect = Rect::from_min_size(Pos2::new(top_left.x, y0), Vec2::new(width, h));
	p.rect_filled(rect, CornerRadius::same(8), t.sunken);
	p.rect_stroke(rect, CornerRadius::same(8), Stroke::new(1.0, t.border), StrokeKind::Inside);
	icon(&p, Icon::Folder, Pos2::new(rect.min.x + 12.0, rect.min.y + 9.5), 16.0, t.muted, 1.5);
	p.galley(Pos2::new(rect.min.x + 12.0 + 16.0 + 10.0, rect.min.y + 9.0), g, Color32::PLACEHOLDER);
	let b = Rect::from_min_size(Pos2::new(rect.max.x - 6.0 - bw, rect.center().y - 13.0), Vec2::new(bw, 26.0));
	let r = ui.interact(b, id, Sense::click());
	let fill = if r.hovered() { t.track } else { t.bg };
	p.rect_filled(b, CornerRadius::same(6), fill);
	p.rect_stroke(b, CornerRadius::same(6), Stroke::new(1.0, t.border_strong), StrokeKind::Inside);
	if r.has_focus() {
		p.rect_stroke(b.expand(2.0), CornerRadius::same(8), Stroke::new(2.0, t.accent), StrokeKind::Outside);
	}
	p.text(b.center(), Align2::CENTER_CENTER, label, semibold(12.5), t.text);
	let clicked = r.clicked();
	r.on_hover_cursor(egui::CursorIcon::PointingHand);
	(y0 + h - top_left.y, clicked)
}

/// A checkbox row; returns (height, clicked).
pub fn checkbox(ui: &mut Ui, top_left: Pos2, t: &Tok, label: &str, on: bool, id: egui::Id) -> (f32, bool) {
	let tw = text_width(ui.painter(), label, regular(13.0));
	let rect = Rect::from_min_size(top_left, Vec2::new(16.0 + 10.0 + tw, 19.0));
	let r = ui.interact(rect, id, Sense::click());
	let p = ui.painter();
	let b = Rect::from_min_size(Pos2::new(top_left.x, top_left.y + 1.5), Vec2::splat(16.0));
	if r.has_focus() {
		p.rect_stroke(b.expand(2.0), CornerRadius::same(7), Stroke::new(2.0, t.accent), StrokeKind::Outside);
	}
	if on {
		p.rect_filled(b, CornerRadius::same(5), t.accent);
		icon(p, Icon::Check, b.min + Vec2::splat(2.0), 12.0, t.on_accent, 2.2);
	} else {
		p.rect_filled(b, CornerRadius::same(5), t.bg);
		p.rect_stroke(b, CornerRadius::same(5), Stroke::new(1.5, t.border_strong), StrokeKind::Inside);
	}
	p.text(Pos2::new(top_left.x + 26.0, top_left.y + 9.5), Align2::LEFT_CENTER, label, regular(13.0), t.text);
	let clicked = r.clicked();
	r.on_hover_cursor(egui::CursorIcon::PointingHand);
	(19.0, clicked)
}

#[derive(Clone, Copy, PartialEq)]
pub enum Tone {
	Info,
	Warn,
	Danger,
}

/// A banner: icon, optional bold title (tone colour), body text (muted when there's a title).
pub fn banner(p: &Painter, top_left: Pos2, width: f32, t: &Tok, tone: Tone, ic: Icon, title: Option<&str>, body: &str) -> f32 {
	let (bg, fg) = match tone {
		Tone::Info => (t.accent_soft, t.accent_text),
		Tone::Warn => (t.warn_soft, t.warn_text),
		Tone::Danger => (t.danger_soft, t.danger_text),
	};
	let tx = top_left.x + 12.0 + 16.0 + 10.0;
	let tw = width - 12.0 - 16.0 - 10.0 - 12.0;
	let mut gs = Vec::new();
	if let Some(h) = title {
		gs.push(p.layout_job(text_job(h, semibold(13.0), fg, 18.0, tw, 0, false)));
	}
	let body_col = if title.is_some() { t.muted } else { t.text };
	gs.push(p.layout_job(text_job(body, regular(12.5), body_col, 18.0, tw, 0, false)));
	let text_h: f32 = gs.iter().map(|g| g.size().y).sum::<f32>() + if title.is_some() { 2.0 } else { 0.0 };
	let rect = Rect::from_min_size(top_left, Vec2::new(width, text_h + 20.0));
	p.rect_filled(rect, CornerRadius::same(8), bg);
	icon(p, ic, Pos2::new(top_left.x + 12.0, top_left.y + 11.0), 16.0, fg, 1.5);
	let mut y = top_left.y + 10.0;
	let n = gs.len();
	for (i, g) in gs.into_iter().enumerate() {
		let h = g.size().y;
		p.galley(Pos2::new(tx, y), g, Color32::PLACEHOLDER);
		y += h + if i + 1 < n { 2.0 } else { 0.0 };
	}
	rect.height()
}

/// The progress block: header (label + counter), 6 px track, one file line.
pub fn progress(p: &Painter, top_left: Pos2, width: f32, t: &Tok, label: &str, counter: &str, frac: f32, file: &str) -> f32 {
	p.text(top_left + Vec2::new(0.0, 8.5), Align2::LEFT_CENTER, label, semibold(13.0), t.text);
	p.text(top_left + Vec2::new(width, 8.5), Align2::RIGHT_CENTER, counter, mono(12.0), t.muted);
	let track = Rect::from_min_size(top_left + Vec2::new(0.0, 25.0), Vec2::new(width, 6.0));
	p.rect_filled(track, CornerRadius::same(3), t.track);
	let w = (width * frac.clamp(0.0, 1.0)).max(if frac > 0.0 { 6.0 } else { 0.0 });
	if w > 0.0 {
		p.rect_filled(Rect::from_min_size(track.min, Vec2::new(w, 6.0)), CornerRadius::same(3), t.accent);
	}
	let f = ellipsize_middle(p, file, &mono(11.5), width);
	p.text(top_left + Vec2::new(0.0, 39.0 + 8.0), Align2::LEFT_CENTER, f, mono(11.5), t.muted);
	56.0
}

/// One list row: an icon (16, its own colour) and text (13/19), optional suffix in its own font
/// and `suffix_col`.
#[allow(clippy::too_many_arguments)]
pub fn list_row(
	p: &Painter,
	top_left: Pos2,
	width: f32,
	ic: Icon,
	ic_col: Color32,
	text: &str,
	text_col: Color32,
	suffix: Option<(&str, FontId)>,
	suffix_col: Color32,
) -> f32 {
	icon(p, ic, Pos2::new(top_left.x, top_left.y + 1.5), 16.0, ic_col, 1.8);
	let x = top_left.x + 26.0;
	let mut job = text_job(text, regular(13.0), text_col, 19.0, width - 26.0, 0, false);
	if let Some((s, font)) = suffix {
		job.append(s, 0.0, TextFormat { font_id: font, color: suffix_col, line_height: Some(19.0), ..Default::default() });
	}
	draw_job(p, Pos2::new(x, top_left.y), job).y.max(19.0)
}
