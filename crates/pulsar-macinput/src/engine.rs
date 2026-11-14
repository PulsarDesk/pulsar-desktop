//! The platform-neutral half of the macOS capture: engage/release lifecycle, Pulsar's combos,
//! held-key bookkeeping, the mouse decisions (relative / absolute / confine / free-cursor
//! handover / click-to-engage). Mirrors src-tauri's Windows `kbdhook/imp.rs` rule for rule, so
//! the three desktop platforms behave the same; differences are called out where they are.
//!
//! The OS layer (`crate::os`: the CGEventTap thread on macOS, a stub elsewhere) feeds events
//! in and carries out the few cursor side effects (warp, mouse/cursor association). Nothing
//! in here calls a macOS API directly, so all of it is unit-tested on every platform.

use crate::geom::{self, Rect};
use crate::os;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// One input event for the host — `pulsar_core::service::InputEvent`'s variants this capture
/// produces (the app maps them 1:1; this crate stays free of the engine dependency).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Out {
	/// evdev key code.
	Key { code: u32, down: bool },
	/// Absolute pointer, 0..=1 across the video.
	Motion { x: f64, y: f64 },
	/// Relative motion (already scaled by the mouse sensitivity).
	Relative { dx: f64, dy: f64 },
	/// 0 = left, 1 = right, 2 = middle.
	Button { button: u8, down: bool },
	/// Browser wheel pixels: +y = down, +x = right.
	Scroll { dx: f64, dy: f64 },
}

/// A lifecycle edge or combo the app turns into its Tauri event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
	/// `kbd-engaged`
	Engaged,
	/// `kbd-released`
	Released,
	/// `kbd-detach` (the user let go with Ctrl+Alt+Z / 3×RightCtrl)
	Detach,
	/// `overlay-toggle`; `true` = Ctrl+Shift are kept as held (`keep_chord`).
	OverlayToggle(bool),
	/// `kbd-leave` (Ctrl+Shift+Q ends the session)
	Leave,
	/// `fullscreen-toggle`
	FullscreenToggle,
}

/// Where the capture sends its output. `send` must never block (it is called on the event-tap
/// thread, which macOS disables when a callback stalls).
pub trait Sink: Send + Sync {
	fn send(&self, ev: Out);
	fn signal(&self, s: Signal);
	/// Mouse-sensitivity scaling of a relative delta.
	fn scale(&self, dx: f64, dy: f64) -> (f64, f64) {
		(dx, dy)
	}
}

/// What `enable` arms the capture for.
#[derive(Debug, Clone, Copy)]
pub struct Arm {
	/// The play (session) id.
	pub id: u64,
	/// Also capture the mouse (native-renderer session). `false` = keyboard only.
	pub mouse: bool,
	/// Remote-desktop mode (absolute mouse). `false` = game mode (relative mouse).
	pub remote: bool,
	/// "Serbest imleç" setting (only acts in remote mode).
	pub free_cursor: bool,
}

// ───────────────────────────── state ─────────────────────────────

struct State {
	sink: Option<Arc<dyn Sink>>,
	ctrl: bool,
	alt: bool,
	shift: bool,
	/// Cmd (evdev Meta) — Cmd+Shift+F11 counts as the fullscreen combo, as on the webview path.
	meta: bool,
	/// Ctrl / Shift of a combo that released control while they stayed down (`keep_chord`).
	chord_kept: [Option<Instant>; 2],
	/// Recent RightCtrl presses for the 3×RightCtrl release (≤ 1 s).
	rctrl_presses: Vec<Instant>,
	/// Keys held down on the host (flushed as up-strokes when forwarding stops).
	held: HashSet<u32>,
	/// Combo trigger keys physically held (rising-edge detection vs autorepeat).
	combo_held: HashSet<u32>,
	/// Combo trigger keys whose current press was the real chord.
	combo_active: HashSet<u32>,
	/// Mouse buttons held on the host.
	held_buttons: HashSet<u8>,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

fn state() -> MutexGuard<'static, State> {
	STATE
		.get_or_init(|| {
			Mutex::new(State {
				sink: None,
				ctrl: false,
				alt: false,
				shift: false,
				meta: false,
				chord_kept: [None; 2],
				rctrl_presses: Vec::new(),
				held: HashSet::new(),
				combo_held: HashSet::new(),
				combo_active: HashSet::new(),
				held_buttons: HashSet::new(),
			})
		})
		.lock()
		.unwrap_or_else(|e| e.into_inner())
}

/// Capture armed for a session (`enable` … `disable`).
pub(crate) static ENABLED: AtomicBool = AtomicBool::new(false);
/// The user is controlling: input goes to the host and is swallowed locally.
pub(crate) static ENGAGED: AtomicBool = AtomicBool::new(false);
/// Native-renderer session: the mouse is captured too.
pub(crate) static MOUSE_CAPTURE: AtomicBool = AtomicBool::new(false);
static REMOTE: AtomicBool = AtomicBool::new(true);
static FREE_SETTING: AtomicBool = AtomicBool::new(false);
/// The OS capture (event tap) is running. Without it nothing engages: the webview path
/// (canvas control) stays in charge, exactly as before this capture existed.
pub(crate) static TAP_OK: AtomicBool = AtomicBool::new(false);
/// Pulsar (or one of its renderer children) is the frontmost app.
pub(crate) static APP_FOCUSED: AtomicBool = AtomicBool::new(true);
static LAST_PLAY: AtomicU64 = AtomicU64::new(u64::MAX);
static ENGAGED_MEMO: AtomicBool = AtomicBool::new(false);
static ENGAGED_AT: Mutex<Option<Instant>> = Mutex::new(None);
static KIOSK_ENGAGE: AtomicBool = AtomicBool::new(false);
static KIOSK_SESSION: AtomicBool = AtomicBool::new(false);
static PENDING_ENGAGE: AtomicU64 = AtomicU64::new(u64::MAX);
static STREAM_STALLED: AtomicBool = AtomicBool::new(false);
static HOVER_ARMED: AtomicBool = AtomicBool::new(true);
static OVERLAY_OPEN: AtomicBool = AtomicBool::new(false);
static HOVER_EDGE: AtomicBool = AtomicBool::new(false);
static HOVER_HINTED: AtomicU64 = AtomicU64::new(u64::MAX);
static POINTER_LOCK: AtomicBool = AtomicBool::new(false);
static POINTER_LOCK_ID: AtomicU64 = AtomicU64::new(u64::MAX);
static POINTER_LOCK_REPORTED: AtomicBool = AtomicBool::new(false);
/// Right after an engage the frontmost-app poll may still see the app the click came from
/// (activation is asynchronous): it must not release control during this window.
static FOCUS_GRACE: Mutex<Option<Instant>> = Mutex::new(None);
const FOCUS_GRACE_FOR: Duration = Duration::from_millis(600);
/// Where the frozen cursor sits in relative mode.
static ANCHOR: Mutex<Option<(f64, f64)>> = Mutex::new(None);

