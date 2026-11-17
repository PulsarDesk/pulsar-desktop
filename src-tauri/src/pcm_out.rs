//! Local playback of raw s16le 48 kHz mono PCM through SDL3: the peer's voice in a call (the
//! mic side channel, `DataMsg::Audio`) and the ringtone (`ringtone`). Same device rule as the
//! session audio: the in-session "Audio output" pick while it is connected, else the system
//! default (`audio_devices`).
//!
//! When SDL audio cannot start at all, [`voice_sink`] falls back to the older player process
//! (`audio_io::spawn_audio_player`: paplay / pw-cat / aplay — Linux only).

use std::process::Child;

use pulsar_core::service::DataMsg;

use crate::audio_devices::{self as devs, Kind};

/// 48 kHz mono s16 = 96 bytes per ms.
const BYTES_PER_MS: usize = 96;
/// More than this queued (a burst after a network stall) → drop the backlog: a call must
/// stay live, not drift behind.
const MAX_QUEUED_MS: usize = 250;

/// An open SDL playback stream for s16le 48 kHz mono.
pub struct PcmOut {
	stream: *mut sdl3_sys::audio::SDL_AudioStream,
}

// SAFETY: SDL3 audio streams are internally locked and may be used from any thread; the
// pointer is owned by this value alone.
unsafe impl Send for PcmOut {}

impl PcmOut {
	/// Open on the picked output (else the default). `None` when SDL audio is unavailable or
	/// no device opens.
	pub fn open() -> Option<PcmOut> {
		if !devs::ensure_sdl() {
			return None;
		}
		let want = devs::devices(Kind::Speaker).and_then(|l| devs::resolve(&devs::pref(Kind::Speaker), &l));
		want.and_then(Self::open_on).or_else(|| Self::open_on(0))
	}

	fn open_on(dev: u32) -> Option<PcmOut> {
		use sdl3_sys::audio::*;
		let spec = SDL_AudioSpec { format: SDL_AUDIO_S16LE, channels: 1, freq: 48_000 };
		let id = if dev == 0 { SDL_AUDIO_DEVICE_DEFAULT_PLAYBACK } else { SDL_AudioDeviceID(dev) };
		// SAFETY: plain SDL3 calls; AUDIO is initialized (ensure_sdl). The stream opens paused.
		unsafe {
			let stream = SDL_OpenAudioDeviceStream(id, &spec, None, std::ptr::null_mut());
			if stream.is_null() {
				tracing::debug!(dev, err = %devs::sdl_error(), "pcm out: open failed");
				return None;
			}
			if !SDL_ResumeAudioStreamDevice(stream) {
				SDL_DestroyAudioStream(stream);
				return None;
			}
			Some(PcmOut { stream })
		}
	}

	/// Milliseconds of audio queued and not yet played.
	pub fn queued_ms(&self) -> usize {
		// SAFETY: `stream` is live and owned by self.
		let q = unsafe { sdl3_sys::audio::SDL_GetAudioStreamQueued(self.stream) };
		q.max(0) as usize / BYTES_PER_MS
	}

	/// Queue `pcm` (s16le mono). A live stream (`live`) drops its backlog first when it grew
	/// past [`MAX_QUEUED_MS`].
	pub fn push(&mut self, pcm: &[u8], live: bool) {
		if pcm.is_empty() {
			return;
		}
		// SAFETY: `stream` is live; SDL copies `pcm.len()` bytes out of the slice.
		unsafe {
			if live && self.queued_ms() > MAX_QUEUED_MS {
				sdl3_sys::audio::SDL_ClearAudioStream(self.stream);
			}
			sdl3_sys::audio::SDL_PutAudioStreamData(self.stream, pcm.as_ptr().cast(), pcm.len() as i32);
		}
	}
}

impl Drop for PcmOut {
	fn drop(&mut self) {
		// SAFETY: destroys the stream `open_on` created (closes its logical device).
		unsafe { sdl3_sys::audio::SDL_DestroyAudioStream(self.stream) };
	}
}

/// Player process killed AND reaped on drop (the fallback path).
struct PlayerGuard(Child);
impl Drop for PlayerGuard {
	fn drop(&mut self) {
		let _ = self.0.kill();
		let _ = self.0.wait();
	}
}

enum Sink {
	Sdl(PcmOut),
	/// The player process (held so it lives as long as its stdin) and its stdin.
	Process(#[allow(dead_code)] PlayerGuard, std::process::ChildStdin),
}

/// The per-session handler for the peer's mic audio (`Audio` / `AudioEnd`): opens a player on
/// the first frame, plays every frame, closes it on `AudioEnd`. Used by the host (the client's
/// mic) and by the client (the host's mic during a call).
pub fn voice_sink() -> impl FnMut(DataMsg) + Send + 'static {
	use std::io::Write;
	let mut sink: Option<Sink> = None;
	// A failed open is retried once per `devs::RETRY`, not on every 20 ms frame.
	let mut next_try = std::time::Instant::now();
	move |m: DataMsg| match m {
		DataMsg::Audio(frame) => {
			let now = std::time::Instant::now();
			if sink.is_none() && now >= next_try {
				sink = PcmOut::open().map(Sink::Sdl).or_else(|| {
					crate::audio_io::spawn_audio_player().map(|(c, s)| Sink::Process(PlayerGuard(c), s))
				});
				if sink.is_none() {
					next_try = now + devs::RETRY;
					tracing::warn!("voice: no output opens — the peer's mic is not played");
				}
			}
			match sink.as_mut() {
				Some(Sink::Sdl(o)) => {
					o.push(&frame, true);
					// It starts playing after what was queued before it.
					let queued = o.queued_ms().saturating_sub(frame.len() / BYTES_PER_MS);
					far_end::push(&frame, now + std::time::Duration::from_millis(queued as u64));
				}
				Some(Sink::Process(_, s)) => {
					// No queue to read here: the player's buffer only delays the echo further.
					far_end::push(&frame, now);
					if s.write_all(&frame).is_err() {
						sink = None;
						next_try = now + devs::RETRY;
					}
				}
				None => {}
			}
		}
		DataMsg::AudioEnd => sink = None,
		_ => {}
	}
}

