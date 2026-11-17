//! The ringtone of an incoming voice call (`call`), played natively (SDL3 via `pcm_out`) so it
//! rings with no window on screen and no webview autoplay rules in the way.
//!
//! The choice lives in the webview's settings (`ui.ringtone`, `ui.ringtonePath`) and is pushed
//! here with `set_ringtone` at start and on every change. Built-in tones are synthesized (no
//! audio assets to license); a custom file is any format the bundled ffmpeg reads, decoded
//! once to 48 kHz mono PCM (the first [`CUSTOM_MAX_SECS`] seconds) and cached.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A custom file is cut here.
const CUSTOM_MAX_SECS: u32 = 40;
/// Silence between two plays of a custom file.
const CUSTOM_GAP_MS: usize = 800;
/// A ring stops on its own after this long (the call layer times out sooner).
const MAX_RING: Duration = Duration::from_secs(60);
/// A preview plays at most this long.
const PREVIEW: Duration = Duration::from_secs(6);

/// The built-in tones, as the settings name them.
pub const TONES: &[&str] = &["classic", "chime", "pulse", "none", "custom"];

struct Pref {
	tone: String,
	path: String,
	ffmpeg: String,
	/// (path, decoded PCM) of the last custom file decoded.
	cache: Option<(String, Arc<Vec<u8>>)>,
}

static PREF: Mutex<Pref> =
	Mutex::new(Pref { tone: String::new(), path: String::new(), ffmpeg: String::new(), cache: None });

fn pref() -> std::sync::MutexGuard<'static, Pref> {
	PREF.lock().unwrap_or_else(|e| e.into_inner())
}

/// The settings' choice (`tone` one of [`TONES`], `path` the custom file).
pub fn set(tone: &str, path: &str, ffmpeg: String) {
	let mut p = pref();
	p.tone = if TONES.contains(&tone) { tone.to_string() } else { "classic".into() };
	p.path = path.trim().to_string();
	p.ffmpeg = ffmpeg;
}

/// A playing ringtone; dropping it stops the sound within ~20 ms.
pub struct Ringer {
	stop: Arc<AtomicBool>,
}

impl Drop for Ringer {
	fn drop(&mut self) {
		self.stop.store(true, Ordering::SeqCst);
	}
}

/// Ring with the chosen tone until the returned handle is dropped (or [`MAX_RING`]).
pub fn ring() -> Ringer {
	let (tone, path) = {
		let p = pref();
		(p.tone.clone(), p.path.clone())
	};
	start(tone, path, MAX_RING)
}

/// The one preview playing (a new one replaces it).
static PREVIEWING: Mutex<Option<Ringer>> = Mutex::new(None);

/// Settings → "Play": `tone`/`path` as currently picked (not yet pushed with `set`).
pub fn preview(tone: &str, path: &str) {
	let r = start(tone.to_string(), path.to_string(), PREVIEW);
	*PREVIEWING.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
}

pub fn stop_preview() {
	PREVIEWING.lock().unwrap_or_else(|e| e.into_inner()).take();
}

fn start(tone: String, path: String, max: Duration) -> Ringer {
	let stop = Arc::new(AtomicBool::new(false));
	let flag = stop.clone();
	let spawned = std::thread::Builder::new().name("ringtone".into()).spawn(move || {
		if tone == "none" {
			return;
		}
		let cycle = match tone.as_str() {
			"custom" => custom_pcm(&path).unwrap_or_else(|| Arc::new(synth("classic"))),
			t => Arc::new(synth(t)),
		};
		play_loop(&cycle, &flag, max);
	});
	if let Err(e) = spawned {
		tracing::warn!("ringtone: thread failed to start: {e}");
	}
	Ringer { stop }
}

/// Feed `cycle` to the output again and again, ~200 ms ahead, until stopped or `max`.
fn play_loop(cycle: &[u8], stop: &AtomicBool, max: Duration) {
	if cycle.is_empty() {
		return;
	}
	let Some(mut out) = crate::pcm_out::PcmOut::open() else {
		tracing::warn!("ringtone: no output device opens");
		return;
	};
	let until = Instant::now() + max;
	let mut pos = 0usize;
	// 20 ms per push.
	const STEP: usize = 1920;
	while !stop.load(Ordering::SeqCst) && Instant::now() < until {
		while out.queued_ms() < 200 {
			let end = (pos + STEP).min(cycle.len());
			out.push(&cycle[pos..end], false);
			pos = if end >= cycle.len() { 0 } else { end };
		}
		std::thread::sleep(Duration::from_millis(15));
	}
	// Dropping `out` cuts what is still queued: a stop is immediate.
}

/// The custom file decoded (cached by path); `None` when it is unset or unreadable (the caller
/// falls back to the classic tone).
fn custom_pcm(path: &str) -> Option<Arc<Vec<u8>>> {
	let path = path.trim();
	if path.is_empty() {
		return None;
	}
	let ffmpeg = {
		let p = pref();
		if let Some((cp, pcm)) = &p.cache {
			if cp == path {
				return Some(pcm.clone());
			}
		}
		p.ffmpeg.clone()
	};
	let ffmpeg = if ffmpeg.is_empty() { "ffmpeg".to_string() } else { ffmpeg };
	let mut cmd = std::process::Command::new(&ffmpeg);
	cmd.args(["-hide_banner", "-loglevel", "error", "-i"])
		.arg(path)
		.args(["-vn", "-t", &CUSTOM_MAX_SECS.to_string(), "-f", "s16le", "-ac", "1", "-ar", "48000", "-"])
		.stdin(std::process::Stdio::null())
		.stderr(std::process::Stdio::null());
	crate::process::no_window(&mut cmd);
	let out = match cmd.output() {
		Ok(o) if o.status.success() && o.stdout.len() > 9600 => o.stdout,
		Ok(_) => {
			tracing::warn!(%path, "ringtone: the custom file does not decode — the classic tone plays");
			return None;
		}
		Err(e) => {
			tracing::warn!(%path, "ringtone: ffmpeg failed to start ({e}) — the classic tone plays");
			return None;
		}
	};
	let mut pcm = out;
	pcm.truncate(pcm.len() & !1);
	pcm.extend(std::iter::repeat_n(0u8, CUSTOM_GAP_MS * 96));
	let pcm = Arc::new(pcm);
	pref().cache = Some((path.to_string(), pcm.clone()));
	Some(pcm)
}

/// One cycle of a built-in tone (s16le 48 kHz mono), shared with mobile (`pulsar_core::call`).
pub fn synth(tone: &str) -> Vec<u8> {
	pulsar_core::call::tone_pcm(tone)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn built_in_tones_have_sound_and_a_pause() {
		for t in ["classic", "chime", "pulse"] {
			let pcm = synth(t);
			assert!(pcm.len() > 96 * 1500, "{t}: at least 1.5 s");
			assert_eq!(pcm.len() % 2, 0);
			let samples: Vec<i16> = pcm.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
			let peak = samples.iter().map(|v| v.unsigned_abs()).max().unwrap();
			assert!(peak > 5000, "{t}: audible (peak {peak})");
			// The cycle ends quiet, so looping it does not click.
			assert!(samples[samples.len() - 1].unsigned_abs() < 50, "{t}: ends in silence");
		}
		assert!(synth("none").is_empty());
	}
}
