//! Streaming client: the session's audio through headphones in the pad's jack ("Kolun
//! kulaklık girişi", PS5 gibi). Pure logic in `pulsar_core::input::ds5::headset`; design in
//! pulsar-core `docs/dualsense.md` ("Headset jack").
//!
//! - The pad thread (`client.rs`) owns a [`PadHeadset`]: it reads the jack bits from every
//!   input report, decides the route (`JackSwitch`), and while routed registers a queue for
//!   the play session's audio here ([`tap`]), mixes it with its own host pad slots
//!   (`HeadsetMix`: that pad's haptics + pad-speaker stream), applies the pad's own volume
//!   ([`gain`]) and queues the result on the pad (BT: Opus in 0x39 reports to block 0x16;
//!   USB: channels 1/2 of the pad's own audio device, which the routing state sends to the
//!   jack).
//! - The client's player (`native_audio.rs`) asks [`tap`] for every decoded packet: play it
//!   locally, hand it to EVERY routed pad of the session, or drop it. While any pad has the
//!   headphones in and the route is on (or shed), Pulsar plays nothing on the client output
//!   (maintainer: no sound anywhere else while the headphones are in; Pulsar has no UI
//!   sounds of its own).
//! - Volume: one level per pad uuid (0-100, default 100), set from the in-session menu's
//!   bars (`hpvol` overlay lines, `render_stats.rs`) or Settings → Kumandalar, persisted by
//!   the UI (`ui.headsetVolume`) and pushed here with `set_ds5_headset_volume`. It is a
//!   software gain on that pad's mix, not the pad's headphone volume field: it acts on the
//!   exact samples the pad plays (session + pad speaker), the same way on BT and USB, takes
//!   effect on the next 10 ms block, and does not fight the host's virtual-pad volume that
//!   `ds5::audio::Volume` merges into every state report (it also sets the headphone field).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Mutex;
use std::time::Instant;

use pulsar_core::input::ds5::bt_audio::{SpeakerOut, SPEAKER_OPUS_LEN};
use pulsar_core::input::ds5::haptics::Frame;
use pulsar_core::input::ds5::headset::{HeadsetMix, JackSwitch, MixedSlot, Route, Switch};
use pulsar_core::input::ds5::pad::Pad;
use pulsar_core::input::ds5::report::{flag, Jack, SetState, USB_INPUT_LEN};
use pulsar_core::input::ds5::speaker::{OPUS_BITRATE, OUT_FRAMES};
use pulsar_core::input::ds5::wire::Ds5Transport;
use pulsar_core::input::ds5::Ds5Features;

use super::audio::{HANGOVER, SILENCE};
use super::client_audio::PadAudio;

/// Decoded session packets queued between the player and the pad thread (≈ 0.6 s at
/// 10 ms packets; the pad thread drains it every ≤ 2 ms).
const QUEUE: usize = 64;
/// Volume of a pad nobody set yet (unity gain: what the route played before the bars).
pub const DEFAULT_VOLUME: u8 = 100;
/// Range of the volume bar: 1 % = -40 dB, 100 % = 0 dB, 0 % = mute.
const RANGE_DB: f32 = 40.0;

/// Where the client's player sends one decoded packet.
pub enum Tap {
	/// Play it on the client output (no headset route).
	Local,
	/// Send it to each pad that carries this session's audio.
	Pads(Vec<SyncSender<Vec<f32>>>),
	/// Drop it: headphones are in a pad but its route is shed (or a pad of another
	/// session has them): no sound anywhere else.
	Silent,
}

/// One routed pad, for the in-session menu's volume bars.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutedPad {
	pub slot: u8,
	pub uuid: String,
	pub volume: u8,
	/// Light-bar colour the game set last (None = never set).
	pub rgb: Option<[u8; 3]>,
}

struct Claim {
	uuid: String,
	tx: SyncSender<Vec<f32>>,
	rgb: Option<[u8; 3]>,
}

#[derive(Default)]
struct Routes {
	/// play id → slot → the routed pad's session-audio queue.
	claims: HashMap<u64, BTreeMap<u8, Claim>>,
	/// (play id, slot) of every pad that wants the client output silent.
	silencers: HashSet<(u64, u8)>,
	/// Volume per pad uuid (0-100).
	volumes: HashMap<String, u8>,
}

