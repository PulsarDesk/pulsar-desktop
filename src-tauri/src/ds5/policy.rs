//! Per-pad DualSense settings + automatic feature shedding ("otomatik kısma") on the
//! streaming client. The policy itself is pure and lives in
//! `pulsar_core::input::ds5::degrade`; this wires it to one pad thread (`client.rs`).
//!
//! - `Kapalı` (Off) = the feature is off.
//! - `Otomatik` (Auto) = on, sheddable while the Bluetooth link is weak (speaker → mic →
//!   HD haptics; shed haptics keep the motors going as classic rumble).
//! - `Açık` (On) = always on, never shed.

use std::time::Instant;

use pulsar_core::input::ds5::degrade::{
	Change, Degrader, Ds5Prefs, LinkMonitor, Shed, WARMUP_WINDOWS,
};
use pulsar_core::input::ds5::pad::Pad;
use pulsar_core::input::ds5::wire::Ds5Transport;
use pulsar_core::input::ds5::Ds5Features;

pub struct PadPolicy {
	prefs: Ds5Prefs,
	deg: Degrader,
	mon: LinkMonitor,
	bt: bool,
	/// Whether the pad mic is actually streaming (a game on the host has it open and it is
	/// allowed; `client_audio.rs` decides). Only a live mic counts as in use for shedding.
	mic_live: bool,
	/// Windows seen so far (the first [`WARMUP_WINDOWS`] are not judged).
	windows: u32,
}

impl PadPolicy {
	pub fn new(prefs: Ds5Prefs, transport: Ds5Transport, pad: &Pad, now: Instant) -> Self {
		Self {
			prefs,
			deg: Degrader::new(),
			mon: LinkMonitor::new(now, pad.stats()),
			bt: transport == Ds5Transport::Bt,
			mic_live: false,
			windows: 0,
		}
	}

	/// What the user's settings allow now, minus shed features (`mic` = the mic may stream
	/// when the host asks for it).
	pub fn allowed(&self) -> Ds5Features {
		self.deg.features(&self.prefs)
	}

	/// The features the pad applies now (`mic` only while it is live).
	pub fn features(&self) -> Ds5Features {
		let mut f = self.allowed();
		f.mic &= self.mic_live;
		f
	}

	/// The pad mic started or stopped streaming.
	pub fn set_mic_live(&mut self, live: bool) {
		self.mic_live = live;
	}

	/// What to tell the host (`Ds5Caps`): the allowed set, so the host asks for the mic
	/// only if it may stream. While haptics are shed but rumble is on, the host keeps
	/// sending haptics: the client turns their energy into rumble.
	pub fn caps(&self) -> Ds5Features {
		let mut f = self.allowed();
		f.haptics |= self.fallback();
		f
	}

	/// Haptics shed by auto-degrade (not turned off by the user) and rumble allowed.
	pub fn fallback(&self) -> bool {
		self.deg.shed().contains(&Shed::Haptics) && self.prefs.rumble.enabled()
	}

	/// Settings changed live.
	pub fn set_prefs(&mut self, prefs: Ds5Prefs) {
		self.prefs = prefs;
		self.deg.reconcile(&prefs);
	}

	pub fn on_input(&mut self, now: Instant) {
		self.mon.on_input(now);
	}

	/// Call every loop; returns a shed/restore step when one happened (BT pads only: a
	/// USB pad's audio does not go over HID).
	pub fn tick(&mut self, now: Instant, pad: &mut Pad) -> Option<Change> {
		let max_write = pad.take_window_max_write_us();
		let w = self.mon.tick(now, pad.stats(), max_write)?;
		self.windows = self.windows.saturating_add(1);
		if !self.bt || self.windows <= WARMUP_WINDOWS {
			return None;
		}
		let live = Ds5Features {
			mic: self.mic_live,
			..self.prefs.features()
		};
		let c = self.deg.on_window(&w, &self.prefs, live);
		if let Some(c) = c {
			tracing::info!(?c, window = ?w, "ds5 client: auto-degrade");
		}
		c
	}
}

