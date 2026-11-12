//! "Is Pulsar the focused app?" — the gate for every controller input path.
//!
//! The rule (maintainer, 2026-11-11): while another app has focus, Pulsar must not act on
//! the controller. Forwarding to the host (`GamepadSlot` in `play.rs`, `Ds5Input` in
//! `ds5/client.rs`) sends one released state instead, and the in-app pad UI (menu nav,
//! session overlay nav + combos, split "press to join") ignores the pad. Output from the
//! host to the pad (rumble, triggers, haptics, speaker) keeps playing.
//!
//! "Focused" = any window of OUR window tree is the foreground:
//! - Windows: the foreground window's root (`GA_ROOT`) belongs to this process. The
//!   `pulsar-render` video window is a `WS_CHILD` of the main window (another process, same
//!   root), and its overlay is drawn inside it, so clicking the video still counts as
//!   focused — the Tauri focus map alone reports `false` there. Read live (a few cheap
//!   user32 calls) because no focus event reaches us when the renderer child holds focus.
//! - Linux: the evdev capture's own focus gate (Tauri focus map OR the standalone render
//!   window), and a kiosk launch (`--connect`, e.g. the Orange Pi player) is always focused:
//!   it is the only app and may never receive a focus event at all.
//! - macOS: the Tauri focus map.
//!
//! Settings → Genel → "Arka planda kol girdisi" (`set_background_input`, default off) lets
//! the pad keep driving the HOST while another app is focused (Moonlight's "process gamepad
//! input in the background"). The in-app pad UI stays focus-gated either way.

use std::sync::atomic::{AtomicBool, Ordering};

/// OR of every Pulsar window's Tauri focus state (`lib.rs` focus map).
static WINDOW_FOCUSED: AtomicBool = AtomicBool::new(true);
/// The user's "background controller input" setting.
static BACKGROUND: AtomicBool = AtomicBool::new(false);

/// The Tauri focus map changed (any Pulsar window focused).
pub fn set_window_focused(focused: bool) {
	WINDOW_FOCUSED.store(focused, Ordering::SeqCst);
}

/// Settings toggle: forward the controller to the host even while Pulsar is not focused.
pub fn set_background_input(on: bool) {
	if BACKGROUND.swap(on, Ordering::SeqCst) != on {
		tracing::info!(on, "background controller input");
	}
}

/// Pulsar (any window of its tree, the video included) is the focused app.
pub fn app_focused() -> bool {
	#[cfg(windows)]
	{
		win::app_focused()
	}
	#[cfg(target_os = "linux")]
	{
		crate::kbdhook::app_focused()
	}
	#[cfg(not(any(windows, target_os = "linux")))]
	{
		WINDOW_FOCUSED.load(Ordering::SeqCst)
	}
}

/// The "background controller input" setting is on.
pub fn background_input() -> bool {
	BACKGROUND.load(Ordering::SeqCst)
}

/// May the controller drive the HOST right now (focused, or background input allowed)?
pub fn pad_forward_allowed() -> bool {
	background_input() || app_focused()
}

/// Plays whose session menu (overlay) is open: the pad drives the menu then, never the host.
/// Mirrors `AppState::overlay_open` for threads without the app state (the DualSense raw-HID
/// threads, ds5/client.rs).
static OVERLAY_PLAYS: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());

pub fn set_overlay_open(play: u64, open: bool) {
	let mut g = OVERLAY_PLAYS.lock().unwrap_or_else(|e| e.into_inner());
	g.retain(|p| *p != play);
	if open {
		g.push(play);
	}
}

pub fn overlay_open(play: u64) -> bool {
	OVERLAY_PLAYS.lock().unwrap_or_else(|e| e.into_inner()).contains(&play)
}

/// One reading of the foreground window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // built from user32 readings on Windows only; the tests use it everywhere
pub(crate) enum Reading {
	/// The foreground root is one of our windows.
	Ours,
	/// Another process owns the foreground.
	Other,
	/// No foreground window: activation is in flight (child/webview handoff), or the
	/// desktop is locked / a secure desktop is up.
	None,
}

/// How long "no foreground window" keeps the last answer before it counts as unfocused.
/// The child/webview handoff takes a few ms; a locked workstation stays there.
pub(crate) const NONE_GRACE_MS: u64 = 400;

/// Turns raw foreground readings into a stable focused answer: a short `None` gap keeps
/// the previous answer (so a click on the video doesn't release a held trigger), a long
/// one means unfocused.
#[derive(Debug)]
pub(crate) struct Tracker {
	last: bool,
	none_since: Option<u64>,
}

