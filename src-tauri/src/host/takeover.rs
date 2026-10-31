//! Host: the person at the host takes the mouse back (AnyDesk/Parsec style). While the host's
//! OWN mouse moved within the last [`HOLD`], remote-desktop clients' pointer input is not
//! injected — their pointer is drawn as a labelled ghost instead (`crate::remote_cursor`).
//!
//! - Windows: a `WH_MOUSE_LL` hook; injected events (`LLMHF_INJECTED`, which includes our
//!   own `SendInput`) don't count.
//! - Linux: reads (never grabs) every pointer device under `/dev/input` except Pulsar's own
//!   uinput ones. Needs read access (the `input` group); without it nothing is detected and
//!   remote input always applies, as before.
//! - macOS: not detected.

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
	std::thread::Builder::new()
		.name("pulsar-takeover".into())
		.spawn(linux::run)
		.ok();
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
		if d.name().is_some_and(|n| n.starts_with("Pulsar")) {
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
