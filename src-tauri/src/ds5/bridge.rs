//! Local bridge mode: present a Bluetooth DualSense to this machine's games as a wired one.
//!
//! The physical pad (`ds5::hid` + `pulsar_core::input::ds5::pad::Pad`) and the virtual
//! wired pad (`pulsar_core::input::ds5::virtual_pad::VirtualDs5`, USB/IP) are wired
//! back to back on one thread:
//! - pad input → virtual pad input;
//! - the game's output state, haptics (USB ch 3/4) and speaker (ch 1/2, Opus-encoded here
//!   in the pad's BT format) → the pad;
//! - the pad's mic (Opus over BT) → decoded → the virtual pad's mic.
//!
//! Needs admin (usbip attach). The same pipeline serves the streaming host later, with the
//! session in the middle.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pulsar_core::input::ds5::bt_audio::{SpeakerOut, SPEAKER_OPUS_LEN};
use pulsar_core::input::ds5::haptics::Frame;
use pulsar_core::input::ds5::pad::{Pad, PadInput};
use pulsar_core::input::ds5::report::{flag, SetState};
use pulsar_core::input::ds5::speaker::{SpeakerFramer, OPUS_BITRATE, OUT_FRAMES};
use pulsar_core::input::ds5::virtual_pad::{Ds5HostEvent, VirtualDs5, VirtualDs5Config};
use pulsar_core::input::ds5::wire::Ds5Transport;
use pulsar_core::input::ds5::Ds5Features;

use super::hid::{enumerate, SdlHid};

/// Speaker blocks quieter than this (peak) are not sent: haptics-only reports cost less
/// BT airtime, and the pad's speaker stays silent anyway.
const SILENCE: f32 = 1.0 / 2048.0;
/// Keep sending speaker audio this many blocks after it went quiet (no clipped tails).
const HANGOVER: u32 = 20;

/// Pad speaker Opus encoder in the format the pad decodes: 48 kHz stereo CELT, 10 ms,
/// CBR 160 kbit/s = 200 bytes per frame.
struct SpeakerEncoder {
	framer: SpeakerFramer,
	enc: opus::Encoder,
	quiet: u32,
}

impl SpeakerEncoder {
	fn new() -> io::Result<Self> {
		let map = |e: opus::Error| io::Error::other(format!("opus: {e}"));
		let mut enc =
			opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::LowDelay)
				.map_err(map)?;
		enc.set_bitrate(opus::Bitrate::Bits(OPUS_BITRATE))
			.map_err(map)?;
		enc.set_vbr(false).map_err(map)?;
		Ok(Self {
			framer: SpeakerFramer::default(),
			enc,
			quiet: HANGOVER,
		})
	}

	/// Feed 48 kHz stereo PCM; returns one Opus frame (or `None` for silence) per
	/// completed 10.667 ms block.
	fn push(&mut self, pcm: &[f32], out: &mut VecDeque<Option<Vec<u8>>>) {
		let mut blocks = Vec::new();
		self.framer.push(pcm, &mut blocks);
		for b in blocks {
			debug_assert_eq!(b.len(), OUT_FRAMES * 2);
			let peak = b.iter().fold(0f32, |m, v| m.max(v.abs()));
			self.quiet = if peak < SILENCE {
				self.quiet.saturating_add(1)
			} else {
				0
			};
			if self.quiet > HANGOVER {
				out.push_back(None);
				continue;
			}
			let mut buf = [0u8; SPEAKER_OPUS_LEN];
			match self.enc.encode_float(&b, &mut buf) {
				Ok(n) => out.push_back(Some(buf[..n].to_vec())),
				Err(e) => {
					tracing::debug!("ds5 bridge: opus encode failed: {e}");
					out.push_back(None);
				}
			}
		}
	}
}

/// Volume and mute from the host OS (UAC feature unit), merged into every state report.
#[derive(Clone, Copy, Default)]
struct Volume {
	known: bool,
	speaker: u8,
	mic: u8,
	speaker_mute: bool,
	mic_mute: bool,
}

impl Volume {
	/// Apply onto a state report the game wrote, unless the game set these fields itself.
	fn merge(&self, s: &mut SetState) {
		if !self.known {
			return;
		}
		let b = &mut s.0;
		if b[0] & flag::B0_SPEAKER_VOLUME == 0 {
			b[0] |= flag::B0_SPEAKER_VOLUME | flag::B0_HEADPHONE_VOLUME;
			b[4] = self.speaker; // headphones
			b[5] = self.speaker; // speaker
		}
		if b[0] & flag::B0_MIC_VOLUME == 0 {
			b[0] |= flag::B0_MIC_VOLUME;
			b[6] = self.mic;
		}
		if b[1] & flag::B1_AUDIO_MUTE == 0 {
			b[1] |= flag::B1_AUDIO_MUTE;
			// byte 9: bit4 mic mute, bit5 speaker mute, bit6 headphone mute
			b[9] = (b[9] & !0x70)
				| if self.mic_mute { 0x10 } else { 0 }
				| if self.speaker_mute { 0x60 } else { 0 };
		}
	}
}

