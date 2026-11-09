//! Windows capture implementation: the Interception driver path (works under
//! ASTER multiseat) with a `WH_KEYBOARD_LL` hook fallback, plus the shared
//! key/mouse forwarding + leave-combo logic.

use super::*;
use libloading::Library;
use std::os::raw::c_void;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use tauri::Emitter;
use tokio::sync::mpsc::Sender;
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
	CallNextHookEx, DispatchMessageW, GetMessageW, PostThreadMessageW, SetWindowsHookExW,
	TranslateMessage, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED, LLKHF_UP,
	LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_LBUTTONDOWN,
	WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT,
	WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN,
};

mod keymap;
use keymap::{scancode_to_evdev, vk_to_evdev};
use windows_sys::Win32::UI::WindowsAndMessaging::{KillTimer, PeekMessageW, SetTimer, PM_NOREMOVE, WM_TIMER};


/// Mutable globals the callback reaches through a `OnceLock<Mutex<…>>`. The
/// callback is `extern "system"` and cannot capture, so everything it needs is
/// here. `tx` is a clone of the active play's input channel; swapping it (on
/// focus change between tabs) is just a re-store under the same lock.
struct Globals {
	tx: Option<Sender<InputEvent>>,
	app: Option<AppHandle>,
	hook_thread_id: u32,
	ctrl_down: bool,
	alt_down: bool,
	shift_down: bool,
	/// Recent RightCtrl press times for the 3×RightCtrl release combo (≤1 s window).
	rctrl_presses: Vec<std::time::Instant>,
	/// Every key currently held DOWN on the host (forwarded down, no up yet). Flushed
	/// as up-strokes whenever the gate closes (disengage/disable) — otherwise a combo
	/// like Ctrl+Shift+M leaves Ctrl+Shift stuck on the host (their up-strokes arrive
	/// after the gate closed and are never forwarded).
	held: std::collections::HashSet<u32>,
	/// Tracks which combo trigger keys (overlay/end/fullscreen/detach: evdev 50, 16, 88,
	/// 44) are currently physically held. Used to detect the DOWN EDGE so autorepeat
	/// does not re-fire each action on every repeated key event while the chord is held.
	/// insert() returns true only on the first press (false on subsequent autorepeats),
	/// making this a precise rising-edge detector. Cleared on the key-up event.
	combo_held: std::collections::HashSet<u32>,
	/// Combo trigger keys whose CURRENT physical press was the real chord (the
	/// modifiers were held at the down-edge). Used to suppress only chord
	/// autorepeats, never a bare M/Q/Z repeat the user is typing into the remote.
	/// Cleared on the key-up and on re-arm.
	combo_active: std::collections::HashSet<u32>,
	/// Every mouse BUTTON currently held DOWN on the host (0=left/1=right/2=middle).
	/// Same guarantee as `held` for buttons: a press-and-hold (drag) followed by a
	/// disengage edge (click-outside implicit release, 3×RightCtrl, Ctrl+Alt+Z,
	/// focus-loss, disable) would otherwise leave the button stuck DOWN on the host —
	/// the up-stroke never gets forwarded because the gate has already closed.
	held_buttons: std::collections::HashSet<u8>,
	/// Main Tauri window HWND (raw, as isize), cached on engage. The WH_MOUSE_LL hook
	/// (Interception-less path) normalizes the cursor's absolute screen position against
	/// this window's client rect to send absolute `PointerMotion` — reading it in the hook
	/// callback avoids a Tauri window lookup on the LowLevelHooksTimeout-bounded thread.
	video_hwnd: isize,
}

static GLOBALS: OnceLock<Mutex<Globals>> = OnceLock::new();
static ENABLED: AtomicBool = AtomicBool::new(false);
/// Set once the hook thread has installed the hook + recorded its thread id.
static THREAD_STARTED: AtomicBool = AtomicBool::new(false);
/// Click-to-engage gate (parity with the Linux evdev capture): the hook is ARMED
/// (ENABLED) for the whole native session, but until the user clicks the video it
/// neither forwards nor suppresses anything — the local desktop stays usable, and a
/// broken session can't trap the keyboard. Set by `engage()` (video click → renderer
/// `ov engage` line), cleared by `release_engage()` / the 3×RightCtrl combo.
static ENGAGED: AtomicBool = AtomicBool::new(false);

/// Hook armed AND engaged (the user is controlling) — drives the renderer's cursor
/// visibility (see `session_cmds::spawn_engage_sync`).
pub fn is_engaged() -> bool {
	ENABLED.load(Ordering::SeqCst) && ENGAGED.load(Ordering::SeqCst)
}
/// The play id the capture is currently armed for (same-session re-arm detection).
/// NOT reset by `disable()`: the real tab deactivate→reactivate path is the frontend
/// calling kbd_capture_stop()→disable() and THEN kbd_capture_start()→enable() for the
/// same id, so the engagement memory must survive disable() (see ENGAGED_MEMO).
static LAST_PLAY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);
/// Engagement intent for `LAST_PLAY`, remembered ACROSS `disable()`. disable() drops the
/// live ENGAGED (capture must stop on a stop), but a stop immediately followed by an
/// enable() for the SAME id (the tab-switch churn) restores the user's engagement from
/// here — otherwise same-session preservation never fires (ENABLED is already false by
/// the time enable() runs). A different id ignores this and starts disengaged.
static ENGAGED_MEMO: AtomicBool = AtomicBool::new(false);
/// When `engage()` last flipped ENGAGED on — lets `enable()` tell a fresh click-to-engage
/// (the renderer's click that raced the frontend's arm) from a stale flag.
static ENGAGED_AT: Mutex<Option<std::time::Instant>> = Mutex::new(None);

fn recently_engaged() -> bool {
	ENGAGED_AT
		.lock()
		.unwrap_or_else(|e| e.into_inner())
		.map(|t| t.elapsed() < std::time::Duration::from_secs(3))
		.unwrap_or(false)
}
/// CLI kiosk (`--connect`) auto-engages on the next `enable()` (same as Linux).
static KIOSK_ENGAGE: AtomicBool = AtomicBool::new(false);
/// True for the lifetime of a kiosk-started session: focus loss must NOT disengage
/// (the appliance runs nothing else, and the auto-launched window may never get
/// focus at all under focus-stealing prevention). Cleared on `disable()`.
static KIOSK_SESSION: AtomicBool = AtomicBool::new(false);
/// Any app window focused (from the Tauri focus map via `set_focused`) — gates the
/// disengaged-state combos (Ctrl+Shift+M) so a GLOBAL hook never reacts while the
/// user is in another application.
static APP_FOCUSED: AtomicBool = AtomicBool::new(true);

fn globals() -> &'static Mutex<Globals> {
	GLOBALS.get_or_init(|| {
		Mutex::new(Globals {
			tx: None,
			app: None,
			hook_thread_id: 0,
			ctrl_down: false,
			alt_down: false,
			shift_down: false,
			rctrl_presses: Vec::new(),
			held: std::collections::HashSet::new(),
			held_buttons: std::collections::HashSet::new(),
			combo_held: std::collections::HashSet::new(),
			combo_active: std::collections::HashSet::new(),
			video_hwnd: 0,
		})
	})
}

/// Forward one input event to the session.
///
/// The 256-slot input channel is drained one-await-per-event by the hold loop, so a
/// congested link can fill it. A bare `try_send` would then DROP the just-arrived
/// event — fatal if it's a button/key UP (a stuck key/button on the host that never
/// recovers mid-session). So coalescible motion may drop, but press/release EDGES
/// `blocking_send` to wait for a slot. The hook + Interception capture run on plain
/// `std::thread::spawn` threads (never a tokio worker), so blocking here is safe.
/// Hand an input event to the forwarder thread. NEVER blocks: `fwd` runs on the
/// WH_KEYBOARD_LL / WH_MOUSE_LL callback thread, and a callback that stalls past
/// LowLevelHooksTimeout (~300 ms) makes Windows SILENTLY REMOVE that hook. The old
/// `blocking_send` here did exactly that whenever the session channel was full — and the
/// pointer-motion try_sends keep it full — so the first key after a burst of mouse moves
/// killed the keyboard hook for good (mouse still working, no keys, no Ctrl+Alt+Z).
/// One unbounded queue + one thread keeps the events in order; the thread does the
/// (possibly blocking) send, still dropping coalescible motion when the channel is full.
fn fwd(tx: &Sender<InputEvent>, ev: InputEvent) {
	use std::sync::mpsc;
	use std::sync::OnceLock;
	static Q: OnceLock<Mutex<mpsc::Sender<(Sender<InputEvent>, InputEvent)>>> = OnceLock::new();
	let q = Q.get_or_init(|| {
		let (qtx, qrx) = mpsc::channel::<(Sender<InputEvent>, InputEvent)>();
		std::thread::Builder::new()
			.name("kbdhook-fwd".into())
			.spawn(move || {
				for (tx, ev) in qrx {
					if crate::util::is_coalescible_input(&ev) {
						let _ = tx.try_send(ev);
					} else {
						if matches!(ev, InputEvent::Key { .. }) {
							FWD_KEYS.fetch_add(1, Ordering::Relaxed);
						}
						if tx.blocking_send(ev).is_err() {
							tracing::warn!("kbd forwarder: session channel closed");
						}
					}
				}
			})
			.expect("spawn kbdhook forwarder");
		Mutex::new(qtx)
	});
	let _ = q.lock().unwrap_or_else(|e| e.into_inner()).send((tx.clone(), ev));
}

/// Windows switches the keyboard layout on a bare Ctrl+Shift (and the input language on
/// Alt+Shift): modifiers pressed and released with no other key seen in between. While merely
/// app-focused the modifiers of a Ctrl+Shift combo DO reach the OS, but the hook swallows the
/// trigger key — so releasing them flipped the layout, and the layout switcher took the
/// foreground (seen as "focus lost to ForegroundStaging" right after a Ctrl+Shift+M). A
/// synthetic tap of an unassigned key in between breaks that sequence (the "menu mask key"
/// trick). Injected, so our hooks pass it through; nothing maps it, so nobody acts on it.
fn mask_modifier_hotkey() {
	use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
		SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
	};
	const VK_MASK: u16 = 0xE8; // unassigned
	let key = |flags| INPUT {
		r#type: INPUT_KEYBOARD,
		Anonymous: INPUT_0 {
			ki: KEYBDINPUT { wVk: VK_MASK, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 },
		},
	};
	let inputs = [key(0), key(KEYEVENTF_KEYUP)];
	// SAFETY: two well-formed keyboard INPUTs, the count and struct size match.
	unsafe {
		SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32);
	}
}

