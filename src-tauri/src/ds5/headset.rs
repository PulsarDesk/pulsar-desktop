//! Streaming client: the session's audio through headphones in the pad's jack ("Kolun
//! kulaklık girişi", PS5 gibi). Pure logic in `pulsar_core::input::ds5::headset`; design in
//! pulsar-core `docs/dualsense.md` ("Headset jack").
//!
//! - The pad thread (`client.rs`) owns a [`PadHeadset`]: it reads the jack bits from every
//!   input report, decides the route (`JackSwitch`), and while routed claims the play
//!   session's audio here ([`tap`]), mixes it with the host's pad slots (`HeadsetMix`) and
//!   queues the result on the pad (BT: Opus in 0x39 reports to block 0x16; USB: channels
//!   1/2 of the pad's own audio device, which the routing state sends to the jack).
//! - The client's player (`native_audio.rs`) asks [`tap`] for every decoded packet: play it
//!   locally, hand it to the pad, or drop it. While any pad has the headphones in and the
//!   route is on (or shed), Pulsar plays nothing on the client output (maintainer: no sound
//!   anywhere else while the headphones are in; Pulsar has no UI sounds of its own).
//!
//! One pad per play session carries the session audio (the first one plugged in); a second
//! pad with headphones only gets the game's pad-speaker stream on its jack.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Mutex;
use std::time::Instant;

use pulsar_core::input::ds5::bt_audio::{SpeakerOut, SPEAKER_OPUS_LEN};
use pulsar_core::input::ds5::haptics::Frame;
use pulsar_core::input::ds5::headset::{HeadsetMix, JackSwitch, MixedSlot, Route, Switch};
use pulsar_core::input::ds5::pad::Pad;
use pulsar_core::input::ds5::report::{Jack, USB_INPUT_LEN};
use pulsar_core::input::ds5::speaker::{OPUS_BITRATE, OUT_FRAMES};
use pulsar_core::input::ds5::wire::Ds5Transport;
use pulsar_core::input::ds5::Ds5Features;

use super::audio::{HANGOVER, SILENCE};
use super::client_audio::PadAudio;

/// Decoded session packets queued between the player and the pad thread (≈ 0.6 s at
/// 10 ms packets; the pad thread drains it every ≤ 2 ms).
const QUEUE: usize = 64;

/// Where the client's player sends one decoded packet.
pub enum Tap {
	/// Play it on the client output (no headset route).
	Local,
	/// Send it to the pad that carries this session's audio.
	Pad(SyncSender<Vec<f32>>),
	/// Drop it: headphones are in a pad but the route is shed (or another session's pad
	/// has them): no sound anywhere else.
	Silent,
}

#[derive(Default)]
struct Routes {
	/// play id → (slot, the pad thread's session-audio queue).
	claims: HashMap<u64, (u8, SyncSender<Vec<f32>>)>,
	/// (play id, slot) of every pad that wants the client output silent.
	silencers: HashSet<(u64, u8)>,
}

static ROUTES: Mutex<Option<Routes>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut Routes) -> R) -> R {
	let mut g = ROUTES.lock().unwrap_or_else(|e| e.into_inner());
	f(g.get_or_insert_with(Routes::default))
}

/// The player's decision for one packet of play session `play_id`.
pub fn tap(play_id: u64) -> Tap {
	with(|r| {
		if let Some((_, tx)) = r.claims.get(&play_id) {
			Tap::Pad(tx.clone())
		} else if r.silencers.is_empty() {
			Tap::Local
		} else {
			Tap::Silent
		}
	})
}

/// Hand a packet to the pad; false when its queue is gone (route just ended).
pub fn send(tx: &SyncSender<Vec<f32>>, pcm: &[f32]) -> bool {
	!matches!(
		tx.try_send(pcm.to_vec()),
		Err(TrySendError::Disconnected(_))
	)
}

/// The headset side of one driven pad (module docs). Lives on the pad thread.
pub struct PadHeadset {
	play_id: u64,
	slot: u8,
	transport: Ds5Transport,
	sw: JackSwitch,
	mix: HeadsetMix,
	jack: Option<Jack>,
	rx: Option<Receiver<Vec<f32>>>,
	enc: Option<opus::Encoder>,
	dec: Option<opus::Decoder>,
	quiet: u32,
	out: Vec<MixedSlot>,
	spk: Vec<f32>,
	/// Slots sent to the pad while routed, for the stats log.
	pub slots: u64,
}