/// Tap-observed modifier state (atomics: the tap callback decides suppression lock-free).
static K_CTRL: AtomicBool = AtomicBool::new(false);
static K_ALT: AtomicBool = AtomicBool::new(false);
static K_SHIFT: AtomicBool = AtomicBool::new(false);
static K_META: AtomicBool = AtomicBool::new(false);

/// Geometry the OS layer measures (window list, every 100 ms while armed).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Geo {
	/// The picture area: input is normalized against it and the cursor confined to it.
	pub video: Option<Rect>,
	/// The renderer window itself (the overlay-open button is placed in its fractions).
	pub render: Option<Rect>,
}
static GEO: Mutex<Geo> = Mutex::new(Geo { video: None, render: None });
static VIDEO_FRAC: Mutex<Option<(f32, f32, f32, f32)>> = Mutex::new(None);
static OVBTN_FRAC: Mutex<Option<(f32, f32, f32, f32)>> = Mutex::new(None);

/// The session area of a Pulsar window: the window's Quartz number and the area inside it
/// (`x, y, w, h`, points from the window's top-left). Used as the video rect when no renderer
/// window of our own is on screen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
	pub window: i64,
	pub rect: (f64, f64, f64, f64),
}
static VIEWS: Mutex<Vec<(u64, View)>> = Mutex::new(Vec::new());

const MAX: u64 = u64::MAX;
const HOVER_LEAVE: &str = "pointer left the video";
/// How long the modifiers of a combo that released control count as still held.
const CHORD_KEEP: Duration = Duration::from_secs(5);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
	m.lock().unwrap_or_else(|e| e.into_inner())
}

fn sink() -> Option<Arc<dyn Sink>> {
	state().sink.clone()
}

fn signal(sigs: &[Signal]) {
	if sigs.is_empty() {
		return;
	}
	if let Some(s) = sink() {
		for sig in sigs {
			s.signal(*sig);
		}
	}
}

// ───────────────────────────── worker ─────────────────────────────