/// Send up-strokes for every key still held on the host and clear the set — called
/// whenever forwarding stops (disengage, disable) so no modifier stays stuck remotely.
///
/// MUST NOT be called while holding the `globals()` MutexGuard on the WH_KEYBOARD_LL
/// hook callback thread: `fwd` uses `blocking_send` for key/button UP edges, which would
/// stall the OS input queue (LowLevelHooksTimeout ~300 ms). Use `flush_held_wh` on the
/// hook thread instead.
fn flush_held(g: &mut Globals) {
	if let Some(tx) = &g.tx {
		for code in g.held.drain() {
			fwd(tx, InputEvent::Key { code, down: false });
		}
		// Same for held mouse buttons (drag-and-hold left/right/middle): release each
		// before the gate closes so a click-outside / leave-combo can't leave the host
		// with a stuck button (runaway drag-select). Mirrors the Linux flush_held.
		for button in g.held_buttons.drain() {
			fwd(tx, InputEvent::PointerButton { button, down: false });
		}
	} else {
		g.held.clear();
		g.held_buttons.clear();
	}
	// Drop the LOCAL chord-modifier tracking too. The combos (Ctrl+Shift+M / Q / F12)
	// and the Ctrl+Alt+Z / 3×RightCtrl release gate on these booleans, and unlike Linux
	// (which re-queries the kernel via `chord_mods` at the trigger moment) Windows trusts
	// them — so a modifier whose UP-stroke is never observed after a disengage edge (the
	// physical key was released while the hook was bypassed, suppressed, or the event was
	// injected) would stay latched true and let a BARE later key mis-fire a combo while
	// merely app-focused. flush_held runs on EVERY disengage/teardown edge, so resetting
	// here keeps the combo gate from ever acting on a stale modifier after control drops.
	g.ctrl_down = false;
	g.alt_down = false;
	g.shift_down = false;
	g.rctrl_presses.clear();
}

/// Variant of `flush_held` safe to call on the WH_KEYBOARD_LL hook callback thread.
///
/// Drains `held`/`held_buttons` and resets modifier state UNDER the lock (via the
/// caller's `MutexGuard`), then returns the sender + collected events so the caller can
/// drop the guard BEFORE sending.  The actual sends (done by the caller with the guard
/// released) use `try_send` — bounded, non-blocking — which is the correct discipline
/// for the LL hook callback thread where ANY blocking stalls the OS input queue and
/// trips `LowLevelHooksTimeout` (~300 ms).  Up-strokes are never coalescible, so
/// `try_send` may drop them if the channel is full (256 slots); however, a combo fires
/// exactly once, the held set is bounded by physically-reachable keys (≤10 in practice),
/// and a full channel under extreme congestion is preferable to freezing the OS.
fn flush_held_wh(g: &mut std::sync::MutexGuard<'_, Globals>) -> (Option<tokio::sync::mpsc::Sender<InputEvent>>, Vec<InputEvent>) {
	let tx = g.tx.clone();
	let mut events: Vec<InputEvent> = Vec::with_capacity(g.held.len() + g.held_buttons.len());
	for code in g.held.drain() {
		events.push(InputEvent::Key { code, down: false });
	}
	for button in g.held_buttons.drain() {
		events.push(InputEvent::PointerButton { button, down: false });
	}
	// Reset modifier tracking (same as flush_held — stale modifier booleans can mis-fire
	// combos on a later bare keypress while merely app-focused).
	g.ctrl_down = false;
	g.alt_down = false;
	g.shift_down = false;
	g.rctrl_presses.clear();
	(tx, events)
}

/// Send the up-stroke events collected by `flush_held_wh` AFTER the globals lock has
/// been dropped.  Uses `try_send` (non-blocking) — see `flush_held_wh` for rationale.
fn send_flush_events(tx: Option<tokio::sync::mpsc::Sender<InputEvent>>, events: Vec<InputEvent>) {
	if let Some(tx) = tx {
		for ev in events {
			let _ = tx.try_send(ev);
		}
	}
}

/// Arm kiosk auto-engage: the next `enable()` engages immediately (CLI `--connect`).
pub(super) fn arm_kiosk() {
	KIOSK_ENGAGE.store(true, Ordering::SeqCst);
}

/// Cursor-confine rectangle (screen px, LTRB) re-asserted on every mouse event while
/// engaged — the OS clears ClipCursor on focus changes / display events, so a one-shot
/// clip wouldn't survive. `None` = not confined (released). Set by `confine_to_video`.
static CONFINE: Mutex<Option<(i32, i32, i32, i32)>> = Mutex::new(None);

/// The Pulsar main window while confined (the renderer child is looked up under it).
static VIDEO_MAIN: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
/// Where the video sits inside the renderer window, as client fractions (from the renderer's
/// `vidsink-vrect` line). `None` = not reported yet → the whole renderer window.
static VIDEO_FRAC: Mutex<Option<(f32, f32, f32, f32)>> = Mutex::new(None);

pub fn set_video_frac(x0: f32, y0: f32, x1: f32, y1: f32) {
	*VIDEO_FRAC.lock().unwrap_or_else(|e| e.into_inner()) = Some((x0, y0, x1, y1));
}

/// The on-screen rectangle (screen px, LTRB) the video is actually drawn in: the renderer
/// child window (`PulsarRenderChild`) narrowed to its picture area (letterbox bars / stretch /
/// rotation as the renderer reports them). Falls back to the main window's client area.
/// `None` when nothing usable is on screen (inactive tab parks the child 1x1 offscreen).
fn content_rect() -> Option<(i32, i32, i32, i32)> {
	use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
	use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
	use windows_sys::Win32::UI::WindowsAndMessaging::{
		FindWindowExW, GetClientRect, GetWindowRect, IsWindowVisible,
	};
	let main = VIDEO_MAIN.load(Ordering::SeqCst) as HWND;
	if main.is_null() {
		return None;
	}
	let class: Vec<u16> = "PulsarRenderChild\0".encode_utf16().collect();
	let child = unsafe { FindWindowExW(main, std::ptr::null_mut(), class.as_ptr(), std::ptr::null()) };
	let mut rc: RECT = unsafe { std::mem::zeroed() };
	let (l, t, r, b, frac) = if !child.is_null() && unsafe { IsWindowVisible(child) } != 0 {
		if unsafe { GetWindowRect(child, &mut rc) } == 0 {
			return None;
		}
		let frac = *VIDEO_FRAC.lock().unwrap_or_else(|e| e.into_inner());
		(rc.left, rc.top, rc.right, rc.bottom, frac)
	} else {
		if unsafe { GetClientRect(main, &mut rc) } == 0 {
			return None;
		}
		let mut tl = POINT { x: rc.left, y: rc.top };
		let mut br = POINT { x: rc.right, y: rc.bottom };
		unsafe {
			ClientToScreen(main, &mut tl);
			ClientToScreen(main, &mut br);
		}
		(tl.x, tl.y, br.x, br.y, None)
	};
	let (w, h) = ((r - l) as f32, (b - t) as f32);
	if w < 8.0 || h < 8.0 {
		return None;
	}
	let (fx0, fy0, fx1, fy1) = match frac {
		Some((a, b2, c, d)) if c > a && d > b2 => (a.clamp(0.0, 1.0), b2.clamp(0.0, 1.0), c.clamp(0.0, 1.0), d.clamp(0.0, 1.0)),
		_ => (0.0, 0.0, 1.0, 1.0),
	};
	let rect = (
		l + (w * fx0).round() as i32,
		t + (h * fy0).round() as i32,
		l + (w * fx1).round() as i32,
		t + (h * fy1).round() as i32,
	);
	if rect.2 - rect.0 < 8 || rect.3 - rect.1 < 8 {
		return None;
	}
	Some(rect)
}

/// Re-measure the video rect and re-apply the clip (the OS drops ClipCursor on focus and
/// display changes; the window can move, resize or go fullscreen). Runs from hook_thread's
/// timer — never inside a hook callback. No-op when not confined.
fn reassert_confine() {
	use windows_sys::Win32::Foundation::RECT;
	use windows_sys::Win32::UI::WindowsAndMessaging::ClipCursor;
	if CONFINE.lock().unwrap().is_none() {
		return;
	}
	let Some((l, t, r, b)) = content_rect() else {
		return;
	};
	*CONFINE.lock().unwrap() = Some((l, t, r, b));
	let rc = RECT {
		left: l,
		top: t,
		right: r,
		bottom: b,
	};
	unsafe {
		ClipCursor(&rc);
	}
}

/// Free the cursor (ClipCursor(NULL)) and forget the confine rect — for disengage paths
/// that don't go through `release_engage` (disable / implicit click-outside release).
fn clear_confine() {
	use windows_sys::Win32::UI::WindowsAndMessaging::ClipCursor;
	*CONFINE.lock().unwrap() = None;
	unsafe {
		ClipCursor(std::ptr::null());
	}
}

/// Confine the cursor to the VIDEO itself while engaged — the picture area of the renderer
/// window (letterbox bars excluded), so the pointer can neither leave Pulsar nor wander onto
/// the app's own tabs/chrome; Ctrl+Alt+Z / 3xRightCtrl release it. `on=false` frees it
/// (ClipCursor(NULL)). The rect is re-measured by `reassert_confine` every 500 ms.
fn confine_to_video(app: &AppHandle, on: bool) {
	use tauri::Manager as _;
	use windows_sys::Win32::Foundation::{HWND, RECT};
	use windows_sys::Win32::UI::WindowsAndMessaging::ClipCursor;

	if !on {
		*CONFINE.lock().unwrap() = None;
		globals().lock().unwrap().video_hwnd = 0;
		unsafe {
			ClipCursor(std::ptr::null());
		}
		return;
	}
	let Some(main_hwnd) = app
		.get_webview_window("main")
		.and_then(|w| w.hwnd().ok())
		.map(|h| h.0 as HWND)
	else {
		return;
	};
	// Cache the window for the WH_MOUSE_LL absolute-pointer normalization (see handle_mouse_wh).
	globals().lock().unwrap().video_hwnd = main_hwnd as isize;
	VIDEO_MAIN.store(main_hwnd as isize, Ordering::SeqCst);
	let Some((l, t, r, b)) = content_rect() else {
		return;
	};
	*CONFINE.lock().unwrap() = Some((l, t, r, b));
	let rc = RECT {
		left: l,
		top: t,
		right: r,
		bottom: b,
	};
	unsafe {
		ClipCursor(&rc);
	}
}

/// Take control: start forwarding + suppressing input. Idempotent; emits
/// `kbd-engaged` on the rising edge (drives the UI hint + renderer cursor state).
/// The stream is stopped (renderer `ov stalled 1`): no engaging until it is back.
static STREAM_STALLED: AtomicBool = AtomicBool::new(false);

pub(super) fn set_stream_stalled(app: &AppHandle, stalled: bool) {
	if STREAM_STALLED.swap(stalled, Ordering::SeqCst) != stalled {
		tracing::info!(stalled, "stream stopped/resumed → input capture {}", if stalled { "released + locked" } else { "allowed" });
	}
	if stalled {
		release_engage_why(app, "stream stopped");
	}
}

pub(super) fn engage(app: &AppHandle) {
	if STREAM_STALLED.load(Ordering::SeqCst) {
		tracing::info!("engage refused: the stream is stopped");
		return;
	}
	if !ENGAGED.swap(true, Ordering::SeqCst) {
		*ENGAGED_AT.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
		tracing::info!(kcalls = KCALLS.load(Ordering::Relaxed), app_focused = APP_FOCUSED.load(Ordering::SeqCst), "kbd capture ENGAGED");
		// Trap the cursor to the streamed screen so it can't roam to another monitor.
		confine_to_video(app, true);
		let _ = app.emit("kbd-engaged", ());
	}
}

