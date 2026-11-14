//! Host-side "screen adaptation" (Parsec-style): switch a captured monitor to the
//! display mode that best fits a client split-pane, and revert it on teardown.
//!
//! When a client streams to a small split pane (e.g. a 1280×800 renderer window) the
//! host's native 3840×2160 capture gets downscaled/letterboxed into that pane — wasting
//! encode bandwidth on pixels the pane can't show and giving the wrong aspect. Parsec
//! instead temporarily changes the HOST monitor's resolution to one that matches the
//! pane's shape, so the captured framebuffer is already the right size/aspect. We do the
//! same: [`set_best_mode`] picks the closest-aspect available mode and applies it; the
//! returned [`PrevMode`] captures the monitor's ORIGINAL mode so [`revert`] can restore it
//! exactly when the session tears down (or when a later `StreamReq` sends `adapt: None`).
//!
//! IMPORTANT — robustness: the host must NEVER be left stuck at a wrong resolution. The
//! revert is the guarantee; it is driven from `SessionCleanupGuard::drop` (host.rs), which
//! fires on BOTH a normal session end and an `abort()`-path teardown. We snapshot the
//! revert target (`ENUM_CURRENT_SETTINGS`) BEFORE applying the change, and only return a
//! `PrevMode` when the apply actually succeeded — so we never store a bogus target.
//!
//! Per platform:
//! * **Windows** — `EnumDisplaySettingsW` / `ChangeDisplaySettingsExW` (below).
//! * **Linux** — [`linux`]: X11 via `xrandr`; Wayland via the compositor's own tool/API
//!   (wlroots `wlr-randr`, KDE `kscreen-doctor`, GNOME Mutter `DisplayConfig` D-Bus) — Wayland
//!   has no standard output-management protocol, so other compositors keep their mode. A crash
//!   journal restores a mode left switched by a crashed Pulsar at the next start.
//! * **macOS** — CoreGraphics (`pulsar_capture::display_mode::macos`): the change is made
//!   `kCGConfigureForAppOnly`, so the window server restores the mode when Pulsar exits, even
//!   on a crash.
//!
//! The best-fit choice itself is shared (`pulsar_capture::display_mode::plan`). Linux and macOS
//! are untested on hardware.

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------
#[cfg(windows)]
pub use win::{revert, set_best_mode, PrevMode};
#[cfg(windows)]
pub use win_fit::{fit_window, restore_window, WindowPrev};

/// Per-session screen-adaptation state, shared (via `Arc<Mutex<_>>`) between the StreamReq
/// handler (`make_on_stream`) and the session's [`SessionCleanupGuard`](crate::host) so the
/// teardown can revert whatever the handler applied.
///
/// * `prev` — the monitor's original [`PrevMode`] when we currently hold an applied change
///   (`Some`), or `None` when the monitor is at its native mode. Teardown `take()`s and
///   reverts it (idempotent — a second drop sees `None`).
/// * `last` — the last `(display_idx, w, h)` adapt TARGET we acted on, so a repeated StreamReq
///   carrying the same target is a no-op instead of re-running the best-fit every frame. It is
///   set even when [`set_best_mode`] returns `None` (current mode already best / apply failed),
///   so we don't retry a no-op target on each restream.
#[derive(Default)]
pub struct AdaptState {
	pub prev: Option<PrevMode>,
	pub last: Option<(u32, u32, u32)>,
	/// Per-WINDOW adaptation (a split pane streaming one host app): the window's original
	/// placement while we hold it resized to the pane's shape — restored on teardown, when the
	/// client turns adaptation off, or when the capture target changes.
	pub win_prev: Option<WindowPrev>,
	/// The last `(hwnd, w, h)` window-fit target acted on (same no-repeat rule as `last`).
	pub win_last: Option<(i64, u32, u32)>,
}

#[cfg(windows)]
mod win {
	use windows_sys::Win32::Graphics::Gdi::{
		ChangeDisplaySettingsExW, EnumDisplaySettingsW, DEVMODEW, DISP_CHANGE_SUCCESSFUL,
		ENUM_CURRENT_SETTINGS,
	};