/// The user notice for a shed/restore step.
pub fn notice_text(c: Change) -> &'static str {
	let key = match (c.feature, c.shed) {
		(Shed::Speaker, true) => "ds5.shed.speaker",
		(Shed::Mic, true) => "ds5.shed.mic",
		(Shed::Haptics, true) => "ds5.shed.haptics",
		(Shed::Speaker, false) => "ds5.restore.speaker",
		(Shed::Mic, false) => "ds5.restore.mic",
		(Shed::Haptics, false) => "ds5.restore.haptics",
	};
	crate::i18n::t(key)
}

#[cfg(test)]
mod tests {
	use super::*;
	use pulsar_core::input::ds5::degrade::{Mode, WINDOW};
	use pulsar_core::input::ds5::pad::HidIo;

	struct Io;
	impl HidIo for Io {
		fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
			Ok(buf.len())
		}
		fn get_feature(&mut self, id: u8, len: usize) -> std::io::Result<Vec<u8>> {
			let mut v = vec![0; len];
			v[0] = id;
			Ok(v)
		}
	}

	fn pad(t: Ds5Transport) -> Pad {
		Pad::open(&mut Io, t, Ds5Features::all()).unwrap().0
	}

	#[test]
	fn off_features_and_closed_mic_stay_off() {
		let p = Ds5Prefs {
			leds: Mode::Off,
			..Default::default()
		};
		let pad = pad(Ds5Transport::Bt);
		let pol = PadPolicy::new(p, Ds5Transport::Bt, &pad, Instant::now());
		let f = pol.features();
		assert!(!f.leds && !f.mic);
		assert!(f.triggers && f.haptics && f.speaker && f.rumble);
		assert!(!pol.fallback());
	}

	#[test]
	fn shed_haptics_keep_host_haptics_for_the_rumble_fallback() {
		let p = Ds5Prefs {
			speaker: Mode::Off,
			..Default::default()
		};
		let mut pad = pad(Ds5Transport::Bt);
		let t0 = Instant::now();
		let mut pol = PadPolicy::new(p, Ds5Transport::Bt, &pad, t0);
		// No input for whole windows = a bad link (after the warm-up); speaker is off and
		// the mic is not live, so haptics go first.
		let mut change = None;
		for i in 1..=2 + WARMUP_WINDOWS {
			change = pol.tick(t0 + WINDOW * i, &mut pad).or(change);
		}
		let c = change.expect("shed after two bad windows");
		assert_eq!((c.feature, c.shed), (Shed::Haptics, true));
		assert!(!pol.features().haptics);
		assert!(pol.fallback());
		assert!(pol.caps().haptics, "the host keeps sending haptics frames");
		// Rumble off: no fallback, the host can stop haptics.
		pol.set_prefs(Ds5Prefs {
			rumble: Mode::Off,
			..p
		});
		assert!(!pol.fallback() && !pol.caps().haptics);
		// Auto-degrade off restores everything.
		pol.set_prefs(Ds5Prefs {
			auto_degrade: false,
			..p
		});
		assert!(pol.features().haptics);
	}

	#[test]
	fn usb_pads_never_shed() {
		let mut pad = pad(Ds5Transport::Usb);
		let t0 = Instant::now();
		let mut pol = PadPolicy::new(Ds5Prefs::default(), Ds5Transport::Usb, &pad, t0);
		for i in 1..=6 {
			assert!(pol.tick(t0 + WINDOW * i, &mut pad).is_none());
		}
	}

	#[test]
	fn notice_keys_exist() {
		for f in [Shed::Speaker, Shed::Mic, Shed::Haptics] {
			for shed in [true, false] {
				let t = notice_text(Change { feature: f, shed });
				assert!(t.starts_with("Bluetooth"), "{t}");
			}
		}
	}
}