/// Drop control WITHOUT ending the session (the user keeps the video and can click
/// back in). Emits `kbd-released` on the falling edge and releases any keys still
/// held on the host.
pub(super) fn release_engage(app: &AppHandle) {
	release_engage_why(app, "api");
}

/// `release_engage` with the caller named in the log line (focus loss vs overlay vs API).
pub(super) fn release_engage_why(app: &AppHandle, why: &str) {
	pinch_end(true);
	// Already disengaged (e.g. by a combo) but a clip is still in force: free it anyway.
	if !ENGAGED.load(Ordering::SeqCst) && CONFINE.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
		clear_confine();
	}
	if ENGAGED.swap(false, Ordering::SeqCst) {
		tracing::info!(why, kcalls = KCALLS.load(Ordering::Relaxed), "kbd capture RELEASED");
		confine_to_video(app, false); // free the cursor — local desktop usable again
		// Collect flush events under the lock, then drop it BEFORE sending — `fwd`
		// uses `blocking_send` which PANICS when called from a tokio worker thread
		// (set_overlay / kbd_capture_stop are async Tauri commands). Use the
		// non-blocking flush_held_wh + send_flush_events pattern throughout.
		let (tx, events) = flush_held_wh(&mut globals().lock().unwrap_or_else(|e| e.into_inner()));
		send_flush_events(tx, events);
		let _ = app.emit("kbd-released", ());
	}
}

// ─────────────────────────── Interception path ───────────────────────────
// On machines where the Interception driver is installed (incl. ASTER multiseat,
// where WH_KEYBOARD_LL is bypassed) we capture *below* the OS hook layer via the
// driver's user-mode DLL, loaded at runtime so the app still runs without it.

type ItCreate = unsafe extern "C" fn() -> *mut c_void;
type ItPredicate = extern "C" fn(i32) -> i32;
type ItSetFilter = unsafe extern "C" fn(*mut c_void, ItPredicate, u16);
type ItWait = unsafe extern "C" fn(*mut c_void) -> i32;
type ItReceive = unsafe extern "C" fn(*mut c_void, i32, *mut u8, u32) -> i32;
type ItSend = unsafe extern "C" fn(*mut c_void, i32, *const u8, u32) -> i32;

/// Resolved Interception entry points + a live context. `_lib` keeps the DLL
/// mapped (the fn pointers point into it). All calls happen on the capture
/// thread; the struct is treated as Send/Sync so it can live in a static.
struct Interception {
	_lib: Library,
	set_filter: ItSetFilter,
	wait: ItWait,
	receive: ItReceive,
	send: ItSend,
	is_kbd: ItPredicate,
	is_mouse: ItPredicate,
	ctx: *mut c_void,
}
unsafe impl Send for Interception {}
unsafe impl Sync for Interception {}

static INTERCEPT: OnceLock<Interception> = OnceLock::new();
/// 0 = undecided, 1 = Interception driver, 2 = WH_KEYBOARD_LL fallback.
static MECHANISM: AtomicU8 = AtomicU8::new(0);
/// Also capture the MOUSE (relative) — only in native-renderer mode, where there's
/// no webview canvas to read pointer events from.
static MOUSE_CAPTURE: AtomicBool = AtomicBool::new(false);

/// Turn the Interception keyboard (and, in native mode, mouse) filter on while
/// controlling, off otherwise. Only the controlling instance keeps it on, so
/// concurrent instances on the same box don't fight over input. No-op unless the
/// Interception mechanism is active.
fn set_capture_filter(on: bool) {
	if MECHANISM.load(Ordering::SeqCst) != 1 {
		return;
	}
	if let Some(it) = INTERCEPT.get() {
		// KEY_DOWN(0x01) | KEY_UP(0x02) while on; NONE(0x00) while off.
		unsafe { (it.set_filter)(it.ctx, it.is_kbd, if on { 0x0003 } else { 0x0000 }) };
		if MOUSE_CAPTURE.load(Ordering::SeqCst) {
			// FILTER_MOUSE_ALL (0xFFFF) captures move + buttons + wheel.
			unsafe { (it.set_filter)(it.ctx, it.is_mouse, if on { 0xFFFF } else { 0x0000 }) };
		}
	}
}

/// Load interception.dll and open a context. Returns None if the DLL is missing
/// or the driver isn't installed/active — the caller then falls back to the
/// WH_KEYBOARD_LL hook.
fn init_interception() -> Option<Interception> {
	// Search order: next to the exe / PATH (the dev layout), then the bundled copy
	// under resources/interception/ — the INSTALLED app ships it there (NSIS
	// `resources` list), and without this fallback an installed Pulsar silently
	// dropped to the WH_KEYBOARD_LL hook, which ASTER bypasses ("engaged but
	// nothing forwards" on multiseat machines).
	let lib = unsafe { Library::new("interception.dll") }.ok().or_else(|| {
		let p = std::env::current_exe()
			.ok()?
			.parent()?
			.join("resources")
			.join("interception")
			.join("interception.dll");
		unsafe { Library::new(p) }.ok()
	})?;
	unsafe {
		let create: ItCreate = *lib.get(b"interception_create_context\0").ok()?;
		let set_filter: ItSetFilter = *lib.get(b"interception_set_filter\0").ok()?;
		let wait: ItWait = *lib.get(b"interception_wait\0").ok()?;
		let receive: ItReceive = *lib.get(b"interception_receive\0").ok()?;
		let send: ItSend = *lib.get(b"interception_send\0").ok()?;
		let is_kbd: ItPredicate = *lib.get(b"interception_is_keyboard\0").ok()?;
		let is_mouse: ItPredicate = *lib.get(b"interception_is_mouse\0").ok()?;
		let ctx = create();
		if ctx.is_null() {
			return None;
		}
		Some(Interception {
			_lib: lib,
			set_filter,
			wait,
			receive,
			send,
			is_kbd,
			is_mouse,
			ctx,
		})
	}
}

/// Persistent capture thread for the Interception path. Mirrors the proven probe:
/// set the keyboard filter ONCE and **block** on `interception_wait` (the
/// `wait_with_timeout` variant proved unreliable on the installed driver — it
/// never delivered keys). Every key flows through here; we only *suppress + forward*
/// while ENABLED (controlling), otherwise we pass it straight through so local
/// typing is unaffected.
fn capture_thread() {
	let Some(it) = INTERCEPT.get() else { return };
	// NOTE: the filter is toggled by enable()/disable() (set only while THIS
	// instance is controlling), so non-controlling instances don't steal keys from
	// the active one (multi-instance on the same ASTER box). The thread just blocks
	// until a filtered key arrives.
	let mut stroke = [0u8; 24];
	loop {
		let device = unsafe { (it.wait)(it.ctx) }; // blocks until a keystroke
		if device <= 0 {
			continue;
		}
		let n = unsafe { (it.receive)(it.ctx, device, stroke.as_mut_ptr(), 1) };
		if n <= 0 {
			continue;
		}
		let armed = ENABLED.load(Ordering::SeqCst);
		let suppress = if !armed {
			false // not controlling → pass everything straight through
		} else if (it.is_kbd)(device) != 0 {
			// Keyboard stroke: code@0, state@2 (KEY_UP 0x01, E0 0x02, E1 0x04).
			let code = u16::from_le_bytes([stroke[0], stroke[1]]);
			let state = u16::from_le_bytes([stroke[2], stroke[3]]);
			if state & 0x04 != 0 {
				false // E1 (Pause/Break) 2-stroke seq we can't map cleanly — pass it
			} else {
				match scancode_to_evdev(code, state & 0x02 != 0) {
					Some(ev) => handle_key(ev, state & 0x01 == 0),
					None => false,
				}
			}
		} else {
			// Mouse stroke: state@0 (button/wheel flags), flags@2 (0x01=ABSOLUTE),
			// rolling@4 (wheel), x@8, y@12 (i32 deltas for relative mice).
			let mstate = u16::from_le_bytes([stroke[0], stroke[1]]);
			let mflags = u16::from_le_bytes([stroke[2], stroke[3]]);
			let rolling = i16::from_le_bytes([stroke[4], stroke[5]]);
			let x = i32::from_le_bytes([stroke[8], stroke[9], stroke[10], stroke[11]]);
			let y = i32::from_le_bytes([stroke[12], stroke[13], stroke[14], stroke[15]]);
			handle_mouse(mstate, mflags, rolling, x, y)
		};
		if !suppress {
			unsafe { (it.send)(it.ctx, device, stroke.as_ptr(), 1) };
		}
	}
}