	/// The monitor's display mode BEFORE we changed it, kept so the session teardown can
	/// restore it byte-for-byte. We keep the full original `DEVMODEW` (queried via
	/// `ENUM_CURRENT_SETTINGS`) so the revert re-applies the exact same width/height/depth/
	/// frequency/orientation the user had — not an approximation we'd have to reconstruct.
	/// `device` is the GDI device name (`\\.\DISPLAYn`) the change/revert target. `DEVMODEW`
	/// is `Copy` + plain-old-data (no pointers), so this is `Send` and safe to stash in an
	/// `Arc<Mutex<Option<PrevMode>>>` shared with the cleanup guard.
	#[derive(Clone)]
	pub struct PrevMode {
		device: Vec<u16>,
		devmode: DEVMODEW,
	}

	/// Resolve the GDI device name (`\\.\DISPLAYn`) for `display_idx` as a NUL-terminated wide
	/// string. We reuse `pulsar_capture::list_displays()` — the SAME enumeration `host_displays`
	/// and the capture path index by — so `display_idx` here means the exact monitor the client
	/// selected (its `name` is the DXGI `DeviceName`, i.e. the GDI `\\.\DISPLAYn` trimmed of the
	/// `\\.\` prefix; we re-prepend it, matching `util::display_rotation_detect`). `None` when the
	/// index is out of range (stale picker / monitor unplugged).
	fn device_name_wide(display_idx: u32) -> Option<Vec<u16>> {
		let displays = pulsar_capture::list_displays();
		let (_idx, name, _w, _h, _primary) = displays.into_iter().nth(display_idx as usize)?;
		let full = format!(r"\\.\{name}");
		Some(full.encode_utf16().chain(std::iter::once(0u16)).collect())
	}

	/// Read the monitor's CURRENT mode (`ENUM_CURRENT_SETTINGS`). `None` on failure.
	unsafe fn current_mode(device: &[u16]) -> Option<DEVMODEW> {
		let mut dm: DEVMODEW = std::mem::zeroed();
		dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
		if EnumDisplaySettingsW(device.as_ptr(), ENUM_CURRENT_SETTINGS, &mut dm) != 0 {
			Some(dm)
		} else {
			None
		}
	}

	/// Score a candidate mode against the target pane size; LOWER is better. Primary key is the
	/// aspect-ratio distance (so the captured frame matches the pane's shape — the whole point of
	/// adaptation). Among near-equal aspect (within ~1%) we tie-break on area distance to the pane,
	/// with a mild bias toward "tall enough" modes (height ≥ 1080) so adaptation doesn't drop a
	/// big monitor to a tiny letterbox-equivalent resolution. `native_area` lets us avoid exceeding
	/// the monitor's native resolution when a smaller equally-good-aspect mode exists.
	fn score(
		mw: u32,
		mh: u32,
		target_w: u32,
		target_h: u32,
		native_area: u64,
	) -> (i64, i64) {
		// Aspect distance, scaled to an integer so the tuple can be Ord-compared. We compare
		// w*th vs h*tw (cross-multiply) to avoid float division entirely.
		let mode_cross = mw as i64 * target_h as i64;
		let target_cross = mh as i64 * target_w as i64;
		// Normalize the cross-product difference so it's comparable across resolutions: divide by
		// the target dimension product magnitude. Multiply up first to keep integer precision.
		let denom = (target_w as i64 * target_h as i64).max(1);
		let aspect_dist = ((mode_cross - target_cross).abs() * 10_000) / denom;
		// Area distance to the pane (secondary). A mode that EXCEEDS native is penalized so an
		// equally-shaped non-native mode is preferred (don't push the monitor past its panel).
		let area = mw as i64 * mh as i64;
		let target_area = target_w as i64 * target_h as i64;
		let mut area_dist = (area - target_area).abs();
		// Mild preference for ≥1080 lines (keeps adaptation from collapsing to a tiny mode when
		// several share the pane's aspect): bump the area_dist of sub-1080 modes.
		if mh < 1080 {
			area_dist += target_area / 2;
		}
		// Penalize exceeding native area (we'd rather not over-drive the panel).
		if area as u64 > native_area {
			area_dist += area - native_area as i64;
		}
		(aspect_dist, area_dist)
	}

