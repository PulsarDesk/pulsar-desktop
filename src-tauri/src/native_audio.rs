//! Client audio, native on every desktop platform — Moonlight's model: the host's Opus/RTP
//! stream is decoded in-process with libopus and played through SDL3.
//!
//! This replaced two older paths: the webview one (a loopback WebSocket into WebCodecs +
//! WebAudio, Windows/macOS) and Linux's decode-only ffmpeg subprocess feeding SDL3. Both were
//! left over from when the whole session ran in the webview.
//!
//! Pipeline: the viewer's audio UDP socket (fed by the media-over-session demuxer, or by the
//! host on the legacy direct path) → broadcast → this player's thread: RTP header strip and
//! sequence check → libopus decode, with packet-loss concealment for a short gap → an SDL3
//! audio stream. The SDL queue is held near `PULSAR_AUDIO_TARGETMS` (default 30 ms) by
//! nudging the playback rate (host and client clocks drift apart), and flushed once if it
//! ever passes 120 ms (a startup burst or a long stall), so latency stays bounded.
//!
//! The SDL device opens on the first packet, so a session without audio never holds it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

/// The host's Opus rate (`pulsar_core::audio::SAMPLE_RATE`).
const RATE: u32 = 48_000;
/// Stereo: the client always asks for it, and the host clamps the stream to the request.
const CHANNELS: usize = 2;
/// Lost packets in a row that libopus conceals (≈50 ms at the host's 10 ms frames). A longer
/// gap is an outage: concealing it would only add latency, so the decoder restarts instead.
const MAX_CONCEAL: u16 = 5;
/// Largest Opus frame, per channel (120 ms).
const MAX_FRAME: usize = 5760;
/// Packets buffered between the network task and the player thread before they are dropped.
const QUEUE: usize = 256;

/// A running player. Dropping it stops the thread (within ~100 ms) and closes the device.
pub struct NativeAudio {
	stop: Arc<AtomicBool>,
}

impl Drop for NativeAudio {
	fn drop(&mut self) {
		self.stop.store(true, Ordering::Relaxed);
	}
}

/// Play every RTP/Opus datagram that arrives on `rx`. Must be called inside the tokio
/// runtime (a task bridges the broadcast to the player thread).
pub fn start(mut rx: tokio::sync::broadcast::Receiver<Vec<u8>>) -> NativeAudio {
	let stop = Arc::new(AtomicBool::new(false));
	let (tx, packets) = mpsc::sync_channel::<Vec<u8>>(QUEUE);
	tokio::spawn(async move {
		use tokio::sync::broadcast::error::RecvError;
		loop {
			match rx.recv().await {
				Ok(p) => match tx.try_send(p) {
					Ok(()) | Err(mpsc::TrySendError::Full(_)) => {}
					Err(mpsc::TrySendError::Disconnected(_)) => break,
				},
				Err(RecvError::Lagged(_)) => continue,
				Err(RecvError::Closed) => break,
			}
		}
	});
	let flag = stop.clone();
	let spawned = std::thread::Builder::new()
		.name("native-audio".into())
		.spawn(move || run(packets, flag));
	if let Err(e) = spawned {
		tracing::warn!("audio: player thread failed to start: {e}");
	}
	NativeAudio { stop }
}

/// The Opus payload and sequence number of an RTP datagram (header, CSRCs, extension and
/// padding skipped). `None` for anything that is not RTP or carries no payload.
pub(crate) fn rtp_opus(pkt: &[u8]) -> Option<(u16, &[u8])> {
	if pkt.len() < 12 || pkt[0] >> 6 != 2 {
		return None;
	}
	let seq = u16::from_be_bytes([pkt[2], pkt[3]]);
	let mut off = 12 + (pkt[0] & 0x0f) as usize * 4;
	if pkt[0] & 0x10 != 0 {
		let ext = pkt.get(off + 2..off + 4)?;
		off += 4 + u16::from_be_bytes([ext[0], ext[1]]) as usize * 4;
	}
	let mut end = pkt.len();
	if pkt[0] & 0x20 != 0 {
		// Padding: the last byte counts the padding bytes.
		end = end.checked_sub(*pkt.last()? as usize)?;
	}
	let payload = pkt.get(off..end)?;
	(!payload.is_empty()).then_some((seq, payload))
}

/// What to do with a packet, from its sequence number.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
	/// Decode it, after concealing this many lost packets in front of it.
	Play { conceal: u16 },
	/// It is older than one already played (late or duplicate): drop it.
	Late,
	/// A gap too long to conceal, or a new stream (the host restarted its encoder): reset the
	/// decoder, then decode it.
	Restart,
}

