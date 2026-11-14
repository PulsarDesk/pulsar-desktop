//! Host: the person at the host takes the mouse back (AnyDesk/Parsec style). While the host's
//! OWN mouse moved within the last [`HOLD`], remote-desktop clients' pointer input is not
//! injected — their pointer is drawn as a labelled ghost instead (`crate::remote_cursor`).
//!
//! - Windows: a `WH_MOUSE_LL` hook; injected events (`LLMHF_INJECTED`, which includes our
//!   own `SendInput`) don't count.
//! - Linux: reads (never grabs) every pointer device under `/dev/input` except Pulsar's own
//!   uinput ones. Needs read access (the `input` group). On X11 an XInput2 raw-event listener
//!   runs as well (no permission needed): raw motion / button events from every slave device
//!   except Pulsar's uinput ones and the XTEST device (injected input). Wayland without
//!   `/dev/input` access: nothing is detected (a Wayland client never sees global pointer
//!   events) and remote input always applies, as before.
//! - macOS: a listen-only `CGEventTap` (`pulsar_core::input::local_pointer_macos`); Pulsar's
//!   own injected events carry a `kCGEventSourceUserData` tag and don't count.
//!
//! [`PointerPos`] reads where the host pointer is on the streamed screen for the client's
//! "Host" marker (Windows: in `host/handlers.rs`; Linux X11: XQueryPointer against the
//! captured monitor; macOS: the main display; Wayland: unknown).
//!
//! Linux X11 / macOS: untested on hardware.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// How long after the host's own mouse movement remote pointer input stays held back.
const HOLD: Duration = Duration::from_millis(1200);

static EPOCH: OnceLock<Instant> = OnceLock::new();
/// ms since [`EPOCH`] of the last local mouse event (0 = never).
static LAST_LOCAL: AtomicU64 = AtomicU64::new(0);

fn touch() {
	let ms = EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64;
	LAST_LOCAL.store(ms.max(1), Ordering::Relaxed);
}

/// Did the host's own mouse move recently?
pub(crate) fn active() -> bool {
	let last = LAST_LOCAL.load(Ordering::Relaxed);
	if last == 0 {
		return false;
	}
	let now = EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64;
	now.saturating_sub(last) < HOLD.as_millis() as u64
}

/// Start watching the local mouse (once; cheap to call again).
pub(crate) fn start() {
	static STARTED: OnceLock<()> = OnceLock::new();
	if STARTED.set(()).is_err() {
		return;
	}
	EPOCH.get_or_init(Instant::now);
	#[cfg(windows)]
	std::thread::Builder::new()
		.name("pulsar-takeover".into())
		.spawn(|| unsafe { win::run() })
		.ok();
	#[cfg(target_os = "linux")]
	{
		std::thread::Builder::new()
			.name("pulsar-takeover".into())
			.spawn(linux::run)
			.ok();
		if !pulsar_core::capture::is_wayland() && std::env::var_os("DISPLAY").is_some() {
			std::thread::Builder::new()
				.name("pulsar-takeover-xi2".into())
				.spawn(|| unsafe { xi2::run() })
				.ok();
		}
	}
	#[cfg(target_os = "macos")]
	std::thread::Builder::new()
		.name("pulsar-takeover".into())
		.spawn(|| {
			use pulsar_core::input::local_pointer_macos as lp;
			if !lp::request_listen_access() {
				tracing::info!("takeover: listen-event access not granted (yet)");
			}
			// The tap fails until the grant exists; retry so a later grant works without a
			// restart. `watch_local_pointer` only returns early on failure.
			let mut warned = false;
			loop {
				match lp::watch_local_pointer(touch) {
					Ok(()) => return,
					Err(e) => {
						if !warned {
							warned = true;
							tracing::warn!("takeover: {e} — host mouse takeover off, retrying every 10 s");
						}
						std::thread::sleep(std::time::Duration::from_secs(10));
					}
				}
			}
		})
		.ok();
}

/// A device that delivers Pulsar's own (or other synthetic) input, never the local user's:
/// Pulsar's uinput devices ("Pulsar Virtual …") and X's XTEST device (xdotool & co.).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn is_synthetic_device(name: &str) -> bool {
	name.starts_with("Pulsar") || name.contains("XTEST")
}

