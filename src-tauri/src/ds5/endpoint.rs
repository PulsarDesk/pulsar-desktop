//! Find a USB DualSense's own audio endpoints (speaker/haptics playback, mic recording) as
//! SDL3 audio devices, from the pad's HID device.
//!
//! Never by name alone: a cabled real pad and Pulsar's virtual pad (or two real pads) are
//! all called "DualSense Wireless Controller" (this picked the wrong pad in testing,
//! 2026-11-11). Per OS:
//!
//! - **Windows**: the HID interface path → its devnode (`DEVPKEY_Device_InstanceId`) →
//!   `DEVPKEY_Device_ContainerId`. Every present MMDevice endpoint (device interfaces
//!   `DEVINTERFACE_AUDIO_RENDER` / `_CAPTURE`, devnodes `SWD\MMDEVAPI\…`) in the same
//!   container is the pad's. Its devnode `FriendlyName` is what WASAPI reports as
//!   `PKEY_Device_FriendlyName` and SDL3 as the device name ("Speakers (2- DualSense
//!   Wireless Controller)": Windows numbers duplicates), so the SDL device is the one with
//!   exactly that name; zero or several such SDL devices → no match. The shared
//!   "this computer" container (`{00000000-0000-0000-ffff-ffffffffffff}`, given to
//!   non-removable devices) is never matched. CfgMgr32 only, no COM.
//! - **Linux**: sysfs. `/sys/class/hidraw/hidrawN/device` → the USB device directory
//!   (the ancestor with `busnum`) → `…/<interface>/sound/cardK`. SDL's PipeWire/Pulse
//!   device names don't carry the ALSA card, so a name match is accepted only when that
//!   card is the **only** DualSense sound card on the system and exactly one SDL device
//!   is a DualSense; otherwise none (logged). Driving `hw:CARD=…` through ALSA directly
//!   would lift that limit (later).
//! - **macOS** (and anything else): no container lookup; accepted only when there is
//!   exactly one USB DualSense HID device and exactly one SDL DualSense device of the
//!   kind (no virtual pads exist on macOS hosts).

use std::ffi::CStr;

/// One SDL audio device the pad owns.
#[derive(Clone)]
pub struct Endpoint {
	pub id: sdl3_sys::audio::SDL_AudioDeviceID,
	pub name: String,
}

/// Playback (speaker + haptics) or recording (mic).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
	Playback,
	Recording,
}

/// The SDL devices of `kind` (id, name). SDL's AUDIO subsystem must be initialized.
pub fn sdl_devices(kind: Kind) -> Vec<(sdl3_sys::audio::SDL_AudioDeviceID, String)> {
	use sdl3_sys::audio::{
		SDL_GetAudioDeviceName, SDL_GetAudioPlaybackDevices, SDL_GetAudioRecordingDevices,
	};
	let mut out = Vec::new();
	// SAFETY: plain SDL3 calls; the id array is SDL-allocated and freed with SDL_free.
	unsafe {
		let mut n: std::ffi::c_int = 0;
		let ids = match kind {
			Kind::Playback => SDL_GetAudioPlaybackDevices(&mut n),
			Kind::Recording => SDL_GetAudioRecordingDevices(&mut n),
		};
		if ids.is_null() {
			return out;
		}
		for i in 0..n.max(0) as usize {
			let id = *ids.add(i);
			let p = SDL_GetAudioDeviceName(id);
			if !p.is_null() {
				out.push((id, CStr::from_ptr(p).to_string_lossy().into_owned()));
			}
		}
		sdl3_sys::stdinc::SDL_free(ids.cast());
	}
	out
}

/// The one entry whose name is exactly `want` (case-insensitive); `None` for zero or many.
pub fn unique_exact<'a, T>(devs: &'a [(T, String)], want: &str) -> Option<&'a (T, String)> {
	let mut it = devs.iter().filter(|(_, n)| n.eq_ignore_ascii_case(want));
	let first = it.next()?;
	it.next().is_none().then_some(first)
}