/// Shared key handling for both capture paths: track modifiers, fire the leave
/// combo (Ctrl+Shift+F12 — Parsec-style Ctrl+Shift+key; F12 never occurs in
/// gameplay and the chord doesn't collide with any Win11 shell shortcut), forward
/// to the active session. Returns true if the key should be SUPPRESSED locally (it
/// then only acts on the remote).
fn handle_key(evdev: u32, down: bool) -> bool {
	let mut g = globals().lock().unwrap();
	match evdev {
		29 | 97 => g.ctrl_down = down,  // L/R Ctrl
		56 | 100 => g.alt_down = down,  // L/R Alt
		42 | 54 => g.shift_down = down, // L/R Shift
		_ => {}
	}
	// Rising-edge detection for combo trigger keys: insert() returns true only on the
	// FIRST press (autorepeat finds the key already in the set → returns false). The
	// else branch clears on key-up, which runs before the ENGAGED gate below, so the
	// set is always cleaned up even when the gate is not taken (e.g. disengaged state
	// where the combos still fire via APP_FOCUSED). Keeping this separate from `held`
	// avoids the dead-lock the combo branches would create: they return early BEFORE
	// the g.held update at the bottom, so a combo_edge check on `held` would never see
	// the first press (held lacks the key at combo-fire time for the same reason).
	let combo_key = matches!(evdev, 50 | 16 | 88 | 44);

	let combo_edge = if combo_key {
		if down {
			let edge = g.combo_held.insert(evdev);
			if edge {
				// Record whether this DOWN-edge is the real chord (modifiers held
				// now), so the key's autorepeat can be suppressed even after a combo's
				// flush_held_wh resets the modifier booleans — WITHOUT eating a BARE
				// M/Q/Z repeat meant for the remote (bare press → chord=false → not
				// recorded → its repeats forward). Mirrors linux.rs keeping the chord
				// test so a bare m/q/z still repeats.
				let chord = ((evdev == 50 || evdev == 16 || evdev == 88)
					&& g.ctrl_down
					&& g.shift_down)
					|| (evdev == 44 && g.ctrl_down && g.alt_down);
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
	// Suppress autorepeat of a combo trigger key while its chord is still held.
	// The down-edge already fired the action; subsequent OS autorepeat events have
	// combo_edge=false (insert() found the key already in combo_held). Without this
	// guard: F12 repeats forward to the host (ENGAGED stays true after fullscreen
	// toggle), and M repeats leak to the local desktop after ENGAGED is cleared.
	// Mirrors linux.rs:716-732 (ev.value()==2 while chord held → continue).
	if combo_key && down && !combo_edge
		&& g.combo_active.contains(&evdev)
		&& (ENGAGED.load(Ordering::SeqCst) || APP_FOCUSED.load(Ordering::SeqCst))
	{
		// Trigger key physically held and already fired its edge → suppress its
		// autorepeat. Two bugs this replaces:
		// (r17-kbd-autorepeat-suppress-unfocused) The old guard read modifier
		// booleans (ctrl_down && shift_down / ctrl_down && alt_down) but carried
		// no ENGAGED||APP_FOCUSED ownership condition, so when Pulsar was
		// disengaged AND another app had focus the hook still swallowed combo-key
		// (M/Q/F12/Z) autorepeats that belonged to the other app.
		// (r17-win-combo-autorepeat-leak) flush_held_wh (called synchronously in
		// the overlay/detach combo blocks) resets ctrl_down/shift_down/alt_down
		// to false; since modifier keys don't autorepeat, those booleans stayed
		// false after the combo fired, so the old 'chord' test was false → the
		// still-held M/Z autorepeats leaked to the local desktop.
		// FIX: combo_active (set only when the down-edge had the chord modifiers) is
		// the discriminator, NOT the modifier booleans (which flush_held_wh resets)
		// and NOT bare combo_held membership. So a BARE M/Q/Z repeat is never in
		// combo_active and forwards to the host normally — the host injects one
		// keydown per event and generates NO repeat of its own, so eating bare
		// repeats would drop remote characters (the r18 regression this fixes).
		return true;
	}
	// Overlay-toggle combo: Ctrl+Shift+M — must work BOTH engaged and merely
	// app-focused (disengaged): the user released control but still wants the menu.
	// Never fires while another app has focus (the hook is global).
	if combo_edge
		&& evdev == 50
		&& g.ctrl_down
		&& g.shift_down
		&& (ENGAGED.load(Ordering::SeqCst) || APP_FOCUSED.load(Ordering::SeqCst))
	{
		// Release control RIGHT HERE, synchronously. The overlay's own release
		// (set_overlay → kbdhook::release) arrives over a webview IPC roundtrip that
		// can lag SECONDS while the video child occludes the webview (WebView2
		// occlusion throttling) — until it landed, the user's next click was still
		// captured and went to the host ("first click swallowed").
		//
		// C16 fix: collect flush events + clone app handle under the lock, then DROP
		// the guard before sending — the WH_KEYBOARD_LL callback thread must not block
		// (LowLevelHooksTimeout ~300 ms), and flush sends must not hold the mutex
		// (blocks disable()/set_focused() during recovery).
		let (flush_tx, flush_evs, was_engaged, app_handle) = if ENGAGED.swap(false, Ordering::SeqCst) {
			tracing::info!("kbd capture RELEASED (overlay combo, immediate)");
			// Free the cursor too (the other release combos do): the menu must be usable and
			// the pointer able to leave the window; without this the clip stayed until an
			// Alt+Tab made Windows drop it.
			clear_confine();
			let (tx, evs) = flush_held_wh(&mut g);
			(tx, evs, true, g.app.clone())
		} else {
			(None, Vec::new(), false, g.app.clone())
		};
		drop(g);
		send_flush_events(flush_tx, flush_evs);
		if was_engaged {
			if let Some(app) = &app_handle {
				let _ = app.emit("kbd-released", ());
			}
		} else {
			mask_modifier_hotkey();
		}
		// Every press, engaged or not (the RELEASED line above only covers the engaged one):
		// "pressed twice before the menu opened" reports need both presses in the log.
		tracing::info!(was_engaged, "overlay combo → overlay-toggle");
		if let Some(app) = &app_handle {
			let _ = app.emit("overlay-toggle", ());
		}
		return true;
	}
	// End combo: Ctrl+Shift+Q — ends the session. Like the overlay combo it must work
	// BOTH engaged and merely app-focused (the user released control, video still up).
	if combo_edge
		&& evdev == 16
		&& g.ctrl_down
		&& g.shift_down
		&& (ENGAGED.load(Ordering::SeqCst) || APP_FOCUSED.load(Ordering::SeqCst))
	{
		// C16 fix: collect flush events under lock, drop guard, then send non-blocking.
		let (flush_tx, flush_evs) = flush_held_wh(&mut g);
		let app_handle = g.app.clone();
		drop(g);
		send_flush_events(flush_tx, flush_evs); // un-stick the chord on the (still-alive) host
		if !ENGAGED.load(Ordering::SeqCst) {
			mask_modifier_hotkey();
		}
		if let Some(app) = &app_handle {
			let _ = app.emit("kbd-leave", ());
		}
		return true; // never leak the Q locally
	}
	// Fullscreen combo: Ctrl+Shift+F12 — toggles the window's fullscreen state; the
	// session AND control state stay as they are (this is NOT the leave combo).
	if combo_edge
		&& evdev == 88
		&& g.ctrl_down
		&& g.shift_down
		&& (ENGAGED.load(Ordering::SeqCst) || APP_FOCUSED.load(Ordering::SeqCst))
	{
		// Emit OUTSIDE the globals lock: the mouse hook callback takes that lock, and a
		// Tauri IPC emit under it could stall the callback past LowLevelHooksTimeout.
		let app_handle = g.app.clone();
		drop(g);
		if !ENGAGED.load(Ordering::SeqCst) {
			mask_modifier_hotkey();
		}
		if let Some(app) = &app_handle {
			let _ = app.emit("fullscreen-toggle", ());
		}
		return true;
	}
	// Click-to-engage gate: armed but not engaged → behave as if no hook were
	// installed (no forward, no suppress) so local typing is unaffected.
	if !ENGAGED.load(Ordering::SeqCst) {
		return false;
	}
	// Release combo (matches the on-screen hint): 3×RightCtrl within 1 s DISENGAGES
	// capture (session stays alive; click the video to take control again).
	// Guard: only count true key-down edges (first press), NOT OS autorepeat.
	// g.held was populated by the forwarding path at the bottom for prior key-down events,
	// so on the FIRST physical press g.held lacks 97; every autorepeat finds it present
	// (it was inserted on the first press). This mirrors Linux linux.rs 'ev.value()==1'.
	if down && evdev == 97 && !g.held.contains(&97) {
		let now = std::time::Instant::now();
		g.rctrl_presses
			.retain(|t| now.duration_since(*t).as_millis() < 1000);
		g.rctrl_presses.push(now);
		if g.rctrl_presses.len() >= 3 {
			g.rctrl_presses.clear();
			// The combo's own down-stroke is on the host too — count it, then flush
			// everything held so nothing stays stuck remotely.
			// C16 fix: collect flush events + app handle under lock, drop guard BEFORE
			// sending — WH_KEYBOARD_LL callback thread must never block on a send.
			g.held.insert(97);
			let (flush_tx, flush_evs) = flush_held_wh(&mut g);
			ENGAGED.store(false, Ordering::SeqCst);
			tracing::info!("kbd capture RELEASED (3xRightCtrl)");
			clear_confine();
			let app_handle = g.app.clone();
			drop(g);
			send_flush_events(flush_tx, flush_evs);
			if let Some(app) = &app_handle {
				let _ = app.emit("kbd-released", ());
				// The USER detached (vs a focus-loss release): the webview drops its control too.
				let _ = app.emit("kbd-detach", ());
			}
			return true;
		}
	}
	// Release combo #2 (the one the engage hint advertises): Ctrl+Alt+Z (evdev 44)
	// — Parsec's detach shortcut, same behavior as 3×RightCtrl above.
	if combo_edge && evdev == 44 && g.ctrl_down && g.alt_down {
		ENGAGED.store(false, Ordering::SeqCst);
		tracing::info!("kbd capture RELEASED (Ctrl+Alt+Z)");
		clear_confine();
		// Un-stick everything held on the host (the chord's up-strokes won't be forwarded).
		// C16 fix: collect flush events + app handle under lock, drop guard BEFORE sending.
		let (flush_tx, flush_evs) = flush_held_wh(&mut g);
		let app_handle = g.app.clone();
		drop(g);
		send_flush_events(flush_tx, flush_evs);
		if let Some(app) = &app_handle {
			let _ = app.emit("kbd-released", ());
			let _ = app.emit("kbd-detach", ()); // user detach (see 3×RightCtrl)
		}
		return true; // never leak the Z locally
	}
	// Clone the sender and build the event BEFORE releasing the lock so the
	// held-set update is atomic with the send decision.  Then drop the lock and
	// call fwd() outside it.
	//
	// This matters on the WH_KEYBOARD_LL path: ll_keyboard_proc runs on the
	// hook callback thread, where ANY blocking is forbidden — it stalls the OS
	// input queue and trips LowLevelHooksTimeout (~300 ms).  fwd() calls
	// blocking_send for non-coalescible edges, so holding the globals() Mutex
	// while blocking also prevents disable()/flush_held()/set_focused() from
	// acquiring the lock (they need it for teardown/recovery).  Drop the guard
	// first so the lock is always free before any potentially-blocking send.
	if let Some(tx) = g.tx.clone() {
		// Track held-on-host keys for the disengage flush (under the lock).
		if down {
			g.held.insert(evdev);
		} else {
			g.held.remove(&evdev);
		}
		// Release the globals lock BEFORE any send — on the WH path this must
		// not block while the OS input queue is held (LL hook callback thread).
		drop(g);
		// Key repeat re-fires down=true; forward each (the host de-dupes).
		fwd(&tx, InputEvent::Key { code: evdev, down });
		return true;
	}
	false
}

/// Forward a captured mouse stroke (native-renderer mode) to the host as relative
/// motion + buttons + wheel, and suppress it locally (cursor lock — the mouse only
/// drives the remote). Returns true (suppress) whenever a session is active.
fn handle_mouse(state: u16, flags: u16, rolling: i16, x: i32, y: i32) -> bool {
	// Same click-to-engage gate as handle_key: not engaged → local mouse untouched.
	if !ENGAGED.load(Ordering::SeqCst) {
		return false;
	}
	let mut g = globals().lock().unwrap();
	if g.tx.is_none() {
		return false;
	}
	// A physical click while the LOCAL cursor sits OUTSIDE every Pulsar window must not
	// go to the host — the user is clicking another application. (This happens when the
	// device's MOVES bypass the driver capture — e.g. precision-touchpad pointer
	// injection — so the cursor roams freely while buttons are still intercepted.)
	// Implicit release, Parsec-style: disengage and pass the click through, so the
	// FIRST click switches apps instead of silently clicking the remote.
	if state & (0x001 | 0x004 | 0x010) != 0 {
		use windows_sys::Win32::System::Threading::GetCurrentProcessId;
		use windows_sys::Win32::UI::WindowsAndMessaging::{
			GetAncestor, GetCursorPos, GetWindowThreadProcessId, WindowFromPoint, GA_ROOT,
		};
		let mut pt = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
		if unsafe { GetCursorPos(&mut pt) } != 0 {
			let hw = unsafe { WindowFromPoint(pt) };
			// Compare the TOP-LEVEL ancestor's process: the window under the cursor over
			// the video is the pulsar-render CHILD (a different process), but its root is
			// our Tauri window — only a root owned by another app counts as "outside".
			let root = if hw.is_null() {
				hw
			} else {
				unsafe { GetAncestor(hw, GA_ROOT) }
			};
			let mut pid = 0u32;
			unsafe { GetWindowThreadProcessId(root, &mut pid) };
			if !root.is_null() && pid != unsafe { GetCurrentProcessId() } {
				tracing::info!("click outside Pulsar while engaged → implicit release, click passes through");
				ENGAGED.store(false, Ordering::SeqCst);
				clear_confine();
				let (flush_tx, flush_evs) = flush_held_wh(&mut g);
				let app_handle = g.app.clone();
				drop(g);
				send_flush_events(flush_tx, flush_evs);
				if let Some(app) = &app_handle {
					let _ = app.emit("kbd-released", ());
				}
				return false; // the click acts locally (activates the other app)
			}
		}
	}
	let Some(tx) = &g.tx else { return false };
	// Keep the cursor trapped to the streamed screen — the OS drops ClipCursor on focus
	// flaps (e.g. the render-child activation), so re-assert it on each mouse event.
	reassert_confine();
	// Relative movement (skip absolute-coordinate mice — 0x001 = ABSOLUTE flag).
	if flags & 0x001 == 0 && (x != 0 || y != 0) {
		let (dx, dy) = super::scale_relative(x as f64, y as f64);
		if dx != 0.0 || dy != 0.0 {
			let _ = tx.try_send(InputEvent::PointerRelative { dx, dy });
		}
	}
	// Buttons (Interception mouse-state bitflags → 0=left, 1=right, 2=middle).
	for (bit, button, down) in [
		(0x001u16, 0u8, true),
		(0x002, 0, false),
		(0x004, 1, true),
		(0x008, 1, false),
		(0x010, 2, true),
		(0x020, 2, false),
	] {
		if state & bit != 0 {
			if down {
				// Diagnostic: every locally-SUPPRESSED physical click while engaged.
				tracing::info!(button, "physical click suppressed (engaged) → forwarded to host");
			}
			fwd(tx, InputEvent::PointerButton { button, down });
		}
	}
	// Wheel: state carries the WHEEL bit (0x400), rolling is the signed delta.
	if state & 0x400 != 0 && rolling != 0 {
		let _ = tx.try_send(InputEvent::Scroll {
			dx: 0.0,
			dy: -(rolling as f64),
		});
	}
	// Track held buttons for the disengage flush (mirror `g.held` for keys). Done after
	// the `tx` borrow above so the disjoint `g.held_buttons` field can be mutated. A
	// press-and-hold (drag) leaves the button in the set until its up-stroke arrives;
	// if a disengage edge fires first, flush_held() releases it on the host.
	for (bit, button, down) in [
		(0x001u16, 0u8, true),
		(0x002, 0, false),
		(0x004, 1, true),
		(0x008, 1, false),
		(0x010, 2, true),
		(0x020, 2, false),
	] {
		if state & bit != 0 {
			if down {
				g.held_buttons.insert(button);
			} else {
				g.held_buttons.remove(&button);
			}
		}
	}
	true
}

/// WH_MOUSE_LL analogue of `handle_mouse` for the Interception-LESS path. The LL mouse
/// hook delivers ABSOLUTE screen coordinates (not the relative deltas Interception gives),
/// so we normalize the cursor position against the video window's client rect and send
/// absolute `PointerMotion` (0..1) — the same event the webview canvas used to send, which
/// the host injects as an absolute pointer. Without this, a machine without the Interception
/// driver captures NO mouse at all (the native renderer occludes the webview, so the JS
/// pointer path is dead too), leaving the viewer unable to control the remote. `msg` is the
/// WM_* message; `data` is MSLLHOOKSTRUCT.mouseData (wheel delta in the high word). Returns
/// true to SUPPRESS the event locally (engaged → the cursor only drives the remote).
/// Connected host injects real pinch gestures (`FEAT_PINCH`); set per session by play.rs.
static HOST_PINCH: AtomicBool = AtomicBool::new(false);
/// A touchpad pinch is being forwarded as `InputEvent::Pinch`.
static PINCH_ACTIVE: AtomicBool = AtomicBool::new(false);
/// Cumulative pinch scale (f64 bits) and when its last step arrived (ms, `now_ms`).
static PINCH_SCALE_BITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static PINCH_LAST_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Zoom per 120 units of the Ctrl + wheel Windows synthesizes for a touchpad pinch.
const PINCH_STEP: f64 = 1.12;
/// No pinch step for this long (and the synthesized Ctrl released) = the gesture ended.
const PINCH_IDLE_MS: u64 = 200;

pub fn set_host_pinch(on: bool) {
	HOST_PINCH.store(on, Ordering::SeqCst);
}

/// The host's cursor is hidden — a game grabbed the mouse and reads RELATIVE motion
/// (`DataMsg::PointerLock`). The hook path below positions the host pointer ABSOLUTELY, which
/// such a game turns into an endless spin; while this is on it sends raw-input deltas instead
/// (`raw_mouse_proc`) and freezes the local cursor. The Interception path is relative anyway.
static POINTER_LOCK: AtomicBool = AtomicBool::new(false);

pub fn set_pointer_lock(on: bool) {
	POINTER_LOCK.store(on, Ordering::SeqCst);
}

/// A message-only window on the hook thread that takes raw mouse input (`RIDEV_INPUTSINK`:
/// also while the renderer child — another process — holds the focus). Raw input carries the
/// device's own deltas, unclipped by the screen edge or the cursor confine.
unsafe fn raw_mouse_window(hmod: windows_sys::Win32::Foundation::HMODULE) {
	use windows_sys::Win32::UI::Input::{RegisterRawInputDevices, RAWINPUTDEVICE, RIDEV_INPUTSINK};
	use windows_sys::Win32::UI::WindowsAndMessaging::{
		CreateWindowExW, RegisterClassW, HWND_MESSAGE, WNDCLASSW,
	};
	let name: Vec<u16> = "PulsarRawMouse\0".encode_utf16().collect();
	let mut wc: WNDCLASSW = std::mem::zeroed();
	wc.lpfnWndProc = Some(raw_mouse_proc);
	wc.hInstance = hmod;
	wc.lpszClassName = name.as_ptr();
	RegisterClassW(&wc);
	let hwnd = CreateWindowExW(
		0,
		name.as_ptr(),
		name.as_ptr(),
		0,
		0,
		0,
		0,
		0,
		HWND_MESSAGE,
		std::ptr::null_mut(),
		hmod,
		std::ptr::null(),
	);
	if hwnd.is_null() {
		tracing::warn!("raw mouse window failed — no relative mouse for pointer-locked games");
		return;
	}
	let rid = RAWINPUTDEVICE { usUsagePage: 0x01, usUsage: 0x02, dwFlags: RIDEV_INPUTSINK, hwndTarget: hwnd };
	if RegisterRawInputDevices(&rid, 1, std::mem::size_of::<RAWINPUTDEVICE>() as u32) == 0 {
		tracing::warn!("RegisterRawInputDevices(mouse) failed — no relative mouse for pointer-locked games");
	}
}

unsafe extern "system" fn raw_mouse_proc(
	hwnd: windows_sys::Win32::Foundation::HWND,
	msg: u32,
	wp: WPARAM,
	lp: LPARAM,
) -> LRESULT {
	use windows_sys::Win32::UI::Input::{
		GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTHEADER, RID_INPUT, RIM_TYPEMOUSE,
	};
	use windows_sys::Win32::UI::WindowsAndMessaging::{DefWindowProcW, WM_INPUT};
	if msg == WM_INPUT
		&& POINTER_LOCK.load(Ordering::SeqCst)
		&& ENGAGED.load(Ordering::SeqCst)
		&& MOUSE_CAPTURE.load(Ordering::SeqCst)
		&& MECHANISM.load(Ordering::SeqCst) != 1
	{
		let mut raw: RAWINPUT = std::mem::zeroed();
		let mut size = std::mem::size_of::<RAWINPUT>() as u32;
		let got = GetRawInputData(
			lp as HRAWINPUT,
			RID_INPUT,
			&mut raw as *mut RAWINPUT as *mut core::ffi::c_void,
			&mut size,
			std::mem::size_of::<RAWINPUTHEADER>() as u32,
		);
		// MOUSE_MOVE_ABSOLUTE (0x01): a tablet / remote-desktop pointer — no deltas to give.
		if got != u32::MAX && raw.header.dwType == RIM_TYPEMOUSE && raw.data.mouse.usFlags & 0x01 == 0 {
			let (dx, dy) = (raw.data.mouse.lLastX, raw.data.mouse.lLastY);
			if dx != 0 || dy != 0 {
				// try_lock: never stall the hook thread (it also runs the LL hooks).
				let tx = globals().try_lock().ok().and_then(|g| g.tx.clone());
				if let Some(tx) = tx {
					let (dx, dy) = super::scale_relative(dx as f64, dy as f64);
					if dx != 0.0 || dy != 0.0 {
						fwd(&tx, InputEvent::PointerRelative { dx, dy });
					}
				}
			}
		}
	}
	DefWindowProcW(hwnd, msg, wp, lp)
}

fn now_ms() -> u64 {
	static T0: OnceLock<std::time::Instant> = OnceLock::new();
	T0.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

fn ctrl_down_locally() -> bool {
	unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(0x11) as u16 & 0x8000 != 0 }
}

/// Close a forwarded pinch. `force` = control is going away (no idle/Ctrl checks).
fn pinch_end(force: bool) {
	if !PINCH_ACTIVE.load(Ordering::SeqCst) {
		return;
	}
	if !force
		&& (now_ms().saturating_sub(PINCH_LAST_MS.load(Ordering::SeqCst)) < PINCH_IDLE_MS || ctrl_down_locally())
	{
		return;
	}
	if !PINCH_ACTIVE.swap(false, Ordering::SeqCst) {
		return;
	}
	let scale = f64::from_bits(PINCH_SCALE_BITS.load(Ordering::SeqCst));
	let tx = globals().lock().unwrap_or_else(|e| e.into_inner()).tx.clone();
	if let Some(tx) = tx {
		fwd(&tx, InputEvent::Pinch { phase: 2, scale });
	}
}

fn handle_mouse_wh(msg: u32, x: i32, y: i32, data: u32) -> bool {
	use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
	use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
	use windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect;
	// Only the native-renderer path captures the mouse (webview sessions read the pointer
	// from the canvas instead — mirrors the Interception filter gate at set_capture_filter).
	if !MOUSE_CAPTURE.load(Ordering::SeqCst) {
		return false;
	}
	if !ENGAGED.load(Ordering::SeqCst) {
		return false; // disengaged → local mouse untouched (click-to-engage gate)
	}
	// try_lock, never block: this runs inside the WH_MOUSE_LL callback (see ll_keyboard_proc
	// on why a stalled LL callback is fatal). Busy lock → drop this one event, still
	// suppressing it locally while engaged (a lost move is invisible, the next one lands).
	let Ok(mut g) = globals().try_lock() else {
		return msg != WM_MOUSEMOVE;
	};
	let Some(tx) = g.tx.clone() else {
		return false;
	};
	let hwnd = g.video_hwnd as HWND;
	// NO reassert_confine() here: ClipCursor needs the raw-input thread, which is blocked
	// waiting for THIS callback to return — so every engaged mouse move stalled the hook
	// thread until LowLevelHooksTimeout, and Windows silently REMOVED our keyboard hook
	// (keys never reached the remote, Ctrl+Alt+Z / 3×RightCtrl dead). The confine is
	// re-asserted from hook_thread's timer instead, outside any hook callback.
	match msg {
		// Pointer lock: the raw-input deltas drive the host; keep the local cursor still (a
		// game's view would otherwise also follow the absolute position).
		WM_MOUSEMOVE if POINTER_LOCK.load(Ordering::SeqCst) => return true,
		WM_MOUSEMOVE => {
			// Normalize against the VIDEO rect (the same one the cursor is clipped to): the
			// main window's client area also holds the tab bar, which skewed the remote
			// pointer. try_lock: never block in the hook callback.
			let video = CONFINE.try_lock().ok().and_then(|c| *c);
			if let Some((l, t, r, b)) = video.filter(|(l, t, r, b)| r - l > 1 && b - t > 1) {
				drop(g);
				let nx = ((x - l) as f64 / (r - l - 1) as f64).clamp(0.0, 1.0);
				let ny = ((y - t) as f64 / (b - t - 1) as f64).clamp(0.0, 1.0);
				fwd(&tx, InputEvent::PointerMotion { x: nx, y: ny });
				return false;
			}
			// Normalize the absolute screen position against the video window's client rect.
			if hwnd.is_null() {
				return true;
			}
			let normalized = unsafe {
				let mut rc: RECT = std::mem::zeroed();
				if GetClientRect(hwnd, &mut rc) == 0 || rc.right <= rc.left || rc.bottom <= rc.top {
					None
				} else {
					let mut tl = POINT { x: rc.left, y: rc.top };
					let mut br = POINT { x: rc.right, y: rc.bottom };
					ClientToScreen(hwnd, &mut tl);
					ClientToScreen(hwnd, &mut br);
					if br.x <= tl.x || br.y <= tl.y {
						None
					} else {
						let nx = ((x - tl.x) as f64 / (br.x - tl.x) as f64).clamp(0.0, 1.0);
						let ny = ((y - tl.y) as f64 / (br.y - tl.y) as f64).clamp(0.0, 1.0);
						Some((nx, ny))
					}
				}
			};
			drop(g);
			if let Some((nx, ny)) = normalized {
				fwd(&tx, InputEvent::PointerMotion { x: nx, y: ny }); // queued: keeps order vs clicks
			}
			// Let the move through locally. This path sends ABSOLUTE positions read from the
			// hook's `pt` — suppressing the move froze the real cursor, so every following
			// `pt` was the frozen spot + one tiny delta and the remote pointer only jittered
			// in place ("tries to move but doesn't"). The cursor stays trapped over the video
			// by reassert_confine() (ClipCursor) and hidden by the renderer while engaged.
			return false;
		}
		WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
		| WM_MBUTTONUP => {
			let (button, down) = match msg {
				WM_LBUTTONDOWN => (0u8, true),
				WM_LBUTTONUP => (0, false),
				WM_RBUTTONDOWN => (1, true),
				WM_RBUTTONUP => (1, false),
				WM_MBUTTONDOWN => (2, true),
				_ => (2, false),
			};
			if down {
				g.held_buttons.insert(button);
			} else if !g.held_buttons.remove(&button) {
				// An UP whose DOWN we never saw: the click-to-engage press went to the
				// renderer BEFORE control engaged. Let its release through locally (the
				// renderer/OS button state stays consistent) and don't forward it — the
				// engaging click must not reach the host at all.
				drop(g);
				return false;
			}
			drop(g);
			fwd(&tx, InputEvent::PointerButton { button, down });
		}
		WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
			// High word of mouseData = signed wheel delta (120 per notch; precision touchpads
			// send finer steps). The wire unit is BROWSER wheel pixels (100 per notch, +y =
			// down, +x = right), what the host's injector and the Linux path use. Sending raw
			// notches here (+1 = up) made Windows-client scrolling ~100x too slow AND inverted.
			let raw = (data >> 16) as i16 as f64;
			if raw == 0.0 {
				drop(g);
				return true;
			}
			let px = raw * 100.0 / 120.0;
			let (dx, dy) = if msg == WM_MOUSEHWHEEL { (px, 0.0) } else { (0.0, -px) };
			// Touchpad pinch-zoom arrives as Ctrl + wheel where Windows synthesizes the Ctrl
			// (an injected key our keyboard hooks pass through, so the host never sees it).
			// Ctrl down locally but not held on the host → wrap this wheel step in Ctrl so the
			// remote app zooms instead of scrolling. A physically held (forwarded) Ctrl is
			// already in `held`.
			let ctrl_local = ctrl_down_locally();
			let wrap_ctrl = ctrl_local && !g.held.contains(&29) && !g.held.contains(&97);
			drop(g);
			// Host injects real pinch gestures: forward the pinch itself (begin / scale / end)
			// instead of Ctrl + wheel. The end is detected by hook_thread's pinch timer.
			if wrap_ctrl && msg == WM_MOUSEWHEEL && HOST_PINCH.load(Ordering::SeqCst) {
				let factor = PINCH_STEP.powf(raw / 120.0);
				let was = PINCH_ACTIVE.swap(true, Ordering::SeqCst);
				let prev = if was { f64::from_bits(PINCH_SCALE_BITS.load(Ordering::SeqCst)) } else { 1.0 };
				let scale = (prev * factor).clamp(0.05, 20.0);
				PINCH_SCALE_BITS.store(scale.to_bits(), Ordering::SeqCst);
				PINCH_LAST_MS.store(now_ms(), Ordering::SeqCst);
				if !was {
					fwd(&tx, InputEvent::Pinch { phase: 0, scale: 1.0 });
				}
				fwd(&tx, InputEvent::Pinch { phase: 1, scale });
				return true;
			}
			if wrap_ctrl {
				fwd(&tx, InputEvent::Key { code: 29, down: true });
			}
			fwd(&tx, InputEvent::Scroll { dx, dy });
			if wrap_ctrl {
				fwd(&tx, InputEvent::Key { code: 29, down: false });
			}
		}
		_ => {
			drop(g);
		}
	}
	true // engaged → suppress locally; the event only acts on the remote
}

/// The LL mouse callback (Interception-less path). Runs on the hook thread for every mouse
/// event system-wide; forwards to `handle_mouse_wh` and suppresses locally when engaged.
unsafe extern "system" fn ll_mouse_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
	if ncode < 0 || !ENABLED.load(Ordering::SeqCst) {
		return CallNextHookEx(std::ptr::null_mut(), ncode, wparam, lparam);
	}
	let ms = &*(lparam as *const MSLLHOOKSTRUCT);
	// Never consume injected events (our own ClipCursor side effects, accessibility tools)
	// EXCEPT the wheel: Windows synthesizes touchpad pinch-zoom (Ctrl+wheel) and some
	// precision-touchpad scrolling as injected wheel input, and while control is engaged that
	// gesture belongs to the remote.
	let wheel = matches!(wparam as u32, WM_MOUSEWHEEL | WM_MOUSEHWHEEL);
	if ms.flags & LLMHF_INJECTED != 0 && !wheel {
		return CallNextHookEx(std::ptr::null_mut(), ncode, wparam, lparam);
	}
	if handle_mouse_wh(wparam as u32, ms.pt.x, ms.pt.y, ms.mouseData) {
		return 1; // SUPPRESS locally — the event only acts on the remote.
	}
	CallNextHookEx(std::ptr::null_mut(), ncode, wparam, lparam)
}

/// Arm capture for `tx`'s session. The first call picks a mechanism: the
/// Interception driver if present (works under ASTER), else WH_KEYBOARD_LL. Later
/// calls just swap in the active sender + app handle and flip ENABLED.
pub fn enable(app: AppHandle, tx: Sender<InputEvent>, mouse: bool, id: u64, _start_suspended: bool) {
	// A re-arm of the SAME live play session must preserve the user's engagement —
	// resetting it mid-control silently stopped forwarding with no UI cue (Linux
	// parity; see kbdhook/linux.rs enable). The real trigger is a disable()→enable()
	// pair (tab switch), so ENABLED is already false here — key same_session off
	// LAST_PLAY (which survives disable()), NOT off ENABLED.
	// (`start_suspended` is the Linux overlay-grab gate; Windows has no evdev grab —
	// the overlay floats over the live canvas — so it's unused here, kept for a shared
	// signature.)
	let same_session = LAST_PLAY.load(Ordering::SeqCst) == id;
	// A different session starts unlocked: a stopped-stream lock must not outlive the session
	// (its renderer reports the new stream's state itself).
	if !same_session {
		STREAM_STALLED.store(false, Ordering::SeqCst);
		// Same for a host pointer lock: the new host reports its own cursor state.
		POINTER_LOCK.store(false, Ordering::SeqCst);
	}
	tracing::info!(
		id,
		same_session,
		mouse,
		memo = ENGAGED_MEMO.load(Ordering::SeqCst),
		kiosk = KIOSK_ENGAGE.load(Ordering::SeqCst),
		"kbd capture enable()"
	);
	LAST_PLAY.store(id, Ordering::SeqCst);
	{
		let mut g = globals().lock().unwrap();
		g.tx = Some(tx);
		g.app = Some(app.clone());
		g.ctrl_down = false;
		g.alt_down = false;
		g.shift_down = false;
		g.rctrl_presses.clear();
		g.combo_held.clear();
		g.combo_active.clear();
	}
	// Sessions start DISENGAGED (click-to-engage) — except a kiosk `--connect` start,
	// which engages immediately like the Linux evdev path. KIOSK_ENGAGE is LATCHED
	// (not consumed): the frontend re-arms capture on tab-activation churn, and a
	// one-shot left that second enable() disengaged (see kbdhook/linux.rs).
	let kiosk = KIOSK_ENGAGE.load(Ordering::SeqCst);
	KIOSK_SESSION.store(kiosk, Ordering::SeqCst);
	if kiosk {
		engage(&app);
	} else if same_session {
		// Restore the engagement this id had when last active (disable() cleared the live
		// ENGAGED but stashed it in ENGAGED_MEMO). engage() emits kbd-engaged on the edge so
		// the renderer cursor/hint resyncs; a remembered-disengaged session stays off.
		// A LIVE engagement wins over a stale memo: a re-arm without a disable() in between
		// (effect re-run on the same tab) used to clear the user's control silently.
		if ENGAGED_MEMO.load(Ordering::SeqCst) {
			engage(&app);
		} else if ENGAGED.load(Ordering::SeqCst) {
			tracing::info!("kbd capture enable(): keeping the live engagement");
		}
	} else if ENGAGED.load(Ordering::SeqCst) && recently_engaged() {
		// New id, but the renderer's click-to-engage (`ov engage` → engage_render) landed a
		// moment BEFORE the frontend armed the capture — that click belongs to THIS session,
		// so don't throw it away (the user had to click twice). Anything older is stale.
		tracing::info!("kbd capture enable(): keeping the renderer's fresh engagement");
	} else {
		ENGAGED.store(false, Ordering::SeqCst);
	}
	// Native-renderer sessions also capture the mouse (relative); webview sessions
	// read the pointer from the canvas instead, so keep mouse capture off there.
	MOUSE_CAPTURE.store(mouse, Ordering::SeqCst);
	ENABLED.store(true, Ordering::SeqCst);
	match MECHANISM.load(Ordering::SeqCst) {
		0 => {
			// First arm — decide the capture mechanism once.
			if let Some(it) = init_interception() {
				tracing::info!("kbd capture mechanism: Interception driver");
				let _ = INTERCEPT.set(it);
				MECHANISM.store(1, Ordering::SeqCst);
				std::thread::spawn(capture_thread);
			} else {
				// Visible in the log on machines where this matters (ASTER multiseat
				// physical input is INVISIBLE to the LL hook — capture will look
				// engaged but forward nothing).
				tracing::warn!("kbd capture mechanism: WH_KEYBOARD_LL fallback (interception.dll not loaded)");
				MECHANISM.store(2, Ordering::SeqCst);
				THREAD_STARTED.store(true, Ordering::SeqCst);
				std::thread::spawn(hook_thread);
			}
		}
		2 => {
			// WH fallback tears its thread down on disable; re-spawn if needed.
			if !THREAD_STARTED.swap(true, Ordering::SeqCst) {
				std::thread::spawn(hook_thread);
			}
		}
		_ => {} // Interception: persistent thread; the filter is what arms it.
	}
	// Interception: turn the filter ON so this (controlling) instance captures.
	set_capture_filter(true);
}

/// Disarm: stop suppressing + forwarding, drop the sender, and tear the hook
/// thread down cleanly (posts WM_QUIT, which exits the pump and unhooks).
pub fn disable() {
	tracing::info!(engaged = ENGAGED.load(Ordering::SeqCst), kcalls = KCALLS.load(Ordering::Relaxed), "kbd capture disable()");
	pinch_end(true);
	ENABLED.store(false, Ordering::SeqCst);
	// Remember the engagement before dropping the live flag so a same-id re-arm (tab
	// switch = disable()→enable()) can restore it. LAST_PLAY is deliberately kept too;
	// a different id still starts disengaged.
	ENGAGED_MEMO.store(ENGAGED.load(Ordering::SeqCst), Ordering::SeqCst);
	ENGAGED.store(false, Ordering::SeqCst);
	clear_confine(); // free the cursor when the session is torn down / tab switched away
	KIOSK_SESSION.store(false, Ordering::SeqCst);
	// Interception: drop the filter so we stop capturing (frees keys for any other
	// instance + the local OS).
	set_capture_filter(false);
	let (tid, tx, flush_events) = {
		let mut g = globals().lock().unwrap_or_else(|e| e.into_inner());
		// Collect flush events under the lock (non-blocking — flush_held_wh uses
		// try_send-safe collection). Send them AFTER dropping the guard so that
		// blocking_send is never attempted from a tokio worker thread (kbd_capture_stop
		// is an async Tauri command; blocking_send panics when entered() == true and
		// would also poison the mutex).
		let (tx, flush_events) = flush_held_wh(&mut g);
		g.tx = None;
		// ctrl_down/alt_down/shift_down already reset by flush_held_wh above.
		let tid = g.hook_thread_id;
		(tid, tx, flush_events)
	};
	// Send outside the lock with non-blocking try_send (flush_held_wh collected them).
	send_flush_events(tx, flush_events);
	if tid != 0 {
		unsafe { PostThreadMessageW(tid, WM_QUIT, 0, 0) };
		THREAD_STARTED.store(false, Ordering::SeqCst);
	}
}

/// Overlay open/close. On Windows the webview HW-decodes and the overlay floats
/// over the still-live canvas, so there's no grab to release — capture stays ON
/// (Ctrl+Shift+M only opens the menu; see `handle_key`). No-op for grab state.
pub fn overlay_suspend(_suspend: bool) {}

/// Window focus changed. Losing ALL app focus while engaged must DISENGAGE the
/// capture (Linux parity): a global LL/Interception hook otherwise keeps eating the
/// keyboard while the user is in another app — the mouse looked free (they clicked
/// away) but every keystroke still went to the remote.
pub fn set_focused(focused: bool) {
	tracing::info!(focused, engaged = ENGAGED.load(Ordering::SeqCst), "app focus changed");
	// A click over the live video lands on the pulsar-render CHILD (a separate
	// process), which momentarily deactivates the Tauri webview window — Windows
	// reports focused=false then true within milliseconds. Disengaging on that flap
	// released the capture mid-click (host input devices re-armed → the click fell
	// into the recreate gap: "cursor moves but clicks do nothing"). Focus moving to
	// a window whose TOP-LEVEL root is still ours is NOT the user leaving the app.
	if !focused {
		match foreground_is_ours() {
			FgOwner::Ours => {
				tracing::info!("focus moved within our own window tree — keeping engagement");
				return;
			}
			FgOwner::None => {
				// GetForegroundWindow() is NULL while activation is in flight (the
				// child/webview handoff) — decide on a settled reading instead of
				// dropping control on a transient.
				tracing::info!("focus lost with no foreground window — deferring the decision");
				std::thread::spawn(|| {
					std::thread::sleep(std::time::Duration::from_millis(150));
					match foreground_is_ours() {
						FgOwner::Ours => tracing::info!("deferred focus check: ours — keeping engagement"),
						owner => {
							tracing::info!(?owner, "deferred focus check: foreign — releasing");
							focus_lost();
						}
					}
				});
				return;
			}
			owner => {
				tracing::info!(?owner, "focus lost to another process");
			}
		}
	}
	APP_FOCUSED.store(focused, Ordering::SeqCst);
	if !focused {
		focus_lost();
	}
}

/// Who owns the foreground window right now (diagnostic + the focus-loss gate).
#[derive(Debug)]
enum FgOwner {
	Ours,
	None,
	Other { pid: u32, class: String },
}

fn foreground_is_ours() -> FgOwner {
	use windows_sys::Win32::System::Threading::GetCurrentProcessId;
	use windows_sys::Win32::UI::WindowsAndMessaging::{
		GetAncestor, GetClassNameW, GetForegroundWindow, GetWindowThreadProcessId, GA_ROOT,
	};
	let fg = unsafe { GetForegroundWindow() };
	if fg.is_null() {
		return FgOwner::None;
	}
	let root = unsafe { GetAncestor(fg, GA_ROOT) };
	let root = if root.is_null() { fg } else { root };
	let mut pid = 0u32;
	unsafe { GetWindowThreadProcessId(root, &mut pid) };
	if pid == unsafe { GetCurrentProcessId() } {
		return FgOwner::Ours;
	}
	let mut buf = [0u16; 64];
	let n = unsafe { GetClassNameW(fg, buf.as_mut_ptr(), buf.len() as i32) };
	let class = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
	FgOwner::Other { pid, class }
}

/// The app really lost focus: drop the engagement (unless kiosk).
fn focus_lost() {
	APP_FOCUSED.store(false, Ordering::SeqCst);
	// Kiosk sessions ignore focus loss entirely (see KIOSK_SESSION).
	if KIOSK_SESSION.load(Ordering::SeqCst) {
		return;
	}
	if ENGAGED.load(Ordering::SeqCst) {
		let app = globals().lock().unwrap().app.clone();
		match app {
			Some(app) => release_engage_why(&app, "focus-loss"),
			None => ENGAGED.store(false, Ordering::SeqCst),
		}
	}
}

/// Owns the hooks for their lifetime: install → pump messages → uninstall.
///
/// The KEYBOARD hook lives on its OWN thread (`kbd_hook_thread`), the mouse hook on this one —
/// RustDesk's layout (rdev grab: keyboard-only hook thread, `KEYBOARD_ONLY`). Sharing one
/// thread, the keyboard hook stopped being called once control was engaged (the mouse hook
/// kept firing): keys reached the local webview but never the remote, and the release combos
/// (Ctrl+Alt+Z / 3×RightCtrl) were dead.
fn hook_thread() {
	unsafe {
		let hmod = GetModuleHandleW(std::ptr::null());
		// Keyboard hook thread first; it reports its id so disable() can stop it with us.
		let (ktx, krx) = std::sync::mpsc::channel::<u32>();
		let kbd = std::thread::Builder::new()
			.name("kbdhook-kbd".into())
			.spawn(move || kbd_hook_thread(ktx))
			.ok();
		let kbd_tid = krx.recv_timeout(std::time::Duration::from_secs(2)).unwrap_or(0);
		if kbd_tid == 0 {
			tracing::warn!("keyboard hook thread failed to start — no keyboard capture");
		}
		// MOUSE hook: without the Interception driver this is the ONLY way the viewer captures
		// the mouse (the native renderer occludes the webview, so the JS pointer path is dead).
		let mouse_hook: HHOOK = SetWindowsHookExW(WH_MOUSE_LL, Some(ll_mouse_proc), hmod, 0);
		if mouse_hook.is_null() {
			tracing::warn!("WH_MOUSE_LL hook install failed — mouse capture unavailable (keyboard still active)");
		}
		// Relative deltas for pointer-locked games (see POINTER_LOCK); WM_INPUT is dispatched
		// by this thread's message loop below.
		raw_mouse_window(hmod);
		// Foreground watcher: with the renderer child holding the keyboard focus the app gets
		// no Focused(false) when another app takes the foreground (Start, Win+L, a toast
		// click...), so watch the foreground directly and drop control the moment it leaves us.
		let fg_hook = windows_sys::Win32::UI::Accessibility::SetWinEventHook(
			windows_sys::Win32::UI::WindowsAndMessaging::EVENT_SYSTEM_FOREGROUND,
			windows_sys::Win32::UI::WindowsAndMessaging::EVENT_SYSTEM_FOREGROUND,
			std::ptr::null_mut(),
			Some(foreground_event),
			0,
			0,
			windows_sys::Win32::UI::WindowsAndMessaging::WINEVENT_OUTOFCONTEXT,
		);
		let my_tid = GetCurrentThreadId();
		tracing::debug!(tid = my_tid, kbd_tid, mouse_ok = !mouse_hook.is_null(), "kbd: LL hooks installed");
		globals().lock().unwrap().hook_thread_id = my_tid;
		let mut msg: MSG = std::mem::zeroed();
		// Re-assert the cursor confine from here (outside any hook callback — see handle_mouse_wh).
		let timer = SetTimer(std::ptr::null_mut(), 0, 500, None);
		// Fast tick that closes a forwarded touchpad pinch once its steps stop.
		let pinch_timer = SetTimer(std::ptr::null_mut(), 0, 50, None);
		// GetMessageW returns 0 on WM_QUIT, -1 on error.
		while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
			if msg.message == WM_TIMER && pinch_timer != 0 && msg.wParam == pinch_timer {
				pinch_end(false);
				continue;
			}
			if msg.message == WM_TIMER {
				if ENGAGED.load(Ordering::SeqCst) {
					reassert_confine();
				}
				// Health line for the keyboard hook (only when the count moved): a hook Windows
				// silently removed shows up as a count that stops while keys are being typed.
				static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
				let n = KCALLS.load(Ordering::Relaxed);
				if LAST.swap(n, Ordering::Relaxed) != n {
					tracing::debug!(kcalls = n, engaged = ENGAGED.load(Ordering::SeqCst), "kbd: keyboard hook alive");
				}
				continue;
			}
			TranslateMessage(&msg);
			DispatchMessageW(&msg);
		}
		if timer != 0 {
			KillTimer(std::ptr::null_mut(), timer);
		}
		if pinch_timer != 0 {
			KillTimer(std::ptr::null_mut(), pinch_timer);
		}
		if !mouse_hook.is_null() {
			UnhookWindowsHookEx(mouse_hook);
		}
		if !fg_hook.is_null() {
			windows_sys::Win32::UI::Accessibility::UnhookWinEvent(fg_hook);
		}
		if kbd_tid != 0 {
			PostThreadMessageW(kbd_tid, WM_QUIT, 0, 0);
		}
		if let Some(h) = kbd {
			let _ = h.join();
		}
		// Compare-and-clear: a fast disable()→enable() churn can spawn a NEW hook
		// thread before this (old) one drains WM_QUIT. The new thread has already
		// stored its own id; blindly zeroing here would clobber the LIVE thread's id,
		// so a later disable() would PostThreadMessageW(0,…) (a no-op) and leak the
		// live thread with the keyboard still hooked. Only clear if it's still ours.
		let mut g = globals().lock().unwrap();
		if g.hook_thread_id == my_tid {
			g.hook_thread_id = 0;
		}
	}
}