/// Work the tap callback hands off: it must return fast, so key logic, lifecycle edges and
/// app signals run on one long-lived worker thread (in order).
#[derive(Debug, Clone, Copy)]
pub(crate) enum Job {
	Key(u32, bool),
	/// Re-evaluate the free-cursor handover.
	Hover,
	/// A click on the video while disengaged: take control (`x, y` = the click).
	ClickEngage(f64, f64),
	/// ENGAGED was already dropped inline (the click must pass locally): flush + signal.
	Released(&'static str),
	/// The frontmost-app poll saw Pulsar gain (`true`) / lose the focus.
	Focus(bool),
}

#[cfg(not(test))]
pub(crate) fn queue(job: Job) {
	use std::sync::mpsc;
	static Q: OnceLock<Mutex<mpsc::Sender<Job>>> = OnceLock::new();
	let q = Q.get_or_init(|| {
		let (tx, rx) = mpsc::channel::<Job>();
		std::thread::Builder::new()
			.name("macinput-worker".into())
			.spawn(move || {
				for job in rx {
					run_job(job);
				}
			})
			.expect("spawn macinput worker");
		Mutex::new(tx)
	});
	let _ = lock(q).send(job);
}

/// Tests run jobs inline so their effects can be asserted right away.
#[cfg(test)]
pub(crate) fn queue(job: Job) {
	run_job(job);
}

fn run_job(job: Job) {
	match job {
		Job::Key(code, down) => {
			let _ = handle_key(code, down);
		}
		Job::Hover => handle_hover(),
		Job::ClickEngage(x, y) => {
			if !ENGAGED.load(SeqCst) {
				HOVER_EDGE.store(false, SeqCst);
				tracing::info!("click on the video → engage");
				engage_edge(Some((x, y)));
			}
		}
		Job::Released(why) => {
			tracing::info!(why, "kbd capture RELEASED");
			finish_release();
		}
		Job::Focus(true) => {
			APP_FOCUSED.store(true, SeqCst);
			try_pending_engage();
		}
		Job::Focus(false) => {
			if !in_focus_grace() {
				tracing::info!("frontmost app is no longer Pulsar");
				focus_lost();
			}
		}
	}
}

// ───────────────────────────── lifecycle ─────────────────────────────

fn pointer_lock_for(id: u64) -> bool {
	POINTER_LOCK_ID.load(SeqCst) == id && POINTER_LOCK_REPORTED.load(SeqCst)
}

fn recently_engaged() -> bool {
	lock(&ENGAGED_AT).map(|t| t.elapsed() < Duration::from_secs(3)).unwrap_or(false)
}

fn in_focus_grace() -> bool {
	lock(&FOCUS_GRACE).map(|t| t.elapsed() < FOCUS_GRACE_FOR).unwrap_or(false)
}

/// Relative mouse: game mode, or a host pointer lock (a game hid the cursor).
pub(crate) fn relative_mode() -> bool {
	MOUSE_CAPTURE.load(SeqCst) && (POINTER_LOCK.load(SeqCst) || !REMOTE.load(SeqCst))
}

/// The free-cursor handover is live: remote mode, setting on, no host pointer lock.
pub(crate) fn free_mode() -> bool {
	REMOTE.load(SeqCst) && FREE_SETTING.load(SeqCst) && !POINTER_LOCK.load(SeqCst)
}

/// Arm the capture for a session. Returns whether the OS capture (event tap) runs — `false`
/// means the Accessibility permission is missing and nothing is captured (the app keeps its
/// webview input path).
pub fn enable(new_sink: Arc<dyn Sink>, arm: Arm) -> bool {
	let id = arm.id;
	let same_session = LAST_PLAY.load(SeqCst) == id;
	if !same_session {
		if PENDING_ENGAGE.load(SeqCst) != id {
			PENDING_ENGAGE.store(MAX, SeqCst);
		}
		STREAM_STALLED.store(false, SeqCst);
		HOVER_ARMED.store(true, SeqCst);
		POINTER_LOCK.store(pointer_lock_for(id), SeqCst);
	}
	LAST_PLAY.store(id, SeqCst);
	MOUSE_CAPTURE.store(arm.mouse, SeqCst);
	REMOTE.store(arm.remote, SeqCst);
	FREE_SETTING.store(arm.free_cursor, SeqCst);
	{
		let mut g = state();
		g.sink = Some(new_sink);
		g.ctrl = false;
		g.alt = false;
		g.shift = false;
		g.meta = false;
		g.chord_kept = [None; 2];
		g.rctrl_presses.clear();
		g.combo_held.clear();
		g.combo_active.clear();
	}
	let tap = os::start_tap();
	TAP_OK.store(tap, SeqCst);
	tracing::info!(
		id,
		same_session,
		mouse = arm.mouse,
		remote = arm.remote,
		tap,
		memo = ENGAGED_MEMO.load(SeqCst),
		kiosk = KIOSK_ENGAGE.load(SeqCst),
		"mac kbd capture enable()"
	);
	ENABLED.store(true, SeqCst);
	os::set_tap_enabled(true);
	if tap {
		os::refresh_geometry();
	}
	let kiosk = KIOSK_ENGAGE.load(SeqCst);
	KIOSK_SESSION.store(kiosk, SeqCst);
	if kiosk && !ENGAGED.load(SeqCst) {
		engage_auto();
	} else if kiosk {
		tracing::info!("mac kbd capture enable(): kiosk, keeping the live engagement");
	} else if same_session {
		if ENGAGED_MEMO.load(SeqCst) {
			engage();
		} else if ENGAGED.load(SeqCst) {
			tracing::info!("mac kbd capture enable(): keeping the live engagement");
		}
	} else if ENGAGED.load(SeqCst) && recently_engaged() {
		tracing::info!("mac kbd capture enable(): keeping the fresh engagement");
	} else {
		ENGAGED.store(false, SeqCst);
	}
	try_pending_engage();
	tap
}

/// Disarm: stop swallowing + forwarding, flush everything held on the host, free the cursor.
pub fn disable() {
	tracing::info!(engaged = ENGAGED.load(SeqCst), "mac kbd capture disable()");
	ENABLED.store(false, SeqCst);
	ENGAGED_MEMO.store(ENGAGED.load(SeqCst), SeqCst);
	ENGAGED.store(false, SeqCst);
	KIOSK_SESSION.store(false, SeqCst);
	os::set_tap_enabled(false);
	released_effects();
	let mut g = state();
	flush_held(&mut g);
	g.sink = None;
}

/// CLI kiosk (`--connect`): the next `enable` engages right away (latched).
pub fn arm_kiosk() {
	KIOSK_ENGAGE.store(true, SeqCst);
}

/// Take control (a click on the video, the menu closing on its combo, a re-arm).
pub fn engage() {
	HOVER_EDGE.store(false, SeqCst);
	engage_edge(None);
}

/// An automatic engage (game mode's connect-time "control immediately"): only while Pulsar is
/// the frontmost app, otherwise when it becomes frontmost.
pub fn engage_auto() {
	if app_focused_now() {
		engage();
	} else {
		let id = LAST_PLAY.load(SeqCst);
		PENDING_ENGAGE.store(id, SeqCst);
		tracing::info!(id, "auto-engage deferred: Pulsar is not the frontmost app");
	}
}

fn engage_edge(click: Option<(f64, f64)>) {
	PENDING_ENGAGE.store(MAX, SeqCst);
	if STREAM_STALLED.load(SeqCst) {
		tracing::info!("engage refused: the stream is stopped");
		return;
	}
	if !TAP_OK.load(SeqCst) || !ENABLED.load(SeqCst) {
		// No OS capture (Accessibility permission missing) or not armed: nothing to engage;
		// the webview path stays in charge.
		return;
	}
	if !ENGAGED.swap(true, SeqCst) {
		let now = Instant::now();
		*lock(&ENGAGED_AT) = Some(now);
		*lock(&FOCUS_GRACE) = Some(now);
		APP_FOCUSED.store(true, SeqCst);
		if video_rect().is_none() {
			os::refresh_geometry();
		}
		tracing::info!(relative = relative_mode(), "mac kbd capture ENGAGED");
		apply_pointer_mode(click);
		signal(&[Signal::Engaged]);
	}
}

fn try_pending_engage() {
	let id = PENDING_ENGAGE.load(SeqCst);
	if id == MAX || !ENABLED.load(SeqCst) || LAST_PLAY.load(SeqCst) != id || !APP_FOCUSED.load(SeqCst) {
		return;
	}
	tracing::info!(id, "Pulsar frontmost → deferred auto-engage");
	engage();
}

/// Drop control without ending the session (`why` goes to the log; "overlay" = the session
/// menu is opening over the video, so hovering must not grab control straight back).
pub fn release(why: &str) {
	HOVER_EDGE.store(why == HOVER_LEAVE, SeqCst);
	if why == "overlay" {
		HOVER_ARMED.store(false, SeqCst);
	}
	if ENGAGED.swap(false, SeqCst) {
		tracing::info!(why, "mac kbd capture RELEASED");
		finish_release();
	}
}

/// Everything a release does after ENGAGED dropped: cursor back, host keys/buttons up, signal.
fn finish_release() {
	released_effects();
	{
		let mut g = state();
		flush_held(&mut g);
	}
	signal(&[Signal::Released]);
}

/// Give the cursor back to the local desktop (relative mode froze it).
fn released_effects() {
	*lock(&ANCHOR) = None;
	os::associate(true);
}

/// Pulsar's window focus changed (the app's per-window focus map). Losing it is not always
/// leaving: our renderer child (a separate process with its own window) taking the focus is
/// still Pulsar — the frontmost app decides.
pub fn set_focused(focused: bool) {
	tracing::info!(focused, engaged = ENGAGED.load(SeqCst), "app focus changed");
	if focused {
		APP_FOCUSED.store(true, SeqCst);
		try_pending_engage();
		return;
	}
	if os::frontmost_is_ours() == Some(true) {
		tracing::info!("focus moved to another Pulsar window / the renderer — keeping engagement");
		return;
	}
	focus_lost();
}

fn focus_lost() {
	APP_FOCUSED.store(false, SeqCst);
	drop_kept_chord(&mut state());
	if ENGAGED.load(SeqCst) {
		if KIOSK_SESSION.load(SeqCst) {
			PENDING_ENGAGE.store(LAST_PLAY.load(SeqCst), SeqCst);
		}
		release("focus-loss");
	}
}

/// The OS layer's frontmost-app poll (every 100 ms while armed).
pub(crate) fn focus_tick(ours: bool) {
	if ours != APP_FOCUSED.load(SeqCst) && (ours || !in_focus_grace()) {
		queue(Job::Focus(ours));
	}
}

/// Pulsar is the frontmost app (last known state).
pub fn app_focused() -> bool {
	APP_FOCUSED.load(SeqCst)
}

/// Pulsar (or one of its renderer children) is the frontmost app, asked fresh where possible.
pub fn app_focused_now() -> bool {
	os::frontmost_is_ours().unwrap_or_else(|| APP_FOCUSED.load(SeqCst))
}

/// Session menu (overlay) open: no hover engage meanwhile; closing re-arms the handover.
pub fn overlay_open(open: bool) {
	OVERLAY_OPEN.store(open, SeqCst);
	if !open {
		HOVER_ARMED.store(true, SeqCst);
	}
}

/// The stream stopped (`true`) / came back: no engaging while stopped.
pub fn set_stream_stalled(stalled: bool) {
	if STREAM_STALLED.swap(stalled, SeqCst) != stalled {
		tracing::info!(stalled, "stream stopped/resumed (mac input capture)");
	}
	if stalled {
		release("stream stopped");
	}
}

/// The host hid / showed its cursor (a game grabbed the mouse): relative while hidden.
pub fn set_pointer_lock(id: u64, on: bool) {
	POINTER_LOCK_ID.store(id, SeqCst);
	POINTER_LOCK_REPORTED.store(on, SeqCst);
	if LAST_PLAY.load(SeqCst) != id && ENABLED.load(SeqCst) {
		return; // another session's report; applied when that one is armed
	}
	let was = POINTER_LOCK.swap(on, SeqCst);
	if was != on && ENGAGED.load(SeqCst) {
		apply_pointer_mode(None);
	}
}

/// The free-cursor setting flipped.
pub fn set_pointer_free(on: bool) {
	if FREE_SETTING.swap(on, SeqCst) != on && ENGAGED.load(SeqCst) {
		apply_pointer_mode(None);
	}
}

/// Where the video sits inside the renderer window (fractions; the renderer reports them).
pub fn set_video_frac(x0: f32, y0: f32, x1: f32, y1: f32) {
	*lock(&VIDEO_FRAC) = Some((x0, y0, x1, y1));
}

/// Where the renderer's overlay-open button sits (fractions of the renderer window).
pub fn set_ovbtn_frac(f: Option<(f32, f32, f32, f32)>) {
	*lock(&OVBTN_FRAC) = f;
}

/// The session area of play `id` inside its Pulsar window (`None` = hidden / inactive tab).
pub fn set_view(id: u64, view: Option<View>) {
	let mut v = lock(&VIEWS);
	v.retain(|(i, _)| *i != id);
	if let Some(view) = view {
		v.push((id, view));
	}
}

/// What the OS layer needs to measure the video rect: the renderer's video fractions and the
/// armed session's view.
type Frac = (f32, f32, f32, f32);

pub(crate) fn rect_inputs() -> (Option<Frac>, Option<View>) {
	let id = LAST_PLAY.load(SeqCst);
	let view = lock(&VIEWS).iter().find(|(i, _)| *i == id).map(|(_, v)| *v);
	(*lock(&VIDEO_FRAC), view)
}

pub(crate) fn set_geo(geo: Geo) {
	*lock(&GEO) = geo;
}

pub(crate) fn video_rect() -> Option<Rect> {
	lock(&GEO).video
}

fn over_ovbtn(x: f64, y: f64) -> bool {
	let Some(f) = *lock(&OVBTN_FRAC) else { return false };
	let Some(r) = lock(&GEO).render else { return false };
	geom::in_frac(r, f, x, y)
}

/// A key edge from the webview (fallback relay); same handling as a tapped key.
pub fn web_key(evdev: u32, down: bool) {
	if ENABLED.load(SeqCst) {
		queue(Job::Key(evdev, down));
	}
}

/// Armed AND engaged.
pub fn is_engaged() -> bool {
	ENABLED.load(SeqCst) && ENGAGED.load(SeqCst)
}

/// The last engage/release edge came from the free-cursor handover.
pub fn hover_edge() -> bool {
	HOVER_EDGE.load(SeqCst)
}

/// The OS capture is running for the armed session.
pub fn capture_active() -> bool {
	ENABLED.load(SeqCst) && TAP_OK.load(SeqCst)
}

// ───────────────────────────── cursor ─────────────────────────────

/// Put the cursor into the state the current mode wants: relative = frozen (disassociated from
/// the mouse) at an anchor inside the video; absolute = associated, pulled into the video unless
/// the free cursor lets it roam. `click` = the engaging click: the host pointer starts there.
fn apply_pointer_mode(click: Option<(f64, f64)>) {
	if !ENGAGED.load(SeqCst) || !MOUSE_CAPTURE.load(SeqCst) {
		released_effects();
		return;
	}
	let pos = click.or_else(os::cursor_pos);
	let rect = video_rect();
	if let (Some((x, y)), Some(r)) = (pos, rect) {
		let (cx, cy) = geom::clamp_into(r, x, y);
		if click.is_some() {
			if let Some(s) = sink() {
				let (nx, ny) = geom::normalize(r, cx, cy);
				s.send(Out::Motion { x: nx, y: ny });
			}
		}
		if relative_mode() {
			*lock(&ANCHOR) = Some((cx, cy));
			if !geom::contains(r, x, y) {
				os::warp(cx, cy);
			}
			os::associate(false);
		} else {
			*lock(&ANCHOR) = None;
			os::associate(true);
			if !free_mode() && !geom::contains(r, x, y) {
				os::warp(cx, cy);
			}
		}
	} else if relative_mode() {
		*lock(&ANCHOR) = pos;
		os::associate(false);
	} else {
		*lock(&ANCHOR) = None;
		os::associate(true);
	}
}

/// Periodic upkeep (OS layer, 100 ms): re-freeze the cursor in relative mode (another app
/// may have re-associated it), pull it back into the video in confined absolute mode (the
/// window moved under it).
pub(crate) fn maintain() {
	if !ENGAGED.load(SeqCst) || !MOUSE_CAPTURE.load(SeqCst) {
		return;
	}
	let Some((x, y)) = os::cursor_pos() else { return };
	if relative_mode() {
		os::associate(false);
		let anchor = *lock(&ANCHOR);
		match anchor {
			Some((ax, ay)) if (x - ax).abs() > 0.5 || (y - ay).abs() > 0.5 => os::warp(ax, ay),
			Some(_) => {}
			None => *lock(&ANCHOR) = Some((x, y)),
		}
	} else if !free_mode() {
		if let Some(r) = video_rect() {
			if !geom::contains(r, x, y) {
				let (cx, cy) = geom::clamp_into(r, x, y);
				os::warp(cx, cy);
			}
		}
	}
}

/// The anchor the relative-mode cursor is held at.
pub(crate) fn anchor() -> Option<(f64, f64)> {
	*lock(&ANCHOR)
}

// ───────────────────────────── keys ─────────────────────────────

/// A modifier edge seen by the tap (lock-free state for `key_suppress`).
pub(crate) fn tap_modifier(evdev: u32, down: bool) {
	match evdev {
		29 | 97 => K_CTRL.store(down, SeqCst),
		56 | 100 => K_ALT.store(down, SeqCst),
		42 | 54 => K_SHIFT.store(down, SeqCst),
		125 | 126 => K_META.store(down, SeqCst),
		_ => {}
	}
}

/// Swallow this key locally? Engaged: every key acts only on the host. Disengaged but Pulsar
/// frontmost: only the Ctrl+Shift+M / Q / F11 combo triggers (and Cmd+Shift+F11) are eaten —
/// their actions run on the worker — everything else types locally.
pub(crate) fn key_suppress(evdev: u32) -> bool {
	if ENGAGED.load(SeqCst) {
		return true;
	}
	if !APP_FOCUSED.load(SeqCst) {
		return false;
	}
	let shift = K_SHIFT.load(SeqCst);
	let ctrl = K_CTRL.load(SeqCst);
	match evdev {
		50 | 16 => ctrl && shift,
		87 => (ctrl || K_META.load(SeqCst)) && shift,
		_ => false,
	}
}

fn flush_held(g: &mut State) {
	if let Some(s) = g.sink.clone() {
		for code in g.held.drain() {
			s.send(Out::Key { code, down: false });
		}
		for button in g.held_buttons.drain() {
			s.send(Out::Button { button, down: false });
		}
	} else {
		g.held.clear();
		g.held_buttons.clear();
	}
	// Drop the local chord tracking too (see imp.rs flush_held): a modifier whose up-stroke is
	// never seen must not let a later bare key fire a combo.
	g.ctrl = false;
	g.alt = false;
	g.shift = false;
	g.meta = false;
	g.chord_kept = [None; 2];
	g.rctrl_presses.clear();
}

/// Ctrl+Shift stay "held" after the overlay combo released control (imp.rs `keep_chord`): the
/// same M closes the menu again while they stay down.
fn keep_chord(g: &mut State) {
	g.ctrl = true;
	g.shift = true;
	g.chord_kept = [Some(Instant::now()); 2];
}

fn drop_kept_chord(g: &mut State) {
	if g.chord_kept[0].take().is_some() {
		g.ctrl = false;
	}
	if g.chord_kept[1].take().is_some() {
		g.shift = false;
	}
}

/// One key edge: combos, the engaged forward, held-key bookkeeping. Returns whether the key
/// was consumed (forwarded or a combo). Runs on the worker (or the test thread).
pub(crate) fn handle_key(evdev: u32, down: bool) -> bool {
	let mut sigs = Vec::new();
	let consumed = {
		let mut g = state();
		handle_key_locked(&mut g, evdev, down, &mut sigs)
	};
	signal(&sigs);
	consumed
}

fn handle_key_locked(g: &mut State, evdev: u32, down: bool, sigs: &mut Vec<Signal>) -> bool {
	match evdev {
		29 | 97 => {
			g.ctrl = down;
			g.chord_kept[0] = None;
		}
		56 | 100 => g.alt = down,
		42 | 54 => {
			g.shift = down;
			g.chord_kept[1] = None;
		}
		125 | 126 => g.meta = down,
		_ => {}
	}
	if g.chord_kept.iter().flatten().any(|at| at.elapsed() > CHORD_KEEP) {
		drop_kept_chord(g);
	}
	let engaged = ENGAGED.load(SeqCst);
	let owned = engaged || APP_FOCUSED.load(SeqCst);
	let combo_key = matches!(evdev, 50 | 16 | 87 | 44);
	let combo_edge = if combo_key {
		if down {
			let edge = g.combo_held.insert(evdev);
			if edge {
				let chord = ((evdev == 50 || evdev == 16) && g.ctrl && g.shift)
					|| (evdev == 87 && (g.ctrl || g.meta) && g.shift)
					|| (evdev == 44 && g.ctrl && g.alt);
				if chord {
					g.combo_active.insert(evdev);
				}
			}
			edge
		} else {
			g.combo_held.remove(&evdev);
			g.combo_active.remove(&evdev);
			false
		}
	} else {
		false
	};
	// Autorepeat of a combo trigger whose chord fired: swallow (a bare M/Q/Z repeat forwards).
	if combo_key && down && !combo_edge && g.combo_active.contains(&evdev) && owned {
		return true;
	}
	// Ctrl+Shift+M — the session menu, engaged or merely frontmost.
	if combo_edge && evdev == 50 && g.ctrl && g.shift && owned {
		let was_engaged = ENGAGED.swap(false, SeqCst);
		if was_engaged {
			tracing::info!("mac kbd capture RELEASED (overlay combo, immediate)");
			HOVER_EDGE.store(false, SeqCst);
			HOVER_ARMED.store(false, SeqCst);
			released_effects();
			flush_held(g);
			keep_chord(g);
			sigs.push(Signal::Released);
		}
		tracing::info!(was_engaged, "overlay combo → overlay-toggle");
		sigs.push(Signal::OverlayToggle(was_engaged));
		return true;
	}
	// Ctrl+Shift+Q — end the session.
	if combo_edge && evdev == 16 && g.ctrl && g.shift && owned {
		flush_held(g);
		sigs.push(Signal::Leave);
		return true;
	}
	// Ctrl+Shift+F11 (or Cmd+Shift+F11) — fullscreen; control stays as it is.
	if combo_edge && evdev == 87 && (g.ctrl || g.meta) && g.shift && owned {
		sigs.push(Signal::FullscreenToggle);
		return true;
	}
	if !engaged {
		return false;
	}
	// 3×RightCtrl within 1 s: let go (session stays).
	if down && evdev == 97 && !g.held.contains(&97) {
		let now = Instant::now();
		g.rctrl_presses.retain(|t| now.duration_since(*t) < Duration::from_secs(1));
		g.rctrl_presses.push(now);
		if g.rctrl_presses.len() >= 3 {
			g.rctrl_presses.clear();
			g.held.insert(97);
			ENGAGED.store(false, SeqCst);
			tracing::info!("mac kbd capture RELEASED (3xRightCtrl)");
			HOVER_EDGE.store(false, SeqCst);
			HOVER_ARMED.store(false, SeqCst);
			released_effects();
			flush_held(g);
			sigs.push(Signal::Released);
			sigs.push(Signal::Detach);
			return true;
		}
	}
	// Ctrl+Alt+Z (Ctrl+Option+Z): Parsec's detach.
	if combo_edge && evdev == 44 && g.ctrl && g.alt {
		ENGAGED.store(false, SeqCst);
		tracing::info!("mac kbd capture RELEASED (Ctrl+Alt+Z)");
		HOVER_EDGE.store(false, SeqCst);
		HOVER_ARMED.store(false, SeqCst);
		released_effects();
		flush_held(g);
		sigs.push(Signal::Released);
		sigs.push(Signal::Detach);
		return true;
	}
	let Some(s) = g.sink.clone() else { return false };
	if down {
		g.held.insert(evdev);
	} else {
		g.held.remove(&evdev);
	}
	s.send(Out::Key { code: evdev, down });
	true
}

// ───────────────────────────── mouse ─────────────────────────────

/// A mouse event as the OS layer reads it (Quartz global points).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum MouseEv {
	/// Moved / dragged: the new location and the device delta.
	Move { x: f64, y: f64, dx: f64, dy: f64 },
	/// `hit_ours` = the topmost window under the point is Pulsar's or a renderer child's.
	Button { button: u8, down: bool, x: f64, y: f64, hit_ours: bool },
	/// Browser wheel pixels.
	Scroll { dx: f64, dy: f64 },
}

