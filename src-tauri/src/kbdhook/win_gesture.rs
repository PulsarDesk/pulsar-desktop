//! Windows client: 3 / 4-finger touchpad gestures → host (`InputEvent::Gesture`, see
//! `crate::gesture`).
//!
//! Precision-touchpad gestures are executed by the shell (Explorer) and never reach an app as
//! messages, so the gestures are recognised from the touchpad's own contacts: Raw Input from
//! the HID digitizer (usage page 0x0D, usage 0x05 "touch pad", `RIDEV_INPUTSINK` on a
//! hidden window, so the reports arrive while the renderer — another process — has the
//! focus), parsed with the `HidP_*` API (contact count, per-finger tip switch / X / Y), fed to
//! `gesture::Recognizer`.
//!
//! While control is engaged (and gestures go to this host), Windows' own 3/4-finger gestures
//! are switched off so a swipe does not ALSO act on this PC: the user's
//! `HKCU\…\PrecisionTouchPad` `ThreeFingerSlideEnabled` / `FourFingerSlideEnabled` /
//! `ThreeFingerTapEnabled` / `FourFingerTapEnabled` are saved to `gesture-restore.json`
//! (before anything is changed), set to 0, and put back on release, session end, app exit
//! and — after a crash — at the next start ([`startup`]). Explorer caches these values, so
//! each change is followed by the shell refresh System Settings itself uses
//! ([`notify_shell`], undocumented). Untested with real fingers on the pad (see
//! docs/agents/status.md).

use super::imp;
use crate::gesture::Recognizer;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
	HidP_GetCaps, HidP_GetUsageValue, HidP_GetUsages, HidP_GetValueCaps, HidP_Input, HIDP_CAPS,
	HIDP_STATUS_SUCCESS, HIDP_VALUE_CAPS,
};
use windows_sys::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};

const PAGE_DIGITIZER: u16 = 0x0D;
const PAGE_GENERIC: u16 = 0x01;
const USAGE_TOUCHPAD: u16 = 0x05;
const USAGE_TIP: u16 = 0x42;
const USAGE_CONTACT_ID: u16 = 0x51;
const USAGE_CONTACT_COUNT: u16 = 0x54;
const USAGE_X: u16 = 0x30;
const USAGE_Y: u16 = 0x31;

/// The four registry values switched off while engaged.
const VALUES: [&str; 4] = [
	pulsar_core::input::gesture::win::THREE_SLIDE,
	pulsar_core::input::gesture::win::FOUR_SLIDE,
	pulsar_core::input::gesture::win::THREE_TAP,
	pulsar_core::input::gesture::win::FOUR_TAP,
];
const RESTORE_FILE: &str = "gesture-restore.json";

static STARTED: AtomicBool = AtomicBool::new(false);
static RESTORE_PATH: OnceLock<std::path::PathBuf> = OnceLock::new();
/// The local gestures are currently switched off by us.
static SUPPRESSED: Mutex<bool> = Mutex::new(false);

/// App start: remember where the saved settings live, put back what a crashed run left
/// switched off, and start the reader.
pub fn startup(dir: std::path::PathBuf) {
	let _ = RESTORE_PATH.set(dir.join(RESTORE_FILE));
	restore_saved();
	ensure_started();
}

/// App exit: give the user's gestures back.
pub fn shutdown() {
	set_suppressed(false);
}

pub fn ensure_started() {
	if STARTED.swap(true, Ordering::SeqCst) {
		return;
	}
	let _ = std::thread::Builder::new().name("pulsar-gesture-rd".into()).spawn(|| unsafe { run() });
}

/// Local gestures off while control is engaged and gestures go to the host.
fn want_suppressed() -> bool {
	imp::is_engaged() && crate::gesture::forwarding()
}

// ---- local suppression (registry) ----

fn key() -> &'static str {
	pulsar_core::input::gesture::win::KEY
}

