// Prevents an extra console window on Windows in release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
	// Linux AppImage: swap in the host's libva before anything loads a VA driver.
	#[cfg(target_os = "linux")]
	appimage_host_libva();
	// Linux: GTK derives WM_CLASS from g_get_prgname() (= argv[0]); force the brand
	// name so taskbars/Alt-Tab show "Pulsar" regardless of how the binary was named
	// or launched (plain `cargo run`, symlinks, renamed AppImage). Must run before
	// tao opens the GTK display. NB: `gdk::set_program_class` would panic here (the
	// binding asserts GDK is initialized) — unnecessary anyway, GTK derives the
	// class half of WM_CLASS by capitalizing the prgname ("pulsar" → "Pulsar").
	#[cfg(all(unix, not(target_os = "macos")))]
	gtk::glib::set_prgname(Some("pulsar"));
	// Linux: run the GTK stack under X11 (XWayland on a Wayland session) — the native
	// video renderer embeds INSIDE the app window via X11 `--wid` child windows, which
	// is impossible on a native Wayland surface (no XID → the video opened a separate
	// toplevel). Host-side capture is unaffected (the Wayland portal path keys off
	// XDG_SESSION_TYPE/WAYLAND_DISPLAY, not the GDK backend). An explicit env wins.
	#[cfg(target_os = "linux")]
	if std::env::var_os("GDK_BACKEND").is_none() {
		std::env::set_var("GDK_BACKEND", "x11");
	}
	// Linux AppImage: give the bundled GStreamer its own plugin registry.
	#[cfg(target_os = "linux")]
	appimage_env();
	// APP-UI hardware acceleration (the WebKitGTK webview's accelerated/GPU compositing) — NOT
	// the video stream's encode/decode (those are separate per-session codec settings). Decided
	// ONCE here, before the webview is created (the WebKitGTK env can't change at runtime → the
	// `ui_hardware_accel` setting needs an app restart to apply).
	//
	// Default: ON everywhere EXCEPT the Orange Pi 5 (RK3588/Mali), where WebKitGTK's accelerated
	// compositing has an UNRECOVERABLE "stops presenting" freeze (verified: not recoverable by a
	// window resize or fullscreen toggle). So opi5 defaults to the software path — stable, and the
	// per-row blurred-shadow paint cost is stripped in gaming mode (app.css) so heavy pages stay
	// smooth without the GPU. The `Config.ui_hardware_accel` setting (Some(true)/Some(false))
	// overrides the per-device default. On opi5-with-AC-on we also set PULSAR_FORCE_AC so the
	// frontend runs the present-keepalive (a best-effort freeze reducer).
	#[cfg(target_os = "linux")]
	{
		let is_rk3588 = std::fs::read("/proc/device-tree/compatible")
			.map(|b| b.windows(6).any(|w| w == b"rk3588"))
			.unwrap_or(false);
		let ac_on = ui_hwaccel_pref().unwrap_or(!is_rk3588);
		if ac_on {
			if is_rk3588 && std::env::var_os("PULSAR_FORCE_AC").is_none() {
				std::env::set_var("PULSAR_FORCE_AC", "1");
			}
		} else if std::env::var_os("WEBKIT_DISABLE_COMPOSITING_MODE").is_none() {
			std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
		}
	}
	// Windows: the native video child fully OCCLUDES the webview during a session, and
	// WebView2's native-window occlusion detection then THROTTLES the page (timers +
	// event handling lag by seconds) — Ctrl+Shift combos, the session top bar and the
	// viewrect reports all arrived late. Disable the occlusion calculation; the session
	// UI must keep running at full speed behind the video. An existing env wins.
	#[cfg(windows)]
	{
		const FLAG: &str = "--disable-features=CalculateNativeWinOcclusion";
		match std::env::var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS") {
			Ok(cur) if !cur.contains("CalculateNativeWinOcclusion") => {
				std::env::set_var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", format!("{cur} {FLAG}"));
			}
			Err(_) => std::env::set_var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", FLAG),
			_ => {}
		}
	}
	// `pulsar --relay …` runs a headless relay/rendezvous server instead of the GUI,
	// so the same install can self-host a relay.
	let args: Vec<String> = std::env::args().collect();
	if args.iter().any(|a| a == "--relay") {
		pulsar_tauri::run_relay(&args);
		return;
	}
	// `pulsar --selftest`: check the bundled media stack headless, print a report, exit.
	if args.iter().any(|a| a == "--selftest") {
		std::process::exit(pulsar_tauri::selftest());
	}
	// Diagnostic: exercise the SDL3 client rumble path standalone (no host session).
	if std::env::var_os("PULSAR_RUMBLE_TEST").is_some() {
		pulsar_tauri::rumble_selftest();
		return;
	}
	// Windows: re-launch ELEVATED (UAC) before the GUI starts, so this machine can act as a full
	// host — injecting keyboard/mouse into elevated app windows (Task Manager, installers, any
	// "Run as administrator" window) is blocked for a non-elevated process (UIPI). Controlled by
	// the `request_admin` setting (default ON; user can disable it in Settings → Güvenlik);
	// `PULSAR_NO_ELEVATE=1` bypasses for dev. Done HERE — before `run()` takes the single-instance
	// mutex — so the unelevated process exits cleanly and the elevated one acquires the lock.
	#[cfg(windows)]
	if std::env::var_os("PULSAR_NO_ELEVATE").is_none()
		&& !is_elevated()
		&& request_admin_pref()
		&& relaunch_elevated(&args)
	{
		return;
	}

	pulsar_tauri::run()
}

