//! Streaming client: drive the physical DualSense from the host's virtual wired DualSense.
//!
//! When the host advertises `FEAT_DS5`, the play loop (`play.rs`) hands each forwarded
//! DualSense to [`Ds5Client::sync`] instead of sending `GamepadSlot` for it. Each pad then
//! gets its own thread that owns a raw HID handle (`ds5::hid`) and a
//! `pulsar_core::input::ds5::pad::Pad`:
//! - pad input → `InputEvent::Ds5Input` (≤ 250 Hz, latest wins);
//! - host `DataMsg::Ds5Output` → state report; host `TAG_DS5` media slots → BT
//!   haptics/speaker reports (a USB pad plays them on its own audio device instead);
//!
//! - host `DataMsg::Ds5Mic` → the pad mic (BT pad mic stream / USB mic capture) →
//!   `TAG_DS5` mic media to the host; a USB pad's haptics + speaker play on its own audio
//!   device. Both live in `client_audio.rs`.
//!
//! Settings (`Ds5Prefs`, Settings → Kontroller → DualSense) apply live through
//! [`set_prefs_all`]; each pad thread runs its own auto-degrade policy (`policy.rs`).
//!
//! Instances are registered per play id so the session loop (`play/hold.rs`) can route
//! host messages to the right pads without new plumbing.

use std::collections::HashMap;
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use pulsar_core::input::ds5::degrade::Ds5Prefs;
use pulsar_core::input::ds5::pad::{Pad, PadInput};
use pulsar_core::input::ds5::report::{SetState, PID_DUALSENSE, PID_DUALSENSE_EDGE};
use pulsar_core::input::ds5::wire::{Ds5Media, Ds5Report, Ds5Transport};
use pulsar_core::input::ds5::Ds5Features;
use pulsar_core::service::{DataMsg, InputEvent};

use super::client_audio::{MediaTx, PadAudio};
use super::hid::{enumerate, Ds5Device, SdlHid};
use super::policy::{notice_text, PadPolicy};

/// Least gap between two `Ds5Input` sends (250 Hz).
const INPUT_GAP: Duration = Duration::from_millis(4);
/// Send the input at least this often even if unchanged (keepalive, like `GamepadSlot`).
const INPUT_KEEPALIVE: Duration = Duration::from_secs(1);

enum Cmd {
	Output(SetState),
	Media(Ds5Media),
	Prefs(Ds5Prefs),
	Mic(bool),
}

/// The user's DualSense settings (pushed by the UI with `set_ds5_settings`).
static PREFS: Mutex<Option<Ds5Prefs>> = Mutex::new(None);

/// Current settings (defaults until the UI pushed them). `PULSAR_DS5=0` forces the
/// passthrough off.
pub fn prefs() -> Ds5Prefs {
	let mut p = PREFS.lock().unwrap().unwrap_or_default();
	if std::env::var("PULSAR_DS5").as_deref() == Ok("0") {
		p.enabled = false;
	}
	p
}

