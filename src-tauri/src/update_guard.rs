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
	// Reopened from the update window after a failed / declined install: the marker is
	// stale by definition.
	if std::env::args().any(|a| a == "--update-failed") {
		let _ = std::fs::remove_file(&path);
		return false;
	}
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

/// The version a running in-app update is installing (from the marker), if any.
pub(crate) fn pending_version() -> Option<String> {
	let text = std::fs::read_to_string(marker_path()?).ok()?;
	let v = norm(text.lines().next()?).to_string();
	(!v.is_empty()).then_some(v)
}

/// Windows: the updater runs the NSIS installer QUIETLY (no wizard window) and exits this
/// process. Right before that, open the branded "Pulsar is updating" window
/// (`resources/pulsar-update.exe` = `pulsar-setup --updating`): it shows progress while the
/// installer works, closes when the new build starts (the installer's /R relaunch), and
/// offers to reopen Pulsar if the install never ran (UAC declined) or failed. It is copied
/// to %TEMP% first because the installer overwrites the install dir. Best-effort: without
/// the helper the update still installs, just without the window. Called by the frontend
/// right before `install()` (the updater plugin owns its own on-before-exit hook).
#[tauri::command]
pub(crate) fn launch_update_window() {
	#[cfg(windows)]
	{
		use std::os::windows::process::CommandExt;
		const DETACHED_PROCESS: u32 = 0x0000_0008;
		let Some(src) = std::env::current_exe()
			.ok()
			.and_then(|e| e.parent().map(|d| d.join("resources").join("pulsar-update.exe")))
			.filter(|p| p.is_file())
		else {
			tracing::warn!("update window: resources/pulsar-update.exe missing — installing without it");
			return;
		};
		let dst = std::env::temp_dir().join("pulsar-update.exe");
		if let Err(e) = std::fs::copy(&src, &dst) {
			tracing::warn!("update window: copy to temp failed: {e}");
			return;
		}
		let mut cmd = std::process::Command::new(&dst);
		cmd.arg("--updating")
			.arg(pending_version().unwrap_or_default())
			.arg("--old-pid")
			.arg(std::process::id().to_string());
		if let Ok(exe) = std::env::current_exe() {
			cmd.arg("--app").arg(exe);
		}
		match cmd.creation_flags(DETACHED_PROCESS).spawn() {
			Ok(_) => tracing::info!("update window started"),
			Err(e) => tracing::warn!("update window: spawn failed: {e}"),
		}
	}
}

/// Frontend: the install did not happen (error / deferred) — unblock launches.
#[tauri::command]
pub(crate) fn clear_update_installing() {
	if let Some(path) = marker_path() {
		let _ = std::fs::remove_file(path);
	}
}