/// Tracks the expected RTP sequence number.
#[derive(Default)]
pub(crate) struct Sequencer {
	next: Option<u16>,
}

impl Sequencer {
	pub(crate) fn step(&mut self, seq: u16) -> Step {
		let Some(next) = self.next else {
			self.next = Some(seq.wrapping_add(1));
			return Step::Play { conceal: 0 };
		};
		let ahead = seq.wrapping_sub(next);
		if ahead >= 0x8000 {
			// Behind `next`. A small step back is a late or repeated packet; a huge one is a
			// new stream whose numbers start lower.
			if next.wrapping_sub(seq) <= 64 {
				return Step::Late;
			}
			self.next = Some(seq.wrapping_add(1));
			return Step::Restart;
		}
		self.next = Some(seq.wrapping_add(1));
		if ahead <= MAX_CONCEAL {
			Step::Play { conceal: ahead }
		} else {
			Step::Restart
		}
	}
}

fn run(packets: mpsc::Receiver<Vec<u8>>, stop: Arc<AtomicBool>) {
	let mut decoder = match opus::Decoder::new(RATE, opus::Channels::Stereo) {
		Ok(d) => d,
		Err(e) => {
			tracing::warn!("audio: Opus decoder failed — no client audio: {e}");
			return;
		}
	};
	let mut out: Option<SdlOut> = None;
	let mut seq = Sequencer::default();
	let mut pcm = vec![0f32; MAX_FRAME * CHANNELS];
	// The host's frame length, learnt from the stream; a concealed packet is this long.
	let mut frame = RATE as usize / 100;
	let mut concealed: u64 = 0;
	while !stop.load(Ordering::Relaxed) {
		let pkt = match packets.recv_timeout(Duration::from_millis(100)) {
			Ok(p) => p,
			Err(mpsc::RecvTimeoutError::Timeout) => continue,
			Err(mpsc::RecvTimeoutError::Disconnected) => break,
		};
		let Some((n, payload)) = rtp_opus(&pkt) else {
			continue;
		};
		if out.is_none() {
			out = SdlOut::open();
		}
		let Some(sink) = out.as_mut() else {
			return; // logged by open()
		};
		match seq.step(n) {
			Step::Late => continue,
			Step::Restart => {
				let _ = decoder.reset_state();
			}
			Step::Play { conceal } => {
				for _ in 0..conceal {
					if let Ok(got) = decoder.decode_float(&[], &mut pcm[..frame * CHANNELS], false) {
						sink.push(&pcm[..got * CHANNELS]);
						concealed += 1;
					}
				}
			}
		}
		match decoder.decode_float(payload, &mut pcm, false) {
			Ok(got) => {
				if got > 0 {
					frame = got.min(MAX_FRAME);
				}
				sink.push(&pcm[..got * CHANNELS]);
			}
			Err(e) => tracing::debug!("audio: undecodable Opus packet: {e}"),
		}
	}
	tracing::info!(concealed, "audio: native player stopped");
}

/// SDL3 playback stream with the drift and latency bound described in the module docs.
struct SdlOut {
	stream: *mut sdl3_sys::audio::SDL_AudioStream,
	target_bytes: i32,
	high_bytes: i32,
}

impl SdlOut {
	fn open() -> Option<SdlOut> {
		use sdl3_sys::audio::{
			SDL_AudioSpec, SDL_OpenAudioDeviceStream, SDL_ResumeAudioStreamDevice,
			SDL_AUDIO_DEVICE_DEFAULT_PLAYBACK, SDL_AUDIO_F32,
		};
		use sdl3_sys::init::{SDL_InitSubSystem, SDL_QuitSubSystem, SDL_INIT_AUDIO};
		// SAFETY: plain SDL3 C calls; AUDIO is its own subsystem and safe off the main thread.
		unsafe {
			if !SDL_InitSubSystem(SDL_INIT_AUDIO) {
				tracing::warn!("audio: SDL_InitSubSystem(AUDIO) failed — no client audio");
				return None;
			}
			let spec = SDL_AudioSpec {
				format: SDL_AUDIO_F32,
				channels: CHANNELS as i32,
				freq: RATE as i32,
			};
			let stream = SDL_OpenAudioDeviceStream(
				SDL_AUDIO_DEVICE_DEFAULT_PLAYBACK,
				&spec,
				None,
				std::ptr::null_mut(),
			);
			if stream.is_null() {
				tracing::warn!("audio: SDL_OpenAudioDeviceStream failed — no client audio");
				SDL_QuitSubSystem(SDL_INIT_AUDIO);
				return None;
			}
			SDL_ResumeAudioStreamDevice(stream); // the device opens paused
			let target_ms: usize = std::env::var("PULSAR_AUDIO_TARGETMS")
				.ok()
				.and_then(|s| s.parse().ok())
				.unwrap_or(30)
				.max(1);
			let bytes_per_ms = RATE as usize / 1000 * CHANNELS * 4;
			tracing::info!(target_ms, "audio: native player up (libopus + SDL3)");
			Some(SdlOut {
				stream,
				target_bytes: (bytes_per_ms * target_ms) as i32,
				high_bytes: (bytes_per_ms * 120) as i32,
			})
		}
	}

