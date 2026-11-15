//! Wi-Fi low-latency mode while a session streams (Windows).
//!
//! WHY: Windows' WLAN service scans the other channels in the background about every 40-60 s;
//! the card is off-channel for a few hundred ms and the stream loses a burst of packets each
//! time. 2026-11-15, this laptop hosting a friend over home Wi-Fi: 3-15 % loss bursts almost
//! exactly every 40 s even at 0.5 Mbit/s, the client's controller cut after each one (14 → 2
//! Mbit/s in ten minutes) and the picture showed the recovery spinner. Moonlight does the same
//! (`moonlight-common-c/src/PlatformSockets.c`): `wlan_intf_opcode_media_streaming_mode` on
//! every WLAN interface for the stream's lifetime — the setting holds only while the handle that
//! set it stays open, so the handle is kept until the last session ends. Background scanning is
//! switched off too where Windows allows it (it may need admin; failures are ignored).
//!
//! Used by both sides: a host streaming over Wi-Fi and a client receiving over Wi-Fi.
//! Refcounted: `acquire()` per session, the mode ends when the last guard drops.

/// Keeps low-latency Wi-Fi on while alive.
pub(crate) struct Guard(());

impl Drop for Guard {
	fn drop(&mut self) {
		imp::release();
	}
}

/// Turn the low-latency mode on (or keep it on) for one more session.
pub(crate) fn acquire() -> Guard {
	imp::acquire();
	Guard(())
}

#[cfg(windows)]
mod imp {
	use std::ffi::c_void;
	use std::sync::Mutex;

	#[repr(C)]
	struct Guid(u32, u16, u16, [u8; 8]);
	#[repr(C)]
	struct InterfaceInfo {
		guid: Guid,
		description: [u16; 256],
		state: u32,
	}
	#[repr(C)]
	struct InterfaceList {
		count: u32,
		index: u32,
		items: [InterfaceInfo; 1],
	}
	#[link(name = "wlanapi")]
	extern "system" {
		fn WlanOpenHandle(ver: u32, reserved: *mut c_void, negotiated: *mut u32, handle: *mut isize) -> u32;
		fn WlanCloseHandle(handle: isize, reserved: *mut c_void) -> u32;
		fn WlanEnumInterfaces(handle: isize, reserved: *mut c_void, list: *mut *mut InterfaceList) -> u32;
		fn WlanSetInterface(
			handle: isize,
			guid: *const Guid,
			opcode: u32,
			size: u32,
			data: *const c_void,
			reserved: *mut c_void,
		) -> u32;
		fn WlanFreeMemory(mem: *mut c_void);
	}
	/// `WLAN_INTF_OPCODE`: wlan_intf_opcode_background_scan_enabled / _media_streaming_mode.
	const OP_BACKGROUND_SCAN: u32 = 2;
	const OP_MEDIA_STREAMING: u32 = 3;

	/// (sessions holding it, the open WLAN handle; 0 = none).
	static STATE: Mutex<(usize, isize)> = Mutex::new((0, 0));

	pub(super) fn acquire() {
		let mut st = STATE.lock().unwrap();
		st.0 += 1;
		if st.1 != 0 {
			return;
		}
		// SAFETY: plain Win32 calls with valid out-pointers; the list is freed with WlanFreeMemory.
		unsafe {
			let mut ver = 0u32;
			let mut h: isize = 0;
			if WlanOpenHandle(2, std::ptr::null_mut(), &mut ver, &mut h) != 0 {
				return; // no WLAN service (desktop on Ethernet): nothing to do
			}
			let mut list: *mut InterfaceList = std::ptr::null_mut();
			if WlanEnumInterfaces(h, std::ptr::null_mut(), &mut list) != 0 || list.is_null() {
				WlanCloseHandle(h, std::ptr::null_mut());
				return;
			}
			let n = (*list).count as usize;
			let items = std::slice::from_raw_parts((*list).items.as_ptr(), n);
			let mut on = 0;
			for it in items {
				let yes: u32 = 1;
				let no: u32 = 0;
				if WlanSetInterface(h, &it.guid, OP_MEDIA_STREAMING, 4, &yes as *const u32 as *const c_void, std::ptr::null_mut())
					== 0
				{
					on += 1;
				}
				let _ = WlanSetInterface(h, &it.guid, OP_BACKGROUND_SCAN, 4, &no as *const u32 as *const c_void, std::ptr::null_mut());
			}
			WlanFreeMemory(list as *mut c_void);
			if on == 0 {
				WlanCloseHandle(h, std::ptr::null_mut());
				return;
			}
			tracing::info!(interfaces = on, "wifi: low-latency (media streaming) mode on for the session");
			st.1 = h;
		}
	}

	pub(super) fn release() {
		let mut st = STATE.lock().unwrap();
		st.0 = st.0.saturating_sub(1);
		if st.0 == 0 && st.1 != 0 {
			// SAFETY: the handle opened in `acquire`; closing it ends the mode (and re-enables
			// background scanning, which is per handle as well).
			unsafe { WlanCloseHandle(st.1, std::ptr::null_mut()) };
			st.1 = 0;
			tracing::info!("wifi: low-latency mode off (no session streams)");
		}
	}
}

#[cfg(not(windows))]
mod imp {
	// macOS / Linux: no comparable per-application switch (macOS: AWDL / location scans are
	// system-wide; Linux: NetworkManager background scans can be tuned only system-wide).
	pub(super) fn acquire() {}
	pub(super) fn release() {}
}