fn wide(s: &str) -> Vec<u16> {
	s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn read_dword(name: &str) -> Option<u32> {
	use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
	let (k, n) = (wide(key()), wide(name));
	let mut v = 0u32;
	let mut len = 4u32;
	let r = unsafe {
		RegGetValueW(
			HKEY_CURRENT_USER,
			k.as_ptr(),
			n.as_ptr(),
			RRF_RT_REG_DWORD,
			std::ptr::null_mut(),
			&mut v as *mut u32 as *mut core::ffi::c_void,
			&mut len,
		)
	};
	(r == 0).then_some(v)
}

/// Write (`Some`) or delete (`None`) one value. `true` = done.
fn write_dword(name: &str, v: Option<u32>) -> bool {
	use windows_sys::Win32::System::Registry::{
		RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_DWORD,
	};
	let (k, n) = (wide(key()), wide(name));
	let mut h: HKEY = std::ptr::null_mut();
	if unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, k.as_ptr(), 0, KEY_SET_VALUE, &mut h) } != 0 {
		return false;
	}
	let r = match v {
		Some(v) => unsafe { RegSetValueExW(h, n.as_ptr(), 0, REG_DWORD, &v as *const u32 as *const u8, 4) },
		None => {
			let r = unsafe { RegDeleteValueW(h, n.as_ptr()) };
			if r == 2 { 0 } else { r } // ERROR_FILE_NOT_FOUND: already absent
		}
	};
	unsafe { RegCloseKey(h) };
	r == 0
}

/// Explorer's setting id for a touchpad value (`twinui` / `SettingsHandlers_nt` share them).
fn setting_id(name: &str) -> Option<u32> {
	use pulsar_core::input::gesture::win::{FOUR_SLIDE, FOUR_TAP, THREE_SLIDE, THREE_TAP};
	match name {
		THREE_TAP => Some(0x0F),
		FOUR_TAP => Some(0x10),
		THREE_SLIDE => Some(0x11),
		FOUR_SLIDE => Some(0x12),
		_ => None,
	}
}

/// Make Explorer re-read the given touchpad values NOW. Explorer (twinui) caches them and does
/// not watch the key; System Settings applies a change by asking the shell to refresh that one
/// setting: `CoCreateInstance(CLSID_ImmersiveShell, CLSCTX_LOCAL_SERVER)` →
/// `IServiceProvider::QueryService({53660488-…}, {4214f6fa-…})` → vtable slot 3
/// `(this, setting_id: u32) -> HRESULT`, once per changed value (read from the Windows 11
/// binaries, 2026-11-15; undocumented — best effort, logged when it fails).
fn notify_shell(names: &[&str]) -> bool {
	use std::ffi::c_void;
	use windows_sys::core::{GUID, HRESULT};
	use windows_sys::Win32::System::Com::{
		CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_LOCAL_SERVER, CLSCTX_NO_CODE_DOWNLOAD,
		COINIT_MULTITHREADED,
	};
	const CLSID_IMMERSIVE_SHELL: GUID = GUID::from_u128(0xC2F03A33_21F5_47FA_B4BB_156362A2F239);
	const IID_SERVICE_PROVIDER: GUID = GUID::from_u128(0x6D5140C1_7436_11CE_8034_00AA006009FA);
	const SID_TOUCHPAD_SETTINGS: GUID = GUID::from_u128(0x53660488_8855_460B_A9AB_5CFC6B5012CA);
	const IID_TOUCHPAD_SETTINGS: GUID = GUID::from_u128(0x4214F6FA_EB36_4E2F_9CA2_23FDC1832DF7);
	type Release = unsafe extern "system" fn(*mut c_void) -> u32;
	type QueryService = unsafe extern "system" fn(*mut c_void, *const GUID, *const GUID, *mut *mut c_void) -> HRESULT;
	type OnSettingChanged = unsafe extern "system" fn(*mut c_void, u32) -> HRESULT;
	/// Method `i` of a COM object's vtable.
	unsafe fn slot<T: Copy>(obj: *mut c_void, i: usize) -> T {
		let vt = *(obj as *const *const usize);
		std::mem::transmute_copy(&*vt.add(i))
	}
	unsafe {
		// MTA: an outgoing call does not pump this thread's messages (no re-entry into the
		// timer that called us). On an STA thread (app exit on the main thread) this fails with
		// RPC_E_CHANGED_MODE and the call goes out from that STA instead.
		let init = CoInitializeEx(std::ptr::null(), COINIT_MULTITHREADED as u32) >= 0;
		let mut sp: *mut c_void = std::ptr::null_mut();
		let mut ok = false;
		let hr = CoCreateInstance(
			&CLSID_IMMERSIVE_SHELL,
			std::ptr::null_mut(),
			CLSCTX_LOCAL_SERVER | CLSCTX_NO_CODE_DOWNLOAD,
			&IID_SERVICE_PROVIDER,
			&mut sp,
		);
		if hr >= 0 && !sp.is_null() {
			let mut p: *mut c_void = std::ptr::null_mut();
			let qs: QueryService = slot(sp, 3);
			let hr = qs(sp, &SID_TOUCHPAD_SETTINGS, &IID_TOUCHPAD_SETTINGS, &mut p);
			if hr >= 0 && !p.is_null() {
				let changed: OnSettingChanged = slot(p, 3);
				ok = true;
				for id in names.iter().filter_map(|n| setting_id(n)) {
					let hr = changed(p, id);
					if hr < 0 {
						ok = false;
						tracing::warn!(id, hr = format!("{hr:#x}"), "touchpad gestures: shell refresh failed");
					}
				}
				let rel: Release = slot(p, 2);
				rel(p);
			} else {
				tracing::warn!(hr = format!("{hr:#x}"), "touchpad gestures: no shell touchpad-settings service");
			}
			let rel: Release = slot(sp, 2);
			rel(sp);
		} else {
			tracing::warn!(hr = format!("{hr:#x}"), "touchpad gestures: ImmersiveShell unavailable");
		}
		if init {
			CoUninitialize();
		}
		ok
	}
}