	/// Queue decoded interleaved PCM, then steer the playback rate so the queue stays near
	/// the target: above it plays slightly faster, below it slightly slower (±3 %, inaudible
	/// at steady drift). Never drops a chunk (that clicks), except one flush past `high_bytes`.
	fn push(&mut self, pcm: &[f32]) {
		use sdl3_sys::audio::{
			SDL_ClearAudioStream, SDL_GetAudioStreamQueued, SDL_PutAudioStreamData,
			SDL_SetAudioStreamFrequencyRatio,
		};
		if pcm.is_empty() {
			return;
		}
		// SAFETY: `stream` is a live SDL audio stream owned by `self`; SDL copies the buffer.
		unsafe {
			SDL_PutAudioStreamData(
				self.stream,
				pcm.as_ptr() as *const std::ffi::c_void,
				std::mem::size_of_val(pcm) as i32,
			);
			let q = SDL_GetAudioStreamQueued(self.stream);
			if q > self.high_bytes {
				SDL_ClearAudioStream(self.stream);
				SDL_SetAudioStreamFrequencyRatio(self.stream, 1.0);
			} else {
				let err = (q - self.target_bytes) as f32 / self.target_bytes as f32;
				let ratio = (1.0 + 0.5 * err).clamp(0.97, 1.03);
				SDL_SetAudioStreamFrequencyRatio(self.stream, if ratio.is_finite() { ratio } else { 1.0 });
			}
		}
	}
}

