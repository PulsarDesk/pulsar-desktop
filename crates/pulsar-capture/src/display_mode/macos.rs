//! macOS: switch a display's mode for screen adaptation, and hear display reconfigurations.
//!
//! Plain CoreGraphics C API (no Objective-C): `CGDisplayCopyAllDisplayModes` lists the modes
//! (with `kCGDisplayShowDuplicateLowResolutionModes`, so the HiDPI and 1x variants of a size are
//! both visible), [`super::plan`] picks one, and `CGConfigureDisplayWithDisplayMode` applies it
//! inside a display configuration completed with `kCGConfigureForAppOnly`. That option is the
//! crash guarantee: the change belongs to this process, and the window server puts the original
//! mode back by itself when the process exits — however it exits. [`restore`] does the same on
//! a normal session end.
//!
//! Display index: 0 is the main display (`CGMainDisplayID`), then the other active displays in
//! `CGGetActiveDisplayList` order — the order [`displays`] returns.
//!
//! Untested on macOS hardware (written and type-checked on Windows; no Mac available).

use std::ffi::c_void;

use super::Cand;

type CGDirectDisplayID = u32;
type CGError = i32;
type CGDisplayModeRef = *const c_void;
type CGDisplayConfigRef = *mut c_void;
type CFTypeRef = *const c_void;
type CFArrayRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFStringRef = *const c_void;
type CFIndex = isize;

const K_CG_ERROR_SUCCESS: CGError = 0;
/// `kCGConfigureForAppOnly`: the change ends with the process.
const K_CG_CONFIGURE_FOR_APP_ONLY: u32 = 0;
/// `kCGDisplayBeginConfigurationFlag`: the "about to change" half of a reconfiguration.
const K_CG_DISPLAY_BEGIN_CONFIGURATION_FLAG: u32 = 1 << 0;

#[repr(C)]
struct CFDictionaryKeyCallBacks {
	_private: [usize; 6],
}
#[repr(C)]
struct CFDictionaryValueCallBacks {
	_private: [usize; 5],
}

type ReconfigCallback = extern "C" fn(display: CGDirectDisplayID, flags: u32, user: *mut c_void);

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
	static kCGDisplayShowDuplicateLowResolutionModes: CFStringRef;
	fn CGMainDisplayID() -> CGDirectDisplayID;
	fn CGGetActiveDisplayList(max: u32, displays: *mut CGDirectDisplayID, count: *mut u32) -> CGError;
	fn CGDisplayCopyDisplayMode(display: CGDirectDisplayID) -> CGDisplayModeRef;
	fn CGDisplayCopyAllDisplayModes(display: CGDirectDisplayID, options: CFDictionaryRef) -> CFArrayRef;
	fn CGDisplayModeGetPixelWidth(mode: CGDisplayModeRef) -> usize;
	fn CGDisplayModeGetPixelHeight(mode: CGDisplayModeRef) -> usize;
	fn CGDisplayModeGetWidth(mode: CGDisplayModeRef) -> usize;
	fn CGDisplayModeGetRefreshRate(mode: CGDisplayModeRef) -> f64;
	fn CGDisplayModeGetIODisplayModeID(mode: CGDisplayModeRef) -> i32;
	fn CGDisplayModeIsUsableForDesktopGUI(mode: CGDisplayModeRef) -> bool;
	fn CGDisplayModeRetain(mode: CGDisplayModeRef) -> CGDisplayModeRef;
	fn CGDisplayModeRelease(mode: CGDisplayModeRef);
	fn CGDisplayRotation(display: CGDirectDisplayID) -> f64;
	fn CGBeginDisplayConfiguration(config: *mut CGDisplayConfigRef) -> CGError;
	fn CGConfigureDisplayWithDisplayMode(
		config: CGDisplayConfigRef,
		display: CGDirectDisplayID,
		mode: CGDisplayModeRef,
		options: CFDictionaryRef,
	) -> CGError;
	fn CGCompleteDisplayConfiguration(config: CGDisplayConfigRef, option: u32) -> CGError;
	fn CGCancelDisplayConfiguration(config: CGDisplayConfigRef) -> CGError;
	fn CGDisplayRegisterReconfigurationCallback(cb: ReconfigCallback, user: *mut c_void) -> CGError;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
	static kCFBooleanTrue: CFTypeRef;
	static kCFTypeDictionaryKeyCallBacks: CFDictionaryKeyCallBacks;
	static kCFTypeDictionaryValueCallBacks: CFDictionaryValueCallBacks;
	fn CFDictionaryCreate(
		alloc: *const c_void,
		keys: *const *const c_void,
		values: *const *const c_void,
		count: CFIndex,
		key_cb: *const CFDictionaryKeyCallBacks,
		value_cb: *const CFDictionaryValueCallBacks,
	) -> CFDictionaryRef;
	fn CFArrayGetCount(array: CFArrayRef) -> CFIndex;
	fn CFArrayGetValueAtIndex(array: CFArrayRef, idx: CFIndex) -> *const c_void;
	fn CFRelease(cf: CFTypeRef);
}

