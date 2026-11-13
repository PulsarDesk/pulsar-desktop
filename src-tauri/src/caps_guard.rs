//! The host's answer to `QueryStreamCaps` must never hang (2026-11-12: a host whose GPU path
//! wedged left its encoder probe stuck — the client got no caps within its 2 s window, then
//! "host silent" and the session closed with no reason shown). The real computation runs on a
//! helper thread; if it isn't done within `REPLY_WITHIN`, the reply is the last good answer
//! saved on disk, else the always-available minimum (software H.264). A computation that is
//! still stuck is not started again — every later query gets the fallback until it finishes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pulsar_core::service::StreamCaps;
use tauri::{AppHandle, Manager};

/// Below the client's 2 s caps wait, with room for the network.
const REPLY_WITHIN: Duration = Duration::from_millis(1500);
const CACHE_FILE: &str = "last-stream-caps.json";

static INFLIGHT: AtomicBool = AtomicBool::new(false);

fn cache_path(app: &AppHandle) -> Option<std::path::PathBuf> {
	// Seat host: keep it in the seat's `--data-dir` (as SYSTEM the app config dir is the
	// systemprofile one, shared by every seat). Otherwise the per-user dir as before.
	if let Some(d) = crate::seat::data_dir() {
		return Some(d.join(CACHE_FILE));
	}
	app.path().app_config_dir().ok().map(|d| d.join(CACHE_FILE))
}

fn save(app: &AppHandle, caps: &StreamCaps) {
	let Some(p) = cache_path(app) else { return };
	if let Some(dir) = p.parent() {
		let _ = std::fs::create_dir_all(dir);
	}
	if let Ok(json) = serde_json::to_string(caps) {
		let _ = std::fs::write(p, json);
	}
}

/// What to answer when the real computation is late: the last good caps (with today's
/// transport features), else software H.264. Monitors are left empty when not cached — the
/// client then shows no picker and streams the primary.
fn fallback(app: &AppHandle, features: Vec<String>) -> StreamCaps {
	let cached = cache_path(app)
		.and_then(|p| std::fs::read_to_string(p).ok())
		.and_then(|s| serde_json::from_str::<StreamCaps>(&s).ok());
	match cached {
		Some(mut c) => {
			c.features = features;
			c
		}
		None => StreamCaps {
			codecs: vec!["h264".into()],
			encoders: vec!["software".into()],
			features,
			displays: Vec::new(),
		},
	}
}

/// Wrap the host's caps computation (`compute`) so a reply always comes within `REPLY_WITHIN`.
pub(crate) fn guarded<F>(app: AppHandle, features: fn() -> Vec<String>, compute: F) -> impl Fn() -> StreamCaps + Send
where
	F: Fn() -> StreamCaps + Send + 'static,
{
	let compute = Arc::new(Mutex::new(compute));
	move || {
		if INFLIGHT.swap(true, Ordering::SeqCst) {
			tracing::warn!("stream caps: an earlier computation is still stuck — answering from the fallback");
			return fallback(&app, features());
		}
		let (tx, rx) = std::sync::mpsc::channel();
		let compute = compute.clone();
		let app_t = app.clone();
		let t0 = std::time::Instant::now();
		let spawned = std::thread::Builder::new().name("stream-caps".into()).spawn(move || {
			let caps = (compute.lock().unwrap_or_else(|e| e.into_inner()))();
			save(&app_t, &caps);
			INFLIGHT.store(false, Ordering::SeqCst);
			let late = t0.elapsed();
			if late > REPLY_WITHIN {
				tracing::warn!(ms = late.as_millis() as u64, "stream caps: computation finished late");
			}
			let _ = tx.send(caps);
		});
		if spawned.is_err() {
			INFLIGHT.store(false, Ordering::SeqCst);
			return fallback(&app, features());
		}
		match rx.recv_timeout(REPLY_WITHIN) {
			Ok(caps) => {
				tracing::info!(ms = t0.elapsed().as_millis() as u64, codecs = ?caps.codecs, encoders = ?caps.encoders, "stream caps answered");
				caps
			}
			Err(_) => {
				tracing::error!(
					ms = REPLY_WITHIN.as_millis() as u64,
					"stream caps: computation not done in time (encoder probe / GPU stuck?) — answering from the fallback"
				);
				fallback(&app, features())
			}
		}
	}
}