impl PadHeadset {
	pub fn new(play_id: u64, slot: u8, transport: Ds5Transport) -> Self {
		Self {
			play_id,
			slot,
			transport,
			sw: JackSwitch::default(),
			mix: HeadsetMix::default(),
			jack: None,
			rx: None,
			enc: None,
			dec: None,
			quiet: HANGOVER,
			out: Vec::new(),
			spk: vec![0.0; 960],
			slots: 0,
		}
	}

	/// Session audio goes to this pad now.
	pub fn routed(&self) -> bool {
		self.sw.routed()
	}

	/// An input report from the pad (USB 0x01 layout).
	pub fn on_state(&mut self, s: &[u8; USB_INPUT_LEN]) {
		self.jack = Some(Jack::from_usb_input(s));
	}

	/// A host media slot. While routed it joins the mix (returns true: the caller must not
	/// queue it again); otherwise the caller plays it as usual.
	pub fn on_host_slot(
		&mut self,
		haptics: Option<Frame>,
		speaker: Option<&(SpeakerOut, Vec<u8>)>,
		f: &Ds5Features,
	) -> bool {
		if !self.sw.routed() {
			return false;
		}
		let pcm = speaker.filter(|_| f.speaker).and_then(|(_, opus)| {
			let dec = match self.dec.as_mut() {
				Some(d) => d,
				None => self
					.dec
					.insert(opus::Decoder::new(48_000, opus::Channels::Stereo).ok()?),
			};
			match dec.decode_float(opus, &mut self.spk, false) {
				Ok(n) => Some(self.spk[..n * 2].to_vec()),
				Err(e) => {
					tracing::debug!("ds5 headset: pad speaker decode failed: {e}");
					None
				}
			}
		});
		self.mix.push_host(haptics, pcm);
		true
	}

	/// Call every pad loop turn: jack/route switches, then the mixed slots to the pad.
	/// Returns the switches (for the user notices).
	pub fn tick(
		&mut self,
		route: Route,
		pad: &mut Pad,
		audio: &mut PadAudio,
		f: &Ds5Features,
	) -> Vec<Switch> {
		let claimable = with(|r| {
			r.claims
				.get(&self.play_id)
				.is_none_or(|(s, _)| *s == self.slot)
		});
		let switches = self
			.sw
			.update(self.jack.take(), route, claimable, Instant::now());
		for s in &switches {
			tracing::info!(slot = self.slot, switch = ?s, "ds5 headset");
			match s {
				Switch::Plugged => pad.set_headphones(true),
				Switch::Unplugged => pad.set_headphones(false),
				Switch::RouteOn => self.claim(pad),
				Switch::RouteOff { .. } => self.release(pad, audio),
			}
		}
		let key = (self.play_id, self.slot);
		let silenced = self.sw.silenced();
		with(|r| {
			if silenced {
				r.silencers.insert(key);
			} else {
				r.silencers.remove(&key);
			}
		});
		if self.sw.routed() {
			self.pump(pad, audio, f);
		}
		switches
	}

	fn claim(&mut self, pad: &mut Pad) {
		let (tx, rx) = sync_channel(QUEUE);
		with(|r| r.claims.insert(self.play_id, (self.slot, tx)));
		self.rx = Some(rx);
		self.mix.reset();
		pad.set_headset_route(true);
	}

	fn release(&mut self, pad: &mut Pad, audio: &mut PadAudio) {
		with(|r| {
			if r.claims
				.get(&self.play_id)
				.is_some_and(|(s, _)| *s == self.slot)
			{
				r.claims.remove(&self.play_id);
			}
		});
		self.rx = None;
		self.mix.reset();
		pad.set_headset_route(false);
		audio.flush_usb();
	}

