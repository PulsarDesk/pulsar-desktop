//! Host per-window capture off Windows: the window list the client's picker shows and
//! what a picked window id means for this host's stream. Windows captures one window
//! natively through WGC (`pulsar-capture`) and never comes here.
//!
//! | Host | Picker entries | Stream source |
//! | --- | --- | --- |
//! | Linux X11 | `_NET_CLIENT_LIST` windows (id = X window id) | `x11grab -window_id` (libav / ffmpeg CLI) or gst `ximagesrc xid=` |
//! | Linux Wayland | one entry, [`WindowInfo::PORTAL_PICK`] | ScreenCast portal, windows only — the person at the host picks |
//! | macOS | ScreenCaptureKit windows (id = `CGWindowID`) | ScreenCaptureKit window → raw NV12 on loopback → ffmpeg |
//!
//! The listing / capture code lives in `pulsar_core::window_capture`. Untested on Linux and
//! macOS hardware.

use pulsar_core::service::WindowInfo;
use pulsar_core::window_capture::{self as wc, Backend};

/// The window picker's entries for this host (answers `QueryWindows`).
pub(super) fn list() -> Vec<WindowInfo> {
	entries(wc::backend(), wc::list(), crate::i18n::t("host.windowPortalPick"))
}

/// [`list`] for a given backend and window list (pure, for tests).
fn entries(backend: Backend, windows: Vec<wc::HostWindow>, portal_title: &str) -> Vec<WindowInfo> {
	match backend {
		Backend::Portal => vec![WindowInfo { hwnd: WindowInfo::PORTAL_PICK, title: portal_title.to_string() }],
		Backend::None => Vec::new(),
		Backend::X11 | Backend::ScreenCaptureKit => windows
			.into_iter()
			.filter_map(|w| Some(WindowInfo { hwnd: i64::try_from(w.id).ok().filter(|v| *v > 0)?, title: w.title }))
			.collect(),
	}
}

/// What a stream's window target (`StreamReq::window_hwnd`, or the window of a game this
/// session launched) is on this host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Target {
	/// Linux X11: grab this X window.
	X11(u64),
	/// Linux Wayland: ask the person at the host to pick a window in the portal dialog.
	Portal,
	/// macOS: ScreenCaptureKit window with this `CGWindowID`.
	Mac(u64),
}

/// Resolve the stream's window target for `backend`; `None` = stream the screen (no
/// target, or an id this host cannot capture: a negative id is only ever
/// [`WindowInfo::PORTAL_PICK`], meaningless off Wayland).
pub(super) fn target(backend: Backend, id: Option<i64>) -> Option<Target> {
	let id = id?;
	match backend {
		// Wayland has no window ids at all: any window request opens the window dialog.
		Backend::Portal => Some(Target::Portal),
		Backend::X11 if id > 0 => Some(Target::X11(id as u64)),
		Backend::ScreenCaptureKit if id > 0 => Some(Target::Mac(id as u64)),
		_ => None,
	}
}

/// [`target`] for this host.
pub(super) fn current_target(id: Option<i64>) -> Option<Target> {
	target(wc::backend(), id)
}

/// The window's current pixel size (`None` = it is gone / not capturable).
pub(super) fn window_size(id: u64) -> Option<(u32, u32)> {
	wc::size(id).filter(|(w, h)| *w > 1 && *h > 1)
}

/// The encode size of a window of `size` streamed into the client's `max` box: its aspect,
/// never upscaled, even.
pub(super) fn encode_size(size: (u32, u32), max: (u32, u32)) -> (u32, u32) {
	pulsar_core::pipeline::fit_within(size, max)
}

/// Stop the session's in-process window source (macOS), on re-stream and teardown.
pub(super) fn stop(sid: u64) {
	wc::stop_for(sid);
}

#[cfg(test)]
mod tests {
	use super::*;

	fn hw(id: u64, title: &str) -> wc::HostWindow {
		wc::HostWindow { id, title: title.into(), pid: 1, width: 800, height: 600 }
	}

	#[test]
	fn picker_entries_per_backend() {
		let windows = vec![hw(0x3a0_0007, "Terminal"), hw(42, "Firefox")];
		let x = entries(Backend::X11, windows.clone(), "pick");
		assert_eq!(x.iter().map(|w| w.hwnd).collect::<Vec<_>>(), vec![0x3a0_0007, 42]);
		assert_eq!(x[1].title, "Firefox");
		let m = entries(Backend::ScreenCaptureKit, windows.clone(), "pick");
		assert_eq!(m.len(), 2);
		// Wayland: exactly one portal entry, whatever was listed.
		let w = entries(Backend::Portal, windows.clone(), "Pencereyi seç");
		assert_eq!(w, vec![WindowInfo { hwnd: WindowInfo::PORTAL_PICK, title: "Pencereyi seç".into() }]);
		assert!(entries(Backend::None, windows, "pick").is_empty());
		// An id that doesn't fit the wire (or is 0) is never offered.
		assert!(entries(Backend::X11, vec![hw(u64::MAX, "x"), hw(0, "y")], "pick").is_empty());
	}

	#[test]
	fn window_targets_per_backend() {
		assert_eq!(target(Backend::X11, Some(77)), Some(Target::X11(77)));
		assert_eq!(target(Backend::ScreenCaptureKit, Some(77)), Some(Target::Mac(77)));
		assert_eq!(target(Backend::Portal, Some(WindowInfo::PORTAL_PICK)), Some(Target::Portal));
		assert_eq!(target(Backend::Portal, Some(77)), Some(Target::Portal));
		// The portal token on a non-Wayland host, no target, no backend: the screen.
		assert_eq!(target(Backend::X11, Some(WindowInfo::PORTAL_PICK)), None);
		assert_eq!(target(Backend::ScreenCaptureKit, Some(0)), None);
		assert_eq!(target(Backend::X11, None), None);
		assert_eq!(target(Backend::None, Some(77)), None);
	}

	#[test]
	fn encode_size_keeps_the_window_aspect() {
		assert_eq!(encode_size((1000, 2000), (1920, 1080)), (540, 1080));
		assert_eq!(encode_size((801, 601), (1920, 1080)), (800, 600));
	}
}
