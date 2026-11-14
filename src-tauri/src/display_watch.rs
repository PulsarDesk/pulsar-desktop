//! "The host's monitors changed" — a monitor plugged in or removed, a resolution / refresh /
//! primary change — for pushing the live monitor list to connected clients
//! (`DataMsg::Displays`; the client otherwise only learns it once, at connect).
//!
//! Windows: event-driven — a hidden top-level window (display broadcasts don't reach
//! message-only windows) hears `WM_DISPLAYCHANGE` and bumps a generation counter. It is its
//! own window, independent of the UI's dpi_watch, so a headless host gets it too.
//! macOS / Linux: `pulsar_core::displays::watch` — CoreGraphics'
//! `CGDisplayRegisterReconfigurationCallback` on macOS; on Linux a long-lived Wayland
//! connection (`wl_output` added / removed / `done`) or X11 RandR notifications. Until that
//! source is live (macOS arms it on the main queue) or after it dropped out (a lost
//! connection), [`event_driven`] is false and the caller polls by comparing the list.
//! The macOS and Linux sources are untested on real hardware.

use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

struct Gen {
	n: Mutex<u64>,
	cv: Condvar,
}

fn state() -> &'static Gen {
	static G: OnceLock<Gen> = OnceLock::new();
	G.get_or_init(|| {
		#[cfg(windows)]
		start_listener();
		#[cfg(not(windows))]
		{
			// Not from inside this initializer: the callback runs on the watcher's own
			// thread (or the macOS main thread) after init returned.
			let armed = pulsar_core::displays::watch(Box::new(bump));
			tracing::info!(armed, "display watch: started");
		}
		Gen { n: Mutex::new(0), cv: Condvar::new() }
	})
}

/// The current change generation (starts the watcher on first use).
pub(crate) fn generation() -> u64 {
	*state().n.lock().unwrap()
}

/// Block until the generation moves past `seen` or `timeout` passes; returns the generation.
pub(crate) fn wait_change(seen: u64, timeout: Duration) -> u64 {
	let g = state();
	let n = g.n.lock().unwrap();
	let (n, _) = g.cv.wait_timeout_while(n, timeout, |n| *n == seen).unwrap();
	*n
}

/// True while [`wait_change`] is woken by a real OS event (else callers poll). Windows: always;
/// macOS / Linux: while the `pulsar_core::displays` watcher is live.
pub(crate) fn event_driven() -> bool {
	let _ = state();
	cfg!(windows) || pulsar_core::displays::watching()
}

fn bump() {
	let g = state();
	*g.n.lock().unwrap() += 1;
	g.cv.notify_all();
}

#[cfg(windows)]
fn start_listener() {
	use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
	use windows_sys::Win32::UI::WindowsAndMessaging::{
		CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW, TranslateMessage,
		MSG, WM_DISPLAYCHANGE, WNDCLASSW,
	};
	unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
		if msg == WM_DISPLAYCHANGE {
			// Not from inside state()'s initializer: this runs on the listener thread, after
			// init returned (the window is created there, messages pumped later).
			bump();
		}
		DefWindowProcW(hwnd, msg, wp, lp)
	}
	let _ = std::thread::Builder::new().name("display-watch".into()).spawn(|| unsafe {
		let class: Vec<u16> = "PulsarHostDisplayWatch\0".encode_utf16().collect();
		let wc = WNDCLASSW {
			lpfnWndProc: Some(wndproc),
			lpszClassName: class.as_ptr(),
			..std::mem::zeroed()
		};
		RegisterClassW(&wc);
		// Top-level but never shown (no WS_VISIBLE): receives the display broadcasts.
		let hwnd = CreateWindowExW(
			0,
			class.as_ptr(),
			std::ptr::null(),
			0,
			0,
			0,
			0,
			0,
			std::ptr::null_mut(),
			std::ptr::null_mut(),
			std::ptr::null_mut(),
			std::ptr::null(),
		);
		if hwnd.is_null() {
			tracing::warn!("display watch: could not create the display-change listener");
			return;
		}
		let mut msg: MSG = std::mem::zeroed();
		while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
			TranslateMessage(&msg);
			DispatchMessageW(&msg);
		}
	});
}
