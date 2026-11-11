//! Helper-side injection: `SendInput` on whatever desktop currently receives input
//! (Default, or Winlogon for the lock screen / secure-desktop UAC). Runs as SYSTEM, so UIPI
//! does not block it from consent.exe / LogonUI.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use pulsar_sysagent::proto::{
	DesktopKind, Rect, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
	MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
	MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
};
use windows_sys::Win32::Foundation::{GENERIC_ALL, POINT};
use windows_sys::Win32::System::StationsAndDesktops::{
	CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop, HDESK, UOI_NAME,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
	SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, MOUSEINPUT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

use crate::log::log;

/// How often an idle-path call re-checks the input desktop (a failed SendInput forces it).
const RECHECK: Duration = Duration::from_millis(200);

/// Keeps the calling thread attached to the current input desktop
/// (OpenInputDesktop + SetThreadDesktop — the Sunshine/Parsec technique). One per thread:
/// a thread's desktop is per-thread state, and SetThreadDesktop fails once the thread owns
/// windows or hooks, which our injector threads never do.
pub struct DesktopFollower {
	current: HDESK,
	name: String,
	checked: Option<Instant>,
}

// SAFETY: only used from the thread that owns it; HDESK is a plain kernel handle.
unsafe impl Send for DesktopFollower {}

fn desktop_name(h: HDESK) -> String {
	let mut buf = [0u16; 256];
	let mut need = 0u32;
	let ok = unsafe {
		GetUserObjectInformationW(
			h,
			UOI_NAME,
			buf.as_mut_ptr().cast(),
			(buf.len() * 2) as u32,
			&mut need,
		)
	};
	if ok == 0 {
		return String::new();
	}
	let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
	String::from_utf16_lossy(&buf[..n])
}

impl DesktopFollower {
	pub fn new() -> Self {
		Self {
			current: std::ptr::null_mut(),
			name: String::new(),
			checked: None,
		}
	}

	/// Re-attach if the input desktop changed. `force` skips the rate limit.
	pub fn sync(&mut self, force: bool) -> DesktopKind {
		if !force && self.checked.is_some_and(|t| t.elapsed() < RECHECK) {
			return DesktopKind::from_name(&self.name);
		}
		self.checked = Some(Instant::now());
		let h = unsafe { OpenInputDesktop(0, 0, GENERIC_ALL) };
		if h.is_null() {
			// Transient during a desktop switch; keep the old attachment.
			return DesktopKind::Unknown;
		}
		let name = desktop_name(h);
		if name == self.name && !self.current.is_null() {
			unsafe { CloseDesktop(h) };
			return DesktopKind::from_name(&name);
		}
		if unsafe { SetThreadDesktop(h) } == 0 {
			log!(
				"SetThreadDesktop({name}) failed: {}",
				std::io::Error::last_os_error()
			);
			unsafe { CloseDesktop(h) };
			return DesktopKind::Unknown;
		}
		if !self.current.is_null() {
			unsafe { CloseDesktop(self.current) };
		}
		log!("input desktop → {name}");
		self.current = h;
		self.name = name;
		DesktopKind::from_name(&self.name)
	}
}

impl Drop for DesktopFollower {
	fn drop(&mut self) {
		// The thread is about to exit; its desktop handle can go with it.
		if !self.current.is_null() {
			unsafe { CloseDesktop(self.current) };
		}
	}
}

fn send(desk: &mut DesktopFollower, input: &INPUT) {
	desk.sync(false);
	let size = std::mem::size_of::<INPUT>() as i32;
	if unsafe { SendInput(1, input, size) } == 0 {
		// Usually: the input desktop switched since the last check (UAC prompt / lock).
		desk.sync(true);
		unsafe { SendInput(1, input, size) };
	}
}

fn mouse_input(dx: i32, dy: i32, data: i32, flags: u32) -> INPUT {
	INPUT {
		r#type: INPUT_MOUSE,
		Anonymous: INPUT_0 {
			mi: MOUSEINPUT {
				dx,
				dy,
				mouseData: data as u32,
				dwFlags: flags,
				time: 0,
				dwExtraInfo: 0,
			},
		},
	}
}

fn key_input(vk: u16, scan: u16, flags: u32) -> INPUT {
	INPUT {
		r#type: INPUT_KEYBOARD,
		Anonymous: INPUT_0 {
			ki: KEYBDINPUT {
				wVk: vk,
				wScan: scan,
				dwFlags: flags,
				time: 0,
				dwExtraInfo: 0,
			},
		},
	}
}

/// One per pipe connection. Tracks what THIS connection holds down so a vanished host
/// never leaves a key stuck (e.g. Shift on the lock screen).
pub struct Injector {
	desk: DesktopFollower,
	/// Up-flags of buttons currently down.
	held_buttons: HashSet<u32>,
	/// vk → (scan, extended flag) of keys currently down.
	held_keys: HashMap<u16, (u16, u32)>,
	pinch_tx: Option<std::sync::mpsc::Sender<PinchCmd>>,
}

impl Injector {
	pub fn new() -> Self {
		Self {
			desk: DesktopFollower::new(),
			held_buttons: HashSet::new(),
			held_keys: HashMap::new(),
			pinch_tx: None,
		}
	}

	pub fn desktop(&mut self) -> DesktopKind {
		self.desk.sync(false)
	}

	pub fn mouse(&mut self, dx: i32, dy: i32, data: i32, flags: u32) {
		for (down, up) in [
			(MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
			(MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
			(MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
		] {
			if flags & down != 0 {
				self.held_buttons.insert(up);
			}
			if flags & up != 0 {
				self.held_buttons.remove(&up);
			}
		}
		send(&mut self.desk, &mouse_input(dx, dy, data, flags));
	}

	pub fn key(&mut self, vk: u16, scan: u16, flags: u32) {
		if flags & KEYEVENTF_UNICODE == 0 {
			if flags & KEYEVENTF_KEYUP == 0 {
				self.held_keys
					.insert(vk, (scan, flags & KEYEVENTF_EXTENDEDKEY));
			} else {
				self.held_keys.remove(&vk);
			}
		}
		send(&mut self.desk, &key_input(vk, scan, flags));
	}

	pub fn clamp(&mut self, r: Rect) {
		self.desk.sync(false);
		unsafe {
			let mut p = POINT { x: 0, y: 0 };
			if GetCursorPos(&mut p) != 0 {
				let cx = p.x.clamp(r.left, r.left + r.width - 1);
				let cy = p.y.clamp(r.top, r.top + r.height - 1);
				if cx != p.x || cy != p.y {
					SetCursorPos(cx, cy);
				}
			}
		}
	}

	pub fn pinch(&mut self, phase: u8, scale: f64) {
		let tx = self.pinch_tx.get_or_insert_with(|| {
			let (tx, rx) = std::sync::mpsc::channel();
			let _ = std::thread::Builder::new()
				.name("sysagent-pinch".into())
				.spawn(move || pinch_worker(rx));
			tx
		});
		let cmd = match phase {
			0 => PinchCmd::Begin,
			1 => PinchCmd::Scale(scale),
			_ => PinchCmd::End,
		};
		if tx.send(cmd).is_err() {
			self.pinch_tx = None;
		}
	}

	pub fn release_all(&mut self) {
		if let Some(tx) = &self.pinch_tx {
			let _ = tx.send(PinchCmd::End);
		}
		for up in std::mem::take(&mut self.held_buttons) {
			send(&mut self.desk, &mouse_input(0, 0, 0, up));
		}
		for (vk, (scan, ext)) in std::mem::take(&mut self.held_keys) {
			send(&mut self.desk, &key_input(vk, scan, KEYEVENTF_KEYUP | ext));
		}
	}
}

impl Drop for Injector {
	fn drop(&mut self) {
		self.release_all();
	}
}

enum PinchCmd {
	Begin,
	Scale(f64),
	End,
}

const PINCH_D0: f64 = 300.0;
const PINCH_MIN: f64 = 40.0;
const PINCH_MAX: f64 = 3000.0;

/// Port of `pulsar_core::input::windows::pinch_worker` (two synthetic touch contacts around
/// the cursor), with its own desktop follower since it runs on its own thread.
fn pinch_worker(rx: std::sync::mpsc::Receiver<PinchCmd>) {
	use std::sync::mpsc::RecvTimeoutError;
	use windows_sys::Win32::Foundation::RECT;
	use windows_sys::Win32::UI::Input::Pointer::{
		InitializeTouchInjection, InjectTouchInput, POINTER_FLAG_DOWN, POINTER_FLAG_INCONTACT,
		POINTER_FLAG_INRANGE, POINTER_FLAG_UP, POINTER_FLAG_UPDATE, POINTER_TOUCH_INFO,
		TOUCH_FEEDBACK_NONE,
	};
	use windows_sys::Win32::UI::WindowsAndMessaging::{
		GetSystemMetrics, PT_TOUCH, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
		SM_YVIRTUALSCREEN, TOUCH_MASK_CONTACTAREA, TOUCH_MASK_ORIENTATION, TOUCH_MASK_PRESSURE,
	};
	let mut desk = DesktopFollower::new();
	desk.sync(true);
	if unsafe { InitializeTouchInjection(2, TOUCH_FEEDBACK_NONE) } == 0 {
		log!(
			"touch injection unavailable — pinch ignored: {}",
			std::io::Error::last_os_error()
		);
		for _ in rx {}
		return;
	}
	let inject = |desk: &mut DesktopFollower, center: POINT, d: f64, flags: u32| {
		desk.sync(false);
		let (vx, vy) = unsafe {
			(
				GetSystemMetrics(SM_XVIRTUALSCREEN),
				GetSystemMetrics(SM_YVIRTUALSCREEN),
			)
		};
		let (vw, vh) = unsafe {
			(
				GetSystemMetrics(SM_CXVIRTUALSCREEN),
				GetSystemMetrics(SM_CYVIRTUALSCREEN),
			)
		};
		let clamp_x = |x: i32| x.clamp(vx + 4, vx + vw.max(9) - 5);
		let y = center.y.clamp(vy + 4, vy + vh.max(9) - 5);
		let half = (d / 2.0).round() as i32;
		let xs = [clamp_x(center.x - half), clamp_x(center.x + half)];
		let mut c: [POINTER_TOUCH_INFO; 2] = unsafe { std::mem::zeroed() };
		for (i, t) in c.iter_mut().enumerate() {
			t.pointerInfo.pointerType = PT_TOUCH;
			t.pointerInfo.pointerId = i as u32;
			t.pointerInfo.pointerFlags = flags;
			t.pointerInfo.ptPixelLocation = POINT { x: xs[i], y };
			t.touchMask = TOUCH_MASK_CONTACTAREA | TOUCH_MASK_ORIENTATION | TOUCH_MASK_PRESSURE;
			t.rcContact = RECT {
				left: xs[i] - 4,
				top: y - 4,
				right: xs[i] + 4,
				bottom: y + 4,
			};
			t.orientation = 90;
			t.pressure = 32000;
		}
		unsafe { InjectTouchInput(2, c.as_ptr()) }
	};
	const MOVE: u32 = POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT;
	let mut active = false;
	let mut center = POINT { x: 0, y: 0 };
	let (mut d, mut target) = (PINCH_D0, PINCH_D0);
	let mut last_msg = Instant::now();
	loop {
		let wait = if active {
			Duration::from_millis(16)
		} else {
			Duration::from_secs(3600)
		};
		match rx.recv_timeout(wait) {
			Ok(PinchCmd::Begin) => {
				if active {
					inject(&mut desk, center, d, POINTER_FLAG_UP);
				}
				unsafe { GetCursorPos(&mut center) };
				d = PINCH_D0;
				target = PINCH_D0;
				active = inject(
					&mut desk,
					center,
					d,
					POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT,
				) != 0;
				last_msg = Instant::now();
				continue;
			}
			Ok(PinchCmd::Scale(s)) => {
				target = (PINCH_D0 * s).clamp(PINCH_MIN, PINCH_MAX);
				last_msg = Instant::now();
			}
			Ok(PinchCmd::End) => {
				if active {
					inject(&mut desk, center, target, MOVE);
					inject(&mut desk, center, target, POINTER_FLAG_UP);
					active = false;
				}
				continue;
			}
			Err(RecvTimeoutError::Timeout) => {}
			Err(RecvTimeoutError::Disconnected) => {
				if active {
					inject(&mut desk, center, d, POINTER_FLAG_UP);
				}
				return;
			}
		}
		if active {
			if last_msg.elapsed() > Duration::from_millis(1500) {
				inject(&mut desk, center, d, POINTER_FLAG_UP);
				active = false;
				continue;
			}
			d += (target - d) * 0.45;
			inject(&mut desk, center, d, MOVE);
		}
	}
}