/// The saved user values (`name → value`, `None` = the value did not exist).
type Saved = Vec<(String, Option<u32>)>;

fn encode(saved: &Saved) -> String {
	let m: serde_json::Map<String, serde_json::Value> = saved
		.iter()
		.map(|(k, v)| (k.clone(), v.map_or(serde_json::Value::Null, |v| serde_json::Value::from(v))))
		.collect();
	serde_json::Value::Object(m).to_string()
}

fn decode(text: &str) -> Option<Saved> {
	let v: serde_json::Value = serde_json::from_str(text).ok()?;
	let m = v.as_object()?;
	let saved: Saved = m
		.iter()
		.filter(|(k, _)| VALUES.contains(&k.as_str()))
		.map(|(k, v)| (k.clone(), v.as_u64().map(|v| v as u32)))
		.collect();
	(!saved.is_empty()).then_some(saved)
}

/// Put back the values a file from an earlier suppression holds (then delete the file).
fn restore_saved() {
	let Some(path) = RESTORE_PATH.get() else { return };
	let Ok(text) = std::fs::read_to_string(path) else { return };
	match decode(&text) {
		Some(saved) => {
			let ok = saved.iter().all(|(k, v)| write_dword(k, *v));
			let names: Vec<&str> = saved.iter().map(|(k, _)| k.as_str()).collect();
			let live = notify_shell(&names);
			tracing::info!(?saved, ok, live, "touchpad gestures: user settings restored");
			if ok {
				let _ = std::fs::remove_file(path);
			}
		}
		None => {
			let _ = std::fs::remove_file(path);
		}
	}
}

