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
		"auto" | "fsr" | "ai" => mode,
		_ => "off",
	};
	*UPSCALE.lock().unwrap() = m.to_string();
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
