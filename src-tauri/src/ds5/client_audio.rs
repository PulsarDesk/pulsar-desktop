//! Streaming client, the audio side of a driven DualSense (kept apart from `client.rs` so
//! the pad thread there only has small call sites):
//!
//! - **Pad mic** (BT and USB): the host says when a game has the virtual pad's mic open
//!   (`DataMsg::Ds5Mic`, refreshed while on; [`MicCtl`] times it out). Only then does a BT
//!   pad stream its mic (`Pad::set_features`, mic on) — mic streaming halves a BT pad's
//!   input rate. Each Opus packet goes to the host as a `TAG_DS5` `Ds5Media::Mic` session
//!   payload through [`MediaTx`] (the only client→host raw-payload path).
//! - **USB pad audio** ([`UsbAudio`]): a cabled DualSense has its own 4-channel USB audio
//!   device (speaker ch 1/2, haptics ch 3/4) and a mic. Host slots are mixed to 48 kHz
//!   (`usb_audio::UsbSlotMixer`: haptics upsampled 3 → 48 kHz, speaker Opus decoded and
//!   stretched 480 → 512) and played through SDL3 on that device, paced by
//!   `usb_audio::UsbPacer`. The pad mic is captured from its recording endpoint and sent as
//!   10 ms mono Opus. Endpoints are found by device identity (`endpoint.rs`), never by
//!   name alone.

use std::ffi::CString;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pulsar_core::input::ds5::bt_audio::SpeakerOut;
use pulsar_core::input::ds5::haptics::{Frame, USB_CHANNELS};
use pulsar_core::input::ds5::mic::{MicFramer, MicGate, MicSeq, MIC_FRAME, MIC_RATE};
use pulsar_core::input::ds5::usb_audio::{Pace, UsbPacer, UsbSlotMixer, SLOT_FRAMES};
use pulsar_core::input::ds5::wire::Ds5Media;
use pulsar_core::input::ds5::Ds5Features;

use pulsar_core::input::ds5::pad::Pad;
use pulsar_core::input::ds5::wire::Ds5Transport;

use super::endpoint::{self, Kind};
use super::hid::Ds5Device;

/// Client → host raw session payloads (`TAG_DS5` media). Set once the session loop runs.
pub type MediaTx = Arc<Mutex<Option<tokio::sync::mpsc::Sender<Vec<u8>>>>>;

