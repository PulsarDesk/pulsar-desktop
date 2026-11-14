//! The macOS OS layer: a Quartz event tap on its own CFRunLoop thread, the 100 ms geometry /
//! focus poll on that same run loop, and the cursor primitives (warp, mouse/cursor
//! association). The decisions are all `engine`'s; this file only translates `CGEvent`s.
//!
//! Untested on macOS hardware (written and type-checked from Windows, see the crate docs).

#![allow(non_upper_case_globals)]

use crate::engine::{self, Geo, Job, MouseEv, Verdict};
use crate::geom::{self, Rect};
use crate::keymap;
use crate::sys::*;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicPtr, AtomicU64, Ordering::SeqCst};
use std::sync::Mutex;

/// The tap's mach port (null until created). Re-enabled from the callback when macOS
/// disables a tap (timeout / secure input) and toggled by `set_tap_enabled`.
static TAP_PORT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static TAP_RUNNING: AtomicBool = AtomicBool::new(false);
static START_LOCK: Mutex<()> = Mutex::new(());

/// One on-screen window from the last poll, front to back.
#[derive(Debug, Clone, Copy)]
struct Win {
	rect: Rect,
	layer: i64,
	number: i64,
	pid: i32,
	ours: bool,
}
/// Layer-0 windows of the last poll (front to back): the click hit test reads them.
static WINDOWS: Mutex<Vec<Win>> = Mutex::new(Vec::new());
/// Quartz number of the window the video rect was measured on (0 = none).
static VIDEO_WIN: AtomicI64 = AtomicI64::new(0);

/// Keys whose DOWN the tap swallowed (one bit per virtual key code < 128): their UP is
/// swallowed too; an up whose down acted locally is let through to the local app.
static SWALLOWED: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

fn bit(kc: u16) -> Option<(usize, u64)> {
	(kc < 128).then(|| ((kc / 64) as usize, 1u64 << (kc % 64)))
}

fn my_pid() -> i32 {
	std::process::id() as i32
}

/// `pid` is a direct child of this process (the pulsar-render / mpv / ffplay renderer).
fn is_child(pid: i32) -> bool {
	if pid <= 0 {
		return false;
	}
	// SAFETY: proc_pidinfo fills at most `size` bytes of the zeroed struct.
	unsafe {
		let mut info: libc::proc_bsdinfo = std::mem::zeroed();
		let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
		let n = libc::proc_pidinfo(
			pid,
			libc::PROC_PIDTBSDINFO,
			0,
			&mut info as *mut libc::proc_bsdinfo as *mut c_void,
			size,
		);
		n == size && info.pbi_ppid as i32 == my_pid()
	}
}

fn is_ours(pid: i32) -> bool {
	pid == my_pid() || is_child(pid)
}

// ───────────────────────────── permission ─────────────────────────────

pub fn trusted() -> bool {
	// SAFETY: plain query, no arguments.
	unsafe { AXIsProcessTrusted() != 0 }
}

pub fn request_trust() -> bool {
	// SAFETY: a one-entry CF dictionary built from CF constants, released after use.
	unsafe {
		let keys = [kAXTrustedCheckOptionPrompt];
		let values = [kCFBooleanTrue];
		let opts = CFDictionaryCreate(
			std::ptr::null(),
			keys.as_ptr(),
			values.as_ptr(),
			1,
			&kCFTypeDictionaryKeyCallBacks,
			&kCFTypeDictionaryValueCallBacks,
		);
		let ok = AXIsProcessTrustedWithOptions(opts) != 0;
		if !opts.is_null() {
			CFRelease(opts);
		}
		ok
	}
}

pub fn open_accessibility_settings() {
	let _ = std::process::Command::new("open")
		.arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
		.spawn();
}

// ───────────────────────────── tap thread ─────────────────────────────

/// Make sure the tap runs (created once, then kept for the app's lifetime and only
/// enabled/disabled). Returns false without the Accessibility permission: macOS refuses to
/// create an active tap then. A later call retries (the user may have granted it meanwhile).
pub fn start_tap() -> bool {
	let _g = START_LOCK.lock().unwrap_or_else(|e| e.into_inner());
	if TAP_RUNNING.load(SeqCst) {
		return true;
	}
	if !trusted() {
		tracing::warn!("macOS input capture: Accessibility permission missing — no event tap");
		return false;
	}
	let (tx, rx) = std::sync::mpsc::channel::<bool>();
	let spawned = std::thread::Builder::new().name("macinput-tap".into()).spawn(move || run_tap(tx));
	if spawned.is_err() {
		return false;
	}
	let ok = rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap_or(false);
	TAP_RUNNING.store(ok, SeqCst);
	ok
}