fn set_suppressed(on: bool) {
	let mut cur = SUPPRESSED.lock().unwrap_or_else(|e| e.into_inner());
	if *cur == on {
		return;
	}
	if on {
		let Some(path) = RESTORE_PATH.get() else { return };
		// A file left by an earlier run holds the user's real values: never overwrite it
		// with our zeros — restore it first.
		if path.exists() {
			restore_saved();
			if path.exists() {
				return;
			}
		}
		let saved: Saved = VALUES.iter().map(|n| (n.to_string(), read_dword(n))).collect();
		if let Some(dir) = path.parent() {
			let _ = std::fs::create_dir_all(dir);
		}
		if std::fs::write(path, encode(&saved)).is_err() {
			tracing::warn!("touchpad gestures: cannot save the user's settings — local gestures stay on");
			return;
		}
		let ok = VALUES.iter().all(|n| write_dword(n, Some(0)));
		let live = notify_shell(&VALUES);
		tracing::info!(?saved, ok, live, "touchpad gestures: local 3/4-finger gestures off while engaged");
		*cur = true;
	} else {
		restore_saved();
		*cur = false;
	}
}

// ---- Raw Input reader ----

/// One finger collection of the touchpad's input report.
#[derive(Clone, Copy, Debug)]
struct Finger {
	link: u16,
	x: (i32, i32),
	y: (i32, i32),
}

struct Pad {
	/// The preparsed data buffer (`PHIDP_PREPARSED_DATA` points into it).
	pp: Vec<u8>,
	fingers: Vec<Finger>,
	/// Link collection carrying the contact count.
	count_link: Option<u16>,
	/// Hybrid mode: contacts of the frame being assembled and how many it announced.
	pending: Vec<(f64, f64)>,
	expected: usize,
	seen: usize,
	rec: Recognizer,
}

impl Pad {
	fn pp(&self) -> isize {
		self.pp.as_ptr() as isize
	}
}

static PADS: Mutex<Option<HashMap<isize, Option<Pad>>>> = Mutex::new(None);

unsafe fn open_pad(dev: HANDLE) -> Option<Pad> {
	use windows_sys::Win32::UI::Input::{GetRawInputDeviceInfoW, RIDI_PREPARSEDDATA};
	let mut size = 0u32;
	GetRawInputDeviceInfoW(dev, RIDI_PREPARSEDDATA, std::ptr::null_mut(), &mut size);
	if size == 0 || size > 1 << 20 {
		return None;
	}
	let mut pp = vec![0u8; size as usize];
	if GetRawInputDeviceInfoW(dev, RIDI_PREPARSEDDATA, pp.as_mut_ptr() as *mut _, &mut size) == u32::MAX {
		return None;
	}
	let ppd = pp.as_ptr() as isize;
	let mut caps: HIDP_CAPS = std::mem::zeroed();
	if HidP_GetCaps(ppd, &mut caps) != HIDP_STATUS_SUCCESS {
		return None;
	}
	let mut n = caps.NumberInputValueCaps;
	if n == 0 {
		return None;
	}
	let mut vcaps = vec![HIDP_VALUE_CAPS::default(); n as usize];
	if HidP_GetValueCaps(HidP_Input, vcaps.as_mut_ptr(), &mut n, ppd) != HIDP_STATUS_SUCCESS {
		return None;
	}
	vcaps.truncate(n as usize);
	let usage_of = |c: &HIDP_VALUE_CAPS| if c.IsRange { c.Anonymous.Range.UsageMin } else { c.Anonymous.NotRange.Usage };
	let mut xs: HashMap<u16, (i32, i32)> = HashMap::new();
	let mut ys: HashMap<u16, (i32, i32)> = HashMap::new();
	let mut count_link = None;
	for c in &vcaps {
		let u = usage_of(c);
		match (c.UsagePage, u) {
			(PAGE_GENERIC, USAGE_X) => {
				xs.insert(c.LinkCollection, (c.LogicalMin, c.LogicalMax));
			}
			(PAGE_GENERIC, USAGE_Y) => {
				ys.insert(c.LinkCollection, (c.LogicalMin, c.LogicalMax));
			}
			(PAGE_DIGITIZER, USAGE_CONTACT_COUNT) => count_link = Some(c.LinkCollection),
			_ => {}
		}
	}
	let mut fingers: Vec<Finger> = xs
		.iter()
		.filter_map(|(&link, &x)| ys.get(&link).map(|&y| Finger { link, x, y }))
		.filter(|f| f.x.1 > f.x.0 && f.y.1 > f.y.0)
		.collect();
	fingers.sort_by_key(|f| f.link);
	if fingers.is_empty() {
		return None;
	}
	tracing::info!(
		fingers = fingers.len(),
		count_link = ?count_link,
		x = ?fingers[0].x,
		y = ?fingers[0].y,
		"gesture: reading precision touchpad"
	);
	Some(Pad { pp, fingers, count_link, pending: Vec::new(), expected: 0, seen: 0, rec: Recognizer::new() })
}