	fn pump(&mut self, pad: &mut Pad, audio: &mut PadAudio, f: &Ds5Features) {
		if let Some(rx) = &self.rx {
			while let Ok(pcm) = rx.try_recv() {
				self.mix.push_session(&pcm);
			}
		}
		self.mix.pop(&mut self.out);
		for s in std::mem::take(&mut self.out) {
			self.slots += 1;
			match self.transport {
				Ds5Transport::Bt => {
					let opus = s.pcm.as_deref().and_then(|p| self.encode(p));
					pad.push_headset_slot(s.haptics, opus);
				}
				Ds5Transport::Usb => audio.push_usb_pcm(s.haptics, s.pcm.as_deref(), f),
			}
		}
	}

	/// One 480-frame block as the pad's 200-byte Opus frame; `None` for silence (a
	/// haptics-only report costs less airtime and the headphones stay quiet anyway).
	fn encode(&mut self, pcm: &[f32]) -> Option<Vec<u8>> {
		debug_assert_eq!(pcm.len(), OUT_FRAMES * 2);
		let peak = pcm.iter().fold(0f32, |m, v| m.max(v.abs()));
		self.quiet = if peak < SILENCE {
			self.quiet.saturating_add(1)
		} else {
			0
		};
		if self.quiet > HANGOVER {
			return None;
		}
		let enc = match self.enc.as_mut() {
			Some(e) => e,
			None => self.enc.insert(new_encoder()?),
		};
		let mut buf = [0u8; SPEAKER_OPUS_LEN];
		match enc.encode_float(pcm, &mut buf) {
			Ok(n) => Some(buf[..n].to_vec()),
			Err(e) => {
				tracing::debug!("ds5 headset: opus encode failed: {e}");
				None
			}
		}
	}
}

/// The pad's speaker format (same as `audio::SpeakerEncoder`): 48 kHz stereo CELT low
/// delay, 10 ms, CBR 160 kbit/s = 200 bytes.
fn new_encoder() -> Option<opus::Encoder> {
	let mut e =
		opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::LowDelay).ok()?;
	e.set_bitrate(opus::Bitrate::Bits(OPUS_BITRATE)).ok()?;
	e.set_vbr(false).ok()?;
	Some(e)
}

impl Drop for PadHeadset {
	fn drop(&mut self) {
		let key = (self.play_id, self.slot);
		with(|r| {
			r.silencers.remove(&key);
			if r.claims
				.get(&self.play_id)
				.is_some_and(|(s, _)| *s == self.slot)
			{
				r.claims.remove(&self.play_id);
			}
		});
	}
}