/// The one entry that looks like a DualSense; `None` for zero or many.
#[cfg_attr(windows, allow(dead_code))]
pub fn unique_dualsense<T>(devs: &[(T, String)]) -> Option<&(T, String)> {
	let mut it = devs
		.iter()
		.filter(|(_, n)| n.to_ascii_lowercase().contains("dualsense"));
	let first = it.next()?;
	it.next().is_none().then_some(first)
}

/// The SDL device of `kind` that belongs to the USB pad at HID `path`, if it can be told
/// apart for sure (module docs). SDL's AUDIO subsystem must be initialized.
pub fn find(hid_path: &CStr, kind: Kind) -> Option<Endpoint> {
	let devs = sdl_devices(kind);
	let hit = os::find(hid_path, kind, &devs);
	match &hit {
		Some(e) => tracing::info!(?kind, name = %e.name, "ds5 usb audio: pad endpoint found"),
		None => tracing::warn!(
			?kind,
			sdl_devices = ?devs.iter().map(|d| &d.1).collect::<Vec<_>>(),
			"ds5 usb audio: no endpoint that is surely this pad's"
		),
	}
	hit
}

#[cfg(windows)]
mod os {
	use super::{unique_exact, Endpoint, Kind};
	use std::ffi::CStr;
	use windows_sys::core::GUID;
	use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
		CM_Get_DevNode_PropertyW, CM_Get_Device_Interface_ListW,
		CM_Get_Device_Interface_List_SizeW, CM_Get_Device_Interface_PropertyW, CM_Locate_DevNodeW,
		CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_LOCATE_DEVNODE_NORMAL, CR_SUCCESS,
	};
	use windows_sys::Win32::Devices::Properties::{
		DEVPKEY_Device_ContainerId, DEVPKEY_Device_FriendlyName, DEVPKEY_Device_InstanceId,
	};
	use windows_sys::Win32::Foundation::DEVPROPKEY;

	/// mmdeviceapi.h `DEVINTERFACE_AUDIO_RENDER` / `DEVINTERFACE_AUDIO_CAPTURE`.
	const AUDIO_RENDER: GUID = GUID::from_u128(0xe6327cad_dcec_4949_ae8a_991e976a79d2);
	const AUDIO_CAPTURE: GUID = GUID::from_u128(0x2eef81be_33fa_4800_9670_1cd474972c3f);
	/// The container of non-removable devices ("this computer"): never a match.
	const COMPUTER_CONTAINER: u128 = 0x00000000_0000_0000_ffff_ffffffffffff;

	fn wide(s: &str) -> Vec<u16> {
		s.encode_utf16().chain(std::iter::once(0)).collect()
	}

	fn from_wide(b: &[u8]) -> String {
		let w: Vec<u16> = b
			.chunks_exact(2)
			.map(|c| u16::from_le_bytes([c[0], c[1]]))
			.take_while(|&c| c != 0)
			.collect();
		String::from_utf16_lossy(&w)
	}

	fn iface_prop(iface: &[u16], key: &DEVPROPKEY) -> Option<Vec<u8>> {
		let mut ty = 0u32;
		let mut buf = vec![0u8; 1024];
		let mut len = buf.len() as u32;
		// SAFETY: `iface` is NUL-terminated; buffer and size are valid.
		let r = unsafe {
			CM_Get_Device_Interface_PropertyW(
				iface.as_ptr(),
				key,
				&mut ty,
				buf.as_mut_ptr(),
				&mut len,
				0,
			)
		};
		(r == CR_SUCCESS).then(|| {
			buf.truncate(len as usize);
			buf
		})
	}

	fn node_prop(inst: u32, key: &DEVPROPKEY) -> Option<Vec<u8>> {
		let mut ty = 0u32;
		let mut buf = vec![0u8; 1024];
		let mut len = buf.len() as u32;
		// SAFETY: buffer and size are valid.
		let r =
			unsafe { CM_Get_DevNode_PropertyW(inst, key, &mut ty, buf.as_mut_ptr(), &mut len, 0) };
		(r == CR_SUCCESS).then(|| {
			buf.truncate(len as usize);
			buf
		})
	}

	/// Devnode of a device interface (NUL-terminated wide path).
	fn iface_devnode(iface: &[u16]) -> Option<u32> {
		let id = iface_prop(iface, &DEVPKEY_Device_InstanceId)?;
		let id: Vec<u16> = id
			.chunks_exact(2)
			.map(|c| u16::from_le_bytes([c[0], c[1]]))
			.collect();
		let mut inst = 0u32;
		// SAFETY: `id` is the NUL-terminated instance id CfgMgr returned.
		let r = unsafe { CM_Locate_DevNodeW(&mut inst, id.as_ptr(), CM_LOCATE_DEVNODE_NORMAL) };
		(r == CR_SUCCESS).then_some(inst)
	}

	fn container(inst: u32) -> Option<u128> {
		let b = node_prop(inst, &DEVPKEY_Device_ContainerId)?;
		let g: [u8; 16] = b.get(..16)?.try_into().ok()?;
		// GUID memory layout: data1 LE u32, data2 LE u16, data3 LE u16, data4 bytes.
		let d1 = u32::from_le_bytes([g[0], g[1], g[2], g[3]]) as u128;
		let d2 = u16::from_le_bytes([g[4], g[5]]) as u128;
		let d3 = u16::from_le_bytes([g[6], g[7]]) as u128;
		let d4 = u64::from_be_bytes(g[8..16].try_into().ok()?) as u128;
		Some(d1 << 96 | d2 << 80 | d3 << 64 | d4)
	}

	/// Present device interfaces of `class` (NUL-terminated wide strings).
	fn interfaces(class: &GUID) -> Vec<Vec<u16>> {
		let mut len = 0u32;
		// SAFETY: valid out-pointer and class GUID.
		if unsafe {
			CM_Get_Device_Interface_List_SizeW(
				&mut len,
				class,
				std::ptr::null(),
				CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
			)
		} != CR_SUCCESS
		{
			return Vec::new();
		}
		let mut buf = vec![0u16; len as usize + 1];
		// SAFETY: buffer of `len` + 1 wide chars.
		if unsafe {
			CM_Get_Device_Interface_ListW(
				class,
				std::ptr::null(),
				buf.as_mut_ptr(),
				buf.len() as u32,
				CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
			)
		} != CR_SUCCESS
		{
			return Vec::new();
		}
		buf.split(|&c| c == 0)
			.filter(|s| !s.is_empty())
			.map(|s| s.iter().copied().chain(std::iter::once(0)).collect())
			.collect()
	}

	/// Friendly names of the present audio endpoints of `kind` in container `c`.
	fn endpoint_names(kind: Kind, c: u128) -> Vec<String> {
		let class = match kind {
			Kind::Playback => &AUDIO_RENDER,
			Kind::Recording => &AUDIO_CAPTURE,
		};
		interfaces(class)
			.iter()
			.filter_map(|i| iface_devnode(i))
			.filter(|&n| container(n) == Some(c))
			.filter_map(|n| node_prop(n, &DEVPKEY_Device_FriendlyName).map(|b| from_wide(&b)))
			.collect()
	}

	/// Every present endpoint of `kind` as (container, friendly name), for diagnostics.
	#[allow(dead_code)]
	pub fn all_endpoints(kind: Kind) -> Vec<(u128, String)> {
		let class = match kind {
			Kind::Playback => &AUDIO_RENDER,
			Kind::Recording => &AUDIO_CAPTURE,
		};
		interfaces(class)
			.iter()
			.filter_map(|i| iface_devnode(i))
			.filter_map(|n| {
				Some((
					container(n)?,
					from_wide(&node_prop(n, &DEVPKEY_Device_FriendlyName)?),
				))
			})
			.collect()
	}

	pub fn find(
		hid_path: &CStr,
		kind: Kind,
		devs: &[(sdl3_sys::audio::SDL_AudioDeviceID, String)],
	) -> Option<Endpoint> {
		let iface = wide(&hid_path.to_string_lossy());
		let c = container(iface_devnode(&iface)?)?;
		if c == COMPUTER_CONTAINER || c == 0 {
			tracing::warn!(
				"ds5 usb audio: pad is in the computer's container; can't match its audio"
			);
			return None;
		}
		let names = endpoint_names(kind, c);
		if names.len() != 1 {
			tracing::warn!(
				?kind,
				?names,
				"ds5 usb audio: expected one endpoint in the pad's container"
			);
			return None;
		}
		unique_exact(devs, &names[0]).map(|(id, name)| Endpoint {
			id: *id,
			name: name.clone(),
		})
	}
}