	/// Switch the monitor at `display_idx` to the available mode that best fits a `target_w ×
	/// target_h` pane, recording the original mode for [`revert`]. Returns `Some(PrevMode)` only
	/// when a real change was applied; `None` when there is nothing to do or revert:
	///   - the index is invalid / the device name can't be resolved,
	///   - the current mode is already the best fit (no change → nothing to revert),
	///   - the `ChangeDisplaySettingsExW` apply did not return `DISP_CHANGE_SUCCESSFUL`.
	///
	/// Best-fit: smallest aspect-ratio distance to the pane, then closest area (see [`score`]),
	/// preferring ≥1080 lines and not over-driving the panel past its native resolution.
	pub fn set_best_mode(display_idx: u32, target_w: u32, target_h: u32) -> Option<PrevMode> {
		if target_w == 0 || target_h == 0 {
			return None;
		}
		let device = device_name_wide(display_idx)?;
		unsafe {
			// Snapshot the current mode FIRST — this is the exact revert target.
			let original = current_mode(&device)?;
			let cur_w = original.dmPelsWidth;
			let cur_h = original.dmPelsHeight;
			// Native = the monitor's current (largest sensible) resolution; used so we don't pick a
			// mode that over-drives the panel when an equally-shaped smaller mode exists.
			let native_area = cur_w as u64 * cur_h as u64;

			// Enumerate every mode at the current color depth and pick the best fit.
			let mut best: Option<((i64, i64), DEVMODEW)> = None;
			let mut i = 0u32;
			loop {
				let mut dm: DEVMODEW = std::mem::zeroed();
				dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
				if EnumDisplaySettingsW(device.as_ptr(), i, &mut dm) == 0 {
					break; // exhausted the mode list
				}
				i += 1;
				// Only consider modes at the CURRENT color depth (mixing bit depths would let a
				// lower-quality mode win on a near-tie, and we don't want to change the depth).
				if dm.dmBitsPerPel != original.dmBitsPerPel {
					continue;
				}
				let s = score(dm.dmPelsWidth, dm.dmPelsHeight, target_w, target_h, native_area);
				match &best {
					Some((bs, _)) if *bs <= s => {}
					_ => best = Some((s, dm)),
				}
			}

			let (_, mut chosen) = best?;
			// Nothing to do if the best fit is what we're already running (avoids a needless mode
			// flap + a bogus PrevMode that revert() would then re-apply for no reason).
			if chosen.dmPelsWidth == cur_w && chosen.dmPelsHeight == cur_h {
				return None;
			}
			// Apply the chosen resolution. We mask dmFields to exactly the geometry we set so the
			// driver doesn't try to honor stale/zeroed fields from our copy. flags = 0 = a dynamic
			// change that takes effect immediately (no reboot, persists for the session). NULL hwnd
			// + NULL lparam per the Win32 contract for a plain resolution change.
			use windows_sys::Win32::Graphics::Gdi::{
				DM_BITSPERPEL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH,
			};
			chosen.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_BITSPERPEL | DM_DISPLAYFREQUENCY;
			let rc = ChangeDisplaySettingsExW(
				device.as_ptr(),
				&chosen as *const DEVMODEW,
				std::ptr::null_mut(),
				0,
				std::ptr::null(),
			);
			if rc != DISP_CHANGE_SUCCESSFUL {
				// Failed apply: do NOT store a PrevMode (there is no change to revert). The monitor
				// is untouched, so the stream just proceeds at the host's existing resolution.
				tracing::warn!(
					display_idx,
					target_w,
					target_h,
					new_w = chosen.dmPelsWidth,
					new_h = chosen.dmPelsHeight,
					rc,
					"screen-adapt: ChangeDisplaySettingsExW failed; leaving mode unchanged"
				);
				return None;
			}
			tracing::info!(
				display_idx,
				target_w,
				target_h,
				from = format!("{cur_w}x{cur_h}"),
				to = format!("{}x{}", chosen.dmPelsWidth, chosen.dmPelsHeight),
				"screen-adapt: switched host display mode for split pane"
			);
			Some(PrevMode { device, devmode: original })
		}
	}

	/// Restore the monitor's original mode recorded in `prev`. Best-effort (logs on failure):
	/// teardown must not panic, and there is no better fallback than leaving the user to fix the
	/// resolution manually if Windows refuses the restore (rare — re-applying a mode the panel was
	/// just at). flags = 0 = a dynamic change, same as the forward switch.
	pub fn revert(prev: PrevMode) {
		unsafe {
			let rc = ChangeDisplaySettingsExW(
				prev.device.as_ptr(),
				&prev.devmode as *const DEVMODEW,
				std::ptr::null_mut(),
				0,
				std::ptr::null(),
			);
			if rc != DISP_CHANGE_SUCCESSFUL {
				tracing::warn!(
					rc,
					w = prev.devmode.dmPelsWidth,
					h = prev.devmode.dmPelsHeight,
					"screen-adapt: revert ChangeDisplaySettingsExW failed (host left at adapted mode)"
				);
			} else {
				tracing::info!(
					w = prev.devmode.dmPelsWidth,
					h = prev.devmode.dmPelsHeight,
					"screen-adapt: reverted host display mode on teardown"
				);
			}
		}
	}
}