/// What the OS layer does with the event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Verdict {
	/// Deliver it locally, unchanged.
	Pass,
	/// Drop it (it acted on the host only).
	Swallow,
	/// Deliver it at this point instead and warp the cursor there (the confine).
	Clamp(f64, f64),
	/// Drop it and keep the cursor at this anchor (relative mode).
	Anchor(f64, f64),
}

/// Decide one mouse event. Runs on the tap thread: no blocking, lifecycle work is queued.
pub(crate) fn mouse(ev: MouseEv) -> Verdict {
	if !MOUSE_CAPTURE.load(SeqCst) || !ENABLED.load(SeqCst) {
		return Verdict::Pass;
	}
	if !ENGAGED.load(SeqCst) {
		match ev {
			MouseEv::Move { .. } if free_mode() => queue(Job::Hover),
			MouseEv::Button { button: 0, down: true, x, y, hit_ours } => {
				// Click-to-engage: a left click on the video while Pulsar owns that spot. The
				// click itself still acts locally (it activates the window); its release is
				// passed too (an up whose down was never forwarded), so it never reaches the host.
				let inside = video_rect().is_some_and(|r| geom::contains(r, x, y));
				if hit_ours && inside && !over_ovbtn(x, y) && TAP_OK.load(SeqCst) && !STREAM_STALLED.load(SeqCst) {
					queue(Job::ClickEngage(x, y));
				}
			}
			_ => {}
		}
		return Verdict::Pass;
	}
	let mut jobs: Vec<Job> = Vec::new();
	let verdict = {
		let mut g = state();
		let Some(s) = g.sink.clone() else { return Verdict::Pass };
		match ev {
			MouseEv::Move { x, y, dx, dy } => {
				if relative_mode() {
					let (sx, sy) = s.scale(dx, dy);
					if sx != 0.0 || sy != 0.0 {
						s.send(Out::Relative { dx: sx, dy: sy });
					}
					let (ax, ay) = anchor().unwrap_or((x, y));
					Verdict::Anchor(ax, ay)
				} else if let Some(r) = video_rect() {
					let dragging = !g.held_buttons.is_empty();
					let inside = geom::contains(r, x, y);
					if free_mode() {
						if !dragging && inside && over_ovbtn(x, y) {
							jobs.push(Job::Hover);
						} else if !inside {
							if dragging {
								let (nx, ny) = geom::normalize(r, x, y);
								s.send(Out::Motion { x: nx, y: ny });
							} else {
								jobs.push(Job::Hover);
							}
						} else {
							let (nx, ny) = geom::normalize(r, x, y);
							s.send(Out::Motion { x: nx, y: ny });
						}
						Verdict::Pass
					} else if inside {
						let (nx, ny) = geom::normalize(r, x, y);
						s.send(Out::Motion { x: nx, y: ny });
						Verdict::Pass
					} else {
						let (cx, cy) = geom::clamp_into(r, x, y);
						let (nx, ny) = geom::normalize(r, cx, cy);
						s.send(Out::Motion { x: nx, y: ny });
						Verdict::Clamp(cx, cy)
					}
				} else {
					Verdict::Pass // no video on screen to map onto
				}
			}
			MouseEv::Button { button, down, x, y, hit_ours } => {
				if button > 2 {
					// No host injector knows back/forward buttons: never act locally while
					// controlling, never forward.
					Verdict::Swallow
				} else if down
					&& !hit_ours
					&& !relative_mode()
					&& !video_rect().is_some_and(|r| geom::contains(r, x, y))
				{
					// A click on another app (the cursor got out — free cursor, a lagging
					// confine): implicit release, the click acts locally (Parsec-style).
					ENGAGED.store(false, SeqCst);
					jobs.push(Job::Released("click outside Pulsar"));
					Verdict::Pass
				} else if down {
					g.held_buttons.insert(button);
					s.send(Out::Button { button, down });
					Verdict::Swallow
				} else if !g.held_buttons.remove(&button) {
					// An up whose down was not forwarded (the engaging click): local only.
					Verdict::Pass
				} else {
					let released_all = g.held_buttons.is_empty();
					s.send(Out::Button { button, down });
					if released_all && free_mode() {
						jobs.push(Job::Hover); // a drag may have ended outside the video
					}
					Verdict::Swallow
				}
			}
			MouseEv::Scroll { dx, dy } => {
				if dx != 0.0 || dy != 0.0 {
					s.send(Out::Scroll { dx, dy });
				}
				Verdict::Swallow
			}
		}
	};
	for j in jobs {
		queue(j);
	}
	verdict
}

