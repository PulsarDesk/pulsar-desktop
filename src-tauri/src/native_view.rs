//! Native video renderer: play the host's RTP/H.264 stream in a borderless
//! fullscreen **ffplay** window with hardware decode — far lighter than the webview
//! WebCodecs+canvas path. ffplay (bundled, statically linked SDL) reads the stream
//! via a tiny SDP; keyboard+mouse input is captured below the OS by the Interception
//! driver (see `kbdhook`), so a separate top-level window doesn't break control.
//! Opt-in on all platforms (the embedded webview WebCodecs canvas is the default). On
//! Windows it spawns ffplay; on Linux/macOS it spawns **mpv** (`spawn_mpv`, hwdec=auto →
//! rkmpp on RK3588). It's a separate top-level window, so it's a fallback for when the
//! embedded path can't hardware-decode; the embedded webview is preferred because it stays
//! inside the app + keeps input capture (on Linux WebKitGTK decodes via GStreamer
//! `mppvideodec`).
//!
//! Split into submodules (re-exported here so existing `native_view::*` paths are unchanged):
//! - `spawn` — free port / SDP writer / ffplay / vidsink / render / mpv spawners.
//! - `ipc` — mpv JSON-IPC helpers (pause/resume, numeric property polling).
//! - `mpvgl` — libmpv RENDER-API single-surface renderer into a `GtkGLArea` (Linux/X11).

mod ipc;
mod spawn;

// Re-export so the previous flat paths (`native_view::spawn_mpv`, `native_view::write_sdp`,
// `native_view::mpv_ipc_get_f64`, …) keep working unchanged. The submodule items carry their
// own `#[cfg]`s, so the glob only re-exports what is actually compiled on each platform.
// All of `ipc`'s items are `cfg(all(unix, not(macos)))`, so gate its glob the same way —
// otherwise the re-export is empty (e.g. on Windows) and warns as unused.
#[cfg(all(unix, not(target_os = "macos")))]
pub use ipc::*;

/// Picture upscaling (Settings → Görüntü → Görüntü iyileştirme): "off" | "auto" | "fsr" | "ai".
/// pulsar-render gets it as `upscale <mode>` (Windows: FSR / NVIDIA RTX VSR / Intel VSR;
/// Linux: FSR in the GL presenter); mpv (macOS, the Linux mpv fallback) loads the FSR shader
/// for any mode but "off".
static UPSCALE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

pub fn set_upscale_mode(mode: &str) {
	let m = match mode {
		"auto" | "fsr" | "ai" | "sgsr" => mode,
		_ => "off",
	};
	*UPSCALE.lock().unwrap() = supported_or_nearest(m, &upscale_modes()).to_string();
}

/// The picture upscalers this machine can run, in menu order — Settings → Görüntü iyileştirme
/// and the in-session menu offer only these (an option that silently ran something else was
/// confusing: "Snapdragon GSR" on a Mac ran FSR).
/// - Windows: off · auto · FSR · AI (the driver's super resolution: NVIDIA / Intel GPUs only) · SGSR
/// - macOS: off · auto · FSR · AI (MetalFX, macOS 13+) — SGSR is not ported to Metal
/// - Linux: off · auto · FSR · SGSR — no vendor AI upscaler
pub fn upscale_modes() -> Vec<&'static str> {
	static MODES: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
	MODES.get_or_init(|| modes_for(std::env::consts::OS, ai_upscaler())).clone()
}

fn modes_for(os: &str, ai: bool) -> Vec<&'static str> {
	let mut m = vec!["off", "auto", "fsr"];
	if ai && os != "linux" {
		m.push("ai");
	}
	if os != "macos" {
		m.push("sgsr");
	}
	m
}

/// A saved mode this machine cannot run → the closest one it can (a setting synced from
/// another computer, or a GPU change): AI → auto, SGSR → FSR.
fn supported_or_nearest<'a>(mode: &'a str, modes: &[&'static str]) -> &'a str {
	if modes.contains(&mode) {
		mode
	} else if mode == "sgsr" {
		"fsr"
	} else {
		"auto"
	}
}

/// The vendor super resolution the renderer can use: NVIDIA / Intel VSR on Windows (the same
/// test as pulsar-render's present::pick), MetalFX on macOS 13+.
fn ai_upscaler() -> bool {
	#[cfg(windows)]
	{
		pulsar_capture::gpu_vendors().iter().any(|v| *v == 0x10DE || *v == 0x8086)
	}
	#[cfg(target_os = "macos")]
	{
		std::process::Command::new("sw_vers")
			.arg("-productVersion")
			.output()
			.ok()
			.and_then(|o| String::from_utf8(o.stdout).ok())
			.and_then(|v| v.trim().split('.').next()?.parse::<u32>().ok())
			.is_some_and(|major| major >= 13)
	}
	#[cfg(not(any(windows, target_os = "macos")))]
	{
		false
	}
}

/// The renderer's upscalers <m1,m2,…> line: what its menu row offers.
pub fn upscalers_line() -> String {
	format!("upscalers {}", upscale_modes().join(","))
}

#[cfg(test)]
mod upscale_tests {
	use super::{modes_for, supported_or_nearest};

	#[test]
	fn only_what_the_platform_runs() {
		assert_eq!(modes_for("windows", true), ["off", "auto", "fsr", "ai", "sgsr"]);
		assert_eq!(modes_for("windows", false), ["off", "auto", "fsr", "sgsr"]);
		assert_eq!(modes_for("macos", true), ["off", "auto", "fsr", "ai"]);
		assert_eq!(modes_for("macos", false), ["off", "auto", "fsr"]);
		assert_eq!(modes_for("linux", true), ["off", "auto", "fsr", "sgsr"]);
	}

	#[test]
	fn an_unsupported_pick_falls_to_the_nearest() {
		let mac = modes_for("macos", false);
		assert_eq!(supported_or_nearest("sgsr", &mac), "fsr");
		assert_eq!(supported_or_nearest("ai", &mac), "auto");
		assert_eq!(supported_or_nearest("fsr", &mac), "fsr");
	}
}

pub fn upscale_mode() -> String {
	let g = UPSCALE.lock().unwrap();
	if g.is_empty() {
		"auto".into()
	} else {
		g.clone()
	}
}

/// FSR for mpv (AMD FidelityFX FSR 1.0.2, MIT — agyild's mpv port), written next to the temp
/// files on first use. None if it can't be written.
#[cfg_attr(windows, allow(dead_code))] // mpv plays video on macOS / the Linux fallback only
pub fn mpv_fsr_shader() -> Option<std::path::PathBuf> {
	const SRC: &str = include_str!("native_view/FSR.glsl");
	let p = std::env::temp_dir().join("pulsar-fsr.glsl");
	if std::fs::read_to_string(&p).ok().as_deref() != Some(SRC) {
		std::fs::write(&p, SRC).ok()?;
	}
	Some(p)
}
pub use spawn::*;

// `mpvgl` stays a public module so `native_view::mpvgl::{MpvGl, SharedMpv}` is unchanged.
#[cfg(target_os = "linux")]
pub mod mpvgl;