#[cfg(target_os = "linux")]
mod os {
	use super::{unique_dualsense, Endpoint, Kind};
	use std::ffi::CStr;
	use std::path::Path;

	pub fn find(
		hid_path: &CStr,
		_kind: Kind,
		devs: &[(sdl3_sys::audio::SDL_AudioDeviceID, String)],
	) -> Option<Endpoint> {
		let node = hid_path.to_str().ok()?.rsplit('/').next()?.to_string(); // hidrawN
		let dev = std::fs::canonicalize(format!("/sys/class/hidraw/{node}/device")).ok()?;
		let usb = super::linux::usb_device_dir(&dev)?;
		let mine = super::linux::sound_cards(&usb);
		let all = super::linux::dualsense_cards(Path::new("/sys/class/sound"));
		if mine.len() != 1 || all.len() != 1 {
			tracing::warn!(
				?mine,
				?all,
				"ds5 usb audio: pad's sound card not unique; no audio"
			);
			return None;
		}
		unique_dualsense(devs).map(|(id, name)| Endpoint {
			id: *id,
			name: name.clone(),
		})
	}
}

#[cfg(not(any(windows, target_os = "linux")))]
mod os {
	use super::{unique_dualsense, Endpoint, Kind};
	use std::ffi::CStr;

	pub fn find(
		_hid_path: &CStr,
		_kind: Kind,
		devs: &[(sdl3_sys::audio::SDL_AudioDeviceID, String)],
	) -> Option<Endpoint> {
		use pulsar_core::input::ds5::wire::Ds5Transport;
		let usb = super::super::hid::enumerate()
			.into_iter()
			.filter(|d| d.transport == Ds5Transport::Usb)
			.count();
		if usb != 1 {
			return None;
		}
		unique_dualsense(devs).map(|(id, name)| Endpoint {
			id: *id,
			name: name.clone(),
		})
	}
}

