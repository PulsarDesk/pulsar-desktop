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
//! Pads are dropped (usbip detach / Duo remove) when the client disconnects the slot or the
//! session ends: [`Ds5Host::shutdown`] runs from the session's cleanup guard on every exit
//! path (Bye, peer timeout, host kick, task abort, a panic unwinding the session task), and a
//! pad whose plug-in finishes after that is unplugged at once ([`PadSlots`]). `FEAT_DS5` is advertised only when a backend can work (`host_duo.rs`: usbip-win2 /
//! usbip installed, or Duo's driver inside a Duo seat — HID only there, no media).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

/// Virtual pads with an audio endpoint (usbip) plugged in on this host, all sessions.
static AUDIO_PADS: AtomicUsize = AtomicUsize::new(0);

/// Whether a virtual DualSense with its own audio endpoint is plugged in on this host. The
/// host's audio capture then taps the default endpoint so the pad's speaker/haptics audio
/// stays out of the client's sound (`host/handlers/ds5_route.rs`).
pub fn audio_pad_plugged() -> bool {
	AUDIO_PADS.load(Ordering::Relaxed) > 0
}

/// Counts one pad in [`AUDIO_PADS`] while alive.
struct AudioPadToken;

impl AudioPadToken {
	fn new() -> Self {
		AUDIO_PADS.fetch_add(1, Ordering::Relaxed);
		Self
	}
}

impl Drop for AudioPadToken {
	fn drop(&mut self) {
		AUDIO_PADS.fetch_sub(1, Ordering::Relaxed);
	}
}

/// One session's pad slots: live pads, plug-ins in flight, slots that failed. Generic over
/// the pad so the lifecycle (disconnect / session end → pad dropped) is unit-tested.
pub(crate) struct PadSlots<T> {
	live: HashMap<u8, T>,
	/// `(slot, generation)` of each plug-in in flight (at most one per slot).
	pending: Vec<(u8, u64)>,
	/// Slots whose pad failed to start; not retried this session (no attach storm).
	failed: Vec<u8>,
	next_gen: u64,
	/// The session ended: nothing is plugged in any more.
	closed: bool,
}

impl<T> Default for PadSlots<T> {
	fn default() -> Self {
		Self {
			live: HashMap::new(),
			pending: Vec::new(),
			failed: Vec::new(),
			next_gen: 0,
			closed: false,
		}
	}
}

impl<T> PadSlots<T> {
	pub(crate) fn get(&self, slot: u8) -> Option<&T> {
		self.live.get(&slot)
	}

	/// A report for a slot without a pad: `Some(generation)` when the caller should plug
	/// one in now (nothing live, in flight or failed for it, session open).
	pub(crate) fn begin(&mut self, slot: u8) -> Option<u64> {
		if self.closed
			|| self.live.contains_key(&slot)
			|| self.failed.contains(&slot)
			|| self.pending.iter().any(|(s, _)| *s == slot)
		{
			return None;
		}
		self.next_gen += 1;
		self.pending.push((slot, self.next_gen));
		Some(self.next_gen)
	}

	/// Whether plug-in `generation` for `slot` is still wanted (not disconnected meanwhile,
	/// session still open).
	pub(crate) fn wanted(&self, slot: u8, generation: u64) -> bool {
		!self.closed && self.pending.contains(&(slot, generation))
	}

	/// Plug-in `generation` ended with `pad` (`None` = it failed). A pad nobody wants any
	/// more (slot disconnected, session ended) is handed back for the caller to drop
	/// outside the lock.
	pub(crate) fn finish(&mut self, slot: u8, generation: u64, pad: Option<T>) -> Option<T> {
		let wanted = self.wanted(slot, generation);
		self.pending.retain(|p| *p != (slot, generation));
		match pad {
			None => {
				if wanted {
					self.failed.push(slot);
				}
				None
			}
			Some(p) if wanted => self.live.insert(slot, p),
			Some(p) => Some(p),
		}
	}

	/// The client's pad at `slot` went away: its pad (to drop outside the lock).
	pub(crate) fn disconnect(&mut self, slot: u8) -> Option<T> {
		self.pending.retain(|(s, _)| *s != slot);
		self.live.remove(&slot)
	}

