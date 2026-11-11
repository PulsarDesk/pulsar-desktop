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

use pulsar_core::input::ds5::bt_audio::SpeakerOut;
use pulsar_core::input::ds5::haptics::Frame;
use pulsar_core::input::ds5::pad::{Pad, PadInput};
use pulsar_core::input::ds5::report::SetState;
use pulsar_core::input::ds5::virtual_pad::{Ds5HostEvent, VirtualDs5, VirtualDs5Config};
use pulsar_core::input::ds5::wire::Ds5Transport;
use pulsar_core::input::ds5::Ds5Features;

use super::audio::{SpeakerEncoder, Volume};
use super::hid::{enumerate, SdlHid};

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
					volume = Volume::from_db(speaker_db, mic_db, speaker_mute, mic_mute);
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
