//! In-app updates for Linux `.deb` / `.rpm` installs.
//!
//! The Tauri updater replaces the running binary in place on Windows, macOS and a
//! FUSE-mounted AppImage (`$APPIMAGE`). A distro-package install (`/usr/bin/pulsar`, owned by
//! dpkg or rpm) is different: the files belong to the package manager, so the update must go
//! through it — otherwise the next `apt upgrade` fights a hand-replaced binary, and the udev
//! rules / postinst of the new version never run.
//!
//! Flow (the update modal's Install button, `linux_package_update`):
//! 1. look the release up in the SAME channel manifest the updater uses, under the package
//!    target `linux-<arch>-deb` / `linux-<arch>-rpm` (scripts/gen-update-manifest.mjs);
//! 2. download it through the updater plugin, which verifies the minisign signature against
//!    the app's pubkey — an unsigned or tampered package is never installed;
//! 3. hand it to the package manager behind ONE graphical admin prompt:
//!    `pkexec apt-get install ./Pulsar_<v>_<arch>.deb` (apt resolves any new dependency —
//!    plain `dpkg -i` would leave the package half-configured), `dnf`/`zypper` for rpm;
//! 4. without pkexec (or without a polkit agent to ask the password) the verified file is
//!    opened with `xdg-open` instead, so the desktop's software installer takes over.
//!
//! The pure decisions (what kind of install this is, which manifest key, which command) are
//! split out and unit-tested on every OS; only the process spawning is Linux-only.
//! Untested on Linux hardware (written 2026-11-14).

// The decisions are Linux-only at runtime but compiled (and unit-tested) everywhere.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use serde::Serialize;

/// How this install can be updated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum InstallKind {
	/// The Tauri updater replaces the app in place (Windows, macOS, FUSE AppImage).
	InPlace,
	/// A Debian package (dpkg owns the binary).
	Deb,
	/// An RPM package.
	Rpm,
	/// Neither: Flatpak, an AppImage started without FUSE, a raw/dev binary. The update is
	/// shown but installing is manual.
	None,
}

/// The manifest key the package of `kind` is published under, e.g. `linux-x86_64-deb`
/// (`arch` = `std::env::consts::ARCH`, which matches the updater's own arch names).
pub(crate) fn updater_target(kind: InstallKind, arch: &str) -> Option<String> {
	match kind {
		InstallKind::Deb => Some(format!("linux-{arch}-deb")),
		InstallKind::Rpm => Some(format!("linux-{arch}-rpm")),
		_ => None,
	}
}

/// The file name the verified package is saved under (Debian / RPM naming, so a software
/// installer opened on it shows a sensible title).
pub(crate) fn package_file_name(kind: InstallKind, version: &str, arch: &str) -> String {
	let v = version.trim().trim_start_matches('v');
	match kind {
		InstallKind::Rpm => format!("Pulsar-{v}-1.{arch}.rpm"),
		_ => {
			let deb_arch = match arch {
				"x86_64" => "amd64",
				"aarch64" => "arm64",
				"arm" => "armhf",
				other => other,
			};
			format!("Pulsar_{v}_{deb_arch}.deb")
		}
	}
}

