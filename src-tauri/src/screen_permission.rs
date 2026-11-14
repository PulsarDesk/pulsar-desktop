//! macOS Screen Recording permission (TCC) for the host. ScreenCaptureKit — the native video
//! capture and the system-audio capture — needs it; without it the ffmpeg fallback sees only
//! the wallpaper and the menu bar. The home screen asks for it and points at System Settings
//! (`ScreenPermissionNotice.svelte`). Other systems have no such permission: "na".
//!
//! Untested on macOS hardware.

/// `"granted"`, `"denied"` (macOS without the grant) or `"na"` (no such permission here). With
/// `request`, macOS shows its permission dialog (once per app; later calls only answer). A grant
/// takes effect after Pulsar is restarted.
#[tauri::command]
pub(crate) fn screen_permission(request: bool) -> &'static str {
	#[cfg(target_os = "macos")]
	{
		if pulsar_core::macos_capture::screen_capture_access(request) {
			"granted"
		} else {
			"denied"
		}
	}
	#[cfg(not(target_os = "macos"))]
	{
		let _ = request;
		"na"
	}
}

/// Open System Settings → Privacy & Security → Screen (& System Audio) Recording.
#[tauri::command]
pub(crate) fn open_screen_permission_settings() -> Result<(), String> {
	#[cfg(target_os = "macos")]
	{
		std::process::Command::new("open")
			.arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
			.spawn()
			.map(|_| ())
			.map_err(|e| e.to_string())
	}
	#[cfg(not(target_os = "macos"))]
	{
		Err("only macOS has a screen recording permission".into())
	}
}
