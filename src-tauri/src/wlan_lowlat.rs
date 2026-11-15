//! Wi-Fi low-latency mode while a session streams (Windows; macOS / Linux: a logged hint, see
//! the non-Windows `imp`).
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

/// macOS / Linux: no per-application switch exists, and the system-wide ones need root, so
/// nothing is changed — the first session LOGS what would cost latency, with the command that
/// fixes it (the person running the machine decides):
///
/// * **Linux** — Wi-Fi power save (`iw dev <if> get power_save` = on; most laptop drivers'
///   default: the card sleeps between beacons and adds delay in bursts). Off with
///   `sudo iw dev <if> set power_save off`, or NetworkManager's `wifi.powersave = 2`.
///   NetworkManager's wpa_supplicant `bgscan` (a scan every ~30 s while the signal is weak) is
///   the Linux twin of the Windows background scan; no user-level switch exists for it (a fixed
///   BSSID on the connection disables it — a persistent change, so not made here).
/// * **macOS** — AWDL (`awdl0`: AirDrop / AirPlay / Sidecar) hops the radio to its own channel
///   several times a second: periodic delay spikes on a Wi-Fi stream. Off with
///   `sudo ifconfig awdl0 down` (macOS brings it back when needed). Location services' Wi-Fi
///   scans are system-wide too.
///
/// Untested on Linux / macOS hardware.
#[cfg(not(windows))]
mod imp {
	use std::sync::Once;

	static HINT: Once = Once::new();

	pub(super) fn acquire() {
		// Off the session's thread: the tools may be slow or missing.
		HINT.call_once(|| {
			std::thread::spawn(|| {
				for line in hints() {
					tracing::warn!("wifi: {line}");
				}
			});
		});
	}

	pub(super) fn release() {}

	#[cfg(target_os = "linux")]
	fn hints() -> Vec<String> {
		let mut v = Vec::new();
		let Ok(dir) = std::fs::read_dir("/sys/class/net") else { return v };
		for e in dir.flatten() {
			if !e.path().join("wireless").exists() {
				continue;
			}
			let ifname = e.file_name().to_string_lossy().into_owned();
			let Ok(o) = std::process::Command::new("iw").args(["dev", &ifname, "get", "power_save"]).output() else {
				continue;
			};
			if super::power_save_on(&String::from_utf8_lossy(&o.stdout)) {
				v.push(format!(
					"{ifname}: power save is ON — it adds delay bursts to a stream; off with `sudo iw dev {ifname} set power_save off` (or NetworkManager wifi.powersave = 2)"
				));
			}
		}
		v
	}

	#[cfg(target_os = "macos")]
	fn hints() -> Vec<String> {
		match std::process::Command::new("/sbin/ifconfig").arg("awdl0").output() {
			Ok(o) if super::awdl_active(&String::from_utf8_lossy(&o.stdout)) => vec![
				"awdl0 (AirDrop / AirPlay) is active — it takes the radio off channel and spikes a Wi-Fi stream's delay; `sudo ifconfig awdl0 down` turns it off until it is needed".to_string(),
			],
			_ => Vec::new(),
		}
	}

	#[cfg(not(any(target_os = "linux", target_os = "macos")))]
	fn hints() -> Vec<String> {
		Vec::new()
	}
}

/// `iw dev <if> get power_save` says it is on.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn power_save_on(iw: &str) -> bool {
	iw.lines().any(|l| l.trim().eq_ignore_ascii_case("power save: on"))
}

/// `ifconfig awdl0` shows the interface up and active.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn awdl_active(ifconfig: &str) -> bool {
	let up = ifconfig.lines().next().is_some_and(|l| l.contains("<UP") || l.contains(",UP"));
	up && ifconfig.lines().any(|l| l.trim() == "status: active")
}

#[cfg(test)]
mod tests {
	#[test]
	fn reads_the_tools_output() {
		assert!(super::power_save_on("Power save: on\n"));
		assert!(!super::power_save_on("Power save: off\n"));
		let awdl = "awdl0: flags=8943<UP,BROADCAST,RUNNING,PROMISC,SIMPLEX,MULTICAST> mtu 1484\n\tstatus: active\n";
		assert!(super::awdl_active(awdl));
		assert!(!super::awdl_active("awdl0: flags=8902<BROADCAST,PROMISC,SIMPLEX,MULTICAST> mtu 1484\n\tstatus: inactive\n"));
	}
}
