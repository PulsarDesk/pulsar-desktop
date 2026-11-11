//! Streaming host: one virtual wired DualSense per client slot that sends `Ds5Input`.
//!
//! - Client → host: `InputEvent::Ds5Input` feeds the slot's [`VirtualDs5`] (created on
//!   the first report); `DataMsg::Ds5Feature` replaces its feature defaults;
//!   `DataMsg::Ds5Caps` says which features the client lets through.
//! - Host → client: the game's output state as `DataMsg::Ds5Output` (on change + 500 ms
//!   refresh, with OS volume merged in), and haptics + pad-speaker Opus as `TAG_DS5`
//!   media slots (`Ds5Media::Slot`), one per 10.667 ms.
//! - Pad mic: when a game opens the virtual pad's mic (`Ds5HostEvent::MicOpen`) and the
//!   client allows it (`Ds5Caps`), the client is told with `DataMsg::Ds5Mic` (refreshed
//!   every second while on). Its mic arrives as `TAG_DS5` `Ds5Media::Mic` Opus (48 kHz
//!   mono, 10 ms) through `DataHandlers::on_ds5_media` → [`Ds5Host::on_media`], is decoded
//!   here and fed to the virtual pad's mic (stereo, behind a 40 ms jitter buffer).
//!
//! Pads are dropped (usbip detach) when the client disconnects the slot or the session
//! ends. `FEAT_DS5` is advertised only when a backend can work (`host_duo.rs`: usbip-win2 /
//! usbip installed, or Duo's driver inside a Duo seat — HID only there, no media).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pulsar_core::input::ds5::bt_audio::SpeakerOut;
use pulsar_core::input::ds5::haptics::Frame;
use pulsar_core::input::ds5::mic::{mono_to_stereo, MicAnnouncer, MicSeq, MIC_FRAME};
use pulsar_core::input::ds5::report::SetState;
use pulsar_core::input::ds5::virtual_pad::{local_mac, Ds5HostEvent, VirtualDs5Config};
use pulsar_core::input::ds5::wire::{Ds5Media, Ds5Report};
use pulsar_core::input::ds5::Ds5Features;
use pulsar_core::service::DataMsg;

use super::audio::{SpeakerEncoder, Volume};
use super::host_duo::HostPad;

/// Same cap as the ViGEm pads (`MAX_PADS` in host.rs).
const MAX_SLOTS: u8 = 4;
/// Re-send the output state this often even if unchanged (session datagrams can drop).
const STATE_REFRESH: Duration = Duration::from_millis(500);
/// Wait this long after a slot's first report before plugging its virtual pad in. The
/// first report comes right at session start, while the host is opening its audio
/// capture; a new USB audio device appearing then made the process-loopback activation
/// time out (doubled audio, 2026-11-11). A 3 s late pad costs nothing in a game.
const PLUG_DELAY: Duration = Duration::from_secs(3);

/// Mic jitter buffer on the virtual pad (network arrival is bursty).
const MIC_PREBUFFER_MS: u32 = 40;
/// Lost mic packets in a row that Opus conceals; a longer gap is left silent.
const MIC_MAX_CONCEAL: u64 = 3;

/// Decoder side of one slot's pad mic.
struct MicRx {
	dec: Option<opus::Decoder>,
	seq: MicSeq,
	pcm: Vec<i16>,
	packets: u64,
}

impl MicRx {
	fn new() -> Self {
		Self {
			dec: None,
			seq: MicSeq::default(),
			pcm: vec![0; MIC_FRAME * 6],
			packets: 0,
		}
	}

	/// One Opus packet → 48 kHz stereo s16 for `push_mic` (with concealment of a short gap
	/// before it).
	fn decode(&mut self, seq: u16, opus: &[u8], out: &mut Vec<i16>) {
		let lost_before = self.seq.lost;
		if !self.seq.accept(seq) {
			return;
		}
		if self.dec.is_none() {
			self.dec = opus::Decoder::new(48_000, opus::Channels::Mono)
				.map_err(|e| tracing::warn!("ds5 host: no mic decoder ({e})"))
				.ok();
		}
		let Some(dec) = self.dec.as_mut() else {
			return;
		};
		self.packets += 1;
		let gap = self.seq.lost - lost_before;
		if self.packets > 1 && gap > 0 && gap <= MIC_MAX_CONCEAL {
			for _ in 0..gap {
				if let Ok(n) = dec.decode(&[], &mut self.pcm[..MIC_FRAME], false) {
					out.extend(mono_to_stereo(&self.pcm[..n]));
				}
			}
		}
		match dec.decode(opus, &mut self.pcm, false) {
			Ok(n) => out.extend(mono_to_stereo(&self.pcm[..n])),
			Err(e) => tracing::debug!("ds5 host: mic decode failed: {e}"),
		}
	}
}

/// Whether this host can present a virtual DualSense (see `host_duo.rs`: Duo inside a Duo
/// seat; otherwise usbip-win2 on Windows, vhci-hcd with a writable sysfs attach on Linux).
pub fn available() -> bool {
	super::host_duo::current().is_some()
}