fn event_mask() -> u64 {
	[
		kCGEventLeftMouseDown,
		kCGEventLeftMouseUp,
		kCGEventRightMouseDown,
		kCGEventRightMouseUp,
		kCGEventMouseMoved,
		kCGEventLeftMouseDragged,
		kCGEventRightMouseDragged,
		kCGEventKeyDown,
		kCGEventKeyUp,
		kCGEventFlagsChanged,
		kCGEventScrollWheel,
		kCGEventOtherMouseDown,
		kCGEventOtherMouseUp,
		kCGEventOtherMouseDragged,
	]
	.iter()
	.fold(0u64, |m, t| m | (1u64 << t))
}

fn run_tap(report: std::sync::mpsc::Sender<bool>) {
	// SAFETY: CF/CG calls on this thread's own run loop; the port, source and timer live for
	// the process lifetime (the run loop never returns).
	unsafe {
		// Warps must not freeze the mouse for the default 0.25 s.
		CGSetLocalEventsSuppressionInterval(0.0);
		// A session tap at the head: sees events before any application and before the
		// system's own shortcuts (Cmd+Tab, Cmd+Space, Mission Control) act on them.
		let port = CGEventTapCreate(
			kCGSessionEventTap,
			kCGHeadInsertEventTap,
			kCGEventTapOptionDefault,
			event_mask(),
			tap_cb,
			std::ptr::null_mut(),
		);
		if port.is_null() {
			tracing::warn!("CGEventTapCreate failed — no macOS input capture (Accessibility permission?)");
			let _ = report.send(false);
			return;
		}
		TAP_PORT.store(port, SeqCst);
		let rl = CFRunLoopGetCurrent();
		let source = CFMachPortCreateRunLoopSource(std::ptr::null(), port, 0);
		CFRunLoopAddSource(rl, source, kCFRunLoopCommonModes);
		let timer = CFRunLoopTimerCreate(
			std::ptr::null(),
			CFAbsoluteTimeGetCurrent() + 0.1,
			0.1,
			0,
			0,
			tick_cb,
			std::ptr::null_mut(),
		);
		if !timer.is_null() {
			CFRunLoopAddTimer(rl, timer, kCFRunLoopCommonModes);
		}
		CGEventTapEnable(port, engine::ENABLED.load(SeqCst));
		tracing::info!("macOS input capture: event tap running");
		let _ = report.send(true);
		CFRunLoopRun();
	}
}

pub fn set_tap_enabled(on: bool) {
	let port = TAP_PORT.load(SeqCst);
	if !port.is_null() {
		// SAFETY: a live tap port (never released).
		unsafe { CGEventTapEnable(port, on) };
	}
}

unsafe extern "C" fn tap_cb(_proxy: CGEventTapProxy, ty: u32, ev: CGEventRef, _info: *mut c_void) -> CGEventRef {
	if ty == kCGEventTapDisabledByTimeout || ty == kCGEventTapDisabledByUserInput {
		// macOS turned the tap off (a slow callback, or secure input): back on while armed.
		if engine::ENABLED.load(SeqCst) {
			set_tap_enabled(true);
		}
		tracing::info!(timeout = ty == kCGEventTapDisabledByTimeout, "event tap was disabled by macOS — re-enabled");
		return ev;
	}
	if ev.is_null() || !engine::ENABLED.load(SeqCst) {
		return ev;
	}
	// Never act on events this process posted itself.
	if CGEventGetIntegerValueField(ev, kCGEventSourceUnixProcessID) == my_pid() as i64 {
		return ev;
	}
	let swallow = match ty {
		kCGEventKeyDown | kCGEventKeyUp => key_event(ev, ty == kCGEventKeyDown),
		kCGEventFlagsChanged => flags_event(ev),
		_ => return mouse_event(ty, ev),
	};
	if swallow {
		std::ptr::null_mut()
	} else {
		ev
	}
}