/// EVENT_SYSTEM_FOREGROUND callback (out-of-context, delivered on hook_thread's pump).
unsafe extern "system" fn foreground_event(
	_hook: windows_sys::Win32::UI::Accessibility::HWINEVENTHOOK,
	_event: u32,
	_hwnd: windows_sys::Win32::Foundation::HWND,
	_id_object: i32,
	_id_child: i32,
	_thread: u32,
	_time: u32,
) {
	if !ENGAGED.load(Ordering::SeqCst) || KIOSK_SESSION.load(Ordering::SeqCst) {
		return;
	}
	if let FgOwner::Other { pid, class } = foreground_is_ours() {
		tracing::info!(pid, %class, "foreground moved to another app while engaged");
		focus_lost();
	}
}

/// The keyboard-only LL hook thread: install, pump, uninstall on WM_QUIT (posted by
/// `hook_thread` on teardown). Nothing else runs on this thread, so the keyboard callback
/// is never queued behind mouse callbacks.
fn kbd_hook_thread(report: std::sync::mpsc::Sender<u32>) {
	unsafe {
		let mut msg: MSG = std::mem::zeroed();
		// Create this thread's message queue BEFORE hooking (PostThreadMessage target).
		PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
		let hmod = GetModuleHandleW(std::ptr::null());
		let hook: HHOOK = SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_keyboard_proc), hmod, 0);
		if hook.is_null() {
			tracing::warn!(err = %std::io::Error::last_os_error(), "WH_KEYBOARD_LL hook install failed");
			let _ = report.send(0);
			return;
		}
		let _ = report.send(GetCurrentThreadId());
		// Installed ONCE for the thread's lifetime (RustDesk's rdev grab does the same): no
		// periodic re-install. Heartbeat every 2 s while engaged — proves this thread is
		// pumping (a blocked pump = no callbacks) and shows where keyboard focus sits.
		let timer = SetTimer(std::ptr::null_mut(), 0, 2000, None);
		while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
			if msg.message == WM_TIMER {
				if ENGAGED.load(Ordering::SeqCst) {
					tracing::info!(
						kcalls = KCALLS.load(Ordering::Relaxed),
						render_keys = RENDER_KEYS.load(Ordering::Relaxed),
						render_hook_keys = RENDER_HOOK_KEYS.load(Ordering::Relaxed),
						web_keys = WEB_KEYS.load(Ordering::Relaxed),
						fwd_keys = FWD_KEYS.load(Ordering::Relaxed),
						focus = %focus_owner(),
						"kbd thread heartbeat"
					);
				}
				continue;
			}
			TranslateMessage(&msg);
			DispatchMessageW(&msg);
		}
		if timer != 0 {
			KillTimer(std::ptr::null_mut(), timer);
		}
		UnhookWindowsHookEx(hook);
	}
}

