//! Per-peer adaptive-streaming memory (Phase 1.3): the last rate a session to a peer ran
//! cleanly at, keyed by peer AND path class (`lan` / `wan` / `relay`), so the next session starts
//! near it instead of probing from scratch.
//!
//! The map, its keys and the path classification are shared with the mobile client
//! (`pulsar_core::adapt::memory`, `pulsar_core::netpath`); this file only places it in the app
//! config dir (`adapt-memory.json`).

use std::net::IpAddr;
use std::path::PathBuf;

use tauri::{AppHandle, Manager};

/// What kind of network path a session runs on — the memory's second key. Shared with the
/// mobile client (pulsar-core `netpath`): a ZeroTier / WireGuard / Tailscale path is `Wan`
/// even with a private address, so an overlay session no longer shares (and starts from) the
/// home-LAN rate, nor picks uncompressed audio.
pub(crate) use pulsar_core::netpath::PathClass;

/// Classify a live path (see [`pulsar_core::netpath::classify`]): relayed, or direct to
/// `peer` — `Lan` only for a private address that is not on an overlay VPN and, when the
/// round trip is known, answers like a LAN.
pub(crate) fn classify(transport: pulsar_core::Transport, peer: Option<IpAddr>, rtt_ms: Option<f32>) -> PathClass {
	pulsar_core::netpath::classify(transport, peer, rtt_ms)
}

fn path(app: &AppHandle) -> PathBuf {
	app.path()
		.app_config_dir()
		.unwrap_or_else(|_| PathBuf::from("."))
		.join("adapt-memory.json")
}

/// The peer's last known-good rate (kbit/s) on this kind of path, if remembered within the
/// last 30 days.
pub(crate) fn hint(app: &AppHandle, peer: &str, class: PathClass) -> Option<u32> {
	pulsar_core::adapt::memory::hint(&path(app), peer, class)
}

/// Remember `kbps` as the peer's last known-good rate on this kind of path (overwrites).
pub(crate) fn remember(app: &AppHandle, peer: &str, class: PathClass, kbps: u32) {
	pulsar_core::adapt::memory::remember(&path(app), peer, class, kbps)
}

#[cfg(test)]
mod tests {
	use super::*;
	use pulsar_core::Transport;

	fn ip(s: &str) -> Option<IpAddr> {
		Some(s.parse().unwrap())
	}

	#[test]
	fn classifies_the_path() {
		assert_eq!(classify(Transport::Relay, ip("192.168.1.5"), None), PathClass::Relay);
		assert_eq!(classify(Transport::Relay, None, None), PathClass::Relay);
		// Local addresses (whether one is LAN here also depends on this machine's overlay
		// interfaces, so only the address test is pinned).
		for local in ["192.168.1.61", "10.0.0.2", "172.16.4.1", "169.254.3.3", "127.0.0.1", "::1", "fe80::1", "fd12::3", "::ffff:192.168.1.2"] {
			assert!(pulsar_core::netpath::is_local_ip(ip(local).unwrap()), "{local}");
		}
		assert_eq!(classify(Transport::Direct, ip("127.0.0.1"), Some(0.1)), PathClass::Lan);
		for public in ["2.28.36.116", "100.72.1.1", "172.32.0.1", "2001:db8::1", "::ffff:8.8.8.8"] {
			assert_eq!(classify(Transport::Direct, ip(public), None), PathClass::Wan, "{public}");
		}
		// A private address with an internet round trip (an overlay the interface check
		// missed) is not a LAN.
		assert_eq!(classify(Transport::Direct, ip("10.8.0.3"), Some(14.0)), PathClass::Wan);
		// Unknown address: the cautious class.
		assert_eq!(classify(Transport::Direct, None, None), PathClass::Wan);
	}
}