unsafe fn key_event(ev: CGEventRef, down: bool) -> bool {
	let kc = CGEventGetIntegerValueField(ev, kCGKeyboardEventKeycode) as u16;
	let Some(code) = keymap::to_evdev(kc) else {
		return false; // no host equivalent: stays local
	};
	let suppress = engine::key_suppress(code);
	if let Some((i, b)) = bit(kc) {
		if down {
			if suppress {
				SWALLOWED[i].fetch_or(b, SeqCst);
			}
		} else {
			let was = SWALLOWED[i].fetch_and(!b, SeqCst) & b != 0;
			if !was && suppress {
				// Its down acted locally (pressed before control engaged): the local app must
				// see the release too. The host gets it as well (a no-op there).
				engine::queue(Job::Key(code, false));
				return false;
			}
		}
	}
	engine::queue(Job::Key(code, down));
	suppress
}

unsafe fn flags_event(ev: CGEventRef) -> bool {
	let kc = CGEventGetIntegerValueField(ev, kCGKeyboardEventKeycode) as u16;
	let engaged = engine::ENGAGED.load(SeqCst);
	if kc == keymap::KC_CAPSLOCK {
		// One FlagsChanged per toggle, no release: a tap on the host.
		if engaged {
			engine::queue(Job::Key(58, true));
			engine::queue(Job::Key(58, false));
		}
		return engaged;
	}
	let Some(code) = keymap::to_evdev(kc) else { return false };
	let Some(down) = keymap::modifier_down(kc, CGEventGetFlags(ev)) else {
		return false;
	};
	engine::tap_modifier(code, down);
	// Always queued: the combos need the modifier state while disengaged too.
	engine::queue(Job::Key(code, down));
	engaged
}

/// The topmost layer-0 window at the point: (Pulsar's or a renderer child's, the video's own
/// window). Unknown (no window there in the last poll) counts as ours — never release on
/// missing data — but not as the video window — never engage on it either.
fn hit(x: f64, y: f64) -> (bool, bool) {
	let wins = WINDOWS.lock().unwrap_or_else(|e| e.into_inner());
	let video = VIDEO_WIN.load(SeqCst);
	wins.iter()
		.find(|w| geom::contains(w.rect, x, y))
		.map(|w| (w.ours, video != 0 && w.number == video))
		.unwrap_or((true, false))
}

unsafe fn mouse_event(ty: u32, ev: CGEventRef) -> CGEventRef {
	if !engine::MOUSE_CAPTURE.load(SeqCst) {
		return ev;
	}
	let p = CGEventGetLocation(ev);
	let button = |b: u8, down: bool| {
		let (hit_ours, hit_video) = hit(p.x, p.y);
		MouseEv::Button { button: b, down, x: p.x, y: p.y, hit_ours, hit_video }
	};
	let mev = match ty {
		kCGEventMouseMoved | kCGEventLeftMouseDragged | kCGEventRightMouseDragged | kCGEventOtherMouseDragged => {
			MouseEv::Move {
				x: p.x,
				y: p.y,
				dx: CGEventGetDoubleValueField(ev, kCGMouseEventDeltaX),
				dy: CGEventGetDoubleValueField(ev, kCGMouseEventDeltaY),
			}
		}
		kCGEventLeftMouseDown => button(0, true),
		kCGEventLeftMouseUp => button(0, false),
		kCGEventRightMouseDown => button(1, true),
		kCGEventRightMouseUp => button(1, false),
		kCGEventOtherMouseDown | kCGEventOtherMouseUp => {
			let n = CGEventGetIntegerValueField(ev, kCGMouseEventButtonNumber).clamp(0, 255) as u8;
			button(n, ty == kCGEventOtherMouseDown)
		}
		kCGEventScrollWheel => {
			// Browser wheel pixels (+y = down, +x = right), what every host injector takes.
			// Quartz axis 1 is vertical, positive = towards the top; axis 2 horizontal,
			// positive = towards the left. Natural scrolling is already in the values.
			let (dx, dy) = if CGEventGetIntegerValueField(ev, kCGScrollWheelEventIsContinuous) != 0 {
				(
					-CGEventGetDoubleValueField(ev, kCGScrollWheelEventPointDeltaAxis2),
					-CGEventGetDoubleValueField(ev, kCGScrollWheelEventPointDeltaAxis1),
				)
			} else {
				// A notched wheel: lines (accelerated), 100 px each like the Windows client.
				(
					-(CGEventGetIntegerValueField(ev, kCGScrollWheelEventDeltaAxis2) as f64) * 100.0,
					-(CGEventGetIntegerValueField(ev, kCGScrollWheelEventDeltaAxis1) as f64) * 100.0,
				)
			};
			MouseEv::Scroll { dx, dy }
		}
		_ => return ev,
	};
	match engine::mouse(mev) {
		Verdict::Pass => ev,
		Verdict::Swallow => std::ptr::null_mut(),
		Verdict::Clamp(x, y) => {
			let at = CGPoint { x, y };
			CGEventSetLocation(ev, at);
			CGWarpMouseCursorPosition(at);
			ev
		}
		Verdict::Anchor(x, y) => {
			if (p.x - x).abs() > 0.5 || (p.y - y).abs() > 0.5 {
				CGWarpMouseCursorPosition(CGPoint { x, y });
			}
			std::ptr::null_mut()
		}
	}
}