/// New settings from the UI: stored for the next session and applied to every running
/// pad (features, jitter buffer, auto-degrade; `Ds5Caps` re-sent). The master switch
/// takes effect on the next connection.
pub fn set_prefs_all(p: Ds5Prefs) {
	*PREFS.lock().unwrap() = Some(p);
	for c in registry().lock().unwrap().values() {
		let mut inner = c.0.lock().unwrap();
		inner.prefs = p;
		for t in inner.pads.values() {
			let _ = t.tx.send(Cmd::Prefs(p));
		}
	}
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
	prefs: Ds5Prefs,
	play_id: u64,
	media_tx: MediaTx,
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
		prefs: Ds5Prefs,
	) -> Self {
		let c = Self(Arc::new(Mutex::new(Inner {
			pads: HashMap::new(),
			input_tx,
			data_tx: None,
			prefs,
			play_id,
			media_tx: MediaTx::default(),
		})));
		registry().lock().unwrap().insert(play_id, c.clone());
		c
	}

	/// Drop the session's pads (each thread stops and leaves its pad reset).
	pub fn unregister(play_id: u64) {
		if let Some(c) = registry().lock().unwrap().remove(&play_id) {
			c.0.lock().unwrap().pads.clear();
		}
		crate::play_notice::take(play_id);
	}

	/// The session's client→host DataMsg queue (created after the reader starts).
	pub fn set_data_tx(play_id: u64, tx: tokio::sync::mpsc::Sender<DataMsg>) {
		if let Some(c) = registry().lock().unwrap().get(&play_id) {
			c.0.lock().unwrap().data_tx = Some(tx);
		}
	}

	/// The session's client→host raw payload path (pad mic media). Call from the session
	/// loop (inside the tokio runtime).
	pub fn set_media_sender(play_id: u64, sender: pulsar_core::SessionSender) {
		if let Some(c) = registry().lock().unwrap().get(&play_id) {
			super::client_audio::connect_media(&c.0.lock().unwrap().media_tx, sender);
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
				prefs: inner.prefs,
				play_id: inner.play_id,
				media_tx: inner.media_tx.clone(),
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

/// Route a host `Ds5Mic` to the pad at `slot`.
pub fn on_mic(play_id: u64, slot: u8, on: bool) {
	send(play_id, slot, Cmd::Mic(on));
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

/// A state that releases both adaptive triggers.
fn triggers_off() -> SetState {
	let mut s = SetState::default();
	s.0[0] = pulsar_core::input::ds5::report::flag::B0_RIGHT_TRIGGER
		| pulsar_core::input::ds5::report::flag::B0_LEFT_TRIGGER;
	s.0[10] = 0x05;
	s.0[21] = 0x05;
	s
}

struct PadCtx {
	slot: u8,
	dev: Ds5Device,
	rx: Receiver<Cmd>,
	stop: Arc<AtomicBool>,
	input_tx: tokio::sync::mpsc::Sender<InputEvent>,
	data_tx: Option<tokio::sync::mpsc::Sender<DataMsg>>,
	prefs: Ds5Prefs,
	play_id: u64,
	media_tx: MediaTx,
}

impl PadCtx {
	fn send_caps(&self, slot: u8, caps: pulsar_core::input::ds5::Ds5Features) {
		if let Some(tx) = &self.data_tx {
			let _ = tx.try_send(DataMsg::Ds5Caps {
				slot,
				transport: self.dev.transport,
				features: caps,
			});
		}
	}

	fn run(self) {
		let slot = self.slot;
		let mut hid = match SdlHid::open(&self.dev) {
			Ok(h) => h,
			Err(e) => {
				tracing::warn!(slot, "ds5 client: open failed: {e}");
				return;
			}
		};
		// The pad mic starts off: it streams only while the host says a game has the mic
		// open (`Ds5Mic`, see client_audio.rs) and the policy allows it.
		let feats = Ds5Features {
			mic: false,
			..self.prefs.features()
		};
		let (mut pad, pad_features) = match Pad::open(&mut hid, self.dev.transport, feats) {
			Ok(p) => p,
			Err(e) => {
				tracing::warn!(slot, "ds5 client: init failed: {e}");
				return;
			}
		};
		// Network jitter: by default start after ~64 ms of slots, cap at ~256 ms (the first
		// WAN test overflowed and underran the local-bridge sized 8-slot FIFO).
		let (pre, max) = self.prefs.jitter_slots();
		pad.set_jitter_buffer(pre, max);
		let mut policy = PadPolicy::new(self.prefs, self.dev.transport, &pad, Instant::now());
		pad.set_features(policy.features());
		tracing::info!(slot, transport = ?self.dev.transport, "ds5 client: driving pad");
		self.send_caps(slot, policy.caps());
		if let Some(tx) = &self.data_tx {
			for (id, data) in pad_features {
				if id == 0x05 || id == 0x20 {
					let _ = tx.try_send(DataMsg::Ds5Feature { slot, id, data });
				}
			}
		}
		let mut audio = PadAudio::new(slot, &self.dev, self.media_tx.clone());
		let mut buf = [0u8; 600];
		let mut latest: Option<Ds5Report> = None;
		let mut sent: Option<(Ds5Report, Instant)> = None;
		let mut last_log = Instant::now();
		// Media arrival jitter (diagnostics): slots per window and the longest gap.
		let (mut media_n, mut media_gap_max, mut media_last) =
			(0u32, Duration::ZERO, None::<Instant>);
		while !self.stop.load(Ordering::Relaxed) {
			while let Ok(cmd) = self.rx.try_recv() {
				match cmd {
					Cmd::Output(s) => pad.set_output(s),
					Cmd::Media(Ds5Media::Slot {
						haptics, speaker, ..
					}) => {
						let now = Instant::now();
						if let Some(t) = media_last {
							media_gap_max = media_gap_max.max(now - t);
						}
						media_last = Some(now);
						media_n += 1;
						if !audio.on_slot(haptics, speaker.clone(), &policy.features())
							&& self.dev.transport == Ds5Transport::Bt
						{
							pad.push_slot(haptics, speaker);
						}
					}
					Cmd::Media(_) => {}
					Cmd::Prefs(p) => {
						let was = policy.features();
						policy.set_prefs(p);
						if was.triggers && !policy.features().triggers {
							// Masking only stops new effects; release the one in force.
							pad.set_output(triggers_off());
						}
						let (pre, max) = p.jitter_slots();
						pad.set_jitter_buffer(pre, max);
						pad.set_features(policy.features());
						pad.set_rumble_fallback(policy.fallback());
						self.send_caps(slot, policy.caps());
					}
					Cmd::Mic(on) => audio.mic.on_host(on),
				}
			}
			// Mic on/off follows the host + what the policy allows (mic shed = not allowed).
			audio.tick(&mut pad, &policy.allowed());
			policy.set_mic_live(audio.pad_mic());
			if let Some(change) = policy.tick(Instant::now(), &mut pad) {
				pad.set_features(policy.features());
				pad.set_rumble_fallback(policy.fallback());
				self.send_caps(slot, policy.caps());
				crate::play_notice::push(self.play_id, notice_text(change));
			}
			let wait = pad.poll(Instant::now(), &mut hid);
			match hid.read_timeout(&mut buf, wait.as_millis().clamp(0, 2) as i32) {
				Ok(0) => {}
				Ok(n) => {
					policy.on_input(Instant::now());
					match pad.on_input(&buf[..n]) {
						Some(PadInput::State(s)) => latest = Ds5Report::from_usb_input(&s),
						Some(PadInput::Mic(opus)) => audio.on_pad_mic(opus),
						None => {}
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
				tracing::info!(
					slot,
					stats = ?pad.stats(),
					media_slots = media_n,
					media_gap_max_ms = media_gap_max.as_millis() as u64,
					audio = ?audio.counters(),
					"ds5 client"
				);
				media_n = 0;
				media_gap_max = Duration::ZERO;
			}
		}
		// Leave the pad clean: triggers off, no rumble.
		// Every feature on so the reset reaches the pad, except the mic: a BT pad left
		// streaming its mic sends mic-carrying input reports that SDL's own driver
		// misreads as random button presses once we let go (2026-11-11).
		pad.set_features(Ds5Features {
			mic: false,
			..Ds5Features::all()
		});
		pad.set_output(triggers_off());
		let _ = pad.poll(Instant::now() + Duration::from_millis(10), &mut hid);
		tracing::info!(slot, "ds5 client: pad released");
	}
}