/// Where `(x, y)` falls in the rect `(ox, oy, w, h)`, as 0..1; `None` = outside / empty rect.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn fraction_in(x: i32, y: i32, ox: i32, oy: i32, w: u32, h: u32) -> Option<(f32, f32)> {
	if w == 0 || h == 0 {
		return None;
	}
	let nx = (x - ox) as f32 / w as f32;
	let ny = (y - oy) as f32 / h as f32;
	((0.0..=1.0).contains(&nx) && (0.0..=1.0).contains(&ny)).then_some((nx, ny))
}

/// Reads the host pointer's position on the streamed screen (0..1) for the client's "Host"
/// marker on Linux X11 (Windows reads it in `host/handlers.rs`, macOS through
/// `pulsar_core::displays::pointer_in` against the streamed display). `None` = unknown.
/// Used from one thread (the X connection is not shared).
#[cfg(target_os = "linux")]
pub(crate) struct PointerPos {
	x: Option<x11pos::X11Pointer>,
}

#[cfg(target_os = "linux")]
impl PointerPos {
	/// `display_idx` = the monitor the stream captures (Linux X11 multi-monitor).
	pub(crate) fn new(display_idx: u32) -> Self {
		Self { x: x11pos::X11Pointer::open(display_idx) }
	}

	pub(crate) fn read(&mut self) -> Option<(f32, f32)> {
		self.x.as_mut().and_then(|x| x.read())
	}
}

#[cfg(target_os = "linux")]
mod x11pos {
	use x11::xlib;

	pub(super) struct X11Pointer {
		dpy: *mut xlib::Display,
		root: xlib::Window,
		/// The captured monitor in root coordinates: x, y, w, h.
		rect: (i32, i32, u32, u32),
	}

	impl X11Pointer {
		pub(super) fn open(display_idx: u32) -> Option<Self> {
			if pulsar_core::capture::is_wayland() {
				return None; // a Wayland client can't read the global pointer
			}
			// SAFETY: plain Xlib calls; the display is closed in Drop.
			unsafe {
				let dpy = xlib::XOpenDisplay(std::ptr::null());
				if dpy.is_null() {
					return None;
				}
				let root = xlib::XDefaultRootWindow(dpy);
				let scr = xlib::XDefaultScreen(dpy);
				let whole = (
					0,
					0,
					xlib::XDisplayWidth(dpy, scr).max(0) as u32,
					xlib::XDisplayHeight(dpy, scr).max(0) as u32,
				);
				// Same choice as the capture (`host/handlers.rs`): one monitor of several, else
				// the whole root.
				let displays = crate::process::linux_displays();
				let rect = displays
					.get(display_idx as usize)
					.filter(|_| displays.len() > 1)
					.map(|d| (d.1, d.2, d.3, d.4))
					.unwrap_or(whole);
				Some(Self { dpy, root, rect })
			}
		}

		pub(super) fn read(&mut self) -> Option<(f32, f32)> {
			let (mut root_ret, mut child_ret) = (0, 0);
			let (mut rx, mut ry, mut wx, mut wy) = (0, 0, 0, 0);
			let mut mask = 0u32;
			// SAFETY: a live display + out-params on the stack.
			let ok = unsafe {
				xlib::XQueryPointer(
					self.dpy, self.root, &mut root_ret, &mut child_ret, &mut rx, &mut ry, &mut wx,
					&mut wy, &mut mask,
				)
			} != 0;
			let (ox, oy, w, h) = self.rect;
			if ok {
				super::fraction_in(rx, ry, ox, oy, w, h)
			} else {
				None
			}
		}
	}

	impl Drop for X11Pointer {
		fn drop(&mut self) {
			// SAFETY: opened in `open`, closed once.
			unsafe { xlib::XCloseDisplay(self.dpy) };
		}
	}
}

/// X11: XInput2 raw events (delivered regardless of focus or grabs, no permission needed).
#[cfg(target_os = "linux")]
mod xi2 {
	use std::collections::HashMap;
	use x11::{xinput2 as xi, xlib};

