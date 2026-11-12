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

/// Relative-mouse sensitivity (Settings / session menu): a multiplier on the device's raw
/// deltas — the ones a game that grabbed the mouse receives (1.0 = unchanged, as Moonlight
/// sends them). f32 bits.
static MOUSE_SENS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x3f80_0000);

pub fn set_mouse_sensitivity(v: f32) {
	let v = if v.is_finite() { v.clamp(0.1, 4.0) } else { 1.0 };
	MOUSE_SENS.store(v.to_bits(), std::sync::atomic::Ordering::Relaxed);
}

pub fn mouse_sensitivity() -> f32 {
	f32::from_bits(MOUSE_SENS.load(std::sync::atomic::Ordering::Relaxed))
}

/// "Serbest imleç" (Settings → Görüntü + the session menu's Görüntü view, default OFF): in a
/// REMOTE-mode session the cursor is not trapped in the video. Leaving the video hands the
/// mouse and keyboard back to this PC, coming back over it takes control again (Parsec's
/// handover, issue #3). Game mode always keeps the confine, and so does a host pointer lock.
static POINTER_FREE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// The armed capture belongs to a remote-mode session (set by `kbd_capture_start`).
static SESSION_REMOTE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_pointer_free(on: bool) {
	let was = POINTER_FREE.swap(on, std::sync::atomic::Ordering::SeqCst);
	if was != on {
		#[cfg(windows)]
		imp::pointer_free_changed();
	}
}

pub fn pointer_free() -> bool {
	POINTER_FREE.load(std::sync::atomic::Ordering::SeqCst)
}

pub fn set_session_remote(remote: bool) {
	SESSION_REMOTE.store(remote, std::sync::atomic::Ordering::SeqCst);
}

/// "Karşı tarafın imlecini göster" (Settings → Görüntü + session menu, default ON, remote
/// mode only): the renderer marks the host's pointer while the person at the host uses it.
static HOST_CURSOR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub fn set_host_cursor(on: bool) {
	HOST_CURSOR.store(on, std::sync::atomic::Ordering::SeqCst);
}

pub fn host_cursor() -> bool {
	HOST_CURSOR.load(std::sync::atomic::Ordering::SeqCst)
}

/// The free-cursor handover applies to the armed session (setting on + remote mode).
#[allow(dead_code)] // only the Windows capture reads it
pub(crate) fn free_pointer_session() -> bool {
	pointer_free() && SESSION_REMOTE.load(std::sync::atomic::Ordering::SeqCst)
}

/// The last engage/release edge came from the free-cursor handover (pointer entering or
/// leaving the video), not a click, a combo or focus: the renderer skips its helper tooltip
/// for it (it would flash on every pass over the video).
#[cfg(windows)]
pub fn hover_edge() -> bool {
	imp::hover_edge()
}
#[cfg(not(windows))]
pub fn hover_edge() -> bool {
	false
}

/// Scale a relative delta by the sensitivity. The host injects whole counts, so the fraction
/// is carried per capture thread — slow moves at 0.25× still add up instead of vanishing.
#[allow(dead_code)] // not every platform's capture sends relative motion
pub(crate) fn scale_relative(dx: f64, dy: f64) -> (f64, f64) {
	thread_local!(static CARRY: std::cell::Cell<(f64, f64)> = const { std::cell::Cell::new((0.0, 0.0)) });
	let s = mouse_sensitivity() as f64;
	if (s - 1.0).abs() < 1e-6 {
		return (dx, dy);
	}
	CARRY.with(|c| {
		let (cx, cy) = c.get();
		let (x, y) = (cx + dx * s, cy + dy * s);
		let (ox, oy) = (x.trunc(), y.trunc());
		c.set((x - ox, y - oy));
		(ox, oy)
	})
}

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

#[cfg(any(windows, test))]
mod confine;
#[cfg(windows)]
mod imp;
#[cfg(windows)]
pub use imp::{disable, enable, overlay_suspend, set_focused};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{
	app_focused, arm_kiosk_engage, disable, enable, engage, engage_render, input_active, is_engaged,
	overlay_suspend, release, set_focused, set_render_focused,
};

// Windows now has the same click-to-engage lifecycle as Linux: the hook is armed for
// the whole native session but inert until `engage` (video click → renderer
// `ov engage`); 3×RightCtrl disengages, Ctrl+Shift+Q ends (see imp::handle_key).
#[cfg(windows)]
pub fn engage(app: &AppHandle) {
	imp::engage(app)
}
/// An AUTOMATIC engage (game mode's connect-time "control immediately"), not a click:
/// Windows engages only while Pulsar is the focused app and otherwise waits for the focus
/// (imp::engage_auto). Linux's evdev grab is already held only while focused, so a plain
/// engage there takes the devices when the user focuses Pulsar.
#[cfg(windows)]
pub fn engage_auto(app: &AppHandle) {
	imp::engage_auto(app, "connect")
}
#[cfg(not(windows))]
pub fn engage_auto(app: &AppHandle) {
	engage(app)
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
/// Where the renderer's overlay-open button sits (client fractions; None = hidden).
#[cfg(windows)]
pub fn set_ovbtn_frac(f: Option<(f32, f32, f32, f32)>) {
	imp::set_ovbtn_frac(f)
}
#[cfg(not(windows))]
pub fn set_ovbtn_frac(_f: Option<(f32, f32, f32, f32)>) {}
/// The host hid its cursor (a game grabbed the mouse): send relative motion meanwhile.
#[cfg(windows)]
pub fn set_pointer_lock(id: u64, on: bool) {
	imp::set_pointer_lock(id, on)
}
#[cfg(not(windows))]
pub fn set_pointer_lock(_id: u64, _on: bool) {}
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

#[cfg(test)]
mod sensitivity_tests {
	use super::{scale_relative, set_mouse_sensitivity};

	#[test]
	fn scaling_carries_the_fraction_so_slow_moves_add_up() {
		set_mouse_sensitivity(0.25);
		// Four 1-count moves at 0.25× make exactly one count, not four zeros.
		let sum: f64 = (0..4).map(|_| scale_relative(1.0, -1.0).0).sum();
		assert_eq!(sum, 1.0);
		set_mouse_sensitivity(2.0);
		assert_eq!(scale_relative(3.0, -5.0), (6.0, -10.0));
		set_mouse_sensitivity(1.0);
		assert_eq!(scale_relative(7.0, 1.0), (7.0, 1.0));
		set_mouse_sensitivity(f32::NAN); // garbage from the UI → unchanged
		assert_eq!(scale_relative(2.0, 2.0), (2.0, 2.0));
	}
}