/// Active displays, main first (the index the host uses as `display_idx`).
pub fn displays() -> Vec<u32> {
	let mut ids = [0u32; 32];
	let mut n = 0u32;
	let main = unsafe { CGMainDisplayID() };
	let ok = unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut n) } == K_CG_ERROR_SUCCESS;
	let mut out = vec![main];
	if ok {
		out.extend(ids[..n as usize].iter().copied().filter(|&d| d != main));
	}
	out
}

/// A retained `CGDisplayModeRef` (released on drop).
struct Mode(CGDisplayModeRef);
// SAFETY: CGDisplayMode is an immutable CF object; retain/release are thread-safe.
unsafe impl Send for Mode {}
impl Drop for Mode {
	fn drop(&mut self) {
		unsafe { CGDisplayModeRelease(self.0) }
	}
}

/// What the display ran in before [`set_best_mode`] changed it.
pub struct SavedMode {
	display: CGDirectDisplayID,
	mode: Mode,
	w: usize,
	h: usize,
}

impl SavedMode {
	/// The original mode's size in pixels (for logging).
	pub fn size(&self) -> (usize, usize) {
		(self.w, self.h)
	}
}

fn cand(mode: CGDisplayModeRef, quarter_turn: bool) -> Cand {
	unsafe {
		let pw = CGDisplayModeGetPixelWidth(mode) as u32;
		let ph = CGDisplayModeGetPixelHeight(mode) as u32;
		let pts = CGDisplayModeGetWidth(mode).max(1) as u32;
		let (w, h) = if quarter_turn { (ph, pw) } else { (pw, ph) };
		Cand { w, h, hz: CGDisplayModeGetRefreshRate(mode), scale: (pw / pts).max(1) }
	}
}

/// Apply `mode` to `display`, scoped to this process.
unsafe fn configure(display: CGDirectDisplayID, mode: CGDisplayModeRef) -> Result<(), CGError> {
	let mut cfg: CGDisplayConfigRef = std::ptr::null_mut();
	let rc = CGBeginDisplayConfiguration(&mut cfg);
	if rc != K_CG_ERROR_SUCCESS {
		return Err(rc);
	}
	let rc = CGConfigureDisplayWithDisplayMode(cfg, display, mode, std::ptr::null());
	if rc != K_CG_ERROR_SUCCESS {
		CGCancelDisplayConfiguration(cfg);
		return Err(rc);
	}
	let rc = CGCompleteDisplayConfiguration(cfg, K_CG_CONFIGURE_FOR_APP_ONLY);
	if rc != K_CG_ERROR_SUCCESS {
		return Err(rc);
	}
	Ok(())
}

