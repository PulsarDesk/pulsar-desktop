//! Two-way clipboard sync between this machine and every connected peer — the remote
//! sessions this app is VIEWING (client) and the ones it is HOSTING. On by default; the
//! Settings toggle and the session overlay drive the same switch (`set_clipboard_sync`).
//!
//! A watcher polls the OS clipboard (text) while at least one session is open and sends a
//! change to all of them; text that arrives from a peer is written to the local clipboard
//! and remembered, so the watcher never echoes it back. With sync off nothing is sent
//! automatically and inbound text falls back to the old manual flow (the client's
//! "Send clipboard" button / the host's "Copy" notification).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use pulsar_core::service::DataMsg;
use tauri::{AppHandle, Manager as _};

use crate::state::AppState;

/// Bigger clipboards are not synced (a stray multi-MB copy must not flood a session).
const MAX_BYTES: usize = 1 << 20;
const POLL: Duration = Duration::from_millis(500);

static ENABLED: AtomicBool = AtomicBool::new(true);
/// What the watcher last observed in the local clipboard (`None` = no baseline yet: the next
/// read only sets it, so opening a session doesn't push whatever was copied before).
static SEEN: Mutex<Option<String>> = Mutex::new(None);

pub(crate) fn enabled() -> bool {
	ENABLED.load(Ordering::Relaxed)
}

/// UI → app: the user's `clipboardSync` setting (Settings → General, session overlay).
/// Also mirrors it into every open renderer overlay so its toggle shows the truth.
#[tauri::command]
pub(crate) fn set_clipboard_sync(state: tauri::State<'_, AppState>, enabled: bool) {
	ENABLED.store(enabled, Ordering::Relaxed);
	*SEEN.lock().unwrap() = None;
	let stdins: Vec<_> = state
		.plays
		.lock()
		.unwrap()
		.values()
		.map(|p| p.render_stdin.clone())
		.collect();
	for si in stdins {
		use std::io::Write as _;
		if let Some(si) = si.lock().unwrap().as_mut() {
			let _ = writeln!(si, "audio clip={}", if enabled { 1 } else { 0 });
		}
	}
}

/// Text that came from a peer: write it to the local clipboard. Returns `true` when sync is
/// on and it was applied (the caller then skips the manual-flow notification).
pub(crate) fn apply_remote(text: &str) -> bool {
	if !enabled() || text.is_empty() || text.len() > MAX_BYTES {
		return false;
	}
	*SEEN.lock().unwrap() = Some(text.to_string());
	match crate::io_cmds::write_clipboard_text(text.to_string()) {
		Ok(()) => true,
		Err(e) => {
			tracing::warn!("clipboard sync: writing the remote clipboard failed: {e}");
			false
		}
	}
}

/// Every open session's side-channel sender: viewed (client) and hosted.
fn senders(state: &AppState) -> Vec<tokio::sync::mpsc::Sender<DataMsg>> {
	let mut v: Vec<_> = state
		.plays
		.lock()
		.unwrap()
		.values()
		.map(|p| p.data_tx.clone())
		.collect();
	v.extend(
		state
			.host_out
			.lock()
			.unwrap()
			.values()
			.map(|(_, tx)| tx.clone()),
	);
	v
}

/// Windows bumps this on every clipboard change: the watcher only opens the clipboard when it
/// moved, so the poll never contends with other apps' OpenClipboard. Elsewhere: always read.
#[cfg(windows)]
fn clipboard_seq() -> u32 {
	unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() }
}
#[cfg(not(windows))]
fn clipboard_seq() -> u32 {
	0
}

pub(crate) fn spawn_watcher(app: AppHandle) {
	std::thread::Builder::new()
		.name("clipboard-sync".into())
		.spawn(move || {
			let mut last_seq: Option<u32> = None;
			loop {
				std::thread::sleep(POLL);
				let state = app.state::<AppState>();
				let out = if enabled() {
					senders(&state)
				} else {
					Vec::new()
				};
				if out.is_empty() {
					*SEEN.lock().unwrap() = None;
					last_seq = None;
					continue;
				}
				let seq = clipboard_seq();
				if cfg!(windows) && last_seq == Some(seq) {
					continue;
				}
				last_seq = Some(seq);
				// An empty / non-text clipboard reads as "": it still has to become the baseline,
				// or the first real copy after a session opened on an empty clipboard was taken
				// AS the baseline and never sent (seen on the Orange Pi, 2026-11-10).
				let text = crate::io_cmds::read_clipboard_text().unwrap_or_default();
				if text.len() > MAX_BYTES {
					continue;
				}
				{
					let mut seen = SEEN.lock().unwrap();
					match seen.as_deref() {
						None => {
							*seen = Some(text);
							continue;
						}
						Some(s) if s == text => continue,
						_ => *seen = Some(text.clone()),
					}
				}
				if text.is_empty() {
					continue; // cleared / non-text now: nothing to send
				}
				for tx in out {
					let _ = tx.try_send(DataMsg::Clipboard(text.clone()));
				}
			}
		})
		.expect("spawn clipboard-sync");
}