	/// Is slave device `id` a real (local) one? Cached per id; the cache is cleared on every
	/// `XI_HierarchyChanged` (ids are reused when devices go away). A failed lookup (device
	/// already gone) counts as local but is not cached.
	unsafe fn is_local(dpy: *mut xlib::Display, id: i32, cache: &mut HashMap<i32, bool>) -> bool {
		if let Some(&v) = cache.get(&id) {
			return v;
		}
		let mut n = 0;
		let info = xi::XIQueryDevice(dpy, id, &mut n);
		if info.is_null() {
			return true;
		}
		let local = if n < 1 || (*info).name.is_null() {
			None
		} else {
			let name = std::ffi::CStr::from_ptr((*info).name).to_string_lossy();
			Some(!super::is_synthetic_device(&name))
		};
		xi::XIFreeDeviceInfo(info);
		match local {
			Some(v) => {
				cache.insert(id, v);
				v
			}
			None => true,
		}
	}

	pub(super) unsafe fn run() {
		let dpy = xlib::XOpenDisplay(std::ptr::null());
		if dpy.is_null() {
			return;
		}
		let (mut opcode, mut ev, mut err) = (0, 0, 0);
		if xlib::XQueryExtension(dpy, c"XInputExtension".as_ptr(), &mut opcode, &mut ev, &mut err)
			== 0
		{
			tracing::info!("takeover: no XInput extension — X11 raw-event watch off");
			xlib::XCloseDisplay(dpy);
			return;
		}
		// Raw events on the root window regardless of grabs need XI 2.1.
		let (mut major, mut minor) = (2, 1);
		if xi::XIQueryVersion(dpy, &mut major, &mut minor) != 0 || (major, minor) < (2, 1) {
			tracing::info!(major, minor, "takeover: XInput < 2.1 — X11 raw-event watch off");
			xlib::XCloseDisplay(dpy);
			return;
		}
		let mut bits = [0u8; 4];
		for e in [xi::XI_RawMotion, xi::XI_RawButtonPress, xi::XI_RawButtonRelease] {
			xi::XISetMask(&mut bits, e);
		}
		let mask = xi::XIEventMask {
			deviceid: xi::XIAllMasterDevices,
			mask_len: bits.len() as i32,
			mask: bits.as_mut_ptr(),
		};
		// Devices come and go (hot-plug, our own uinput ones per session) and X reuses ids:
		// the hierarchy event clears the cache so a reused id is looked up again.
		let mut hbits = [0u8; 4];
		xi::XISetMask(&mut hbits, xi::XI_HierarchyChanged);
		let mut masks = [
			mask,
			xi::XIEventMask {
				deviceid: xi::XIAllDevices,
				mask_len: hbits.len() as i32,
				mask: hbits.as_mut_ptr(),
			},
		];
		xi::XISelectEvents(dpy, xlib::XDefaultRootWindow(dpy), masks.as_mut_ptr(), 2);
		xlib::XFlush(dpy);
		let mut cache: HashMap<i32, bool> = HashMap::new();
		let mut event: xlib::XEvent = std::mem::zeroed();
		loop {
			xlib::XNextEvent(dpy, &mut event);
			if event.get_type() != xlib::GenericEvent {
				continue;
			}
			let cookie = &mut event.generic_event_cookie;
			if cookie.extension != opcode || xlib::XGetEventData(dpy, cookie) == 0 {
				continue;
			}
			let raw_ev = matches!(
				cookie.evtype,
				xi::XI_RawMotion | xi::XI_RawButtonPress | xi::XI_RawButtonRelease
			);
			if cookie.evtype == xi::XI_HierarchyChanged {
				cache.clear();
			} else if raw_ev && !cookie.data.is_null() {
				let raw = &*(cookie.data as *const xi::XIRawEvent);
				if is_local(dpy, raw.sourceid, &mut cache) {
					super::touch();
				}
			}
			xlib::XFreeEventData(dpy, cookie);
		}
	}
}

