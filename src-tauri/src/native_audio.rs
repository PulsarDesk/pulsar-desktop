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
//! audio stream. The SDL queue is a sawtooth (+10 ms per packet, −one device buffer per device
//! pull); its LOW point is held near a margin (`PULSAR_AUDIO_TARGETMS`, default 15 ms) by
//! nudging the playback rate (slowly, at most ±2 %; host and client clocks drift apart by far
//! less). Playback starts once margin + one device buffer is queued; a queue that runs dry
//! (late packets: Wi-Fi jitter) raises the margin by 10 ms, up to 120 ms, and it eases back
//! after 20 s without one. A queue far past the target is flushed and re-buffered, so latency
//! stays bounded. SDL is asked for a 10 ms device buffer.
//!
//! The old steering held the queue right after each push near 30 ms and moved the rate up to
//! ±3 % per packet. On a laptop whose device pulled ~100 ms at a time that could never hold:
//! measured with a 1 kHz tone through a real session, it played 1030 Hz (+3 %, half a
//! semitone) the whole run, with 114 dropouts and 478 clicks in 30 s — the "crackle".
//!
//! The SDL device opens on the first packet, so a session without audio never holds it.
//!
//! Headphones in a driven DualSense's jack (`ds5/headset.rs`): each decoded packet goes to
//! every routed pad of the session instead (`Route`), and the client output stays silent; while a pad has the
//! headphones in but its route is shed, packets are dropped (no sound anywhere else). The
//! switch fades the last local packet out, and the first one after the route ends back in;
//! the device is re-buffered on return so the dry-queue logic doesn't count the pause.
//!
//! Headphones in a USB DualShock 4's jack (`ds4_headset.rs`, experimental): local playback
//! moves to the pad's own audio device; the SDL stream is reopened on the switch.

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
pub fn start(mut rx: tokio::sync::broadcast::Receiver<Vec<u8>>, play_id: u64) -> NativeAudio {
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
		.spawn(move || run(packets, flag, play_id));
	if let Err(e) = spawned {
		tracing::warn!("audio: player thread failed to start: {e}");
	}
	NativeAudio { stop }
}

/// The Opus payload and sequence number of an RTP datagram (header, CSRCs, extension and
/// padding skipped). `None` for anything that is not RTP or carries no payload.
pub(crate) fn rtp_opus(pkt: &[u8]) -> Option<(u16, &[u8])> {
	rtp_audio(pkt).map(|(_, seq, payload)| (seq, payload))
}

/// [`rtp_opus`] plus the payload type: 97 = Opus, 98 = uncompressed L16
/// (`pulsar_core::audio::{OPUS,L16}_PAYLOAD_TYPE`).
pub(crate) fn rtp_audio(pkt: &[u8]) -> Option<(u8, u16, &[u8])> {
	if pkt.len() < 12 || pkt[0] >> 6 != 2 {
		return None;
	}
	let pt = pkt[1] & 0x7f;
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
	(!payload.is_empty()).then_some((pt, seq, payload))
}