/// One HID input report → the finished frame's contacts (hybrid mode spreads a frame over
/// several reports; only the first carries the contact count).
unsafe fn report(pad: &mut Pad, data: &mut [u8]) -> Option<Vec<(f64, f64)>> {
	let ppd = pad.pp();
	let len = data.len() as u32;
	let mut count = 0u32;
	let has_count = pad.count_link.is_some_and(|l| {
		HidP_GetUsageValue(HidP_Input, PAGE_DIGITIZER, l, USAGE_CONTACT_COUNT, &mut count, ppd, data.as_ptr(), len)
			== HIDP_STATUS_SUCCESS
	});
	if has_count && count > 0 {
		pad.pending.clear();
		pad.expected = count as usize;
		pad.seen = 0;
	} else if pad.expected == 0 {
		// No frame open and no count: a stray report (or a pad without a count): skip.
		if !has_count {
			return None;
		}
		// count == 0: every finger lifted.
		pad.pending.clear();
		return Some(Vec::new());
	}
	for f in pad.fingers.clone() {
		if pad.seen >= pad.expected {
			break;
		}
		pad.seen += 1;
		let mut usages = [0u16; 8];
		let mut n = usages.len() as u32;
		let tip = HidP_GetUsages(HidP_Input, PAGE_DIGITIZER, f.link, usages.as_mut_ptr(), &mut n, ppd, data.as_mut_ptr(), len)
			== HIDP_STATUS_SUCCESS
			&& usages[..n as usize].contains(&USAGE_TIP);
		if !tip {
			continue;
		}
		let (mut x, mut y, mut id) = (0u32, 0u32, 0u32);
		let ok = HidP_GetUsageValue(HidP_Input, PAGE_GENERIC, f.link, USAGE_X, &mut x, ppd, data.as_ptr(), len)
			== HIDP_STATUS_SUCCESS
			&& HidP_GetUsageValue(HidP_Input, PAGE_GENERIC, f.link, USAGE_Y, &mut y, ppd, data.as_ptr(), len)
				== HIDP_STATUS_SUCCESS;
		let _ = HidP_GetUsageValue(HidP_Input, PAGE_DIGITIZER, f.link, USAGE_CONTACT_ID, &mut id, ppd, data.as_ptr(), len);
		if ok {
			pad.pending.push((norm(x as i32, f.x), norm(y as i32, f.y)));
		}
	}
	if pad.seen >= pad.expected {
		pad.expected = 0;
		return Some(std::mem::take(&mut pad.pending));
	}
	None
}

fn norm(v: i32, (lo, hi): (i32, i32)) -> f64 {
	((v - lo) as f64 / (hi - lo) as f64).clamp(0.0, 1.0)
}