/// Run the bridge until `stop` is set (or the pad goes away). Blocks the calling thread.
pub fn run(stop: Arc<AtomicBool>, features: Ds5Features) -> io::Result<()> {
	let dev = enumerate()
		.into_iter()
		.find(|d| d.transport == Ds5Transport::Bt)
		.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no Bluetooth DualSense"))?;
	let mut hid = SdlHid::open(&dev)?;
	// The pad mic streams only while a game has the virtual mic open.
	let mut feats = Ds5Features {
		mic: false,
		..features
	};
	let (mut pad, pad_features) = Pad::open(&mut hid, Ds5Transport::Bt, feats)?;
	let (vpad, rx) = VirtualDs5::start(VirtualDs5Config::default())?;
	// Real calibration + firmware info; the pairing MAC (0x09) stays the virtual pad's own
	// so the OS doesn't treat the two as the same device.
	for (id, data) in &pad_features {
		if *id == 0x05 || *id == 0x20 {
			vpad.set_feature(*id, data);
		}
	}
	tracing::info!(
		port = vpad.port(),
		attached = vpad.attached(),
		"ds5 bridge: up"
	);

	let mut speaker = SpeakerEncoder::new()?;
	let mut mic = opus::Decoder::new(48_000, opus::Channels::Mono)
		.map_err(|e| io::Error::other(format!("opus: {e}")))?;
	let mut haptics: VecDeque<Frame> = VecDeque::new();
	let mut opus_q: VecDeque<Option<Vec<u8>>> = VecDeque::new();
	let mut speaker_on = false;
	let mut game_state = SetState::default();
	let mut volume = Volume::default();
	let mut last_input = Instant::now();
	let mut buf = [0u8; 600];
	let mut mic_pcm = [0i16; 960];
	let mut last_log = Instant::now();

	while !stop.load(Ordering::Relaxed) {
		// Game → pad.
		while let Ok(ev) = rx.try_recv() {
			match ev {
				Ds5HostEvent::Output(s) => {
					game_state = s;
					let mut m = s;
					volume.merge(&mut m);
					pad.set_output(m);
				}
				Ds5HostEvent::Haptics(f) => haptics.push_back(f),
				Ds5HostEvent::Speaker(pcm) => {
					if speaker_on && features.speaker {
						speaker.push(&pcm, &mut opus_q);
					}
				}
				Ds5HostEvent::SpeakerOpen(on) => {
					speaker_on = on;
					if !on {
						haptics.clear();
						opus_q.clear();
					}
				}
				Ds5HostEvent::MicOpen(on) => {
					feats.mic = on && features.mic;
					pad.set_features(feats);
				}
				Ds5HostEvent::Volume {
					speaker_db,
					mic_db,
					speaker_mute,
					mic_mute,
				} => {
					volume = Volume {
						known: true,
						speaker: (100 + speaker_db).clamp(0, 100) as u8,
						mic: mic_db.clamp(0, 64) as u8,
						speaker_mute,
						mic_mute,
					};
					let mut m = game_state;
					volume.merge(&mut m);
					pad.set_output(m);
				}
				Ds5HostEvent::SetFeature { id, .. } => {
					tracing::debug!("ds5 bridge: game SET_FEATURE 0x{id:02x} dropped");
				}
			}
		}
		// Pair each haptics frame with the speaker block of the same 10.667 ms slot.
		while let Some(h) = haptics.pop_front() {
			let opus = if speaker_on {
				opus_q.pop_front().flatten()
			} else {
				None
			};
			let out = SpeakerOut::Speaker;
			pad.push_slot(Some(h), opus.map(|o| (out, o)));
		}
		// Pad → game.
		let wait = pad.poll(Instant::now(), &mut hid);
		let ms = wait.as_millis().clamp(0, 1) as i32;
		match hid.read_timeout(&mut buf, ms) {
			Ok(0) => {}
			Ok(n) => match pad.on_input(&buf[..n]) {
				Some(PadInput::State(s)) => {
					vpad.set_input(&s);
					last_input = Instant::now();
				}
				Some(PadInput::Mic(packet)) => {
					if let Ok(n) = mic.decode(&packet, &mut mic_pcm[..480], false) {
						let stereo: Vec<i16> = mic_pcm[..n].iter().flat_map(|&v| [v, v]).collect();
						vpad.push_mic(&stereo);
					}
				}
				None => {}
			},
			Err(e) => {
				tracing::warn!("ds5 bridge: pad read failed ({e}); stopping");
				break;
			}
		}
		if last_input.elapsed() > Duration::from_secs(3) {
			tracing::warn!("ds5 bridge: no input from the pad for 3 s; stopping");
			break;
		}
		if last_log.elapsed() > Duration::from_secs(5) {
			last_log = Instant::now();
			tracing::info!(stats = ?pad.stats(), speaker_on, "ds5 bridge");
		}
	}
	drop(vpad); // detach before the pad handle closes
	Ok(())
}

/// `pulsar --ds5-bridge [seconds]`: run the bridge headless (developer check).
pub fn run_cli(secs: Option<u64>) -> i32 {
	let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
	let stop = Arc::new(AtomicBool::new(false));
	if let Some(s) = secs {
		let stop = stop.clone();
		std::thread::spawn(move || {
			std::thread::sleep(Duration::from_secs(s));
			stop.store(true, Ordering::Relaxed);
		});
	}
	match run(stop, Ds5Features::all()) {
		Ok(()) => {
			println!("PASS bridge stopped");
			0
		}
		Err(e) => {
			println!("FAIL bridge: {e}");
			1
		}
	}
}
