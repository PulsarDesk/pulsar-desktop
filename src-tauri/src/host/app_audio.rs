//! Audio → App sounds (issue #13): the client leaves some of the host's apps out of the session
//! audio (a Discord call the client is also in, say). This module holds which apps are left out
//! and tells each client what plays; the capture side is `handlers/app_mix.rs`.
//!
//! - The list (`DataMsg::AudioApps`): every app with an audio session (Windows audio session
//!   manager, grouped by executable), plus any left-out app that has none right now (so it can
//!   be put back). Voice-chat apps are flagged; the menu lists them first.
//! - Left-out apps are host-wide (every session of this host), by executable name, case-blind,
//!   for as long as Pulsar runs.
//! - Windows only (Win10 20H1+, the process loopback). The other hosts never send the list, so
//!   their clients offer nothing: macOS (ScreenCaptureKit can exclude apps) and Linux
//!   (per-sink-input recording) are not built yet.

use std::collections::BTreeSet;
use std::sync::Mutex;

use pulsar_core::service::{AudioAppInfo, DataMsg};

static MUTED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
/// Bumped on every change of [`MUTED`] (the capture controller and the list watchers look).
static GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Voice-chat executables (lower case): their sound is what echoes back to a caller.
const VOICE: &[&str] = &[
	"discord.exe",
	"discordptb.exe",
	"discordcanary.exe",
	"teams.exe",
	"ms-teams.exe",
	"zoom.exe",
	"skype.exe",
	"slack.exe",
	"whatsapp.exe",
	"telegram.exe",
	"signal.exe",
	"ts3client_win64.exe",
	"teamspeak.exe",
	"mumble.exe",
	"guilded.exe",
	"element.exe",
];

/// Leave `key` (an executable name) out of the session audio, or put it back.
pub(crate) fn set_muted(key: &str, muted: bool) {
	let k = key.trim().to_lowercase();
	if k.is_empty() {
		return;
	}
	let mut g = MUTED.lock().unwrap_or_else(|e| e.into_inner());
	let changed = if muted { g.insert(k) } else { g.remove(&k) };
	if changed {
		GEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
		tracing::info!(app = key, muted, "audio: app sound {}", if muted { "left out" } else { "back in" });
	}
}

/// The left-out executables (lower case).
pub(crate) fn muted() -> BTreeSet<String> {
	MUTED.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Changes so far (see [`GEN`]).
pub(crate) fn generation() -> u64 {
	GEN.load(std::sync::atomic::Ordering::SeqCst)
}

/// This host can leave apps out.
pub(crate) fn supported() -> bool {
	#[cfg(windows)]
	{
		pulsar_core::audio::process_loopback_supported()
	}
	#[cfg(not(windows))]
	{
		false
	}
}

fn is_voice(exe_lower: &str) -> bool {
	VOICE.contains(&exe_lower)
}

/// A name for the menu: the executable without `.exe`, a few well-known ones spelled out.
fn display_name(exe: &str) -> String {
	let stem = exe.strip_suffix(".exe").or_else(|| exe.strip_suffix(".EXE")).unwrap_or(exe);
	let known = match stem.to_lowercase().as_str() {
		"ms-teams" | "teams" => Some("Microsoft Teams"),
		"msedge" => Some("Microsoft Edge"),
		"ts3client_win64" => Some("TeamSpeak"),
		"discordptb" => Some("Discord PTB"),
		"discordcanary" => Some("Discord Canary"),
		"msrdc" => Some("Remote Desktop"),
		_ => None,
	};
	if let Some(k) = known {
		return k.to_string();
	}
	let mut c = stem.chars();
	match c.next() {
		Some(f) => f.to_uppercase().chain(c).collect(),
		None => exe.to_string(),
	}
}

/// The list a client sees: `apps` = (executable, playing) of every session, Pulsar's own left
/// out by the caller. Grouped by executable, left-out apps without a session added.
fn build_list(apps: &[(String, bool)], muted: &BTreeSet<String>) -> Vec<AudioAppInfo> {
	let mut out: Vec<AudioAppInfo> = Vec::new();
	for (exe, _) in apps {
		let low = exe.to_lowercase();
		if out.iter().any(|a| a.key.to_lowercase() == low) {
			continue;
		}
		out.push(AudioAppInfo {
			key: exe.clone(),
			name: display_name(exe),
			muted: muted.contains(&low),
			voice: is_voice(&low),
		});
	}
	for m in muted {
		if !out.iter().any(|a| a.key.to_lowercase() == *m) {
			out.push(AudioAppInfo { key: m.clone(), name: display_name(m), muted: true, voice: is_voice(m) });
		}
	}
	out.sort_by(|a, b| b.voice.cmp(&a.voice).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
	out
}

/// The current list (empty off Windows).
pub(crate) fn app_list() -> Vec<AudioAppInfo> {
	#[cfg(windows)]
	{
		let own = std::process::id();
		let parents = pulsar_core::audio::parent_pids();
		let apps: Vec<(String, bool)> = pulsar_core::audio::audio_session_apps()
			.into_iter()
			.filter(|a| a.pid != own && !pulsar_core::audio::descends_from(a.pid, own, &parents))
			.map(|a| (a.exe, a.active))
			.collect();
		build_list(&apps, &muted())
	}
	#[cfg(not(windows))]
	{
		Vec::new()
	}
}

/// Tell this session's client what plays, now and whenever it changes (polled every second,
/// at once after a mute), for as long as this stream generation lives. Nothing on a host that
/// cannot leave apps out.
pub(crate) fn spawn_list_watch(out: tokio::sync::mpsc::Sender<DataMsg>, epoch: crate::process::ProcEpoch) {
	use std::sync::atomic::Ordering;
	if !supported() {
		return;
	}
	let born = epoch.load(Ordering::SeqCst);
	let _ = std::thread::Builder::new().name("audio-apps".into()).spawn(move || {
		let mut last: Option<Vec<AudioAppInfo>> = None;
		let mut seen_gen = u64::MAX;
		let mut next = std::time::Instant::now();
		while epoch.load(Ordering::SeqCst) == born && !out.is_closed() {
			let g = generation();
			if g != seen_gen || std::time::Instant::now() >= next {
				seen_gen = g;
				next = std::time::Instant::now() + std::time::Duration::from_secs(1);
				let now = app_list();
				if last.as_ref() != Some(&now) {
					let _ = out.try_send(DataMsg::AudioApps(now.clone()));
					last = Some(now);
				}
			}
			std::thread::sleep(std::time::Duration::from_millis(100));
		}
	});
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn list_groups_flags_and_keeps_left_out_apps() {
		let apps = vec![
			("firefox.exe".to_string(), true),
			("Discord.exe".to_string(), true),
			("firefox.exe".to_string(), false),
		];
		let muted: BTreeSet<String> = ["discord.exe".to_string(), "spotify.exe".to_string()].into_iter().collect();
		let l = build_list(&apps, &muted);
		let keys: Vec<&str> = l.iter().map(|a| a.key.as_str()).collect();
		assert_eq!(keys, vec!["Discord.exe", "firefox.exe", "spotify.exe"], "voice first, then by name");
		assert!(l[0].voice && l[0].muted && l[0].name == "Discord");
		assert!(!l[1].muted && l[1].name == "Firefox");
		assert!(l[2].muted && !l[2].voice, "left out without a session: still listed");
	}

	#[test]
	fn names() {
		assert_eq!(display_name("ms-teams.exe"), "Microsoft Teams");
		assert_eq!(display_name("spotify.exe"), "Spotify");
		assert_eq!(display_name("x"), "X");
	}
}
