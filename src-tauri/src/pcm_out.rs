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
/// Jitter cushion of a live stream: when its queue is empty (the first frame, or frames came
/// late and it ran dry) this much silence goes in ahead of the frame. Without it every frame
/// played the moment it arrived, so any late one left a gap: the call stuttered on Wi-Fi.
const CUSHION_MS: usize = 60;

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
			if live && sdl3_sys::audio::SDL_GetAudioStreamQueued(self.stream) <= 0 {
				let pad = vec![0u8; CUSHION_MS * BYTES_PER_MS];
				sdl3_sys::audio::SDL_PutAudioStreamData(self.stream, pad.as_ptr().cast(), pad.len() as i32);
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
///
/// Two sources share the reference, each on its own timeline, summed by [`window`]: the call
/// voice ([`push`]) and, on a client, the host's session audio ([`push_session`]). Without the
/// second, the host's sound played here (a Discord call on the host, a game) reached our mic,
/// went back to the host and out through its speakers into the host's own voice apps: the
/// remote party heard themselves, round and round.
pub mod far_end {
	use std::collections::VecDeque;
	use std::sync::Mutex;
	use std::time::{Duration, Instant};

	/// Settings → Echo cancellation (on by default). Off, [`window`] returns `None`: the mic and
	/// a Linux host's system-audio capture pass through untouched.
	static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

	pub fn set_enabled(on: bool) {
		ENABLED.store(on, std::sync::atomic::Ordering::Relaxed);
	}

	fn enabled() -> bool {
		ENABLED.load(std::sync::atomic::Ordering::Relaxed)
	}

	/// Which timeline a push belongs to.
	#[derive(Clone, Copy)]
	enum Src {
		/// The peer's voice in a call (`voice_sink`).
		Voice = 0,
		/// The host's session audio played on this client (`native_audio`).
		Session = 1,
	}

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

	static LINES: Mutex<[Option<Line>; 2]> = Mutex::new([None, None]);

	fn idx(origin: Instant, t: Instant) -> i64 {
		if t >= origin {
			(t.duration_since(origin).as_secs_f64() * RATE) as i64
		} else {
			-((origin.duration_since(t).as_secs_f64() * RATE) as i64)
		}
	}

	/// `pcm` (s16le mono, the call voice) starts playing at `play_at`.
	pub fn push(pcm: &[u8], play_at: Instant) {
		push_src(Src::Voice, pcm.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32_768.0), play_at);
	}

	/// `pcm` (interleaved f32 with `channels` channels, 48 kHz: the host's session audio)
	/// starts playing at `play_at`. Downmixed to mono: the mic hears the speakers' sum.
	pub fn push_session(pcm: &[f32], channels: usize, play_at: Instant) {
		let ch = channels.max(1);
		let k = 1.0 / ch as f32;
		push_src(Src::Session, pcm.chunks_exact(ch).map(|f| f.iter().sum::<f32>() * k), play_at);
	}

	fn push_src(src: Src, samples: impl Iterator<Item = f32>, play_at: Instant) {
		let mut g = LINES.lock().unwrap_or_else(|e| e.into_inner());
		let now = Instant::now();
		let slot = &mut g[src as usize];
		// A line idle past IDLE starts over (its old samples are long gone from the window).
		if slot.as_ref().is_some_and(|l| l.last_push.elapsed() >= IDLE) {
			*slot = None;
		}
		let line = slot.get_or_insert_with(|| Line { origin: now, start: idx(now, play_at), buf: VecDeque::new(), last_push: now });
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
		line.buf.extend(samples);
		let over = line.buf.len().saturating_sub(KEEP);
		line.buf.drain(..over);
		line.start += over as i64;
	}

	/// Whether a far end played within [`IDLE`] (the Linux system-audio capture checks it).
	#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
	pub fn active() -> bool {
		enabled()
			&& LINES
			.lock()
			.unwrap_or_else(|e| e.into_inner())
			.iter()
			.flatten()
			.any(|l| l.last_push.elapsed() < IDLE)
	}

	/// The `n` far-end samples that played up to `end`, every source summed (zeros where nothing
	/// played); `None` when there is no far end ([`active`] false).
	pub fn window(end: Instant, n: usize) -> Option<Vec<f32>> {
		if !enabled() {
			return None;
		}
		let g = LINES.lock().unwrap_or_else(|e| e.into_inner());
		let mut out: Option<Vec<f32>> = None;
		for line in g.iter().flatten().filter(|l| l.last_push.elapsed() < IDLE) {
			let acc = out.get_or_insert_with(|| vec![0.0; n]);
			let s = idx(line.origin, end) - n as i64;
			for (j, o) in acc.iter_mut().enumerate() {
				let k = s + j as i64 - line.start;
				if k >= 0 && (k as usize) < line.buf.len() {
					*o += line.buf[k as usize];
				}
			}
		}
		out
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
			// The session audio (stereo, downmixed) adds onto the voice. One test only: the
			// timelines are process-wide.
			let stereo: Vec<f32> = std::iter::repeat_n([0.25f32, 0.15], 960).flatten().collect();
			push_session(&stereo, 2, t0);
			let w = window(t0 + Duration::from_millis(20), 960).unwrap();
			assert!((w[100] - 0.7).abs() < 1e-3, "voice 0.5 + session 0.2, got {}", w[100]);
		}
	}
}
