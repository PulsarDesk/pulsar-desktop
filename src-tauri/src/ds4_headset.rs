//! DualShock 4 headset jack on the CLIENT (experimental, USB only; default off). Windows,
//! Linux and macOS: the same watcher; the endpoint match is per OS (`ds5::endpoint`).
//!
//! With headphones in a USB-cabled DS4's 3.5 mm jack, the session audio plays on the pad's
//! own USB audio device (which drives the jack) instead of this PC's default output —
//! the DualSense feature of the same name without passthrough (`ds5/headset.rs`). Pulled
//! out, or the pad unplugged: back to the default output.
//!
//! - **Jack state:** a second, read-only HID handle on the pad (SDL's gamepad driver keeps
//!   its own); the USB input report 0x01 byte 30 carries battery (bits 0-3), cable (bit 4),
//!   headphones (bit 5) and mic (bit 6). Checked 2026-11-12 against psdevwiki's DS4-USB table
//!   and ds4drv (both 0x20 = headphones); Linux `hid-playstation` and DS4Windows read the
//!   same byte for battery/cable only. **Not verified on hardware** (no DS4 at hand).
//! - **Which pads:** only the DS4 v2 (CUH-ZCT2) on a cable and Sony's USB wireless adapter
//!   expose a USB audio device; the v1 (CUH-ZCT1) does not (DS4Windows docs), so it is not
//!   watched.
//! - **Volume:** nothing is written to the pad. DS4Windows dropped the volume flags from its
//!   output report (0xF7 → 0x07) and the jack plays at the pad's own level, which Windows'
//!   volume for that device controls.
//! - **Endpoint:** the DualSense matcher with the DS4 family (`ds5::endpoint::find_family`):
//!   Windows by ContainerId from the HID path; Linux by the pad's sysfs sound card (only
//!   while it is the one DS4 card) plus a unique "Wireless Controller" SDL device; macOS only
//!   with exactly one USB DS4 and one such SDL device. Linux needs hidraw access
//!   (`70-pulsar-ds5.rules` covers 09cc/0ba0; Steam's rules usually do too).
//! - **Playback:** `native_audio` asks [`target`] for each packet and reopens its SDL stream
//!   on the pad's device (or back on the default one) when it changes. SDL converts 48 kHz
//!   stereo to whatever the pad's device takes.
//! - Bluetooth DS4s are not covered: their audio is Sony-specific HID reports (SBC), with no
//!   audio device to open.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);
/// The SDL playback device the session audio should use (0 = the default output).
static TARGET: AtomicU32 = AtomicU32::new(0);
/// The watcher thread runs.
static WATCHING: AtomicBool = AtomicBool::new(false);

/// Settings → Kumandalar → "DS4 kulaklık girişi". Starts/stops the jack watcher.
pub fn set_enabled(on: bool) {
	ENABLED.store(on, Ordering::SeqCst);
	if !on {
		TARGET.store(0, Ordering::SeqCst);
		return;
	}
	if !WATCHING.swap(true, Ordering::SeqCst) {
		if let Err(e) = std::thread::Builder::new().name("ds4-headset".into()).spawn(imp::watch) {
			WATCHING.store(false, Ordering::SeqCst);
			tracing::warn!("ds4 headset: watcher failed to start: {e}");
		}
	}
}

/// The SDL playback device for the session audio: `Some(id)` = a DS4's jack, `None` = the
/// default output.
pub fn target() -> Option<u32> {
	match TARGET.load(Ordering::SeqCst) {
		0 => None,
		id => Some(id),
	}
}

/// DS4s with a USB audio device: CUH-ZCT2 and the USB wireless adapter (not the v1 0x05c4).
pub(crate) const DS4_PIDS: [u16; 2] = [0x09cc, 0x0ba0];

/// Headphones plugged, from a USB input report as hidapi returns it (report id at [0]).
/// `None` = not a full 0x01 report.
pub(crate) fn headphones_in(report: &[u8]) -> Option<bool> {
	if report.len() < 31 || report[0] != 0x01 {
		return None;
	}
	Some(report[30] & 0x20 != 0)
}

/// HID paths of the USB-connected DS4s (v2 / adapter). Used by the macOS endpoint matcher.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn usb_ds4_paths() -> Vec<std::ffi::CString> {
	imp::usb_ds4_paths()
}

mod imp {
	use super::{headphones_in, DS4_PIDS, ENABLED, TARGET, WATCHING};
	use std::ffi::{CStr, CString};
	use std::sync::atomic::Ordering;
	use std::time::Duration;

