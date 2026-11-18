//! Client side of Audio → App sounds (issue #13): the host's list of apps playing sound
//! (`DataMsg::AudioApps`) goes to the session's renderer as an `audioapps …` line, and the
//! renderer's `ov set appmute <key>,<0|1>` goes back to the host as `DataMsg::AudioAppMute`.
//! The host side is `host/app_audio.rs`.

use std::collections::HashMap;
use std::sync::Mutex;

use pulsar_core::service::{AudioAppInfo, DataMsg};
use tauri::{AppHandle, Manager};

/// The last line per play session, re-sent when its renderer restarts.
static LINES: Mutex<Option<HashMap<u64, String>>> = Mutex::new(None);

/// The renderer line for `list`: `audioapps -` (none playing) or `<key> <name> <flags>` triples.
pub(crate) fn line(list: &[AudioAppInfo]) -> String {
	let mut s = String::from("audioapps");
	if list.is_empty() {
		s.push_str(" -");
	}
	for a in list {
		let mut flags = String::new();
		if a.muted {
			flags.push('m');
		}
		if a.voice {
			flags.push('v');
		}
		if flags.is_empty() {
			flags.push('-');
		}
		s.push(' ');
		s.push_str(&crate::audio_devices::encode(&a.key));
		s.push(' ');
		s.push_str(&crate::audio_devices::encode(&a.name));
		s.push(' ');
		s.push_str(&flags);
	}
	s
}

/// The host sent its list for play session `id`: remember the line and return it.
pub(crate) fn on_list(id: u64, list: &[AudioAppInfo]) -> String {
	let l = line(list);
	LINES.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(id, l.clone());
	l
}

/// The line a restarted renderer of `id` needs (`None` = the host never sent one).
pub(crate) fn seed_line(id: u64) -> Option<String> {
	LINES.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(&id).cloned())
}

/// The session ended.
pub(crate) fn forget(id: u64) {
	if let Some(m) = LINES.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
		m.remove(&id);
	}
}

/// The renderer's `appmute <key>,<0|1>`: ask the host of play session `id`.
pub(crate) fn mute(app: &AppHandle, id: u64, val: &str) {
	let Some((tok, m)) = val.rsplit_once(',') else { return };
	let Some(key) = crate::audio_devices::decode(tok).filter(|k| !k.is_empty()) else { return };
	let muted = m == "1";
	let tx = app.state::<crate::state::AppState>().plays.lock().unwrap().get(&id).map(|p| p.data_tx.clone());
	if let Some(tx) = tx {
		tracing::info!(id, app = %key, muted, "audio: app sound on the host {}", if muted { "off" } else { "on" });
		let _ = tx.try_send(DataMsg::AudioAppMute { key, muted });
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn line_format() {
		let l = line(&[
			AudioAppInfo { key: "Discord.exe".into(), name: "Discord".into(), muted: true, voice: true },
			AudioAppInfo { key: "my app.exe".into(), name: "My App".into(), muted: false, voice: false },
		]);
		assert_eq!(l, "audioapps Discord.exe Discord mv my%20app.exe My%20App -");
		assert_eq!(line(&[]), "audioapps -");
	}
}
