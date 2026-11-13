//! The in-session overlay — pure egui, backend-agnostic, so every platform paints
//! the IDENTICAL UI. Mirrors the Svelte game overlay (`Session.svelte`): header +
//! stat tiles + selector rows + quality segment + End + footer. Game mode shows the
//! slim set; Remote mode adds the full remote controls (file/clipboard/monitor) later.

/// Phosphor icon glyphs (the `regular` variant — installed into egui's atlas in `apply_theme`).
/// Used for the category boxes and stat tiles so each shows a DISTINCT icon (the old emoji
/// strings rendered as tofu in the bundled egui font).
use egui_phosphor::regular as icon;

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
	Game,
	Remote,
}

/// Live state the host process feeds in each frame (stats) + the current selector
/// values (the overlay owns these once open; changes flow OUT as `OverlayCmd`).
pub struct OverlayState {
	pub mode: Mode,
	pub open: bool,
	pub id: String,
	pub conn_label: String,
	// live stats
	pub fps: f32,
	pub latency_ms: f32,
	pub decode_ms: f32,
	pub mbps: f32,
	// selector current values (string keys matching the frontend setters)
	pub codec: String,
	pub encoder: String,
	/// The ACTUAL decoder in use (read-only display — selection is automatic).
	pub decoder: String,
	/// Host's validated caps (empty = unknown → no filtering): codecs the host can
	/// emit and encoder backends it has. Synced from the app via the stdin `caps` line.
	pub host_codecs: Vec<String>,
	pub host_encoders: Vec<String>,
	/// Host's streamable monitors as `(idx, label)`, primary first — the Display
	/// section's screen picker. Empty / single = no picker. Synced over the `caps` line.
	pub displays: Vec<(u32, String)>,
	/// Currently-streamed host monitor index (0 = primary). Owned by the overlay once
	/// open; a change flows out as `OverlayCmd::Set("display", "<idx>")`.
	pub display_idx: u32,
	/// Host's ACTIVE encode summary (split per-field under the selectors).
	pub host_active: String,
	/// Always-on mini stats HUD while the overlay is CLOSED (user toggle, persisted
	/// by the frontend; synced over stdin `statshud 0|1`).
	pub stats_hud: bool,
	/// Let the host show this device's pointer with its name (remote mode, the Yayın row;
	/// synced over stdin `curlabel 0|1`).
	pub cur_label: bool,
	/// Parsec-style always-visible overlay-open button (Pulsar mark, top-center)
	/// while the overlay is CLOSED. Toggleable from the overlay + Settings.
	pub overlay_btn: bool,
	/// Overlay-open button position: top-left offset in egui POINTS. Drag-movable
	/// from the webview hotspot; synced over stdin (`ovbtnpos <x> <y>`, re-seeded
	/// after a respawn via the caps line's `btnpos=x,y`).
	pub btn_pos: (f32, f32),
	pub res: String,
	pub fps_sel: String,
	pub bitrate: String, // Mbit, "0" = auto
	pub quality: String, // "latency" | "quality"
	pub pace: bool,      // frame pacing (Moonlight-style smoothing) on/off
	/// View-fit mode label ("fit"/"stretch"/"original") — mirrors video::fit_label.
	pub fit: String,
	/// Session audio state (transmit from host / host muted / mic to host).
	pub audio_tx: bool,
	pub audio_mute: bool,
	pub mic_on: bool,
	/// Chat log (me, text) — fed by the app over stdin (`chat in|out …`).
	pub chat: Vec<(bool, String)>,
	/// Host messages received while the Chat view wasn't open — drawn as a count
	/// badge on the overlay-open button; the backend zeroes it on Chat entry.
	pub chat_unread: usize,
	/// Whether the relayed Enter key fired this frame (sends the chat composer).
	pub chat_enter: bool,
	/// Recent host messages as (text, age in s) — drawn as bubbles beside the open button
	/// while the overlay is CLOSED (see [`draw_chat_bubbles`]); the backend expires them.
	pub chat_toasts: Vec<(String, f32)>,
	/// Remote file pane: current HOME-relative path + its entries (`fsjson` stdin).
	pub fs_remote_path: String,
	pub fs_remote: Vec<FsRow>,
	/// Connected controllers (slot, kind_label, device_name, uuid, target, rumble, disabled) —
	/// synced from the app over stdin (`ctrls slot:kind:name:uuid:target:rumble:disabled,...`).
	/// Game mode only. `target` "auto"/"xbox360"/"ds4"; `rumble` the per-pad level
	/// "off"/"weak"/"medium"/"strong"; `disabled` = the pad is toggled off. Legacy short lines
	/// default the missing tail.
	pub controllers: Vec<(u8, String, String, String, String, String, bool)>,
	/// SPLIT MODE: the set of pad uuids LOCKED to THIS session (this renderer process == one
	/// pane/session), parsed from the `ctrls` line's NEW 8th per-pad field (`…:disabled:locked`,
	/// `locked` = 1 when locked to this session). Drives the per-pad "Bu oturuma kilitle" toggle's
	/// checked state in `draw_controllers`. Kept SEPARATE from the `controllers` tuple so the
	/// platform `ctrls` parsers (linux.rs / win/mod.rs) can fill it without changing the long-lived
	/// 7-tuple's arity. Empty when split mode is off / before any lock.
	pub controllers_locked: std::collections::HashSet<String>,
	/// Available display MODES per host monitor (`idx → Vec<(w,h)>`, largest-first), parsed from
	/// the caps line's `displays=` field (Windows host only — empty elsewhere). Feeds the Stream
	/// view's resolution list (the captured monitor's REAL resolutions, not just fixed presets)
	/// and pairs with the Display view's "screen adaptation" toggle.
	pub display_modes: Vec<(u32, Vec<(u32, u32)>)>,
	/// SCREEN ADAPTATION (Parsec-style) toggle for THIS pane: when on, the host switches the
	/// captured monitor to the display mode that best fits the pane and reverts it on exit. Owned
	/// by the overlay once open; a change flows out as `OverlayCmd::Set("adapt", "<w>x<h>"|"off")`.
	pub adapt: bool,
}

/// Which overlay page is showing: the compact ROOT (category boxes) or a section.
/// The windowed app is often ~720p — the old single tall panel didn't fit, so the
/// overlay is now a small hub whose boxes open per-topic sub-views (AnyDesk-style).
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
	#[default]
	Root,
	Stream,      // codec/encoder/decoder/res/fps/bandwidth/quality/pacing
	Display,     // view-fit modes
	Audio,       // transmit / host-mute / mic / call
	Tools,       // clipboard / reverse / fullscreen / file-pick
	Chat,        // NATIVE chat (the webview menu is gone on Linux)
	Files,       // NATIVE two-pane file manager
	Gauges,      // stats HUD + overlay button toggles
	Controllers, // player-slot list + swap order (game mode only)
}

/// One row of a file listing (local pane = the renderer's own std::fs; remote pane
/// = the host's `FsEntries`, relayed by the app over stdin as one-line JSON).
#[derive(Clone)]
pub struct FsRow {
	pub name: String,
	pub dir: bool,
	pub size: u64,
}

/// Frame-persistent UI state the render loop owns (the overlay itself is immediate
/// mode): current page, the chat composer buffer, and the LOCAL file pane.
#[derive(Default)]
pub struct UiState {
	pub view: View,
	/// Mirror of `view` from the previous frame + a bump counter: a mismatch marks
	/// the frame a section was entered, (re)starting the fade/slide-in animation
	/// (the counter keys a fresh egui animation id per transition).
	pub anim_view: View,
	pub anim_gen: u64,
	/// Overlay open-state from the previous draw, so the enter animation can be
	/// (re)triggered on the closed→open edge even when the view is unchanged (e.g.
	/// reopening while the last view was Root — without this the fade never plays
	/// because anim_view already equals view). See `draw`.
	pub was_open: bool,
	pub chat_input: String,
	pub local_path: String, // HOME-relative ("" = home)
	pub local_rows: Vec<FsRow>,
	pub local_loaded: bool,
	/// Whether the Files view already asked the host for its initial listing —
	/// once per overlay open (backends reset it on the open edge), NOT per frame:
	/// an empty/failed reply must not loop `ov fsls` at frame rate.
	pub remote_requested: bool,
	/// Root-menu selection index for controller/keyboard nav. egui's own Tab-focus would
	/// not stick in this override-redirect overlay (focused() stayed None every frame), so
	/// the menu is driven by an explicit index instead: the pad's up/down move it, the
	/// highlighted box is `sel`, and A/Enter activates it.
	pub sel: usize,
	/// Selection index WITHIN a sub-view (Stream/Display/Controllers rows), driven by the pad
	/// up/down; left/right change the selected row's value. Reset to 0 whenever back on Root.
	pub sub_sel: usize,
	/// STAGED (un-applied) value for the selected value-row: `(sub_sel, value)`. A controller
	/// left/right stages it (visual only); A/activate confirms → it's applied
	/// (`OverlayCmd::Set`) and cleared; moving rows or changing view discards it. So a pad
	/// cycle never disrupts the live stream until the user confirms (mouse clicks on a value's
	/// seg button still apply immediately, per the "instant for mouse" rule).
	pub pending: Option<(usize, String)>,
	/// Whether the user has STARTED navigating with the keyboard/controller this overlay open.
	/// The Root-menu selection highlight stays HIDDEN until the FIRST directional/activate input,
	/// so a freshly-opened overlay shows nothing pre-selected (mouse users never see a stray
	/// cursor). Reset on every open edge; the first input reveals the cursor at box 0.
	pub nav_active: bool,
	/// Index in the chat log of the first message that was UNREAD when the Chat view was
	/// entered — the "— New —" divider goes above it. Set once per visit (messages arriving
	/// while the view is on screen are read on arrival and don't move it); cleared when the
	/// Chat view isn't on screen. See [`mark_new_chat`].
	pub chat_new_from: Option<usize>,
	/// Choices that RESTART the stream (codec, encoder, resolution, fps, quality, monitor,
	/// screen adaptation — [`RESTART_FIELDS`]), held while the overlay is open and applied when
	/// it closes: each one rebuilds the host encoder (and on Windows respawns this renderer), so
	/// applying them on click tore the picture down under the menu — and twice when the user
	/// changed two of them. The rows show the staged value meanwhile. Last choice per field wins.
	pub deferred: Vec<(&'static str, String)>,
	/// Root hub: the pad cursor is on headset volume bar N (above the boxes) instead of the
	/// box grid. None = on the grid.
	pub hp_sel: Option<usize>,
	/// The options page of a many-option row, open in place of the section's rows.
	pub picker: Option<Picker>,
	/// Inside a section the key / pad cursor sits on the header's category strip (one step
	/// above the first row): left/right walk the sections, down returns to the rows.
	pub tab_focus: bool,
	/// With `tab_focus`: the cursor is on the header's back button, not on a category.
	pub back_focus: bool,
}

/// An open options page: the row it belongs to and the option under the key / pad cursor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Picker {
	pub row: usize,
	pub hi: usize,
	/// Tiles per line, as the page was last drawn (0 before its first frame): the cursor is
	/// on the first line while `hi < cols`, and up from there reaches the back button.
	pub cols: usize,
}

/// One DualSense whose headphones carry the session audio (the app's `hpvol` line):
/// a volume bar on the Root hub. Slot and name come from the `ctrls` list by uuid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadsetBar {
	pub uuid: String,
	/// 0-100.
	pub volume: u8,
	/// The pad's light-bar colour, when the game set one.
	pub rgb: Option<[u8; 3]>,
}

static HEADSETS: std::sync::Mutex<Vec<HeadsetBar>> = std::sync::Mutex::new(Vec::new());

/// Parse `uuid:volume:rrggbb,...` (rgb `-` = unknown; empty = no pad routed).
pub fn parse_headsets(line: &str) -> Vec<HeadsetBar> {
	line.trim()
		.split(',')
		.filter_map(|e| {
			let mut p = e.splitn(3, ':');
			let uuid = p.next()?.trim();
			if uuid.is_empty() {
				return None;
			}
			let volume = p.next()?.parse::<u8>().ok()?.min(100);
			let rgb = p.next().and_then(|h| {
				let v = u32::from_str_radix(h, 16).ok().filter(|_| h.len() == 6)?;
				Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
			});
			Some(HeadsetBar {
				uuid: uuid.to_string(),
				volume,
				rgb,
			})
		})
		.collect()
}

/// The app's `hpvol <payload>` stdin line (both backends call this).
pub fn set_headsets(line: &str) {
	*HEADSETS.lock().unwrap_or_else(|e| e.into_inner()) = parse_headsets(line);
}

fn headsets() -> Vec<HeadsetBar> {
	HEADSETS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Optimistic local update while the user drags (the app echoes the level back).
fn set_headset_volume_local(uuid: &str, v: u8) {
	for b in HEADSETS.lock().unwrap_or_else(|e| e.into_inner()).iter_mut() {
		if b.uuid == uuid {
			b.volume = v;
		}
	}
}

/// Volume step per d-pad press.
const HEADSET_STEP: u8 = 5;

/// Overlay settings whose change restarts the stream; see [`UiState::deferred`].
pub const RESTART_FIELDS: [&str; 8] =
	["codec", "encoder", "res", "fps", "quality", "display", "adapt", "aquality"];

/// The session's audio-quality PREFERENCE (`auto`/`standard`/`high`/`raw`) for the Ses view:
/// seeded by the app's `audio … q=<v>` line, mirrored optimistically on a pick. One value for
/// every backend (the render thread only reads it).
static AUDIO_QUALITY: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
const AUDIO_QUALITIES: [&str; 4] = ["auto", "standard", "high", "raw"];

pub fn set_audio_quality(v: &str) {
	if let Some(i) = AUDIO_QUALITIES.iter().position(|q| *q == v) {
		AUDIO_QUALITY.store(i as u8, std::sync::atomic::Ordering::Relaxed);
	}
}

/// Relative-mouse sensitivity for the Görüntü view (`msens <x>` from the app; a pick mirrors
/// it). Same one-value-for-every-backend model as [`AUDIO_QUALITY`].
static MOUSE_SENS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(3);
const MOUSE_SENSES: [&str; 7] = ["0.25", "0.5", "0.75", "1", "1.25", "1.5", "2"];

/// Snap a multiplier to the nearest menu step.
pub fn set_mouse_sens(v: &str) {
	let Ok(x) = v.trim().parse::<f32>() else { return };
	let i = MOUSE_SENSES
		.iter()
		.enumerate()
		.min_by(|a, b| {
			let d = |s: &str| (s.parse::<f32>().unwrap_or(1.0) - x).abs();
			d(a.1).total_cmp(&d(b.1))
		})
		.map(|(i, _)| i)
		.unwrap_or(3);
	MOUSE_SENS.store(i as u8, std::sync::atomic::Ordering::Relaxed);
}

pub fn mouse_sens() -> &'static str {
	MOUSE_SENSES[(MOUSE_SENS.load(std::sync::atomic::Ordering::Relaxed) as usize).min(MOUSE_SENSES.len() - 1)]
}

/// "Serbest imleç" (remote mode, `ptrfree 0|1` from the app; the Görüntü toggle flips it
/// optimistically and emits `ov set ptrfree on|off`, which the app persists + applies).
static PTR_FREE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_ptr_free(on: bool) {
	PTR_FREE.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn ptr_free() -> bool {
	PTR_FREE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Host pointer marker (issue #7, remote mode): while the person AT the host uses their own
/// mouse the app sends `hostptr 1 <x> <y>` (normalized over the host screen; negative = the
/// position is unknown) and `hostptr 0` when it goes idle; `hostname <name>` labels it. The
/// backends map the position onto the video and call [`draw_host_pointer`], and show the
/// local cursor meanwhile so both pointers are visible. `hostptrshow 0|1` = the user's switch
/// ("Karşı tarafın imlecini göster", default on); the Görüntü row emits `ov set hostptr`.
struct HostPtr {
	pos: Option<(f32, f32)>,
	active: bool,
	at: std::time::Instant,
}
static HOST_PTR: std::sync::Mutex<Option<HostPtr>> = std::sync::Mutex::new(None);
static HOST_NAME: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
static HOST_PTR_SHOW: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
/// How long the marker fades out after the host mouse went idle.
const HOST_PTR_FADE: f32 = 1.5;

/// A `hostptr …` stdin line (the part after the keyword).
pub fn host_ptr_line(rest: &str) {
	let v: Vec<f32> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
	let mut g = HOST_PTR.lock().unwrap_or_else(|e| e.into_inner());
	if v.first().copied().unwrap_or(0.0) >= 1.0 {
		let pos = match (v.get(1), v.get(2)) {
			(Some(&x), Some(&y)) if x >= 0.0 && y >= 0.0 => Some((x.min(1.0), y.min(1.0))),
			_ => None,
		};
		*g = Some(HostPtr { pos, active: true, at: std::time::Instant::now() });
	} else if let Some(h) = g.as_mut() {
		if h.active {
			h.active = false;
			h.at = std::time::Instant::now();
		}
	}
}

pub fn set_host_name(name: &str) {
	*HOST_NAME.lock().unwrap_or_else(|e| e.into_inner()) = name.trim().to_string();
}

/// Picture-upscaling setting as the overlay shows it ("off"|"auto"|"fsr"|"ai"), mirrored from
/// the app's `upscale <mode>` line and the overlay's own pick.
static UPSCALE_MODE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
/// The upscaler the presenter is running right now ("" = none: the picture isn't enlarged, or
/// upscaling is off), e.g. "FSR", "NVIDIA VSR" — shown on the menu's stream line.
static UPSCALER_ACTIVE: std::sync::Mutex<&'static str> = std::sync::Mutex::new("");

pub fn set_upscale_mode(mode: &str) {
	*UPSCALE_MODE.lock().unwrap() = mode.trim().to_string();
}

fn upscale_mode() -> String {
	let m = UPSCALE_MODE.lock().unwrap();
	if m.is_empty() { "off".into() } else { m.clone() }
}

pub fn set_upscaler_active(name: &'static str) {
	*UPSCALER_ACTIVE.lock().unwrap() = name;
}

fn upscaler_active() -> &'static str {
	*UPSCALER_ACTIVE.lock().unwrap()
}

pub fn set_host_ptr_show(on: bool) {
	HOST_PTR_SHOW.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn host_ptr_show() -> bool {
	HOST_PTR_SHOW.load(std::sync::atomic::Ordering::Relaxed)
}

/// The host user is on their mouse right now (the backends un-hide the local cursor).
pub fn host_ptr_active(mode: Mode) -> bool {
	mode == Mode::Remote
		&& host_ptr_show()
		&& HOST_PTR.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|h| h.active)
}

/// What to draw this frame: the host position (normalized, if known) and the opacity, or
/// `None` (nothing: off, game mode, idle and faded out).
pub fn host_ptr_visible(mode: Mode) -> Option<(Option<(f32, f32)>, f32)> {
	if mode != Mode::Remote || !host_ptr_show() {
		return None;
	}
	let g = HOST_PTR.lock().unwrap_or_else(|e| e.into_inner());
	let h = g.as_ref()?;
	let alpha = if h.active {
		1.0
	} else {
		1.0 - h.at.elapsed().as_secs_f32() / HOST_PTR_FADE
	};
	(alpha > 0.0).then_some((h.pos, alpha))
}

/// Draw the host pointer marker at `at` (egui points; `None` = position unknown → the notice
/// only) plus the "host is using the mouse" notice, at opacity `alpha`.
pub fn draw_host_pointer(ctx: &egui::Context, at: Option<egui::Pos2>, alpha: f32) {
	let a = alpha.clamp(0.0, 1.0);
	const MARK: egui::Color32 = egui::Color32::from_rgb(255, 152, 48); // amber: not our indigo/cyan
	if let Some(p) = at {
		let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("pulsar_hostptr")));
		painter.circle_stroke(p, 11.0, egui::Stroke::new(2.0, MARK.gamma_multiply(a)));
		painter.circle_filled(p, 3.0, MARK.gamma_multiply(a));
		let name = HOST_NAME.lock().unwrap_or_else(|e| e.into_inner()).clone();
		let label = if name.is_empty() { t("hostptr.label").to_string() } else { format!("{} · {name}", t("hostptr.label")) };
		let font = egui::FontId::proportional(12.0);
		let galley = ctx.fonts_mut(|f| f.layout_no_wrap(label, font, egui::Color32::BLACK.gamma_multiply(a)));
		let screen = ctx.viewport_rect();
		let size = galley.size() + egui::vec2(14.0, 8.0);
		// Below-right of the pointer, kept on screen.
		let mut min = p + egui::vec2(14.0, 14.0);
		min.x = min.x.min(screen.right() - size.x - 4.0).max(screen.left() + 4.0);
		min.y = min.y.min(screen.bottom() - size.y - 4.0).max(screen.top() + 4.0);
		let rect = egui::Rect::from_min_size(min, size);
		painter.rect_filled(rect, 6.0, MARK.gamma_multiply(a));
		painter.galley(rect.min + egui::vec2(7.0, 4.0), galley, egui::Color32::BLACK);
	}
	egui::Area::new(egui::Id::new("pulsar_hostptr_notice"))
		.order(egui::Order::Foreground)
		.anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 14.0))
		.interactable(false)
		.show(ctx, |ui| {
			ui.set_opacity(a);
			egui::Frame::NONE
				.fill(egui::Color32::from_rgba_premultiplied(10, 11, 16, 215))
				.stroke(egui::Stroke::new(1.0, MARK))
				.corner_radius(8.0)
				.inner_margin(egui::Margin::symmetric(12, 6))
				.show(ui, |ui| {
					ui.add(
						egui::Label::new(
							egui::RichText::new(t("hostptr.notice")).size(12.5).color(egui::Color32::from_rgb(235, 225, 210)),
						)
						.wrap_mode(egui::TextWrapMode::Extend),
					);
				});
		});
}

