//! Client audio devices: which microphone goes to the host and which output plays the
//! session (the in-session menu's Ses pickers), and how both survive a device that goes away.
//!
//! - **Pick:** by device NAME (SDL device ids are per run). Empty = the system default. Set
//!   by the webview (`ui.micDevice` / `ui.speakerDevice`, saved like every client pref) through
//!   `set_audio_devices`, and by the renderer's `ov set micdev|spkdev <token>` (render_stats).
//! - **Lists:** SDL3's playback / recording devices, kept fresh by SDL's own hotplug handling
//!   (WASAPI notification client, PulseAudio / PipeWire hotplug, CoreAudio listeners). The
//!   [`start_watcher`] thread sends them to every session's renderer (`micdevs` / `spkdevs`
//!   lines) whenever they or the pick change.
//! - **Fallback** (both `native_audio` and `mic_capture` use [`resolve`] + [`needs_reopen`]):
//!   the pick is used while it is present; otherwise the system default. A stream on a picked
//!   device that vanishes is reopened on the default; once the pick is back, it moves back.
//!   A stream on the default stays put: SDL itself migrates a default-device stream when the
//!   OS default changes (or the default device is unplugged). If no device opens at all, the
//!   caller stays silent and retries every [`RETRY`]. A stream that stops moving data
//!   (driver reset, a zombie device SDL did not notice) is reopened too (the callers' stall
//!   checks).


mod pick;
pub use pick::*;

use std::sync::Mutex;
use std::time::{Duration, Instant};


/// The user's picks (mic, speaker); empty = system default.
static PREFS: Mutex<(String, String)> = Mutex::new((String::new(), String::new()));

/// Set one pick (empty = the system default). Streams follow within [`CHECK_EVERY`].
pub fn set_pref(kind: Kind, name: &str) {
	let name = name.trim().to_string();
	let mut p = PREFS.lock().unwrap_or_else(|e| e.into_inner());
	let slot = match kind {
		Kind::Mic => &mut p.0,
		Kind::Speaker => &mut p.1,
	};
	if *slot != name {
		tracing::info!(?kind, device = %if name.is_empty() { "<default>" } else { &name }, "audio device pick");
		*slot = name;
	}
}

pub fn pref(kind: Kind) -> String {
	let p = PREFS.lock().unwrap_or_else(|e| e.into_inner());
	match kind {
		Kind::Mic => p.0.clone(),
		Kind::Speaker => p.1.clone(),
	}
}


/// SDL's AUDIO subsystem, initialized once and held for the app's life: the device lists are
/// read from it between sessions too, and SDL keeps them current only while it is up.
pub fn ensure_sdl() -> bool {
	static UP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
	*UP.get_or_init(|| {
		// SAFETY: AUDIO is its own subsystem, reference-counted and safe off the main thread
		// (native_audio and ds4_headset init it the same way).
		let ok = unsafe { sdl3_sys::init::SDL_InitSubSystem(sdl3_sys::init::SDL_INIT_AUDIO) };
		if !ok {
			tracing::warn!(err = %sdl_error(), "audio devices: SDL audio init failed — no device lists");
		}
		ok
	})
}

/// SDL's last error text.
pub fn sdl_error() -> String {
	// SAFETY: SDL_GetError returns a valid (possibly empty) C string owned by SDL.
	unsafe {
		let p = sdl3_sys::error::SDL_GetError();
		if p.is_null() {
			String::new()
		} else {
			std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
		}
	}
}

struct Snap {
	at: Option<Instant>,
	mic: Vec<Dev>,
	spk: Vec<Dev>,
}

static SNAP: Mutex<Snap> = Mutex::new(Snap { at: None, mic: Vec::new(), spk: Vec::new() });

/// How long a device list is reused before SDL is asked again.
const FRESH: Duration = Duration::from_millis(400);

/// The devices of `kind` present now; `None` when SDL audio is unavailable (callers then
/// skip their device checks instead of treating every device as gone).
pub fn devices(kind: Kind) -> Option<Vec<Dev>> {
	if !ensure_sdl() {
		return None;
	}
	let mut s = SNAP.lock().unwrap_or_else(|e| e.into_inner());
	let now = Instant::now();
	if s.at.is_none_or(|t| now.duration_since(t) > FRESH) {
		use crate::ds5::endpoint::{sdl_devices, Kind as K};
		s.mic = sdl_devices(K::Recording).into_iter().map(|(id, n)| (id.0, n)).collect();
		s.spk = sdl_devices(K::Playback).into_iter().map(|(id, n)| (id.0, n)).collect();
		s.at = Some(now);
	}
	Some(match kind {
		Kind::Mic => s.mic.clone(),
		Kind::Speaker => s.spk.clone(),
	})
}

/// The cached lists without asking SDL (empty before the first ask): for the renderer seeds,
/// which run under locks on async tasks.
fn cached(kind: Kind) -> Vec<Dev> {
	let s = SNAP.lock().unwrap_or_else(|e| e.into_inner());
	match kind {
		Kind::Mic => s.mic.clone(),
		Kind::Speaker => s.spk.clone(),
	}
}


/// Both renderer lines from the cached lists (a respawned renderer's seed).
pub fn seed_lines() -> [String; 2] {
	[
		line(Kind::Mic, &pref(Kind::Mic), &cached(Kind::Mic)),
		line(Kind::Speaker, &pref(Kind::Speaker), &cached(Kind::Speaker)),
	]
}

/// Keep every session's renderer supplied with the device lists (1 Hz; idle without sessions).
pub fn start_watcher(app: tauri::AppHandle) {
	let spawned = std::thread::Builder::new().name("audio-devices".into()).spawn(move || {
		use std::collections::HashMap;
		use std::io::Write as _;
		use tauri::Manager;
		let mut sent: HashMap<u64, [String; 2]> = HashMap::new();
		let mut last_counts: Option<(usize, usize)> = None;
		loop {
			std::thread::sleep(Duration::from_secs(1));
			let stdins: Vec<_> = {
				let state = app.state::<crate::state::AppState>();
				let plays = state.plays.lock().unwrap_or_else(|e| e.into_inner());
				plays.iter().map(|(id, p)| (*id, p.render_stdin.clone())).collect()
			};
			sent.retain(|id, _| stdins.iter().any(|(i, _)| i == id));
			if stdins.is_empty() {
				continue;
			}
			let mic = devices(Kind::Mic).unwrap_or_default();
			let spk = devices(Kind::Speaker).unwrap_or_default();
			if last_counts != Some((mic.len(), spk.len())) {
				tracing::info!(
					mics = ?mic.iter().map(|d| d.1.as_str()).collect::<Vec<_>>(),
					outputs = ?spk.iter().map(|d| d.1.as_str()).collect::<Vec<_>>(),
					"audio devices"
				);
				last_counts = Some((mic.len(), spk.len()));
			}
			let lines = [line(Kind::Mic, &pref(Kind::Mic), &mic), line(Kind::Speaker, &pref(Kind::Speaker), &spk)];
			for (id, stdin) in stdins {
				if sent.get(&id) == Some(&lines) {
					continue;
				}
				let mut g = stdin.lock().unwrap_or_else(|e| e.into_inner());
				if let Some(si) = g.as_mut() {
					let ok = lines.iter().all(|l| writeln!(si, "{l}").is_ok()) && si.flush().is_ok();
					if ok {
						sent.insert(id, lines.clone());
					}
				}
			}
		}
	});
	if let Err(e) = spawned {
		tracing::warn!("audio devices: watcher failed to start: {e}");
	}
}