/// The user notice for a switch, if any.
pub fn notice_key(s: Switch, was_routed: bool) -> Option<&'static str> {
	match s {
		Switch::RouteOn => Some("ds5.headset.on"),
		// Shed: auto-degrade has its own notice. Plain unplug without a route: nothing.
		Switch::RouteOff { restore: true } if was_routed => Some("ds5.headset.off"),
		_ => None,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The route table is global: tests that read it run one at a time.
	static SERIAL: Mutex<()> = Mutex::new(());

	#[test]
	fn the_player_follows_claims_and_silencers() {
		let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
		let id = 0xDEAD_0001;
		assert!(matches!(tap(id), Tap::Local));
		let (tx, rx) = sync_channel(4);
		with(|r| r.claims.insert(id, (0, tx)));
		let Tap::Pad(t) = tap(id) else {
			panic!("routed")
		};
		assert!(send(&t, &[0.5, 0.5]));
		assert_eq!(rx.try_recv().unwrap(), vec![0.5, 0.5]);
		// Another session while a pad is silencing: dropped.
		with(|r| r.silencers.insert((id, 0)));
		assert!(matches!(tap(id + 1), Tap::Silent));
		drop(rx);
		assert!(!send(&t, &[0.0]), "pad thread gone");
		with(|r| {
			r.claims.remove(&id);
			r.silencers.remove(&(id, 0));
		});
		assert!(matches!(tap(id), Tap::Local));
	}

	struct Io(Vec<Vec<u8>>);
	impl pulsar_core::input::ds5::pad::HidIo for Io {
		fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
			self.0.push(buf.to_vec());
			Ok(buf.len())
		}
		fn get_feature(&mut self, id: u8, len: usize) -> std::io::Result<Vec<u8>> {
			let mut v = vec![0; len];
			v[0] = id;
			Ok(v)
		}
	}

	/// BT pad end to end without hardware: plug → route claimed → session PCM from the
	/// player + a host slot → 0x39 reports with the headphone block; unplug → released.
	#[test]
	fn bt_pad_plays_session_audio_on_the_jack() {
		let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
		let id = 0xDEAD_0002;
		let mut io = Io(Vec::new());
		let (mut pad, _) = Pad::open(&mut io, Ds5Transport::Bt, Ds5Features::all()).unwrap();
		let dev = super::super::hid::Ds5Device {
			path: c"test".into(),
			transport: Ds5Transport::Bt,
			serial: String::new(),
			product_id: 0x0CE6,
		};
		let mut audio = PadAudio::new(0, &dev, Default::default());
		let mut h = PadHeadset::new(id, 0, Ds5Transport::Bt);
		let f = Ds5Features::all();
		let mut input = pulsar_core::input::ds5::report::neutral_usb_input();
		input[pulsar_core::input::ds5::report::JACK_STATUS] = 0x01;
		h.on_state(&input);
		assert!(h.tick(Route::Allowed, &mut pad, &mut audio, &f).is_empty());
		std::thread::sleep(
			pulsar_core::input::ds5::headset::DEBOUNCE + std::time::Duration::from_millis(20),
		);
		h.on_state(&input);
		let sw = h.tick(Route::Allowed, &mut pad, &mut audio, &f);
		assert_eq!(sw, vec![Switch::Plugged, Switch::RouteOn]);
		let Tap::Pad(tx) = tap(id) else {
			panic!("claimed")
		};
		assert!(
			matches!(tap(id + 1), Tap::Silent),
			"nothing plays anywhere else"
		);
		// 6 slots' worth of a tone from the player, one host slot with haptics.
		let tone: Vec<f32> = (0..512 * 6 * 2)
			.map(|i| ((i / 2) as f32 * 0.05).sin() * 0.3)
			.collect();
		for c in tone.chunks(960) {
			assert!(send(&tx, c));
		}
		assert!(h.on_host_slot(Some([5; 64]), None, &f));
		h.tick(Route::Allowed, &mut pad, &mut audio, &f);
		assert_eq!(h.slots, 6);
		io.0.clear();
		pad.poll(Instant::now(), &mut io);
		let r = io.0.iter().find(|w| w[0] == 0x39).expect("audio report");
		assert_eq!(r[140], 0xD6, "headphone Opus block");
		assert_eq!(r[12], 5, "host haptics rode along");
		assert!(r[142..342].iter().any(|&b| b != 0), "Opus payload");
		// Unplug: released, the player plays locally again.
		input[pulsar_core::input::ds5::report::JACK_STATUS] = 0;
		h.on_state(&input);
		h.tick(Route::Allowed, &mut pad, &mut audio, &f);
		std::thread::sleep(
			pulsar_core::input::ds5::headset::DEBOUNCE + std::time::Duration::from_millis(20),
		);
		h.on_state(&input);
		let sw = h.tick(Route::Allowed, &mut pad, &mut audio, &f);
		assert_eq!(
			sw,
			vec![Switch::Unplugged, Switch::RouteOff { restore: true }]
		);
		assert!(matches!(tap(id), Tap::Local));
		assert!(!send(&tx, &[0.0; 2]), "old queue closed");
	}

	#[test]
	fn notices() {
		assert_eq!(notice_key(Switch::RouteOn, false), Some("ds5.headset.on"));
		assert_eq!(
			notice_key(Switch::RouteOff { restore: true }, true),
			Some("ds5.headset.off")
		);
		assert_eq!(notice_key(Switch::RouteOff { restore: true }, false), None);
		assert_eq!(notice_key(Switch::RouteOff { restore: false }, true), None);
		assert_eq!(notice_key(Switch::Plugged, false), None);
		for k in ["ds5.headset.on", "ds5.headset.off"] {
			assert_ne!(crate::i18n::t(k), k, "{k} missing from lang/tr.json");
			assert_ne!(
				crate::i18n::t_in(k, true),
				crate::i18n::t(k),
				"{k} missing from lang/en.json"
			);
		}
	}
}