/// Diagnostic: the window with keyboard focus in the foreground thread, as "pid:class".
fn focus_owner() -> String {
	use windows_sys::Win32::UI::WindowsAndMessaging::{
		GetClassNameW, GetGUIThreadInfo, GetWindowThreadProcessId, GUITHREADINFO,
	};
	unsafe {
		let mut gti: GUITHREADINFO = std::mem::zeroed();
		gti.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
		if GetGUIThreadInfo(0, &mut gti) == 0 {
			return "?".into();
		}
		let h = if gti.hwndFocus.is_null() { gti.hwndActive } else { gti.hwndFocus };
		if h.is_null() {
			return "none".into();
		}
		let mut pid = 0u32;
		GetWindowThreadProcessId(h, &mut pid);
		let mut buf = [0u16; 64];
		let n = GetClassNameW(h, buf.as_mut_ptr(), buf.len() as i32);
		format!("{pid}:{}", String::from_utf16_lossy(&buf[..n.max(0) as usize]))
	}
}

/// Key edges that came in through the webview fallback (`web_key`) / were forwarded to the host.
static WEB_KEYS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static RENDER_KEYS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static RENDER_HOOK_KEYS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static FWD_KEYS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A key edge the WEBVIEW saw (its keydown/keyup), fed into the same `handle_key` the LL hook
/// uses — combos (Ctrl+Alt+Z, Ctrl+Shift+M/Q/F12, 3×RightCtrl), held-key bookkeeping and
/// forwarding all behave identically.
///
/// Why: on some machines WH_KEYBOARD_LL is never called while Pulsar's WebView2 window has
/// keyboard focus (seen 2026-11-07 on a Windows on ARM laptop: zero
/// callbacks while the session window was focused, normal counts the moment another app was
/// focused; the mouse hook kept working). The webview DOES get those keys, so it relays them.
/// No double-send where the hook works: while engaged the hook suppresses every mapped key
/// (returns 1), so the webview never sees it; while disengaged `handle_key` ignores plain keys
/// and the hook already swallows the combo triggers.
pub fn web_key(evdev: u32, down: bool) {
	if !ENABLED.load(Ordering::SeqCst) {
		return;
	}
	WEB_KEYS.fetch_add(1, Ordering::Relaxed);
	queue_key(evdev, down);
}

