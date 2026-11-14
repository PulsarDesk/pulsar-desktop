//! Host per-window capture off Windows: the window list the client's picker shows, what a
//! picked window id means for this host's stream, and the window a session streams right
//! now (for the input mapping and the geometry watcher). Windows captures one window
//! natively through WGC (`pulsar-capture`) and never comes here.
//!
//! | Host | Picker entries | Stream source | Pointer |
//! | --- | --- | --- | --- |
//! | Linux X11 | `_NET_CLIENT_LIST` windows (id = X window id) | `x11grab -window_id` (libav / ffmpeg CLI) or gst `ximagesrc xid=` | uinput region = the window's rect in the root |
//! | Linux Wayland | one entry, [`WindowInfo::PORTAL_PICK`] | ScreenCast portal, windows only — the person at the host picks | whole desktop (Wayland tells no program where a window is) |
//! | macOS | ScreenCaptureKit windows (id = `CGWindowID`) | ScreenCaptureKit window → raw NV12 on loopback → ffmpeg | `CGEvent` points inside the window's frame |
//!
//! The listing / capture code lives in `pulsar_core::window_capture`. Untested on Linux and
//! macOS hardware.

use std::collections::HashMap;
use std::sync::Mutex;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::time::{Duration, Instant};

use pulsar_core::service::WindowInfo;
use pulsar_core::window_capture::{self as wc, Backend};

/// Run `f`, a blocking call (an X11 round trip, a ScreenCaptureKit query that can wait
/// seconds behind the Screen Recording prompt, a window resize), without stalling the
/// async runtime: on a multi-thread runtime the worker hands its other tasks off first
/// (`block_in_place`); elsewhere `f` just runs.
pub(crate) fn blocking<R>(f: impl FnOnce() -> R) -> R {
	match tokio::runtime::Handle::try_current() {
		Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
			tokio::task::block_in_place(f)
		}
		_ => f(),
	}
}

/// The window picker's entries for this host (answers `QueryWindows`).
pub(super) fn list() -> Vec<WindowInfo> {
	blocking(|| entries(wc::backend(), wc::list(), crate::i18n::t("host.windowPortalPick")))
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

/// What the input path maps the client's absolute pointer into while a session streams a
/// window: a 0..1 region of the root for the Linux uinput pointer, the window's frame in
/// global points for macOS `CGEvent`s.
#[cfg(target_os = "linux")]
pub(super) type InputArea = pulsar_core::capture::PointerRegion;
#[cfg(target_os = "macos")]
pub(super) type InputArea = wc::WindowRect;

#[cfg(target_os = "linux")]
fn query_area(id: u64) -> Option<InputArea> {
	wc::pointer_region(id)
}
#[cfg(target_os = "macos")]
fn query_area(id: u64) -> Option<InputArea> {
	wc::rect(id)
}

/// How long the input path reuses a window's position before asking again (it can be
/// moved while streamed; a pointer event asks at most this often).
#[cfg(any(target_os = "linux", target_os = "macos"))]
const AREA_TTL: Duration = Duration::from_millis(250);

/// The window one session streams right now.
struct Streamed {
	id: u64,
	/// Its size when the stream started (X11: pixels; macOS: points), for the watcher.
	started: Option<(u32, u32)>,
	#[cfg(any(target_os = "linux", target_os = "macos"))]
	area: Option<(Instant, Option<InputArea>)>,
}

/// Per session: the window streamed now (none while the screen is streamed).
static STREAMED: Mutex<Option<HashMap<u64, Streamed>>> = Mutex::new(None);
/// Per session: the window last brought to the front (raised once per pick, not on every
/// re-stream of the same window).
static RAISED: Mutex<Option<HashMap<u64, u64>>> = Mutex::new(None);

fn rounded_size(r: wc::WindowRect) -> (u32, u32) {
	(r.w.round().max(0.0) as u32, r.h.round().max(0.0) as u32)
}

/// Session `sid` now streams window `id`: map its input there, watch its geometry, and
/// bring it to the front (and give it the keyboard focus) if this session didn't already.
/// Blocking (an X11 / window-server round trip).
pub(super) fn began(sid: u64, id: u64) {
	let started = wc::rect(id).map(rounded_size);
	STREAMED.lock().unwrap().get_or_insert_with(HashMap::new).insert(
		sid,
		Streamed {
			id,
			started,
			#[cfg(any(target_os = "linux", target_os = "macos"))]
			area: None,
		},
	);
	let fresh = RAISED.lock().unwrap().get_or_insert_with(HashMap::new).insert(sid, id) != Some(id);
	if fresh {
		wc::raise(id);
	}
}

/// Session `sid` streams no window (the screen, or a re-stream is starting).
pub(super) fn ended(sid: u64) {
	if let Some(m) = STREAMED.lock().unwrap().as_mut() {
		m.remove(&sid);
	}
}

/// The area the session's absolute pointer maps into, `None` = no window streamed (the
/// caller keeps its screen mapping). Refreshed at most every [`AREA_TTL`].
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn input_area(sid: u64) -> Option<InputArea> {
	let id = {
		let g = STREAMED.lock().unwrap();
		let s = g.as_ref()?.get(&sid)?;
		match s.area {
			Some((at, area)) if at.elapsed() < AREA_TTL => return area,
			_ => s.id,
		}
	};
	// Asked outside the lock (an X11 / window-server round trip).
	let area = query_area(id);
	if let Some(s) = STREAMED.lock().unwrap().as_mut().and_then(|m| m.get_mut(&sid)) {
		if s.id == id {
			s.area = Some((Instant::now(), area));
		}
	}
	area
}

/// A change of the streamed window that needs a new stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Change {
	/// Its size changed (x11grab keeps the size it opened with; ScreenCaptureKit scales the
	/// window into the old frame): restart at the new size.
	Resized((u32, u32)),
	/// It was closed / can't be captured any more: restart, which streams the screen.
	Gone,
}