/// (Linux AppImage) The release AppImage bundles GStreamer 1.20 + plugins from Ubuntu 22.04
/// (`release.yml`) for the in-process Wayland capture — a distro's own, newer plugins can't
/// load into the bundled core. linuxdeploy's AppRun hook points GStreamer at the bundled
/// plugins; give them their own registry file too, so the system's GStreamer and the bundled
/// one don't rebuild each other's cache on every launch. (`pipewiresrc` uses the SYSTEM
/// libpipewire — linuxdeploy's excludelist keeps it out — so PipeWire needs nothing here.)
/// An explicit env wins; system gst tools spawned later get it stripped again
/// (`pulsar_core::capture::use_system_gst_env`).
#[cfg(target_os = "linux")]
fn appimage_env() {
	let Some(appdir) = std::env::var_os("APPDIR").filter(|d| !d.is_empty()) else {
		return;
	};
	let appdir = std::path::PathBuf::from(appdir);
	if appdir.join("usr/lib/gstreamer-1.0").is_dir() && std::env::var_os("GST_REGISTRY_1_0").is_none() {
		let cache = std::env::var_os("XDG_CACHE_HOME")
			.filter(|d| !d.is_empty())
			.map(std::path::PathBuf::from)
			.or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")));
		if let Some(cache) = cache {
			let file = format!(
				"{}{}.bin",
				pulsar_core::capture::APPIMAGE_REGISTRY_PREFIX,
				std::env::consts::ARCH
			);
			std::env::set_var("GST_REGISTRY_1_0", cache.join("pulsar").join(file));
		}
	}
}

/// (Linux AppImage) Re-exec once with the HOST's libva preloaded, when the host has one.
///
/// The bundled libva is Ubuntu 22.04's 2.14 loader, and a VA driver only exports the init
/// symbol of the libva it was built against (Mesa 25: `__vaDriverInit_1_24`) — so on any newer
/// distro the bundled loader opens no driver: no VA-API encode (`vaapih264enc`) and no VA-API
/// decode in pulsar-render/libmpv. libva's soname is ABI-stable, so the host's own loader
/// (always matched to its drivers) slots in for the bundled one; `LD_PRELOAD` makes the
/// bundled copies unused without shadowing every other bundled lib the way an
/// `LD_LIBRARY_PATH=/usr/lib…` would. It has to be set before the process starts, hence the
/// exec. A host without libva keeps the bundled one, so the app still starts everywhere.
/// `PULSAR_HOST_LIBVA=0` opts out; children inherit the preload (it's the host's own lib).
#[cfg(target_os = "linux")]
fn appimage_host_libva() {
	use std::os::unix::process::CommandExt;
	if std::env::var_os("APPDIR").is_none() || std::env::var_os("PULSAR_HOST_LIBVA").is_some() {
		return;
	}
	let arch = std::env::consts::ARCH;
	let mut dirs = vec![
		format!("/usr/lib/{arch}-linux-gnu"),
		format!("/lib/{arch}-linux-gnu"),
		"/usr/lib64".to_string(),
	];
	// Plain /usr/lib holds 64-bit libs only where lib64 is not its own dir (Arch & co.).
	let lib64_is_dir = std::fs::symlink_metadata("/usr/lib64")
		.map(|m| m.is_dir())
		.unwrap_or(false);
	if !lib64_is_dir {
		dirs.push("/usr/lib".to_string());
	}
	let host_lib = |name: &str| {
		dirs.iter()
			.map(|d| std::path::Path::new(d).join(name))
			.find(|p| p.exists())
	};
	let Some(libva) = host_lib("libva.so.2") else {
		return;
	};
	let mut preload = vec![libva.into_os_string()];
	preload.extend(
		["libva-drm.so.2", "libva-x11.so.2", "libva-wayland.so.2"]
			.iter()
			.filter_map(|n| host_lib(n))
			.map(|p| p.into_os_string()),
	);
	if let Some(cur) = std::env::var_os("LD_PRELOAD").filter(|v| !v.is_empty()) {
		preload.push(cur);
	}
	let Ok(exe) = std::env::current_exe() else {
		return;
	};
	let err = std::process::Command::new(exe)
		.args(std::env::args_os().skip(1))
		.env("LD_PRELOAD", preload.join(std::ffi::OsStr::new(":")))
		.env("PULSAR_HOST_LIBVA", "1")
		.exec();
	// Only reached when exec failed: carry on with the bundled libva.
	eprintln!("pulsar: re-exec with the host libva failed: {err}");
}