#[cfg(windows)]
mod win {
	use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
	use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
	use windows_sys::Win32::UI::WindowsAndMessaging::{
		CallNextHookEx, GetMessageW, SetWindowsHookExW, LLMHF_INJECTED, LLMHF_LOWER_IL_INJECTED, MSG,
		MSLLHOOKSTRUCT, WH_MOUSE_LL,
	};

	unsafe extern "system" fn proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
		if code >= 0 && lp != 0 {
			let ms = &*(lp as *const MSLLHOOKSTRUCT);
			if ms.flags & (LLMHF_INJECTED | LLMHF_LOWER_IL_INJECTED) == 0 {
				super::touch();
			}
		}
		CallNextHookEx(std::ptr::null_mut(), code, wp, lp)
	}

	pub(super) unsafe fn run() {
		let hook = SetWindowsHookExW(WH_MOUSE_LL, Some(proc), GetModuleHandleW(std::ptr::null()), 0);
		if hook.is_null() {
			tracing::warn!("takeover: WH_MOUSE_LL hook failed — host mouse takeover disabled");
			return;
		}
		// LL hooks are called on this thread's message loop.
		let mut msg: MSG = std::mem::zeroed();
		while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {}
	}
}

#[cfg(target_os = "linux")]
mod linux {
	use evdev::{AbsoluteAxisCode, EventType, RelativeAxisCode};
	use std::collections::HashSet;
	use std::path::PathBuf;
	use std::sync::{Arc, Mutex};
	use std::time::Duration;

	fn is_pointer(d: &evdev::Device) -> bool {
		if d.name().is_some_and(super::is_synthetic_device) {
			return false; // our own uinput devices (the injected input)
		}
		d.supported_relative_axes().is_some_and(|a| a.contains(RelativeAxisCode::REL_X))
			|| d.supported_absolute_axes().is_some_and(|a| a.contains(AbsoluteAxisCode::ABS_X))
				&& d.supported_events().contains(EventType::KEY)
				&& d.supported_keys().is_some_and(|k| {
					k.contains(evdev::KeyCode::BTN_TOUCH) || k.contains(evdev::KeyCode::BTN_LEFT)
				})
	}

	/// Watch every pointer device; rescan for hot-plugged ones every few seconds.
	pub(super) fn run() {
		let open: Arc<Mutex<HashSet<PathBuf>>> = Arc::default();
		let mut warned = false;
		loop {
			let mut seen = 0;
			for (path, mut dev) in evdev::enumerate() {
				seen += 1;
				if !is_pointer(&dev) || !open.lock().unwrap().insert(path.clone()) {
					continue;
				}
				let open = open.clone();
				std::thread::Builder::new()
					.name("pulsar-takeover-dev".into())
					.spawn(move || {
						while let Ok(evs) = dev.fetch_events() {
							if evs.into_iter().any(|e| {
								matches!(e.event_type(), EventType::RELATIVE | EventType::ABSOLUTE | EventType::KEY)
							}) {
								super::touch();
							}
						}
						open.lock().unwrap().remove(&path);
					})
					.ok();
			}
			if seen == 0 && !warned {
				warned = true;
				tracing::info!("takeover: no readable /dev/input devices — host mouse takeover disabled");
			}
			std::thread::sleep(Duration::from_secs(3));
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn synthetic_devices_are_ours() {
		assert!(is_synthetic_device("Pulsar Virtual Pointer"));
		assert!(is_synthetic_device("Pulsar Virtual Mouse"));
		assert!(is_synthetic_device("Virtual core XTEST pointer"));
		assert!(!is_synthetic_device("Logitech USB Optical Mouse"));
		assert!(!is_synthetic_device("SynPS/2 Synaptics TouchPad"));
	}

	#[test]
	fn fraction_in_rect() {
		assert_eq!(fraction_in(960, 540, 0, 0, 1920, 1080), Some((0.5, 0.5)));
		assert_eq!(fraction_in(1920 + 640, 400, 1920, 0, 1280, 800), Some((0.5, 0.5)));
		assert_eq!(fraction_in(100, 100, 1920, 0, 1280, 800), None);
		assert_eq!(fraction_in(1, 1, 0, 0, 0, 0), None);
	}

	#[test]
	fn local_move_holds_remote_input() {
		touch();
		assert!(active());
	}
}
