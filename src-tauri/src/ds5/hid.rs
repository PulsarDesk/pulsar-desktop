//! Raw HID access to a physical DualSense through SDL3's hidapi (`SDL_hid_*`), so the
//! app keeps one HID stack. Implements `pulsar_core::input::ds5::pad::HidIo`.
//!
//! SDL's own PS5 gamepad driver keeps its handle for normal input (UI navigation, the
//! non-DS5 path). This second handle reads the raw reports and writes the host's output.
//! While a pad is in passthrough, the app must not send SDL rumble/LED to it (two writers
//! would fight over the lightbar and BT sequence numbers).
//!
//! Open question (step 6): Windows hidapi pads every write to the device's largest output
//! report (547 B on a BT DualSense), which may cost BT airtime on 78-byte 0x31 reports.
//! ZerkSense5 writes the exact length with `WriteFile`. Measure with `PadStats` first.

use std::ffi::{CStr, CString};
use std::io;

use pulsar_core::input::ds5::pad::HidIo;
use pulsar_core::input::ds5::report::{PID_DUALSENSE, PID_DUALSENSE_EDGE, VID_SONY};
use pulsar_core::input::ds5::wire::Ds5Transport;
use sdl3_sys::hidapi::{
	SDL_hid_close, SDL_hid_device, SDL_hid_enumerate, SDL_hid_free_enumeration,
	SDL_hid_get_feature_report, SDL_hid_init, SDL_hid_open_path, SDL_hid_read_timeout,
	SDL_hid_write, SDL_HID_API_BUS_BLUETOOTH,
};

/// A DualSense found by enumeration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ds5Device {
	pub path: CString,
	pub transport: Ds5Transport,
	/// The pad's serial (its BT MAC on most platforms), for matching it to an SDL gamepad.
	pub serial: String,
	pub product_id: u16,
}

/// Every connected DualSense / DualSense Edge HID interface.
pub fn enumerate() -> Vec<Ds5Device> {
	let mut out = Vec::new();
	unsafe {
		if SDL_hid_init() != 0 {
			return out;
		}
		let head = SDL_hid_enumerate(VID_SONY, 0);
		let mut cur = head;
		while !cur.is_null() {
			let d = &*cur;
			if (d.product_id == PID_DUALSENSE || d.product_id == PID_DUALSENSE_EDGE)
				&& !d.path.is_null()
			{
				let serial = if d.serial_number.is_null() {
					String::new()
				} else {
					wide_to_string(d.serial_number)
				};
				out.push(Ds5Device {
					path: CStr::from_ptr(d.path).to_owned(),
					transport: if d.bus_type == SDL_HID_API_BUS_BLUETOOTH {
						Ds5Transport::Bt
					} else {
						Ds5Transport::Usb
					},
					serial,
					product_id: d.product_id,
				});
			}
			cur = d.next;
		}
		SDL_hid_free_enumeration(head);
	}
	out
}

unsafe fn wide_to_string(p: *const sdl3_sys::ffi::c_wchar_t) -> String {
	let mut v = Vec::new();
	let mut i = 0;
	loop {
		let c = *p.add(i);
		if c == 0 || i > 256 {
			break;
		}
		v.push(c as u32);
		i += 1;
	}
	v.into_iter().filter_map(char::from_u32).collect()
}

/// An open DualSense HID handle.
pub struct SdlHid {
	dev: *mut SDL_hid_device,
}

// SAFETY: hidapi device handles may be used from any one thread at a time; `SdlHid` is
// owned by the single pad thread.
unsafe impl Send for SdlHid {}

impl SdlHid {
	pub fn open(d: &Ds5Device) -> io::Result<Self> {
		let dev = unsafe { SDL_hid_open_path(d.path.as_ptr()) };
		if dev.is_null() {
			return Err(io::Error::other(format!(
				"SDL_hid_open_path failed: {}",
				sdl_error()
			)));
		}
		Ok(Self { dev })
	}

	/// Read one input report, waiting up to `ms` (0 = poll). `Ok(0)` on timeout.
	pub fn read_timeout(&mut self, buf: &mut [u8], ms: i32) -> io::Result<usize> {
		let n = unsafe { SDL_hid_read_timeout(self.dev, buf.as_mut_ptr(), buf.len(), ms) };
		if n < 0 {
			return Err(io::Error::other(format!(
				"SDL_hid_read_timeout: {}",
				sdl_error()
			)));
		}
		Ok(n as usize)
	}
}

impl HidIo for SdlHid {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		let n = unsafe { SDL_hid_write(self.dev, buf.as_ptr(), buf.len()) };
		if n < 0 {
			return Err(io::Error::other(format!("SDL_hid_write: {}", sdl_error())));
		}
		Ok(n as usize)
	}

	fn get_feature(&mut self, id: u8, len: usize) -> io::Result<Vec<u8>> {
		let mut buf = vec![0u8; len];
		buf[0] = id;
		let n = unsafe { SDL_hid_get_feature_report(self.dev, buf.as_mut_ptr(), buf.len()) };
		if n < 0 {
			return Err(io::Error::other(format!(
				"feature 0x{id:02x}: {}",
				sdl_error()
			)));
		}
		buf.truncate(n as usize);
		Ok(buf)
	}
}

impl Drop for SdlHid {
	fn drop(&mut self) {
		unsafe {
			SDL_hid_close(self.dev);
		}
	}
}

fn sdl_error() -> String {
	unsafe {
		let p = sdl3_sys::error::SDL_GetError();
		if p.is_null() {
			String::new()
		} else {
			CStr::from_ptr(p).to_string_lossy().into_owned()
		}
	}
}
