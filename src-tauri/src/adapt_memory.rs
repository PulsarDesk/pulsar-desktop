//! Per-peer adaptive-streaming memory (Phase 1.3): the last rate a session to a peer ran
//! cleanly at, keyed by peer AND path class (`lan` / `wan` / `relay`), so the next session starts
//! near it instead of probing from scratch.
//!
//! The map, its keys and the path classification are shared with the mobile client
//! (`pulsar_core::adapt::memory`); this file only places it in the app config dir
//! (`adapt-memory.json`).

use std::path::PathBuf;

use tauri::{AppHandle, Manager};

pub(crate) use pulsar_core::adapt::memory::PathClass;

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
