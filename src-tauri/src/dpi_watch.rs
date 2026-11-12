//! Windows: keep WebView2's render scale in step with the window's real DPI.
//!
//! 2026-11-13: plugging a monitor into the laptop left Pulsar's UI drawn tiny where it was —
//! the display change re-scaled the window's monitor, but WebView2 kept rasterizing at the old
//! scale until the window was dragged to another monitor (that move delivers WM_DPICHANGED).
//!
//! Event-driven, no polling: a hidden top-level window (broadcasts don't reach message-only
//! windows) hears `WM_DISPLAYCHANGE` (a monitor added/removed, a mode change) and
//! `WM_SETTINGCHANGE` (e.g. the scale setting). On each, every webview window's DPI
//! (`GetDpiForWindow`) is compared with its WebView2 controller's `RasterizationScale` — now
//! and a few times as the new layout settles — and the scale is set where they disagree.
#![cfg(windows)]

use std::sync::OnceLock;
use std::time::Duration;

use tauri::{AppHandle, Manager};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{
	CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW, TranslateMessage,
	MSG, WM_DISPLAYCHANGE, WM_SETTINGCHANGE, WNDCLASSW,
};

static APP: OnceLock<AppHandle> = OnceLock::new();

/// Re-checks after a display event: Windows settles the new layout (and each monitor's
/// scale) over a moment, so one check right away can still read the old DPI.
const SETTLE: [Duration; 4] = [
	Duration::ZERO,
	Duration::from_millis(300),
	Duration::from_secs(1),
	Duration::from_secs(3),
];

pub(crate) fn start(app: AppHandle) {
	if APP.set(app).is_err() {
		return;
	}
	let _ = std::thread::Builder::new().name("dpi-watch".into()).spawn(|| unsafe {
		let class: Vec<u16> = "PulsarDisplayWatch\0".encode_utf16().collect();
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
			tracing::warn!("dpi watch: could not create the display-change listener");
			return;
		}
		let mut msg: MSG = std::mem::zeroed();
		while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
			TranslateMessage(&msg);
			DispatchMessageW(&msg);
		}
	});
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
	if msg == WM_DISPLAYCHANGE || msg == WM_SETTINGCHANGE {
		if let Some(app) = APP.get() {
			let app = app.clone();
			std::thread::spawn(move || {
				let mut last = Duration::ZERO;
				for at in SETTLE {
					std::thread::sleep(at - last);
					last = at;
					sync_all(&app);
				}
			});
		}
	}
	DefWindowProcW(hwnd, msg, wp, lp)
}

/// Every webview window: set WebView2's RasterizationScale to the window's DPI if it differs.
fn sync_all(app: &AppHandle) {
	for (label, w) in app.webview_windows() {
		let Ok(hwnd) = w.hwnd() else { continue };
		let dpi = unsafe { windows_sys::Win32::UI::HiDpi::GetDpiForWindow(hwnd.0 as _) };
		if dpi == 0 {
			continue;
		}
		let want = dpi as f64 / 96.0;
		let main = label == "main";
		// The webview is single-threaded (STA): read + set the scale on the UI thread.
		let _ = w.with_webview(move |pw| unsafe {
			use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Controller3;
			use windows_core::Interface as _;
			let Ok(c3) = pw.controller().cast::<ICoreWebView2Controller3>() else { return };
			let mut cur = 0f64;
			if c3.RasterizationScale(&mut cur).is_err() || (cur - want).abs() < 0.01 {
				return;
			}
			tracing::info!(window = %label, from = cur, to = want, "dpi watch: webview scale out of step with the window — corrected");
			let _ = c3.SetRasterizationScale(want);
			if main {
				crate::process::set_ui_scale(want);
			}
		});
	}
}