struct Slot {
	vpad: Arc<HostPad>,
	features: Arc<Mutex<Ds5Features>>,
	mic: Mutex<MicRx>,
	stop: Arc<AtomicBool>,
}

impl Drop for Slot {
	fn drop(&mut self) {
		self.stop.store(true, Ordering::Relaxed);
	}
}

/// Per-session DualSense host state. Cheap to clone (shared inside).
#[derive(Clone)]
pub struct Ds5Host {
	slots: Arc<Mutex<HashMap<u8, Slot>>>,
	/// Slots whose pad failed to start; not retried this session (no attach storm).
	failed: Arc<Mutex<Vec<u8>>>,
	/// Slots whose pad is being plugged in on a background thread (inputs are dropped
	/// until it is up).
	pending: Arc<Mutex<Vec<u8>>>,
	out: tokio::sync::mpsc::Sender<DataMsg>,
	media: tokio::sync::mpsc::Sender<Vec<u8>>,
	/// Seed for the virtual pads' MACs (stable per client so Linux hosts never see dupes).
	peer: String,
}

impl Ds5Host {
	/// `out` is the session's host→client DataMsg queue; `media` sends raw session payloads.
	pub fn new(
		out: tokio::sync::mpsc::Sender<DataMsg>,
		media: pulsar_core::SessionSender,
		peer: String,
	) -> Self {
		let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
		tokio::spawn(async move {
			while let Some(p) = rx.recv().await {
				if media.send(&p).await.is_err() {
					break;
				}
			}
		});
		Self {
			slots: Arc::default(),
			failed: Arc::default(),
			pending: Arc::default(),
			out,
			media: tx,
			peer,
		}
	}

	/// A raw DualSense input report for `slot`. The first one schedules the virtual pad
	/// (after [`PLUG_DELAY`], off the session loop: `usbip attach` blocks).
	pub fn on_input(&self, slot: u8, report: &Ds5Report) {
		if slot >= MAX_SLOTS {
			return;
		}
		if let Some(s) = self.slots.lock().unwrap().get(&slot) {
			s.vpad.set_input(&report.to_usb_input());
			return;
		}
		if self.failed.lock().unwrap().contains(&slot) {
			return;
		}
		{
			let mut pending = self.pending.lock().unwrap();
			if pending.contains(&slot) {
				return;
			}
			pending.push(slot);
		}
		let me = self.clone();
		let first = *report;
		std::thread::Builder::new()
			.name(format!("ds5-plug-{slot}"))
			.spawn(move || {
				std::thread::sleep(PLUG_DELAY);
				// Disconnected meanwhile (slot no longer pending) or the session ended (its
				// outbound queue closed): don't plug a pad nobody will unplug.
				let still_wanted =
					me.pending.lock().unwrap().contains(&slot) && !me.out.is_closed();
				if still_wanted {
					me.plug(slot, &first);
				}
				me.pending.lock().unwrap().retain(|s| *s != slot);
			})
			.ok();
	}

	fn plug(&self, slot: u8, first: &Ds5Report) {
		let cfg = VirtualDs5Config {
			mac: local_mac(format!("{}/{slot}", self.peer).as_bytes()),
			..Default::default()
		};
		match HostPad::start(cfg) {
			Ok((vpad, rx)) => {
				tracing::info!(slot, pad = %vpad.describe(), "ds5 host: virtual DualSense up");
				vpad.set_input(&first.to_usb_input());
				vpad.set_mic_prebuffer(MIC_PREBUFFER_MS);
				let vpad = Arc::new(vpad);
				let features = Arc::new(Mutex::new(Ds5Features::all()));
				let stop = Arc::new(AtomicBool::new(false));
				let f = Forwarder {
					slot,
					rx,
					features: features.clone(),
					stop: stop.clone(),
					out: self.out.clone(),
					media: self.media.clone(),
				};
				std::thread::Builder::new()
					.name(format!("ds5-host-{slot}"))
					.spawn(move || f.run())
					.ok();
				self.slots.lock().unwrap().insert(
					slot,
					Slot {
						vpad,
						features,
						mic: Mutex::new(MicRx::new()),
						stop,
					},
				);
			}
			Err(e) => {
				tracing::warn!(slot, "ds5 host: virtual DualSense unavailable: {e}");
				self.failed.lock().unwrap().push(slot);
			}
		}
	}

	/// `Ds5Caps` / `Ds5Feature` from the client.
	pub fn on_data(&self, m: DataMsg) {
		let slots = self.slots.lock().unwrap();
		match m {
			DataMsg::Ds5Caps { slot, features, .. } => {
				if let Some(s) = slots.get(&slot) {
					*s.features.lock().unwrap() = features;
				}
			}
			DataMsg::Ds5Feature { slot, id, data } => {
				// The pairing MAC stays the virtual pad's own (see bridge.rs).
				if id != 0x09 {
					if let Some(s) = slots.get(&slot) {
						s.vpad.set_feature(id, &data);
					}
				}
			}
			_ => {}
		}
	}

