//! "An update is being installed" marker. On Windows the updater hands off to the NSIS
//! installer and the app exits; for the few seconds until the installer relaunches the
//! NEW build there is no Pulsar on screen, and a user who starts Pulsar in that gap used
//! to launch the OLD exe — which then held the file open, so the install could not
//! replace it and the machine stayed on the old version.
//!
//! The frontend writes the marker (target version + timestamp) right before
//! `install()`. At startup — before anything else touches the install dir — a build
//! whose version is NOT the target sees a fresh marker, tells the user an update is in
//! progress and exits. The new build (version == target) deletes the marker and starts
//! normally. A stale marker (failed / cancelled install) is ignored after [`TTL`].

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long a marker blocks launches. Generous for a slow UAC prompt + driver installs,
/// short enough that a failed install never locks the user out for long.
const TTL: Duration = Duration::from_secs(180);

fn marker_path() -> Option<PathBuf> {
	crate::logging::log_dir()
		.and_then(|d| d.parent().map(|p| p.to_path_buf()))
		.map(|d| d.join("update-installing"))
}

fn norm(v: &str) -> &str {
	v.trim().trim_start_matches('v')
}

fn now_secs() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|d| d.as_secs())
		.unwrap_or(0)
}

/// Startup check. Returns `true` when this process must exit because an update to a
/// different version is being installed right now.
pub(crate) fn blocked_by_update() -> bool {
	let Some(path) = marker_path() else {
		return false;
	};
	let Ok(text) = std::fs::read_to_string(&path) else {
		return false;
	};
	let mut lines = text.lines();
	let target = norm(lines.next().unwrap_or(""));
	let at: u64 = lines.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
	let fresh = now_secs().saturating_sub(at) < TTL.as_secs();
	if target == norm(env!("CARGO_PKG_VERSION")) || !fresh {
		// We ARE the new build (or the marker is left over from a failed install).
		let _ = std::fs::remove_file(&path);
		return false;
	}
	true
}

/// Tell the user why nothing opened. Windows only — elsewhere `install()` replaces the
/// binary in place and relaunches at once, so there is no visible gap.
pub(crate) fn notify_blocked() {
	#[cfg(windows)]
	{
		use windows_sys::Win32::UI::WindowsAndMessaging::{
			MessageBoxW, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
		};
		let w = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
		let title = w("Pulsar");
		let body = w(crate::i18n::t("update.inProgress"));
		unsafe {
			MessageBoxW(
				std::ptr::null_mut(),
				body.as_ptr(),
				title.as_ptr(),
				MB_OK | MB_ICONINFORMATION | MB_TOPMOST | MB_SETFOREGROUND,
			);
		}
	}
}

/// Frontend: an install of `version` starts now (written right before `install()`).
#[tauri::command]
pub(crate) fn mark_update_installing(version: String) -> Result<(), String> {
	let path = marker_path().ok_or("no app data dir")?;
	if let Some(dir) = path.parent() {
		std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
	}
	std::fs::write(&path, format!("{}\n{}\n", norm(&version), now_secs())).map_err(|e| e.to_string())
}

/// Frontend: the install did not happen (error / deferred) — unblock launches.
#[tauri::command]
pub(crate) fn clear_update_installing() {
	if let Some(path) = marker_path() {
		let _ = std::fs::remove_file(path);
	}
}
