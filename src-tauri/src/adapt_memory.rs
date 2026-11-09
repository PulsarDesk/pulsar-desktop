//! Per-peer adaptive-streaming memory (Phase 1.3): the last rate a session to a peer ran
//! cleanly at, so the next session starts near it instead of probing from scratch.
//!
//! A tiny JSON map in the app config dir (`adapt-memory.json`): `{ "<peer>|<class>": {
//! "kbps": N, "at": <unix secs> } }`. Best-effort — a missing/corrupt file just means no hint.
//!
//! Keyed by peer AND path class (`lan` / `wan` / `relay`, see [`PathClass`]): the same host
//! reached over the home LAN and over a tethered mobile link are two different paths, and a
//! 40 Mbit LAN rate seeding a 10 Mbit mobile session was the opening of the 2026-11-09 field
//! collapse (loss from the first window → the ABR halved to the floor and stayed there).
//! Entries of the old peer-only format (no `|class` suffix) are ignored and dropped on the
//! next write.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
struct Entry {
	kbps: u32,
	at: u64,
}

/// What kind of network path a session runs on — the memory's second key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathClass {
	/// Direct to a private / link-local / loopback address: a local network.
	Lan,
	/// Direct to a public address: the internet (incl. a punched mobile / CGNAT path).
	Wan,
	/// Relayed by the rendezvous server.
	Relay,
}

impl PathClass {
	pub(crate) fn as_str(self) -> &'static str {
		match self {
			PathClass::Lan => "lan",
			PathClass::Wan => "wan",
			PathClass::Relay => "relay",
		}
	}

	/// Classify a live path: relayed, or direct to `peer` (its address; unknown → `wan`, the
	/// cautious answer — a wrong `lan` would seed a thin path with a LAN rate).
	pub(crate) fn of(transport: pulsar_core::Transport, peer: Option<IpAddr>) -> Self {
		match transport {
			pulsar_core::Transport::Relay => PathClass::Relay,
			pulsar_core::Transport::Direct => match peer {
				Some(ip) if is_local_ip(ip) => PathClass::Lan,
				_ => PathClass::Wan,
			},
		}
	}
}

/// Private (RFC 1918 / IPv6 unique-local), link-local or loopback. The CGNAT shared space
/// (100.64/10) is NOT local: a direct path to it is a carrier network or an overlay VPN
/// (Tailscale) that crosses the internet.
fn is_local_ip(ip: IpAddr) -> bool {
	match ip {
		IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
		IpAddr::V6(v6) => {
			if let Some(v4) = v6.to_ipv4_mapped() {
				return is_local_ip(IpAddr::V4(v4));
			}
			let s0 = v6.segments()[0];
			v6.is_loopback()
				// fc00::/7 unique-local
				|| (s0 & 0xfe00) == 0xfc00
				// fe80::/10 link-local
				|| (s0 & 0xffc0) == 0xfe80
		}
	}
}

fn key(peer: &str, class: PathClass) -> String {
	format!("{peer}|{}", class.as_str())
}

/// Remembered for 30 days; the file keeps 90.
const HINT_MAX_AGE_SECS: u64 = 30 * 24 * 3600;
const KEEP_SECS: u64 = 90 * 24 * 3600;

fn lookup(m: &HashMap<String, Entry>, peer: &str, class: PathClass, now: u64) -> Option<u32> {
	let e = m.get(&key(peer, class))?;
	(e.kbps > 0 && now.saturating_sub(e.at) < HINT_MAX_AGE_SECS).then_some(e.kbps)
}

fn store(m: &mut HashMap<String, Entry>, peer: &str, class: PathClass, kbps: u32, now: u64) {
	m.insert(key(peer, class), Entry { kbps, at: now });
	// Keep the file small: drop entries older than 90 days, and the old peer-only keys (no
	// path class — they can't tell a LAN rate from a mobile one).
	let cutoff = now.saturating_sub(KEEP_SECS);
	m.retain(|k, e| e.at >= cutoff && k.contains('|'));
}

fn path(app: &AppHandle) -> PathBuf {
	app.path()
		.app_config_dir()
		.unwrap_or_else(|_| PathBuf::from("."))
		.join("adapt-memory.json")
}

