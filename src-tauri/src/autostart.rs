//! Settings → General → "Start with the computer" (+ "Start in the background").
//!
//! * Windows: a logon task for this user (`Pulsar autostart (<user>)`, highest privileges) when
//!   Pulsar runs elevated — Pulsar relaunches itself elevated by default, so a plain Run-key
//!   start would ask UAC at every sign-in. Not elevated: the per-user Run key instead.
//! * Linux: `~/.config/autostart/pulsar.desktop` (XDG autostart).
//! * macOS: `~/Library/LaunchAgents/com.pulsardesk.pulsar.plist` (RunAtLoad).
//!
//! "In the background" adds `--background`: the app starts to the tray with no window
//! (`splash::background`).

use std::path::PathBuf;

/// The command line flag for a background start (no window; the tray brings it up).
pub(crate) const BACKGROUND_FLAG: &str = "--background";

fn exe() -> Result<PathBuf, String> {
	// An AppImage runs from a temporary mount: start the image itself.
	#[cfg(target_os = "linux")]
	if let Some(img) = std::env::var_os("APPIMAGE").filter(|v| !v.is_empty()) {
		return Ok(PathBuf::from(img));
	}
	std::env::current_exe().map_err(|e| e.to_string())
}

#[cfg(windows)]
fn task_name() -> String {
	let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
	format!("Pulsar autostart ({user})")
}

#[cfg(windows)]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(windows)]
const RUN_VALUE: &str = "Pulsar";

#[cfg(windows)]
fn run(cmd: &str, args: &[&str]) -> bool {
	let mut c = std::process::Command::new(cmd);
	c.args(args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
	crate::process::no_window(&mut c);
	c.status().map(|s| s.success()).unwrap_or(false)
}

/// Turn starting with the computer on/off; `background` = start to the tray.
#[tauri::command]
pub(crate) fn set_autostart(enabled: bool, background: bool) -> Result<(), String> {
	let exe = exe()?;
	let exe_s = exe.to_string_lossy().into_owned();
	tracing::info!(enabled, background, exe = %exe_s, "autostart");
	#[cfg(windows)]
	{
		// Remove both forms first: switching elevation or the background flag rewrites it.
		let _ = run("schtasks", &["/Delete", "/TN", &task_name(), "/F"]);
		let _ = run("reg", &["delete", RUN_KEY, "/v", RUN_VALUE, "/f"]);
		if !enabled {
			return Ok(());
		}
		let line = if background {
			format!("\"{exe_s}\" {BACKGROUND_FLAG}")
		} else {
			format!("\"{exe_s}\"")
		};
		if crate::shared_mode::is_elevated() {
			let user = format!(
				"{}\\{}",
				std::env::var("USERDOMAIN").unwrap_or_default(),
				std::env::var("USERNAME").unwrap_or_default()
			);
			if run(
				"schtasks",
				&["/Create", "/TN", &task_name(), "/TR", &line, "/SC", "ONLOGON", "/RU", &user, "/IT", "/RL", "HIGHEST", "/F"],
			) {
				return Ok(());
			}
			tracing::warn!("autostart: logon task not created — using the Run key");
		}
		if run("reg", &["add", RUN_KEY, "/v", RUN_VALUE, "/t", "REG_SZ", "/d", &line, "/f"]) {
			Ok(())
		} else {
			Err("autostart could not be registered".into())
		}
	}
	#[cfg(target_os = "linux")]
	{
		let dir = dirs_home()?.join(".config/autostart");
		let file = dir.join("pulsar.desktop");
		if !enabled {
			let _ = std::fs::remove_file(&file);
			return Ok(());
		}
		std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
		let exec = if background {
			format!("\"{exe_s}\" {BACKGROUND_FLAG}")
		} else {
			format!("\"{exe_s}\"")
		};
		let body = format!(
			"[Desktop Entry]\nType=Application\nName=Pulsar\nExec={exec}\nIcon=pulsar\nX-GNOME-Autostart-enabled=true\nTerminal=false\n"
		);
		std::fs::write(&file, body).map_err(|e| e.to_string())
	}
	#[cfg(target_os = "macos")]
	{
		let dir = dirs_home()?.join("Library/LaunchAgents");
		let file = dir.join("com.pulsardesk.pulsar.plist");
		if !enabled {
			let _ = std::fs::remove_file(&file);
			return Ok(());
		}
		std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
		let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
		let mut args = format!("\t\t<string>{}</string>\n", esc(&exe_s));
		if background {
			args.push_str(&format!("\t\t<string>{BACKGROUND_FLAG}</string>\n"));
		}
		let body = format!(
			"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n\t<key>Label</key>\n\t<string>com.pulsardesk.pulsar</string>\n\t<key>ProgramArguments</key>\n\t<array>\n{args}\t</array>\n\t<key>RunAtLoad</key>\n\t<true/>\n</dict>\n</plist>\n"
		);
		std::fs::write(&file, body).map_err(|e| e.to_string())
	}
}

/// Is starting with the computer registered (by these settings)? `[enabled, background]`.
#[tauri::command]
pub(crate) fn autostart_status() -> (bool, bool) {
	#[cfg(windows)]
	{
		let mut c = std::process::Command::new("schtasks");
		c.args(["/Query", "/TN", &task_name(), "/XML"]);
		crate::process::no_window(&mut c);
		if let Ok(out) = c.output() {
			if out.status.success() {
				let xml = String::from_utf8_lossy(&out.stdout);
				return (true, xml.contains(BACKGROUND_FLAG));
			}
		}
		let mut c = std::process::Command::new("reg");
		c.args(["query", RUN_KEY, "/v", RUN_VALUE]);
		crate::process::no_window(&mut c);
		match c.output() {
			Ok(out) if out.status.success() => (true, String::from_utf8_lossy(&out.stdout).contains(BACKGROUND_FLAG)),
			_ => (false, false),
		}
	}
	#[cfg(target_os = "linux")]
	{
		let file = dirs_home().map(|h| h.join(".config/autostart/pulsar.desktop"));
		match file.ok().and_then(|f| std::fs::read_to_string(f).ok()) {
			Some(s) => (true, s.contains(BACKGROUND_FLAG)),
			None => (false, false),
		}
	}
	#[cfg(target_os = "macos")]
	{
		let file = dirs_home().map(|h| h.join("Library/LaunchAgents/com.pulsardesk.pulsar.plist"));
		match file.ok().and_then(|f| std::fs::read_to_string(f).ok()) {
			Some(s) => (true, s.contains(BACKGROUND_FLAG)),
			None => (false, false),
		}
	}
}

#[cfg(not(windows))]
fn dirs_home() -> Result<PathBuf, String> {
	std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| "no HOME".to_string())
}

/// This launch is a background start (`--background`).
pub(crate) fn is_background_launch() -> bool {
	std::env::args().any(|a| a == BACKGROUND_FLAG)
}