	/// A `TAG_DS5` payload from the client (`DataHandlers::on_ds5_media`): the pad mic.
	pub fn on_media(&self, payload: &[u8]) {
		let Some(Ds5Media::Mic { slot, seq, opus }) = Ds5Media::decode(payload) else {
			return;
		};
		let slots = self.slots.lock().unwrap();
		let Some(s) = slots.get(&slot) else {
			return;
		};
		if !s.features.lock().unwrap().mic {
			return;
		}
		let mut pcm = Vec::with_capacity(MIC_FRAME * 2);
		let mut rx = s.mic.lock().unwrap();
		rx.decode(seq, &opus, &mut pcm);
		if rx.packets % 1000 == 1 {
			tracing::info!(
				slot,
				packets = rx.packets,
				lost = rx.seq.lost,
				"ds5 host: pad mic"
			);
		}
		drop(rx);
		if !pcm.is_empty() {
			s.vpad.push_mic(&pcm);
		}
	}

	/// The client's pad at `slot` went away: unplug the virtual pad.
	pub fn disconnect(&self, slot: u8) {
		self.pending.lock().unwrap().retain(|s| *s != slot);
		if self.slots.lock().unwrap().remove(&slot).is_some() {
			tracing::info!(slot, "ds5 host: virtual DualSense unplugged");
		}
	}
}

/// Moves one virtual pad's events to the client.
struct Forwarder {
	slot: u8,
	rx: std::sync::mpsc::Receiver<Ds5HostEvent>,
	features: Arc<Mutex<Ds5Features>>,
	stop: Arc<AtomicBool>,
	out: tokio::sync::mpsc::Sender<DataMsg>,
	media: tokio::sync::mpsc::Sender<Vec<u8>>,
}

impl Forwarder {
	fn run(self) {
		let mut speaker = match SpeakerEncoder::new() {
			Ok(s) => Some(s),
			Err(e) => {
				tracing::warn!("ds5 host: no speaker encoder ({e}); haptics only");
				None
			}
		};
		let mut state = SetState::default();
		let mut volume = Volume::default();
		let mut sent: Option<(SetState, Instant)> = None;
		let mut haptics: VecDeque<Frame> = VecDeque::new();
		let mut opus_q: VecDeque<Option<Vec<u8>>> = VecDeque::new();
		let mut speaker_on = false;
		let mut seq: u16 = 0;
		let mut have_state = false;
		let mut mic = MicAnnouncer::new(self.features.lock().unwrap().mic);
		while !self.stop.load(Ordering::Relaxed) {
			match self.rx.recv_timeout(Duration::from_millis(20)) {
				Ok(Ds5HostEvent::Output(s)) => {
					state = s;
					have_state = true;
				}
				Ok(Ds5HostEvent::Volume {
					speaker_db,
					mic_db,
					speaker_mute,
					mic_mute,
				}) => {
					volume = Volume::from_db(speaker_db, mic_db, speaker_mute, mic_mute);
					have_state = true;
				}
				Ok(Ds5HostEvent::Haptics(f)) => {
					if self.features.lock().unwrap().haptics {
						haptics.push_back(f);
					}
				}
				Ok(Ds5HostEvent::Speaker(pcm)) => {
					let on = self.features.lock().unwrap().speaker;
					if let (true, true, Some(enc)) = (on, speaker_on, speaker.as_mut()) {
						enc.push(&pcm, &mut opus_q);
					}
				}
				Ok(Ds5HostEvent::SpeakerOpen(on)) => {
					speaker_on = on;
					if !on {
						haptics.clear();
						opus_q.clear();
					}
				}
				Ok(Ds5HostEvent::MicOpen(on)) => {
					tracing::info!(slot = self.slot, on, "ds5 host: game mic");
					mic.set_open(on);
				}
				Ok(Ds5HostEvent::SetFeature { .. }) => {}
				Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
				Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
			}
			// Tell the client when to stream its pad mic (changes + 1 s refresh while on).
			mic.set_allowed(self.features.lock().unwrap().mic);
			if let Some(on) = mic.poll(Instant::now()) {
				let _ = self.out.try_send(DataMsg::Ds5Mic {
					slot: self.slot,
					on,
				});
			}
			// One media datagram per 10.667 ms slot: haptics (or silence) + speaker Opus.
			while let Some(h) = haptics.pop_front() {
				let opus = opus_q.pop_front().flatten();
				let m = Ds5Media::Slot {
					slot: self.slot,
					seq,
					haptics: Some(h),
					speaker: opus.map(|o| (SpeakerOut::Speaker, o)),
				};
				seq = seq.wrapping_add(1);
				// Drop rather than queue: late haptics are worse than missing ones.
				let _ = self.media.try_send(m.encode());
			}
			if have_state {
				let mut merged = state;
				volume.merge(&mut merged);
				let due = match sent {
					Some((last, at)) => last != merged || at.elapsed() >= STATE_REFRESH,
					None => true,
				};
				if due {
					sent = Some((merged, Instant::now()));
					let msg = DataMsg::Ds5Output {
						slot: self.slot,
						report: merged.0.to_vec(),
					};
					if self.out.blocking_send(msg).is_err() {
						break;
					}
				}
			}
		}
		tracing::debug!(slot = self.slot, "ds5 host: forwarder ended");
	}
}