fn now_ms() -> u64 {
	static T0: OnceLock<std::time::Instant> = OnceLock::new();
	T0.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

unsafe fn on_input(lp: LPARAM) {
	use windows_sys::Win32::UI::Input::{GetRawInputData, HRAWINPUT, RAWINPUTHEADER, RID_INPUT, RIM_TYPEHID};
	let hdr = std::mem::size_of::<RAWINPUTHEADER>() as u32;
	let mut size = 0u32;
	GetRawInputData(lp as HRAWINPUT, RID_INPUT, std::ptr::null_mut(), &mut size, hdr);
	if size == 0 || size > 1 << 16 {
		return;
	}
	// u64 storage keeps the RAWINPUT header aligned.
	let mut buf = vec![0u64; (size as usize).div_ceil(8)];
	if GetRawInputData(lp as HRAWINPUT, RID_INPUT, buf.as_mut_ptr() as *mut _, &mut size, hdr) == u32::MAX {
		return;
	}
	let raw = &*(buf.as_ptr() as *const windows_sys::Win32::UI::Input::RAWINPUT);
	if raw.header.dwType != RIM_TYPEHID {
		return;
	}
	let dev = raw.header.hDevice;
	let (each, count) = (raw.data.hid.dwSizeHid as usize, raw.data.hid.dwCount as usize);
	let off = std::mem::offset_of!(windows_sys::Win32::UI::Input::RAWINPUT, data)
		+ std::mem::offset_of!(windows_sys::Win32::UI::Input::RAWHID, bRawData);
	if each == 0 || off + each * count > size as usize {
		return;
	}
	let bytes = std::slice::from_raw_parts_mut((buf.as_mut_ptr() as *mut u8).add(off), each * count);
	let mut pads = PADS.lock().unwrap_or_else(|e| e.into_inner());
	let pads = pads.get_or_insert_with(HashMap::new);
	let pad = pads.entry(dev as isize).or_insert_with(|| open_pad(dev));
	let Some(pad) = pad.as_mut() else { return };
	for chunk in bytes.chunks_mut(each) {
		let Some(contacts) = report(pad, chunk) else { continue };
		let Some((fingers, kind)) = pad.rec.frame(now_ms(), &contacts) else { continue };
		let forward = want_suppressed();
		tracing::info!(fingers, ?kind, forward, "touchpad gesture");
		if forward {
			if let Some(tx) = imp::session_tx() {
				imp::forward(&tx, crate::gesture::event(fingers, kind));
			}
		}
	}
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
	use windows_sys::Win32::UI::WindowsAndMessaging::{
		DefWindowProcW, GIDC_REMOVAL, WM_ENDSESSION, WM_INPUT, WM_INPUT_DEVICE_CHANGE, WM_TIMER,
	};
	match msg {
		WM_INPUT => on_input(lp),
		WM_INPUT_DEVICE_CHANGE if wp as u32 == GIDC_REMOVAL => {
			if let Some(p) = PADS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
				p.remove(&(lp as isize));
			}
		}
		WM_TIMER => set_suppressed(want_suppressed()),
		// Sign-out / shutdown while engaged: the next sign-in must find the user's settings.
		WM_ENDSESSION if wp != 0 => set_suppressed(false),
		_ => {}
	}
	DefWindowProcW(hwnd, msg, wp, lp)
}

