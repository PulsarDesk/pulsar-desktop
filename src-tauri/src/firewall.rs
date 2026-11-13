//! Windows: one firewall decision instead of a prompt per program.
//!
//! Windows Defender Firewall asks "allow access?" for EVERY executable that opens a listening
//! socket — on a fresh install that was a stack of prompts: pulsar.exe, the video renderer
//! (pulsar-render.exe receives RTP), ffmpeg.exe (the host's fallback encoder sends RTP from a
//! wildcard socket), ffplay.exe. Pulsar adds one inbound allow rule per program it ships,
//! scoped to that program's path and named "Pulsar (<file>)" (visible / removable in the
//! Windows Firewall UI — the same thing Steam, Parsec, Sunshine do):
//!
//! * the installer (pulsar-setup) runs `pulsar.exe --firewall-setup` through ONE UAC prompt
//!   when the rules are missing; the uninstaller runs `--firewall-remove`;
//! * an elevated Pulsar (the default: it relaunches elevated) re-checks them at startup — its
//!   own before it goes online, the bundled ones in the background — so a moved install heals.
//!   Rules are per path: each Windows user's per-user install keeps its own.
//!
//! Not elevated and no installer run: nothing — Windows asks as before.
#![cfg(windows)]

use std::path::{Path, PathBuf};

/// Rule names carry the program name; one rule per program, matched on its path.
fn rule_name(exe: &str) -> String {
	let file = Path::new(exe)
		.file_name()
		.map(|f| f.to_string_lossy().into_owned())
		.unwrap_or_else(|| exe.to_string());
	format!("Pulsar ({file})")
}

fn netsh(args: &[&str]) -> Option<String> {
	let mut c = std::process::Command::new("netsh");
	c.args(args);
	crate::process::no_window(&mut c);
	let out = c.output().ok()?;
	out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// netsh rejects the verbatim `\\?\` form resource paths come in.
fn plain(exe: &str) -> &str {
	exe.strip_prefix(r"\\?\").unwrap_or(exe)
}

/// Make sure an inbound allow rule exists for `exe` (any profile). Rules of the same name for
/// other paths are left alone: on a shared PC every Windows user has their own per-user install
/// (same file names, different folders) and each needs its own rule.
fn ensure(exe: &str) {
	let exe = plain(exe);
	if !Path::new(exe).is_file() {
		return;
	}
	let name = rule_name(exe);
	if let Some(shown) = netsh(&["advfirewall", "firewall", "show", "rule", &format!("name={name}"), "verbose"]) {
		if shown.to_lowercase().contains(&exe.to_lowercase()) {
			return; // already allowed at this path
		}
	}
	let ok = netsh(&[
		"advfirewall",
		"firewall",
		"add",
		"rule",
		&format!("name={name}"),
		"dir=in",
		"action=allow",
		&format!("program={exe}"),
		"enable=yes",
		"profile=any",
	])
	.is_some();
	tracing::info!(%name, %exe, ok, "firewall: inbound allow rule");
}

/// The programs a Pulsar install ships next to `exe` (resources\ or the exe's own folder).
fn bundled_near(exe: &Path) -> Vec<PathBuf> {
	let Some(dir) = exe.parent() else { return Vec::new() };
	let mut out = Vec::new();
	for base in ["pulsar-render", "ffmpeg", "ffplay"] {
		let file = format!("{base}.exe");
		for cand in [dir.join("resources").join(&file), dir.join(&file)] {
			if cand.is_file() {
				out.push(cand);
				break;
			}
		}
	}
	out
}

/// At startup (before going online), elevated only: the app's own rule now, the bundled
/// programs' in the background.
pub(crate) fn setup(app: &tauri::AppHandle) {
	if !crate::shared_mode::is_elevated() {
		return;
	}
	if let Ok(me) = std::env::current_exe() {
		ensure(&me.to_string_lossy());
	}
	let bundled: Vec<String> = ["pulsar-render", "ffmpeg", "ffplay"]
		.iter()
		.map(|b| crate::process::bundled_bin(app, b))
		.collect();
	std::thread::spawn(move || {
		for exe in bundled {
			ensure(&exe);
		}
	});
}

/// `pulsar --firewall-setup` / `--firewall-remove` (the installer runs it elevated): add or
/// delete the rules for this exe and the programs next to it. Exit code 0 = done.
pub(crate) fn cli(add: bool) -> i32 {
	let Ok(me) = std::env::current_exe() else { return 1 };
	let mut all = vec![me.clone()];
	all.extend(bundled_near(&me));
	for exe in all {
		let exe = exe.to_string_lossy().into_owned();
		if add {
			ensure(&exe);
		} else {
			// Only this install's rule (matched on its path), not another user's.
			let exe = plain(&exe);
			let _ = netsh(&[
				"advfirewall",
				"firewall",
				"delete",
				"rule",
				&format!("name={}", rule_name(exe)),
				&format!("program={exe}"),
			]);
		}
	}
	0
}