	/// The session ended: every live pad (to drop outside the lock); later plug-ins are
	/// handed back by [`Self::finish`].
	pub(crate) fn shutdown(&mut self) -> Vec<T> {
		self.closed = true;
		self.pending.clear();
		self.live.drain().map(|(_, p)| p).collect()
	}

	#[cfg(test)]
	fn live_count(&self) -> usize {
		self.live.len()
	}
}

struct Slot {
	vpad: Arc<HostPad>,
	/// `Some` for a pad with an audio endpoint (usbip; Duo pads are HID only).
	_audio: Option<AudioPadToken>,
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
	/// Live pads + plug-ins in flight (inputs for a slot are dropped until its pad is up).
	slots: Arc<Mutex<PadSlots<Slot>>>,
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
		let generation = {
			let mut slots = self.slots.lock().unwrap();
			if let Some(s) = slots.get(slot) {
				s.vpad.set_input(&report.to_usb_input());
				return;
			}
			match slots.begin(slot) {
				Some(g) => g,
				None => return,
			}
		};
		let me = self.clone();
		let first = *report;
		std::thread::Builder::new()
			.name(format!("ds5-plug-{slot}"))
			.spawn(move || {
				std::thread::sleep(PLUG_DELAY);
				// Disconnected meanwhile or the session ended (shut down, or its outbound
				// queue closed): don't plug a pad nobody will unplug.
				let still_wanted =
					me.slots.lock().unwrap().wanted(slot, generation) && !me.out.is_closed();
				let pad = if still_wanted {
					me.plug(slot, &first)
				} else {
					None
				};
				// Re-checked after the (seconds-long) plug-in: a pad for a slot or session
				// that went away meanwhile comes back here and is unplugged at once.
				let unwanted = me.slots.lock().unwrap().finish(slot, generation, pad);
				if unwanted.is_some() {
					drop(unwanted);
					tracing::info!(slot, "ds5 host: slot gone while plugging in; unplugged");
				}
			})
			.ok();
	}

	/// Plug a pad in (blocks: usbip attach / Duo discovery) and start its forwarder.
	/// `None` when it failed.
	fn plug(&self, slot: u8, first: &Ds5Report) -> Option<Slot> {
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
				Some(Slot {
					_audio: matches!(*vpad, HostPad::Usbip(_)).then(AudioPadToken::new),
					vpad,
					features,
					mic: Mutex::new(MicRx::new()),
					stop,
				})
			}
			Err(e) => {
				tracing::warn!(slot, "ds5 host: virtual DualSense unavailable: {e}");
				None
			}
		}
	}

	/// `Ds5Caps` / `Ds5Feature` from the client.
	pub fn on_data(&self, m: DataMsg) {
		let slots = self.slots.lock().unwrap();
		match m {
			DataMsg::Ds5Caps { slot, features, .. } => {
				if let Some(s) = slots.get(slot) {
					*s.features.lock().unwrap() = features;
				}
			}
			DataMsg::Ds5Feature { slot, id, data } => {
				// The pairing MAC stays the virtual pad's own (see bridge.rs).
				if id != 0x09 {
					if let Some(s) = slots.get(slot) {
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
		let Some(s) = slots.get(slot) else {
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
		// Dropped outside the lock: unplugging blocks (usbip detach, up to ~3 s).
		let pad = self.slots.lock().unwrap().disconnect(slot);
		if pad.is_some() {
			drop(pad);
			tracing::info!(slot, "ds5 host: virtual DualSense unplugged");
		}
	}

	/// The session ended: unplug every pad now, and any still being plugged in as soon as
	/// it is up. Idempotent. Called from the session's cleanup guard, so it runs on every
	/// exit path, not only when the last clone of this host happens to drop.
	pub fn shutdown(&self) {
		let pads = self
			.slots
			.lock()
			.unwrap_or_else(|e| e.into_inner())
			.shutdown();
		if pads.is_empty() {
			return;
		}
		// Off the caller's thread: this runs in the session guard's Drop on an async
		// executor thread, and a usbip unplug blocks for seconds.
		// If the thread can't be spawned, unplug right here.
		let n = pads.len();
		let held = Arc::new(Mutex::new(Some(pads)));
		let unplug = move |held: Arc<Mutex<Option<Vec<Slot>>>>| {
			let pads = held.lock().unwrap_or_else(|e| e.into_inner()).take();
			if pads.is_some() {
				drop(pads);
				tracing::info!(
					pads = n,
					"ds5 host: session ended; virtual DualSense(s) unplugged"
				);
			}
		};
		let h = held.clone();
		if std::thread::Builder::new()
			.name("ds5-unplug".into())
			.spawn(move || unplug(h))
			.is_err()
		{
			unplug(held);
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

#[cfg(test)]
mod tests {
	use super::PadSlots;
	use std::sync::atomic::{AtomicUsize, Ordering};
	use std::sync::Arc;

	/// Counts drops (a virtual pad unplugs on drop).
	struct Pad(Arc<AtomicUsize>);
	impl Drop for Pad {
		fn drop(&mut self) {
			self.0.fetch_add(1, Ordering::SeqCst);
		}
	}

	#[test]
	fn disconnect_drops_the_pad() {
		let dropped = Arc::new(AtomicUsize::new(0));
		let mut t = PadSlots::default();
		let g = t.begin(0).unwrap();
		assert_eq!(t.begin(0), None, "one plug-in per slot");
		assert!(t.finish(0, g, Some(Pad(dropped.clone()))).is_none());
		assert!(t.get(0).is_some());
		drop(t.disconnect(0));
		assert_eq!(dropped.load(Ordering::SeqCst), 1);
		assert_eq!(t.live_count(), 0);
		// The client's pad comes back: a new plug-in.
		assert!(t.begin(0).is_some());
	}

	#[test]
	fn session_end_drops_every_pad_and_late_plugins() {
		let dropped = Arc::new(AtomicUsize::new(0));
		let mut t = PadSlots::default();
		for slot in 0..2 {
			let g = t.begin(slot).unwrap();
			assert!(t.finish(slot, g, Some(Pad(dropped.clone()))).is_none());
		}
		let late = t.begin(3).unwrap();
		drop(t.shutdown());
		assert_eq!(dropped.load(Ordering::SeqCst), 2);
		assert!(!t.wanted(3, late));
		// The in-flight plug-in finishes after the session ended: handed back, not kept.
		let back = t.finish(3, late, Some(Pad(dropped.clone())));
		assert!(back.is_some());
		drop(back);
		assert_eq!(dropped.load(Ordering::SeqCst), 3);
		assert_eq!(t.live_count(), 0);
		assert_eq!(t.begin(1), None, "nothing new after the session ended");
		assert!(t.shutdown().is_empty(), "idempotent");
	}

	#[test]
	fn disconnect_during_plugin_hands_the_pad_back() {
		let dropped = Arc::new(AtomicUsize::new(0));
		let mut t = PadSlots::default();
		let old = t.begin(1).unwrap();
		assert!(t.disconnect(1).is_none());
		// The client's pad came back while the first plug-in still ran: only the newer
		// plug-in keeps its pad.
		let new = t.begin(1).unwrap();
		let back = t.finish(1, old, Some(Pad(dropped.clone())));
		assert!(back.is_some());
		drop(back);
		assert!(t.finish(1, new, Some(Pad(dropped.clone()))).is_none());
		assert_eq!(dropped.load(Ordering::SeqCst), 1);
		assert_eq!(t.live_count(), 1);
	}

	#[test]
	fn failed_slot_is_not_retried() {
		let mut t: PadSlots<()> = PadSlots::default();
		let g = t.begin(2).unwrap();
		assert!(t.finish(2, g, None).is_none());
		assert_eq!(t.begin(2), None);
		// A failure after a disconnect doesn't blacklist the slot.
		let g = t.begin(0).unwrap();
		t.disconnect(0);
		t.finish(0, g, None);
		assert!(t.begin(0).is_some());
	}
}