/// Two-way clipboard sync switch (`audio clip=0|1` from the app; the Tools toggle flips it
/// optimistically and emits `ov set clipsync on|off` so the app persists + applies it).
static CLIP_SYNC: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub fn set_clip_sync(on: bool) {
	CLIP_SYNC.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn clip_sync() -> bool {
	CLIP_SYNC.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn audio_quality() -> &'static str {
	AUDIO_QUALITIES[(AUDIO_QUALITY.load(std::sync::atomic::Ordering::Relaxed) as usize).min(3)]
}

/// Stage a stream-restarting choice (replacing an earlier one for the same field).
fn stage(ui: &mut UiState, field: &'static str, val: String) {
	match ui.deferred.iter_mut().find(|(f, _)| *f == field) {
		Some(slot) => slot.1 = val,
		None => ui.deferred.push((field, val)),
	}
}

/// The staged choices as `Set` commands, emptying the queue. The backends call this on the
/// overlay's open→closed edge (the hotkey / button / app-side close), and `draw` itself when
/// it emits `Close`, so the restarts happen once the menu is out of the way.
pub fn take_deferred(ui: &mut UiState) -> Vec<OverlayCmd> {
	ui.deferred.drain(..).map(|(f, v)| OverlayCmd::Set(f, v)).collect()
}

/// The value a row shows: the staged choice when there is one, else the live one.
fn staged<'a>(deferred: &'a [(&'static str, String)], field: &str, live: &'a str) -> &'a str {
	deferred.iter().find(|(f, _)| *f == field).map(|(_, v)| v.as_str()).unwrap_or(live)
}

/// Per-frame bookkeeping for the chat's "— New —" divider. `in_chat`: the overlay is open
/// on the Chat view this frame; `unread`: host messages that were unread until now (the
/// backend's counter, which it zeroes while the Chat view is on screen).
pub fn mark_new_chat(ui: &mut UiState, in_chat: bool, chat_len: usize, unread: usize) {
	if !in_chat {
		ui.chat_new_from = None;
	} else if unread > 0 && ui.chat_new_from.is_none() {
		ui.chat_new_from = Some(chat_len.saturating_sub(unread));
	}
}

/// One frame's controller/keyboard nav input, read once in `draw` and handed to the active
/// view. Directions are plain egui arrow keys (the renderer relays the pad d-pad/stick as
/// arrows — Tab is avoided because egui's focus pass swallows it before a view can read it).
#[derive(Clone, Copy, Default)]
struct Nav {
	up: bool,
	down: bool,
	left: bool,
	right: bool,
	activate: bool,
}

impl Default for OverlayState {
	fn default() -> Self {
		Self {
			mode: Mode::Game,
			open: false,
			id: String::new(),
			conn_label: String::new(),
			fps: 0.0,
			latency_ms: 0.0,
			decode_ms: 0.0,
			mbps: 0.0,
			codec: "auto".into(),
			encoder: "auto".into(),
			host_codecs: Vec::new(),
			displays: Vec::new(),
			display_idx: 0,
			host_active: String::new(),
			stats_hud: false,
			cur_label: true,
			overlay_btn: true,
			btn_pos: BTN_POS_DEFAULT,
			host_encoders: Vec::new(),
			decoder: "auto".into(),
			res: "auto".into(),
			fps_sel: "auto".into(),
			bitrate: "0".into(),
			quality: "latency".into(),
			pace: false,
			fit: "fit".into(),
			audio_tx: true,
			audio_mute: false,
			mic_on: false,
			chat: Vec::new(),
			chat_unread: 0,
			chat_enter: false,
			chat_toasts: Vec::new(),
			fs_remote_path: String::new(),
			fs_remote: Vec::new(),
			controllers: Vec::new(), // Vec<(slot, kind_label, name, uuid, target, rumble)>
			controllers_locked: std::collections::HashSet::new(),
			display_modes: Vec::new(),
			adapt: false,
		}
	}
}

/// Emitted on interaction → serialized to stdout (`ov <field> <value>`) for the host.
pub enum OverlayCmd {
	Set(&'static str, String),
	End,
	Close,
	/// Send a chat line to the host (`ov chat <text>` — rest-of-line payload).
	Chat(String),
	/// Remote file-pane ops, HOME-relative paths (rest-of-line):
	/// `ov fsls <path>` list, `ov fsget <path>` download, `ov fssend <abs>` upload.
	FsLs(String),
	FsGet(String),
	FsSend(String),
	/// Open the app-side per-session file-manager WINDOW (`ov files`) — the overlay's
	/// Files box routes here instead of the cramped in-overlay two-pane view.
	OpenFiles,
}

fn codecs_opts() -> [(&'static str, &'static str); 4] {
	[
		("auto", t("auto")),
		("h264", "H.264"),
		("h265", "H.265"),
		("av1", "AV1"),
	]
}
fn encoders_opts() -> [(&'static str, &'static str); 8] {
	[
		("auto", t("auto")),
		("nvenc", "NVIDIA NVENC"),
		("qsv", "Intel QuickSync"),
		("amf", "AMD AMF"),
		("videotoolbox", "Apple VideoToolbox"),
		("vaapi", "VA-API"),
		("rkmpp", "Rockchip MPP"),
		("software", t("software")),
	]
}
fn res_opts() -> [(&'static str, &'static str); 5] {
	[
		("auto", t("auto")),
		("720p", "720p"),
		("1080p", "1080p"),
		("1440p", "1440p"),
		("4K", "4K"),
	]
}
fn fps_opts() -> [(&'static str, &'static str); 4] {
	[("auto", t("auto")), ("30", "30"), ("60", "60"), ("120", "120")]
}
fn bitrate_opts() -> [(&'static str, &'static str); 7] {
	[
		("0", t("auto")),
		("5", "5 Mbit"),
		("10", "10 Mbit"),
		("20", "20 Mbit"),
		("30", "30 Mbit"),
		("50", "50 Mbit"),
		("100", "100 Mbit"),
	]
}

const ACCENT: egui::Color32 = egui::Color32::from_rgb(124, 110, 245); // electric indigo
const CYAN: egui::Color32 = egui::Color32::from_rgb(120, 200, 240);

/// All overlay strings come from the central language catalogs (`lang/*.json`)
/// through the keyed lookup — see `i18n.rs`.
use crate::i18n::t;

/// Apply the Pulsar dark theme to an egui context (call once at startup).
/// Centered "switching screen…" indicator (spinner + label) drawn over the held last
/// frame while a monitor/codec switch waits for the new stream's first keyframe. Keeps
/// the user informed that the change is loading, not hung.
pub fn draw_switching(ctx: &egui::Context) {
	egui::Area::new(egui::Id::unique("switching"))
		// Background layer: drawn over the video but always UNDER the open menu window.
		.order(egui::Order::Background)
		.anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
		.show(ctx, |ui| {
			egui::Frame::NONE
				.fill(egui::Color32::from_rgba_unmultiplied(18, 20, 26, 230))
				.corner_radius(12.0)
				.inner_margin(egui::Margin::symmetric(22, 18))
				.show(ui, |ui| {
					ui.vertical_centered(|ui| {
						ui.add(egui::Spinner::new().size(28.0));
						ui.add_space(8.0);
						ui.label(
							egui::RichText::new(t("switching"))
								.size(14.0)
								.color(egui::Color32::WHITE),
						);
					});
				});
		});
}

/// Non-blocking "recovering connection" pill (spinner + label, top centre) drawn over the
/// frozen last frame while the session is alive but no decodable frame arrives — a keyframe /
/// RFI recovery wait on a lossy link. Unlike [`draw_stalled`] it covers nothing important and
/// takes no input: the picture resumes under it on its own.
pub fn draw_recovering(ctx: &egui::Context) {
	egui::Area::new(egui::Id::unique("recovering"))
		// Background layer: over the video but UNDER the open menu.
		.order(egui::Order::Background)
		.interactable(false)
		.anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 18.0))
		.show(ctx, |ui| {
			egui::Frame::NONE
				.fill(egui::Color32::from_rgba_unmultiplied(18, 20, 26, 215))
				.corner_radius(16.0)
				.inner_margin(egui::Margin::symmetric(14, 8))
				.show(ui, |ui| {
					ui.horizontal(|ui| {
						ui.add(egui::Spinner::new().size(14.0));
						ui.add_space(6.0);
						ui.label(
							egui::RichText::new(t("recovering"))
								.size(13.0)
								.color(egui::Color32::WHITE),
						);
					});
				});
		});
	// The spinner animates only while egui is asked to repaint.
	ctx.request_repaint();
}

/// Centered "stream stopped" indicator drawn over the frozen last frame when the stream
/// really stopped (Windows: the app's `stopped 1` or its stdin closed; Linux: the stall
/// detector — no fresh frame for ≥ 3 s with the keepalive stale too). Mirrors the
/// webview's `.stall` div that is permanently occluded by the native renderer window;
/// this surfaces the same message inside the renderer where the user can see it.
pub fn draw_stalled(ctx: &egui::Context) {
	egui::Area::new(egui::Id::unique("stalled"))
		// Background layer: over the video but UNDER the open menu (it used to cover it).
		.order(egui::Order::Background)
		.anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
		.show(ctx, |ui| {
			egui::Frame::NONE
				.fill(egui::Color32::from_rgba_unmultiplied(44, 18, 18, 230))
				.stroke(egui::Stroke::new(
					1.0,
					egui::Color32::from_rgba_unmultiplied(200, 60, 60, 180),
				))
				.corner_radius(12.0)
				.inner_margin(egui::Margin::symmetric(22, 18))
				.show(ui, |ui| {
					ui.set_max_width(320.0);
					ui.vertical_centered(|ui| {
						// Warning glyph as a PAINTED triangle + "!" (the ⚠ emoji is tofu in the
						// bundled font).
						let (wr, _) = ui.allocate_exact_size(egui::vec2(34.0, 30.0), egui::Sense::hover());
						let amber = egui::Color32::from_rgb(255, 160, 100);
						let c = wr.center();
						let h = 13.0;
						ui.painter().add(egui::Shape::convex_polygon(
							vec![
								egui::pos2(c.x, c.y - h),
								egui::pos2(c.x - h, c.y + h * 0.85),
								egui::pos2(c.x + h, c.y + h * 0.85),
							],
							egui::Color32::TRANSPARENT,
							egui::Stroke::new(2.2, amber),
						));
						ui.painter().text(
							egui::pos2(c.x, c.y + 2.0),
							egui::Align2::CENTER_CENTER,
							"!",
							egui::FontId::proportional(15.0),
							amber,
						);
						ui.add_space(6.0);
						ui.label(
							egui::RichText::new(t("stalled"))
								.size(13.5)
								.color(egui::Color32::from_rgb(255, 210, 200)),
						);
					});
				});
		});
}

/// Parse the `caps` line's `displays=idx:name:w:h:primary,…` field into the overlay's
/// `(idx, label)` monitor list (primary marked). Tolerant: skips malformed entries.
pub fn parse_displays(s: &str) -> Vec<(u32, String)> {
	if s.is_empty() {
		return Vec::new();
	}
	s.split(',')
		.filter_map(|e| {
			let mut p = e.split(':');
			let idx: u32 = p.next()?.parse().ok()?;
			let name = p.next()?;
			let w = p.next().unwrap_or("");
			let h = p.next().unwrap_or("");
			let primary = p.next() == Some("1");
			let mut label = name.to_string();
			if !w.is_empty() && !h.is_empty() {
				label.push_str(&format!(" {w}×{h}"));
			}
			if primary {
				label.push_str(&format!(" ({})", t("monitor.primary")));
			}
			Some((idx, label))
		})
		.collect()
}

pub fn apply_theme(ctx: &egui::Context) {
	// Install the Phosphor icon font into egui's fonts BEFORE any frame tessellates, so its
	// glyphs land in the font atlas. This is the single shared font-setup point — every backend
	// (Linux egui_glow, the Windows custom D3D11 painter `win/egui_paint.rs`, macOS eframe) calls
	// `apply_theme` on its fresh `egui::Context`, so all three get the icons. `add_to_fonts`
	// inserts "phosphor" at index 1 of the Proportional family, i.e. as a fallback for our
	// proportional text — so `PHOSPHOR_*` glyph constants render wherever we draw normal text.
	// Done by hand rather than via `egui_phosphor::add_to_fonts`: the newest egui-phosphor
	// (0.14) is built against egui 0.36, so its `FontDefinitions` is a different type from
	// our egui 0.37 one. Only the font bytes + the icon `&str` constants are used from it.
	let mut fonts = egui::FontDefinitions::default();
	fonts.font_data.insert(
		"phosphor".into(),
		std::sync::Arc::new(egui::FontData::from_static(
			egui_phosphor::Variant::Regular.font_bytes(),
		)),
	);
	if let Some(keys) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
		keys.insert(1, "phosphor".into());
	}
	ctx.set_fonts(fonts);

	let mut v = egui::Visuals::dark();
	pin_egui_029_look(&mut v);
	v.panel_fill = egui::Color32::from_rgba_premultiplied(13, 14, 20, 235);
	v.window_fill = egui::Color32::from_rgb(18, 20, 28);
	v.widgets.inactive.bg_fill = egui::Color32::from_rgb(28, 30, 42);
	v.widgets.hovered.bg_fill = egui::Color32::from_rgb(40, 42, 58);
	// Controller/keyboard focus indicator: egui paints a focused widget with the `active`
	// visuals AND a focus ring keyed off `selection.stroke`. The defaults were nearly
	// invisible on the dark panel, so the Tab-focused control inside sub-views looked dead.
	// A bold cyan stroke (matching the Root menu's selection border) makes it unmistakable.
	let focus_stroke = egui::Stroke::new(2.0, CYAN);
	v.selection.stroke = focus_stroke;
	v.widgets.active.bg_stroke = focus_stroke;
	v.widgets.active.weak_bg_fill = egui::Color32::from_rgb(34, 64, 82);
	v.widgets.active.bg_fill = egui::Color32::from_rgb(34, 64, 82);
	v.selection.bg_fill = ACCENT;
	v.override_text_color = Some(egui::Color32::from_rgb(228, 230, 240));
	ctx.set_visuals(v);
	let mut style = (*ctx.global_style()).clone();
	style.spacing.item_spacing = egui::vec2(10.0, 10.0);
	style.spacing.button_padding = egui::vec2(10.0, 6.0);
	// egui 0.29 defaults that later releases changed (the overlay was designed on 0.29):
	// faster animations, 18 pt widget rows, 12.5 pt body/button text, no ScrollArea edge fade.
	style.animation_time = 1.0 / 12.0;
	style.spacing.interact_size = egui::vec2(40.0, 18.0);
	style.spacing.scroll.fade.strength = 0.0;
	for (ts, size) in [
		(egui::TextStyle::Body, 12.5),
		(egui::TextStyle::Button, 12.5),
		(egui::TextStyle::Monospace, 12.0),
	] {
		if let Some(f) = style.text_styles.get_mut(&ts) {
			f.size = size;
		}
	}
	ctx.set_global_style(style);
}

/// Restore egui 0.29's dark-theme widget visuals (corner radii, fills, strokes, hover/press
/// expansion) that egui 0.37 changed, so the overlay keeps the look it was designed with.
/// `apply_theme` then layers the Pulsar colours on top, exactly as before the upgrade.
fn pin_egui_029_look(v: &mut egui::Visuals) {
	use egui::{Color32, CornerRadius, Stroke};
	let wv = |weak: u8, bg: u8, bg_stroke: Stroke, fg_stroke: Stroke, r: u8, expansion: f32| {
		egui::style::WidgetVisuals {
			weak_bg_fill: Color32::from_gray(weak),
			bg_fill: Color32::from_gray(bg),
			bg_stroke,
			fg_stroke,
			corner_radius: CornerRadius::same(r),
			expansion,
		}
	};
	let gray = |w: f32, g: u8| Stroke::new(w, Color32::from_gray(g));
	v.widgets.noninteractive = wv(27, 27, gray(1.0, 60), gray(1.0, 140), 2, 0.0);
	v.widgets.inactive = wv(60, 60, Stroke::NONE, gray(1.0, 180), 2, 0.0);
	v.widgets.hovered = wv(70, 70, gray(1.0, 150), gray(1.5, 240), 3, 1.0);
	let white = |w: f32| Stroke::new(w, Color32::WHITE);
	v.widgets.active = wv(55, 55, white(1.0), white(2.0), 2, 1.0);
	v.widgets.open = wv(45, 27, gray(1.0, 60), gray(1.0, 210), 2, 0.0);
	v.handle_shape = egui::style::HandleShape::Circle;
}

