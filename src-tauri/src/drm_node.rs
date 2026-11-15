//! Which DRM render node the Linux host's VA-API / QSV encode opens.
//!
//! libva's default device is `/dev/dri/renderD128`, the first GPU the kernel probed. On a
//! hybrid machine that is often NOT the GPU driving the screen: an AMD/Intel iGPU panel next to
//! an NVIDIA dGPU (no VA-API encode on the NVIDIA node), or two AMD GPUs where the monitor hangs
//! off the second. The ffmpeg CLI's `-vaapi_device`, the VA-API encoder probes (`caps.rs`) and
//! the in-process libav host (`host/libav.rs`) all read `StreamCfg::vaapi_device`, so this picks
//! it once per process from sysfs (memoised: `StreamCfg::default` runs on every settings push
//! from the UI, which never sends the node):
//!
//! 1. `PULSAR_VAAPI_DEVICE` (a path) wins, untouched.
//! 2. The render node of the first VA-capable GPU (not NVIDIA) with an enabled, connected
//!    connector — the one scanning out the desktop.
//! 3. The other VA-capable GPUs (a headless iGPU still encodes uploaded frames).
//! 4. `/dev/dri/renderD128`, the old default.
//!
//! With more than one candidate, the startup encoder probe ([`verify_encode_node`]) takes the
//! first one that really encodes through VA-API — a screen GPU without VA-API encode (an old
//! radeon without VCE/VCN support in Mesa, virtio-gpu, a display-only card) must not push the
//! host onto x264 while another GPU could encode.
//!
//! Not every Linux path follows this node: QSV opens an Intel GPU ([`qsv_node`] for the
//! in-process engine; the CLI's `-init_hw_device qsv` and its caps probe use libva's default
//! node), and Wayland hosts encode VA-API through GStreamer's `va*`/`vaapi*` elements, which
//! pick their own device (see docs/agents/host-encode.md).
//!
//! The pure parts ([`pick_from`], [`pick_verified`], [`qsv_node_in`]) take a sysfs root / list so
//! they are tested on every OS; only Linux calls them. A monitor moved to another GPU after
//! startup keeps the startup pick (the encode still works: the frames come from x11grab in
//! system memory). Untested on Linux hardware.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::path::Path;

/// libva's own default; the result when nothing better is found.
pub(crate) const DEFAULT_NODE: &str = "/dev/dri/renderD128";
const VENDOR_NVIDIA: u16 = 0x10de;
const VENDOR_INTEL: u16 = 0x8086;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Card {
	/// `cardN` index (sort key: the kernel's probe order).
	idx: u32,
	/// `/dev/dri/renderD*` of the same device.
	render_node: String,
	vendor: Option<u16>,
	/// Connected connectors with a CRTC (`enabled`), i.e. screens this GPU scans out.
	active_outputs: usize,
}

/// The node [`verify_encode_node`] confirmed (set once the startup probe ran).
#[cfg(target_os = "linux")]
static VERIFIED: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// `/sys/class/drm`, read once per process.
#[cfg(target_os = "linux")]
fn cards() -> &'static [Card] {
	static CARDS: std::sync::OnceLock<Vec<Card>> = std::sync::OnceLock::new();
	CARDS.get_or_init(|| scan(Path::new("/sys/class/drm")))
}

#[cfg(target_os = "linux")]
fn env_override() -> Option<String> {
	let v = std::env::var("PULSAR_VAAPI_DEVICE").ok()?;
	let v = v.trim();
	(!v.is_empty()).then(|| v.to_string())
}

/// The render node for VA-API on this machine (see the module doc): the probe-confirmed one
/// once [`verify_encode_node`] ran, else the sysfs pick. Memoised — no sysfs walk or log per call.
#[cfg(target_os = "linux")]
pub(crate) fn screen_render_node() -> String {
	if let Some(v) = VERIFIED.get() {
		return v.clone();
	}
	static PICK: std::sync::OnceLock<String> = std::sync::OnceLock::new();
	PICK.get_or_init(|| {
		let node = env_override()
			.or_else(|| candidates(cards()).into_iter().next())
			.unwrap_or_else(|| DEFAULT_NODE.to_string());
		tracing::info!(node = %node, "VA-API render node (the GPU driving the screen)");
		node
	})
	.clone()
}

/// Pick the VA-API node that really encodes: `probe(node)` = a VA-API encode probe on that node.
/// Runs the probes only when there is something to choose between (more than one candidate,
/// no `PULSAR_VAAPI_DEVICE`); memoised, so later calls (and [`screen_render_node`]) return the
/// same node. With no candidate passing, the screen pick stays (the probes then report VA-API
/// as unavailable, as before).
#[cfg(target_os = "linux")]
pub(crate) fn verify_encode_node(probe: impl FnMut(&str) -> bool) -> String {
	VERIFIED
		.get_or_init(|| {
			if env_override().is_some() {
				return screen_render_node();
			}
			let cands = candidates(cards());
			match pick_verified(&cands, probe) {
				Some(n) => {
					if cands.len() > 1 {
						tracing::info!(node = %n, ?cands, "VA-API render node confirmed by an encode probe");
					}
					n
				}
				None => screen_render_node(),
			}
		})
		.clone()
}