static ROUTES: Mutex<Option<Routes>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut Routes) -> R) -> R {
	let mut g = ROUTES.lock().unwrap_or_else(|e| e.into_inner());
	f(g.get_or_insert_with(Routes::default))
}

/// The player's decision for one packet of play session `play_id`.
pub fn tap(play_id: u64) -> Tap {
	with(|r| match r.claims.get(&play_id) {
		Some(c) if !c.is_empty() => Tap::Pads(c.values().map(|c| c.tx.clone()).collect()),
		_ if r.silencers.is_empty() => Tap::Local,
		_ => Tap::Silent,
	})
}

/// Hand a packet to a pad; false when its queue is gone (route just ended).
pub fn send(tx: &SyncSender<Vec<f32>>, pcm: &[f32]) -> bool {
	!matches!(
		tx.try_send(pcm.to_vec()),
		Err(TrySendError::Disconnected(_))
	)
}

/// The session's routed pads (by player slot), for the menu's volume bars.
pub fn routed_pads(play_id: u64) -> Vec<RoutedPad> {
	with(|r| {
		r.claims
			.get(&play_id)
			.map(|c| {
				c.iter()
					.map(|(slot, c)| RoutedPad {
						slot: *slot,
						uuid: c.uuid.clone(),
						volume: r.volumes.get(&c.uuid).copied().unwrap_or(DEFAULT_VOLUME),
						rgb: c.rgb,
					})
					.collect()
			})
			.unwrap_or_default()
	})
}

/// The `hpvol` line for the renderer: `uuid:volume:rrggbb,...` (rgb `-` when unknown),
/// empty when no pad is routed.
pub fn overlay_line(pads: &[RoutedPad]) -> String {
	pads.iter()
		.map(|p| {
			let rgb = p.rgb.map_or("-".to_string(), |[r, g, b]| {
				format!("{r:02x}{g:02x}{b:02x}")
			});
			format!("{}:{}:{rgb}", p.uuid, p.volume)
		})
		.collect::<Vec<_>>()
		.join(",")
}

/// All saved levels (the UI's `ui.headsetVolume`), replacing the previous map.
pub fn set_volumes(map: HashMap<String, u8>) {
	with(|r| r.volumes = map.into_iter().map(|(k, v)| (k, v.min(100))).collect());
}

/// One pad's level, live (from the in-session menu).
pub fn set_volume(uuid: &str, volume: u8) {
	with(|r| r.volumes.insert(uuid.to_string(), volume.min(100)));
}

fn volume(uuid: &str) -> u8 {
	with(|r| r.volumes.get(uuid).copied().unwrap_or(DEFAULT_VOLUME))
}

/// Linear gain for a volume level: 0 = mute, 1..=100 spans -40..0 dB (even steps for the
/// ear, unlike a linear bar where the top half barely changes anything).
pub fn gain(volume: u8) -> f32 {
	match volume.min(100) {
		0 => 0.0,
		100 => 1.0,
		v => 10f32.powf((v as f32 - 100.0) / 99.0 * RANGE_DB / 20.0),
	}
}

/// The headset side of one driven pad (module docs). Lives on the pad thread.
pub struct PadHeadset {
	play_id: u64,
	slot: u8,
	uuid: String,
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
	rgb: Option<[u8; 3]>,
	/// Gain applied to the last block (ramped toward the target, no zipper noise).
	last_gain: f32,
	/// Slots sent to the pad while routed, for the stats log.
	pub slots: u64,
}