unsafe fn run() {
	use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
	use windows_sys::Win32::UI::Input::{RegisterRawInputDevices, RAWINPUTDEVICE, RIDEV_DEVNOTIFY, RIDEV_INPUTSINK};
	use windows_sys::Win32::UI::WindowsAndMessaging::{
		CreateWindowExW, DispatchMessageW, GetMessageW, RegisterClassW, SetTimer, TranslateMessage, MSG,
		WNDCLASSW,
	};
	let hmod = GetModuleHandleW(std::ptr::null());
	let name = wide("PulsarTouchpadGestures");
	let mut wc: WNDCLASSW = std::mem::zeroed();
	wc.lpfnWndProc = Some(wnd_proc);
	wc.hInstance = hmod;
	wc.lpszClassName = name.as_ptr();
	RegisterClassW(&wc);
	// A hidden top-level window, not a message-only one: only top-level windows get
	// WM_ENDSESSION (sign-out / shutdown while engaged must give the settings back).
	let hwnd = CreateWindowExW(
		0,
		name.as_ptr(),
		name.as_ptr(),
		0,
		0,
		0,
		0,
		0,
		std::ptr::null_mut(),
		std::ptr::null_mut(),
		hmod,
		std::ptr::null(),
	);
	if hwnd.is_null() {
		tracing::warn!("gesture: window failed — no touchpad gestures");
		return;
	}
	let rid = RAWINPUTDEVICE {
		usUsagePage: PAGE_DIGITIZER,
		usUsage: USAGE_TOUCHPAD,
		dwFlags: RIDEV_INPUTSINK | RIDEV_DEVNOTIFY,
		hwndTarget: hwnd,
	};
	if RegisterRawInputDevices(&rid, 1, std::mem::size_of::<RAWINPUTDEVICE>() as u32) == 0 {
		tracing::warn!("gesture: RegisterRawInputDevices(touchpad) failed — no touchpad gestures");
	}
	// Engage / release edges for the local suppression (and the setting / host flags).
	SetTimer(hwnd, 1, 100, None);
	let mut msg: MSG = std::mem::zeroed();
	while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
		TranslateMessage(&msg);
		DispatchMessageW(&msg);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn saved_settings_round_trip() {
		let saved: Saved = vec![
			("ThreeFingerSlideEnabled".into(), Some(2)),
			("FourFingerSlideEnabled".into(), Some(1)),
			("ThreeFingerTapEnabled".into(), Some(4)),
			("FourFingerTapEnabled".into(), None),
		];
		let mut back = decode(&encode(&saved)).unwrap();
		back.sort();
		let mut want = saved.clone();
		want.sort();
		assert_eq!(back, want);
		// Foreign keys are ignored; garbage is no restore.
		assert_eq!(decode(r#"{"Other":1}"#), None);
		assert_eq!(decode("not json"), None);
	}

	/// Live check on a real Windows desktop (not CI): the shell refresh path resolves and
	/// Explorer accepts the refresh. Changes nothing — Explorer re-reads the unchanged values.
	/// `cargo test -p pulsar-tauri --lib shell_refresh -- --ignored`
	#[test]
	#[ignore]
	fn shell_refresh_reaches_explorer() {
		let before: Vec<_> = VALUES.iter().map(|n| read_dword(n)).collect();
		assert!(notify_shell(&VALUES), "ImmersiveShell touchpad-settings refresh failed");
		let after: Vec<_> = VALUES.iter().map(|n| read_dword(n)).collect();
		assert_eq!(before, after);
	}

	/// Live check: switch the four values off and back exactly like an engage / release does
	/// (registry write + shell refresh), reading each state back. Restores the user's values.
	/// `cargo test -p pulsar-tauri --lib flip_and_restore -- --ignored`
	#[test]
	#[ignore]
	fn flip_and_restore() {
		let saved: Vec<_> = VALUES.iter().map(|n| (*n, read_dword(n))).collect();
		assert!(VALUES.iter().all(|n| write_dword(n, Some(0))));
		let live_off = notify_shell(&VALUES);
		let off: Vec<_> = VALUES.iter().map(|n| read_dword(n)).collect();
		assert!(saved.iter().all(|(n, v)| write_dword(n, *v)));
		let live_back = notify_shell(&VALUES);
		let back: Vec<_> = VALUES.iter().map(|n| (*n, read_dword(n))).collect();
		eprintln!("saved {saved:?} | off {off:?} live {live_off} | back {back:?} live {live_back}");
		assert_eq!(off, vec![Some(0); 4]);
		assert_eq!(back, saved);
		assert!(live_off && live_back);
	}

	#[test]
	fn setting_ids_match_explorers() {
		assert_eq!(setting_id("ThreeFingerTapEnabled"), Some(0x0F));
		assert_eq!(setting_id("FourFingerSlideEnabled"), Some(0x12));
		assert!(VALUES.iter().all(|n| setting_id(n).is_some()));
		assert_eq!(setting_id("PanEnabled"), None);
	}

	#[test]
	fn coordinates_normalize_to_the_logical_range() {
		assert_eq!(norm(0, (0, 1000)), 0.0);
		assert_eq!(norm(500, (0, 1000)), 0.5);
		assert_eq!(norm(2000, (0, 1000)), 1.0);
		assert_eq!(norm(-5, (0, 1000)), 0.0);
	}
}