/// The VA-API child node for QSV (in-process engine): `preferred` when it is an Intel GPU,
/// else the first Intel GPU's node (FFmpeg forces the iHD driver on the QSV child device, so a
/// screen GPU from another vendor can never be it), else `preferred`.
#[cfg(target_os = "linux")]
pub(crate) fn qsv_node(preferred: &str) -> String {
	if env_override().is_some() {
		return preferred.to_string();
	}
	qsv_node_in(cards(), preferred)
}

/// Walk a `/sys/class/drm`-shaped directory and pick the render node (rules 2-3).
pub(crate) fn pick_from(root: &Path) -> Option<String> {
	candidates(&scan(root)).into_iter().next()
}

/// The VA-capable render nodes (not NVIDIA) in preference order: GPUs driving a screen first,
/// then the rest, each group in the kernel's probe order.
fn candidates(cards: &[Card]) -> Vec<String> {
	let va: Vec<&Card> = cards.iter().filter(|c| c.vendor != Some(VENDOR_NVIDIA)).collect();
	let mut out: Vec<String> = va.iter().filter(|c| c.active_outputs > 0).map(|c| c.render_node.clone()).collect();
	for c in va.iter().filter(|c| c.active_outputs == 0) {
		if !out.contains(&c.render_node) {
			out.push(c.render_node.clone());
		}
	}
	out
}

/// The first candidate `probe` accepts. A single candidate is taken without a probe (there is
/// nothing to fall back to; the caps probe reports whether it encodes).
pub(crate) fn pick_verified(cands: &[String], mut probe: impl FnMut(&str) -> bool) -> Option<String> {
	if cands.len() <= 1 {
		return cands.first().cloned();
	}
	cands.iter().find(|n| probe(n)).cloned()
}

/// See [`qsv_node`].
fn qsv_node_in(cards: &[Card], preferred: &str) -> String {
	let intel = |c: &&Card| c.vendor == Some(VENDOR_INTEL);
	if cards.iter().filter(intel).any(|c| c.render_node == preferred) {
		return preferred.to_string();
	}
	cards.iter().find(intel).map(|c| c.render_node.clone()).unwrap_or_else(|| preferred.to_string())
}

fn scan(root: &Path) -> Vec<Card> {
	let Ok(rd) = std::fs::read_dir(root) else {
		return Vec::new();
	};
	let names: Vec<String> = rd.filter_map(|e| e.ok()?.file_name().into_string().ok()).collect();
	let mut cards: Vec<Card> = names
		.iter()
		.filter_map(|n| {
			let idx: u32 = n.strip_prefix("card")?.parse().ok()?;
			let dev = root.join(n).join("device");
			let render = std::fs::read_dir(dev.join("drm"))
				.ok()?
				.filter_map(|e| e.ok()?.file_name().into_string().ok())
				.filter(|f| f.starts_with("renderD"))
				.min()?;
			let vendor = std::fs::read_to_string(dev.join("vendor"))
				.ok()
				.and_then(|v| u16::from_str_radix(v.trim().trim_start_matches("0x"), 16).ok());
			// Connectors are siblings named `cardN-<type>-<i>` (also nested under cardN/).
			let prefix = format!("{n}-");
			let active_outputs = names
				.iter()
				.filter(|c| c.starts_with(&prefix))
				.filter(|c| connector_active(&root.join(c)))
				.count();
			Some(Card { idx, render_node: format!("/dev/dri/{render}"), vendor, active_outputs })
		})
		.collect();
	cards.sort_by_key(|c| c.idx);
	cards
}