// ---------------------------------------------------------------------------
// Per-window fit (Windows): resize ONE host app window to a split pane's shape
// ---------------------------------------------------------------------------
/// A split pane streaming a single host window (WGC capture) gets a capture with the window's
/// own aspect. Instead of changing the monitor mode (impossible for a 3.5:1 pane, and wrong when
/// two panes share the monitor), resize that window's CLIENT area to the pane's aspect, scaled
/// to fit the monitor's work area, and restore its exact placement (incl. the maximized state)
/// afterwards. Borderless-fullscreen games may ignore the resize — the capture then simply stays
/// at the window's own size.
#[cfg(windows)]
mod win_fit {
	use windows_sys::Win32::Foundation::{HWND, RECT};
	use windows_sys::Win32::Graphics::Gdi::{
		GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
	};
	use windows_sys::Win32::UI::WindowsAndMessaging::{
		GetClientRect, GetWindowPlacement, GetWindowRect, IsWindow, SetWindowPlacement,
		SetWindowPos, ShowWindow, SWP_NOACTIVATE, SWP_NOZORDER, SW_RESTORE, SW_SHOWMAXIMIZED,
		SW_SHOWMINIMIZED, WINDOWPLACEMENT,
	};

	/// The window's placement before we resized it (`SetWindowPlacement` restores it exactly).
	pub struct WindowPrev {
		hwnd: isize,
		placement: WINDOWPLACEMENT,
	}

	/// Resize `hwnd`'s client area to the `pane_w × pane_h` aspect, as large as fits the window's
	/// monitor work area (never larger than the pane itself). Returns the original placement when
	/// a change was applied, `None` when there was nothing to do or it failed.
	pub fn fit_window(hwnd: i64, pane_w: u32, pane_h: u32) -> Option<WindowPrev> {
		if pane_w == 0 || pane_h == 0 {
			return None;
		}
		let h = hwnd as isize as HWND;
		unsafe {
			if IsWindow(h) == 0 {
				return None;
			}
			let mut placement: WINDOWPLACEMENT = std::mem::zeroed();
			placement.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
			if GetWindowPlacement(h, &mut placement) == 0 {
				return None;
			}
			// A maximized / minimized window can't take a size: restore it first.
			let sc = placement.showCmd as i32;
			if sc == SW_SHOWMAXIMIZED || sc == SW_SHOWMINIMIZED {
				ShowWindow(h, SW_RESTORE);
			}
			let mut mi: MONITORINFO = std::mem::zeroed();
			mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
			if GetMonitorInfoW(MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST), &mut mi) == 0 {
				return None;
			}
			let work = mi.rcWork;
			let mut wr: RECT = std::mem::zeroed();
			let mut cr: RECT = std::mem::zeroed();
			if GetWindowRect(h, &mut wr) == 0 || GetClientRect(h, &mut cr) == 0 {
				return None;
			}
			// Frame = outer size minus client size (title bar + borders), in physical pixels.
			let frame_w = (wr.right - wr.left) - (cr.right - cr.left);
			let frame_h = (wr.bottom - wr.top) - (cr.bottom - cr.top);
			let (cw, ch) = fit_client(
				pane_w,
				pane_h,
				((work.right - work.left) - frame_w).max(1) as u32,
				((work.bottom - work.top) - frame_h).max(1) as u32,
			);
			let (out_w, out_h) = (cw as i32 + frame_w, ch as i32 + frame_h);
			// Keep the window on its monitor: clamp the top-left so the whole window fits.
			let x = wr.left.clamp(work.left, (work.right - out_w).max(work.left));
			let y = wr.top.clamp(work.top, (work.bottom - out_h).max(work.top));
			if SetWindowPos(h, std::ptr::null_mut(), x, y, out_w, out_h, SWP_NOZORDER | SWP_NOACTIVATE) == 0 {
				tracing::warn!(hwnd, "window-fit: SetWindowPos failed");
				return None;
			}
			tracing::info!(
				hwnd,
				pane = %format!("{pane_w}x{pane_h}"),
				client = %format!("{cw}x{ch}"),
				"window-fit: resized the host window to the split pane's shape"
			);
			Some(WindowPrev { hwnd: hwnd as isize, placement })
		}
	}

	/// Put the window back exactly where it was (position, size, maximized state).
	pub fn restore_window(prev: WindowPrev) {
		unsafe {
			let h = prev.hwnd as HWND;
			if IsWindow(h) == 0 {
				return; // closed meanwhile — nothing to restore
			}
			if SetWindowPlacement(h, &prev.placement) == 0 {
				tracing::warn!(hwnd = prev.hwnd, "window-fit: restoring the window placement failed");
			} else {
				tracing::info!(hwnd = prev.hwnd, "window-fit: restored the host window");
			}
		}
	}

	/// Largest `w × h` with the pane's aspect that fits `max_w × max_h` and is no larger than the
	/// pane (even sizes — encoders want them).
	pub(super) fn fit_client(pane_w: u32, pane_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
		let scale = (max_w as f64 / pane_w as f64).min(max_h as f64 / pane_h as f64).min(1.0);
		let w = ((pane_w as f64 * scale) as u32).max(64) & !1;
		let h = ((pane_h as f64 * scale) as u32).max(64) & !1;
		(w, h)
	}

	#[cfg(test)]
	mod tests {
		use super::fit_client;
		#[test]
		fn fits_a_wide_pane_into_the_work_area() {
			// 2560x700 top/bottom pane on a 1920x1040 work area: width-bound, aspect kept.
			let (w, h) = fit_client(2560, 700, 1904, 1009);
			assert_eq!(w, 1904);
			assert!((h as i32 - 520).abs() <= 2, "{h}");
		}
		#[test]
		fn never_upscales_past_the_pane() {
			assert_eq!(fit_client(800, 600, 1904, 1009), (800, 600));
		}
	}
}

