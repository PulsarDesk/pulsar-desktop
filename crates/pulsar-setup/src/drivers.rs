//! Kernel drivers the per-user install sets up: today only **usbip-win2** (the virtual
//! wired DualSense a Pulsar host presents, `pulsar_core::input::ds5::attach`).
//!
//! The app itself installs per-user without UAC, but a kernel driver needs admin, so the
//! usbip-win2 installer runs through its own UAC prompt (`Start-Process -Verb RunAs`).
//! It only runs when usbip-win2 is missing or older than the bundled one, and only from the
//! interactive install: the headless `/S` path never prompts. A declined prompt or a failed
//! install never fails the Pulsar install; the host then simply does not offer the virtual
//! DualSense (FEAT_DS5) until usbip-win2 is installed.
//!
//! Pulsar's uninstall leaves usbip-win2 installed, like the NSIS installer leaves ViGEmBus
//! and the other drivers (other software may use them; it has its own uninstaller).

use std::path::{Path, PathBuf};

/// The bundled usbip-win2 version. Bump together with `scripts/fetch-drivers.mjs`
/// (`USBIP_VERSION` + sha256) and `src-tauri/installer-hooks.nsh` (`USBIP_VERSION`).
pub const USBIP_VERSION: &str = "0.9.8.1";

/// The Inno Setup uninstall key usbip-win2 writes (AppId `{199505b0-…}` + `_is1`), 64-bit view.
#[cfg(windows)]
const USBIP_UNINSTALL_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{199505b0-b93d-4521-a8c7-897818e0205a}_is1";

/// The bundled usbip-win2 installer in an extracted install dir (`resources/usbip/USBip-<arch>.exe`,
/// one file for the payload's arch), if the payload carries one.
pub fn bundled_usbip(dir: &Path) -> Option<PathBuf> {
	let arch = if cfg!(target_arch = "aarch64") {
		"arm64"
	} else {
		"x64"
	};
	let p = dir
		.join("resources")
		.join("usbip")
		.join(format!("USBip-{arch}.exe"));
	p.is_file().then_some(p)
}

/// Whether usbip-win2 must be (re)installed: no `usbip.exe`, or an installed version older than
/// [`USBIP_VERSION`]. An `usbip.exe` without the uninstall key (a manual / source install) is
/// left alone.
pub fn usbip_needed() -> bool {
	let installed = installed_usbip_version();
	let exe_present = std::env::var_os("ProgramFiles")
		.map(|pf| Path::new(&pf).join("USBip").join("usbip.exe").is_file())
		.unwrap_or(false);
	needs_install(installed.as_deref(), exe_present, USBIP_VERSION)
}

/// The decision behind [`usbip_needed`], pure for tests.
fn needs_install(installed: Option<&str>, exe_present: bool, bundled: &str) -> bool {
	match installed {
		Some(v) => version_older(v, bundled),
		None => !exe_present,
	}
}

/// `a < b` for dotted numeric versions ("0.9.7" < "0.9.8.1"); missing parts count as 0 and a
/// non-numeric part as 0, so a garbled registry value means "older" (reinstall over it).
fn version_older(a: &str, b: &str) -> bool {
	let parse = |s: &str| -> Vec<u64> {
		s.trim()
			.split('.')
			.map(|p| p.trim().parse().unwrap_or(0))
			.collect()
	};
	let (a, b) = (parse(a), parse(b));
	for i in 0..a.len().max(b.len()) {
		let (x, y) = (
			a.get(i).copied().unwrap_or(0),
			b.get(i).copied().unwrap_or(0),
		);
		if x != y {
			return x < y;
		}
	}
	false
}

/// DisplayVersion of the installed usbip-win2 (HKLM, 64-bit registry view), if any.
fn installed_usbip_version() -> Option<String> {
	#[cfg(windows)]
	unsafe {
		use windows_sys::Win32::System::Registry::*;
		let wide = |s: &str| {
			s.encode_utf16()
				.chain(std::iter::once(0))
				.collect::<Vec<u16>>()
		};
		let mut h: HKEY = std::ptr::null_mut();
		if RegOpenKeyExW(
			HKEY_LOCAL_MACHINE,
			wide(USBIP_UNINSTALL_KEY).as_ptr(),
			0,
			KEY_READ | KEY_WOW64_64KEY,
			&mut h,
		) != 0
		{
			return None;
		}
		let mut buf = [0u16; 256];
		let mut len = (buf.len() * 2) as u32;
		let mut ty = 0u32;
		let ok = RegQueryValueExW(
			h,
			wide("DisplayVersion").as_ptr(),
			std::ptr::null_mut(),
			&mut ty,
			buf.as_mut_ptr() as *mut u8,
			&mut len,
		) == 0;
		RegCloseKey(h);
		if !ok || ty != REG_SZ {
			return None;
		}
		let n = (len as usize / 2).saturating_sub(1);
		let s = String::from_utf16_lossy(&buf[..n]);
		(!s.is_empty()).then_some(s)
	}
	#[cfg(not(windows))]
	{
		None
	}
}

/// Run the usbip-win2 Inno Setup installer silently through a UAC prompt and wait for it.
/// Blocking — run it on a worker thread. `Err` carries a short reason (declined / exit code).
pub fn install_usbip(installer: &Path) -> Result<(), String> {
	#[cfg(windows)]
	{
		use std::os::windows::process::CommandExt;
		const CREATE_NO_WINDOW: u32 = 0x0800_0000;
		// Start-Process throws when the UAC prompt is declined → exit 1223 (ERROR_CANCELLED).
		let script = format!(
			"try {{ $p = Start-Process -Wait -PassThru -Verb RunAs -FilePath '{}' -ArgumentList '/VERYSILENT','/SUPPRESSMSGBOXES','/NORESTART','/SP-' }} catch {{ exit 1223 }}; exit $p.ExitCode",
			installer.display().to_string().replace('\'', "''")
		);
		let status = std::process::Command::new("powershell")
			.args(["-NoProfile", "-NonInteractive", "-Command", &script])
			.creation_flags(CREATE_NO_WINDOW)
			.status()
			.map_err(|e| e.to_string())?;
		match status.code() {
			Some(0) => Ok(()),
			Some(1223) => Err("yönetici izni verilmedi".to_string()),
			Some(c) => Err(format!("çıkış kodu {c}")),
			None => Err("bilinmeyen hata".to_string()),
		}
	}
	#[cfg(not(windows))]
	{
		let _ = installer;
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn version_order() {
		assert!(version_older("0.9.7", "0.9.8.1"));
		assert!(version_older("0.9.8", "0.9.8.1"));
		assert!(!version_older("0.9.8.1", "0.9.8.1"));
		assert!(!version_older("0.9.8.1", "0.9.8"));
		assert!(!version_older("0.10.0", "0.9.8.1"));
		assert!(version_older("garbage", "0.9.8.1"));
	}

	#[test]
	fn install_decision() {
		assert!(needs_install(None, false, "0.9.8.1"));
		assert!(
			!needs_install(None, true, "0.9.8.1"),
			"manual install without the uninstall key is kept"
		);
		assert!(needs_install(Some("0.9.7.0"), true, "0.9.8.1"));
		assert!(!needs_install(Some("0.9.8.1"), true, "0.9.8.1"));
		assert!(!needs_install(Some("1.0"), true, "0.9.8.1"));
	}
}