/// The privileged install command for `pkg` (argv, first = program), or `None` when there is
/// no `pkexec` to ask for the password (→ open the file in the software installer instead).
/// `has` reports whether a program is on PATH.
///
/// Why not tauri-plugin-updater's own deb/rpm installer: it runs a bare `dpkg -i` / `rpm -U`,
/// so a release that adds a dependency fails to install. apt-get/dnf/zypper resolve
/// dependencies, and without pkexec we can still hand the file to the software installer.
/// pkexec clears the environment and there is no tty, so apt runs with
/// DEBIAN_FRONTEND=noninteractive and keeps the user's changed conffiles instead of asking.
pub(crate) fn install_command(kind: InstallKind, pkg: &str, has: impl Fn(&str) -> bool) -> Option<Vec<String>> {
	if !has("pkexec") {
		return None;
	}
	let argv: Vec<&str> = match kind {
		// apt-get needs a path with a slash to treat the argument as a file, not a package name.
		InstallKind::Deb if has("apt-get") => vec![
			"env",
			"DEBIAN_FRONTEND=noninteractive",
			"apt-get",
			"install",
			"-y",
			"--allow-downgrades",
			"-o",
			"Dpkg::Options::=--force-confdef",
			"-o",
			"Dpkg::Options::=--force-confold",
			pkg,
		],
		InstallKind::Deb => vec!["env", "DEBIAN_FRONTEND=noninteractive", "dpkg", "--force-confdef", "--force-confold", "-i", pkg],
		InstallKind::Rpm if has("dnf") => vec!["dnf", "install", "-y", pkg],
		InstallKind::Rpm if has("zypper") => {
			vec!["zypper", "--non-interactive", "install", "--allow-unsigned-rpm", pkg]
		}
		InstallKind::Rpm => vec!["rpm", "-U", "--replacepkgs", pkg],
		_ => return None,
	};
	Some(std::iter::once("pkexec").chain(argv).map(str::to_string).collect())
}

/// What a finished install command means for the UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InstallOutcome {
	Installed,
	/// The user dismissed the password dialog (pkexec 126).
	Cancelled,
	/// pkexec could not ask at all (127: no polkit agent / not authorized) — fall back to the
	/// software installer.
	NoAgent,
	/// The package manager ran and failed.
	Failed,
}

/// pkexec's documented exit codes: 126 = the authorization dialog was dismissed, 127 = not
/// authorized / no authentication agent. Anything else is the package manager's own status.
pub(crate) fn classify_exit(code: Option<i32>) -> InstallOutcome {
	match code {
		Some(0) => InstallOutcome::Installed,
		Some(126) => InstallOutcome::Cancelled,
		Some(127) => InstallOutcome::NoAgent,
		_ => InstallOutcome::Failed,
	}
}

/// This install's update kind (detected once).
pub(crate) fn install_kind() -> InstallKind {
	static KIND: std::sync::OnceLock<InstallKind> = std::sync::OnceLock::new();
	*KIND.get_or_init(detect)
}

#[cfg(target_os = "linux")]
fn detect() -> InstallKind {
	use tauri::utils::config::BundleType;
	// A FUSE-mounted AppImage exports $APPIMAGE: the updater rewrites that file in place.
	if std::env::var_os("APPIMAGE").is_some() {
		return InstallKind::InPlace;
	}
	// The bundler stamps the package type into the binary it puts in each bundle.
	match tauri::utils::platform::bundle_type() {
		Some(BundleType::Deb) => return InstallKind::Deb,
		Some(BundleType::Rpm) => return InstallKind::Rpm,
		// The AppImage's binary, started without FUSE (extract-and-run): nothing to update.
		Some(BundleType::AppImage) => return InstallKind::None,
		_ => {}
	}
	// No stamp (a package built without the bundler, e.g. by hand): ask the package
	// managers whether they own the running executable.
	let Ok(exe) = std::env::current_exe() else { return InstallKind::None };
	let owned = |prog: &str, args: &[&str]| {
		std::process::Command::new(prog)
			.args(args)
			.arg(&exe)
			.stdout(std::process::Stdio::null())
			.stderr(std::process::Stdio::null())
			.status()
			.is_ok_and(|s| s.success())
	};
	if owned("dpkg-query", &["-S"]) {
		InstallKind::Deb
	} else if owned("rpm", &["-qf"]) {
		InstallKind::Rpm
	} else {
		InstallKind::None
	}
}

#[cfg(not(target_os = "linux"))]
fn detect() -> InstallKind {
	InstallKind::InPlace
}