impl Tracker {
	pub(crate) const fn new() -> Self {
		Self {
			last: true,
			none_since: None,
		}
	}

	pub(crate) fn observe(&mut self, r: Reading, now_ms: u64) -> bool {
		match r {
			Reading::Ours => {
				self.last = true;
				self.none_since = None;
			}
			Reading::Other => {
				self.last = false;
				self.none_since = None;
			}
			Reading::None => {
				let since = *self.none_since.get_or_insert(now_ms);
				if now_ms.saturating_sub(since) >= NONE_GRACE_MS {
					return false;
				}
			}
		}
		self.last
	}
}

/// Rising-edge guard for in-app pad UI: after the app regains focus, nothing counts until
/// every button/direction was released once — a button still held from the other app
/// must not click something in Pulsar.
#[derive(Debug, Default)]
pub(crate) struct RefocusLatch {
	blocked: bool,
}

impl RefocusLatch {
	/// `true` = pass this input to the UI.
	pub(crate) fn pass(&mut self, focused: bool, any_pressed: bool) -> bool {
		if !focused {
			self.blocked = true;
			return false;
		}
		if self.blocked && any_pressed {
			return false;
		}
		self.blocked = false;
		true
	}
}

#[cfg(windows)]
pub(crate) mod win {
	use super::{Reading, Tracker};
	use std::sync::Mutex;

	/// Who owns the foreground window right now (diagnostic + the focus gates).
	#[derive(Debug)]
	pub(crate) enum FgOwner {
		Ours,
		None,
		Other { pid: u32, class: String },
	}

	pub(crate) fn foreground_owner() -> FgOwner {
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

	static TRACKER: Mutex<Tracker> = Mutex::new(Tracker::new());

	pub(super) fn app_focused() -> bool {
		use std::sync::OnceLock;
		static T0: OnceLock<std::time::Instant> = OnceLock::new();
		let reading = match foreground_owner() {
			FgOwner::Ours => Reading::Ours,
			FgOwner::Other { .. } => Reading::Other,
			FgOwner::None => Reading::None,
		};
		let now = T0
			.get_or_init(std::time::Instant::now)
			.elapsed()
			.as_millis() as u64;
		TRACKER
			.lock()
			.unwrap_or_else(|e| e.into_inner())
			.observe(reading, now)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn tracker_follows_definite_readings() {
		let mut t = Tracker::new();
		assert!(t.observe(Reading::Ours, 0));
		assert!(!t.observe(Reading::Other, 10));
		assert!(t.observe(Reading::Ours, 20));
	}

	#[test]
	fn short_none_gap_keeps_the_last_answer() {
		let mut t = Tracker::new();
		assert!(t.observe(Reading::Ours, 0));
		// The child/webview handoff of a click on the video.
		assert!(t.observe(Reading::None, 5));
		assert!(t.observe(Reading::None, 5 + NONE_GRACE_MS - 1));
		assert!(t.observe(Reading::Ours, 5 + NONE_GRACE_MS));
		// The gap restarts after a definite reading.
		assert!(t.observe(Reading::None, 1000));
		assert!(t.observe(Reading::None, 1000 + NONE_GRACE_MS - 1));
	}

	#[test]
	fn long_none_gap_is_unfocused() {
		let mut t = Tracker::new();
		assert!(t.observe(Reading::Ours, 0));
		assert!(t.observe(Reading::None, 100));
		assert!(!t.observe(Reading::None, 100 + NONE_GRACE_MS));
		assert!(!t.observe(Reading::None, 5000));
		// Unfocused before the gap stays unfocused through it.
		let mut t = Tracker::new();
		assert!(!t.observe(Reading::Other, 0));
		assert!(!t.observe(Reading::None, 1));
	}

	#[test]
	fn refocus_latch_waits_for_a_release() {
		let mut l = RefocusLatch::default();
		assert!(l.pass(true, true), "focused from the start: input passes");
		assert!(!l.pass(false, false), "unfocused: nothing passes");
		assert!(!l.pass(false, true));
		// Back in Pulsar with A still held from the other app: ignored until released.
		assert!(!l.pass(true, true));
		assert!(l.pass(true, false));
		assert!(l.pass(true, true), "a fresh press after the release passes");
	}

	#[test]
	fn background_setting_opens_host_forwarding_only() {
		// app_focused() reads the real desktop on Windows/Linux, so only the setting's
		// effect is asserted here: with it on, forwarding is allowed whatever the focus.
		set_background_input(true);
		assert!(pad_forward_allowed());
		set_background_input(false);
		assert_eq!(pad_forward_allowed(), app_focused());
	}
}
