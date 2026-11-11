//! Streaming client: drive the physical DualSense from the host's virtual wired DualSense.
//!
//! When the host advertises `FEAT_DS5`, the play loop (`play.rs`) hands each forwarded
//! DualSense to [`Ds5Client::sync`] instead of sending `GamepadSlot` for it. Each pad then
//! gets its own thread that owns a raw HID handle (`ds5::hid`) and a
//! `pulsar_core::input::ds5::pad::Pad`:
//! - pad input → `InputEvent::Ds5Input` (≤ 250 Hz, latest wins);
//! - host `DataMsg::Ds5Output` → state report; host `TAG_DS5` media slots → BT
//!   haptics/speaker reports (a USB pad gets state only in v1: its haptics/speaker play
//!   through its audio device, which is not wired yet).
//!
//! Instances are registered per play id so the session loop (`play/hold.rs`) can route
//! host messages to the right pads without new plumbing.

use std::collections::HashMap;
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use pulsar_core::input::ds5::pad::{Pad, PadInput};
use pulsar_core::input::ds5::report::{SetState, PID_DUALSENSE, PID_DUALSENSE_EDGE};
use pulsar_core::input::ds5::wire::{Ds5Media, Ds5Report, Ds5Transport};
use pulsar_core::input::ds5::Ds5Features;
use pulsar_core::service::{DataMsg, InputEvent};

use super::hid::{enumerate, Ds5Device, SdlHid};

/// Least gap between two `Ds5Input` sends (250 Hz).
const INPUT_GAP: Duration = Duration::from_millis(4);
/// Send the input at least this often even if unchanged (keepalive, like `GamepadSlot`).
const INPUT_KEEPALIVE: Duration = Duration::from_secs(1);

enum Cmd {
	Output(SetState),
	Media(Ds5Media),
}

struct PadThread {
	uuid: String,
	tx: Sender<Cmd>,
	stop: Arc<AtomicBool>,
}

impl Drop for PadThread {
	fn drop(&mut self) {
		self.stop.store(true, Ordering::Relaxed);
	}
}

struct Inner {
	/// Forwarded slot → its pad thread.
	pads: HashMap<u8, PadThread>,
	input_tx: tokio::sync::mpsc::Sender<InputEvent>,
	data_tx: Option<tokio::sync::mpsc::Sender<DataMsg>>,
	features: Ds5Features,
}

/// The DualSense passthrough state of one play session. Cheap to clone.
#[derive(Clone)]
pub struct Ds5Client(Arc<Mutex<Inner>>);

fn registry() -> &'static Mutex<HashMap<u64, Ds5Client>> {
	static R: OnceLock<Mutex<HashMap<u64, Ds5Client>>> = OnceLock::new();
	R.get_or_init(Default::default)
}

impl Ds5Client {
	/// Create and register for play session `play_id`.
	pub fn register(
		play_id: u64,
		input_tx: tokio::sync::mpsc::Sender<InputEvent>,
		features: Ds5Features,
	) -> Self {
		let c = Self(Arc::new(Mutex::new(Inner {
			pads: HashMap::new(),
			input_tx,
			data_tx: None,
			features,
		})));
		registry().lock().unwrap().insert(play_id, c.clone());
		c
	}

	/// Drop the session's pads (each thread stops and leaves its pad reset).
	pub fn unregister(play_id: u64) {
		if let Some(c) = registry().lock().unwrap().remove(&play_id) {
			c.0.lock().unwrap().pads.clear();
		}
	}

	/// The session's client→host DataMsg queue (created after the reader starts).
	pub fn set_data_tx(play_id: u64, tx: tokio::sync::mpsc::Sender<DataMsg>) {
		if let Some(c) = registry().lock().unwrap().get(&play_id) {
			c.0.lock().unwrap().data_tx = Some(tx);
		}
	}

	/// The forwarded DualSenses this tick: `(uuid, forwarded slot, HID path)`. Starts a pad
	/// thread for each new one and stops those that are gone or moved slots. Returns the
	/// uuids actually driven here, so the caller skips `GamepadSlot` only for those.
	pub fn sync(&self, pads: &[(String, u8, Option<CString>)]) -> Vec<String> {
		let mut inner = self.0.lock().unwrap();
		inner
			.pads
			.retain(|slot, t| pads.iter().any(|(u, s, _)| u == &t.uuid && s == slot));
		let mut driven = Vec::new();
		for (uuid, slot, path) in pads {
			if inner.pads.get(slot).is_some_and(|t| &t.uuid == uuid) {
				driven.push(uuid.clone());
				continue;
			}
			let Some(dev) = find_device(path.as_ref()) else {
				continue;
			};
			let (tx, rx) = std::sync::mpsc::channel();
			let stop = Arc::new(AtomicBool::new(false));
			let ctx = PadCtx {
				slot: *slot,
				dev,
				rx,
				stop: stop.clone(),
				input_tx: inner.input_tx.clone(),
				data_tx: inner.data_tx.clone(),
				features: inner.features,
			};
			if std::thread::Builder::new()
				.name(format!("ds5-pad-{slot}"))
				.spawn(move || ctx.run())
				.is_ok()
			{
				inner.pads.insert(
					*slot,
					PadThread {
						uuid: uuid.clone(),
						tx,
						stop,
					},
				);
				driven.push(uuid.clone());
			}
		}
		driven
	}
}

