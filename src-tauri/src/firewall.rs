//! Windows: one firewall decision instead of a prompt per program.
//!
//! Windows Defender Firewall asks "allow access?" for EVERY executable that opens a listening
//! socket — on a fresh install that was a stack of prompts: pulsar.exe, the video renderer
//! (pulsar-render.exe receives RTP), ffmpeg.exe (the host's fallback encoder sends RTP from a
//! wildcard socket), ffplay.exe. Pulsar runs elevated by default, so at startup it adds an
//! inbound allow rule for itself and the programs it ships (once; re-added when the install
//! moved). pulsar.exe's rule goes in BEFORE it goes online (its first socket would otherwise
//! raise the prompt); the bundled ones in the background. Not elevated: nothing — Windows asks
//! as before.
#![cfg(windows)]

use tauri::AppHandle;

/// Rule names carry the program name; one rule per program, matched on its path.
fn rule_name(exe: &str) -> String {
	let file = std::path::Path::new(exe)
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

/// Make sure an inbound allow rule exists for `exe` (any profile). Existing rule for another
/// path (the app moved / reinstalled elsewhere) is replaced.
fn ensure(exe: &str) {
	// netsh rejects the verbatim `\\?\` form resource paths come in.
	let exe = exe.strip_prefix(r"\\?\").unwrap_or(exe);
	if !std::path::Path::new(exe).is_file() {
		return;
	}
	let name = rule_name(exe);
	if let Some(shown) = netsh(&["advfirewall", "firewall", "show", "rule", &format!("name={name}"), "verbose"]) {
		if shown.to_lowercase().contains(&exe.to_lowercase()) {
			return; // already allowed at this path
		}
		let _ = netsh(&["advfirewall", "firewall", "delete", "rule", &format!("name={name}")]);
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

/// At startup (before going online): the app's own rule now, the bundled programs' in the
/// background.
pub(crate) fn setup(app: &AppHandle) {
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