/// Placeholder off Windows (no window fit there).
#[cfg(not(windows))]
pub struct WindowPrev;
#[cfg(not(windows))]
pub fn fit_window(_hwnd: i64, _w: u32, _h: u32) -> Option<WindowPrev> {
	None
}
#[cfg(not(windows))]
pub fn restore_window(_prev: WindowPrev) {}

// ---------------------------------------------------------------------------
// Linux (X11 xrandr / Wayland compositor tools) — see display_mode/linux.rs
// ---------------------------------------------------------------------------
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{recover_stale, revert, set_best_mode, PrevMode};

// ---------------------------------------------------------------------------
// macOS (CoreGraphics, app-scoped configuration)
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
pub use mac::{revert, set_best_mode, PrevMode};

#[cfg(target_os = "macos")]
mod mac {
	use pulsar_capture::display_mode::macos;

	/// The display's original mode (a retained `CGDisplayModeRef`). The window server also puts
	/// it back by itself when Pulsar exits, since the change is app-scoped.
	pub struct PrevMode(macos::SavedMode);

	/// Switch display `display_idx` (`CGGetActiveDisplayList` order, as advertised) to
	/// the best fit for the pane; `Some` only when a change was applied.
	pub fn set_best_mode(display_idx: u32, target_w: u32, target_h: u32) -> Option<PrevMode> {
		if target_w == 0 || target_h == 0 {
			return None;
		}
		match macos::set_best_mode(display_idx, target_w, target_h) {
			Ok(Some(saved)) => {
				let (w, h) = saved.size();
				tracing::info!(
					display_idx,
					target_w,
					target_h,
					from = format!("{w}x{h}"),
					"screen-adapt: switched host display mode for split pane"
				);
				Some(PrevMode(saved))
			}
			Ok(None) => None,
			Err(e) => {
				tracing::warn!(display_idx, target_w, target_h, "screen-adapt: {e}; leaving mode unchanged");
				None
			}
		}
	}

	/// Restore the original mode (best-effort; logs on failure).
	pub fn revert(prev: PrevMode) {
		let (w, h) = prev.0.size();
		match macos::restore(prev.0) {
			Ok(()) => tracing::info!(w, h, "screen-adapt: reverted host display mode"),
			Err(e) => tracing::warn!(w, h, "screen-adapt: revert failed (host left at adapted mode): {e}"),
		}
	}
}

// ---------------------------------------------------------------------------
// Other targets: no-op stubs — keep handlers.rs / host.rs compiling everywhere.
// ---------------------------------------------------------------------------
/// Placeholder so callers can name the type on every platform.
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub struct PrevMode;

/// No-op where no display-mode API is wired.
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub fn set_best_mode(_display_idx: u32, _target_w: u32, _target_h: u32) -> Option<PrevMode> {
	None
}

/// No-op revert (there is never a stored `PrevMode` to restore).
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub fn revert(_prev: PrevMode) {}