impl PadHeadset {
	pub fn new(play_id: u64, slot: u8, uuid: String, transport: Ds5Transport) -> Self {
		Self {
			play_id,
			slot,
			uuid,
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
			rgb: None,
			last_gain: 1.0,
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

	/// The host's output state: remember the light-bar colour for the volume bar's label.
	pub fn on_host_state(&mut self, s: &SetState) {
		if s.0[1] & flag::B1_LED_COLOR != 0 {
			let rgb = [s.0[44], s.0[45], s.0[46]];
			if self.rgb != Some(rgb) {
				self.rgb = Some(rgb);
				let key = self.slot;
				with(|r| {
					if let Some(c) = r
						.claims
						.get_mut(&self.play_id)
						.and_then(|m| m.get_mut(&key))
					{
						c.rgb = Some(rgb);
					}
				});
			}
		}
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
		// Every pad of a session may carry its audio (each with its own mix and volume).
		let switches = self
			.sw
			.update(self.jack.take(), route, true, Instant::now());
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
		let claim = Claim {
			uuid: self.uuid.clone(),
			tx,
			rgb: self.rgb,
		};
		with(|r| {
			r.claims
				.entry(self.play_id)
				.or_default()
				.insert(self.slot, claim)
		});
		self.rx = Some(rx);
		self.mix.reset();
		self.last_gain = gain(volume(&self.uuid));
		pad.set_headset_route(true);
	}

	fn unclaim(&self) {
		with(|r| {
			if let Some(m) = r.claims.get_mut(&self.play_id) {
				m.remove(&self.slot);
				if m.is_empty() {
					r.claims.remove(&self.play_id);
				}
			}
		});
	}

	fn release(&mut self, pad: &mut Pad, audio: &mut PadAudio) {
		self.unclaim();
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
		if self.out.is_empty() {
			return;
		}
		let target = gain(volume(&self.uuid));
		for mut s in std::mem::take(&mut self.out) {
			self.slots += 1;
			if let Some(p) = s.pcm.as_mut() {
				apply_gain(p, self.last_gain, target);
			}
			self.last_gain = target;
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

/// Scale an interleaved stereo block, ramping linearly from `from` to `to` across it.
fn apply_gain(pcm: &mut [f32], from: f32, to: f32) {
	let frames = pcm.len() / 2;
	if frames == 0 || (from == 1.0 && to == 1.0) {
		return;
	}
	for (i, fr) in pcm.chunks_exact_mut(2).enumerate() {
		let g = from + (to - from) * (i + 1) as f32 / frames as f32;
		fr[0] *= g;
		fr[1] *= g;
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
		self.unclaim();
		let key = (self.play_id, self.slot);
		with(|r| r.silencers.remove(&key));
	}
}

/// The user notice for a switch, if any: an i18n key and whether to name the pad.
pub fn notice_key(s: Switch, was_routed: bool) -> Option<&'static str> {
	match s {
		Switch::RouteOn => Some("ds5.headset.on"),
		// Shed: auto-degrade has its own notice. Plain unplug without a route: nothing.
		Switch::RouteOff { restore: true } if was_routed => Some("ds5.headset.off"),
		_ => None,
	}
}

/// The notice text; names the pad ("· Oyuncu 2") when the session drives more than one.
pub fn notice_text(key: &str, slot: u8, pads_in_session: usize) -> String {
	let base = crate::i18n::t(key);
	if pads_in_session > 1 {
		format!(
			"{base} · {} {}",
			crate::i18n::t("ds5.headset.player"),
			slot + 1
		)
	} else {
		base.to_string()
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
		with(|r| {
			r.claims.entry(id).or_default().insert(
				0,
				Claim {
					uuid: "u".into(),
					tx,
					rgb: None,
				},
			)
		});
		let Tap::Pads(t) = tap(id) else {
			panic!("routed")
		};
		assert!(send(&t[0], &[0.5, 0.5]));
		assert_eq!(rx.try_recv().unwrap(), vec![0.5, 0.5]);
		// Another session while a pad is silencing: dropped.
		with(|r| r.silencers.insert((id, 0)));
		assert!(matches!(tap(id + 1), Tap::Silent));
		drop(rx);
		assert!(!send(&t[0], &[0.0]), "pad thread gone");
		with(|r| {
			r.claims.remove(&id);
			r.silencers.remove(&(id, 0));
		});
		assert!(matches!(tap(id), Tap::Local));
	}

	#[test]
	fn gain_curve_and_ramp() {
		assert_eq!(gain(0), 0.0);
		assert_eq!(gain(100), 1.0);
		assert_eq!(gain(200), 1.0, "clamped");
		assert!((20.0 * gain(1).log10() + 40.0).abs() < 0.01, "1 % = -40 dB");
		let mid = 20.0 * gain(50).log10();
		assert!((-21.0..-19.0).contains(&mid), "50 % ≈ -20 dB ({mid})");
		assert!((1..100).all(|v| gain(v) < gain(v + 1)));
		let mut b = vec![1.0f32; 8];
		apply_gain(&mut b, 1.0, 0.0);
		assert_eq!(b[7], 0.0);
		assert!(b[0] > 0.7, "ramped, not a step");
		let mut u = vec![0.3f32; 4];
		apply_gain(&mut u, 1.0, 1.0);
		assert_eq!(u, vec![0.3; 4]);
	}

	#[test]
	fn overlay_line_format() {
		let pads = vec![
			RoutedPad {
				slot: 0,
				uuid: "abc".into(),
				volume: 80,
				rgb: Some([0, 255, 16]),
			},
			RoutedPad {
				slot: 2,
				uuid: "def".into(),
				volume: 100,
				rgb: None,
			},
		];
		assert_eq!(overlay_line(&pads), "abc:80:00ff10,def:100:-");
		assert_eq!(overlay_line(&[]), "");
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

	struct TestPad {
		io: Io,
		pad: Pad,
		audio: PadAudio,
		h: PadHeadset,
	}

	fn test_pad(play: u64, slot: u8, uuid: &str) -> TestPad {
		let mut io = Io(Vec::new());
		let (pad, _) = Pad::open(&mut io, Ds5Transport::Bt, Ds5Features::all()).unwrap();
		let dev = super::super::hid::Ds5Device {
			path: c"test".into(),
			transport: Ds5Transport::Bt,
			serial: String::new(),
			product_id: 0x0CE6,
		};
		TestPad {
			io,
			pad,
			audio: PadAudio::new(slot, &dev, Default::default()),
			h: PadHeadset::new(play, slot, uuid.into(), Ds5Transport::Bt),
		}
	}

	fn jack(plugged: bool) -> [u8; USB_INPUT_LEN] {
		let mut input = pulsar_core::input::ds5::report::neutral_usb_input();
		input[pulsar_core::input::ds5::report::JACK_STATUS] = plugged as u8;
		input
	}

	/// Feed the jack reading to every pad, wait out the debounce, feed it again.
	fn set_jack(pads: &mut [&mut TestPad], plugged: bool) -> Vec<Vec<Switch>> {
		let f = Ds5Features::all();
		for p in pads.iter_mut() {
			p.h.on_state(&jack(plugged));
			p.h.tick(Route::Allowed, &mut p.pad, &mut p.audio, &f);
		}
		std::thread::sleep(
			pulsar_core::input::ds5::headset::DEBOUNCE + std::time::Duration::from_millis(20),
		);
		pads.iter_mut()
			.map(|p| {
				p.h.on_state(&jack(plugged));
				p.h.tick(Route::Allowed, &mut p.pad, &mut p.audio, &f)
			})
			.collect()
	}

	fn tone() -> Vec<f32> {
		(0..512 * 6 * 2)
			.map(|i| ((i / 2) as f32 * 0.05).sin() * 0.3)
			.collect()
	}

	/// Deliver what the player would: every packet to every routed pad of the session.
	fn play(id: u64, pcm: &[f32]) {
		let Tap::Pads(txs) = tap(id) else {
			panic!("routed")
		};
		for c in pcm.chunks(960) {
			for tx in &txs {
				assert!(send(tx, c));
			}
		}
	}

	fn audio_report(p: &mut TestPad) -> Vec<u8> {
		p.io.0.clear();
		p.pad.poll(Instant::now(), &mut p.io);
		p.io.0
			.iter()
			.find(|w| w[0] == 0x39)
			.expect("audio report")
			.clone()
	}

	/// BT pad end to end without hardware: plug → route claimed → session PCM from the
	/// player + a host slot → 0x39 reports with the headphone block; unplug → released.
	#[test]
	fn bt_pad_plays_session_audio_on_the_jack() {
		let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
		let id = 0xDEAD_0002;
		let mut a = test_pad(id, 0, "pad-a");
		let f = Ds5Features::all();
		assert_eq!(
			set_jack(&mut [&mut a], true),
			vec![vec![Switch::Plugged, Switch::RouteOn]]
		);
		assert!(
			matches!(tap(id + 1), Tap::Silent),
			"nothing plays anywhere else"
		);
		let mut st = SetState::default();
		st.0[1] = flag::B1_LED_COLOR;
		st.0[44..47].copy_from_slice(&[1, 2, 3]);
		a.h.on_host_state(&st);
		assert_eq!(
			routed_pads(id),
			vec![RoutedPad {
				slot: 0,
				uuid: "pad-a".into(),
				volume: DEFAULT_VOLUME,
				rgb: Some([1, 2, 3])
			}]
		);
		play(id, &tone());
		assert!(a.h.on_host_slot(Some([5; 64]), None, &f));
		a.h.tick(Route::Allowed, &mut a.pad, &mut a.audio, &f);
		assert_eq!(a.h.slots, 6);
		let r = audio_report(&mut a);
		assert_eq!(r[140], 0xD6, "headphone Opus block");
		assert_eq!(r[12], 5, "host haptics rode along");
		assert!(r[142..342].iter().any(|&b| b != 0), "Opus payload");
		let Tap::Pads(old) = tap(id) else { panic!() };
		assert_eq!(
			set_jack(&mut [&mut a], false),
			vec![vec![Switch::Unplugged, Switch::RouteOff { restore: true }]]
		);
		assert!(matches!(tap(id), Tap::Local));
		assert!(!send(&old[0], &[0.0; 2]), "old queue closed");
	}

	/// Two pads with headphones in the same session: both get the session audio, each
	/// with its own host stream and volume; unplugging one leaves the other routed and the
	/// laptop silent.
	#[test]
	fn two_pads_each_get_the_session_audio() {
		let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
		let id = 0xDEAD_0003;
		let f = Ds5Features::all();
		let mut a = test_pad(id, 0, "two-a");
		let mut b = test_pad(id, 1, "two-b");
		let sw = set_jack(&mut [&mut a, &mut b], true);
		assert!(sw.iter().all(|s| s.contains(&Switch::RouteOn)), "{sw:?}");
		let Tap::Pads(txs) = tap(id) else { panic!() };
		assert_eq!(txs.len(), 2);
		set_volume("two-b", 0);
		play(id, &tone());
		// Each pad's own host stream rides only on that pad.
		assert!(a.h.on_host_slot(Some([7; 64]), None, &f));
		assert!(b.h.on_host_slot(Some([9; 64]), None, &f));
		a.h.tick(Route::Allowed, &mut a.pad, &mut a.audio, &f);
		b.h.tick(Route::Allowed, &mut b.pad, &mut b.audio, &f);
		assert_eq!((a.h.slots, b.h.slots), (6, 6));
		let ra = audio_report(&mut a);
		assert_eq!((ra[140], ra[12]), (0xD6, 7));
		assert!(ra[142..342].iter().any(|&x| x != 0));
		// Pad B at volume 0: after the gain ramp its blocks are silence → after the hangover
		// they go out haptics-only, so its first report still carries the ramp-down frame.
		let rb = audio_report(&mut b);
		assert_eq!(rb[12], 9, "B's own haptics");
		let pads = routed_pads(id);
		assert_eq!(
			pads.iter().map(|p| (p.slot, p.volume)).collect::<Vec<_>>(),
			vec![(0, DEFAULT_VOLUME), (1, 0)]
		);
		// Unplug A: B stays routed, the laptop stays silent.
		let sw = set_jack(&mut [&mut a], false);
		assert_eq!(
			sw[0],
			vec![Switch::Unplugged, Switch::RouteOff { restore: true }]
		);
		assert!(b.h.routed());
		let Tap::Pads(txs) = tap(id) else {
			panic!("B still routed")
		};
		assert_eq!(txs.len(), 1);
		assert_eq!(routed_pads(id)[0].uuid, "two-b");
		// Unplug B too: the laptop plays again.
		set_jack(&mut [&mut b], false);
		assert!(matches!(tap(id), Tap::Local));
		set_volumes(HashMap::new());
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
		for k in ["ds5.headset.on", "ds5.headset.off", "ds5.headset.player"] {
			assert_ne!(crate::i18n::t(k), k, "{k} missing from lang/tr.json");
			assert_ne!(
				crate::i18n::t_in(k, true),
				crate::i18n::t(k),
				"{k} missing from lang/en.json"
			);
		}
		assert_eq!(
			notice_text("ds5.headset.on", 1, 1),
			crate::i18n::t("ds5.headset.on")
		);
		assert!(notice_text("ds5.headset.on", 1, 2).ends_with(" 2"));
	}
}
