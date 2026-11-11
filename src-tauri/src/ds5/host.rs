//! Streaming host: one virtual wired DualSense per client slot that sends `Ds5Input`.
//!
//! - Client → host: `InputEvent::Ds5Input` feeds the slot's [`VirtualDs5`] (created on
//!   the first report); `DataMsg::Ds5Feature` replaces its feature defaults;
//!   `DataMsg::Ds5Caps` says which features the client lets through.
//! - Host → client: the game's output state as `DataMsg::Ds5Output` (on change + 500 ms
//!   refresh, with OS volume merged in), and haptics + pad-speaker Opus as `TAG_DS5`
//!   media slots (`Ds5Media::Slot`), one per 10.667 ms.
//!
//! Pads are dropped (usbip detach) when the client disconnects the slot or the session
//! ends. `FEAT_DS5` is advertised only when usbip-win2 / usbip is installed.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pulsar_core::input::ds5::bt_audio::SpeakerOut;
use pulsar_core::input::ds5::haptics::Frame;
use pulsar_core::input::ds5::report::SetState;
use pulsar_core::input::ds5::virtual_pad::{local_mac, Ds5HostEvent, VirtualDs5, VirtualDs5Config};
use pulsar_core::input::ds5::wire::{Ds5Media, Ds5Report};
use pulsar_core::input::ds5::Ds5Features;
use pulsar_core::service::DataMsg;

use super::audio::{SpeakerEncoder, Volume};

/// Same cap as the ViGEm pads (`MAX_PADS` in host.rs).
const MAX_SLOTS: u8 = 4;
/// Re-send the output state this often even if unchanged (session datagrams can drop).
const STATE_REFRESH: Duration = Duration::from_millis(500);

/// Whether this host can present a virtual DualSense (usbip tool installed).
pub fn available() -> bool {
	cfg!(any(windows, target_os = "linux")) && pulsar_core::input::ds5::attach::locate(None).is_ok()
}

struct Slot {
	vpad: Arc<VirtualDs5>,
	features: Arc<Mutex<Ds5Features>>,
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
			out,
			media: tx,
			peer,
		}
	}

	/// A raw DualSense input report for `slot`.
	pub fn on_input(&self, slot: u8, report: &Ds5Report) {
		if slot >= MAX_SLOTS {
			return;
		}
		let mut slots = self.slots.lock().unwrap();
		if let Some(s) = slots.get(&slot) {
			s.vpad.set_input(&report.to_usb_input());
			return;
		}
		if self.failed.lock().unwrap().contains(&slot) {
			return;
		}
		let cfg = VirtualDs5Config {
			mac: local_mac(format!("{}/{slot}", self.peer).as_bytes()),
			..Default::default()
		};
		match VirtualDs5::start(cfg) {
			Ok((vpad, rx)) => {
				tracing::info!(slot, port = vpad.port(), "ds5 host: virtual DualSense up");
				vpad.set_input(&report.to_usb_input());
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
				slots.insert(
					slot,
					Slot {
						vpad,
						features,
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

	/// The client's pad at `slot` went away: unplug the virtual pad.
	pub fn disconnect(&self, slot: u8) {
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
				Ok(Ds5HostEvent::MicOpen(_)) | Ok(Ds5HostEvent::SetFeature { .. }) => {}
				Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
				Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
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