/// `update_install_kind`: how the UI should install a found update.
/// Async (a worker thread, not the main thread): the first call may run `dpkg-query` /
/// `rpm -qf` to detect the package type.
#[tauri::command]
pub(crate) async fn update_install_kind() -> InstallKind {
	install_kind()
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Progress {
	received: u64,
	total: u64,
}

/// Download (signature-verified) and install the newer `.deb` / `.rpm` from the update
/// channel. Emits `pkg-update-progress` `{received, total}` while downloading. Returns
/// `"installed"` (relaunch now), `"opened"` (the software installer took the file; the user
/// finishes there) or an error; `"cancelled"` when the password dialog was dismissed.
#[tauri::command]
pub(crate) async fn linux_package_update(app: tauri::AppHandle, version: String) -> Result<String, String> {
	#[cfg(target_os = "linux")]
	{
		linux::run(app, version).await
	}
	#[cfg(not(target_os = "linux"))]
	{
		let _ = (app, version);
		Err("package updates exist only on Linux".into())
	}
}

#[cfg(target_os = "linux")]
mod linux {
	use super::*;
	use std::path::{Path, PathBuf};
	use std::time::{Duration, Instant};
	use tauri::{Emitter, Manager};
	use tauri_plugin_updater::UpdaterExt;

	fn on_path(prog: &str) -> bool {
		std::env::var_os("PATH")
			.is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(prog).is_file()))
	}

	pub(super) async fn run(app: tauri::AppHandle, version: String) -> Result<String, String> {
		let kind = install_kind();
		let arch = std::env::consts::ARCH;
		let target = updater_target(kind, arch).ok_or("this install is not a .deb/.rpm package")?;
		let update = app
			.updater_builder()
			.target(target.clone())
			.build()
			.map_err(|e| e.to_string())?
			.check()
			.await
			.map_err(|e| format!("{target}: {e}"))?
			.ok_or("no newer version in the update channel")?;
		if update.version.trim_start_matches('v') != version.trim().trim_start_matches('v') {
			// The channel moved on since the modal opened: install what it offers now (still
			// newer than this build and signed), but say so in the log.
			tracing::info!(shown = %version, offered = %update.version, "package update: channel moved on");
		}
		tracing::info!(%target, version = %update.version, url = %update.download_url, "package update: downloading");

		let mut received = 0u64;
		let mut last = Instant::now() - Duration::from_secs(1);
		let app2 = app.clone();
		// download() verifies the signature before returning the bytes.
		let bytes = update
			.download(
				move |chunk, total| {
					received += chunk as u64;
					if last.elapsed() >= Duration::from_millis(150) {
						last = Instant::now();
						let _ = app2.emit("pkg-update-progress", Progress { received, total: total.unwrap_or(0) });
					}
				},
				|| {},
			)
			.await
			.map_err(|e| e.to_string())?;
		let total = bytes.len() as u64;
		let _ = app.emit("pkg-update-progress", Progress { received: total, total });

		let dir = app
			.path()
			.app_cache_dir()
			.map_err(|e| e.to_string())?
			.join("update");
		std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
		let file = dir.join(package_file_name(kind, &update.version, arch));
		std::fs::write(&file, &bytes).map_err(|e| format!("{}: {e}", file.display()))?;
		drop(bytes);

		let file2 = file.clone();
		tauri::async_runtime::spawn_blocking(move || install(kind, &file2))
			.await
			.map_err(|e| e.to_string())?
	}

	fn install(kind: InstallKind, file: &Path) -> Result<String, String> {
		let path = file.to_string_lossy().into_owned();
		let Some(argv) = install_command(kind, &path, on_path) else {
			tracing::info!(file = %path, "package update: no pkexec — opening the package in the software installer");
			return open_in_installer(file);
		};
		tracing::info!(cmd = ?argv, "package update: installing");
		let status = std::process::Command::new(&argv[0])
			.args(&argv[1..])
			.status()
			.map_err(|e| format!("{}: {e}", argv[0]))?;
		match classify_exit(status.code()) {
			InstallOutcome::Installed => {
				tracing::info!("package update: installed");
				let _ = std::fs::remove_file(file);
				Ok("installed".into())
			}
			InstallOutcome::Cancelled => Ok("cancelled".into()),
			InstallOutcome::NoAgent => {
				tracing::info!("package update: pkexec could not authenticate (no polkit agent?) — opening the package instead");
				open_in_installer(file)
			}
			InstallOutcome::Failed => Err(format!("{} exited with {status}", argv[1])),
		}
	}

	fn open_in_installer(file: &Path) -> Result<String, String> {
		let dir: PathBuf = file.parent().map(Path::to_path_buf).unwrap_or_default();
		std::process::Command::new("xdg-open")
			.arg(file)
			.current_dir(dir)
			.spawn()
			.map(|_| "opened".to_string())
			.map_err(|e| format!("xdg-open: {e}"))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn targets_follow_the_manifest_keys() {
		assert_eq!(updater_target(InstallKind::Deb, "x86_64").as_deref(), Some("linux-x86_64-deb"));
		assert_eq!(updater_target(InstallKind::Rpm, "aarch64").as_deref(), Some("linux-aarch64-rpm"));
		assert_eq!(updater_target(InstallKind::InPlace, "x86_64"), None);
		assert_eq!(updater_target(InstallKind::None, "x86_64"), None);
	}

	#[test]
	fn package_names_use_distro_arch_names() {
		assert_eq!(package_file_name(InstallKind::Deb, "v0.28.0", "x86_64"), "Pulsar_0.28.0_amd64.deb");
		assert_eq!(package_file_name(InstallKind::Deb, "0.28.0", "aarch64"), "Pulsar_0.28.0_arm64.deb");
		assert_eq!(package_file_name(InstallKind::Rpm, "0.28.0", "x86_64"), "Pulsar-0.28.0-1.x86_64.rpm");
	}

	#[test]
	fn deb_prefers_apt_so_new_dependencies_resolve() {
		let all = |_: &str| true;
		assert_eq!(
			install_command(InstallKind::Deb, "/c/P.deb", all).unwrap(),
			[
				"pkexec",
				"env",
				"DEBIAN_FRONTEND=noninteractive",
				"apt-get",
				"install",
				"-y",
				"--allow-downgrades",
				"-o",
				"Dpkg::Options::=--force-confdef",
				"-o",
				"Dpkg::Options::=--force-confold",
				"/c/P.deb"
			]
		);
		let no_apt = |p: &str| p != "apt-get";
		assert_eq!(
			install_command(InstallKind::Deb, "/c/P.deb", no_apt).unwrap(),
			["pkexec", "env", "DEBIAN_FRONTEND=noninteractive", "dpkg", "--force-confdef", "--force-confold", "-i", "/c/P.deb"]
		);
	}

	#[test]
	fn rpm_picks_the_distro_frontend() {
		let dnf = |p: &str| p == "pkexec" || p == "dnf";
		assert_eq!(install_command(InstallKind::Rpm, "/x.rpm", dnf).unwrap()[1], "dnf");
		let zypper = |p: &str| p == "pkexec" || p == "zypper";
		assert_eq!(install_command(InstallKind::Rpm, "/x.rpm", zypper).unwrap()[1], "zypper");
		let bare = |p: &str| p == "pkexec";
		assert_eq!(install_command(InstallKind::Rpm, "/x.rpm", bare).unwrap()[1..3], ["rpm", "-U"]);
	}

	#[test]
	fn no_pkexec_means_open_the_file() {
		assert_eq!(install_command(InstallKind::Deb, "/x.deb", |p| p != "pkexec"), None);
		assert_eq!(install_command(InstallKind::InPlace, "/x", |_| true), None);
	}

	#[test]
	fn pkexec_exit_codes() {
		assert_eq!(classify_exit(Some(0)), InstallOutcome::Installed);
		assert_eq!(classify_exit(Some(126)), InstallOutcome::Cancelled);
		assert_eq!(classify_exit(Some(127)), InstallOutcome::NoAgent);
		assert_eq!(classify_exit(Some(100)), InstallOutcome::Failed);
		assert_eq!(classify_exit(None), InstallOutcome::Failed);
	}
}
