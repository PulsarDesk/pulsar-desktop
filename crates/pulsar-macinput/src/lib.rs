//! macOS client input capture for Pulsar — the counterpart of the Windows hook
//! (src-tauri `kbdhook/imp.rs`) and the Linux evdev grab (`kbdhook/linux.rs`).
//!
//! While a session is armed a Quartz event tap (`CGEventTapCreate`, session tap, head insert)
//! sees every keyboard and mouse event before any application does:
//!
//! - **Engaged** (the user clicked the video, or game mode's connect-time engage): every key is
//!   swallowed locally and forwarded — Cmd+Tab, Cmd+Space, Cmd+Q, Ctrl+arrows reach the host.
//!   The mouse is relative in game mode and while the host holds a pointer lock (the cursor is
//!   frozen with `CGAssociateMouseAndMouseCursorPosition(false)`, the device deltas
//!   `kCGMouseEventDeltaX/Y` are sent), absolute otherwise, confined to the video by
//!   `CGWarpMouseCursorPosition` unless the free cursor setting lets it leave (Parsec's handover).
//! - **Disengaged but Pulsar frontmost**: only Pulsar's combos are eaten (Ctrl+Shift+M menu,
//!   Ctrl+Shift+Q end, Ctrl+Shift+F11 / Cmd+Shift+F11 fullscreen); everything else is local.
//! - **Another app frontmost**: nothing is touched; losing the focus releases control.
//!
//! Releasing (Ctrl+Alt+Z, 3×RightCtrl, the menu combo, focus loss, a click on another app,
//! the pointer leaving the video with the free cursor) flushes every key and button still held
//! on the host.
//!
//! The tap needs the Accessibility permission (`AXIsProcessTrustedWithOptions`). Without it
//! [`enable`] returns `false`, nothing engages and the app keeps its webview input path.
//!
//! Layout: `engine` (platform-neutral, unit-tested everywhere), `keymap` + `geom` (pure), `os`
//! (the CGEventTap thread on macOS; a stub elsewhere).

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

pub mod geom;
pub mod keymap;

mod engine;

#[cfg(target_os = "macos")]
mod sys;
#[cfg(target_os = "macos")]
#[path = "tap.rs"]
mod os;

/// Non-macOS stand-in for the OS layer: no tap, no cursor. (Tests pretend a tap runs.)
#[cfg(not(target_os = "macos"))]
mod os {
	pub fn trusted() -> bool {
		false
	}
	pub fn request_trust() -> bool {
		false
	}
	pub fn start_tap() -> bool {
		cfg!(test)
	}
	pub fn set_tap_enabled(_on: bool) {}
	pub fn refresh_geometry() {}
	pub fn cursor_pos() -> Option<(f64, f64)> {
		None
	}
	pub fn warp(_x: f64, _y: f64) {}
	pub fn associate(_connected: bool) {}
	pub fn buttons_down() -> bool {
		false
	}
	pub fn frontmost_is_ours() -> Option<bool> {
		None
	}
	pub fn ns_window_number(_ns_window: *mut std::ffi::c_void) -> Option<i64> {
		None
	}
	pub fn open_accessibility_settings() {}
}

pub use engine::{
	app_focused, app_focused_now, arm_kiosk, capture_active, disable, enable, engage, engage_auto,
	hover_edge, is_engaged, overlay_open, release, set_focused, set_ovbtn_frac, set_pointer_free,
	set_pointer_lock, set_stream_stalled, set_video_frac, set_view, web_key, Arm, Out, Signal, Sink,
	View,
};

/// The Accessibility permission is granted (the event tap can be created).
pub fn trusted() -> bool {
	os::trusted()
}

/// The event tap really runs (the permission is granted AND `CGEventTapCreate` succeeded),
/// starting it if needed — what decides whether the session UI hands control to the OS
/// capture. Trust alone is not enough: a stale TCC grant on a rebuilt unsigned binary reports
/// trusted while the tap creation still fails, and the webview path must then stay in charge.
pub fn tap_ready() -> bool {
	os::trusted() && os::start_tap()
}

/// Ask for the Accessibility permission: macOS shows its own "Pulsar would like to control
/// this computer" prompt (once per app; after that only System Settings can grant it).
/// Returns whether it is already granted.
pub fn request_trust() -> bool {
	os::request_trust()
}

/// Open System Settings → Privacy & Security → Accessibility.
pub fn open_accessibility_settings() {
	os::open_accessibility_settings()
}

/// Pulsar or one of its renderer children is the frontmost app (`None` = unknown / not macOS).
pub fn frontmost_is_ours() -> Option<bool> {
	os::frontmost_is_ours()
}

/// The Quartz window number (`-[NSWindow windowNumber]`) of an `NSWindow*` — matches the
/// `kCGWindowNumber` of the window list, so [`set_view`] can follow the window as it moves.
pub fn ns_window_number(ns_window: *mut std::ffi::c_void) -> Option<i64> {
	os::ns_window_number(ns_window)
}
