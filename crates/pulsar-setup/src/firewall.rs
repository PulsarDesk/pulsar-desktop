//! Windows Firewall rules for the installed programs, through ONE UAC prompt.
//!
//! The app itself does the work (`pulsar.exe --firewall-setup` / `--firewall-remove`, see the
//! app's firewall.rs): one inbound allow rule per shipped program (pulsar, pulsar-render,
//! ffmpeg, ffplay), named "Pulsar (<file>)". The installer only checks whether they are already
//! there (reading rules needs no elevation) and runs the app elevated when they are not, so a
//! fresh install asks once instead of Windows asking once per program on first use.
//! Never fatal: declined → Windows asks per program as before.

use std::path::Path;

/// True when the app's own rule exists and points at `dir\pulsar.exe`.
pub fn rules_present(dir: &Path) -> bool {
	#[cfg(windows)]
	{
		use std::os::windows::process::CommandExt;
		const CREATE_NO_WINDOW: u32 = 0x0800_0000;
		let exe = dir.join("pulsar.exe").display().to_string().to_lowercase();
		let Ok(out) = std::process::Command::new("netsh")
			.args(["advfirewall", "firewall", "show", "rule", "name=Pulsar (pulsar.exe)", "verbose"])
			.creation_flags(CREATE_NO_WINDOW)
			.output()
		else {
			return false;
		};
		out.status.success() && String::from_utf8_lossy(&out.stdout).to_lowercase().contains(&exe)
	}
	#[cfg(not(windows))]
	{
		let _ = dir;
		true
	}
}

/// Run `dir\pulsar.exe <flag>` elevated (UAC) and wait. `Err` = declined / failed.
pub fn run_elevated(dir: &Path, flag: &str) -> Result<(), String> {
	#[cfg(windows)]
	{
		use std::os::windows::process::CommandExt;
		const CREATE_NO_WINDOW: u32 = 0x0800_0000;
		let exe = dir.join("pulsar.exe");
		if !exe.is_file() {
			return Err("pulsar.exe yok".into());
		}
		let script = format!(
			"try {{ $p = Start-Process -Wait -PassThru -WindowStyle Hidden -Verb RunAs -FilePath '{}' -ArgumentList '{}' }} catch {{ exit 1223 }}; exit $p.ExitCode",
			exe.display().to_string().replace('\'', "''"),
			flag
		);
		let status = std::process::Command::new("powershell")
			.args(["-NoProfile", "-NonInteractive", "-Command", &script])
			.creation_flags(CREATE_NO_WINDOW)
			.status()
			.map_err(|e| e.to_string())?;
		match status.code() {
			Some(0) => Ok(()),
			Some(1223) => Err("yönetici izni verilmedi".into()),
			Some(c) => Err(format!("çıkış kodu {c}")),
			None => Err("bilinmeyen hata".into()),
		}
	}
	#[cfg(not(windows))]
	{
		let _ = (dir, flag);
		Ok(())
	}
}