impl Drop for SdlOut {
	fn drop(&mut self) {
		use sdl3_sys::audio::SDL_DestroyAudioStream;
		use sdl3_sys::init::{SDL_QuitSubSystem, SDL_INIT_AUDIO};
		// SAFETY: destroys the stream `open` created, then drops this thread's AUDIO init.
		unsafe {
			SDL_DestroyAudioStream(self.stream);
			SDL_QuitSubSystem(SDL_INIT_AUDIO);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn rtp(seq: u16, payload: &[u8]) -> Vec<u8> {
		let mut p = vec![0x80, 97, (seq >> 8) as u8, seq as u8, 0, 0, 0, 0, 0, 0, 0, 1];
		p.extend_from_slice(payload);
		p
	}

	fn tone(frames: usize, phase: usize, amp: f32) -> Vec<f32> {
		(0..frames * CHANNELS)
			.map(|i| {
				let t = (phase + i / CHANNELS) as f32 / RATE as f32;
				amp * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
			})
			.collect()
	}

	#[test]
	fn the_payload_skips_the_rtp_header_csrcs_extension_and_padding() {
		assert_eq!(rtp_opus(&rtp(7, &[1, 2, 3])), Some((7, &[1u8, 2, 3][..])));
		// 1 CSRC + a 1-word extension + 2 padding bytes.
		let mut p = vec![0xB1, 97, 0, 9, 0, 0, 0, 0, 0, 0, 0, 1];
		p.extend_from_slice(&[0, 0, 0, 2]); // CSRC
		p.extend_from_slice(&[0xBE, 0xDE, 0, 1, 1, 2, 3, 4]); // extension header + 1 word
		p.extend_from_slice(&[5, 6]); // payload
		p.extend_from_slice(&[0, 2]); // padding (count = 2)
		assert_eq!(rtp_opus(&p), Some((9, &[5u8, 6][..])));
	}

	#[test]
	fn non_rtp_and_empty_payloads_are_ignored() {
		assert_eq!(rtp_opus(&[0x80, 97, 0]), None);
		assert_eq!(rtp_opus(&rtp(1, &[])), None);
		let mut v1 = rtp(1, &[1]);
		v1[0] = 0x40; // RTP version 1
		assert_eq!(rtp_opus(&v1), None);
	}

	#[test]
	fn a_short_gap_is_concealed_a_long_one_restarts() {
		let mut s = Sequencer::default();
		assert_eq!(s.step(100), Step::Play { conceal: 0 });
		assert_eq!(s.step(101), Step::Play { conceal: 0 });
		assert_eq!(s.step(104), Step::Play { conceal: 2 });
		assert_eq!(s.step(104 + MAX_CONCEAL + 2), Step::Restart);
	}

	#[test]
	fn late_packets_drop_and_the_sequence_wraps() {
		let mut s = Sequencer::default();
		assert_eq!(s.step(65534), Step::Play { conceal: 0 });
		assert_eq!(s.step(65535), Step::Play { conceal: 0 });
		assert_eq!(s.step(0), Step::Play { conceal: 0 });
		assert_eq!(s.step(65535), Step::Late);
		assert_eq!(s.step(2), Step::Play { conceal: 1 });
		// A restarted host encoder starts somewhere far behind: a new stream, not "late".
		assert_eq!(s.step(30_000), Step::Restart);
		assert_eq!(s.step(30_001), Step::Play { conceal: 0 });
	}

	#[test]
	fn libopus_round_trips_and_conceals_a_loss() {
		let mut enc =
			opus::Encoder::new(RATE, opus::Channels::Stereo, opus::Application::LowDelay).unwrap();
		let mut dec = opus::Decoder::new(RATE, opus::Channels::Stereo).unwrap();
		let frame = 480;
		let mut pkt = vec![0u8; 1500];
		let mut pcm = vec![0f32; MAX_FRAME * CHANNELS];
		for i in 0..10 {
			let n = enc.encode_float(&tone(frame, i * frame, 0.25), &mut pkt).unwrap();
			assert_eq!(dec.decode_float(&pkt[..n], &mut pcm, false).unwrap(), frame);
		}
		let peak = pcm[..frame * CHANNELS].iter().fold(0f32, |m, x| m.max(x.abs()));
		assert!(peak > 0.1, "decoded tone peak {peak}");
		assert_eq!(dec.decode_float(&[], &mut pcm[..frame * CHANNELS], false).unwrap(), frame);
	}

	/// Live, end to end: 1.5 s of a quiet 440 Hz tone (−37 dBFS, barely audible) as Opus/RTP
	/// through the real player, captured back from this process with process loopback. Needs
	/// an audio device, so it is ignored by default:
	/// `cargo test -p pulsar-tauri native_audio -- --ignored`.
	#[cfg(windows)]
	#[tokio::test(flavor = "multi_thread")]
	#[ignore]
	async fn the_player_plays_a_tone() {
		struct Level {
			sum: f64,
			n: u64,
			stop_after: usize,
			bytes: usize,
		}
		impl std::io::Write for Level {
			fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
				for s in b.chunks_exact(4) {
					let x = f32::from_le_bytes([s[0], s[1], s[2], s[3]]) as f64;
					self.sum += x * x;
					self.n += 1;
				}
				self.bytes += b.len();
				if self.bytes >= self.stop_after {
					return Err(std::io::ErrorKind::BrokenPipe.into());
				}
				Ok(b.len())
			}
			fn flush(&mut self) -> std::io::Result<()> {
				Ok(())
			}
		}
		let (tx, rx) = tokio::sync::broadcast::channel::<Vec<u8>>(1024);
		let player = start(rx);
		let feed = tokio::spawn(async move {
			let mut enc =
				opus::Encoder::new(RATE, opus::Channels::Stereo, opus::Application::LowDelay).unwrap();
			let mut buf = vec![0u8; 1500];
			let mut tick = tokio::time::interval(Duration::from_millis(10));
			for i in 0..150u16 {
				tick.tick().await;
				let n = enc.encode_float(&tone(480, i as usize * 480, 0.02), &mut buf).unwrap();
				let _ = tx.send(rtp(i, &buf[..n]));
			}
		});
		// Capture this process's output from 0.5 s in, for 0.6 s (48 kHz stereo f32).
		tokio::time::sleep(Duration::from_millis(500)).await;
		let level = tokio::task::spawn_blocking(|| {
			let mut l = Level { sum: 0.0, n: 0, stop_after: 48_000 * 8 * 6 / 10, bytes: 0 };
			let _ = pulsar_core::audio::run_process_loopback_capture_mode(
				&mut l,
				std::process::id(),
				pulsar_core::audio::ProcessLoopbackMode::Include,
				|| {},
			);
			l
		})
		.await
		.unwrap();
		feed.await.unwrap();
		drop(player);
		let rms = (level.sum / level.n.max(1) as f64).sqrt();
		let db = 20.0 * rms.max(1e-12).log10();
		eprintln!("captured {} samples, {db:.1} dBFS", level.n);
		assert!(db > -50.0, "the player's output is silent ({db:.1} dBFS)");
	}
}