/// Draw the overlay for this frame; returns the commands the user triggered.
/// `ui_state` (page, chat composer, local file pane) is owned by the render loop
/// so it survives frames; the page resets when the overlay (re)opens.
pub fn draw(ctx: &egui::Context, st: &OverlayState, ui_state: &mut UiState) -> Vec<OverlayCmd> {
	let mut cmds = Vec::new();
	if !st.open {
		// Remember we're closed so the next open is detected as an edge below. Closing from
		// outside the overlay (hotkey, button, app): apply the staged restart choices now.
		if ui_state.was_open {
			cmds = take_deferred(ui_state);
		}
		ui_state.was_open = false;
		return cmds;
	}
	// Closed→open edge: force the enter animation to restart even if the view didn't
	// change since last open (the backends reset `view` to Root on open but the fade
	// is keyed off an anim_view != view mismatch — reopening on Root would otherwise
	// skip the fade). Resetting anim_view to a sentinel guarantees the mismatch fires.
	let open_edge = !ui_state.was_open;
	ui_state.was_open = true;
	// Fresh open starts the menu cursor on the first box — but HIDDEN until the user navigates
	// (nav_active gates the highlight, reset here so each open is ring-less until the first input).
	if open_edge {
		ui_state.sel = 0;
		ui_state.nav_active = false;
		// A fresh open always lands on the hub (main page), never the section it was closed
		// on — for every backend (Windows used not to reset it).
		ui_state.view = View::Root;
		ui_state.sub_sel = 0;
		ui_state.pending = None;
		ui_state.hp_sel = None;
		ui_state.picker = None;
		ui_state.tab_focus = false;
	}
	// B / Escape steps back one level: an options page returns to its rows, a sub-view to the
	// Root menu; from Root it closes the overlay. (The pad's B button is relayed as Escape —
	// see play.rs.)
	if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
		if ui_state.picker.take().is_some() {
			// back on the rows
		} else if ui_state.view != View::Root {
			ui_state.view = View::Root;
		} else {
			cmds.push(OverlayCmd::Close);
		}
	}
	// Dim scrim behind the panel; clicking it closes the overlay.
	egui::Area::new(egui::Id::unique("scrim"))
		.order(egui::Order::Background)
		.fixed_pos(egui::pos2(0.0, 0.0))
		.show(ctx, |ui| {
			let r = ctx.viewport_rect();
			ui.painter()
				.rect_filled(r, 0.0, egui::Color32::from_rgba_premultiplied(0, 0, 0, 120));
			if ui
				.interact(r, egui::Id::unique("scrim_click"), egui::Sense::click())
				.clicked()
			{
				cmds.push(OverlayCmd::Close);
			}
		});

	// The panel must FIT a windowed app (~720p): compact fixed width and per-topic
	// sub-views keep every page short (tallest ≈ 340 pt — under a 576 pt 720p
	// surface), so no scroll container is needed (an auto-height egui Window
	// collapses a ScrollArea to its first row).
	let frame = egui::Frame::window(&ctx.global_style()).inner_margin(egui::Margin::same(14));
	egui::Window::new("pulsar_overlay")
		.title_bar(false)
		.resizable(false)
		.collapsible(false)
		// A FIXED top edge: the panel is as tall as its page (hub, a section's rows, an options
		// page of two tiles), and centred it jumped up and down under the pointer with every
		// step — the back button and the strip were never where the last click left them.
		// The top sits where the tallest page ([`PANEL_REF_H`]) would be centred.
		.anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, panel_top(ctx.viewport_rect().height())))
		// `PANEL_W` pt of CONTENT width: in egui 0.37 `fixed_size` is the OUTER size (margins +
		// stroke included), so add the frame's margins back.
		.fixed_size(egui::vec2(PANEL_W + frame.total_margin().sum().x, 0.0))
		.frame(frame)
		.show(ctx, |ui| {
			// Header: the hub shows the brand, the mode and the transport; a section shows a
			// large back button and the category strip.
			if ui_state.view == View::Root {
				// As tall as a section's strip: the page under it starts at the same height on
				// the hub and in a section, so nothing shifts on the way in or back.
				ui.allocate_ui_with_layout(
					egui::vec2(ui.available_width(), HEADER_H),
					egui::Layout::left_to_right(egui::Align::Center),
					|ui| {
						ui.label(egui::RichText::new("Pulsar").size(20.0).color(CYAN).strong());
						let key = match st.mode {
							Mode::Game => "mode.game",
							Mode::Remote => "mode.remote",
						};
						ui.label(egui::RichText::new(t(key)).size(13.0).color(egui::Color32::from_gray(150)));
						ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
							// The transport (P2P / relay) in a well: something to read, not to click.
							if !st.conn_label.is_empty() {
								egui::Frame::new()
									.fill(SEG_BG)
									.corner_radius(8.0)
									.inner_margin(egui::Margin::symmetric(10, 5))
									.show(ui, |ui| {
										ui.label(
											egui::RichText::new(st.conn_label.clone())
												.monospace()
												.size(12.0)
												.color(egui::Color32::from_rgb(150, 155, 170)),
										);
									});
							}
						});
					},
				);
			} else {
				draw_section_header(ui, st, ui_state);
			}
			ui.add_space(6.0);

			// One read of the controller/keyboard nav for this frame, handed to the active view.
			let mut nav = ui.input(|i| Nav {
				up: i.key_pressed(egui::Key::ArrowUp),
				down: i.key_pressed(egui::Key::ArrowDown),
				left: i.key_pressed(egui::Key::ArrowLeft),
				right: i.key_pressed(egui::Key::ArrowRight),
				activate: i.key_pressed(egui::Key::Enter),
			});
			// #5: the FIRST nav input only WAKES the cursor (reveals it at box 0) without also
			// moving or activating — so the overlay opens with nothing selected and a single
			// d-pad tap just shows the cursor. Consume that input so it doesn't also step a row.
			let any_nav = nav.up || nav.down || nav.left || nav.right || nav.activate;
			if !ui_state.nav_active && any_nav {
				ui_state.nav_active = true;
				ui_state.sel = 0;
				ui_state.sub_sel = 0;
				// Headphone volume bars come first on the hub: the cursor wakes on the first.
				ui_state.hp_sel = (!headsets().is_empty()).then_some(0);
				nav = Nav::default();
			}
			// A mouse click hands the menu back to the pointer: the cursor ring hides (it would
			// stay on a row the user is no longer looking at) until the next key wakes it.
			if ui_state.nav_active && ui.input(|i| i.pointer.any_click()) {
				ui_state.nav_active = false;
				ui_state.tab_focus = false;
				ui_state.pending = None;
			}
			// The header is one step above a section's first row: up from row 0 puts the cursor
			// on the current category. Left / right there walk the sections, and left from the
			// first one reaches the BACK button (keys and a pad could not get to it: it was a
			// mouse-only button); down, or A on a category, returns to the rows; A on the back
			// button goes back one level. An options page has no strip to walk — up from its
			// first line goes straight to the back button.
			if ui_state.view == View::Root {
				ui_state.tab_focus = false;
			} else if ui_state.tab_focus {
				if ui_state.back_focus {
					if nav.activate {
						if ui_state.picker.take().is_none() {
							ui_state.view = View::Root;
						}
						ui_state.tab_focus = false;
					} else if nav.down {
						ui_state.tab_focus = false;
					} else if nav.right && ui_state.picker.is_none() {
						ui_state.back_focus = false;
					}
				} else {
					let tabs: Vec<View> = section_tabs(st.mode).map(|c| c.2).collect();
					if let Some(i) = tabs.iter().position(|v| *v == ui_state.view) {
						if nav.left {
							if i > 0 {
								ui_state.view = tabs[i - 1];
							} else {
								ui_state.back_focus = true;
							}
						} else if nav.right && i + 1 < tabs.len() {
							ui_state.view = tabs[i + 1];
						}
					}
					if nav.down || nav.activate {
						ui_state.tab_focus = false;
					}
				}
				nav = Nav::default();
			} else if nav.up {
				let (top, paging) = match ui_state.picker {
					Some(p) => (p.cols > 0 && p.hi < p.cols, true),
					None => (ui_state.sub_sel == 0 && has_rows(ui_state.view), false),
				};
				if top {
					ui_state.tab_focus = true;
					ui_state.back_focus = paging;
					nav = Nav::default();
				}
			}

			// Section enter/leave animation: a quick fade + downward slide-in instead of
			// the old instant swap. The transition restarts on every view change (the
			// bumped counter keys a fresh animation id, primed at 0 the first frame).
			if ui_state.anim_view != ui_state.view || open_edge {
				ui_state.anim_view = ui_state.view;
				ui_state.anim_gen = ui_state.anim_gen.wrapping_add(1);
				// A section starts with its cursor on the first row, nothing staged, no
				// options page (the strip moves between sections without a stop at the hub).
				ui_state.sub_sel = 0;
				ui_state.pending = None;
				ui_state.picker = None;
				ui.ctx().animate_value_with_time(
					egui::Id::unique(("ov-view-fade", ui_state.anim_gen)),
					0.0,
					0.0,
				);
			}
			let fade = ui.ctx().animate_value_with_time(
				egui::Id::unique(("ov-view-fade", ui_state.anim_gen)),
				1.0,
				0.16,
			);
			// Entering any sub-view always starts its row cursor at the top.
			if ui_state.view == View::Root {
				ui_state.sub_sel = 0;
				ui_state.pending = None;
			}
			// Moving the row cursor DISCARDS any staged-but-unconfirmed value (revert) —
			// "başka yere geçerse onaylamadan geri alınmalı". Confirm is A/activate (handled
			// inside choice_row), which is not up/down so it survives.
			if nav.up || nav.down {
				ui_state.pending = None;
			}
			// The key / pad cursor shows once it woke, and on the rows unless it is on the strip.
			let cursor = ui_state.nav_active && !ui_state.tab_focus;
			ui.scope(|ui| {
				ui.set_opacity(fade);
				ui.add_space((1.0 - fade) * 8.0);
				let view = ui_state.view;
				match view {
					View::Root => draw_root(
						ui,
						st,
						&mut ui_state.view,
						&mut ui_state.sel,
						&mut ui_state.hp_sel,
						nav,
						ui_state.nav_active,
						&mut cmds,
					),
					View::Chat => draw_chat(ui, st, &mut ui_state.chat_input, ui_state.chat_new_from, &mut cmds),
					View::Files => draw_files(ui, st, ui_state, &mut cmds),
					_ => {
						// A section's rows scroll when they are taller than the surface (a
						// windowed 720p app is ~576 pt high; four controllers are far more).
						// The cap is what the surface leaves under the panel's fixed top once
						// the header, the footer and the frame are taken off (`PANEL_CHROME_H`).
						// `min_scrolled_height` = the cap: this auto-height window offers the
						// area no height of its own, and it would collapse to one row.
						let vh = ui.ctx().viewport_rect().height();
						let max_h = (vh - panel_top(vh) - PANEL_CHROME_H).max(160.0);
						let out = egui::ScrollArea::vertical()
							.id_salt(("ov-rows", ui_state.anim_gen))
							.max_height(max_h)
							.min_scrolled_height(max_h)
							.auto_shrink([false, true])
							.show(ui, |ui| {
								// Room for the cursor ring of the first / last row (2 pt wider
								// than the row) inside the area's clip.
								ui.add_space(3.0);
								let input = RowsIn {
									sub_sel: &mut ui_state.sub_sel,
									nav,
									cursor,
									pending: &mut ui_state.pending,
									picker: &mut ui_state.picker,
								};
								match view {
									View::Stream => draw_stream(ui, st, &ui_state.deferred, input, &mut cmds),
									View::Display => draw_display(ui, st, &ui_state.deferred, input, &mut cmds),
									View::Audio => draw_audio(ui, st, &ui_state.deferred, input, &mut cmds),
									View::Tools => draw_tools(ui, st.mode, input, &mut cmds),
									View::Gauges => draw_gauges(ui, st, input, &mut cmds),
									View::Controllers => draw_controllers(ui, st, input, &mut cmds),
									View::Root | View::Chat | View::Files => {}
								}
								ui.add_space(3.0);
							});
						// More rows above / below the visible part: fade that edge into the panel.
						let hidden_above = out.state.offset().y;
						let hidden_below = out.content_size.y - out.inner_rect.height() - out.state.offset().y;
						let bg = ui.ctx().global_style().visuals.window_fill;
						if hidden_above > 1.0 {
							paint_fade(ui.painter(), out.inner_rect, true, bg);
						}
						if hidden_below > 1.0 {
							paint_fade(ui.painter(), out.inner_rect, false, bg);
						}
					}
				}
			});
			ui.add_space(6.0);
			// Footer: the hub (and the typing views) list the global shortcuts, a section how
			// keys / a pad drive its rows.
			let hint = if !has_rows(ui_state.view) {
				t("shortcuts")
			} else if ui_state.picker.is_some() {
				t("hint.options")
			} else {
				t("hint.rows")
			};
			ui.label(
				egui::RichText::new(hint)
				.monospace()
				.small()
				.color(egui::Color32::from_rgb(120, 125, 140)),
			);
		});
	// Stream-restarting choices wait for the menu to close (see `UiState::deferred`): hold them
	// here; a Close from inside the overlay applies them right before closing, End drops them.
	let mut out = Vec::with_capacity(cmds.len());
	let mut closing = false;
	for c in cmds {
		match c {
			OverlayCmd::Set(f, v) if RESTART_FIELDS.contains(&f) => stage(ui_state, f, v),
			OverlayCmd::Close => closing = true,
			OverlayCmd::End => {
				ui_state.deferred.clear();
				out.push(OverlayCmd::End);
			}
			other => out.push(other),
		}
	}
	if closing {
		out.extend(take_deferred(ui_state));
		out.push(OverlayCmd::Close);
	}
	out
}

/// Content width of the overlay panel (pt). 2026-11-13: 360 was too narrow for the stream line
/// and the growing rows; 520 still fits a windowed 720p app (≈ 1280 pt wide).
const PANEL_W: f32 = 520.0;
/// Gap between the tiles of the stat and category grids (pt).
const GRID_GAP: f32 = 8.0;
/// Height of the tallest page (pt, frame included): the remote Stream section's eight rows.
const PANEL_REF_H: f32 = 540.0;

/// What a section's panel has around its rows (pt): frame margins (28), the header strip and
/// the gap under it (42), the footer line and its gap (24), the enter slide (8) and a gap to
/// the bottom of the surface (24).
const PANEL_CHROME_H: f32 = 126.0;

/// The panel's top edge on a surface `viewport_h` pt high: where a [`PANEL_REF_H`] panel would
/// be centred, and never closer than 12 pt to the top.
fn panel_top(viewport_h: f32) -> f32 {
	((viewport_h - PANEL_REF_H) / 2.0).max(12.0)
}

/// Category grid columns: remote's 8 boxes in 2 rows of 4, game's 6 in 2 rows of 3.
fn grid_cols(n: usize) -> usize {
	if n % 4 == 0 { 4 } else { 3 }
}

/// Move the Root-menu selection over a `grid_cols`-column grid of `n` boxes plus an End-session button
/// at index `n` (its own full-width row below the grid). left/right step within a grid row;
/// up/down step between rows, with the bottom grid row going to End and End going back up.
fn move_grid_sel(sel: usize, n: usize, nav: Nav) -> usize {
	let cols = grid_cols(n);
	let end = n;
	let mut s = sel.min(end);
	if nav.right && s < n && s % cols < cols - 1 && s + 1 < n {
		s += 1;
	}
	if nav.left && s < n && s % cols > 0 {
		s -= 1;
	}
	if nav.down {
		if s < n {
			let nx = s + cols;
			s = if nx < n { nx } else { end };
		}
		// From End there is nowhere lower — stay.
	}
	if nav.up {
		if s == end {
			// Jump back to the first cell of the last grid row.
			s = if n == 0 { 0 } else { ((n - 1) / cols) * cols };
		} else if s >= cols {
			s -= cols;
		}
	}
	s
}

/// Root hub: compact stat tiles + the category boxes + end-session.
#[allow(clippy::too_many_arguments)]
fn draw_root(
	ui: &mut egui::Ui,
	st: &OverlayState,
	view: &mut View,
	sel: &mut usize,
	hp_sel: &mut Option<usize>,
	nav: Nav,
	nav_active: bool,
	cmds: &mut Vec<OverlayCmd>,
) {
	let (enc_ms, target_mbit) = host_parts(st);
	let bw = fmt_mbps(st.mbps);
	// Two compact tile rows: RTT · fps · encode / decode · BITRATE target · bandwidth.
	ui.spacing_mut().item_spacing = egui::vec2(GRID_GAP, GRID_GAP);
	ui.horizontal(|ui| {
		stat_tile(ui, &format!("{:.0}", st.latency_ms), icon::PULSE, t("stat.latency"));
		stat_tile(ui, &format!("{:.0}", st.fps), icon::FILM_STRIP, "FPS");
		stat_tile(
			ui,
			&enc_ms.map_or("—".into(), |v: f32| format!("{v:.1}")),
			icon::CPU,
			t("stat.encode"),
		);
	});
	ui.horizontal(|ui| {
		stat_tile(ui, &format!("{:.1}", st.decode_ms), icon::LIGHTNING, t("stat.decode"));
		stat_tile(
			ui,
			&target_mbit.map_or("—".into(), |v: f32| format!("{v:.0}")),
			icon::TARGET,
			"Bitrate Mbps",
		);
		stat_tile(ui, &bw, icon::CELL_SIGNAL_HIGH, t("stat.band"));
	});
	// What is on the wire: codec · host encoder · resolution · the upscaler drawing it (when the
	// picture is enlarged) · target fps (ⓘ) · host monitor, e.g.
	// "H.265 · NVENC · 1080p · FSR · 120 fps ⓘ · Display 2".
	let gray = egui::Color32::from_gray(150);
	let head = [stream_codec(st), stream_encoder(st), stream_res_label(), upscaler_active().to_string()]
		.into_iter()
		.filter(|p| !p.is_empty())
		.collect::<Vec<_>>()
		.join(" · ");
	let fps = stream_target_fps(st);
	let display = if st.displays.is_empty() { String::new() } else { format!("Display {}", st.display_idx + 1) };
	ui.horizontal(|ui| {
		ui.spacing_mut().item_spacing.x = 0.0;
		ui.label(egui::RichText::new(&head).size(11.0).color(gray));
		if !fps.is_empty() {
			ui.label(egui::RichText::new(format!(" · {fps} ")).size(11.0).color(gray));
			info_hover_below(ui, t("stat.fpsTargetHint"));
		}
		if !display.is_empty() {
			ui.label(egui::RichText::new(format!(" · {display}")).size(11.0).color(gray));
		}
	});
	ui.add_space(6.0);

	// DualSense headphone volume bars (one per pad whose headphones carry the session audio).
	// The pad cursor visits them before the box grid: up/down move between bars (down from
	// the last one enters the grid, up from the grid's top row comes back), left/right change
	// the focused bar's level. The mouse drags or clicks a bar. Applied live.
	let bars = headsets();
	if hp_sel.is_some_and(|i| i >= bars.len()) {
		*hp_sel = bars.len().checked_sub(1);
	}
	let mut nav = nav;
	if let Some(i) = *hp_sel {
		let moved = Nav::default();
		if nav.up && i > 0 {
			*hp_sel = Some(i - 1);
		} else if nav.down {
			if i + 1 < bars.len() {
				*hp_sel = Some(i + 1);
			} else {
				*hp_sel = None;
				*sel = 0;
			}
		}
		let step = |v: u8, up: bool| {
			if up {
				v.saturating_add(HEADSET_STEP).min(100)
			} else {
				v.saturating_sub(HEADSET_STEP)
			}
		};
		if let Some(b) = bars.get(i) {
			if nav.left || nav.right {
				let v = step(b.volume, nav.right);
				if v != b.volume {
					set_headset_volume_local(&b.uuid, v);
					cmds.push(OverlayCmd::Set("hpvol", format!("{},{v}", b.uuid)));
				}
			}
		}
		nav = moved;
	} else if !bars.is_empty() && nav.up && *sel < 2 {
		*hp_sel = Some(bars.len() - 1);
		nav = Nav::default();
	}
	if !bars.is_empty() {
		ui.label(
			egui::RichText::new(t("headset.volume"))
				.size(11.0)
				.color(egui::Color32::from_gray(150)),
		);
		for (i, b) in bars.iter().enumerate() {
			let label = match st.controllers.iter().find(|c| c.3 == b.uuid) {
				Some((slot, _, name, ..)) => format!("{} {} · {}", t("controllers.slot"), slot + 1, name),
				None => "DualSense".to_string(),
			};
			if let Some(v) = headset_bar(ui, b, &label, nav_active && *hp_sel == Some(i)) {
				set_headset_volume_local(&b.uuid, v);
				cmds.push(OverlayCmd::Set("hpvol", format!("{},{v}", b.uuid)));
				*hp_sel = Some(i);
			}
		}
		ui.add_space(6.0);
	}
	let nav_active_grid = nav_active && hp_sel.is_none();

	// Category boxes, 2 per row (AnyDesk-style hub). Chat + Files are NATIVE views
	// now — the webview session menu is gone on Linux (it lived under the video).
	// Game mode shows the slim set; Audio + Tools were added on the maintainer's request —
	// in-game audio control (host audio / speakers / mic) is essential, and Tools carries the
	// fullscreen toggle (game mode trims Tools to JUST fullscreen, see draw_tools). Remote mode
	// gets the full eight boxes incl. file transfer, chat, mic, monitor.
	let boxes = categories(st.mode);
	let n = boxes.len();
	// Selection cycles over the n boxes PLUS the End-session button at index n, so the pad
	// reaches End too.
	let total = n + 1;
	// 2D grid nav (matches the visible 2-column layout): left/right move within a row, up/down
	// move between rows; the End-session button (index n) sits in its own row below the grid.
	let activate = nav.activate;
	if *sel >= total {
		*sel = 0;
	}
	*sel = move_grid_sel(*sel, n, nav);
	let cols = grid_cols(n);
	egui::Grid::new("ov_boxes")
		.num_columns(cols)
		.spacing(egui::vec2(GRID_GAP, GRID_GAP))
		.show(ui, |ui| {
			for (i, (icon, key, v)) in boxes.iter().enumerate() {
				// Unread host messages also show on the Chat box (not only on the closed-state
				// open button), until the Chat view is opened.
				let badge = if *v == View::Chat { st.chat_unread } else { 0 };
				let resp = cat_box(ui, icon, t(key), nav_active_grid && i == *sel, badge, cols);
				if resp.clicked() || (activate && i == *sel) {
					// Files opens the app's dedicated per-session window — the in-overlay
					// two-pane view was too cramped (kept as code for a potential fallback).
					if *v == View::Files {
						cmds.push(OverlayCmd::OpenFiles);
					} else {
						*view = *v;
					}
				}
				if i % cols == cols - 1 {
					ui.end_row();
				}
			}
		});
	ui.add_space(10.0);
	// End-session button = the last nav item (index n). Cyan stroke when the pad cursor is on
	// it; A/Enter (activate) ends the session, same as a click. Highlight only once nav woke.
	let end_selected = nav_active_grid && *sel == n;
	// A row like a section's: full width, row height, the cursor ring — in red, calm at rest
	// and bright under the pointer / the cursor (it was a 32 pt bar in full red at all times).
	let (rect, resp) =
		ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h(ui.ctx())), egui::Sense::click());
	let lit = end_selected || resp.hovered();
	if end_selected {
		ui.painter().rect_filled(rect.expand(2.0), 10.0, CYAN);
	}
	ui.painter().rect_filled(rect, 8.0, if lit { DANGER } else { DANGER_DIM });
	let label = t("end");
	let col = if lit { egui::Color32::WHITE } else { egui::Color32::from_rgb(255, 214, 216) };
	let x0 = rect.center().x - (26.0 + text_w(ui, label, 13.5)) / 2.0;
	ui.painter().text(
		egui::pos2(x0 + 9.0, rect.center().y),
		egui::Align2::CENTER_CENTER,
		icon::SIGN_OUT,
		egui::FontId::proportional(17.0),
		col,
	);
	ui.painter().text(
		egui::pos2(x0 + 26.0, rect.center().y),
		egui::Align2::LEFT_CENTER,
		label,
		egui::FontId::proportional(13.5),
		col,
	);
	let clicked = resp.clicked();
	resp.on_hover_cursor(egui::CursorIcon::PointingHand);
	if clicked || (activate && end_selected) {
		cmds.push(OverlayCmd::End);
	}
}

/// One headphone volume bar: a light-bar swatch + the pad's label + the level, then a
/// draggable track. Returns the new level when the mouse set one.
fn headset_bar(ui: &mut egui::Ui, b: &HeadsetBar, label: &str, selected: bool) -> Option<u8> {
	let w = ui.available_width();
	let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 34.0), egui::Sense::click_and_drag());
	let p = ui.painter();
	if selected {
		p.rect_filled(rect.expand(2.0), 10.0, CYAN);
		p.rect_filled(rect, 8.0, ROW_SEL);
	} else {
		p.rect_filled(rect, 8.0, if resp.hovered() { ROW_HOVER } else { ROW_BG });
	}
	let top = rect.top() + 10.0;
	let swatch = match b.rgb {
		Some([r, g, bl]) => egui::Color32::from_rgb(r, g, bl),
		None => egui::Color32::from_gray(90),
	};
	p.circle_filled(egui::pos2(rect.left() + 12.0, top), 4.5, swatch);
	let text_col = if selected { CYAN } else { egui::Color32::from_rgb(228, 230, 240) };
	p.text(
		egui::pos2(rect.left() + 22.0, top),
		egui::Align2::LEFT_CENTER,
		label,
		egui::FontId::proportional(12.0),
		text_col,
	);
	p.text(
		egui::pos2(rect.right() - 10.0, top),
		egui::Align2::RIGHT_CENTER,
		format!("{} %", b.volume),
		egui::FontId::monospace(11.5),
		text_col,
	);
	let track = egui::Rect::from_min_max(
		egui::pos2(rect.left() + 10.0, rect.bottom() - 11.0),
		egui::pos2(rect.right() - 10.0, rect.bottom() - 6.0),
	);
	p.rect_filled(track, 2.5, egui::Color32::from_rgb(62, 66, 84));
	let frac = b.volume as f32 / 100.0;
	let fill = egui::Rect::from_min_max(
		track.min,
		egui::pos2(track.left() + track.width() * frac, track.bottom()),
	);
	p.rect_filled(fill, 2.5, ACCENT);
	p.circle_filled(egui::pos2(fill.right(), track.center().y), 5.0, egui::Color32::WHITE);
	if resp.hovered() {
		resp.clone().on_hover_cursor(egui::CursorIcon::PointingHand);
	}
	if resp.dragged() || resp.clicked() {
		let x = resp.interact_pointer_pos()?.x;
		let v = (((x - track.left()) / track.width()).clamp(0.0, 1.0) * 100.0).round() as u8;
		return (v != b.volume).then_some(v);
	}
	None
}

