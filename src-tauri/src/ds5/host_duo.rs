//! Which virtual DualSense the streaming host plugs in (`ds5/host.rs`):
//!
//! - **USB/IP** ([`VirtualDs5`]): full pad incl. HD haptics + speaker (usbip-win2 / vhci).
//! - **Duo** ([`DuoDs5`], Windows): when the host runs inside a Duo multi-seat (non-console)
//!   session. There a USB/IP pad is a system-wide "console" device that Duo's HID isolation
//!   hides from the seat (and would show to every other seat), so the pad is created through
//!   Duo's own driver instead. HID only: triggers, LEDs, rumble, gyro, touchpad; no haptics,
//!   speaker or mic, so the forwarder ends up sending `Ds5Output` only (no media).
//!
//! The choice follows the same detection as the other virtual pads
//! (`pulsar_core::input::duo_pads_wanted`, `PULSAR_PAD_BACKEND=duo|vigem` forces it).

use std::sync::mpsc::Receiver;

#[cfg(windows)]
use pulsar_core::input::ds5::report::STATE_LEN;
use pulsar_core::input::ds5::report::USB_INPUT_LEN;
use pulsar_core::input::ds5::virtual_pad::{Ds5HostEvent, VirtualDs5, VirtualDs5Config};
#[cfg(windows)]
use pulsar_core::input::DuoDs5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
	Usbip,
	Duo,
}

/// The backend for this host, or `None` when none can work. Inside a Duo seat only Duo
/// is right (a USB/IP pad would land in the console session, not this seat).
pub fn choose(duo_wanted: bool, duo_ok: bool, usbip_ok: bool) -> Option<Backend> {
	if duo_wanted {
		duo_ok.then_some(Backend::Duo)
	} else {
		usbip_ok.then_some(Backend::Usbip)
	}
}

#[cfg(windows)]
fn duo_wanted() -> bool {
	pulsar_core::input::duo_pads_wanted()
}
#[cfg(not(windows))]
fn duo_wanted() -> bool {
	false
}

#[cfg(windows)]
fn duo_ok() -> bool {
	pulsar_core::input::duo_available()
}
#[cfg(not(windows))]
fn duo_ok() -> bool {
	false
}

fn usbip_ok() -> bool {
	cfg!(any(windows, target_os = "linux")) && pulsar_core::input::ds5::attach::available()
}

/// The backend this host would use now. Only checks what the current session needs
/// (the Duo DLL is loaded only inside a Duo seat).
pub fn current() -> Option<Backend> {
	let wanted = duo_wanted();
	choose(wanted, wanted && duo_ok(), !wanted && usbip_ok())
}

/// A plugged-in virtual DualSense. Dropping it unplugs the device.
pub enum HostPad {
	Usbip(VirtualDs5),
	#[cfg(windows)]
	Duo(DuoDs5),
}

impl HostPad {
	/// Plug in a pad on the [`current`] backend. Blocks (usbip attach / Duo device
	/// discovery, a few seconds).
	pub fn start(cfg: VirtualDs5Config) -> std::io::Result<(Self, Receiver<Ds5HostEvent>)> {
		match current() {
			Some(Backend::Usbip) => VirtualDs5::start(cfg).map(|(p, rx)| (Self::Usbip(p), rx)),
			#[cfg(windows)]
			Some(Backend::Duo) => DuoDs5::start(false).map(|(p, rx)| (Self::Duo(p), rx)),
			#[cfg(not(windows))]
			Some(Backend::Duo) => unreachable!("Duo is Windows-only"),
			None => Err(std::io::Error::new(
				std::io::ErrorKind::NotFound,
				"no virtual DualSense backend (usbip not installed, or Duo seat without DuoController)",
			)),
		}
	}

	/// Short label for logs.
	pub fn describe(&self) -> String {
		match self {
			Self::Usbip(p) => format!("usbip port {}", p.port()),
			#[cfg(windows)]
			Self::Duo(p) => format!("duo {}", p.parent_instance_id().unwrap_or("?")),
		}
	}

	/// The game-facing input report (USB 0x01, 64 B).
	pub fn set_input(&self, usb_input: &[u8; USB_INPUT_LEN]) {
		match self {
			Self::Usbip(p) => p.set_input(usb_input),
			#[cfg(windows)]
			Self::Duo(p) => {
				let mut state = [0u8; STATE_LEN];
				state.copy_from_slice(&usb_input[1..]);
				p.send_raw(&mut state);
			}
		}
	}

	/// A feature report from the client's real pad. Duo's driver answers feature
	/// requests itself, so only USB/IP takes them.
	pub fn set_feature(&self, id: u8, data: &[u8]) {
		match self {
			Self::Usbip(p) => p.set_feature(id, data),
			#[cfg(windows)]
			Self::Duo(_) => {}
		}
	}

	/// Mic jitter buffer of the virtual pad. Duo's pad has no audio device: no-op.
	pub fn set_mic_prebuffer(&self, ms: u32) {
		match self {
			Self::Usbip(p) => p.set_mic_prebuffer(ms),
			#[cfg(windows)]
			Self::Duo(_) => {}
		}
	}

	/// Pad-mic audio (48 kHz stereo s16) for the game. Duo's pad has no audio device: dropped.
	pub fn push_mic(&self, pcm: &[i16]) {
		match self {
			Self::Usbip(p) => p.push_mic(pcm),
			#[cfg(windows)]
			Self::Duo(_) => {}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn duo_seat_uses_duo_only() {
		assert_eq!(choose(true, true, true), Some(Backend::Duo));
		assert_eq!(choose(true, true, false), Some(Backend::Duo));
		assert_eq!(
			choose(true, false, true),
			None,
			"no USB/IP fallback inside a Duo seat"
		);
	}

	#[test]
	fn console_uses_usbip() {
		assert_eq!(choose(false, true, true), Some(Backend::Usbip));
		assert_eq!(choose(false, false, true), Some(Backend::Usbip));
		assert_eq!(
			choose(false, true, false),
			None,
			"Duo is never used outside a Duo seat"
		);
	}
}
