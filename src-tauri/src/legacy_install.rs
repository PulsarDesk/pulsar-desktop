//! One-time cleanup of a per-user `pulsar-setup` install after the in-app updater moved
//! this machine to the per-machine NSIS install.
//!
//! The branded bootstrapper (`crates/pulsar-setup`) installs to
//! `%LOCALAPPDATA%\Programs\Pulsar` with its own Start Menu / Desktop shortcuts and an HKCU
//! uninstall entry. The updater's install vehicle is the NSIS installer, which installs
//! per-machine to `Program Files\Pulsar` and — in update mode — creates no shortcuts. So
//! after the first in-app update the new build ran once (the installer relaunches it), but
//! every later launch from the shortcuts started the OLD per-user copy again.
//!
//! When this build runs from somewhere else than the registered per-user install, it points
//! those shortcuts at itself and removes the stale copy + its uninstall entry.

#[cfg(windows)]
pub(crate) fn migrate_in_background() {
	std::thread::spawn(|| {
		if let Err(e) = imp::migrate() {
			tracing::warn!("legacy per-user install cleanup: {e}");
		}
	});
}

#[cfg(not(windows))]
pub(crate) fn migrate_in_background() {}

#[cfg(windows)]
mod imp {
	use std::path::{Path, PathBuf};
	use windows_sys::Win32::System::Registry::*;

	const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Pulsar";

	fn wide(s: &str) -> Vec<u16> {
		s.encode_utf16().chain(std::iter::once(0)).collect()
	}

	fn hkcu_value(name: &str) -> Option<String> {
		unsafe {
			let mut h: HKEY = std::ptr::null_mut();
			if RegOpenKeyExW(HKEY_CURRENT_USER, wide(UNINSTALL_KEY).as_ptr(), 0, KEY_READ, &mut h) != 0 {
				return None;
			}
			let mut buf = [0u16; 1024];
			let mut len = (buf.len() * 2) as u32;
			let mut ty = 0u32;
			let ok = RegQueryValueExW(
				h,
				wide(name).as_ptr(),
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
	}

	fn same_dir(a: &Path, b: &Path) -> bool {
		let norm = |p: &Path| {
			std::fs::canonicalize(p)
				.unwrap_or_else(|_| p.to_path_buf())
				.to_string_lossy()
				.trim_end_matches('\\')
				.to_ascii_lowercase()
		};
		norm(a) == norm(b)
	}

	fn shortcut_paths() -> Vec<PathBuf> {
		let mut v = Vec::new();
		if let Some(appdata) = std::env::var_os("APPDATA") {
			v.push(PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs\Pulsar.lnk"));
		}
		if let Some(profile) = std::env::var_os("USERPROFILE") {
			v.push(PathBuf::from(profile).join(r"Desktop\Pulsar.lnk"));
		}
		v
	}

	/// PowerShell single-quoted literal.
	fn ps_quote(s: &str) -> String {
		format!("'{}'", s.replace('\'', "''"))
	}

	pub(super) fn migrate() -> Result<(), String> {
		// Only a per-user pulsar-setup install registers this HKCU entry with its own
		// uninstaller (the NSIS install's entry lives under HKLM).
		let Some(unins) = hkcu_value("UninstallString") else {
			return Ok(());
		};
		if !unins.to_ascii_lowercase().contains("pulsar-setup.exe") {
			return Ok(());
		}
		let Some(old_dir) = hkcu_value("InstallLocation").map(PathBuf::from) else {
			return Ok(());
		};
		let exe = std::env::current_exe().map_err(|e| e.to_string())?;
		let Some(my_dir) = exe.parent() else {
			return Ok(());
		};
		if same_dir(my_dir, &old_dir) {
			return Ok(()); // we ARE the per-user install — nothing to migrate
		}
		// A dev build (target\debug|release) must never touch the user's real install.
		if my_dir.components().any(|c| c.as_os_str().eq_ignore_ascii_case("target")) {
			return Ok(());
		}
		tracing::info!(old = %old_dir.display(), new = %my_dir.display(), "migrating from the per-user pulsar-setup install");

		// 1) Shortcuts that pointed at the old copy → this exe.
		let lnks: Vec<String> = shortcut_paths()
			.into_iter()
			.filter(|p| p.exists())
			.map(|p| ps_quote(&p.display().to_string()))
			.collect();
		if !lnks.is_empty() {
			let script = format!(
				"$w=New-Object -ComObject WScript.Shell; foreach($l in @({lnks})){{ $s=$w.CreateShortcut($l); \
				 if($s.TargetPath -and $s.TargetPath.ToLower().StartsWith({old}.ToLower())){{ \
				 $s.TargetPath={exe}; $s.WorkingDirectory={dir}; $s.Save() }} }}",
				lnks = lnks.join(","),
				old = ps_quote(&old_dir.display().to_string()),
				exe = ps_quote(&exe.display().to_string()),
				dir = ps_quote(&my_dir.display().to_string()),
			);
			use std::os::windows::process::CommandExt;
			const CREATE_NO_WINDOW: u32 = 0x0800_0000;
			let st = std::process::Command::new("powershell")
				.args(["-NoProfile", "-NonInteractive", "-Command", &script])
				.creation_flags(CREATE_NO_WINDOW)
				.status()
				.map_err(|e| e.to_string())?;
			if !st.success() {
				return Err(format!("shortcut rewrite failed ({st})"));
			}
		}

		// 2) The stale copy. Sanity: it must look like a pulsar-setup install dir, so a bad
		// registry value can never make us delete an unrelated folder.
		let looks_right = old_dir.join("pulsar.exe").is_file() && old_dir.join("pulsar-setup.exe").is_file();
		if looks_right {
			if let Err(e) = std::fs::remove_dir_all(&old_dir) {
				// Old copy still running / file locked: keep the uninstall entry so the next
				// launch retries.
				return Err(format!("could not remove {}: {e}", old_dir.display()));
			}
		}

		// 3) Its uninstall entry (Settings → Apps would otherwise list two Pulsars).
		unsafe {
			RegDeleteKeyW(HKEY_CURRENT_USER, wide(UNINSTALL_KEY).as_ptr());
		}
		tracing::info!("per-user pulsar-setup install removed; shortcuts now point here");
		Ok(())
	}
}
