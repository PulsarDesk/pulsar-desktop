//! Which DRM render node the Linux host's VA-API / QSV encode opens.
//!
//! libva's default device is `/dev/dri/renderD128`, the first GPU the kernel probed. On a
//! hybrid machine that is often NOT the GPU driving the screen: an AMD/Intel iGPU panel next to
//! an NVIDIA dGPU (no VA-API encode on the NVIDIA node), or two AMD GPUs where the monitor hangs
//! off the second. The ffmpeg CLI's `-vaapi_device`, the encoder probes (`caps.rs`) and the
//! in-process libav host (`host/libav.rs`) all read `StreamCfg::vaapi_device`, so this picks it
//! once at startup from sysfs:
//!
//! 1. `PULSAR_VAAPI_DEVICE` (a path) wins, untouched.
//! 2. The render node of the first VA-capable GPU (not NVIDIA) with an enabled, connected
//!    connector — the one scanning out the desktop.
//! 3. The first VA-capable GPU at all (a headless iGPU still encodes uploaded frames).
//! 4. `/dev/dri/renderD128`, the old default.
//!
//! The pure walk ([`pick_from`]) takes a sysfs root so it is tested on every OS; only Linux
//! calls it. A monitor moved to another GPU after startup keeps the startup pick (the encode
//! still works: the frames come from x11grab in system memory). Untested on Linux hardware.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::path::Path;

/// libva's own default; the result when nothing better is found.
pub(crate) const DEFAULT_NODE: &str = "/dev/dri/renderD128";
const VENDOR_NVIDIA: u16 = 0x10de;

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

/// The render node for VA-API / QSV on this machine (see the module doc).
#[cfg(target_os = "linux")]
pub(crate) fn screen_render_node() -> String {
	if let Ok(v) = std::env::var("PULSAR_VAAPI_DEVICE") {
		let v = v.trim();
		if !v.is_empty() {
			return v.to_string();
		}
	}
	let node = pick_from(Path::new("/sys/class/drm")).unwrap_or_else(|| DEFAULT_NODE.to_string());
	tracing::info!(node = %node, "VA-API render node (the GPU driving the screen)");
	node
}

/// Walk a `/sys/class/drm`-shaped directory and pick the render node (rules 2-3).
pub(crate) fn pick_from(root: &Path) -> Option<String> {
	let cards = scan(root);
	let va_capable = |c: &&Card| c.vendor != Some(VENDOR_NVIDIA);
	cards
		.iter()
		.filter(va_capable)
		.find(|c| c.active_outputs > 0)
		.or_else(|| cards.iter().find(va_capable))
		.map(|c| c.render_node.clone())
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
	fn connector_without_enabled_file_counts_on_status() {
		let r = root("noenabled");
		card(&r, 0, 128, None); // SoC: no PCI vendor file — not NVIDIA, VA-capable
		card(&r, 1, 129, Some("0x1002"));
		connector(&r, 1, "HDMI-A-1", "connected", None);
		assert_eq!(pick_from(&r.0).as_deref(), Some("/dev/dri/renderD129"));
	}
}