/// Route a host `Ds5Output` to the pad at `slot` of play session `play_id`.
pub fn on_output(play_id: u64, slot: u8, report: &[u8]) {
	let Some(s) = SetState::from_bytes(report) else {
		return;
	};
	send(play_id, slot, Cmd::Output(s));
}

/// Route a `TAG_DS5` media payload (the whole session payload, tag included).
pub fn on_media(play_id: u64, payload: &[u8]) {
	if let Some(m @ Ds5Media::Slot { slot, .. }) = Ds5Media::decode(payload) {
		send(play_id, slot, Cmd::Media(m));
	}
}

fn send(play_id: u64, slot: u8, cmd: Cmd) {
	let reg = registry().lock().unwrap();
	if let Some(c) = reg.get(&play_id) {
		if let Some(t) = c.0.lock().unwrap().pads.get(&slot) {
			let _ = t.tx.send(cmd);
		}
	}
}

/// The HID device for SDL's pad path; falls back to the only DualSense when SDL gave no
/// path (non-hidapi driver).
fn find_device(path: Option<&CString>) -> Option<Ds5Device> {
	let devs: Vec<Ds5Device> = enumerate()
		.into_iter()
		.filter(|d| d.product_id == PID_DUALSENSE || d.product_id == PID_DUALSENSE_EDGE)
		.collect();
	if let Some(p) = path {
		if let Some(d) = devs
			.iter()
			.find(|d| d.path.as_bytes().eq_ignore_ascii_case(p.as_bytes()))
		{
			return Some(d.clone());
		}
	}
	(devs.len() == 1).then(|| devs[0].clone())
}

struct PadCtx {
	slot: u8,
	dev: Ds5Device,
	rx: Receiver<Cmd>,
	stop: Arc<AtomicBool>,
	input_tx: tokio::sync::mpsc::Sender<InputEvent>,
	data_tx: Option<tokio::sync::mpsc::Sender<DataMsg>>,
	features: Ds5Features,
}

impl PadCtx {
	fn run(self) {
		let slot = self.slot;
		let mut hid = match SdlHid::open(&self.dev) {
			Ok(h) => h,
			Err(e) => {
				tracing::warn!(slot, "ds5 client: open failed: {e}");
				return;
			}
		};
		// Pad mic stays off in v1 streaming (the host has no mic-open message yet).
		let feats = Ds5Features {
			mic: false,
			..self.features
		};
		let (mut pad, pad_features) = match Pad::open(&mut hid, self.dev.transport, feats) {
			Ok(p) => p,
			Err(e) => {
				tracing::warn!(slot, "ds5 client: init failed: {e}");
				return;
			}
		};
		// Network jitter: start after ~64 ms of slots, cap at ~256 ms (the first WAN test
		// overflowed and underran the local-bridge sized 8-slot FIFO).
		pad.set_jitter_buffer(6, 24);
		tracing::info!(slot, transport = ?self.dev.transport, "ds5 client: driving pad");
		if let Some(tx) = &self.data_tx {
			let _ = tx.try_send(DataMsg::Ds5Caps {
				slot,
				transport: self.dev.transport,
				features: feats,
			});
			for (id, data) in pad_features {
				if id == 0x05 || id == 0x20 {
					let _ = tx.try_send(DataMsg::Ds5Feature { slot, id, data });
				}
			}
		}
		let mut buf = [0u8; 600];
		let mut latest: Option<Ds5Report> = None;
		let mut sent: Option<(Ds5Report, Instant)> = None;
		let mut last_log = Instant::now();
		while !self.stop.load(Ordering::Relaxed) {
			while let Ok(cmd) = self.rx.try_recv() {
				match cmd {
					Cmd::Output(s) => pad.set_output(s),
					Cmd::Media(Ds5Media::Slot {
						haptics, speaker, ..
					}) => {
						if self.dev.transport == Ds5Transport::Bt {
							pad.push_slot(haptics, speaker);
						}
					}
					Cmd::Media(_) => {}
				}
			}
			let wait = pad.poll(Instant::now(), &mut hid);
			match hid.read_timeout(&mut buf, wait.as_millis().clamp(0, 2) as i32) {
				Ok(0) => {}
				Ok(n) => {
					if let Some(PadInput::State(s)) = pad.on_input(&buf[..n]) {
						latest = Ds5Report::from_usb_input(&s);
					}
				}
				Err(e) => {
					tracing::warn!(slot, "ds5 client: pad read failed ({e}); stopping");
					break;
				}
			}
			if let Some(r) = latest {
				let due = match sent {
					Some((last, at)) => {
						at.elapsed() >= INPUT_GAP && (last != r || at.elapsed() >= INPUT_KEEPALIVE)
					}
					None => true,
				};
				if due
					&& self
						.input_tx
						.try_send(InputEvent::Ds5Input { slot, report: r })
						.is_ok()
				{
					sent = Some((r, Instant::now()));
				}
			}
			if last_log.elapsed() > Duration::from_secs(10) {
				last_log = Instant::now();
				tracing::info!(slot, stats = ?pad.stats(), "ds5 client");
			}
		}
		// Leave the pad clean: triggers off, no rumble.
		let mut reset = SetState::default();
		reset.0[0] = pulsar_core::input::ds5::report::flag::B0_RIGHT_TRIGGER
			| pulsar_core::input::ds5::report::flag::B0_LEFT_TRIGGER;
		reset.0[10] = 0x05;
		reset.0[21] = 0x05;
		pad.set_output(reset);
		let _ = pad.poll(Instant::now() + Duration::from_millis(10), &mut hid);
		tracing::info!(slot, "ds5 client: pad released");
	}
}