/// Free-cursor handover (worker): engaged + pointer outside the video + no button → release;
/// disengaged + pointer over the video + Pulsar frontmost → engage.
fn handle_hover() {
	if !free_mode() || !ENABLED.load(SeqCst) || !MOUSE_CAPTURE.load(SeqCst) {
		return;
	}
	let held = !state().held_buttons.is_empty();
	if !ENGAGED.load(SeqCst) && !APP_FOCUSED.load(SeqCst) {
		return;
	}
	let Some((x, y)) = os::cursor_pos() else { return };
	let inside = video_rect().is_some_and(|r| geom::contains(r, x, y)) && !over_ovbtn(x, y);
	if ENGAGED.load(SeqCst) {
		if !inside && !held {
			HOVER_ARMED.store(true, SeqCst);
			release(HOVER_LEAVE);
		}
		return;
	}
	if !inside {
		HOVER_ARMED.store(true, SeqCst);
		return;
	}
	if !HOVER_ARMED.load(SeqCst)
		|| OVERLAY_OPEN.load(SeqCst)
		|| STREAM_STALLED.load(SeqCst)
		|| os::buttons_down()
		|| os::frontmost_is_ours() == Some(false)
	{
		return;
	}
	let id = LAST_PLAY.load(SeqCst);
	HOVER_EDGE.store(HOVER_HINTED.swap(id, SeqCst) == id, SeqCst);
	tracing::info!("pointer entered the video → engage (free cursor)");
	engage_edge(None);
}

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
pub(crate) mod tests {
	use super::*;