/// A key edge the native renderer's child window received while it held the keyboard focus
/// (it takes the focus on engage so the LL hook can capture Alt+Tab / Win). Reaching here
/// means the hook did NOT swallow it, so it takes the same path as `web_key`.
pub fn render_key(vk: u16, down: bool, from_hook: bool) {
	if !ENABLED.load(Ordering::SeqCst) {
		return;
	}
	if let Some(evdev) = vk_to_evdev(vk) {
		if from_hook {
			RENDER_HOOK_KEYS.fetch_add(1, Ordering::Relaxed);
		} else {
			RENDER_KEYS.fetch_add(1, Ordering::Relaxed);
		}
		queue_key(evdev, down);
	}
}

/// Keyboard LL callbacks seen (cheap health counter, reported by hook_thread's timer).
static KCALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Modifier state as the HOOK sees it (atomics, so the callback never takes a lock).
static HOOK_CTRL: AtomicBool = AtomicBool::new(false);
static HOOK_ALT: AtomicBool = AtomicBool::new(false);
static HOOK_SHIFT: AtomicBool = AtomicBool::new(false);

/// Hand a key edge to the worker that runs `handle_key` (combos, forwarding, held-key
/// bookkeeping, `app.emit`). Never blocks: unbounded queue, one long-lived thread.
fn queue_key(evdev: u32, down: bool) {
	use std::sync::mpsc;
	static Q: OnceLock<Mutex<mpsc::Sender<(u32, bool)>>> = OnceLock::new();
	let q = Q.get_or_init(|| {
		let (tx, rx) = mpsc::channel::<(u32, bool)>();
		std::thread::Builder::new()
			.name("kbdhook-keys".into())
			.spawn(move || {
				for (evdev, down) in rx {
					let _ = handle_key(evdev, down);
				}
			})
			.expect("spawn kbdhook key worker");
		Mutex::new(tx)
	});
	// Uncontended (only the hook thread sends) — a lock-free fast path in practice.
	let _ = q.lock().unwrap_or_else(|e| e.into_inner()).send((evdev, down));
}

