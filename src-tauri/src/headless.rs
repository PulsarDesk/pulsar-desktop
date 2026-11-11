//! Bring the host online with no UI (no webview to call `go_online`): a locked session, a
//! main window that failed to build, or a UI that never started. See `ui_start.rs`.
//!
//! The frontend normally owns `go_online` (startup, retries, settings changes). When it shows
//! up later (session unlocked), its first plain `go_online` ADOPTS the node brought up here
//! instead of tearing it down and rebinding — a rebind would drop the live sessions of a
//! client that connected while the host was locked.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Manager};

use crate::state::AppState;

/// The UI has called `go_online` at least once: it owns registration from now on.
static UI_CALLED: AtomicBool = AtomicBool::new(false);
/// The headless loop has been started (once per process).
static STARTED: AtomicBool = AtomicBool::new(false);
/// What the headless registration brought up, for the UI's first call to adopt.
static ADOPT: Mutex<Option<(String, Key)>> = Mutex::new(None);

/// The config fields a registration depends on: the UI adopts only an identical one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Key {
	relay: String,
	local_relay: bool,
	node_port: u16,
	network_mode: String,
}

pub(crate) fn key_of(cfg: &pulsar_core::config::Config) -> Key {
	Key {
		relay: cfg.relay.clone(),
		local_relay: cfg.use_local_relay,
		node_port: cfg.node_port,
		network_mode: format!("{:?}", cfg.network_mode),
	}
}

/// Pure adoption rule: same config, no one-shot relay credentials (a prompt answer must reach
/// the relay), and the node is still there.
pub(crate) fn adoptable(saved: &Key, now: &Key, creds_empty: bool, node_alive: bool) -> bool {
	saved == now && creds_empty && node_alive
}

pub(crate) fn note_ui_call() {
	UI_CALLED.store(true, Ordering::SeqCst);
}

pub(crate) fn ui_called() -> bool {
	UI_CALLED.load(Ordering::SeqCst)
}

/// The UI's `go_online` (lock held): the headless node's id when it may be adopted. One-shot —
/// later calls (settings change, retry) rebind as before.
pub(crate) fn take_adoptable(now: &Key, creds_empty: bool, node_alive: bool) -> Option<String> {
	let (id, saved) = ADOPT.lock().unwrap_or_else(|e| e.into_inner()).take()?;
	adoptable(&saved, now, creds_empty, node_alive).then_some(id)
}

/// Errors a retry can't fix without the user (the UI shows them when it comes up).
pub(crate) fn needs_user(err: &str) -> bool {
	err.starts_with("RELAY_AUTH_REQUIRED")
		|| err == "RELAY_AUTH_FAILED"
		|| err.contains("incompatible protocol version")
}

/// Start the headless registration loop (idempotent). Retries 3 s → 12 s like the UI's
/// auto-retry, and stops as soon as the UI calls `go_online` itself.
pub(crate) fn start(app: AppHandle, why: &'static str) {
	if ui_called() || STARTED.swap(true, Ordering::SeqCst) {
		return;
	}
	tracing::info!(why, "going online without the UI");
	tauri::async_runtime::spawn(async move {
		let mut delay = Duration::from_secs(3);
		loop {
			let res = {
				let st = app.state::<AppState>();
				let _guard = st.go_online_lock.lock().await;
				if ui_called() {
					tracing::info!("UI is up — it takes over go_online");
					return;
				}
				let key = key_of(&st.config.lock().unwrap());
				let r = crate::host::bring_online(app.clone(), app.state::<AppState>(), None, None)
					.await;
				if let Ok(id) = &r {
					*ADOPT.lock().unwrap_or_else(|e| e.into_inner()) = Some((id.clone(), key));
				}
				r
			};
			match res {
				Ok(id) => {
					tracing::info!(%id, "host online without the UI");
					return;
				}
				Err(e) if needs_user(&e) => {
					tracing::warn!("headless go_online needs the user ({e}) — waiting for the UI");
					return;
				}
				Err(e) => {
					tracing::warn!(retry_in = ?delay, "headless go_online failed: {e}");
					tokio::time::sleep(delay).await;
					delay = (delay * 2).min(Duration::from_secs(12));
				}
			}
		}
	});
}

#[cfg(test)]
mod tests {
	use super::*;

	fn k(relay: &str, port: u16) -> Key {
		Key {
			relay: relay.into(),
			local_relay: false,
			node_port: port,
			network_mode: "Auto".into(),
		}
	}

	#[test]
	fn adopt_rule() {
		let a = k("relay.example:21116", 0);
		assert!(adoptable(&a, &a.clone(), true, true));
		assert!(!adoptable(&a, &k("other:21116", 0), true, true));
		assert!(!adoptable(&a, &k("relay.example:21116", 4000), true, true));
		assert!(!adoptable(&a, &a.clone(), false, true));
		assert!(!adoptable(&a, &a.clone(), true, false));
	}

	#[test]
	fn needs_user_errors() {
		assert!(needs_user("RELAY_AUTH_REQUIRED:true:false"));
		assert!(needs_user("RELAY_AUTH_FAILED"));
		assert!(needs_user("relay: incompatible protocol version (5 vs 4)"));
		assert!(!needs_user("relay unreachable: timed out"));
	}
}