	/// Global state: one test at a time.
	pub(crate) static SERIAL: Mutex<()> = Mutex::new(());

	#[derive(Default)]
	struct Rec {
		out: Mutex<Vec<Out>>,
		sig: Mutex<Vec<Signal>>,
	}
	impl Sink for Rec {
		fn send(&self, ev: Out) {
			self.out.lock().unwrap().push(ev);
		}
		fn signal(&self, s: Signal) {
			self.sig.lock().unwrap().push(s);
		}
	}
	impl Rec {
		fn take_out(&self) -> Vec<Out> {
			std::mem::take(&mut *self.out.lock().unwrap())
		}
		fn take_sig(&self) -> Vec<Signal> {
			std::mem::take(&mut *self.sig.lock().unwrap())
		}
	}

	/// Arm a session with a running "tap" and a known video rect.
	fn arm(id: u64, remote: bool, free: bool) -> Arc<Rec> {
		let rec = Arc::new(Rec::default());
		disable();
		ENGAGED.store(false, SeqCst);
		ENGAGED_MEMO.store(false, SeqCst);
		*lock(&ENGAGED_AT) = None;
		LAST_PLAY.store(MAX, SeqCst);
		KIOSK_ENGAGE.store(false, SeqCst);
		POINTER_LOCK.store(false, SeqCst);
		POINTER_LOCK_REPORTED.store(false, SeqCst);
		APP_FOCUSED.store(true, SeqCst);
		*lock(&OVBTN_FRAC) = None;
		enable(rec.clone(), Arm { id, mouse: true, remote, free_cursor: free });
		// The stub OS layer has no tap; pretend it runs.
		TAP_OK.store(true, SeqCst);
		set_geo(Geo { video: Some((100.0, 100.0, 201.0, 201.0)), render: Some((100.0, 100.0, 201.0, 201.0)) });
		rec.take_sig();
		rec
	}