/// The hub's category boxes per mode, in display order: icon, label key, section. Game mode
/// shows the slim set; Audio + Tools were added on the maintainer's request — in-game audio
/// control (host audio / speakers / mic) is essential, and Tools carries the fullscreen toggle
/// (game mode trims Tools to JUST fullscreen, see draw_tools). Remote mode gets the full eight
/// incl. file transfer, chat, mic, monitor. Chat + Files are NATIVE views.
fn categories(mode: Mode) -> &'static [(&'static str, &'static str, View)] {
	match mode {
		Mode::Game => &[
			(icon::BROADCAST, "view.stream", View::Stream),
			(icon::MONITOR, "view.display", View::Display),
			(icon::SPEAKER_HIGH, "view.audio", View::Audio),
			(icon::CHART_BAR, "view.gauges", View::Gauges),
			(icon::GAME_CONTROLLER, "view.controllers", View::Controllers),
			(icon::WRENCH, "view.tools", View::Tools),
		],
		Mode::Remote => &[
			(icon::BROADCAST, "view.stream", View::Stream),
			(icon::MONITOR, "view.display", View::Display),
			(icon::SPEAKER_HIGH, "view.audio", View::Audio),
			(icon::CHAT_CIRCLE, "view.chat", View::Chat),
			(icon::FOLDER, "view.files", View::Files),
			(icon::WRENCH, "view.tools", View::Tools),
			(icon::GAME_CONTROLLER, "view.controllers", View::Controllers),
			(icon::CHART_BAR, "view.gauges", View::Gauges),
		],
	}
}

/// The sections the header strip lists: every category but Files (it opens its own window from
/// the hub and has no page here).
fn section_tabs(mode: Mode) -> impl Iterator<Item = &'static (&'static str, &'static str, View)> {
	categories(mode).iter().filter(|c| c.2 != View::Files)
}

/// Sections made of rows (cursor, options pages, the scroll area) — not the typing views.
fn has_rows(view: View) -> bool {
	matches!(
		view,
		View::Stream | View::Display | View::Audio | View::Tools | View::Gauges | View::Controllers
	)
}

/// A section's header: a large back button and the category strip. One click on a strip entry —
/// or, with keys / a pad, up from the first row and then left/right — moves between sections
/// without a trip through the hub. (The old header was a 20 pt arrow and nothing else: every
/// change of section was back, aim, click.)
fn draw_section_header(ui: &mut egui::Ui, st: &OverlayState, us: &mut UiState) {
	const H: f32 = HEADER_H;
	ui.horizontal(|ui| {
		ui.spacing_mut().item_spacing.x = 4.0;
		// Back: from an options page to its rows, from the rows to the hub.
		let (rect, resp) = ui.allocate_exact_size(egui::vec2(44.0, H), egui::Sense::click());
		let hov = resp.hovered();
		// The cursor on it: the ring and plate every other cursor stop has.
		let focus = us.nav_active && us.tab_focus && us.back_focus;
		if focus {
			ui.painter().rect_filled(rect.expand(2.0), 10.0, CYAN);
		}
		let fill = if focus {
			ROW_SEL
		} else if hov {
			ROW_HOVER
		} else {
			ROW_BG
		};
		ui.painter().rect_filled(rect, 8.0, fill);
		{
			// PAINTED arrow — the "←" glyph is missing from egui's bundled fonts.
			let col = if hov || focus { egui::Color32::WHITE } else { egui::Color32::from_gray(190) };
			let c = rect.center();
			let s = egui::Stroke::new(2.0, col);
			let p = ui.painter();
			p.line_segment([c + egui::vec2(-7.0, 0.0), c + egui::vec2(7.0, 0.0)], s);
			p.line_segment([c + egui::vec2(-7.0, 0.0), c + egui::vec2(-1.5, -5.5)], s);
			p.line_segment([c + egui::vec2(-7.0, 0.0), c + egui::vec2(-1.5, 5.5)], s);
		}
		if resp.clicked() && us.picker.take().is_none() {
			us.view = View::Root;
		}
		resp.on_hover_cursor(egui::CursorIcon::PointingHand);
		ui.add_space(4.0);
		for (icon, key, v) in section_tabs(st.mode) {
			let active = *v == us.view;
			let label = t(key);
			// The current section shows its name; the others are an icon (the name on hover).
			let w = if active { 38.0 + text_w(ui, label, 13.0) + 12.0 } else { 40.0 };
			let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, H), egui::Sense::click());
			let hov = resp.hovered();
			if active && us.nav_active && us.tab_focus && !us.back_focus {
				ui.painter().rect_filled(rect.expand(2.0), 10.0, CYAN);
			}
			// The category you are in looks like the category the cursor is on in the hub:
			// the same plate (`ROW_SEL`), cyan icon, white name — the hub's box, minus the
			// ring (the ring is the cursor, and it is on the rows). It was indigo, the colour
			// of a VALUE in force, so the hub and the strip marked "this category" in two
			// different colours.
			let fill = if active {
				ROW_SEL
			} else if hov {
				ROW_HOVER
			} else {
				ROW_BG
			};
			ui.painter().rect_filled(rect, 8.0, fill);
			if active {
				ui.painter().rect_stroke(
					rect,
					8.0,
					egui::Stroke::new(1.0, CYAN.gamma_multiply(0.45)),
					egui::StrokeKind::Inside,
				);
			}
			ui.painter().text(
				egui::pos2(rect.left() + 20.0, rect.center().y),
				egui::Align2::CENTER_CENTER,
				*icon,
				egui::FontId::proportional(18.0),
				if hov && !active { egui::Color32::WHITE } else { CYAN },
			);
			if active {
				ui.painter().text(
					egui::pos2(rect.left() + 38.0, rect.center().y),
					egui::Align2::LEFT_CENTER,
					label,
					egui::FontId::proportional(13.0),
					egui::Color32::WHITE,
				);
			} else if *v == View::Chat && st.chat_unread > 0 {
				ui.painter().circle_filled(
					egui::pos2(rect.right() - 8.0, rect.top() + 8.0),
					4.0,
					egui::Color32::from_rgb(225, 60, 70),
				);
			}
			if resp.clicked() && !active {
				us.view = *v;
				us.picker = None;
			}
			let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
			if !active {
				resp.on_hover_text(egui::RichText::new(label).size(12.0));
			}
		}
	});
}