/// sysfs walking (plain directories, so it is testable anywhere).
#[allow(dead_code)]
pub mod linux {
	use std::path::{Path, PathBuf};

	/// The USB device directory above a HID device's sysfs dir (first ancestor with
	/// `busnum`).
	pub fn usb_device_dir(hid_dev: &Path) -> Option<PathBuf> {
		hid_dev
			.ancestors()
			.find(|p| p.join("busnum").is_file())
			.map(Path::to_path_buf)
	}

	/// Sound cards (`cardK` names) under a USB device's interfaces.
	pub fn sound_cards(usb_dir: &Path) -> Vec<String> {
		let mut out = Vec::new();
		let Ok(rd) = std::fs::read_dir(usb_dir) else {
			return out;
		};
		for e in rd.flatten() {
			let Ok(cards) = std::fs::read_dir(e.path().join("sound")) else {
				continue;
			};
			for c in cards.flatten() {
				let n = c.file_name().to_string_lossy().into_owned();
				if n.starts_with("card")
					&& n[4..].chars().all(|ch| ch.is_ascii_digit())
					&& n.len() > 4
				{
					out.push(n);
				}
			}
		}
		out.sort();
		out
	}

	/// DualSense sound cards on the system: `cardK` entries of `/sys/class/sound` whose
	/// USB device (`cardK/device/../idVendor` + `idProduct`) is Sony 0ce6 / 0df2.
	pub fn dualsense_cards(class_sound: &Path) -> Vec<String> {
		let mut out = Vec::new();
		let Ok(rd) = std::fs::read_dir(class_sound) else {
			return out;
		};
		for e in rd.flatten() {
			let n = e.file_name().to_string_lossy().into_owned();
			if !(n.starts_with("card") && n.len() > 4 && n[4..].chars().all(|c| c.is_ascii_digit()))
			{
				continue;
			}
			let dev = std::fs::canonicalize(e.path().join("device"))
				.unwrap_or_else(|_| e.path().join("device"));
			let Some(usb) = usb_device_dir(&dev) else {
				continue;
			};
			let read = |f: &str| {
				std::fs::read_to_string(usb.join(f))
					.map(|s| s.trim().to_ascii_lowercase())
					.unwrap_or_default()
			};
			if read("idVendor") == "054c" && matches!(read("idProduct").as_str(), "0ce6" | "0df2") {
				out.push(n);
			}
		}
		out.sort();
		out
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn exact_and_dualsense_picks_need_one_match() {
		let devs = vec![
			(1u32, "Speakers (Realtek(R) Audio)".to_string()),
			(2, "Speakers (DualSense Wireless Controller)".to_string()),
			(3, "Speakers (2- DualSense Wireless Controller)".to_string()),
		];
		assert_eq!(
			unique_exact(&devs, "speakers (2- dualsense wireless controller)").map(|d| d.0),
			Some(3)
		);
		assert_eq!(
			unique_exact(&devs, "Speakers (3- DualSense Wireless Controller)"),
			None
		);
		assert_eq!(
			unique_dualsense(&devs),
			None,
			"two DualSense endpoints: ambiguous"
		);
		assert_eq!(unique_dualsense(&devs[..2]).map(|d| d.0), Some(2));
		let dup = vec![(1u32, "X".to_string()), (2, "x".to_string())];
		assert_eq!(unique_exact(&dup, "X"), None, "duplicate names: refuse");
	}

	#[test]
	fn sysfs_walk_finds_the_pads_card() {
		let root = std::env::temp_dir().join(format!("pulsar-ds5-sysfs-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&root);
		// Real sysfs names have ':' ("1-2:1.0"); '_' here so the test runs on Windows too.
		let usb = root.join("devices/pci0000_00/usb1/1-2");
		let hid = usb.join("1-2_1.3/0003_054C_0CE6.0007/hidraw/hidraw5");
		std::fs::create_dir_all(&hid).unwrap();
		std::fs::create_dir_all(usb.join("1-2_1.0/sound/card2")).unwrap();
		std::fs::create_dir_all(usb.join("1-2_1.0/sound/controlC2")).unwrap();
		std::fs::write(usb.join("busnum"), "1\n").unwrap();
		std::fs::write(usb.join("idVendor"), "054c\n").unwrap();
		std::fs::write(usb.join("idProduct"), "0ce6\n").unwrap();
		assert_eq!(linux::usb_device_dir(&hid), Some(usb.clone()));
		assert_eq!(linux::sound_cards(&usb), vec!["card2".to_string()]);
		// /sys/class/sound/cardK/device is a symlink on Linux; here a plain dir under the
		// USB device stands in for its target.
		let class = root.join("class/sound");
		let card_dev = usb.join("1-2_1.0/sound/card2/device");
		std::fs::create_dir_all(&card_dev).unwrap();
		std::fs::create_dir_all(class.join("controlC2")).unwrap();
		#[cfg(unix)]
		{
			std::fs::create_dir_all(class.join("card2")).unwrap();
			std::os::unix::fs::symlink(&card_dev, class.join("card2/device")).unwrap();
			assert_eq!(linux::dualsense_cards(&class), vec!["card2".to_string()]);
		}
		let _ = std::fs::remove_dir_all(&root);
	}

	/// Machine check (needs a Windows PC with an audio device): CfgMgr enumeration of the
	/// MMDevice endpoints works and yields their containers + names.
	/// `cargo test -p pulsar-tauri --lib endpoint_enumeration -- --ignored --nocapture`
	#[cfg(windows)]
	#[test]
	#[ignore]
	fn endpoint_enumeration() {
		let r = os::all_endpoints(Kind::Playback);
		let c = os::all_endpoints(Kind::Recording);
		println!(
			"render: {r:#x?}
capture: {c:#x?}"
		);
		assert!(!r.is_empty() || !c.is_empty());
		assert!(r.iter().chain(c.iter()).all(|(_, n)| !n.is_empty()));
	}
}