	fn engaged_session(remote: bool, free: bool) -> Arc<Rec> {
		let rec = arm(7, remote, free);
		engage();
		assert!(is_engaged());
		assert_eq!(rec.take_sig(), vec![Signal::Engaged]);
		rec
	}

	#[test]
	fn the_overlay_combo_fires_again_while_its_modifiers_stay_down() {
		let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
		let rec = engaged_session(true, false);
		handle_key(29, true);
		handle_key(42, true);
		assert!(handle_key(50, true), "Ctrl+Shift+M is the overlay combo");
		assert!(!ENGAGED.load(SeqCst), "the combo releases control");
		assert_eq!(rec.take_sig(), vec![Signal::Released, Signal::OverlayToggle(true)]);
		// The chord's own down-strokes were on the host: released by the flush.
		let ups = rec.take_out();
		assert!(ups.contains(&Out::Key { code: 29, down: false }));
		assert!(ups.contains(&Out::Key { code: 42, down: false }));
		handle_key(50, false);
		// Second M with the chord still held: the menu toggles again (kept chord).
		assert!(handle_key(50, true));
		assert_eq!(rec.take_sig(), vec![Signal::OverlayToggle(false)]);
		handle_key(50, false);
		// Shift comes up for real: Ctrl+M is no combo, a bare M neither.
		handle_key(42, false);
		assert!(!handle_key(50, true));
		handle_key(50, false);
		handle_key(29, false);
		assert!(!handle_key(50, true));
		handle_key(50, false);
		// The kept chord ends with the focus.
		engage();
		handle_key(29, true);
		handle_key(42, true);
		assert!(handle_key(50, true));
		handle_key(50, false);
		focus_lost();
		APP_FOCUSED.store(true, SeqCst);
		assert!(!handle_key(50, true), "a kept chord ends with the focus");
		handle_key(50, false);
		disable();
	}

	#[test]
	fn combos_end_fullscreen_detach() {
		let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
		let rec = engaged_session(true, false);
		// Cmd+Shift+F11 = fullscreen, control stays.
		handle_key(125, true);
		handle_key(42, true);
		assert!(handle_key(87, true));
		assert_eq!(rec.take_sig(), vec![Signal::FullscreenToggle]);
		assert!(ENGAGED.load(SeqCst));
		// Autorepeat of the chorded F11 is swallowed, not forwarded.
		rec.take_out();
		assert!(handle_key(87, true));
		assert!(rec.take_out().is_empty());
		handle_key(87, false);
		handle_key(125, false);
		handle_key(42, false);
		rec.take_out();
		// Ctrl+Alt+Z = detach.
		handle_key(29, true);
		handle_key(56, true);
		assert!(handle_key(44, true));
		assert_eq!(rec.take_sig(), vec![Signal::Released, Signal::Detach]);
		assert!(!ENGAGED.load(SeqCst));
		handle_key(44, false);
		handle_key(29, false);
		handle_key(56, false);
		// Disengaged + frontmost: Ctrl+Shift+Q still ends the session.
		handle_key(29, true);
		handle_key(42, true);
		assert!(handle_key(16, true));
		assert_eq!(rec.take_sig(), vec![Signal::Leave]);
		handle_key(16, false);
		handle_key(29, false);
		handle_key(42, false);
		// Disengaged + another app frontmost: no combo at all.
		APP_FOCUSED.store(false, SeqCst);
		handle_key(29, true);
		handle_key(42, true);
		assert!(!handle_key(16, true));
		assert!(rec.take_sig().is_empty());
		handle_key(16, false);
		handle_key(29, false);
		handle_key(42, false);
		APP_FOCUSED.store(true, SeqCst);
		disable();
	}

	#[test]
	fn engaged_keys_forward_and_flush_on_release() {
		let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
		let rec = engaged_session(true, false);
		assert!(handle_key(30, true)); // A
		assert!(handle_key(125, true)); // Cmd
		assert!(handle_key(15, true)); // Tab — Cmd+Tab goes to the host
		assert_eq!(
			rec.take_out(),
			vec![
				Out::Key { code: 30, down: true },
				Out::Key { code: 125, down: true },
				Out::Key { code: 15, down: true }
			]
		);
		release("test");
		let mut ups = rec.take_out();
		ups.sort_by_key(|e| match e {
			Out::Key { code, .. } => *code,
			_ => 0,
		});
		assert_eq!(
			ups,
			vec![
				Out::Key { code: 15, down: false },
				Out::Key { code: 30, down: false },
				Out::Key { code: 125, down: false }
			]
		);
		assert_eq!(rec.take_sig(), vec![Signal::Released]);
		// Disengaged: plain keys stay local.
		assert!(!handle_key(30, true));
		handle_key(30, false);
		disable();
	}

	#[test]
	fn three_right_ctrl_releases() {
		let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
		let rec = engaged_session(true, false);
		for _ in 0..2 {
			handle_key(97, true);
			handle_key(97, false);
		}
		assert!(ENGAGED.load(SeqCst));
		handle_key(97, true);
		assert!(!ENGAGED.load(SeqCst));
		assert_eq!(rec.take_sig(), vec![Signal::Released, Signal::Detach]);
		disable();
	}

	#[test]
	fn suppression_rule() {
		let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
		let _rec = arm(3, true, false);
		ENGAGED.store(false, SeqCst);
		APP_FOCUSED.store(true, SeqCst);
		assert!(!key_suppress(50));
		tap_modifier(29, true);
		tap_modifier(42, true);
		assert!(key_suppress(50) && key_suppress(16) && key_suppress(87));
		assert!(!key_suppress(30), "only combo triggers are eaten while disengaged");
		APP_FOCUSED.store(false, SeqCst);
		assert!(!key_suppress(50), "never while another app is frontmost");
		tap_modifier(29, false);
		tap_modifier(125, true);
		APP_FOCUSED.store(true, SeqCst);
		assert!(key_suppress(87), "Cmd+Shift+F11");
		assert!(!key_suppress(50), "Cmd+Shift+M is not the menu combo");
		tap_modifier(125, false);
		tap_modifier(42, false);
		ENGAGED.store(true, SeqCst);
		assert!(key_suppress(30), "engaged: every key");
		ENGAGED.store(false, SeqCst);
		disable();
	}