/// Stream section: codec / encoder / resolution / fps / bitrate limit / quality / pacing.
fn draw_stream(
	ui: &mut egui::Ui,
	st: &OverlayState,
	deferred: &[(&'static str, String)],
	input: RowsIn<'_>,
	cmds: &mut Vec<OverlayCmd>,
) {
	// Capability gating: only offer what the HOST really has (empty = unknown host → show
	// everything; it degrades gracefully server-side anyway).
	let codecs: Vec<(&str, &str)> = codecs_opts()
		.iter()
		.filter(|(v, _)| {
			*v == "auto" || st.host_codecs.is_empty() || st.host_codecs.iter().any(|c| c == v)
		})
		.copied()
		.collect();
	let encoders: Vec<(&str, &str)> = encoders_opts()
		.iter()
		.filter(|(v, _)| {
			*v == "auto" || st.host_encoders.is_empty() || st.host_encoders.iter().any(|c| c == v)
		})
		.copied()
		.collect();
	// Resolution options: when the host advertised the CAPTURED monitor's real display modes
	// (Windows), list THOSE — so the picker reflects the actual screen and pairs with the Display
	// view's "screen adaptation" feature — otherwise the fixed presets. Real modes use a "WxH"
	// value (`controls.setRes` parses it into explicit width/height); presets keep their tokens.
	let mode_strs: Vec<(String, String)> = st
		.display_modes
		.iter()
		.find(|(idx, _)| *idx == st.display_idx)
		.map(|(_, modes)| {
			modes
				.iter()
				.map(|(w, h)| (format!("{w}x{h}"), format!("{w}×{h}")))
				.collect()
		})
		.unwrap_or_default();
	let res_owned: Vec<(&str, &str)> = if mode_strs.is_empty() {
		res_opts().to_vec()
	} else {
		let mut v = vec![("auto", t("auto"))];
		v.extend(mode_strs.iter().map(|(val, disp)| (val.as_str(), disp.as_str())));
		v
	};
	let res: &[(&str, &str)] = &res_owned;
	let fps = fps_opts();
	let br = bitrate_opts();
	let quality = [("latency", t("lowlat")), ("quality", t("quality"))];
	let onoff = [("on", t("on")), ("off", t("off"))];
	let pace_cur = if st.pace { "on" } else { "off" };

	// Remote mode adds the "show my pointer on the host" row (game sessions never label it).
	let remote = st.mode == Mode::Remote;
	let mut rows = Rows::new(if remote { 8 } else { 7 }, input);
	ui.spacing_mut().item_spacing.y = ROW_GAP;
	if let Some(v) = rows.choice(ui, 0, "Codec", staged(deferred, "codec", &st.codec), &codecs, None) {
		cmds.push(OverlayCmd::Set("codec", v));
	}
	if let Some(v) = rows.choice(ui, 1, "Encoder", staged(deferred, "encoder", &st.encoder), &encoders, None) {
		cmds.push(OverlayCmd::Set("encoder", v));
	}
	if let Some(v) = rows.choice(ui, 2, t("res"), staged(deferred, "res", &st.res), res, None) {
		cmds.push(OverlayCmd::Set("res", v));
	}
	if let Some(v) = rows.choice(ui, 3, "FPS", staged(deferred, "fps", &st.fps_sel), &fps, None) {
		cmds.push(OverlayCmd::Set("fps", v));
	}
	// The bitrate LIMIT (the adaptive controller's ceiling; it still goes lower on a weak link).
	if let Some(v) = rows.choice(ui, 4, t("bwlimit"), &st.bitrate, &br, Some(t("info.bitrate"))) {
		cmds.push(OverlayCmd::Set("bitrate", v));
	}
	if let Some(v) = rows.choice(ui, 5, t("quality"), staged(deferred, "quality", &st.quality), &quality, None) {
		cmds.push(OverlayCmd::Set("quality", v));
	}
	if let Some(v) = rows.choice(ui, 6, t("pacing"), pace_cur, &onoff, Some(t("info.pacing"))) {
		cmds.push(OverlayCmd::Set("pace", v));
	}
	if remote {
		let cur = if st.cur_label { "on" } else { "off" };
		if let Some(v) = rows.choice(ui, 7, t("curlabel"), cur, &onoff, Some(t("info.curlabel"))) {
			cmds.push(OverlayCmd::Set("curlabel", v));
		}
	}
	if rows.picking() {
		return;
	}
	pending_note(ui, deferred);
	// Decoder is auto-selected by this renderer — display only.
	ui.add_space(4.0);
	ui.label(
		egui::RichText::new(format!(
			"Decoder · {}",
			if st.decoder.is_empty() {
				t("auto")
			} else {
				st.decoder.as_str()
			}
		))
		.size(11.0)
		.color(egui::Color32::from_gray(150)),
	);
}

/// Display section: host monitor picker (when the host has >1 display, BOTH modes) +
/// AnyDesk-style view-fit modes (renderer-local, instant), picture upscaling, screen
/// adaptation, mouse sensitivity and (remote) the pointer rows.
fn draw_display(
	ui: &mut egui::Ui,
	st: &OverlayState,
	deferred: &[(&'static str, String)],
	input: RowsIn<'_>,
	cmds: &mut Vec<OverlayCmd>,
) {
	// Rows: the host monitor picker (only when the host exposes >1 display), the view-fit mode,
	// upscaling, the "screen adaptation" toggle, mouse sensitivity.
	let multi = st.displays.len() > 1;
	// Remote mode adds the free-cursor row (game mode always keeps the cursor in the video)
	// and the host-pointer row (issue #7) right after it.
	let free_row = st.mode == Mode::Remote;
	let mut rows = Rows::new(4 + usize::from(multi) + 2 * usize::from(free_row), input);
	ui.spacing_mut().item_spacing.y = ROW_GAP;
	let mut row = 0;
	if multi {
		let idx_strs: Vec<String> = st.displays.iter().map(|(i, _)| i.to_string()).collect();
		let opts: Vec<(&str, &str)> = st
			.displays
			.iter()
			.enumerate()
			.map(|(k, (_, label))| (idx_strs[k].as_str(), label.as_str()))
			.collect();
		let live = st.display_idx.to_string();
		let cur = staged(deferred, "display", &live);
		if let Some(v) = rows.choice(ui, row, t("monitor.title"), cur, &opts, None) {
			cmds.push(OverlayCmd::Set("display", v));
		}
		row += 1;
	}
	let fit = [
		("fit", t("fit.fit")),
		("stretch", t("fit.stretch")),
		("original", t("fit.original")),
	];
	if let Some(v) = rows.choice(ui, row, t("fit.title"), &st.fit, &fit, Some(t("fit.help"))) {
		cmds.push(OverlayCmd::Set("fit", v));
	}
	row += 1;
	// Picture upscaling (same setting as Settings → Görüntü → Görüntü iyileştirme): applied here
	// at once and forwarded so the app saves it.
	let up = [
		("off", t("upscale.off")),
		("auto", t("upscale.auto")),
		("fsr", "FSR"),
		("ai", t("upscale.ai")),
	];
	let up_live = upscale_mode();
	let up_cur = staged(deferred, "upscale", &up_live);
	if let Some(v) = rows.choice(ui, row, t("upscale.title"), up_cur, &up, None) {
		set_upscale_mode(&v);
		cmds.push(OverlayCmd::Set("upscale", v));
	}
	row += 1;
	// SCREEN ADAPTATION (Parsec-style): when ON, the host switches the captured monitor to the
	// display mode that best fits THIS pane and reverts on exit — unlike "stretch" (which only
	// scales the frame), this changes the host's real resolution so the geometry is native to the
	// pane. The pane size = this renderer's own window (egui screen rect × pixels-per-point), sent
	// so the host can pick the closest mode.
	let onoff = [("on", t("on")), ("off", t("off"))];
	let adapt_cur = match deferred.iter().find(|(f, _)| *f == "adapt") {
		Some((_, v)) => if v == "off" { "off" } else { "on" },
		None => if st.adapt { "on" } else { "off" },
	};
	if let Some(v) = rows.choice(ui, row, t("display.adapt"), adapt_cur, &onoff, Some(t("info.adapt"))) {
		if v == "on" {
			let sr = ui.ctx().viewport_rect();
			let ppp = ui.ctx().pixels_per_point();
			let w = (sr.width() * ppp).round() as u32;
			let h = (sr.height() * ppp).round() as u32;
			cmds.push(OverlayCmd::Set("adapt", format!("{w}x{h}")));
		} else {
			cmds.push(OverlayCmd::Set("adapt", "off".into()));
		}
	}
	row += 1;
	// Mouse sensitivity for relative motion (games that grabbed the mouse): a multiplier on the
	// device's raw deltas, applied by the app. Instant — no stream restart.
	let senses: Vec<(&str, String)> = MOUSE_SENSES.iter().map(|v| (*v, format!("{v}×"))).collect();
	let sense_opts: Vec<(&str, &str)> = senses.iter().map(|(v, l)| (*v, l.as_str())).collect();
	if let Some(v) = rows.choice(ui, row, t("mouse.sens"), mouse_sens(), &sense_opts, Some(t("info.msens"))) {
		cmds.push(OverlayCmd::Set("msens", v));
	}
	row += 1;
	if free_row {
		let cur = if ptr_free() { "on" } else { "off" };
		if let Some(v) = rows.choice(ui, row, t("mouse.free"), cur, &onoff, Some(t("info.mfree"))) {
			set_ptr_free(v == "on");
			cmds.push(OverlayCmd::Set("ptrfree", v));
		}
		row += 1;
		let cur = if host_ptr_show() { "on" } else { "off" };
		if let Some(v) = rows.choice(ui, row, t("hostptr.show"), cur, &onoff, Some(t("info.hostptr"))) {
			set_host_ptr_show(v == "on");
			cmds.push(OverlayCmd::Set("hostptr", v));
		}
	}
	if rows.picking() {
		return;
	}
	pending_note(ui, deferred);
}

/// "Applied when the menu closes" under the rows while stream-restarting choices are staged.
fn pending_note(ui: &mut egui::Ui, deferred: &[(&'static str, String)]) {
	if deferred.is_empty() {
		return;
	}
	ui.add_space(4.0);
	ui.label(egui::RichText::new(t("pending.apply")).size(11.0).color(ACCENT));
}

/// Audio section: voice call / host transmit / host speakers / mic to host / quality.
///
/// "Sesli görüşme" = both directions at once (host audio here + our mic there); the host already
/// plays inbound mic audio, so it's a paired toggle. SEMANTIC (must match the backends' `call`
/// handler): turning the call ON enables BOTH mic and host audio; OFF drops ONLY the mic and
/// leaves host audio as the user had it — so its on/off derives from `mic_on` ALONE.
fn draw_audio(
	ui: &mut egui::Ui,
	st: &OverlayState,
	deferred: &[(&'static str, String)],
	input: RowsIn<'_>,
	cmds: &mut Vec<OverlayCmd>,
) {
	let onoff = [("on", t("on")), ("off", t("off"))];
	let mut rows = Rows::new(5, input);
	ui.spacing_mut().item_spacing.y = ROW_GAP;
	let call = if st.mic_on { "on" } else { "off" };
	if let Some(v) = rows.choice(ui, 0, t("audio.call"), call, &onoff, Some(t("info.call"))) {
		cmds.push(OverlayCmd::Set("call", v));
	}
	let host = if st.audio_tx { "on" } else { "off" };
	if let Some(v) = rows.choice(ui, 1, t("audio.host"), host, &onoff, None) {
		cmds.push(OverlayCmd::Set("atx", v));
	}
	// Speakers ON = not muted; the emitted `amute` is the inverse.
	let spk = if st.audio_mute { "off" } else { "on" };
	if let Some(v) = rows.choice(ui, 2, t("audio.speakers"), spk, &onoff, None) {
		cmds.push(OverlayCmd::Set("amute", if v == "on" { "off".into() } else { "on".into() }));
	}
	let mic = if st.mic_on { "on" } else { "off" };
	if let Some(v) = rows.choice(ui, 3, t("audio.mic"), mic, &onoff, None) {
		cmds.push(OverlayCmd::Set("mic", v));
	}
	// Audio quality — a stream restart, so it waits for the menu to close (RESTART_FIELDS).
	let qualities = [
		("auto", t("aq.auto")),
		("standard", t("aq.standard")),
		("high", t("aq.high")),
		("raw", t("aq.raw")),
	];
	let q = staged(deferred, "aquality", audio_quality());
	if let Some(v) = rows.choice(ui, 4, t("audio.quality"), q, &qualities, Some(t("info.aquality"))) {
		cmds.push(OverlayCmd::Set("aquality", v));
	}
	if rows.picking() {
		return;
	}
	pending_note(ui, deferred);
}

/// Tools section. GAME mode trims this to JUST the fullscreen toggle (clipboard / file pick /
/// reverse are remote-desktop concerns — irrelevant in-game; maintainer's request). REMOTE mode
/// has the clipboard sync switch and the one-shot actions (clipboard push, quick file pick,
/// reverse direction, fullscreen) — rows like every other section, so keys / a pad reach them
/// too (they were plain mouse buttons). Chat + the file manager have their own native views.
fn draw_tools(ui: &mut egui::Ui, mode: Mode, input: RowsIn<'_>, cmds: &mut Vec<OverlayCmd>) {
	ui.spacing_mut().item_spacing.y = ROW_GAP;
	if mode == Mode::Game {
		let mut rows = Rows::new(1, input);
		if rows.action(ui, 0, t("tools.fullscreen"), CYAN, true) {
			cmds.push(OverlayCmd::Set("fullscreen", "1".into()));
		}
		return;
	}
	let mut rows = Rows::new(5, input);
	let onoff = [("on", t("on")), ("off", t("off"))];
	let sync = if clip_sync() { "on" } else { "off" };
	if let Some(v) = rows.choice(ui, 0, t("tools.clipsync"), sync, &onoff, None) {
		set_clip_sync(v == "on");
		cmds.push(OverlayCmd::Set("clipsync", v));
	}
	if rows.action(ui, 1, t("tools.clip"), CYAN, true) {
		cmds.push(OverlayCmd::Set("sendclip", "1".into()));
	}
	if rows.action(ui, 2, t("tools.file"), CYAN, true) {
		// Handled Rust-side (rfd) in render_stats.rs — no webview activation needed.
		cmds.push(OverlayCmd::Set("pickfile", "1".into()));
	}
	if rows.action(ui, 3, t("tools.reverse"), CYAN, true) {
		cmds.push(OverlayCmd::Set("reverse", "1".into()));
	}
	if rows.action(ui, 4, t("tools.fullscreen"), CYAN, true) {
		cmds.push(OverlayCmd::Set("fullscreen", "1".into()));
	}
}

/// NATIVE chat: the session conversation + a composer. Typing arrives as relayed
/// key events (the app's webview captures keydowns while the overlay is open and
/// pipes them over stdin — this child window can't take X focus without killing
/// the focus-gated combos).
fn draw_chat(
	ui: &mut egui::Ui,
	st: &OverlayState,
	input: &mut String,
	new_from: Option<usize>,
	cmds: &mut Vec<OverlayCmd>,
) {
	// An auto-height egui Window gives a ScrollArea only its minimum scrolled height (it
	// showed ~2 lines) — pin min = max so the conversation gets a real column. 300 pt keeps
	// the whole Chat page (header + log + composer + shortcuts) inside a 720p surface.
	egui::ScrollArea::vertical()
		.scroll_source(egui::scroll_area::ScrollSource::ALL) // 0.29 default: drag-to-scroll with the mouse too
		.min_scrolled_height(300.0)
		.max_height(300.0)
		.auto_shrink([false, false])
		.stick_to_bottom(true)
		.show(ui, |ui| {
			if st.chat.is_empty() {
				ui.add_space(130.0);
				ui.vertical_centered(|ui| {
					ui.label(
						egui::RichText::new(t("chat.empty")).color(egui::Color32::from_gray(120)),
					);
				});
				return;
			}
			for (i, (me, text)) in st.chat.iter().enumerate() {
				if new_from == Some(i) {
					new_divider(ui);
				}
				let layout = if *me {
					egui::Layout::right_to_left(egui::Align::Min)
				} else {
					egui::Layout::left_to_right(egui::Align::Min)
				};
				ui.with_layout(layout, |ui| {
					let fill = if *me {
						ACCENT
					} else {
						egui::Color32::from_rgb(34, 36, 50)
					};
					egui::Frame::NONE
						.fill(fill)
						.corner_radius(9.0)
						.inner_margin(egui::Margin::symmetric(10, 6))
						.show(ui, |ui| {
							ui.set_max_width(250.0);
							ui.label(
								egui::RichText::new(text)
									.size(13.0)
									.color(egui::Color32::WHITE),
							);
						});
				});
				ui.add_space(2.0);
			}
		});
	ui.add_space(6.0);
	let send_now = st.chat_enter && !input.trim().is_empty();
	ui.horizontal(|ui| {
		let resp = ui.add_sized(
			egui::vec2(ui.available_width() - 76.0, 28.0),
			egui::TextEdit::singleline(input)
				.hint_text(t("chat.placeholder"))
				// egui 0.29's text placement (top-left, 5×3 incl. the 1 px stroke); 0.37
				// centres single-line text in the box with a theme margin instead.
				.margin(egui::Margin::symmetric(5, 3))
				.align(egui::Align2::LEFT_TOP),
		);
		// Keep the composer focused while this view is up so relayed Text events land.
		resp.request_focus();
		let clicked = ui
			.add(egui::Button::new(t("chat.send")).min_size(egui::vec2(68.0, 28.0)))
			.clicked();
		if (clicked || send_now) && !input.trim().is_empty() {
			cmds.push(OverlayCmd::Chat(input.trim().to_string()));
			input.clear();
		}
	});
}

/// "———— New ————" above the first message that was unread when the Chat view opened.
fn new_divider(ui: &mut egui::Ui) {
	ui.add_space(4.0);
	let red = egui::Color32::from_rgb(235, 90, 100);
	let label = egui::WidgetText::from(egui::RichText::new(t("chat.new")).size(11.0).strong().color(red));
	let galley = label.into_galley(ui, Some(egui::TextWrapMode::Extend), f32::INFINITY, egui::TextStyle::Body);
	let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), galley.size().y + 2.0), egui::Sense::hover());
	let c = rect.center();
	let half = galley.size().x / 2.0 + 8.0;
	let stroke = egui::Stroke::new(1.0, red.gamma_multiply(0.6));
	ui.painter().line_segment([egui::pos2(rect.left(), c.y), egui::pos2(c.x - half, c.y)], stroke);
	ui.painter().line_segment([egui::pos2(c.x + half, c.y), egui::pos2(rect.right(), c.y)], stroke);
	ui.painter().galley(c - galley.size() / 2.0, galley, red);
	ui.add_space(4.0);
}

/// NATIVE two-pane file manager: LEFT = this machine (renderer-local std::fs,
/// starting at HOME), RIGHT = the host (FsEntries relayed over stdin). Download
/// from the right, upload from the left — both go through the session's existing
/// chunked file channel.
fn draw_files(ui: &mut egui::Ui, st: &OverlayState, us: &mut UiState, cmds: &mut Vec<OverlayCmd>) {
	if !us.local_loaded {
		load_local(us);
	}
	// First visit: ask the host for its HOME listing — exactly once per overlay open
	// (an empty or lost reply must not re-emit `ov fsls` every frame).
	if !us.remote_requested && st.fs_remote_path.is_empty() && st.fs_remote.is_empty() {
		us.remote_requested = true;
		cmds.push(OverlayCmd::FsLs(String::new()));
	}
	let pane_w = (ui.available_width() - 8.0) / 2.0;
	ui.horizontal_top(|ui| {
		// LOCAL pane.
		ui.vertical(|ui| {
			ui.set_width(pane_w);
			ui.label(
				egui::RichText::new(format!("{} · ~/{}", t("files.local"), us.local_path))
					.small()
					.color(egui::Color32::GRAY),
			);
			let mut nav: Option<String> = None;
			let mut send: Option<String> = None;
			egui::ScrollArea::vertical()
				.scroll_source(egui::scroll_area::ScrollSource::ALL) // 0.29 default: drag-to-scroll with the mouse too
				.id_salt("fs_local")
				.max_height(220.0)
				.auto_shrink([false, false])
				.show(ui, |ui| {
					if !us.local_path.is_empty()
						&& fs_row(ui, &format!(".. {}", t("files.up")), true, 0, None)
					{
						let mut p = us.local_path.clone();
						if let Some(i) = p.rfind('/') {
							p.truncate(i);
						} else {
							p.clear();
						}
						nav = Some(p);
					}
					for row in &us.local_rows {
						let action = (!row.dir).then_some(">");
						if fs_row(ui, &row.name, row.dir, row.size, action) {
							if row.dir {
								nav = Some(join_rel(&us.local_path, &row.name));
							} else {
								send = Some(join_rel(&us.local_path, &row.name));
							}
						}
					}
				});
			if let Some(p) = nav {
				us.local_path = p;
				load_local(us);
			}
			if let Some(rel) = send {
				if let Some(home) = home() {
					let abs = std::path::Path::new(&home).join(&rel);
					cmds.push(OverlayCmd::FsSend(abs.to_string_lossy().into_owned()));
				}
			}
		});
		ui.add_space(8.0);
		// REMOTE pane.
		ui.vertical(|ui| {
			ui.set_width(pane_w);
			ui.label(
				egui::RichText::new(format!("{} · ~/{}", t("files.remote"), st.fs_remote_path))
				.small()
				.color(egui::Color32::GRAY),
			);
			egui::ScrollArea::vertical()
				.scroll_source(egui::scroll_area::ScrollSource::ALL) // 0.29 default: drag-to-scroll with the mouse too
				.id_salt("fs_remote")
				.max_height(220.0)
				.auto_shrink([false, false])
				.show(ui, |ui| {
					if !st.fs_remote_path.is_empty()
						&& fs_row(ui, &format!(".. {}", t("files.up")), true, 0, None)
					{
						let mut p = st.fs_remote_path.clone();
						if let Some(i) = p.rfind('/') {
							p.truncate(i);
						} else {
							p.clear();
						}
						cmds.push(OverlayCmd::FsLs(p));
					}
					for row in &st.fs_remote {
						let action = (!row.dir).then_some("<");
						if fs_row(ui, &row.name, row.dir, row.size, action) {
							let p = join_rel(&st.fs_remote_path, &row.name);
							if row.dir {
								cmds.push(OverlayCmd::FsLs(p));
							} else {
								cmds.push(OverlayCmd::FsGet(p));
							}
						}
					}
				});
		});
	});
	ui.add_space(4.0);
	ui.label(
		egui::RichText::new(t("files.help"))
		.size(10.0)
		.color(egui::Color32::from_gray(110)),
	);
}

/// One file row; returns true when clicked. `action` renders a trailing marker
/// (">" upload / "<" download) for files. Dirs get a trailing "/" — all ASCII so
/// nothing tofus in the bundled egui font.
fn fs_row(ui: &mut egui::Ui, name: &str, dir: bool, size: u64, action: Option<&str>) -> bool {
	let label = if dir {
		format!("{name}/")
	} else {
		format!("{name}  ·  {}", fmt_size(size))
	};
	let text = egui::RichText::new(if let Some(a) = action {
		format!("{label}   {a}")
	} else {
		label
	})
	.size(12.0);
	ui.add(
		egui::Button::new(text)
			.frame(false)
			.min_size(egui::vec2(ui.available_width(), 20.0)),
	)
	.clicked()
}

fn fmt_size(b: u64) -> String {
	if b >= 1_048_576 {
		format!("{:.1} MB", b as f64 / 1_048_576.0)
	} else if b >= 1024 {
		format!("{:.0} KB", b as f64 / 1024.0)
	} else {
		format!("{b} B")
	}
}

fn join_rel(base: &str, name: &str) -> String {
	if base.is_empty() {
		name.to_string()
	} else {
		format!("{base}/{name}")
	}
}

/// The user's home dir for the LOCAL pane: HOME on unix, USERPROFILE on Windows
/// (where HOME is normally unset — without the fallback the pane stays empty).
fn home() -> Option<std::ffi::OsString> {
	std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
}

/// (Re)load the LOCAL pane from the renderer's own filesystem (HOME-relative).
fn load_local(us: &mut UiState) {
	us.local_loaded = true;
	us.local_rows.clear();
	let Some(home) = home() else { return };
	let dir = std::path::Path::new(&home).join(&us.local_path);
	let Ok(rd) = std::fs::read_dir(dir) else {
		return;
	};
	for e in rd.flatten() {
		let name = e.file_name().to_string_lossy().into_owned();
		if name.starts_with('.') {
			continue; // dotfiles add noise in a 220 pt pane
		}
		let Ok(meta) = e.metadata() else { continue };
		us.local_rows.push(FsRow {
			name,
			dir: meta.is_dir(),
			size: meta.len(),
		});
	}
	us.local_rows.sort_by(|a, b| {
		b.dir
			.cmp(&a.dir)
			.then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
	});
}

/// Gauges section: the always-on HUD + the Parsec-style open button toggles.
fn draw_gauges(ui: &mut egui::Ui, st: &OverlayState, input: RowsIn<'_>, cmds: &mut Vec<OverlayCmd>) {
	let onoff = [("on", t("on")), ("off", t("off"))];
	let mut rows = Rows::new(2, input);
	ui.spacing_mut().item_spacing.y = ROW_GAP;
	let hud = if st.stats_hud { "on" } else { "off" };
	if let Some(v) = rows.choice(ui, 0, t("gauges.statshud"), hud, &onoff, None) {
		cmds.push(OverlayCmd::Set("statshud", v));
	}
	let ovb = if st.overlay_btn { "on" } else { "off" };
	if let Some(v) = rows.choice(ui, 1, t("gauges.ovbtn"), ovb, &onoff, None) {
		cmds.push(OverlayCmd::Set("ovbtn", v));
	}
}

/// Controllers section: the ordered player-slot list. Per pad: a header with the two
/// move-up / move-down buttons (`ctrlswap` "{i},{to}"), the emulation target (`ctrlemu`
/// "{uuid},{target}"), vibration (`ctrlrumble`), a test pulse (`ctrltest`), enabled
/// (`ctrldisable`) and "lock to this session" (`ctrllock`).
fn draw_controllers(ui: &mut egui::Ui, st: &OverlayState, input: RowsIn<'_>, cmds: &mut Vec<OverlayCmd>) {
	if st.controllers.is_empty() {
		ui.add_space(20.0);
		ui.vertical_centered(|ui| {
			ui.label(
				egui::RichText::new(t("controllers.empty"))
					.color(egui::Color32::from_gray(120)),
			);
		});
		return;
	}
	let n = st.controllers.len();
	// Each pad = 6 rows: [header/reorder, emulation, vibration, test, status, lock]. up/down move
	// the cursor across ALL pads' rows; left/right change the focused row (or move the pad, on
	// the header row); A confirms a staged value (or fires the test pulse).
	const PER: usize = 6;
	let mut rows = Rows::new(n * PER, input);

	let emu_opts: &[(&str, &str)] = &[
		("auto", t("controllers.emuAuto")),
		("xbox360", t("controllers.emuXbox")),
		("ds4", t("controllers.emuDs4")),
	];
	let rumble_opts: &[(&str, &str)] = &[
		("off", t("controllers.rumbleOff")),
		("weak", t("controllers.rumbleWeak")),
		("medium", t("controllers.rumbleMedium")),
		("strong", t("controllers.rumbleStrong")),
	];
	let status_opts: &[(&str, &str)] = &[
		("on", t("controllers.enabled")),
		("off", t("controllers.disabled")),
	];
	// SPLIT MODE: per-pad "lock to THIS session" toggle options. "Kilitli" = forward only to this
	// pane regardless of focus; "Serbest" = follow the focused pane (default).
	let lock_opts: &[(&str, &str)] = &[
		("on", t("controllers.lockOn")),
		("off", t("controllers.lockOff")),
	];

	ui.spacing_mut().item_spacing.y = ROW_GAP;
	for (i, (slot, kind, name, uuid, target, rumble, disabled)) in st.controllers.iter().enumerate() {
		let base = i * PER;
		// --- Row 0: the pad ("Oyuncu N · kind · name") and its place in the player order: the
		// two buttons (or left/right on the row) swap it with the pad above / below; the cursor
		// follows the moved pad. ---
		if let Some((hrect, _, hsel)) = rows.frame(ui, base, row_h(ui.ctx())) {
			let hdr = format!("{} {} · {} · {}", t("controllers.slot"), slot + 1, kind, name);
			let hcol = if *disabled {
				egui::Color32::from_gray(110)
			} else if hsel {
				CYAN
			} else {
				egui::Color32::from_rgb(228, 230, 240)
			};
			let cy = hrect.center().y;
			ui.painter().text(
				egui::pos2(hrect.left() + 14.0, cy),
				egui::Align2::LEFT_CENTER,
				&hdr,
				egui::FontId::proportional(13.5),
				hcol,
			);
			let mut swap_to = None;
			for up in [true, false] {
				let x = hrect.right() - if up { 68.0 } else { 28.0 };
				let r = egui::Rect::from_center_size(egui::pos2(x, cy), egui::vec2(36.0, 28.0));
				let can = if up { i > 0 } else { i + 1 < n };
				let bresp = ui.interact(r, ui.make_persistent_id(("ov-ctrl-move", i, up)), egui::Sense::click());
				let hov = can && bresp.hovered();
				ui.painter().rect_filled(r, 6.0, if hov { ACCENT_DIM } else { SEG_BG });
				let col = if !can {
					egui::Color32::from_gray(70)
				} else if hov {
					egui::Color32::WHITE
				} else {
					CYAN
				};
				paint_tri_v(ui.painter(), r.center(), 4.5, up, col);
				if can && bresp.clicked() {
					swap_to = Some(if up { i - 1 } else { i + 1 });
				}
				if hov {
					bresp.on_hover_cursor(egui::CursorIcon::PointingHand);
				}
			}
			if hsel && rows.nav.left && i > 0 {
				swap_to = Some(i - 1);
			} else if hsel && rows.nav.right && i + 1 < n {
				swap_to = Some(i + 1);
			}
			if let Some(to) = swap_to {
				cmds.push(OverlayCmd::Set("ctrlswap", format!("{i},{to}")));
				rows.move_cursor(to * PER);
			}
		}

		// --- Rows 1-2: emulation, vibration. ---
		if let Some(v) = rows.choice(ui, base + 1, t("controllers.emulation"), target, emu_opts, None) {
			if !uuid.is_empty() {
				cmds.push(OverlayCmd::Set("ctrlemu", format!("{uuid},{v}")));
			}
		}
		if let Some(v) = rows.choice(ui, base + 2, t("controllers.rumble"), rumble, rumble_opts, None) {
			if !uuid.is_empty() {
				cmds.push(OverlayCmd::Set("ctrlrumble", format!("{uuid},{v}")));
			}
		}
		// Test row — fire a one-shot rumble pulse at the pad's CURRENT level so the player can feel
		// it. Disabled when vibration is off (would do nothing) or the pad has no uuid.
		let test_on = !uuid.is_empty() && rumble != "off";
		let test_label = if test_on { t("controllers.test") } else { t("controllers.testOff") };
		if rows.action(ui, base + 3, test_label, CYAN, test_on) {
			cmds.push(OverlayCmd::Set("ctrltest", uuid.clone()));
		}
		let status_cur = if *disabled { "off" } else { "on" };
		if let Some(v) = rows.choice(ui, base + 4, t("controllers.status"), status_cur, status_opts, None) {
			if !uuid.is_empty() {
				let dis = if v == "off" { 1 } else { 0 };
				cmds.push(OverlayCmd::Set("ctrldisable", format!("{uuid},{dis}")));
			}
		}
		// --- Row 5 (SPLIT MODE): "Bu oturuma kilitle" — lock this pad to THIS session/pane so it
		// forwards only here regardless of which pane has input focus. Checked state comes from the
		// `ctrls` line's per-pad lock field (st.controllers_locked, filled by the platform parser).
		// Emits `ctrllock` "<uuid>,1|0" — mirroring `ctrldisable` exactly. ---
		let lock_cur = if st.controllers_locked.contains(uuid) { "on" } else { "off" };
		if let Some(v) = rows.choice(ui, base + 5, t("controllers.lock"), lock_cur, lock_opts, None) {
			if !uuid.is_empty() {
				let locked = if v == "on" { 1 } else { 0 };
				cmds.push(OverlayCmd::Set("ctrllock", format!("{uuid},{locked}")));
			}
		}
		if i + 1 < n && !rows.picking() {
			ui.add_space(6.0);
		}
	}
}

/// Parse the host's Stats label parts we surface as tiles: encode pace (optional
/// 6th part) and the TARGET bitrate ("15 Mbit hedef" → 15.0).
/// The stream's decoded size (width << 32 | height; 0 = no frame yet), set by each presenter
/// on the first frame and on a live resolution switch — shown next to the codec.
pub static STREAM_DIMS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn set_stream_dims(w: u32, h: u32) {
	STREAM_DIMS.store((w as u64) << 32 | h as u64, std::sync::atomic::Ordering::Relaxed);
}

/// "1080p" for the streamed height. A decoder surface can be padded to its macroblock grid
/// (1080 → 1088), so a height just above a common one reads as that one.
fn stream_res_label() -> String {
	let v = STREAM_DIMS.load(std::sync::atomic::Ordering::Relaxed);
	// The short side: a rotated (portrait) host still reads as "1080p".
	let h = ((v & 0xffff_ffff) as u32).min((v >> 32) as u32);
	if h == 0 {
		return String::new();
	}
	const COMMON: [u32; 10] = [2160, 1600, 1440, 1200, 1080, 900, 768, 720, 540, 360];
	let h = COMMON.iter().copied().find(|&c| h >= c && h < c + 16).unwrap_or(h);
	format!("{h}p")
}

/// The codec actually streaming: the host's stats label leads with it ("H.265 · NVENC · …");
/// before the first label arrives, the user's explicit pick, else a dash.
fn stream_codec(st: &OverlayState) -> String {
	let first = st.host_active.split(" · ").next().unwrap_or("").trim();
	if !first.is_empty() {
		return first.to_string();
	}
	match st.codec.as_str() {
		"h264" => "H.264".into(),
		"h265" => "H.265".into(),
		"av1" => "AV1".into(),
		_ => "—".into(),
	}
}

/// The host's TARGET frame rate from its stats label (fourth field, "120 fps"); empty when
/// unknown. A target, not a measurement: the FPS tile shows what really arrives.
fn stream_target_fps(st: &OverlayState) -> String {
	st.host_active
		.split(" · ")
		.nth(3)
		.map(str::trim)
		.filter(|p| p.ends_with("fps"))
		.unwrap_or("")
		.to_string()
}

/// The host encoder from the same label (second field), empty when unknown.
fn stream_encoder(st: &OverlayState) -> String {
	st.host_active.split(" · ").nth(1).unwrap_or("").trim().to_string()
}

fn host_parts(st: &OverlayState) -> (Option<f32>, Option<f32>) {
	let parts: Vec<&str> = st.host_active.split(" · ").collect();
	let enc_ms = parts
		.get(5)
		.and_then(|p| p.split_whitespace().next())
		.and_then(|v| v.parse().ok());
	let target = parts
		.get(4)
		.and_then(|p| p.split_whitespace().next())
		.and_then(|v| v.parse().ok());
	(enc_ms, target)
}

/// Measured bandwidth, ALWAYS in Mbps with a 0.1 floor (maintainer rule: never
/// kbps, never "0.0" — an active stream reads at least 0.1).
fn fmt_mbps(mbps: f32) -> String {
	format!("{:.1}", mbps.max(0.1))
}

/// A clickable category box (the root hub's grid cells). `icon` is a Phosphor glyph
/// (e.g. `egui_phosphor::regular::BROADCAST`) painted at the left — the Phosphor font is
/// installed into egui's atlas in `apply_theme`, so the glyph renders on every backend.
/// One hub box; `badge` > 0 draws the unread-count disc at its right edge.
fn cat_box(ui: &mut egui::Ui, icon: &str, label: &str, selected: bool, badge: usize, cols: usize) -> egui::Response {
	// Equal tiles across the panel (`cols` per row): icon on top, label under it.
	let cols = cols.max(1) as f32;
	let w = ((PANEL_W - GRID_GAP * (cols - 1.0)) / cols).floor();
	let short = short_surface(ui.ctx());
	let size = egui::vec2(w, if short { 64.0 } else { 72.0 });
	let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
	// `selected` = the controller-nav cursor (UiState.sel, moved by the pad up/down) is on
	// this box. Draw a bright cyan BORDER + brighter fill so it's unmistakable; A/Enter
	// activates it. Driven purely by sel (not egui has_focus) so egui's invisible Tab-focus
	// traversal can't paint a second highlight on a different box.
	let focused = selected;
	let hov = resp.hovered();
	// The same plate, ring and hover as a section's rows and option tiles.
	if focused {
		ui.painter().rect_filled(rect.expand(2.0), 10.0, CYAN);
	}
	let fill = if focused {
		ROW_SEL
	} else if hov {
		ROW_HOVER
	} else {
		ROW_BG
	};
	ui.painter().rect_filled(rect, 8.0, fill);
	// Section icon: the per-category Phosphor glyph (distinct per box). Cyan so it reads as the
	// accent; it renders because `apply_theme` installed the Phosphor font into egui's atlas.
	ui.painter().text(
		egui::pos2(rect.center().x, rect.top() + if short { 23.0 } else { 26.0 }),
		egui::Align2::CENTER_CENTER,
		icon,
		egui::FontId::proportional(22.0),
		if hov && !focused { egui::Color32::WHITE } else { CYAN },
	);
	ui.painter().text(
		egui::pos2(rect.center().x, rect.bottom() - if short { 16.0 } else { 18.0 }),
		egui::Align2::CENTER_CENTER,
		label,
		egui::FontId::proportional(13.5),
		if hov || focused { egui::Color32::WHITE } else { egui::Color32::from_gray(218) },
	);
	if badge > 0 {
		let n = if badge > 9 { "9+".to_string() } else { badge.to_string() };
		let bc = egui::pos2(rect.right() - 14.0, rect.top() + 14.0);
		ui.painter().circle_filled(bc, 8.5, egui::Color32::from_rgb(225, 60, 70));
		ui.painter().text(
			bc,
			egui::Align2::CENTER_CENTER,
			n,
			egui::FontId::proportional(10.0),
			egui::Color32::WHITE,
		);
	}
	resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

// ===========================================================================
// Section rows
// ===========================================================================
//
// Every setting inside a section is one full-width row, and the whole row (or a wide part of
// it) is the target — the old rows were 30 pt strips whose value changed by clicking the left
// or right half to step through the options, with 9 pt arrows as the only hint: picking the
// fifth resolution took four aimed clicks. Three shapes, chosen by the options themselves:
//
// * on / off            → a switch; a click anywhere on the row flips it;
// * up to four options  → all of them side by side, one click on the wanted one;
// * more (or too wide)  → the row shows the value and opens an OPTIONS PAGE: every option a
//                         large tile, one click picks it and returns.
//
// Keys and a pad use the same rows: up/down move the cursor (from the first row, up reaches
// the header strip), left/right step a switch at once and STAGE a side-by-side option (amber
// until A / Enter confirms, dropped when the cursor moves on — a pad never walks the live
// stream through the options in between), A / Enter opens the options page and picks in it,
// B / Escape goes back one level.

/// Row plate colours.
const ROW_BG: egui::Color32 = egui::Color32::from_rgb(26, 28, 38);
const ROW_HOVER: egui::Color32 = egui::Color32::from_rgb(38, 41, 56);
const ROW_SEL: egui::Color32 = egui::Color32::from_rgb(20, 54, 70);
/// The well behind side-by-side options.
const SEG_BG: egui::Color32 = egui::Color32::from_rgb(15, 17, 24);
/// The VALUE in force: the lit option, the switch that is on, the current tile of an options
/// page (a dimmed [`ACCENT`]). Where you ARE is the other colour, on the hub and in a section
/// alike: [`ROW_SEL`] under the cursor and on the current category, [`CYAN`] for the cursor's
/// ring.
const ACCENT_DIM: egui::Color32 = egui::Color32::from_rgb(74, 66, 156);
/// A pad-staged option that A has not confirmed yet.
const AMBER: egui::Color32 = egui::Color32::from_rgb(240, 185, 70);
/// Gap between rows (pt).
const ROW_GAP: f32 = 6.0;
/// Height of the panel's header (pt) — the hub's brand line and a section's strip alike.
const HEADER_H: f32 = 36.0;
/// "End session": the plate at rest and lit (hover / cursor).
const DANGER_DIM: egui::Color32 = egui::Color32::from_rgb(104, 34, 42);
const DANGER: egui::Color32 = egui::Color32::from_rgb(200, 60, 70);
/// Side-by-side options: gap between two, and the well's padding around them.
const SEG_GAP: f32 = 2.0;
const SEG_PAD: f32 = 3.0;

/// Row height (pt). 44 is the size a pointer (and a finger) hits without aiming; a short
/// surface — a windowed 720p app is ~576 pt high — gets 38, so the longest section still shows
/// whole (anything taller scrolls).
fn row_h(ctx: &egui::Context) -> f32 {
	if short_surface(ctx) { 38.0 } else { 44.0 }
}

/// A surface too short for the full sizes (a windowed 720p app is ~576 pt high): rows, the
/// hub's stat tiles and its category boxes take their compact height.
fn short_surface(ctx: &egui::Context) -> bool {
	ctx.viewport_rect().height() < 620.0
}

/// Width of `s` at `size` pt in the UI font.
fn text_w(ui: &egui::Ui, s: &str, size: f32) -> f32 {
	ui.painter()
		.layout_no_wrap(s.to_string(), egui::FontId::proportional(size), egui::Color32::WHITE)
		.size()
		.x
}

/// What a section hands [`Rows::new`]: its slice of the frame-persistent [`UiState`] plus this
/// frame's nav input.
struct RowsIn<'a> {
	sub_sel: &'a mut usize,
	nav: Nav,
	/// Keys / a pad are in use and the cursor is on the rows (not on the header strip).
	cursor: bool,
	pending: &'a mut Option<(usize, String)>,
	picker: &'a mut Option<Picker>,
}

/// One section's rows for this frame. The section calls [`Rows::choice`] / [`Rows::action`]
/// once per row, top to bottom, with the row's index; while an options page is open the row
/// it belongs to draws the page and every other row draws nothing.
struct Rows<'a> {
	sub_sel: &'a mut usize,
	/// The row under the cursor.
	sel: usize,
	nav: Nav,
	cursor: bool,
	/// The cursor moved this frame: scroll it into view.
	moved: bool,
	pending: &'a mut Option<(usize, String)>,
	picker: &'a mut Option<Picker>,
	/// Options page to open / to close once every row of this frame has been drawn (changing
	/// it mid-frame would draw half the rows and half the page).
	open: Option<Picker>,
	close: bool,
}

/// How a row offers its options.
enum RowKind {
	Switch,
	/// Side by side, each this wide (one width per option).
	Segments(Vec<f32>),
	Page,
}

impl<'a> Rows<'a> {
	/// `n` rows. Moves the cursor by this frame's up / down (the options page takes the
	/// arrows while it is open).
	fn new(n: usize, i: RowsIn<'a>) -> Self {
		let last = n.saturating_sub(1);
		let mut moved = false;
		if i.picker.is_none() {
			if i.nav.down {
				*i.sub_sel = (*i.sub_sel + 1).min(last);
				moved = true;
			}
			if i.nav.up {
				*i.sub_sel = i.sub_sel.saturating_sub(1);
				moved = true;
			}
		}
		let sel = (*i.sub_sel).min(last);
		Rows {
			sub_sel: i.sub_sel,
			sel,
			nav: i.nav,
			cursor: i.cursor,
			moved,
			pending: i.pending,
			picker: i.picker,
			open: None,
			close: false,
		}
	}

	/// An options page is up in place of the rows.
	fn picking(&self) -> bool {
		self.picker.is_some()
	}

	/// Put the cursor on `row` from the next frame on (a pad that was moved takes it along).
	fn move_cursor(&mut self, row: usize) {
		*self.sub_sel = row;
	}

	/// Allocate row `row`, `h` high, and paint its plate (cursor ring, hover). Returns the
	/// rect, the row's own click response and whether the cursor is on it — `None` while an
	/// options page replaces the rows.
	fn frame(&self, ui: &mut egui::Ui, row: usize, h: f32) -> Option<(egui::Rect, egui::Response, bool)> {
		if self.picking() {
			return None;
		}
		let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::click());
		let selected = self.cursor && self.sel == row;
		if selected && self.moved {
			ui.scroll_to_rect(rect.expand(8.0), None);
		}
		if selected {
			ui.painter().rect_filled(rect.expand(2.0), 10.0, CYAN);
			ui.painter().rect_filled(rect, 8.0, ROW_SEL);
		} else {
			// A hover anywhere on the row (also on one of its options) lights the plate.
			let hov = resp.hovered() || ui.rect_contains_pointer(rect);
			ui.painter().rect_filled(rect, 8.0, if hov { ROW_HOVER } else { ROW_BG });
		}
		Some((rect, resp, selected))
	}

	fn kind(ui: &egui::Ui, options: &[(&str, &str)], room: f32) -> RowKind {
		let has = |x: &str| options.iter().any(|(v, _)| *v == x);
		if options.len() == 2 && has("on") && has("off") {
			return RowKind::Switch;
		}
		if (2..=4).contains(&options.len()) {
			let n = options.len() as f32;
			let chrome = SEG_GAP * (n - 1.0) + 2.0 * SEG_PAD;
			let own: Vec<f32> = options.iter().map(|(_, d)| (text_w(ui, d, 13.0) + 22.0).max(52.0)).collect();
			// Equal widths read best; a long name among short ones gets its own width instead.
			let widest = own.iter().copied().fold(0.0, f32::max).max(56.0);
			if widest * n + chrome <= room {
				return RowKind::Segments(vec![widest; options.len()]);
			}
			if own.iter().sum::<f32>() + chrome <= room {
				return RowKind::Segments(own);
			}
		}
		RowKind::Page
	}

	/// One setting: `label` (and an optional ⓘ explainer), its `options` as `(value, display)`
	/// and the value in force. Returns the newly chosen value.
	fn choice(
		&mut self,
		ui: &mut egui::Ui,
		row: usize,
		label: &str,
		current: &str,
		options: &[(&str, &str)],
		info: Option<&str>,
	) -> Option<String> {
		if let Some(p) = *self.picker {
			return if p.row == row { self.options_page(ui, p, label, current, options) } else { None };
		}
		let h = row_h(ui.ctx());
		let (rect, resp, selected) = self.frame(ui, row, h)?;
		let cy = rect.center().y;
		ui.painter().text(
			egui::pos2(rect.left() + 14.0, cy),
			egui::Align2::LEFT_CENTER,
			label,
			egui::FontId::proportional(13.5),
			egui::Color32::from_gray(218),
		);
		let mut used = 14.0 + text_w(ui, label, 13.5);
		// A click ON the ⓘ opens its popup and leaves the row alone.
		let mut on_info = false;
		if let Some(text) = info {
			on_info = info_marker(ui, egui::pos2(rect.left() + used + 15.0, cy), row, text);
			used += 28.0;
		}
		let live = options.iter().position(|(v, _)| *v == current);
		match Self::kind(ui, options, rect.width() - used - 24.0) {
			RowKind::Switch => {
				let on = current == "on";
				let mut set = None;
				if selected {
					if self.nav.activate {
						set = Some(!on);
					} else if self.nav.right {
						set = Some(true);
					} else if self.nav.left {
						set = Some(false);
					}
				}
				if resp.clicked() && !on_info {
					set = Some(!on);
				}
				let (tw, th) = (42.0, 24.0);
				let track = egui::Rect::from_min_size(
					egui::pos2(rect.right() - 12.0 - tw, cy - th / 2.0),
					egui::vec2(tw, th),
				);
				let a = ui
					.ctx()
					.animate_bool_with_time(ui.make_persistent_id(("ov-switch", row)), on, 0.12);
				let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * a).round() as u8;
				let off = egui::Color32::from_rgb(62, 66, 84);
				let fill = egui::Color32::from_rgb(
					mix(off.r(), ACCENT.r()),
					mix(off.g(), ACCENT.g()),
					mix(off.b(), ACCENT.b()),
				);
				let p = ui.painter();
				p.rect_filled(track, th / 2.0, fill);
				p.circle_filled(
					egui::pos2(track.left() + 12.0 + (tw - 24.0) * a, cy),
					9.0,
					egui::Color32::WHITE,
				);
				// The state in words (some switches read "Kilitli / Serbest", not on / off).
				let word = live.and_then(|i| options.get(i)).map(|(_, d)| *d).unwrap_or("");
				p.text(
					egui::pos2(track.left() - 10.0, cy),
					egui::Align2::RIGHT_CENTER,
					word,
					egui::FontId::proportional(12.5),
					egui::Color32::from_gray(if on { 225 } else { 150 }),
				);
				resp.on_hover_cursor(egui::CursorIcon::PointingHand);
				set.filter(|v| *v != on).map(|v| if v { "on" } else { "off" }.to_string())
			}
			RowKind::Segments(widths) => {
				let n = options.len();
				let staged_at = |pending: &Option<(usize, String)>| match pending {
					Some((r, v)) if *r == row => options.iter().position(|(o, _)| o == v),
					_ => None,
				};
				let mut chosen = None;
				if selected {
					let at = staged_at(self.pending).or(live).unwrap_or(0);
					if self.nav.right {
						*self.pending = Some((row, options[(at + 1) % n].0.to_string()));
					} else if self.nav.left {
						*self.pending = Some((row, options[(at + n - 1) % n].0.to_string()));
					} else if self.nav.activate {
						if let Some(i) = staged_at(self.pending) {
							chosen = Some(options[i].0.to_string());
							*self.pending = None;
						}
					}
				}
				let staged = if selected { staged_at(self.pending) } else { None };
				let total = widths.iter().sum::<f32>() + SEG_GAP * (n as f32 - 1.0) + 2.0 * SEG_PAD;
				let sh = h - 12.0;
				let well = egui::Rect::from_min_size(
					egui::pos2(rect.right() - 8.0 - total, cy - sh / 2.0),
					egui::vec2(total, sh),
				);
				ui.painter().rect_filled(well, 8.0, SEG_BG);
				let mut x = well.left() + SEG_PAD;
				for (i, (v, d)) in options.iter().enumerate() {
					let r = egui::Rect::from_min_size(
						egui::pos2(x, well.top() + SEG_PAD),
						egui::vec2(widths[i], sh - 2.0 * SEG_PAD),
					);
					x += widths[i] + SEG_GAP;
					let sresp = ui.interact(r, ui.make_persistent_id(("ov-seg", row, i)), egui::Sense::click());
					let is_live = live == Some(i);
					let is_staged = staged == Some(i);
					let hov = sresp.hovered();
					let p = ui.painter();
					if is_live {
						p.rect_filled(r, 6.0, ACCENT_DIM);
					} else if hov {
						p.rect_filled(r, 6.0, ROW_HOVER);
					}
					if is_staged {
						p.rect_stroke(r, 6.0, egui::Stroke::new(1.5, AMBER), egui::StrokeKind::Inside);
					}
					let col = if is_staged {
						AMBER
					} else if is_live || hov {
						egui::Color32::WHITE
					} else {
						egui::Color32::from_gray(178)
					};
					p.text(r.center(), egui::Align2::CENTER_CENTER, *d, egui::FontId::proportional(13.0), col);
					if sresp.clicked() {
						chosen = Some(v.to_string());
						*self.pending = None;
					}
					if hov {
						sresp.on_hover_cursor(egui::CursorIcon::PointingHand);
					}
				}
				chosen.filter(|v| v != current)
			}
			RowKind::Page => {
				let lit = selected || resp.hovered();
				let disp = live.and_then(|i| options.get(i)).map(|(_, d)| *d).unwrap_or(current);
				ui.painter().text(
					egui::pos2(rect.right() - 32.0, cy),
					egui::Align2::RIGHT_CENTER,
					disp,
					egui::FontId::proportional(13.0),
					if lit { CYAN } else { egui::Color32::from_gray(225) },
				);
				paint_chevron(
					ui.painter(),
					egui::pos2(rect.right() - 17.0, cy),
					5.0,
					if lit { CYAN } else { egui::Color32::from_gray(140) },
				);
				let open = (selected && (self.nav.activate || self.nav.right)) || (resp.clicked() && !on_info);
				if open && !options.is_empty() {
					self.open = Some(Picker { row, hi: live.unwrap_or(0), cols: 0 });
					*self.pending = None;
				}
				resp.on_hover_cursor(egui::CursorIcon::PointingHand);
				None
			}
		}
	}

	/// The options page of row `p.row`: its label, then every option as a large tile (the
	/// current one lit). A click — or the arrows and A / Enter — picks one and returns to the
	/// rows; the back button and B / Escape return without a change.
	fn options_page(
		&mut self,
		ui: &mut egui::Ui,
		mut p: Picker,
		label: &str,
		current: &str,
		options: &[(&str, &str)],
	) -> Option<String> {
		let n = options.len();
		if n == 0 {
			self.close = true;
			return None;
		}
		// As many columns as the longest option allows: short ones (fps, bitrate) four across,
		// names (encoders, monitors) two.
		let widest = options.iter().map(|(_, d)| text_w(ui, d, 13.5)).fold(0.0, f32::max);
		let cols = (if widest <= 92.0 {
			4
		} else if widest <= 136.0 {
			3
		} else {
			2
		})
		.min(n);
		p.hi = p.hi.min(n - 1);
		p.cols = cols;
		let moved = self.nav.up || self.nav.down || self.nav.left || self.nav.right;
		if self.nav.right && p.hi + 1 < n && p.hi % cols < cols - 1 {
			p.hi += 1;
		}
		if self.nav.left && p.hi % cols > 0 {
			p.hi -= 1;
		}
		if self.nav.down {
			if p.hi + cols < n {
				p.hi += cols;
			} else if p.hi / cols < (n - 1) / cols {
				// A short last line: its last tile.
				p.hi = n - 1;
			}
		}
		if self.nav.up && p.hi >= cols {
			p.hi -= cols;
		}
		let mut picked = (self.cursor && self.nav.activate).then_some(p.hi);
		ui.label(egui::RichText::new(label).size(13.0).color(egui::Color32::from_gray(165)));
		let h = row_h(ui.ctx());
		let w = ((ui.available_width() - GRID_GAP * (cols as f32 - 1.0)) / cols as f32).floor();
		let cursor = self.cursor;
		egui::Grid::new(("ov_options", p.row))
			.num_columns(cols)
			.spacing(egui::vec2(GRID_GAP, GRID_GAP))
			.show(ui, |ui| {
				for (i, (v, d)) in options.iter().enumerate() {
					let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::click());
					let is_cur = *v == current;
					let hi = cursor && i == p.hi;
					let hov = resp.hovered();
					if hi && moved {
						ui.scroll_to_rect(rect.expand(8.0), None);
					}
					let painter = ui.painter();
					if hi {
						painter.rect_filled(rect.expand(2.0), 10.0, CYAN);
					}
					let fill = if is_cur {
						ACCENT_DIM
					} else if hi {
						ROW_SEL
					} else if hov {
						ROW_HOVER
					} else {
						ROW_BG
					};
					painter.rect_filled(rect, 8.0, fill);
					painter.text(
						rect.center(),
						egui::Align2::CENTER_CENTER,
						*d,
						egui::FontId::proportional(13.5),
						if is_cur || hi || hov { egui::Color32::WHITE } else { egui::Color32::from_gray(205) },
					);
					if resp.clicked() {
						picked = Some(i);
					}
					resp.on_hover_cursor(egui::CursorIcon::PointingHand);
					if i % cols == cols - 1 {
						ui.end_row();
					}
				}
			});
		*self.picker = Some(p);
		let v = options[picked?].0;
		self.close = true;
		(v != current).then(|| v.to_string())
	}

	/// A row that does something (no value): a click, or A / Enter with the cursor on it.
	/// `accent` colours the label while it is lit; a disabled row is dimmed and inert.
	fn action(&mut self, ui: &mut egui::Ui, row: usize, label: &str, accent: egui::Color32, enabled: bool) -> bool {
		let Some((rect, resp, selected)) = self.frame(ui, row, row_h(ui.ctx())) else {
			return false;
		};
		let lit = enabled && (selected || resp.hovered());
		let col = if !enabled {
			egui::Color32::from_gray(110)
		} else if lit {
			accent
		} else {
			egui::Color32::from_gray(218)
		};
		ui.painter().text(
			egui::pos2(rect.left() + 14.0, rect.center().y),
			egui::Align2::LEFT_CENTER,
			label,
			egui::FontId::proportional(13.5),
			col,
		);
		if !enabled {
			return false;
		}
		paint_chevron(
			ui.painter(),
			egui::pos2(rect.right() - 17.0, rect.center().y),
			5.0,
			if lit { accent } else { egui::Color32::from_gray(140) },
		);
		let fired = (selected && self.nav.activate) || resp.clicked();
		resp.on_hover_cursor(egui::CursorIcon::PointingHand);
		fired
	}
}

impl Drop for Rows<'_> {
	fn drop(&mut self) {
		if self.close {
			*self.picker = None;
		} else if let Some(p) = self.open.take() {
			*self.picker = Some(p);
		}
	}
}

