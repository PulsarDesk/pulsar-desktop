//! Windows WH_KEYBOARD_LL keyboard hook for the Pulsar *client*.
//!
//! While the user is controlling a remote session, this captures EVERY key
//! (including OS-reserved ones the webview never sees — Win, Alt+Tab, Ctrl+Esc,
//! media keys) *before* Windows handles them, forwards each as an
//! `InputEvent::Key` to the active play session, and returns 1 to suppress it
//! locally. The hook is only armed while controlling, so the local desktop is
//! completely unaffected otherwise.
//!
//! The LL callback must be a plain `extern "system" fn` with NO captured state,
//! so the active sender + app handle live in process-global statics. The hook
//! also must run on a thread with a Windows message pump, so we own a dedicated
//! thread that installs the hook, pumps `GetMessageW`, and uninstalls on quit.

use pulsar_core::service::InputEvent;
use tauri::AppHandle;

// macOS (and any non-Windows, non-Linux target): no client-side capture yet.
#[cfg(not(any(windows, target_os = "linux")))]
pub fn enable(
	_app: AppHandle,
	_tx: tokio::sync::mpsc::Sender<InputEvent>,
	_mouse: bool,
	_id: u64,
	_start_suspended: bool,
) {
}
#[cfg(not(any(windows, target_os = "linux")))]
pub fn disable() {}
// No grab to release on these targets (the webview floats over the live video).
#[cfg(not(any(windows, target_os = "linux")))]
pub fn overlay_suspend(_suspend: bool) {}
#[cfg(not(any(windows, target_os = "linux")))]
pub fn set_focused(_focused: bool) {}

#[cfg(windows)]
mod imp;
#[cfg(windows)]
pub use imp::{disable, enable, overlay_suspend, set_focused};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{
	arm_kiosk_engage, disable, enable, engage, engage_render, input_active, is_engaged,
	overlay_suspend, release, set_focused, set_render_focused,
};

// Windows now has the same click-to-engage lifecycle as Linux: the hook is armed for
// the whole native session but inert until `engage` (video click → renderer
// `ov engage`); 3×RightCtrl disengages, Ctrl+Shift+Q ends (see imp::handle_key).
#[cfg(windows)]
pub fn engage(app: &AppHandle) {
	imp::engage(app)
}
#[cfg(windows)]
pub fn engage_render(app: &AppHandle) {
	imp::engage(app)
}
#[cfg(windows)]
pub fn release(app: &AppHandle) {
	imp::release_engage_why(app, "overlay")
}
#[cfg(windows)]
pub fn arm_kiosk_engage() {
	imp::arm_kiosk()
}
/// The stream stopped (`true`) / came back: while stopped no click, combo or kiosk start
/// engages the input capture, and an active one is released.
#[cfg(windows)]
pub fn set_stream_stalled(app: &AppHandle, stalled: bool) {
	imp::set_stream_stalled(app, stalled)
}
#[cfg(not(windows))]
pub fn set_stream_stalled(_app: &AppHandle, _stalled: bool) {}
/// A key edge seen by the webview (fallback for when WH_KEYBOARD_LL isn't called; see
/// imp::web_key). evdev code space, same as the hook.
#[cfg(windows)]
pub fn web_key(evdev: u32, down: bool) {
	imp::web_key(evdev, down)
}
#[cfg(not(windows))]
pub fn web_key(_evdev: u32, _down: bool) {}
/// A key the native renderer's focused child window received (Windows VK, left/right
/// resolved): same fallback role as `web_key`.
#[cfg(windows)]
pub fn render_key(vk: u16, down: bool, from_hook: bool) {
	imp::render_key(vk, down, from_hook)
}
#[cfg(not(windows))]
pub fn render_key(_vk: u16, _down: bool, _from_hook: bool) {}
/// Where the video sits inside the native renderer's window (client fractions).
#[cfg(windows)]
pub fn set_video_frac(x0: f32, y0: f32, x1: f32, y1: f32) {
	imp::set_video_frac(x0, y0, x1, y1)
}
#[cfg(not(windows))]
pub fn set_video_frac(_x0: f32, _y0: f32, _x1: f32, _y1: f32) {}
/// Whether the connected host injects real pinch gestures (`FEAT_PINCH`).
#[cfg(windows)]
pub fn set_host_pinch(on: bool) {
	imp::set_host_pinch(on)
}
#[cfg(not(windows))]
pub fn set_host_pinch(_on: bool) {}
#[cfg(windows)]
pub fn is_engaged() -> bool {
	imp::is_engaged()
}
#[cfg(not(any(windows, target_os = "linux")))]
pub fn is_engaged() -> bool {
	false
}
// The standalone-render-window focus channel is Linux-only (evdev focus gate).
#[cfg(not(target_os = "linux"))]
pub fn set_render_focused(_focused: bool) {}

/// The kb/mouse device names the local capture is currently holding — for the split per-pane
/// assignment UI (each is lockable to a pane via `set_kbm_lock`). Linux-only (the per-device evdev
/// grab knows each device); empty on Windows/macOS, where capture isn't per-device yet.
#[cfg(target_os = "linux")]
pub fn captured_input_devices() -> Vec<String> {
	linux::captured_list()
}
#[cfg(not(target_os = "linux"))]
pub fn captured_input_devices() -> Vec<String> {
	Vec::new()
}

// macOS: no client-side capture — all lifecycle hooks are no-ops.
#[cfg(not(any(windows, target_os = "linux")))]
pub fn engage(_app: &AppHandle) {}
#[cfg(not(any(windows, target_os = "linux")))]
pub fn engage_render(_app: &AppHandle) {}
#[cfg(not(any(windows, target_os = "linux")))]
pub fn release(_app: &AppHandle) {}
#[cfg(not(any(windows, target_os = "linux")))]
pub fn arm_kiosk_engage() {}