/// Switch display `display_idx` (see [`displays`]) to the mode that best fits a
/// `target_w × target_h` pane. `Ok(Some(saved))` when a change was applied (pass it to
/// [`restore`]), `Ok(None)` when there is nothing to do, `Err` with a reason when it failed.
pub fn set_best_mode(display_idx: u32, target_w: u32, target_h: u32) -> Result<Option<SavedMode>, String> {
	let Some(&display) = displays().get(display_idx as usize) else {
		return Err(format!("no display at index {display_idx}"));
	};
	unsafe {
		let cur = CGDisplayCopyDisplayMode(display);
		if cur.is_null() {
			return Err("CGDisplayCopyDisplayMode returned nothing".into());
		}
		let cur = Mode(cur);
		let quarter_turn = {
			let r = CGDisplayRotation(display).rem_euclid(180.0);
			(r - 90.0).abs() < 1.0
		};
		// Every mode, including the 1x duplicates of the HiDPI sizes.
		let key = kCGDisplayShowDuplicateLowResolutionModes;
		let val = kCFBooleanTrue;
		let opts = CFDictionaryCreate(
			std::ptr::null(),
			&key as *const CFStringRef as *const *const c_void,
			&val as *const CFTypeRef as *const *const c_void,
			1,
			&kCFTypeDictionaryKeyCallBacks,
			&kCFTypeDictionaryValueCallBacks,
		);
		let all = CGDisplayCopyAllDisplayModes(display, opts);
		if !opts.is_null() {
			CFRelease(opts);
		}
		if all.is_null() {
			return Err("CGDisplayCopyAllDisplayModes returned nothing".into());
		}
		let mut modes: Vec<CGDisplayModeRef> = Vec::new();
		let mut cands: Vec<Cand> = Vec::new();
		let mut cur_idx = None;
		let cur_id = CGDisplayModeGetIODisplayModeID(cur.0);
		let cur_c = cand(cur.0, quarter_turn);
		for i in 0..CFArrayGetCount(all) {
			let m = CFArrayGetValueAtIndex(all, i) as CGDisplayModeRef;
			if m.is_null() || !CGDisplayModeIsUsableForDesktopGUI(m) {
				continue;
			}
			let c = cand(m, quarter_turn);
			if cur_idx.is_none() && CGDisplayModeGetIODisplayModeID(m) == cur_id && c == cur_c {
				cur_idx = Some(cands.len());
			}
			modes.push(m);
			cands.push(c);
		}
		// The current mode may be missing from the "usable" list (a custom/odd mode): add it as
		// the reference so the fit still has a native size.
		let cur_idx = match cur_idx {
			Some(i) => i,
			None => {
				modes.push(cur.0);
				cands.push(cur_c);
				cands.len() - 1
			}
		};
		let result = match super::plan(&cands, cur_idx, target_w, target_h) {
			None => Ok(None),
			Some(pick) => {
				let chosen = modes[pick];
				match configure(display, chosen) {
					Ok(()) => {
						let (w, h) = (CGDisplayModeGetPixelWidth(cur.0), CGDisplayModeGetPixelHeight(cur.0));
						// Keep the original mode alive past the array release.
						CGDisplayModeRetain(cur.0);
						Ok(Some(SavedMode { display, mode: Mode(cur.0), w, h }))
					}
					Err(rc) => Err(format!(
						"CGConfigureDisplayWithDisplayMode {}x{} failed: CGError {rc}",
						cands[pick].w, cands[pick].h
					)),
				}
			}
		};
		CFRelease(all);
		result
	}
}

/// Put the display back into the mode saved by [`set_best_mode`].
pub fn restore(saved: SavedMode) -> Result<(), String> {
	unsafe { configure(saved.display, saved.mode.0) }.map_err(|rc| format!("CGError {rc}"))
}

static HOOK: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

extern "C" fn on_reconfig(_display: CGDirectDisplayID, flags: u32, _user: *mut c_void) {
	// Only the "done" half: the begin half arrives before anything changed.
	if flags & K_CG_DISPLAY_BEGIN_CONFIGURATION_FLAG != 0 {
		return;
	}
	if let Some(f) = HOOK.get() {
		f();
	}
}

/// Call `f` after every display reconfiguration (a display added/removed, a mode or scale
/// change). Delivered by the window server through the main thread's run loop, so it fires
/// in the app (AppKit runs that loop) — a process without a running main run loop gets
/// nothing, which callers must tolerate. Registers once; a second call is ignored. Returns
/// whether the callback is registered.
pub fn on_reconfigure(f: fn()) -> bool {
	if HOOK.set(f).is_err() {
		return true;
	}
	unsafe { CGDisplayRegisterReconfigurationCallback(on_reconfig, std::ptr::null_mut()) == K_CG_ERROR_SUCCESS }
}