/// The far end of a call as it is PLAYED here, on a timeline: what the echo canceller
/// (`pulsar_core::audio::aec`) removes from the microphone (`mic_capture`) and, on a Linux host,
/// from the system-audio capture (`host/handlers.rs` parec reader).
///
/// A chunk handed to SDL now starts playing after what is already queued, so it is placed at
/// `now + queued`. The real output plays a little later still (device buffer) and a capture is
/// time-stamped when it is READ (a little after it was recorded): both only push the echo later
/// relative to this reference, which the canceller's tail (~213 ms) covers.
pub mod far_end {
	use std::collections::VecDeque;
	use std::sync::Mutex;
	use std::time::{Duration, Instant};

	const RATE: f64 = 48_000.0;
	/// Kept history (the canceller looks back at most one tail plus a chunk).
	const KEEP: usize = 48_000 * 2;
	/// A push this close to where the timeline ends continues it (timing jitter).
	const SLACK: i64 = 1_440;
	/// No far end for this long → nothing to cancel.
	const IDLE: Duration = Duration::from_millis(1_500);

	struct Line {
		origin: Instant,
		/// Sample index of `buf[0]`.
		start: i64,
		buf: VecDeque<f32>,
		last_push: Instant,
	}

	static LINE: Mutex<Option<Line>> = Mutex::new(None);

	fn idx(origin: Instant, t: Instant) -> i64 {
		if t >= origin {
			(t.duration_since(origin).as_secs_f64() * RATE) as i64
		} else {
			-((origin.duration_since(t).as_secs_f64() * RATE) as i64)
		}
	}

	/// `pcm` (s16le mono) starts playing at `play_at`.
	pub fn push(pcm: &[u8], play_at: Instant) {
		let mut g = LINE.lock().unwrap_or_else(|e| e.into_inner());
		let now = Instant::now();
		let line = g.get_or_insert_with(|| Line { origin: now, start: idx(now, play_at), buf: VecDeque::new(), last_push: now });
		line.last_push = now;
		let at = idx(line.origin, play_at);
		let end = line.start + line.buf.len() as i64;
		if line.buf.is_empty() {
			line.start = at;
		} else if at > end + SLACK {
			// A pause in the voice: silence until this chunk.
			line.buf.extend(std::iter::repeat_n(0.0, (at - end) as usize));
		} else if at + SLACK < end {
			// The player dropped its backlog: what was queued after `at` never plays.
			let keep = (at - line.start).max(0) as usize;
			line.buf.truncate(keep);
		}
		line.buf.extend(pcm.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32_768.0));
		let over = line.buf.len().saturating_sub(KEEP);
		line.buf.drain(..over);
		line.start += over as i64;
	}

	/// Whether a far end played within [`IDLE`] (the Linux system-audio capture checks it).
	#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
	pub fn active() -> bool {
		LINE.lock()
			.unwrap_or_else(|e| e.into_inner())
			.as_ref()
			.is_some_and(|l| l.last_push.elapsed() < IDLE)
	}

	/// The `n` far-end samples that played up to `end` (zeros where nothing played); `None`
	/// when there is no far end ([`active`] false).
	pub fn window(end: Instant, n: usize) -> Option<Vec<f32>> {
		let g = LINE.lock().unwrap_or_else(|e| e.into_inner());
		let line = g.as_ref().filter(|l| l.last_push.elapsed() < IDLE)?;
		let e = idx(line.origin, end);
		let s = e - n as i64;
		Some(
			(s..e)
				.map(|i| {
					let k = i - line.start;
					if k >= 0 && (k as usize) < line.buf.len() {
						line.buf[k as usize]
					} else {
						0.0
					}
				})
				.collect(),
		)
	}

	#[cfg(test)]
	mod tests {
		use super::*;

		fn pcm(v: i16, n: usize) -> Vec<u8> {
			std::iter::repeat_n(v.to_le_bytes(), n).flatten().collect()
		}

		#[test]
		fn a_window_reads_back_what_played_when() {
			let t0 = Instant::now() + Duration::from_millis(100);
			push(&pcm(16_384, 960), t0);
			// Continues the line even with a little jitter.
			push(&pcm(-16_384, 960), t0 + Duration::from_millis(21));
			let w = window(t0 + Duration::from_millis(40), 1920).unwrap();
			assert!((w[10] - 0.5).abs() < 1e-3);
			assert!((w[1910] + 0.5).abs() < 1e-3);
			// Before anything played: silence.
			let w = window(t0, 480).unwrap();
			assert!(w.iter().all(|v| *v == 0.0));
			assert!(active());
		}
	}
}