	use sdl3_sys::hidapi::{
		SDL_hid_close, SDL_hid_enumerate, SDL_hid_free_enumeration, SDL_hid_init,
		SDL_hid_open_path, SDL_hid_read_timeout, SDL_HID_API_BUS_BLUETOOTH,
	};

	/// Every USB-connected DS4's HID path.
	pub(super) fn usb_ds4_paths() -> Vec<CString> {
		let mut out = Vec::new();
		// SAFETY: plain SDL3 hidapi enumeration; the list is freed before returning.
		unsafe {
			if SDL_hid_init() != 0 {
				return out;
			}
			let head = SDL_hid_enumerate(0x054c, 0);
			let mut cur = head;
			while !cur.is_null() {
				let d = &*cur;
				if DS4_PIDS.contains(&d.product_id) && d.bus_type != SDL_HID_API_BUS_BLUETOOTH && !d.path.is_null() {
					let p = CStr::from_ptr(d.path).to_owned();
					if !out.contains(&p) {
						out.push(p);
					}
				}
				cur = d.next;
			}
			SDL_hid_free_enumeration(head);
		}
		out
	}

	/// The first USB-connected DS4's HID path.
	fn usb_ds4() -> Option<CString> {
		usb_ds4_paths().into_iter().next()
	}

	/// Read the jack bit from the pad at `path` (one report, up to 100 ms).
	fn read_jack(path: &CStr) -> Option<bool> {
		// SAFETY: open → read → close on this thread; the handle never escapes.
		unsafe {
			let dev = SDL_hid_open_path(path.as_ptr());
			if dev.is_null() {
				return None;
			}
			let mut buf = [0u8; 64];
			let mut got = None;
			for _ in 0..4 {
				let n = SDL_hid_read_timeout(dev, buf.as_mut_ptr(), buf.len(), 25);
				if n <= 0 {
					continue;
				}
				if let Some(j) = headphones_in(&buf[..n as usize]) {
					got = Some(j);
					break;
				}
			}
			SDL_hid_close(dev);
			got
		}
	}

	/// 2 Hz: find a USB DS4, read its jack, point TARGET at its playback device while the
	/// headphones are in.
	pub(super) fn watch() {
		use sdl3_sys::init::{SDL_InitSubSystem, SDL_QuitSubSystem, SDL_INIT_AUDIO};
		// SAFETY: AUDIO is its own subsystem, reference-counted and safe off the main thread.
		let audio = unsafe { SDL_InitSubSystem(SDL_INIT_AUDIO) };
		if !audio {
			tracing::warn!("ds4 headset: SDL audio init failed — feature inactive");
		}
		tracing::info!("ds4 headset: watcher up");
		let mut last: Option<(CString, bool)> = None;
		while ENABLED.load(Ordering::SeqCst) {
			let state = usb_ds4().and_then(|p| read_jack(&p).map(|j| (p, j)));
			if state != last {
				let target = match &state {
					Some((path, true)) if audio => crate::ds5::endpoint::find_family(
						path,
						crate::ds5::endpoint::Kind::Playback,
						crate::ds5::endpoint::Family::Ds4,
					)
						.map(|e| e.id.0)
						.unwrap_or(0),
					_ => 0,
				};
				tracing::info!(
					pad = state.is_some(),
					headphones = state.as_ref().is_some_and(|s| s.1),
					target,
					"ds4 headset: state changed"
				);
				TARGET.store(target, Ordering::SeqCst);
				last = state;
			}
			std::thread::sleep(Duration::from_millis(500));
		}
		TARGET.store(0, Ordering::SeqCst);
		if audio {
			// SAFETY: balances the init above.
			unsafe { SDL_QuitSubSystem(SDL_INIT_AUDIO) };
		}
		WATCHING.store(false, Ordering::SeqCst);
		tracing::info!("ds4 headset: watcher stopped");
	}
}

#[cfg(test)]
mod tests {
	use super::headphones_in;

	fn report(status: u8) -> Vec<u8> {
		let mut r = vec![0u8; 64];
		r[0] = 0x01;
		r[30] = status;
		r
	}

	#[test]
	fn the_jack_bit_is_byte_30_bit_5() {
		assert_eq!(headphones_in(&report(0x1b)), Some(false)); // cable + battery only
		assert_eq!(headphones_in(&report(0x3b)), Some(true)); // + headphones
		assert_eq!(headphones_in(&report(0x7b)), Some(true)); // + headset mic
	}

	#[test]
	fn other_reports_are_ignored() {
		let mut r = report(0x3b);
		r[0] = 0x11; // BT report
		assert_eq!(headphones_in(&r), None);
		assert_eq!(headphones_in(&[0x01; 10]), None);
	}
}