/// The inline ⓘ of a row at `ic`: a painted glyph with a click / hover popup (on a pad- or
/// touch-driven overlay a hover tip never shows, so a click opens it too). Returns whether
/// the pointer is on it.
fn info_marker(ui: &mut egui::Ui, ic: egui::Pos2, row: usize, text: &str) -> bool {
	let irect = egui::Rect::from_center_size(ic, egui::vec2(22.0, 22.0));
	let popup_id = ui.make_persistent_id(("crow-info-popup", row));
	let iresp = ui.interact(irect, ui.make_persistent_id(("crow-info-hit", row)), egui::Sense::click());
	let open = egui::Popup::is_id_open(ui.ctx(), popup_id);
	let col = if iresp.hovered() || open {
		CYAN
	} else {
		egui::Color32::from_rgb(120, 125, 140)
	};
	{
		let p = ui.painter();
		p.circle_stroke(ic, 6.5, egui::Stroke::new(1.3, col));
		p.circle_filled(ic + egui::vec2(0.0, -3.0), 0.9, col);
		p.line_segment(
			[ic + egui::vec2(0.0, -0.4), ic + egui::vec2(0.0, 3.2)],
			egui::Stroke::new(1.5, col),
		);
	}
	iresp.clone().on_hover_text(egui::RichText::new(text).size(12.0));
	// A click toggles the popup (egui 0.37 `Popup` API; `popup_below_widget` and
	// `Memory::toggle_popup` are gone).
	egui::Popup::from_response(&iresp)
		.id(popup_id)
		.open_memory(iresp.clicked().then_some(egui::SetOpenCommand::Toggle))
		.close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
		.show(|ui| {
			ui.set_max_width(260.0);
			ui.label(egui::RichText::new(text).size(12.0));
		});
	ui.input(|i| i.pointer.hover_pos()).is_some_and(|p| irect.contains(p))
}

