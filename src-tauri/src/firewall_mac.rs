//! macOS: one Application Firewall decision instead of a prompt per program.
//!
//! The macOS Application Firewall (ALF; System Settings → Network → Firewall) is OFF by
//! default. When a user turns it on, every program that receives unsolicited inbound traffic
//! gets its own "Do you want the application … to accept incoming network connections?"
//! dialog — Pulsar, the renderer (pulsar-render receives the video) and the fallback
//! ffmpeg/ffplay. Pulsar is not Developer-ID signed, so "automatically allow signed software"
//! does not cover it, and a new build (new ad-hoc signature) can ask again.
//!
//! The Windows analog (firewall.rs) adds program-scoped allow rules from the installer. On
//! macOS a `.app` has no installer step, so the app does it itself, once per version: at
//! startup, only when ALF is ON and one of its programs is not yet permitted, it asks for the
//! admin password ONCE (`osascript … with administrator privileges`) and runs
//! `socketfilterfw --add <path>` + `--unblockapp <path>` for each. A cancelled prompt is not
//! repeated for that version (a marker file next to the config). Firewall off → nothing.
//!
//! The parsing/script building is pure and unit-tested on every OS.
//! Untested on macOS hardware (written 2026-11-14).

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

const SFW: &str = "/usr/libexec/ApplicationFirewall/socketfilterfw";

/// `socketfilterfw --getglobalstate` → "Firewall is enabled. (State = 1)" / "(State = 2)"
/// (block all) / "Firewall is disabled. (State = 0)".
pub(crate) fn global_enabled(out: &str) -> bool {
	let o = out.to_ascii_lowercase();
	o.contains("enabled") && !o.contains("disabled")
}

/// `socketfilterfw --getappblocked <path>` → "The application … is permitted" when allowed;
/// "… is blocked" or "… is not part of the firewall" otherwise (which would prompt).
pub(crate) fn needs_rule(out: &str) -> bool {
	!out.to_ascii_lowercase().contains("permitted")
}

/// POSIX single-quote a path for `sh -c`.
fn sh_quote(s: &str) -> String {
	format!("'{}'", s.replace('\'', r"'\''"))
}

/// AppleScript string literal body (backslash and double quote escaped).
fn as_escape(s: &str) -> String {
	s.replace('\\', r"\\").replace('"', "\\\"")
}

/// The `osascript -e` program that adds + unblocks every path behind one admin prompt.
pub(crate) fn admin_script(paths: &[String], prompt: &str) -> String {
	let sfw = sh_quote(SFW);
	let shell: Vec<String> = paths
		.iter()
		.map(|p| {
			let p = sh_quote(p);
			format!("{sfw} --add {p}; {sfw} --unblockapp {p}")
		})
		.collect();
	format!(
		"do shell script \"{}\" with administrator privileges with prompt \"{}\"",
		as_escape(&shell.join("; ")),
		as_escape(prompt)
	)
}

#[cfg(target_os = "macos")]
fn sfw(args: &[&str]) -> Option<String> {
	let out = std::process::Command::new(SFW).args(args).output().ok()?;
	Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Startup (background thread): permit Pulsar's programs in an ENABLED Application Firewall.
#[cfg(target_os = "macos")]
pub(crate) fn setup(app: &tauri::AppHandle) {
	use tauri::Manager;
	let mut paths: Vec<String> = Vec::new();
	if let Ok(me) = std::env::current_exe() {
		paths.push(me.to_string_lossy().into_owned());
	}
	for b in ["pulsar-render", "ffmpeg", "ffplay"] {
		paths.push(crate::process::bundled_bin(app, b));
	}
	let version = app.package_info().version.to_string();
	let marker = crate::util::config_path(app).with_file_name(format!(".firewall-asked-{version}"));
	std::thread::spawn(move || {
		if !sfw(&["--getglobalstate"]).is_some_and(|o| global_enabled(&o)) {
			return; // ALF off (the default): nothing asks, nothing to do.
		}
		let missing: Vec<String> = paths
			.into_iter()
			.filter(|p| std::path::Path::new(p).is_file())
			.filter(|p| sfw(&["--getappblocked", p.as_str()]).is_none_or(|o| needs_rule(&o)))
			.collect();
		if missing.is_empty() || marker.exists() {
			return;
		}
		// Ask once per version, whatever the answer.
		let _ = std::fs::write(&marker, b"");
		let script = admin_script(&missing, crate::i18n::t("fw.macPrompt"));
		let ok = std::process::Command::new("osascript")
			.args(["-e", &script])
			.status()
			.is_ok_and(|s| s.success());
		tracing::info!(ok, programs = ?missing, "firewall (macOS ALF): allow incoming");
	});
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn reads_the_global_state() {
		assert!(global_enabled("Firewall is enabled. (State = 1)"));
		assert!(global_enabled("Firewall is enabled. (State = 2)"));
		assert!(!global_enabled("Firewall is disabled. (State = 0)"));
	}

	#[test]
	fn only_permitted_apps_are_left_alone() {
		assert!(!needs_rule("The application /Applications/Pulsar.app/Contents/MacOS/pulsar is permitted"));
		assert!(needs_rule("The application /x is blocked"));
		assert!(needs_rule("The application /x is not part of the firewall"));
	}

	#[test]
	fn one_quoted_admin_script() {
		let s = admin_script(&["/Applications/Pulsar.app/Contents/MacOS/pulsar".into(), "/tmp/it's".into()], "Pulsar \"x\"");
		assert!(s.starts_with("do shell script \""));
		assert!(s.ends_with("with administrator privileges with prompt \"Pulsar \\\"x\\\"\""));
		assert_eq!(s.matches("--add").count(), 2);
		assert_eq!(s.matches("--unblockapp").count(), 2);
		// The shell quote of an apostrophe survives the AppleScript layer unchanged.
		assert!(s.contains(r"'/tmp/it'\\''s'"));
	}
}