// ───────────────────────────── poll ─────────────────────────────

/// 100 ms, on the tap thread while armed: measure the video rect, follow the frontmost app,
/// keep the cursor where the mode wants it.
unsafe extern "C" fn tick_cb(_timer: CFRunLoopTimerRef, _info: *mut c_void) {
	if !engine::ENABLED.load(SeqCst) {
		return;
	}
	let pool = objc_autoreleasePoolPush();
	geometry();
	if let Some(ours) = frontmost_is_ours() {
		engine::focus_tick(ours);
	}
	engine::maintain();
	objc_autoreleasePoolPop(pool);
}

/// Measure now (a fresh arm / engage must not wait for the next poll to know the video rect).
pub fn refresh_geometry() {
	// SAFETY: window-list reads inside an autorelease pool.
	unsafe {
		let pool = objc_autoreleasePoolPush();
		geometry();
		objc_autoreleasePoolPop(pool);
	}
}

/// The video rect (and the window list for the click hit test) from the on-screen windows.
unsafe fn geometry() {
	let wins = window_list();
	let cursor = cursor_pos();
	let (frac, view) = engine::rect_inputs();
	let layer0: Vec<Win> = wins.iter().copied().filter(|w| w.layer == 0).collect();
	// The renderer: a child process's window (pulsar-render / mpv / ffplay). Several (split
	// panes, a parked inactive one): the one under the cursor, else the largest.
	let me = my_pid();
	let area = |r: Rect| (r.2 - r.0) * (r.3 - r.1);
	let render = layer0
		.iter()
		.filter(|w| w.ours && w.pid != me && w.rect.2 - w.rect.0 >= geom::MIN_SIDE && w.rect.3 - w.rect.1 >= geom::MIN_SIDE)
		.max_by(|a, b| {
			let under = |w: &Win| cursor.is_some_and(|(x, y)| geom::contains(w.rect, x, y));
			(under(a), area(a.rect))
				.partial_cmp(&(under(b), area(b.rect)))
				.unwrap_or(std::cmp::Ordering::Equal)
		})
		.copied();
	let (video, video_win) = match render.and_then(|w| geom::video_rect(w.rect, frac).map(|r| (r, w.number))) {
		Some((r, n)) => (Some(r), n),
		None => view
			.and_then(|v| {
				let w = layer0.iter().find(|w| w.number == v.window && w.pid == me)?;
				geom::sub_rect(w.rect, v.rect.0, v.rect.1, v.rect.2, v.rect.3).map(|r| (r, w.number))
			})
			.map_or((None, 0), |(r, n)| (Some(r), n)),
	};
	VIDEO_WIN.store(video_win, SeqCst);
	engine::set_geo(Geo { video, render: render.map(|w| w.rect) });
	*WINDOWS.lock().unwrap_or_else(|e| e.into_inner()) = layer0;
}

unsafe fn dict_i64(d: CFDictionaryRef, key: CFStringRef) -> Option<i64> {
	let n = CFDictionaryGetValue(d, key);
	if n.is_null() {
		return None;
	}
	let mut v: i64 = 0;
	(CFNumberGetValue(n, kCFNumberSInt64Type, &mut v as *mut i64 as *mut c_void) != 0).then_some(v)
}