/// The LL keyboard callback. It must return FAST and never block: once a callback overruns
/// LowLevelHooksTimeout Windows SILENTLY REMOVES the hook for good — the viewer then sends no
/// keys at all and Ctrl+Alt+Z / 3×RightCtrl can't release (seen on a Windows on ARM laptop
/// 2026-11-07: the hook went dead after its first key while a pass-through callback kept
/// working). So the callback only reads atomics, decides suppress-or-pass, and queues the
/// edge; ALL logic (globals mutex, combo emits over Tauri IPC, logging, forwarding) runs on
/// the `kbdhook-keys` worker. No tracing, no locks (beyond the uncontended queue) here.
unsafe extern "system" fn ll_keyboard_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
	KCALLS.fetch_add(1, Ordering::Relaxed);
	// Per docs: if ncode < 0 we must pass through untouched.
	if ncode < 0 || !ENABLED.load(Ordering::SeqCst) {
		return CallNextHookEx(std::ptr::null_mut(), ncode, wparam, lparam);
	}
	let kb = &*(lparam as *const KBDLLHOOKSTRUCT);
	// Never consume injected events (we don't inject on the client, but this
	// keeps us robust against feedback loops / accessibility tools).
	if kb.flags & LLKHF_INJECTED != 0 {
		return CallNextHookEx(std::ptr::null_mut(), ncode, wparam, lparam);
	}
	let msg = wparam as u32;
	let down = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN) && (kb.flags & LLKHF_UP == 0);
	let Some(evdev) = vk_to_evdev(kb.vkCode as u16) else {
		// Unknown VK (no evdev mapping): don't break the local machine — let it through.
		return CallNextHookEx(std::ptr::null_mut(), ncode, wparam, lparam);
	};
	match evdev {
		29 | 97 => HOOK_CTRL.store(down, Ordering::Relaxed),
		56 | 100 => HOOK_ALT.store(down, Ordering::Relaxed),
		42 | 54 => HOOK_SHIFT.store(down, Ordering::Relaxed),
		_ => {}
	}
	// Same suppress rule handle_key applies: engaged → every key acts only on the remote;
	// disengaged-but-app-focused → only the Ctrl+Shift+M/Q/F12 combo triggers are eaten
	// (their actions run on the worker), everything else types locally as normal.
	let suppress = if ENGAGED.load(Ordering::SeqCst) {
		true
	} else {
		matches!(evdev, 50 | 16 | 88)
			&& HOOK_CTRL.load(Ordering::Relaxed)
			&& HOOK_SHIFT.load(Ordering::Relaxed)
			&& APP_FOCUSED.load(Ordering::SeqCst)
	};
	queue_key(evdev, down);
	if suppress {
		1 // SUPPRESS locally — the key only acts on the remote.
	} else {
		CallNextHookEx(std::ptr::null_mut(), ncode, wparam, lparam)
	}
}