/// Feed `tx` into the session: a task sends each payload with `sender`. Call inside the
/// tokio runtime.
pub fn connect_media(tx: &MediaTx, sender: pulsar_core::SessionSender) {
	let (q, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
	tokio::spawn(async move {
		while let Some(p) = rx.recv().await {
			if sender.send(&p).await.is_err() {
				break;
			}
		}
	});
	*tx.lock().unwrap() = Some(q);
}

/// Pad-mic state of one driven pad.
#[derive(Default)]
pub struct MicCtl {
	gate: MicGate,
	seq: MicSeq,
	/// What the pad was last told (BT) / whether capture is open (USB).
	active: bool,
	sent: u64,
	dropped: u64,
}

impl MicCtl {
	/// `Ds5Mic` from the host.
	pub fn on_host(&mut self, on: bool) {
		self.gate.on_msg(on, Instant::now());
	}

	/// Whether the mic should stream now (host wants it and the user allows it). Returns
	/// `Some(new)` when that changed since the last call.
	pub fn update(&mut self, allowed: bool, now: Instant) -> Option<bool> {
		let want = allowed && self.gate.is_on(now);
		(want != self.active).then(|| {
			self.active = want;
			want
		})
	}

	pub fn active(&self) -> bool {
		self.active
	}

	/// Send one Opus packet to the host (dropped when not active or the queue is full).
	pub fn send(&mut self, media: &MediaTx, slot: u8, opus: Vec<u8>) {
		if !self.active || opus.is_empty() {
			return;
		}
		let m = Ds5Media::Mic {
			slot,
			seq: self.seq.next(),
			opus,
		};
		let ok = media
			.lock()
			.unwrap()
			.as_ref()
			.is_some_and(|q| q.try_send(m.encode()).is_ok());
		if ok {
			self.sent += 1;
		} else {
			self.dropped += 1;
		}
	}

	/// (sent, dropped) packet counters, for the stats log.
	pub fn counters(&self) -> (u64, u64) {
		(self.sent, self.dropped)
	}
}

/// Everything audio for one driven pad; the pad thread (`client.rs`) calls this at a few
/// points only.
pub struct PadAudio {
	slot: u8,
	transport: Ds5Transport,
	media: MediaTx,
	pub mic: MicCtl,
	usb: Option<UsbAudio>,
	packets: Vec<Vec<u8>>,
}

impl PadAudio {
	pub fn new(slot: u8, dev: &Ds5Device, media: MediaTx) -> Self {
		Self {
			slot,
			transport: dev.transport,
			media,
			mic: MicCtl::default(),
			usb: (dev.transport == Ds5Transport::Usb).then(|| UsbAudio::new(dev.path.clone())),
			packets: Vec::new(),
		}
	}

	/// Whether the pad's own mic stream should be on (what a BT pad's `Ds5Features::mic`
	/// passed to `Pad::set_features` must be). For other `set_features` callers (settings,
	/// auto-degrade): keep `mic` at this value or the pad mic flips.
	#[allow(dead_code)]
	pub fn pad_mic(&self) -> bool {
		self.mic.active()
	}

	/// A host media slot. A USB pad plays it here; returns false for a BT pad (the caller
	/// queues it on the `Pad`).
	pub fn on_slot(
		&mut self,
		haptics: Option<Frame>,
		speaker: Option<(SpeakerOut, Vec<u8>)>,
		f: &Ds5Features,
	) -> bool {
		match self.usb.as_mut() {
			Some(u) => {
				u.push_slot(haptics, speaker, f);
				true
			}
			None => false,
		}
	}

	/// A headset slot for a USB pad (`headset.rs`): haptics + mixed 480-frame speaker PCM.
	pub fn push_usb_pcm(
		&mut self,
		haptics: Option<Frame>,
		pcm480: Option<&[f32]>,
		f: &Ds5Features,
	) {
		if let Some(u) = self.usb.as_mut() {
			u.push_pcm(haptics.filter(|_| f.haptics), pcm480);
		}
	}

	/// Drop the USB pad's queued audio (the headset route ended: what is queued was meant
	/// for the headphones only).
	pub fn flush_usb(&mut self) {
		if let Some(o) = self.usb.as_mut().and_then(|u| u.out.as_mut()) {
			// SAFETY: live stream.
			unsafe { sdl3_sys::audio::SDL_ClearAudioStream(o.stream.0) };
			o.playing = false;
			o.pacer.reset();
			o.stream.play(false);
		}
	}

	/// A mic packet a BT pad sent.
	pub fn on_pad_mic(&mut self, opus: Vec<u8>) {
		self.mic.send(&self.media, self.slot, opus);
	}

	/// Call every loop turn. `features` = what the user lets through; mic state changes go
	/// to the BT pad here (`Pad::set_features`) or open/close the USB mic capture.
	pub fn tick(&mut self, pad: &mut Pad, features: &Ds5Features) {
		let now = Instant::now();
		if let Some(on) = self.mic.update(features.mic, now) {
			tracing::info!(slot = self.slot, on, transport = ?self.transport, "ds5 client: pad mic");
			match self.usb.as_mut() {
				Some(u) => u.set_mic(on),
				None => pad.set_features(Ds5Features {
					mic: on,
					..*features
				}),
			}
		}
		if let Some(u) = self.usb.as_mut() {
			u.tick(now);
			if self.mic.active() {
				u.mic_wanted();
				u.poll_mic(&mut self.packets);
				for p in self.packets.drain(..) {
					self.mic.send(&self.media, self.slot, p);
				}
			}
		}
	}

	/// For the stats log: (mic sent, mic dropped, usb underruns, usb flushes).
	pub fn counters(&self) -> (u64, u64, u64, u64) {
		let (s, d) = self.mic.counters();
		let (u, f) = self.usb.as_ref().map_or((0, 0), |u| u.counters());
		(s, d, u, f)
	}
}

/// Jitter buffer of a USB pad's audio stream: start after ~64 ms, flush past ~256 ms
/// (the same 6 / 24 slots as the BT pad's FIFO).
const PREBUFFER_MS: f32 = 64.0;
const MAX_MS: f32 = 256.0;
/// No slot for this long → the host stream stopped: pause and prebuffer next time.
const IDLE: Duration = Duration::from_millis(200);
/// Retry opening a missing endpoint at most this often (the pad's audio device can show
/// up a little after its HID device).
const RETRY: Duration = Duration::from_secs(5);
/// Pad mic Opus bitrate (mono voice, 10 ms frames).
const MIC_BITRATE: i32 = 32_000;

/// SDL AUDIO subsystem reference (SDL refcounts Init/Quit per subsystem).
struct SdlAudio;

impl SdlAudio {
	fn init() -> Option<Self> {
		use sdl3_sys::init::{SDL_InitSubSystem, SDL_INIT_AUDIO};
		// SAFETY: plain SDL3 call; AUDIO is safe off the main thread (native_audio.rs).
		unsafe { SDL_InitSubSystem(SDL_INIT_AUDIO) }.then_some(SdlAudio)
	}
}

impl Drop for SdlAudio {
	fn drop(&mut self) {
		// SAFETY: balances the Init above.
		unsafe { sdl3_sys::init::SDL_QuitSubSystem(sdl3_sys::init::SDL_INIT_AUDIO) }
	}
}

/// An SDL audio stream bound to one device. Destroyed (device closed) on drop.
struct Stream(*mut sdl3_sys::audio::SDL_AudioStream);

// SAFETY: SDL audio streams are internally locked; this one is used by one thread.
unsafe impl Send for Stream {}

impl Stream {
	fn open(
		dev: sdl3_sys::audio::SDL_AudioDeviceID,
		format: sdl3_sys::audio::SDL_AudioFormat,
		channels: i32,
	) -> Option<Self> {
		use sdl3_sys::audio::{SDL_AudioSpec, SDL_OpenAudioDeviceStream};
		let spec = SDL_AudioSpec {
			format,
			channels,
			freq: MIC_RATE as i32,
		};
		// SAFETY: valid spec; no callback.
		let s = unsafe { SDL_OpenAudioDeviceStream(dev, &spec, None, std::ptr::null_mut()) };
		(!s.is_null()).then_some(Stream(s))
	}

	/// The device's own channel count (0 if unknown).
	fn device_channels(&self) -> i32 {
		use sdl3_sys::audio::{
			SDL_AudioSpec, SDL_GetAudioDeviceFormat, SDL_GetAudioStreamDevice, SDL_AUDIO_F32,
		};
		let mut spec = SDL_AudioSpec {
			format: SDL_AUDIO_F32,
			channels: 0,
			freq: 0,
		};
		let mut frames = 0;
		// SAFETY: live stream; out-pointers valid.
		unsafe {
			let dev = SDL_GetAudioStreamDevice(self.0);
			if SDL_GetAudioDeviceFormat(dev, &mut spec, &mut frames) {
				spec.channels
			} else {
				0
			}
		}
	}

	fn queued(&self) -> i32 {
		// SAFETY: live stream.
		unsafe { sdl3_sys::audio::SDL_GetAudioStreamQueued(self.0) }.max(0)
	}

	fn play(&self, on: bool) {
		// SAFETY: live stream.
		unsafe {
			if on {
				sdl3_sys::audio::SDL_ResumeAudioStreamDevice(self.0);
			} else {
				sdl3_sys::audio::SDL_PauseAudioStreamDevice(self.0);
			}
		}
	}
}

impl Drop for Stream {
	fn drop(&mut self) {
		// SAFETY: destroys the stream `open` created.
		unsafe { sdl3_sys::audio::SDL_DestroyAudioStream(self.0) }
	}
}

/// Playback side of a USB pad.
struct UsbOut {
	stream: Stream,
	/// False when the device isn't 4-channel: haptics would be downmixed into the speaker.
	haptics_ok: bool,
	mixer: UsbSlotMixer,
	pacer: UsbPacer,
	ratio: f32,
	playing: bool,
	dec: opus::Decoder,
	pcm: Vec<f32>,
	spk: Vec<f32>,
}

/// Mic side of a USB pad.
struct UsbMic {
	stream: Stream,
	enc: opus::Encoder,
	framer: MicFramer,
	raw: Vec<i16>,
	blocks: Vec<[i16; MIC_FRAME]>,
}

/// Audio of one USB-connected client pad (module docs). Lives on the pad thread.
pub struct UsbAudio {
	hid_path: CString,
	out: Option<UsbOut>,
	out_tried: Option<Instant>,
	mic: Option<UsbMic>,
	mic_tried: Option<Instant>,
	last_slot: Option<Instant>,
	// Last: SDL's AUDIO subsystem must outlive the streams above.
	_sdl: Option<SdlAudio>,
}

impl UsbAudio {
	pub fn new(hid_path: CString) -> Self {
		Self {
			hid_path,
			out: None,
			out_tried: None,
			mic: None,
			mic_tried: None,
			last_slot: None,
			_sdl: SdlAudio::init(),
		}
	}

	fn due(tried: Option<Instant>) -> bool {
		tried.is_none_or(|t| t.elapsed() >= RETRY)
	}

	fn open_out(&mut self) -> Option<&mut UsbOut> {
		if self.out.is_none() && self._sdl.is_some() && Self::due(self.out_tried) {
			self.out_tried = Some(Instant::now());
			self.out = endpoint::find(&self.hid_path, Kind::Playback).and_then(|ep| {
				let stream = Stream::open(ep.id, sdl3_sys::audio::SDL_AUDIO_F32, USB_CHANNELS as i32)?;
				let ch = stream.device_channels();
				let haptics_ok = ch == USB_CHANNELS as i32;
				if !haptics_ok {
					tracing::warn!(channels = ch, name = %ep.name, "ds5 usb audio: pad endpoint isn't 4-channel; speaker only, no haptics");
				}
				let dec = opus::Decoder::new(48_000, opus::Channels::Stereo).ok()?;
				tracing::info!(name = %ep.name, channels = ch, "ds5 usb audio: playing on the pad");
				Some(UsbOut {
					stream,
					haptics_ok,
					mixer: UsbSlotMixer::default(),
					pacer: UsbPacer::new(PREBUFFER_MS, MAX_MS),
					ratio: 1.0,
					playing: false,
					dec,
					pcm: Vec::with_capacity(SLOT_FRAMES * USB_CHANNELS),
					spk: vec![0.0; 960],
				})
			});
		}
		self.out.as_mut()
	}

	/// One host slot for this pad.
	pub fn push_slot(
		&mut self,
		haptics: Option<Frame>,
		speaker: Option<(SpeakerOut, Vec<u8>)>,
		f: &Ds5Features,
	) {
		let haptics = haptics.filter(|_| f.haptics);
		let speaker = speaker.filter(|_| f.speaker);
		if haptics.is_none() && speaker.is_none() {
			return;
		}
		let Some(o) = self.open_out() else {
			self.last_slot = Some(Instant::now());
			return;
		};
		let spk =
			speaker.and_then(
				|(_, opus)| match o.dec.decode_float(&opus, &mut o.spk, false) {
					Ok(n) => Some(o.spk[..n * 2].to_vec()),
					Err(e) => {
						tracing::debug!("ds5 usb audio: speaker decode failed: {e}");
						None
					}
				},
			);
		self.push_pcm(haptics, spk.as_deref());
	}

	/// One slot as decoded PCM: haptics and 480-frame stereo speaker audio (each optional).
	pub fn push_pcm(&mut self, haptics: Option<Frame>, speaker480: Option<&[f32]>) {
		if haptics.is_none() && speaker480.is_none() {
			return;
		}
		self.last_slot = Some(Instant::now());
		let Some(o) = self.open_out() else {
			return;
		};
		let haptics = haptics.filter(|_| o.haptics_ok);
		o.pcm.clear();
		o.mixer.mix(haptics.as_ref(), speaker480, &mut o.pcm);
		let bytes_per_ms = (48 * USB_CHANNELS * 4) as f32;
		let before = o.stream.queued() as f32 / bytes_per_ms;
		// SAFETY: live stream; the buffer is valid for its byte length.
		unsafe {
			sdl3_sys::audio::SDL_PutAudioStreamData(
				o.stream.0,
				o.pcm.as_ptr().cast(),
				(o.pcm.len() * 4) as i32,
			);
		}
		let after = o.stream.queued() as f32 / bytes_per_ms;
		match o.pacer.on_push(before, after) {
			Pace::Buffer => {
				if o.playing {
					o.playing = false;
					o.stream.play(false);
				}
			}
			Pace::Play(r) => {
				if (r - o.ratio).abs() > f32::EPSILON {
					o.ratio = r;
					// SAFETY: live stream.
					unsafe { sdl3_sys::audio::SDL_SetAudioStreamFrequencyRatio(o.stream.0, r) };
				}
				if !o.playing {
					o.playing = true;
					o.stream.play(true);
				}
			}
			Pace::Flush => {
				// SAFETY: live stream.
				unsafe { sdl3_sys::audio::SDL_ClearAudioStream(o.stream.0) };
				o.playing = false;
				o.stream.play(false);
			}
		}
	}

	/// Housekeeping from the pad loop: pause once the host stream stopped.
	pub fn tick(&mut self, now: Instant) {
		if let (Some(o), Some(t)) = (self.out.as_mut(), self.last_slot) {
			if o.playing && now.duration_since(t) > IDLE && o.stream.queued() == 0 {
				o.playing = false;
				o.pacer.reset();
				o.stream.play(false);
			}
		}
	}

	/// (underruns, flushes) of the playback jitter buffer.
	pub fn counters(&self) -> (u64, u64) {
		self.out
			.as_ref()
			.map_or((0, 0), |o| (o.pacer.underruns, o.pacer.flushes))
	}

	/// Open or close the pad's mic capture.
	pub fn set_mic(&mut self, on: bool) {
		if !on {
			if self.mic.take().is_some() {
				tracing::info!("ds5 usb audio: pad mic closed");
			}
			self.mic_tried = None;
			return;
		}
		if self.mic.is_some() || self._sdl.is_none() || !Self::due(self.mic_tried) {
			return;
		}
		self.mic_tried = Some(Instant::now());
		self.mic = endpoint::find(&self.hid_path, Kind::Recording).and_then(|ep| {
			let stream = Stream::open(ep.id, sdl3_sys::audio::SDL_AUDIO_S16, 1)?;
			let mut enc =
				opus::Encoder::new(MIC_RATE, opus::Channels::Mono, opus::Application::Voip).ok()?;
			enc.set_bitrate(opus::Bitrate::Bits(MIC_BITRATE)).ok()?;
			stream.play(true);
			tracing::info!(name = %ep.name, "ds5 usb audio: pad mic open");
			Some(UsbMic {
				stream,
				enc,
				framer: MicFramer::default(),
				raw: vec![0; MIC_FRAME * 4],
				blocks: Vec::new(),
			})
		});
	}

	/// Retry a mic that failed to open while it is still wanted.
	pub fn mic_wanted(&mut self) {
		if self.mic.is_none() {
			self.set_mic(true);
		}
	}

	/// Captured mic audio as 10 ms Opus packets.
	pub fn poll_mic(&mut self, out: &mut Vec<Vec<u8>>) {
		let Some(m) = self.mic.as_mut() else {
			return;
		};
		loop {
			// SAFETY: live stream; the buffer is valid for its byte length.
			let n = unsafe {
				sdl3_sys::audio::SDL_GetAudioStreamData(
					m.stream.0,
					m.raw.as_mut_ptr().cast(),
					(m.raw.len() * 2) as i32,
				)
			};
			if n <= 0 {
				break;
			}
			m.framer.push(&m.raw[..n as usize / 2], &mut m.blocks);
		}
		for b in m.blocks.drain(..) {
			let mut buf = [0u8; 256];
			match m.enc.encode(&b, &mut buf) {
				Ok(n) => out.push(buf[..n].to_vec()),
				Err(e) => tracing::debug!("ds5 usb audio: mic encode failed: {e}"),
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn mic_ctl_follows_host_and_setting() {
		let mut m = MicCtl::default();
		let now = Instant::now();
		assert_eq!(m.update(true, now), None);
		m.on_host(true);
		assert_eq!(
			m.update(false, Instant::now()),
			None,
			"user turned the mic off"
		);
		assert_eq!(m.update(true, Instant::now()), Some(true));
		assert_eq!(m.update(true, Instant::now()), None);
		m.on_host(false);
		assert_eq!(m.update(true, Instant::now()), Some(false));
	}

	#[test]
	fn mic_packets_reach_the_media_queue_only_while_active() {
		let tx: MediaTx = Arc::default();
		let (q, mut rx) = tokio::sync::mpsc::channel(8);
		*tx.lock().unwrap() = Some(q);
		let mut m = MicCtl::default();
		m.send(&tx, 1, vec![1, 2, 3]);
		assert!(rx.try_recv().is_err(), "not active: dropped");
		m.on_host(true);
		m.update(true, Instant::now());
		m.send(&tx, 1, vec![1, 2, 3]);
		m.send(&tx, 1, vec![4]);
		let a = Ds5Media::decode(&rx.try_recv().unwrap()).unwrap();
		let b = Ds5Media::decode(&rx.try_recv().unwrap()).unwrap();
		assert_eq!(
			a,
			Ds5Media::Mic {
				slot: 1,
				seq: 0,
				opus: vec![1, 2, 3]
			}
		);
		assert_eq!(
			b,
			Ds5Media::Mic {
				slot: 1,
				seq: 1,
				opus: vec![4]
			}
		);
		assert_eq!(m.counters(), (2, 0));
	}

	/// The USB mic path end to end without hardware: 10 ms blocks → Opus → the host's
	/// mono decoder → stereo for the virtual pad.
	#[test]
	fn usb_mic_opus_roundtrip() {
		let mut enc =
			opus::Encoder::new(MIC_RATE, opus::Channels::Mono, opus::Application::Voip).unwrap();
		enc.set_bitrate(opus::Bitrate::Bits(MIC_BITRATE)).unwrap();
		let mut dec = opus::Decoder::new(48_000, opus::Channels::Mono).unwrap();
		let mut framer = MicFramer::default();
		let tone: Vec<i16> = (0..4800)
			.map(|i| ((i as f32 * 0.05).sin() * 8000.0) as i16)
			.collect();
		let mut blocks = Vec::new();
		framer.push(&tone, &mut blocks);
		assert_eq!(blocks.len(), 10);
		let mut energy = 0f64;
		for b in &blocks {
			let mut buf = [0u8; 256];
			let n = enc.encode(b, &mut buf).unwrap();
			assert!(n > 0 && n < 256);
			let mut pcm = [0i16; 960];
			let k = dec.decode(&buf[..n], &mut pcm[..480], false).unwrap();
			assert_eq!(k, 480);
			let st = pulsar_core::input::ds5::mic::mono_to_stereo(&pcm[..k]);
			assert_eq!(st.len(), 960);
			energy += st.iter().map(|&v| (v as f64).powi(2)).sum::<f64>();
		}
		assert!(energy > 0.0, "decoded tone is not silence");
	}
}