unsafe fn dict_f64(d: CFDictionaryRef, key: CFStringRef) -> Option<f64> {
	let n = CFDictionaryGetValue(d, key);
	if n.is_null() {
		return None;
	}
	let mut v: f64 = 0.0;
	(CFNumberGetValue(n, kCFNumberFloat64Type, &mut v as *mut f64 as *mut c_void) != 0).then_some(v)
}

/// On-screen windows, front to back (no screen-recording permission needed for bounds,
/// owner pid, layer and number).
unsafe fn window_list() -> Vec<Win> {
	let arr = CGWindowListCopyWindowInfo(
		kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
		kCGNullWindowID,
	);
	if arr.is_null() {
		return Vec::new();
	}
	let n = CFArrayGetCount(arr);
	let mut out = Vec::with_capacity(n.max(0) as usize);
	let mut owner = std::collections::HashMap::<i32, bool>::new();
	for i in 0..n {
		let d = CFArrayGetValueAtIndex(arr, i) as CFDictionaryRef;
		if d.is_null() {
			continue;
		}
		let (Some(pid), Some(layer)) = (dict_i64(d, kCGWindowOwnerPID), dict_i64(d, kCGWindowLayer)) else {
			continue;
		};
		if dict_f64(d, kCGWindowAlpha).is_some_and(|a| a <= 0.0) {
			continue; // invisible helper windows must not count in the hit test
		}
		let bounds = CFDictionaryGetValue(d, kCGWindowBounds) as CFDictionaryRef;
		let mut r = CGRect::default();
		if bounds.is_null() || !CGRectMakeWithDictionaryRepresentation(bounds, &mut r) {
			continue;
		}
		let pid = pid as i32;
		let ours = *owner.entry(pid).or_insert_with(|| is_ours(pid));
		out.push(Win {
			rect: (r.origin.x, r.origin.y, r.origin.x + r.size.width, r.origin.y + r.size.height),
			layer,
			number: dict_i64(d, kCGWindowNumber).unwrap_or(0),
			pid,
			ours,
		});
	}
	CFRelease(arr);
	out
}

// ───────────────────────────── primitives ─────────────────────────────

pub fn cursor_pos() -> Option<(f64, f64)> {
	// SAFETY: a fresh null-source event carries the current cursor location; released after.
	unsafe {
		let e = CGEventCreate(std::ptr::null_mut());
		if e.is_null() {
			return None;
		}
		let p = CGEventGetLocation(e);
		CFRelease(e as CFTypeRef);
		Some((p.x, p.y))
	}
}

pub fn warp(x: f64, y: f64) {
	// SAFETY: plain call with a point.
	unsafe {
		CGWarpMouseCursorPosition(CGPoint { x, y });
	}
}

/// Connect (`true`) / disconnect the mouse from the cursor: disconnected, the cursor stays
/// put while the mouse still reports deltas (relative mode).
pub fn associate(connected: bool) {
	// SAFETY: plain call.
	unsafe {
		CGAssociateMouseAndMouseCursorPosition(connected as i32);
	}
}

pub fn buttons_down() -> bool {
	// SAFETY: plain queries.
	unsafe { (0..3).any(|b| CGEventSourceButtonState(kCGEventSourceStateCombinedSessionState, b)) }
}

/// `NSWorkspace.sharedWorkspace.frontmostApplication` is this process or a renderer child.
pub fn frontmost_is_ours() -> Option<bool> {
	// SAFETY: class lookup + three zero-argument messages inside an autorelease pool.
	unsafe {
		let pool = objc_autoreleasePoolPush();
		let cls = objc_getClass(c"NSWorkspace".as_ptr());
		let ws = msg_id(cls, c"sharedWorkspace");
		let app = msg_id(ws, c"frontmostApplication");
		let r = if app.is_null() {
			None
		} else {
			let pid: i32 = msg_int(app, c"processIdentifier");
			Some(is_ours(pid))
		};
		objc_autoreleasePoolPop(pool);
		r
	}
}

pub fn ns_window_number(ns_window: *mut c_void) -> Option<i64> {
	// SAFETY: the caller passes a live NSWindow*.
	let n: isize = unsafe { msg_int(ns_window, c"windowNumber") };
	(n > 0).then_some(n as i64)
}