fn load(app: &AppHandle) -> HashMap<String, Entry> {
	std::fs::read(path(app))
		.ok()
		.and_then(|b| serde_json::from_slice(&b).ok())
		.unwrap_or_default()
}

fn now_secs() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_secs())
		.unwrap_or(0)
}

/// The peer's last known-good rate (kbit/s) on this kind of path, if remembered within the
/// last 30 days.
pub(crate) fn hint(app: &AppHandle, peer: &str, class: PathClass) -> Option<u32> {
	lookup(&load(app), peer, class, now_secs())
}

/// Remember `kbps` as the peer's last known-good rate on this kind of path (overwrites).
pub(crate) fn remember(app: &AppHandle, peer: &str, class: PathClass, kbps: u32) {
	if kbps == 0 || peer.is_empty() {
		return;
	}
	let mut m = load(app);
	store(&mut m, peer, class, kbps, now_secs());
	let p = path(app);
	if let Some(dir) = p.parent() {
		let _ = std::fs::create_dir_all(dir);
	}
	if let Ok(json) = serde_json::to_vec_pretty(&m) {
		let _ = std::fs::write(p, json);
	}
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
		assert_eq!(PathClass::of(Transport::Relay, ip("192.168.1.5")), PathClass::Relay);
		assert_eq!(PathClass::of(Transport::Relay, None), PathClass::Relay);
		for local in ["192.168.1.61", "10.0.0.2", "172.16.4.1", "169.254.3.3", "127.0.0.1", "::1", "fe80::1", "fd12::3", "::ffff:192.168.1.2"] {
			assert_eq!(PathClass::of(Transport::Direct, ip(local)), PathClass::Lan, "{local}");
		}
		for public in ["2.28.36.116", "100.72.1.1", "172.32.0.1", "2001:db8::1", "::ffff:8.8.8.8"] {
			assert_eq!(PathClass::of(Transport::Direct, ip(public)), PathClass::Wan, "{public}");
		}
		// Unknown address: the cautious class.
		assert_eq!(PathClass::of(Transport::Direct, None), PathClass::Wan);
	}

	#[test]
	fn memory_is_keyed_by_peer_and_path_class() {
		let now = 1_000_000_000;
		let mut m = HashMap::new();
		store(&mut m, "123456789", PathClass::Lan, 40_000, now);
		// A LAN rate never seeds a mobile (wan) or relayed session to the same peer.
		assert_eq!(lookup(&m, "123456789", PathClass::Lan, now), Some(40_000));
		assert_eq!(lookup(&m, "123456789", PathClass::Wan, now), None);
		assert_eq!(lookup(&m, "123456789", PathClass::Relay, now), None);
		store(&mut m, "123456789", PathClass::Wan, 5_000, now);
		assert_eq!(lookup(&m, "123456789", PathClass::Wan, now), Some(5_000));
		assert_eq!(lookup(&m, "123456789", PathClass::Lan, now), Some(40_000));
		assert_eq!(lookup(&m, "other", PathClass::Lan, now), None);
		// Expiry: 30 days for a hint.
		assert_eq!(lookup(&m, "123456789", PathClass::Lan, now + HINT_MAX_AGE_SECS), None);
	}

	#[test]
	fn old_peer_only_entries_are_ignored_and_dropped() {
		let now = 1_000_000_000;
		let mut m: HashMap<String, Entry> =
			serde_json::from_str(r#"{ "123456789": { "kbps": 40000, "at": 1000000000 } }"#).unwrap();
		for c in [PathClass::Lan, PathClass::Wan, PathClass::Relay] {
			assert_eq!(lookup(&m, "123456789", c, now), None);
		}
		store(&mut m, "123456789", PathClass::Relay, 3_000, now);
		assert_eq!(m.len(), 1);
		assert!(m.contains_key("123456789|relay"));
		// Entries past 90 days are dropped on write.
		store(&mut m, "x", PathClass::Lan, 1_000, now + KEEP_SECS + 1);
		assert_eq!(m.len(), 1);
	}
}