/// Fade the top (or bottom) 18 pt of `area` into `bg`: rows continue out of sight there.
fn paint_fade(painter: &egui::Painter, area: egui::Rect, top: bool, bg: egui::Color32) {
	let clear = egui::Color32::from_rgba_premultiplied(0, 0, 0, 0);
	let (y_bg, y_clear) = if top { (area.top(), area.top() + 18.0) } else { (area.bottom(), area.bottom() - 18.0) };
	let mut mesh = egui::Mesh::default();
	mesh.colored_vertex(egui::pos2(area.left(), y_bg), bg);
	mesh.colored_vertex(egui::pos2(area.right(), y_bg), bg);
	mesh.colored_vertex(egui::pos2(area.right(), y_clear), clear);
	mesh.colored_vertex(egui::pos2(area.left(), y_clear), clear);
	mesh.add_triangle(0, 1, 2);
	mesh.add_triangle(0, 2, 3);
	painter.add(egui::Shape::mesh(mesh));
}

/// A painted ">" (the chevron glyphs are missing from the bundled egui font).
fn paint_chevron(painter: &egui::Painter, c: egui::Pos2, half: f32, color: egui::Color32) {
	let s = egui::Stroke::new(1.8, color);
	painter.line_segment([c + egui::vec2(-half * 0.5, -half), c + egui::vec2(half * 0.5, 0.0)], s);
	painter.line_segment([c + egui::vec2(-half * 0.5, half), c + egui::vec2(half * 0.5, 0.0)], s);
}

/// A small filled triangle pointing up or down (painted: the ▲ / ▼ glyphs are missing from the
/// bundled egui font).
fn paint_tri_v(painter: &egui::Painter, center: egui::Pos2, half: f32, up: bool, color: egui::Color32) {
	let dy = if up { -half } else { half };
	let pts = vec![
		egui::pos2(center.x - half, center.y - dy),
		egui::pos2(center.x + half, center.y - dy),
		egui::pos2(center.x, center.y + dy),
	];
	painter.add(egui::Shape::convex_polygon(pts, color, egui::Stroke::NONE));
}

fn stat_tile(ui: &mut egui::Ui, value: &str, icon: &str, label: &str) {
	// Three equal tiles per row across the panel: the value large on top, icon + label under it.
	let w = ((PANEL_W - GRID_GAP * 2.0) / 3.0).floor();
	let h = if short_surface(ui.ctx()) { 50.0 } else { 58.0 };
	let (rect, _) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::hover());
	let p = ui.painter();
	// A well, not a plate: the numbers are read, the plates below are clicked.
	p.rect_filled(rect, 8.0, SEG_BG);
	p.text(
		egui::pos2(rect.left() + 12.0, rect.top() + if h < 58.0 { 17.0 } else { 19.0 }),
		egui::Align2::LEFT_CENTER,
		value,
		egui::FontId::proportional(19.0),
		CYAN,
	);
	p.text(
		egui::pos2(rect.left() + 12.0, rect.bottom() - if h < 58.0 { 13.0 } else { 15.0 }),
		egui::Align2::LEFT_CENTER,
		icon,
		egui::FontId::proportional(12.0),
		egui::Color32::from_rgb(120, 200, 230),
	);
	p.text(
		egui::pos2(rect.left() + 29.0, rect.bottom() - if h < 58.0 { 13.0 } else { 15.0 }),
		egui::Align2::LEFT_CENTER,
		label,
		egui::FontId::proportional(11.0),
		egui::Color32::from_gray(150),
	);
}

/// A labelled ComboBox; returns Some(value) when the user picks a new value.
/// Mini always-on stats HUD (top-right) drawn while the overlay is CLOSED and the
/// user enabled it. Pointer events are NOT selected in that state (the container is
/// pass-through), so this is display-only by construction.
pub fn draw_hud(ctx: &egui::Context, st: &OverlayState) {
	let bw = fmt_mbps(st.mbps);
	let codec = stream_codec(st);
	egui::Area::new(egui::Id::unique("pulsar_hud"))
		.order(egui::Order::Foreground)
		.anchor(egui::Align2::RIGHT_TOP, egui::vec2(-10.0, 10.0))
		.show(ctx, |ui| {
			egui::Frame::NONE
				.fill(egui::Color32::from_rgba_premultiplied(10, 11, 16, 200))
				.corner_radius(6.0)
				.inner_margin(egui::Margin::symmetric(10, 6))
				.show(ui, |ui| {
					ui.add(
						egui::Label::new(
							egui::RichText::new(format!(
								"{codec} · {:.0} fps · {:.0} ms · {} {:.1} ms · {bw} Mbps",
								st.fps,
								st.latency_ms,
								t("hud.decode"),
								st.decode_ms
							))
							.monospace()
							.size(12.0)
							.color(egui::Color32::from_rgb(180, 220, 240)),
						)
						.wrap_mode(egui::TextWrapMode::Extend),
					);
				});
		});
}

/// Default overlay-open button offset from the top-left, in egui points (the webview
/// hotspot mirrors it as CSS px ×1.25 — keep the two in sync).
pub const BTN_POS_DEFAULT: (f32, f32) = (90.0, 70.0);
/// Overlay-open button size in egui points (the 36 pt disc).
pub const BTN_SIZE: f32 = 36.0;

/// The open button's effective rect: `pos` validated (non-finite → default) and clamped
/// into `screen` so a position persisted from a larger window can't land the button
/// off-window (invisible AND unclickable). Used by `draw_open_button` and by backends
/// that hit-test closed-state clicks — visual and hotspot stay aligned by construction.
pub fn btn_rect(pos: (f32, f32), screen: egui::Rect) -> egui::Rect {
	let (mut x, mut y) = pos;
	if !x.is_finite() || !y.is_finite() {
		(x, y) = BTN_POS_DEFAULT;
	}
	x = x.clamp(0.0, (screen.width() - BTN_SIZE).max(0.0));
	y = y.clamp(0.0, (screen.height() - BTN_SIZE).max(0.0));
	egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(BTN_SIZE, BTN_SIZE))
}

/// Parsec-style overlay-open button: the Pulsar mark (concentric pulse rings) in a
/// translucent dark disc, at `st.btn_pos` (top-LEFT default, user-draggable via the
/// webview hotspot), drawn while the overlay is CLOSED. Returns true when clicked
/// (platforms whose renderer receives pointer events in the closed state — Windows;
/// on Linux the closed container is input pass-through, so the webview provides the
/// matching invisible hotspot instead).
pub fn draw_open_button(ctx: &egui::Context, st: &OverlayState) -> bool {
	let mut clicked = false;
	egui::Area::new(egui::Id::unique("pulsar_ovbtn"))
		.order(egui::Order::Foreground)
		.fixed_pos(btn_rect(st.btn_pos, ctx.viewport_rect()).min)
		.show(ctx, |ui| {
			let size = egui::vec2(BTN_SIZE, BTN_SIZE);
			let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
			let hovered = resp.hovered();
			let p = ui.painter();
			let c = rect.center();
			let bg_a = if hovered { 235 } else { 150 };
			p.circle_filled(
				c,
				18.0,
				egui::Color32::from_rgba_premultiplied(13, 14, 20, bg_a),
			);
			// The brand mark: pure concentric circles (pulse rings) + core dot.
			let ring = if hovered {
				ACCENT
			} else {
				egui::Color32::from_rgb(150, 140, 245)
			};
			p.circle_stroke(c, 12.0, egui::Stroke::new(1.6, ring.gamma_multiply(0.55)));
			p.circle_stroke(c, 7.5, egui::Stroke::new(1.8, ring.gamma_multiply(0.8)));
			p.circle_filled(c, 3.2, ring);
			// Unread host-chat badge (top-right): a red disc with the count, like an
			// app-icon notification. Cleared by the backend when the Chat view opens.
			if st.chat_unread > 0 {
				let n = if st.chat_unread > 9 {
					"9+".to_string()
				} else {
					st.chat_unread.to_string()
				};
				let bc = egui::pos2(rect.right() - 5.0, rect.top() + 5.0);
				p.circle_filled(bc, 7.5, egui::Color32::from_rgb(225, 60, 70));
				p.text(
					bc,
					egui::Align2::CENTER_CENTER,
					n,
					egui::FontId::proportional(9.5),
					egui::Color32::WHITE,
				);
			}
			clicked = resp.clicked();
		});
	clicked
}

/// How long a chat bubble stays beside the closed-state open button (s), the last
/// [`BUBBLE_FADE`] of it fading out; and how many are shown at once (newest kept).
pub const BUBBLE_SECS: f32 = 8.0;
const BUBBLE_FADE: f32 = 0.8;
pub const BUBBLE_MAX: usize = 3;

/// Incoming chat while the overlay is CLOSED: message-sized bubbles next to the open
/// button — on its right when the button sits in the left half of the screen, on its left
/// otherwise; stacked downward from a button in the top half, upward from one in the
/// bottom half (oldest farthest from the button's edge). They slide in, then fade out
/// after [`BUBBLE_SECS`]. Display-only (the closed container is input pass-through).
pub fn draw_chat_bubbles(ctx: &egui::Context, st: &OverlayState) {
	if st.chat_toasts.is_empty() {
		return;
	}
	let screen = ctx.viewport_rect();
	let btn = btn_rect(st.btn_pos, screen);
	let right_side = btn.center().x < screen.center().x;
	let downward = btn.center().y < screen.center().y;
	let gap = 8.0;
	// Widest a bubble may get: the room on that side of the button, capped.
	let room = if right_side { screen.right() - btn.right() } else { btn.left() - screen.left() };
	let max_w = (room - gap - 12.0).clamp(120.0, 340.0);
	let font = egui::FontId::proportional(13.5);
	let pad = egui::vec2(12.0, 8.0);

	// Lay out first (sizes), then place: newest nearest the button's row.
	let bubbles: Vec<(std::sync::Arc<egui::Galley>, f32)> = st
		.chat_toasts
		.iter()
		.map(|(text, age)| {
			let g = ctx.fonts_mut(|f| {
				f.layout(text.clone(), font.clone(), egui::Color32::WHITE, max_w - 2.0 * pad.x)
			});
			(g, *age)
		})
		.collect();
	let mut y = if downward { btn.top() } else { btn.bottom() };
	// Downward: newest at the top (by the button) and older ones below; upward mirrors it.
	for (i, (galley, age)) in bubbles.iter().rev().enumerate() {
		let size = galley.size() + 2.0 * pad;
		let fade_in = (age / 0.18).clamp(0.0, 1.0);
		let fade_out = ((BUBBLE_SECS - age) / BUBBLE_FADE).clamp(0.0, 1.0);
		let alpha = fade_in.min(fade_out);
		let slide = (1.0 - fade_in) * 10.0 * if right_side { -1.0 } else { 1.0 };
		let x = if right_side { btn.right() + gap } else { btn.left() - gap - size.x } + slide;
		let top = if downward { y } else { y - size.y };
		let rect = egui::Rect::from_min_size(egui::pos2(x, top), size);
		let painter = ctx.layer_painter(egui::LayerId::new(
			egui::Order::Foreground,
			egui::Id::new(("pulsar_chat_bubble", i)),
		));
		painter.rect_filled(
			rect,
			12.0,
			egui::Color32::from_rgba_unmultiplied(34, 36, 50, (235.0 * alpha) as u8),
		);
		painter.rect_stroke(
			rect,
			12.0,
			egui::Stroke::new(1.0, CYAN.gamma_multiply(0.35 * alpha)),
			egui::StrokeKind::Inside,
		);
		painter.galley_with_override_text_color(
			rect.min + pad,
			galley.clone(),
			egui::Color32::WHITE.gamma_multiply(alpha),
		);
		y = if downward { rect.bottom() + 6.0 } else { rect.top() - 6.0 };
	}
}

/// Transient helper tooltip, bottom-center over the video (drawn while the overlay is
/// CLOSED): "click to take control" after the overlay closes, "how to release" right
/// after control is engaged. The caller owns the ~3 s lifetime and passes `alpha`
/// (1→0 over the final stretch) for a smooth fade-out. One wide line — the label is
/// explicitly non-wrapping so the text never folds into a narrow column.
pub fn draw_hint(ctx: &egui::Context, text: &str, alpha: f32) {
	egui::Area::new(egui::Id::unique("pulsar_hint"))
		.order(egui::Order::Foreground)
		.anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -28.0))
		.show(ctx, |ui| {
			ui.set_opacity(alpha.clamp(0.0, 1.0));
			egui::Frame::NONE
				.fill(egui::Color32::from_rgba_premultiplied(10, 11, 16, 215))
				.corner_radius(8.0)
				.inner_margin(egui::Margin::symmetric(14, 8))
				.show(ui, |ui| {
					ui.add(
						egui::Label::new(
							egui::RichText::new(text)
								.size(13.0)
								.color(egui::Color32::from_rgb(210, 215, 230)),
						)
						.wrap_mode(egui::TextWrapMode::Extend),
					);
				});
		});
}

// Retained for the mouse/remote-mode menus and as a fallback; the game-mode sub-views now use
// pad-navigable `choice_row`s instead.
#[allow(dead_code)]
fn combo(
	ui: &mut egui::Ui,
	label: &str,
	id: &str,
	cur: &str,
	opts: &[(&str, &str)],
	active: &str,
) -> Option<String> {
	let mut picked = None;
	ui.vertical(|ui| {
		ui.label(
			egui::RichText::new(label)
				.small()
				.color(egui::Color32::GRAY),
		);
		let cur_label = opts
			.iter()
			.find(|(v, _)| *v == cur)
			.map(|(_, l)| *l)
			.unwrap_or(cur);
		egui::ComboBox::from_id_salt(id)
			.selected_text(cur_label)
			.width(180.0)
			.show_ui(ui, |ui| {
				for (v, l) in opts {
					if ui.selectable_label(*v == cur, *l).clicked() && *v != cur {
						picked = Some((*v).to_string());
					}
				}
			});
		// What's REALLY in use right now (faint truth line; the combo is the request).
		if !active.is_empty() {
			ui.label(
				egui::RichText::new(format!("{} {active}", t("active")))
					.size(10.0)
					.color(egui::Color32::from_gray(110)),
			);
		}
	});
	picked
}

// Retained for the remote-mode seg menus / as a fallback — the game sub-views moved to
// pad-navigable choice_rows (which carry the inline ⓘ explainer now), leaving this unused.
#[allow(dead_code)]
fn seg(ui: &mut egui::Ui, on: bool, label: &str) -> bool {
	let fill = if on {
		ACCENT
	} else {
		egui::Color32::from_rgb(28, 30, 42)
	};
	ui.add(egui::Button::new(label).fill(fill)).clicked() && !on
}

/// Painted ⓘ whose tip shows only while the pointer is on it, BELOW the marker (no click).
fn info_hover_below(ui: &mut egui::Ui, text: &str) {
	let (rect, resp) = ui.allocate_exact_size(egui::vec2(15.0, 15.0), egui::Sense::hover());
	let col = if resp.hovered() { CYAN } else { egui::Color32::from_rgb(110, 115, 130) };
	let c = rect.center();
	{
		let p = ui.painter();
		p.circle_stroke(c, 6.0, egui::Stroke::new(1.3, col));
		p.circle_filled(c + egui::vec2(0.0, -2.8), 0.9, col);
		p.line_segment([c + egui::vec2(0.0, -0.4), c + egui::vec2(0.0, 3.0)], egui::Stroke::new(1.5, col));
	}
	egui::Popup::from_response(&resp)
		.kind(egui::PopupKind::Tooltip)
		.align(egui::RectAlign::BOTTOM)
		.align_alternatives(&[egui::RectAlign::BOTTOM_START, egui::RectAlign::BOTTOM_END])
		.open(resp.hovered())
		.show(|ui| {
			ui.set_max_width(260.0);
			ui.label(egui::RichText::new(text).size(12.0));
		});
}

/// Small painted ⓘ marker with a hover tooltip — for options whose name alone
/// doesn't explain them (e.g. frame pacing). Painted (circle + dot + stem)
/// because egui's bundled fonts lack a clean info glyph; the tooltip itself
/// fades in via egui's built-in popup animation. (Superseded inline in `choice_row_i`'s
/// `info` arg, but kept as the standalone helper.)
#[allow(dead_code)]
fn info(ui: &mut egui::Ui, text: &str) {
	// Clickable (not just hover): a tap toggles a popup with the explanation — on a controller-
	// /touch-driven overlay hover tooltips never appear, so clicking did nothing before.
	let (rect, resp) = ui.allocate_exact_size(egui::vec2(15.0, 15.0), egui::Sense::click());
	let popup_id = ui.make_persistent_id(("info-popup", rect.left() as i32, rect.top() as i32));
	let open = egui::Popup::is_id_open(ui.ctx(), popup_id);
	let col = if resp.hovered() || open {
		CYAN
	} else {
		egui::Color32::from_rgb(110, 115, 130)
	};
	let c = rect.center();
	{
		let p = ui.painter();
		p.circle_stroke(c, 6.0, egui::Stroke::new(1.3, col));
		p.circle_filled(c + egui::vec2(0.0, -2.8), 0.9, col);
		p.line_segment(
			[c + egui::vec2(0.0, -0.4), c + egui::vec2(0.0, 3.0)],
			egui::Stroke::new(1.5, col),
		);
	}
	resp.clone().on_hover_text(egui::RichText::new(text).size(12.0));
	egui::Popup::from_response(&resp)
		.id(popup_id)
		.open_memory(resp.clicked().then_some(egui::SetOpenCommand::Toggle))
		.close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
		.show(|ui| {
			ui.set_max_width(240.0);
			ui.label(egui::RichText::new(text).size(12.0));
		});
}

#[cfg(test)]
mod focus_tests {
	use super::*;

	// The pad-nav overlay (G6) moves focus with egui Tab/Shift-Tab events fed from the
	// controller reader (up→shifttab, down→tab). This proves the menu boxes are actually
	// keyboard-focusable and that Tab walks them — the necessary condition for pad nav to
	// work. If this fails, no amount of pad input will move the overlay selection.
	#[test]
	fn tab_walks_the_menu_boxes() {
		let ctx = egui::Context::default();
		let run = |events: Vec<egui::Event>| {
			ctx.run_ui(
				egui::RawInput {
					events,
					screen_rect: Some(egui::Rect::from_min_size(
						egui::pos2(0.0, 0.0),
						egui::vec2(400.0, 600.0),
					)),
					..Default::default()
				},
				|ui| {
					egui::CentralPanel::default().show(ui, |ui| {
						cat_box(ui, icon::BROADCAST, "one", false, 0, 4);
						cat_box(ui, icon::MONITOR, "two", false, 0, 4);
						cat_box(ui, icon::CHART_BAR, "three", false, 0, 4);
					});
				},
			)
			// No painter here: discard the font-atlas upload (egui 0.37 debug-asserts on a
			// dropped, unapplied TexturesDelta).
			.drop_without_applying_deltas();
		};
		let tab = || egui::Event::Key {
			key: egui::Key::Tab,
			physical_key: None,
			pressed: true,
			repeat: false,
			modifiers: egui::Modifiers::default(),
		};
		run(vec![]); // first frame: register the widgets
		run(vec![tab()]);
		let f1 = ctx.memory(|m| m.focused());
		run(vec![tab()]);
		let f2 = ctx.memory(|m| m.focused());
		assert!(f1.is_some(), "Tab focused nothing — cat_box is not keyboard-focusable");
		assert_ne!(f1, f2, "second Tab did not advance focus to the next box");
	}
}