/// (Windows) Read the persisted `request_admin` preference BEFORE Tauri starts. Defaults to `true`
/// (the Config default) when the file/field is absent, so a fresh install elevates by default.
#[cfg(windows)]
fn request_admin_pref() -> bool {
	let Some(appdata) = std::env::var_os("APPDATA") else {
		return true;
	};
	let path = std::path::PathBuf::from(appdata)
		.join("dev.pulsar.app")
		.join("config.json");
	pulsar_core::config::Config::load(&path).request_admin
}

/// (Windows) Whether THIS process is already running elevated (admin token).
#[cfg(windows)]
fn is_elevated() -> bool {
	use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
	use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
	use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
	unsafe {
		let mut token: HANDLE = std::ptr::null_mut();
		if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
			return false;
		}
		let mut elev = TOKEN_ELEVATION { TokenIsElevated: 0 };
		let mut len = 0u32;
		let ok = GetTokenInformation(
			token,
			TokenElevation,
			&mut elev as *mut _ as *mut _,
			std::mem::size_of::<TOKEN_ELEVATION>() as u32,
			&mut len,
		);
		CloseHandle(token);
		ok != 0 && elev.TokenIsElevated != 0
	}
}

/// (Windows) Spawn a fresh, ELEVATED copy of this exe via ShellExecuteW("runas") — pops the UAC
/// prompt. Re-passes the original args (so `--connect …` kiosk launches keep working). Returns
/// `true` if the elevated process was launched (the caller then exits); `false` if the user
/// declined UAC or it errored (the caller keeps running unelevated as a best effort).
#[cfg(windows)]
fn relaunch_elevated(args: &[String]) -> bool {
	use windows_sys::Win32::UI::Shell::ShellExecuteW;
	use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
	let Ok(exe) = std::env::current_exe() else {
		return false;
	};
	let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
	let verb = wide("runas");
	let file = wide(&exe.to_string_lossy());
	// Re-pass argv[1..], quoting each to survive spaces.
	let params_str = args
		.iter()
		.skip(1)
		.map(|a| format!("\"{a}\""))
		.collect::<Vec<_>>()
		.join(" ");
	let params = wide(&params_str);
	let h = unsafe {
		ShellExecuteW(
			std::ptr::null_mut(),
			verb.as_ptr(),
			file.as_ptr(),
			if params_str.is_empty() {
				std::ptr::null()
			} else {
				params.as_ptr()
			},
			std::ptr::null(),
			SW_SHOWNORMAL,
		)
	};
	// ShellExecuteW returns a value > 32 on success.
	h as isize > 32
}

/// Read the persisted `ui_hardware_accel` preference from the app config file BEFORE Tauri starts
/// (so the WebKitGTK compositing env is set before the webview is created). Returns `None` (auto)
/// if the file/field is absent — the per-device default then applies.
#[cfg(target_os = "linux")]
fn ui_hwaccel_pref() -> Option<bool> {
	let base = std::env::var_os("XDG_CONFIG_HOME")
		.map(std::path::PathBuf::from)
		.or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))?;
	let path = base.join("dev.pulsar.app").join("config.json");
	pulsar_core::config::Config::load(&path).ui_hardware_accel
}