/// `status` = `connected` and `enabled` = `enabled` (a connector without the `enabled` file
/// counts on `status` alone).
fn connector_active(dir: &Path) -> bool {
	let read = |f: &str| std::fs::read_to_string(dir.join(f)).map(|s| s.trim().to_string());
	let connected = read("status").is_ok_and(|s| s == "connected");
	connected && read("enabled").map_or(true, |s| s == "enabled")
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::fs;

	struct Root(std::path::PathBuf);
	impl Drop for Root {
		fn drop(&mut self) {
			let _ = fs::remove_dir_all(&self.0);
		}
	}

	fn root(tag: &str) -> Root {
		let p = std::env::temp_dir().join(format!("pulsar-drm-{tag}-{}", std::process::id()));
		let _ = fs::remove_dir_all(&p);
		fs::create_dir_all(&p).unwrap();
		Root(p)
	}

	fn card(r: &Root, idx: u32, render: u32, vendor: Option<&str>) {
		let dev = r.0.join(format!("card{idx}")).join("device");
		fs::create_dir_all(dev.join("drm").join(format!("renderD{render}"))).unwrap();
		fs::create_dir_all(dev.join("drm").join(format!("card{idx}"))).unwrap();
		if let Some(v) = vendor {
			fs::write(dev.join("vendor"), format!("{v}\n")).unwrap();
		}
	}

	fn connector(r: &Root, idx: u32, name: &str, status: &str, enabled: Option<&str>) {
		let d = r.0.join(format!("card{idx}-{name}"));
		fs::create_dir_all(&d).unwrap();
		fs::write(d.join("status"), format!("{status}\n")).unwrap();
		if let Some(e) = enabled {
			fs::write(d.join("enabled"), format!("{e}\n")).unwrap();
		}
	}

	#[test]
	fn hybrid_laptop_picks_the_igpu_that_drives_the_panel() {
		let r = root("hybrid");
		// card0 = NVIDIA dGPU (renderD128) with an HDMI port, nothing on it.
		card(&r, 0, 128, Some("0x10de"));
		connector(&r, 0, "HDMI-A-1", "disconnected", Some("disabled"));
		// card1 = AMD iGPU driving the built-in panel.
		card(&r, 1, 129, Some("0x1002"));
		connector(&r, 1, "eDP-1", "connected", Some("enabled"));
		assert_eq!(pick_from(&r.0).as_deref(), Some("/dev/dri/renderD129"));
	}

	#[test]
	fn the_gpu_with_the_screen_wins_over_probe_order() {
		let r = root("two-amd");
		card(&r, 0, 128, Some("0x1002"));
		connector(&r, 0, "DP-1", "connected", Some("disabled")); // plugged, not lit
		card(&r, 1, 129, Some("0x8086"));
		connector(&r, 1, "DP-2", "connected", Some("enabled"));
		assert_eq!(pick_from(&r.0).as_deref(), Some("/dev/dri/renderD129"));
	}

	#[test]
	fn no_lit_screen_falls_back_to_the_first_va_capable_gpu() {
		let r = root("headless");
		card(&r, 0, 128, Some("0x10de"));
		card(&r, 1, 129, Some("0x8086"));
		card(&r, 2, 130, Some("0x1002"));
		assert_eq!(pick_from(&r.0).as_deref(), Some("/dev/dri/renderD129"));
	}

	#[test]
	fn nvidia_only_or_empty_gives_none() {
		let r = root("nv");
		card(&r, 0, 128, Some("0x10de"));
		connector(&r, 0, "DP-1", "connected", Some("enabled"));
		assert_eq!(pick_from(&r.0), None);
		assert_eq!(pick_from(&r.0.join("missing")), None);
	}

	#[test]
	fn a_screen_gpu_without_va_encode_falls_back_to_the_next_candidate() {
		let r = root("verify");
		card(&r, 0, 128, Some("0x1002")); // encode-capable, no screen
		card(&r, 1, 129, Some("0x1002")); // drives the screen, no VA-API encode
		connector(&r, 1, "DP-1", "connected", Some("enabled"));
		card(&r, 2, 130, Some("0x10de"));
		let cands = candidates(&scan(&r.0));
		assert_eq!(cands, vec!["/dev/dri/renderD129".to_string(), "/dev/dri/renderD128".to_string()]);
		let mut probed = Vec::new();
		let got = pick_verified(&cands, |n| {
			probed.push(n.to_string());
			n == "/dev/dri/renderD128"
		});
		assert_eq!(got.as_deref(), Some("/dev/dri/renderD128"));
		assert_eq!(probed, vec!["/dev/dri/renderD129", "/dev/dri/renderD128"]);
		// Nothing passes: no pick (the caller keeps the screen GPU).
		assert_eq!(pick_verified(&cands, |_| false), None);
		// One candidate: taken without a probe.
		let one = vec!["/dev/dri/renderD128".to_string()];
		assert_eq!(pick_verified(&one, |_| panic!("no probe for a single candidate")).as_deref(), Some("/dev/dri/renderD128"));
	}

	#[test]
	fn qsv_takes_an_intel_node() {
		let r = root("qsv");
		card(&r, 0, 128, Some("0x8086")); // Intel iGPU, no screen
		card(&r, 1, 129, Some("0x1002")); // AMD dGPU drives the screen
		connector(&r, 1, "DP-1", "connected", Some("enabled"));
		let cards = scan(&r.0);
		assert_eq!(qsv_node_in(&cards, "/dev/dri/renderD129"), "/dev/dri/renderD128");
		// The preferred node is Intel already: kept.
		assert_eq!(qsv_node_in(&cards, "/dev/dri/renderD128"), "/dev/dri/renderD128");
		// No Intel GPU at all: the preferred node (QSV then fails and the CLI takes over).
		let r2 = root("qsv-none");
		card(&r2, 0, 128, Some("0x1002"));
		assert_eq!(qsv_node_in(&scan(&r2.0), "/dev/dri/renderD128"), "/dev/dri/renderD128");
	}

	#[test]
	fn connector_without_enabled_file_counts_on_status() {
		let r = root("noenabled");
		card(&r, 0, 128, None); // SoC: no PCI vendor file — not NVIDIA, VA-capable
		card(&r, 1, 129, Some("0x1002"));
		connector(&r, 1, "HDMI-A-1", "connected", None);
		assert_eq!(pick_from(&r.0).as_deref(), Some("/dev/dri/renderD129"));
	}
}