#[cfg(test)]
mod bubble_tests {
	use super::*;

	/// Bubbles land beside the button on the side with room: right of a left-half button,
	/// left of a right-half one — and never draw anything with no messages.
	#[test]
	fn chat_bubbles_sit_on_the_roomy_side_of_the_button() {
		let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 720.0));
		let shapes_for = |btn_pos: (f32, f32), toasts: Vec<(String, f32)>| {
			let ctx = egui::Context::default();
			let st = OverlayState { btn_pos, chat_toasts: toasts, ..OverlayState::default() };
			let mut out = ctx.run_pass(
				egui::RawInput { screen_rect: Some(screen), ..Default::default() },
				|ctx| draw_chat_bubbles(ctx, &st),
			);
			out.textures_delta.clear(); // no painter here
			out.shapes
				.iter()
				.filter_map(|c| match &c.shape {
					egui::Shape::Rect(r) => Some(r.rect),
					_ => None,
				})
				.collect::<Vec<_>>()
		};
		let msg = || vec![("selam, görebiliyor musun?".to_string(), 2.0)];
		assert!(shapes_for((90.0, 70.0), Vec::new()).is_empty());
		let left = shapes_for((90.0, 70.0), msg());
		let btn = btn_rect((90.0, 70.0), screen);
		assert!(!left.is_empty() && left.iter().all(|r| r.left() > btn.right()), "{left:?}");
		let right = shapes_for((1150.0, 600.0), msg());
		let btn = btn_rect((1150.0, 600.0), screen);
		assert!(!right.is_empty() && right.iter().all(|r| r.right() < btn.left()), "{right:?}");
		assert!(right.iter().all(|r| r.bottom() <= btn.bottom() + 0.5), "bottom half grows upward");
	}
}

#[cfg(test)]
mod new_divider_tests {
	use super::*;

	#[test]
	fn the_new_divider_marks_what_was_unread_on_entry_and_resets_after() {
		let mut ui = UiState::default();
		// 5 messages, the last 2 unread → divider above index 3.
		mark_new_chat(&mut ui, true, 5, 2);
		assert_eq!(ui.chat_new_from, Some(3));
		// A message arriving while the view is open is read on arrival: no move.
		mark_new_chat(&mut ui, true, 6, 1);
		assert_eq!(ui.chat_new_from, Some(3));
		// Leaving the view (or closing the overlay) clears it; a visit with nothing
		// unread shows no divider.
		mark_new_chat(&mut ui, false, 6, 0);
		assert_eq!(ui.chat_new_from, None);
		mark_new_chat(&mut ui, true, 6, 0);
		assert_eq!(ui.chat_new_from, None);
	}
}

#[cfg(test)]
mod deferred_restart_tests {
	use super::*;

	fn run(ctx: &egui::Context, st: &OverlayState, ui: &mut UiState) -> Vec<OverlayCmd> {
		let mut cmds = Vec::new();
		let mut out = ctx.run_pass(egui::RawInput::default(), |ctx| {
			cmds = draw(ctx, st, ui);
		});
		out.textures_delta.clear(); // no painter here (egui asserts on unapplied deltas)
		cmds
	}

	fn sets(cmds: &[OverlayCmd]) -> Vec<(&'static str, String)> {
		cmds.iter()
			.filter_map(|c| match c {
				OverlayCmd::Set(f, v) => Some((*f, v.clone())),
				_ => None,
			})
			.collect()
	}

	#[test]
	fn restart_choices_wait_for_the_close_and_the_last_one_wins() {
		let mut ui = UiState::default();
		stage(&mut ui, "codec", "h264".into());
		stage(&mut ui, "res", "1080p".into());
		stage(&mut ui, "codec", "h265".into());
		assert_eq!(staged(&ui.deferred, "codec", "auto"), "h265");
		assert_eq!(staged(&ui.deferred, "fps", "60"), "60", "an unstaged field shows the live value");
		let out = take_deferred(&mut ui);
		assert_eq!(sets(&out), vec![("codec", "h265".to_string()), ("res", "1080p".to_string())]);
		assert!(ui.deferred.is_empty());
	}

	#[test]
	fn closing_from_outside_applies_them_and_reopening_lands_on_the_hub() {
		let ctx = egui::Context::default();
		let mut ui = UiState::default();
		let mut st = OverlayState { open: true, ..OverlayState::default() };
		run(&ctx, &st, &mut ui);
		ui.view = View::Stream;
		stage(&mut ui, "fps", "120".into());
		// Hotkey / app-side close: the next draw sees it closed.
		st.open = false;
		let out = run(&ctx, &st, &mut ui);
		assert_eq!(sets(&out), vec![("fps", "120".to_string())]);
		assert!(run(&ctx, &st, &mut ui).is_empty(), "applied once");
		st.open = true;
		run(&ctx, &st, &mut ui);
		assert!(ui.view == View::Root, "a fresh open shows the main page");
	}

	#[test]
	fn headset_lines_parse() {
		assert_eq!(
			parse_headsets("abc:80:00ff10,def:100:-"),
			vec![
				HeadsetBar { uuid: "abc".into(), volume: 80, rgb: Some([0, 255, 16]) },
				HeadsetBar { uuid: "def".into(), volume: 100, rgb: None },
			]
		);
		assert!(parse_headsets("").is_empty());
		assert!(parse_headsets(" ").is_empty());
		assert_eq!(parse_headsets("x:250:zz")[0].volume, 100, "clamped, bad rgb ignored");
		assert_eq!(parse_headsets("x:250:zz")[0].rgb, None);
	}

	fn key(ctx: &egui::Context, st: &OverlayState, ui: &mut UiState, k: egui::Key) -> Vec<OverlayCmd> {
		let mut cmds = Vec::new();
		let input = egui::RawInput {
			events: vec![egui::Event::Key {
				key: k,
				physical_key: None,
				pressed: true,
				repeat: false,
				modifiers: Default::default(),
			}],
			..Default::default()
		};
		let mut out = ctx.run_pass(input, |ctx| {
			cmds = draw(ctx, st, ui);
		});
		out.textures_delta.clear();
		cmds
	}

	/// The pad drives the headphone bars on the hub: the first input wakes the cursor on
	/// bar 0, left/right change its level, down walks to the next bar and then the grid.
	#[test]
	fn headset_bars_follow_the_dpad() {
		set_headsets("pad-a:80:ff0000,pad-b:50:-");
		let ctx = egui::Context::default();
		let mut ui = UiState::default();
		let st = OverlayState { open: true, ..OverlayState::default() };
		run(&ctx, &st, &mut ui);
		key(&ctx, &st, &mut ui, egui::Key::ArrowDown); // wakes the cursor only
		assert_eq!(ui.hp_sel, Some(0));
		let out = key(&ctx, &st, &mut ui, egui::Key::ArrowRight);
		assert_eq!(sets(&out), vec![("hpvol", "pad-a,85".to_string())]);
		assert_eq!(headsets()[0].volume, 85, "optimistic");
		key(&ctx, &st, &mut ui, egui::Key::ArrowDown);
		assert_eq!(ui.hp_sel, Some(1));
		let out = key(&ctx, &st, &mut ui, egui::Key::ArrowLeft);
		assert_eq!(sets(&out), vec![("hpvol", "pad-b,45".to_string())]);
		key(&ctx, &st, &mut ui, egui::Key::ArrowDown);
		assert_eq!((ui.hp_sel, ui.sel), (None, 0), "into the box grid");
		key(&ctx, &st, &mut ui, egui::Key::ArrowUp);
		assert_eq!(ui.hp_sel, Some(1), "back up to the last bar");
		set_headsets("");
		run(&ctx, &st, &mut ui);
		assert_eq!(ui.hp_sel, None, "bars gone: cursor back on the grid");
	}

	#[test]
	fn live_settings_still_apply_at_once() {
		assert!(!RESTART_FIELDS.contains(&"bitrate"));
		assert!(!RESTART_FIELDS.contains(&"fit"));
		assert!(RESTART_FIELDS.contains(&"codec") && RESTART_FIELDS.contains(&"adapt"));
	}
}

#[cfg(test)]
mod stats_codec_tests {
	use super::*;

	#[test]
	fn the_codec_comes_from_the_host_label_then_the_pick() {
		let mut st = OverlayState::default();
		st.host_active = "H.265 · NVENC · 1920x1200 · 120 fps · 17 Mbit hedef · 3.1 ms kodlama".into();
		assert_eq!(stream_target_fps(&st), "120 fps");
		assert_eq!(stream_codec(&st), "H.265");
		assert_eq!(stream_encoder(&st), "NVENC");
		st.host_active.clear();
		st.codec = "av1".into();
		assert_eq!(stream_codec(&st), "AV1", "before the host's label: the user's pick");
		st.codec = "auto".into();
		assert_eq!(stream_codec(&st), "—");
		assert_eq!(stream_encoder(&st), "");
	}
}

#[cfg(test)]
mod host_ptr_tests {
	use super::*;

	#[test]
	fn host_pointer_lines_drive_the_marker() {
		set_host_ptr_show(true);
		host_ptr_line("1 0.25 0.5");
		assert!(host_ptr_active(Mode::Remote));
		assert!(!host_ptr_active(Mode::Game), "never in game mode");
		assert_eq!(host_ptr_visible(Mode::Remote).map(|v| v.0), Some(Some((0.25, 0.5))));
		host_ptr_line("1 -1 -1"); // position unknown → notice only
		assert_eq!(host_ptr_visible(Mode::Remote).map(|v| v.0), Some(None));
		host_ptr_line("0");
		assert!(!host_ptr_active(Mode::Remote));
		// Still fading right after the idle line.
		assert!(host_ptr_visible(Mode::Remote).is_some_and(|v| v.1 > 0.5));
		set_host_ptr_show(false);
		host_ptr_line("1 0.5 0.5");
		assert!(host_ptr_visible(Mode::Remote).is_none());
		set_host_ptr_show(true);
	}
}

#[cfg(test)]
mod rows_tests {
	use super::*;

	fn key(k: egui::Key) -> egui::Event {
		egui::Event::Key {
			key: k,
			physical_key: None,
			pressed: true,
			repeat: false,
			modifiers: egui::Modifiers::default(),
		}
	}

	fn run(ctx: &egui::Context, st: &OverlayState, ui: &mut UiState, events: Vec<egui::Event>) -> Vec<OverlayCmd> {
		let mut cmds = Vec::new();
		let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 720.0));
		let mut out = ctx.run_pass(
			egui::RawInput { events, screen_rect: Some(screen), ..Default::default() },
			|ctx| {
				cmds = draw(ctx, st, ui);
			},
		);
		out.textures_delta.clear(); // no painter here (egui asserts on unapplied deltas)
		cmds
	}

	fn sets(cmds: &[OverlayCmd]) -> Vec<(&'static str, String)> {
		cmds.iter()
			.filter_map(|c| match c {
				OverlayCmd::Set(f, v) => Some((*f, v.clone())),
				_ => None,
			})
			.collect()
	}

	fn press(ctx: &egui::Context, st: &OverlayState, ui: &mut UiState, k: egui::Key) -> Vec<(&'static str, String)> {
		sets(&run(ctx, st, ui, vec![key(k)]))
	}

	/// An open overlay on `view` with the key / pad cursor awake on the first row.
	fn opened(view: View, st: &OverlayState) -> (egui::Context, UiState) {
		let ctx = egui::Context::default();
		let mut ui = UiState::default();
		run(&ctx, st, &mut ui, vec![]);
		ui.view = view;
		run(&ctx, st, &mut ui, vec![]);
		// The first input only wakes the cursor.
		assert!(press(&ctx, st, &mut ui, egui::Key::ArrowDown).is_empty());
		assert!(ui.nav_active && ui.sub_sel == 0 && !ui.tab_focus);
		(ctx, ui)
	}

	fn open_state() -> OverlayState {
		OverlayState { open: true, ..OverlayState::default() }
	}

	#[test]
	fn a_switch_flips_at_once() {
		let st = open_state();
		let (ctx, mut ui) = opened(View::Gauges, &st);
		// Row 0 = the stats HUD, off: A / Enter turns it on, right too, left does nothing.
		assert_eq!(press(&ctx, &st, &mut ui, egui::Key::Enter), vec![("statshud", "on".to_string())]);
		assert_eq!(press(&ctx, &st, &mut ui, egui::Key::ArrowRight), vec![("statshud", "on".to_string())]);
		assert!(press(&ctx, &st, &mut ui, egui::Key::ArrowLeft).is_empty(), "already off");
		assert!(ui.pending.is_none(), "a switch stages nothing");
	}

	#[test]
	fn side_by_side_options_are_staged_until_confirmed() {
		let st = open_state();
		let (ctx, mut ui) = opened(View::Display, &st);
		// Row 0 = the view fit (fit / stretch / original), on "fit".
		assert!(press(&ctx, &st, &mut ui, egui::Key::ArrowRight).is_empty(), "staged, not applied");
		assert_eq!(ui.pending, Some((0, "stretch".to_string())));
		assert!(press(&ctx, &st, &mut ui, egui::Key::ArrowRight).is_empty());
		assert_eq!(ui.pending, Some((0, "original".to_string())));
		assert_eq!(press(&ctx, &st, &mut ui, egui::Key::Enter), vec![("fit", "original".to_string())]);
		assert!(ui.pending.is_none());
		// Staged, then the cursor moves on: dropped.
		press(&ctx, &st, &mut ui, egui::Key::ArrowLeft);
		assert!(ui.pending.is_some());
		assert!(press(&ctx, &st, &mut ui, egui::Key::ArrowDown).is_empty());
		assert!(ui.pending.is_none() && ui.sub_sel == 1);
	}

	#[test]
	fn a_long_list_opens_its_options_page() {
		let st = open_state();
		let (ctx, mut ui) = opened(View::Stream, &st);
		press(&ctx, &st, &mut ui, egui::Key::ArrowDown);
		assert_eq!(ui.sub_sel, 1, "the encoder row: eight options");
		assert!(press(&ctx, &st, &mut ui, egui::Key::Enter).is_empty());
		assert_eq!(ui.picker, Some(Picker { row: 1, hi: 0, cols: 0 }), "opens on the value in force");
		// The arrows move in the page, not between the rows.
		press(&ctx, &st, &mut ui, egui::Key::ArrowRight);
		press(&ctx, &st, &mut ui, egui::Key::ArrowDown);
		assert_eq!(ui.sub_sel, 1);
		let hi = ui.picker.expect("still open").hi;
		assert!(hi > 1, "one right, one line down: {hi}");
		// A picks: the page closes; the encoder restarts the stream, so it waits for the close.
		assert!(press(&ctx, &st, &mut ui, egui::Key::Enter).is_empty());
		assert_eq!(ui.picker, None);
		assert_eq!(ui.deferred, vec![("encoder", encoders_opts()[hi].0.to_string())]);
	}

	#[test]
	fn escape_goes_back_one_level_at_a_time() {
		let st = open_state();
		let (ctx, mut ui) = opened(View::Stream, &st);
		press(&ctx, &st, &mut ui, egui::Key::ArrowDown);
		press(&ctx, &st, &mut ui, egui::Key::Enter);
		assert!(ui.picker.is_some());
		let out = run(&ctx, &st, &mut ui, vec![key(egui::Key::Escape)]);
		assert!(ui.picker.is_none() && ui.view == View::Stream && out.is_empty(), "page → rows");
		run(&ctx, &st, &mut ui, vec![key(egui::Key::Escape)]);
		assert!(ui.view == View::Root, "rows → hub");
		let out = run(&ctx, &st, &mut ui, vec![key(egui::Key::Escape)]);
		assert!(out.iter().any(|c| matches!(c, OverlayCmd::Close)), "hub → closed");
	}

	#[test]
	fn up_from_the_first_row_reaches_the_section_strip() {
		let st = open_state();
		let (ctx, mut ui) = opened(View::Stream, &st);
		press(&ctx, &st, &mut ui, egui::Key::ArrowUp);
		assert!(ui.tab_focus && ui.view == View::Stream);
		// Left / right walk the sections (game mode: Stream, Display, Audio, …). Left of the
		// first one is the back button; right from it is the first section again.
		press(&ctx, &st, &mut ui, egui::Key::ArrowLeft);
		assert!(ui.view == View::Stream && ui.tab_focus && ui.back_focus);
		press(&ctx, &st, &mut ui, egui::Key::ArrowLeft);
		assert!(ui.view == View::Stream && ui.back_focus, "nothing left of the back button");
		press(&ctx, &st, &mut ui, egui::Key::ArrowRight);
		assert!(ui.view == View::Stream && ui.tab_focus && !ui.back_focus);
		press(&ctx, &st, &mut ui, egui::Key::ArrowRight);
		assert!(ui.view == View::Display && ui.tab_focus);
		press(&ctx, &st, &mut ui, egui::Key::ArrowRight);
		assert!(ui.view == View::Audio);
		// Down returns to the rows of the section it stopped on, at the top.
		press(&ctx, &st, &mut ui, egui::Key::ArrowDown);
		assert!(!ui.tab_focus && ui.view == View::Audio && ui.sub_sel == 0);
		// On the strip the rows take no input.
		press(&ctx, &st, &mut ui, egui::Key::ArrowUp);
		assert!(press(&ctx, &st, &mut ui, egui::Key::Enter).is_empty());
		assert!(!ui.tab_focus, "A on the strip enters the rows, it does not flip the first one");
	}

	#[test]
	fn the_back_button_takes_the_cursor() {
		let st = open_state();
		// From the rows: up (strip), left (back button), A → the hub.
		let (ctx, mut ui) = opened(View::Stream, &st);
		press(&ctx, &st, &mut ui, egui::Key::ArrowUp);
		press(&ctx, &st, &mut ui, egui::Key::ArrowLeft);
		assert!(ui.back_focus);
		// Down from it is the rows again, nothing changed.
		assert!(press(&ctx, &st, &mut ui, egui::Key::ArrowDown).is_empty());
		assert!(!ui.tab_focus && ui.view == View::Stream && ui.sub_sel == 0);
		press(&ctx, &st, &mut ui, egui::Key::ArrowUp);
		assert!(ui.tab_focus && !ui.back_focus, "up lands on the category, not on the button");
		press(&ctx, &st, &mut ui, egui::Key::ArrowLeft);
		let out = run(&ctx, &st, &mut ui, vec![key(egui::Key::Enter)]);
		assert!(ui.view == View::Root && !ui.tab_focus && out.is_empty(), "A on back → the hub");

		// From an options page: up from its first line is the back button; A returns to the
		// rows without a change, and the cursor is where it was.
		let (ctx, mut ui) = opened(View::Stream, &st);
		press(&ctx, &st, &mut ui, egui::Key::ArrowDown);
		press(&ctx, &st, &mut ui, egui::Key::Enter);
		assert!(ui.picker.is_some());
		run(&ctx, &st, &mut ui, vec![]); // the page's first frame (its columns are known after)
		press(&ctx, &st, &mut ui, egui::Key::ArrowUp);
		assert!(ui.tab_focus && ui.back_focus && ui.picker.is_some());
		// Right has nowhere to go (no strip to walk while a page is up), Enter picks nothing.
		press(&ctx, &st, &mut ui, egui::Key::ArrowRight);
		assert!(ui.back_focus && ui.picker.is_some());
		assert!(press(&ctx, &st, &mut ui, egui::Key::Enter).is_empty());
		assert!(ui.picker.is_none() && ui.view == View::Stream && !ui.tab_focus && ui.sub_sel == 1);
	}

	#[test]
	fn the_strip_skips_files_and_the_hub_keeps_every_category() {
		assert_eq!(categories(Mode::Remote).len(), 8);
		assert_eq!(categories(Mode::Game).len(), 6);
		assert!(section_tabs(Mode::Remote).all(|c| c.2 != View::Files));
		assert_eq!(section_tabs(Mode::Remote).count(), 7);
		for (_, _, v) in categories(Mode::Game) {
			assert!(has_rows(*v), "every game-mode section is made of rows");
		}
	}

	#[test]
	fn the_tools_actions_fire_from_the_keys() {
		let st = OverlayState { open: true, mode: Mode::Remote, ..OverlayState::default() };
		let (ctx, mut ui) = opened(View::Tools, &st);
		press(&ctx, &st, &mut ui, egui::Key::ArrowDown);
		assert_eq!(press(&ctx, &st, &mut ui, egui::Key::Enter), vec![("sendclip", "1".to_string())]);
		for _ in 0..3 {
			press(&ctx, &st, &mut ui, egui::Key::ArrowDown);
		}
		assert_eq!(press(&ctx, &st, &mut ui, egui::Key::Enter), vec![("fullscreen", "1".to_string())]);
	}
}