	#[test]
	fn absolute_mouse_is_normalized_and_confined() {
		let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
		let rec = engaged_session(true, false);
		rec.take_out();
		assert_eq!(mouse(MouseEv::Move { x: 150.0, y: 150.0, dx: 1.0, dy: 0.0 }), Verdict::Pass);
		assert_eq!(rec.take_out(), vec![Out::Motion { x: 0.5, y: 0.5 }]);
		// Overshoot: delivered at the edge, the host pointer pinned there.
		assert_eq!(mouse(MouseEv::Move { x: 400.0, y: 150.0, dx: 9.0, dy: 0.0 }), Verdict::Clamp(200.0, 150.0));
		assert_eq!(rec.take_out(), vec![Out::Motion { x: 1.0, y: 0.5 }]);
		// Buttons and wheel act on the host only.
		let click = |down| MouseEv::Button { button: 0, down, x: 150.0, y: 150.0, hit_ours: true };
		assert_eq!(mouse(click(true)), Verdict::Swallow);
		assert_eq!(mouse(click(false)), Verdict::Swallow);
		assert_eq!(
			rec.take_out(),
			vec![Out::Button { button: 0, down: true }, Out::Button { button: 0, down: false }]
		);
		// An up whose down was never forwarded (the engaging click) stays local.
		assert_eq!(mouse(MouseEv::Button { button: 1, down: false, x: 150.0, y: 150.0, hit_ours: true }), Verdict::Pass);
		assert!(rec.take_out().is_empty());
		// Back/forward buttons: swallowed, not forwarded.
		assert_eq!(mouse(MouseEv::Button { button: 3, down: true, x: 150.0, y: 150.0, hit_ours: true }), Verdict::Swallow);
		assert_eq!(mouse(MouseEv::Scroll { dx: 0.0, dy: 30.0 }), Verdict::Swallow);
		assert_eq!(rec.take_out(), vec![Out::Scroll { dx: 0.0, dy: 30.0 }]);
		// A held drag is released on the host when control drops.
		mouse(MouseEv::Button { button: 1, down: true, x: 150.0, y: 150.0, hit_ours: true });
		rec.take_out();
		release("test");
		assert_eq!(rec.take_out(), vec![Out::Button { button: 1, down: false }]);
		disable();
	}

	#[test]
	fn click_outside_releases_and_passes() {
		let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
		let rec = engaged_session(true, false);
		let v = mouse(MouseEv::Button { button: 0, down: true, x: 900.0, y: 900.0, hit_ours: false });
		assert_eq!(v, Verdict::Pass);
		assert!(!ENGAGED.load(SeqCst));
		assert_eq!(rec.take_sig(), vec![Signal::Released]);
		assert!(!rec.take_out().iter().any(|e| matches!(e, Out::Button { down: true, .. })));
		disable();
	}

	#[test]
	fn game_mode_sends_relative_and_holds_the_cursor() {
		let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
		let rec = arm(9, false, false);
		// Click-to-engage on the video: engages, the click stays local, the host pointer
		// starts at the clicked spot.
		let v = mouse(MouseEv::Button { button: 0, down: true, x: 120.0, y: 180.0, hit_ours: true });
		assert_eq!(v, Verdict::Pass);
		assert!(ENGAGED.load(SeqCst));
		assert_eq!(rec.take_sig(), vec![Signal::Engaged]);
		assert_eq!(rec.take_out(), vec![Out::Motion { x: 0.2, y: 0.8 }]);
		assert_eq!(mouse(MouseEv::Button { button: 0, down: false, x: 120.0, y: 180.0, hit_ours: true }), Verdict::Pass);
		// Moves: relative deltas, the event dropped, the cursor kept at the anchor.
		let v = mouse(MouseEv::Move { x: 125.0, y: 180.0, dx: 5.0, dy: -2.0 });
		assert_eq!(v, Verdict::Anchor(120.0, 180.0));
		assert_eq!(rec.take_out(), vec![Out::Relative { dx: 5.0, dy: -2.0 }]);
		disable();
	}

	#[test]
	fn host_pointer_lock_switches_remote_mode_to_relative() {
		let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
		let rec = engaged_session(true, false);
		set_pointer_lock(7, true);
		assert!(relative_mode());
		rec.take_out();
		assert!(matches!(mouse(MouseEv::Move { x: 150.0, y: 150.0, dx: 3.0, dy: 4.0 }), Verdict::Anchor(..)));
		assert_eq!(rec.take_out(), vec![Out::Relative { dx: 3.0, dy: 4.0 }]);
		set_pointer_lock(7, false);
		assert!(!relative_mode());
		disable();
	}

	#[test]
	fn no_tap_no_engage() {
		let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
		let rec = arm(11, true, false);
		TAP_OK.store(false, SeqCst);
		engage();
		assert!(!is_engaged(), "without the OS capture the webview path stays in charge");
		assert!(rec.take_sig().is_empty());
		disable();
	}

	#[test]
	fn same_session_rearm_keeps_engagement() {
		let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
		let rec = engaged_session(true, false);
		// Tab switch: disable() then enable() for the same id restores control.
		disable();
		assert!(!is_engaged());
		enable(rec.clone(), Arm { id: 7, mouse: true, remote: true, free_cursor: false });
		assert!(is_engaged());
		// A different session starts disengaged.
		disable();
		*lock(&ENGAGED_AT) = None;
		enable(rec.clone(), Arm { id: 8, mouse: true, remote: true, free_cursor: false });
		assert!(!is_engaged());
		disable();
	}

	#[test]
	fn stalled_stream_blocks_engage() {
		let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
		let rec = engaged_session(true, false);
		set_stream_stalled(true);
		assert!(!is_engaged());
		assert_eq!(rec.take_sig(), vec![Signal::Released]);
		engage();
		assert!(!is_engaged());
		set_stream_stalled(false);
		engage();
		assert!(is_engaged());
		disable();
	}

	#[test]
	fn free_cursor_leaving_the_video_passes_locally() {
		let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
		let rec = engaged_session(true, true);
		rec.take_out();
		// Outside without a button: nothing forwarded, the cursor moves on.
		assert_eq!(mouse(MouseEv::Move { x: 400.0, y: 150.0, dx: 5.0, dy: 0.0 }), Verdict::Pass);
		assert!(rec.take_out().is_empty());
		// A drag that started inside keeps driving the host, pinned to the edge.
		mouse(MouseEv::Button { button: 0, down: true, x: 150.0, y: 150.0, hit_ours: true });
		rec.take_out();
		assert_eq!(mouse(MouseEv::Move { x: 400.0, y: 150.0, dx: 5.0, dy: 0.0 }), Verdict::Pass);
		assert_eq!(rec.take_out(), vec![Out::Motion { x: 1.0, y: 0.5 }]);
		disable();
	}
}