/// The change between the size a stream started with and the window now (`None` = no
/// window any more).
fn verdict(started: Option<(u32, u32)>, now: Option<(u32, u32)>) -> Option<Change> {
	match now {
		None => Some(Change::Gone),
		Some((w, h)) if w <= 1 || h <= 1 => Some(Change::Gone),
		Some(now) if started.is_some_and(|s| s != now) => Some(Change::Resized(now)),
		Some(_) => None,
	}
}

/// One look at session `sid`'s streamed window (`None` = nothing to do / no window).
/// Blocking: the host's watcher calls it from `spawn_blocking`.
pub(super) fn check(sid: u64) -> Option<Change> {
	let (id, started) = {
		let g = STREAMED.lock().unwrap();
		let s = g.as_ref()?.get(&sid)?;
		(s.id, s.started)
	};
	// macOS: ScreenCaptureKit stopped the stream by itself (window closed, permission
	// revoked) — the frame server would repeat the last frame forever.
	if wc::alive_for(sid) == Some(false) {
		return Some(Change::Gone);
	}
	verdict(started, wc::rect(id).map(rounded_size))
}

/// Only report a change seen on two looks in a row (a window being dragged to a new size
/// passes through many; restart once it settles).
#[derive(Default)]
pub(super) struct Debounce {
	last: Option<Change>,
}

impl Debounce {
	pub(super) fn feed(&mut self, seen: Option<Change>) -> Option<Change> {
		let prev = std::mem::replace(&mut self.last, seen);
		match seen {
			Some(c) if prev == Some(c) => {
				self.last = None;
				Some(c)
			}
			_ => None,
		}
	}
}

/// Stop the session's in-process window source (macOS) and forget its window, on every
/// re-stream (the new stream registers again through [`began`]).
pub(super) fn stop(sid: u64) {
	wc::stop_for(sid);
	ended(sid);
}

/// Session teardown: [`stop`], and forget what it raised.
pub(super) fn end(sid: u64) {
	stop(sid);
	if let Some(m) = RAISED.lock().unwrap().as_mut() {
		m.remove(&sid);
	}
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

	#[test]
	fn window_changes_that_need_a_new_stream() {
		assert_eq!(verdict(Some((800, 600)), Some((800, 600))), None);
		assert_eq!(verdict(Some((800, 600)), Some((1024, 600))), Some(Change::Resized((1024, 600))));
		assert_eq!(verdict(Some((800, 600)), None), Some(Change::Gone));
		// Unmapped / shrunk to nothing counts as gone.
		assert_eq!(verdict(Some((800, 600)), Some((1, 1))), Some(Change::Gone));
		// No start size known: only a vanished window matters.
		assert_eq!(verdict(None, Some((1024, 600))), None);
	}

	#[test]
	fn debounce_waits_for_a_settled_change() {
		let mut d = Debounce::default();
		let a = Some(Change::Resized((900, 600)));
		let b = Some(Change::Resized((1000, 600)));
		assert_eq!(d.feed(a), None);
		// Still being dragged: a different size, wait again.
		assert_eq!(d.feed(b), None);
		assert_eq!(d.feed(b), b);
		// Reported once; the same change must be seen twice again before it fires anew.
		assert_eq!(d.feed(b), None);
		assert_eq!(d.feed(None), None);
		assert_eq!(d.feed(Some(Change::Gone)), None);
		assert_eq!(d.feed(Some(Change::Gone)), Some(Change::Gone));
	}

	#[test]
	fn sessions_track_their_window() {
		// No window streamed: nothing to check, no input area.
		assert_eq!(check(9_999_001), None);
		#[cfg(any(target_os = "linux", target_os = "macos"))]
		assert!(input_area(9_999_001).is_none());
		end(9_999_001);
		assert!(STREAMED.lock().unwrap().as_ref().is_none_or(|m| !m.contains_key(&9_999_001)));
	}
}
