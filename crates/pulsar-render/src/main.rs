//! Pulsar native renderer (separate process; see Cargo.toml for why).
//!
//! Phase 1a (this file): Linux only — create a child X11 window of the Tauri window
//! (`--wid`), bring up an EGL/GLES context, and paint the shared egui overlay
//! (`overlay.rs`) with egui_glow. No video yet — the background is cleared dark so the
//! overlay is verifiable on-device. Next: import the rkmpp DRM_PRIME frame as the
//! background texture (port the proven `pulsar-vidsink.c` path) and wire decode.
//!
//! Usage: pulsar-render <stream.sdp> --wid 0x<parent-xid> [--mode game|remote]
//! SIGUSR1/2 toggle the overlay (open/close), like the C vidsink.

mod i18n;
mod overlay;
// The colour description of a decoded picture (SDR = BT.709 limited unless the stream says
// otherwise) and the YCbCr → RGB constants every backend uses. Pure Rust, tested everywhere.
#[allow(dead_code)] // each backend uses its own part
mod colour;

/// `--lang tr|en` (from the app's `Config.language`) — picks the native UI language
/// for the overlay/hints (the renderer can't share the webview's i18n tables; the
/// catalogs live in `lang/tr.json` + `lang/en.json`, see `i18n.rs`).
fn apply_lang() {
	let mut args = std::env::args();
	while let Some(a) = args.next() {
		if a == "--lang" {
			if let Some(v) = args.next() {
				i18n::set_english(v.starts_with("en"));
			}
		}
	}
}

/// Scheduling boosts for the whole life of this process — it only runs while a session shows
/// video (hidden-gaps 1.14: decode / present / receive ran at the default priority on every OS).
/// The process opts out of EcoQoS / power throttling (Windows) and App Nap (macOS); the main
/// thread, which decodes and presents on Windows and macOS, takes the `Present` level (Windows
/// HIGHEST + MMCSS "Games", macOS QoS user-interactive). On Linux it runs the GL present loop
/// and creates the GL / EGL context (Mesa / driver worker threads, on every core, inherit its
/// nice value) and then starts the decode thread (inheriting it too): `Decode`, nice -5, so
/// those threads never crowd out the compositor. Windows also lets DWM schedule its
/// composition with MMCSS (Moonlight's d3d11va does the same). The receive / decode threads
/// boost themselves where they start. `PULSAR_PRIO=0` turns it off.
#[cfg(any(windows, unix))]
fn boost_process() -> (pulsar_prio::Session, pulsar_prio::ThreadBoost) {
	let session = pulsar_prio::session();
	let dwm = pulsar_prio::enable_dwm_mmcss();
	#[cfg(all(unix, not(target_os = "macos")))]
	let role = pulsar_prio::Role::Decode;
	#[cfg(not(all(unix, not(target_os = "macos"))))]
	let role = pulsar_prio::Role::Present;
	let main = pulsar_prio::boost_current_thread(role);
	eprintln!(
		"pulsar-render: priority: present thread {}, process [{}]{}",
		main.how(),
		pulsar_prio::session_report(),
		if dwm { ", dwm mmcss" } else { "" }
	);
	(session, main)
}

#[cfg(all(unix, not(target_os = "macos")))]
mod decode;
#[cfg(all(unix, not(target_os = "macos")))]
mod gpu;
#[cfg(all(unix, not(target_os = "macos")))]
mod kms_lease;
#[cfg(all(unix, not(target_os = "macos")))]
mod linux;
#[cfg(all(unix, not(target_os = "macos")))]
mod video;
#[cfg(all(unix, not(target_os = "macos")))]
mod gl_fsr;

// Shared streaming types + RTP depacketizer for the native-decode backends (Win MF, mac VT,
// Linux libavcodec — the Linux renderer feeds the decoder from it unless
// `PULSAR_LINUX_DEMUX=ffmpeg` brings back libavformat's RTP demux). Pure Rust, unit-tested
// on every host.
#[cfg(any(target_os = "windows", unix, test))]
mod stream;

// Windows: native zero-copy renderer (child HWND + D3D11 + Media Foundation decode).
// Module is `win` (NOT `windows`) so it never shadows the `windows` crate in `use` paths.
#[cfg(target_os = "windows")]
mod win;

// macOS: native zero-copy renderer (borderless panel over the app window + Metal +
// VideoToolbox decode), the analogue of `win`.
#[cfg(target_os = "macos")]
mod mac;
// The macOS backend's pure parts (bitstream conversion for VideoToolbox, picture placement),
// unit-tested on every host.
#[cfg(all(test, not(target_os = "macos")))]
#[path = "mac/bitstream.rs"]
mod mac_bitstream;
#[cfg(all(test, not(target_os = "macos")))]
#[path = "mac/view.rs"]
mod mac_view;

#[cfg(all(unix, not(target_os = "macos")))]
fn main() {
	// `--probe`: headless capability probe (per-codec decoder selection with REAL
	// canned-frame decodes) printing JSON for the app's startup detection.
	if std::env::args().any(|a| a == "--probe") {
		println!("{}", decode::probe_json());
		return;
	}
	apply_lang();
	let _prio = boost_process();
	linux::run();
}

#[cfg(target_os = "windows")]
fn main() {
	// `--probe`: headless capability probe (which codecs have a SYNC decoder MFT this
	// renderer can drive) printing JSON for the app's startup detection — the Windows
	// analogue of the Linux probe above.
	if std::env::args().any(|a| a == "--probe") {
		println!("{}", win::decode::probe_json());
		return;
	}
	apply_lang();
	let _prio = boost_process();
	win::run();
}

#[cfg(target_os = "macos")]
fn main() {
	// `--probe`: which codecs VideoToolbox decodes here (in hardware or not), same JSON as the
	// Windows / Linux probes.
	if std::env::args().any(|a| a == "--probe") {
		println!("{}", mac::decode::probe_json());
		return;
	}
	apply_lang();
	let _prio = boost_process();
	mac::run();
}

#[cfg(not(any(
	target_os = "windows",
	target_os = "macos",
	all(unix, not(target_os = "macos"))
)))]
fn main() {
	eprintln!("pulsar-render: native backend for this platform not built yet");
	std::process::exit(1);
}