/// Decode an L16 payload (16-bit big-endian, interleaved stereo) into `out`; returns frames.
fn decode_l16(payload: &[u8], out: &mut [f32]) -> usize {
	let n = (payload.len() / 2).min(out.len()) / CHANNELS * CHANNELS;
	for (o, b) in out[..n].iter_mut().zip(payload.chunks_exact(2)) {
		*o = i16::from_be_bytes([b[0], b[1]]) as f32 / 32768.0;
	}
	n / CHANNELS
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

/// Where decoded audio goes: the client output or a DualSense's headset jack (module docs).
struct Route {
	play_id: u64,
	/// Audio currently goes elsewhere (pad, or dropped).
	away: bool,
	/// The next local packet fades in.
	fade_in: bool,
	/// The SDL device the open local stream plays on (0 = the default output).
	dev: u32,
}

/// Fade length on a switch, frames (5 ms).
const FADE_FRAMES: usize = 240;

/// Scale the first `FADE_FRAMES` frames up from 0 (`up`) or the whole packet's tail down.
fn fade(pcm: &mut [f32], up: bool) {
	let frames = pcm.len() / CHANNELS;
	let n = FADE_FRAMES.min(frames);
	if up {
		for i in 0..n {
			let g = i as f32 / n as f32;
			for c in 0..CHANNELS {
				pcm[i * CHANNELS + c] *= g;
			}
		}
	} else {
		let start = frames - n;
		for i in 0..n {
			let g = 1.0 - (i + 1) as f32 / n as f32;
			for c in 0..CHANNELS {
				pcm[(start + i) * CHANNELS + c] *= g;
			}
		}
	}
}

impl Route {
	/// Deliver one decoded packet. Opens the SDL device only for local playback.
	fn emit(&mut self, out: &mut Option<SdlOut>, pcm: &mut [f32]) {
		if pcm.is_empty() {
			return;
		}
		match crate::ds5::headset::tap(self.play_id) {
			crate::ds5::headset::Tap::Local => {
				if self.away {
					self.away = false;
					self.fade_in = true;
					if let Some(s) = out.as_mut() {
						s.rebuffer();
					}
					tracing::info!("audio: back on the client output");
				}
				// A DS4's headset jack took over (or gave back) the local output: reopen there.
				let want = crate::ds4_headset::target().unwrap_or(0);
				if out.is_some() && self.dev != want {
					tracing::info!(device = want, "audio: local output moves (DS4 headset jack)");
					*out = None;
					self.fade_in = true;
				}
				if out.is_none() {
					*out = SdlOut::open(want);
					self.dev = want;
					if out.is_none() && want != 0 {
						*out = SdlOut::open(0); // the pad's device failed: default output
						self.dev = 0;
					}
				}
				if self.fade_in {
					self.fade_in = false;
					fade(pcm, true);
				}
				if let Some(s) = out.as_mut() {
					s.push(pcm);
				}
			}
			tap => {
				if !self.away {
					self.away = true;
					tracing::info!("audio: client output silenced (DualSense headset jack)");
					// The last local packet fades out; the pad's route fades in on its own.
					if let Some(s) = out.as_mut() {
						fade(pcm, false);
						s.push(pcm);
						return;
					}
				}
				if let crate::ds5::headset::Tap::Pads(txs) = tap {
					for tx in &txs {
						crate::ds5::headset::send(tx, pcm);
					}
				}
			}
		}
	}
}

fn run(packets: mpsc::Receiver<Vec<u8>>, stop: Arc<AtomicBool>, play_id: u64) {
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
	// Packet arrival times: the largest gap per stats window tells sender/network bursts apart
	// from device-side chunking when the queue swings.
	let mut last_arrival: Option<std::time::Instant> = None;
	// The last uncompressed packet (L16), replayed (faded) for a lost one.
	let mut last_l16: Vec<f32> = Vec::new();
	let mut route = Route {
		play_id,
		away: false,
		fade_in: false,
		dev: 0,
	};
	// Counters kept while no local device is open (the stats live on the device).
	let mut spare = Stats::default();
	while !stop.load(Ordering::Relaxed) {
		let pkt = match packets.recv_timeout(Duration::from_millis(100)) {
			Ok(p) => p,
			Err(mpsc::RecvTimeoutError::Timeout) => continue,
			Err(mpsc::RecvTimeoutError::Disconnected) => break,
		};
		let Some((pt, n, payload)) = rtp_audio(&pkt) else {
			continue;
		};
		if out.is_none() && matches!(crate::ds5::headset::tap(play_id), crate::ds5::headset::Tap::Local) {
			route.dev = crate::ds4_headset::target().unwrap_or(0);
			out = SdlOut::open(route.dev);
			if out.is_none() && route.dev != 0 {
				route.dev = 0;
				out = SdlOut::open(0);
			}
			if out.is_none() {
				return; // logged by open()
			}
		}
		macro_rules! stats {
			() => {
				match out.as_mut() {
					Some(s) => &mut s.stats,
					None => &mut spare,
				}
			};
		}
		match seq.step(n) {
			Step::Late => {
				stats!().late += 1;
				continue;
			}
			Step::Restart => {
				stats!().restarts += 1;
				let _ = decoder.reset_state();
			}
			Step::Play { conceal } => {
				for _ in 0..conceal {
					if pt == pulsar_core::audio::L16_PAYLOAD_TYPE {
						// Uncompressed: no codec PLC — replay the last packet, halved each time, so
						// a lost one is a dip rather than a hard gap.
						for x in last_l16.iter_mut() {
							*x *= 0.5;
						}
						let mut l = last_l16.clone();
						route.emit(&mut out, &mut l);
						concealed += 1;
						stats!().concealed += 1;
					} else if let Ok(got) = decoder.decode_float(&[], &mut pcm[..frame * CHANNELS], false) {
						route.emit(&mut out, &mut pcm[..got * CHANNELS]);
						concealed += 1;
						stats!().concealed += 1;
					}
				}
			}
		}
		stats!().packets += 1;
		let now = std::time::Instant::now();
		if let Some(prev) = last_arrival.replace(now) {
			let gap = now.duration_since(prev).as_secs_f32() * 1000.0;
			let st = stats!();
			st.gap_max_ms = st.gap_max_ms.max(gap);
		}
		if pt == pulsar_core::audio::L16_PAYLOAD_TYPE {
			stats!().raw = true;
			let got = decode_l16(payload, &mut pcm);
			if got > 0 {
				frame = got.min(MAX_FRAME);
				last_l16.clear();
				last_l16.extend_from_slice(&pcm[..got * CHANNELS]);
				route.emit(&mut out, &mut pcm[..got * CHANNELS]);
			}
			continue;
		}
		match decoder.decode_float(payload, &mut pcm, false) {
			Ok(got) => {
				if got > 0 {
					frame = got.min(MAX_FRAME);
				}
				route.emit(&mut out, &mut pcm[..got * CHANNELS]);
			}
			Err(e) => tracing::debug!("audio: undecodable Opus packet: {e}"),
		}
	}
	tracing::info!(concealed, "audio: native player stopped");
}

/// Per-window player counters, logged every [`STATS_EVERY`]. `dry` = the queue ran empty and
/// the device played silence (a crackle); `in_hz` = decoded frames per wall-clock second.
#[derive(Default)]
struct Stats {
	packets: u32,
	late: u32,
	restarts: u32,
	concealed: u32,
	frames: u64,
	dry: u32,
	flushes: u32,
	gap_max_ms: f32,
	/// The window carried uncompressed (L16) audio.
	raw: bool,
	q_min_ms: Option<f32>,
	q_max_ms: f32,
	ratio_min: Option<f32>,
	ratio_max: f32,
}

const STATS_EVERY: Duration = Duration::from_secs(5);
/// Largest playback-rate correction (±2 %). Real clock drift stays well under 0.1 %, so a
/// healthy stream sits near 1.0 (the gain is slow); the headroom is for a source that does not
/// run in real time — measured 2–3 % off on a host capturing a virtual sink — where a slight
/// pitch bend beats a flush (a gap) every few seconds.
const MAX_RATE_ADJ: f32 = 0.02;
/// Queue error (ms) inside which the rate stays exactly 1.0.
const DEADBAND_MS: f32 = 3.0;
/// Rate change per ms of smoothed queue error past the deadband: 10 ms over → +0.5 %.
const GAIN_PER_MS: f32 = 0.0005;
/// Target growth on a dry queue, its ceiling, and how long without one before it eases back.
const DRY_STEP_MS: f32 = 10.0;
const MAX_TARGET_MS: f32 = 120.0;
const EASE_AFTER: Duration = Duration::from_secs(20);

/// Device buffer asked of SDL (10 ms). A device that pulls big chunks needs that much queued
/// at every pull; the laptop this was measured on pulled ~100 ms at a time by default.
const DEVICE_FRAMES: &std::ffi::CStr = c"480";
/// Pushes per steering update (≈ 0.25 s at 10 ms packets).
const STEER_EVERY: u32 = 25;

/// SDL3 playback stream with the drift and latency bound described in the module docs.
struct SdlOut {
	stream: *mut sdl3_sys::audio::SDL_AudioStream,
	bytes_per_ms: f32,
	/// The device's buffer (what it pulls at a time), ms.
	device_ms: f32,
	base_target_ms: f32,
	/// Margin the queue's LOW point is held at (its high point is this + one device pull).
	target_ms: f32,
	/// Smoothed low point of the queue (ms), the steering input.
	level_ms: Option<f32>,
	/// Lowest queue level seen since the last steering update.
	win_min_ms: f32,
	win_pushes: u32,
	ratio: f32,
	/// Playing; false = device paused while the queue fills to the target.
	playing: bool,
	last_dry: std::time::Instant,
	stats: Stats,
	stats_at: std::time::Instant,
}

impl SdlOut {
	/// Open on SDL playback device `dev` (0 = the default output).
	fn open(dev: u32) -> Option<SdlOut> {
		use sdl3_sys::audio::{
			SDL_AudioSpec, SDL_OpenAudioDeviceStream, SDL_AUDIO_DEVICE_DEFAULT_PLAYBACK,
			SDL_AUDIO_F32,
		};
		use sdl3_sys::init::{SDL_InitSubSystem, SDL_QuitSubSystem, SDL_INIT_AUDIO};
		// SAFETY: plain SDL3 C calls; AUDIO is its own subsystem and safe off the main thread.
		unsafe {
			// A small device buffer: SDL "will try" (not every backend honors it); the real size
			// is read back below and the queue sized from it either way.
			sdl3_sys::hints::SDL_SetHint(
				sdl3_sys::hints::SDL_HINT_AUDIO_DEVICE_SAMPLE_FRAMES,
				DEVICE_FRAMES.as_ptr(),
			);
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
				if dev == 0 { SDL_AUDIO_DEVICE_DEFAULT_PLAYBACK } else { sdl3_sys::audio::SDL_AudioDeviceID(dev) },
				&spec,
				None,
				std::ptr::null_mut(),
			);
			if stream.is_null() {
				tracing::warn!("audio: SDL_OpenAudioDeviceStream failed — no client audio");
				SDL_QuitSubSystem(SDL_INIT_AUDIO);
				return None;
			}
			// The device opens paused and stays so until the prebuffer is in (see push).
			let target_ms: f32 = std::env::var("PULSAR_AUDIO_TARGETMS")
				.ok()
				.and_then(|s| s.parse().ok())
				.unwrap_or(15.0f32)
				.max(5.0);
			let mut dev_spec = SDL_AudioSpec { format: SDL_AUDIO_F32, channels: 0, freq: 0 };
			let mut dev_frames: std::ffi::c_int = 0;
			let dev = sdl3_sys::audio::SDL_GetAudioStreamDevice(stream);
			let device_ms = if sdl3_sys::audio::SDL_GetAudioDeviceFormat(dev, &mut dev_spec, &mut dev_frames)
				&& dev_spec.freq > 0
				&& dev_frames > 0
			{
				dev_frames as f32 * 1000.0 / dev_spec.freq as f32
			} else {
				10.0
			};
			tracing::info!(
				target_ms,
				device_ms,
				device_hz = dev_spec.freq,
				device_frames = dev_frames,
				"audio: native player up (libopus + SDL3)"
			);
			let now = std::time::Instant::now();
			Some(SdlOut {
				stream,
				bytes_per_ms: (RATE as usize / 1000 * CHANNELS * 4) as f32,
				device_ms,
				base_target_ms: target_ms,
				target_ms,
				level_ms: None,
				win_min_ms: f32::MAX,
				win_pushes: 0,
				ratio: 1.0,
				playing: false,
				last_dry: now,
				stats: Stats::default(),
				stats_at: now,
			})
		}
	}

	/// Audio is coming back after a pause (headset route ended): drop whatever is left and
	/// prebuffer again, without counting the pause as a dry queue.
	fn rebuffer(&mut self) {
		// SAFETY: as in queued_ms.
		unsafe {
			sdl3_sys::audio::SDL_ClearAudioStream(self.stream);
		}
		self.set_ratio(1.0);
		self.set_playing(false);
		self.level_ms = None;
	}

	fn queued_ms(&self) -> f32 {
		// SAFETY: `stream` is a live SDL audio stream owned by `self`.
		let q = unsafe { sdl3_sys::audio::SDL_GetAudioStreamQueued(self.stream) };
		q.max(0) as f32 / self.bytes_per_ms
	}

	fn set_ratio(&mut self, ratio: f32) {
		// Tiny steps aren't worth a resampler update; a return to exactly 1.0 always is.
		if (ratio - self.ratio).abs() < 0.0002 && !(ratio == 1.0 && self.ratio != 1.0) {
			return;
		}
		self.ratio = ratio;
		// SAFETY: as in queued_ms.
		unsafe {
			sdl3_sys::audio::SDL_SetAudioStreamFrequencyRatio(self.stream, ratio);
		}
	}

	fn set_playing(&mut self, on: bool) {
		use sdl3_sys::audio::{SDL_PauseAudioStreamDevice, SDL_ResumeAudioStreamDevice};
		if self.playing == on {
			return;
		}
		self.playing = on;
		// SAFETY: as in queued_ms.
		unsafe {
			if on {
				SDL_ResumeAudioStreamDevice(self.stream);
			} else {
				SDL_PauseAudioStreamDevice(self.stream);
			}
		}
	}

	/// Queue decoded interleaved PCM and steer the queue toward the target (module docs).
	fn push(&mut self, pcm: &[f32]) {
		use sdl3_sys::audio::{SDL_ClearAudioStream, SDL_PutAudioStreamData};
		if pcm.is_empty() {
			return;
		}
		let now = std::time::Instant::now();
		let before = self.queued_ms();
		if self.playing && before <= 0.0 {
			// Ran dry: the device has been playing silence — a crackle. Buffer more.
			self.stats.dry += 1;
			self.target_ms = (self.target_ms + DRY_STEP_MS).min(MAX_TARGET_MS);
			self.last_dry = now;
		} else if self.target_ms > self.base_target_ms && now.duration_since(self.last_dry) > EASE_AFTER {
			self.target_ms = (self.target_ms - 5.0).max(self.base_target_ms);
			self.last_dry = now;
		}
		// SAFETY: as in queued_ms; SDL copies the buffer.
		unsafe {
			SDL_PutAudioStreamData(
				self.stream,
				pcm.as_ptr() as *const std::ffi::c_void,
				std::mem::size_of_val(pcm) as i32,
			);
		}
		self.stats.frames += (pcm.len() / CHANNELS) as u64;
		let after = self.queued_ms();
		// The queue is a sawtooth: +10 ms per packet, −one device buffer per device pull. What
		// must not hit zero is its LOW point, so that is what gets steered.
		let full = self.target_ms + self.device_ms;
		if !self.playing {
			if after >= full {
				self.level_ms = None;
				self.win_min_ms = f32::MAX;
				self.win_pushes = 0;
				self.set_playing(true);
			}
		} else if after > full + 150.0 {
			// Far past the target (a burst after a stall): drop it and re-buffer rather than
			// play all of it late.
			// SAFETY: as in queued_ms.
			unsafe {
				SDL_ClearAudioStream(self.stream);
			}
			self.stats.flushes += 1;
			self.set_ratio(1.0);
			self.set_playing(false);
		} else {
			self.win_min_ms = self.win_min_ms.min(before);
			self.win_pushes += 1;
			if self.win_pushes >= STEER_EVERY {
				let low = self.win_min_ms;
				let level = match self.level_ms {
					Some(l) => l + 0.3 * (low - l),
					None => low,
				};
				self.level_ms = Some(level);
				self.win_min_ms = f32::MAX;
				self.win_pushes = 0;
				let err = level - self.target_ms;
				let adj = if err.abs() <= DEADBAND_MS {
					0.0
				} else {
					((err - DEADBAND_MS.copysign(err)) * GAIN_PER_MS).clamp(-MAX_RATE_ADJ, MAX_RATE_ADJ)
				};
				self.set_ratio(1.0 + adj);
			}
		}
		let ratio = self.ratio;
		let st = &mut self.stats;
		st.q_min_ms = Some(st.q_min_ms.map_or(before, |m| m.min(before)));
		st.q_max_ms = st.q_max_ms.max(after);
		st.ratio_min = Some(st.ratio_min.map_or(ratio, |m| m.min(ratio)));
		st.ratio_max = st.ratio_max.max(ratio);
		let span = now.duration_since(self.stats_at);
		if span >= STATS_EVERY {
			tracing::info!(
				packets = st.packets,
				late = st.late,
				restarts = st.restarts,
				concealed = st.concealed,
				in_hz = (st.frames as f32 / span.as_secs_f32()) as u32,
				dry = st.dry,
				flushes = st.flushes,
				gap_max_ms = st.gap_max_ms as u32,
				codec = if st.raw { "l16" } else { "opus" },
				q_ms = format!("{:.0}-{:.0}", st.q_min_ms.unwrap_or(0.0), st.q_max_ms),
				target_ms = self.target_ms,
				ratio = format!("{:.4}-{:.4}", st.ratio_min.unwrap_or(1.0), st.ratio_max),
				"audio: player stats"
			);
			self.stats = Stats::default();
			self.stats_at = now;
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
	fn fades_ramp_in_and_out() {
		let mut a = vec![1.0f32; 480 * CHANNELS];
		fade(&mut a, true);
		assert_eq!((a[0], a[1]), (0.0, 0.0));
		assert!(a[(FADE_FRAMES / 2) * CHANNELS] > 0.4 && a[(FADE_FRAMES / 2) * CHANNELS] < 0.6);
		assert_eq!(a[FADE_FRAMES * CHANNELS], 1.0);
		let mut b = vec![1.0f32; 480 * CHANNELS];
		fade(&mut b, false);
		assert_eq!(b[0], 1.0);
		assert_eq!(*b.last().unwrap(), 0.0);
		// Shorter than the fade: the whole packet ramps.
		let mut c = vec![1.0f32; 10 * CHANNELS];
		fade(&mut c, false);
		assert_eq!(c[c.len() - 1], 0.0);
		assert!(c[0] > 0.8);
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
	fn l16_packets_carry_their_payload_type_and_decode_to_floats() {
		let mut pkt = rtp(7, &[0x40, 0x00, 0xc0, 0x00, 0x7f, 0xff, 0x80, 0x00]);
		pkt[1] = pulsar_core::audio::L16_PAYLOAD_TYPE;
		let (pt, seq, payload) = rtp_audio(&pkt).unwrap();
		assert_eq!((pt, seq), (pulsar_core::audio::L16_PAYLOAD_TYPE, 7));
		let mut out = [0f32; 8];
		assert_eq!(decode_l16(payload, &mut out), 2, "two stereo frames");
		assert_eq!(&out[..4], &[0.5, -0.5, 32767.0 / 32768.0, -1.0]);
		// A trailing odd byte or half frame is dropped, never read past.
		assert_eq!(decode_l16(&payload[..5], &mut out), 1);
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
		let player = start(rx, 0);
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
