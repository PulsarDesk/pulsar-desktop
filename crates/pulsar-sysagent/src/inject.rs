//! Helper-side injection: `SendInput` on whatever desktop currently receives input
//! (Default, or Winlogon for the lock screen / secure-desktop UAC). Runs as SYSTEM, so UIPI
//! does not block it from consent.exe / LogonUI.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use pulsar_sysagent::proto::{
	DesktopKind, PointerContact, Rect, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
	KEYEVENTF_UNICODE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
	MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
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
	/// Bumped on every re-attach (objects bound to the old desktop must be recreated).
	switches: u32,
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
			switches: 0,
		}
	}

	/// How many times this thread moved to another input desktop.
	pub fn switches(&self) -> u32 {
		self.switches
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
		self.switches = self.switches.wrapping_add(1);
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
	pointer_tx: Option<std::sync::mpsc::Sender<PointerCmd>>,
}

impl Injector {
	pub fn new() -> Self {
		Self {
			desk: DesktopFollower::new(),
			held_buttons: HashSet::new(),
			held_keys: HashMap::new(),
			pinch_tx: None,
			pointer_tx: None,
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

	/// One synthetic pen / touch frame (protocol v2), injected on the pointer worker.
	pub fn pointer(&mut self, pen: bool, contacts: &[PointerContact]) {
		let tx = self.pointer_tx.get_or_insert_with(|| {
			let (tx, rx) = std::sync::mpsc::channel();
			let _ = std::thread::Builder::new()
				.name("sysagent-pointer".into())
				.spawn(move || pointer_worker(rx));
			tx
		});
		let cmd = PointerCmd::Frame {
			pen,
			contacts: contacts.to_vec(),
		};
		if tx.send(cmd).is_err() {
			self.pointer_tx = None;
		}
	}

	pub fn release_all(&mut self) {
		if let Some(tx) = &self.pinch_tx {
			let _ = tx.send(PinchCmd::End);
		}
		if let Some(tx) = &self.pointer_tx {
			let _ = tx.send(PointerCmd::Lift);
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

enum PointerCmd {
	Frame {
		pen: bool,
		contacts: Vec<PointerContact>,
	},
	/// Lift the pen and every finger (ReleaseAll).
	Lift,
}

/// The host refreshes an active pen / touch every 50 ms; this long without a frame means it
/// is stuck — lift, as the host's own driver would.
const POINTER_SILENCE: Duration = Duration::from_millis(1000);

type FnCreate = unsafe extern "system" fn(
	windows_sys::Win32::UI::WindowsAndMessaging::POINTER_INPUT_TYPE,
	u32,
	windows_sys::Win32::UI::Controls::POINTER_FEEDBACK_MODE,
) -> windows_sys::Win32::UI::Controls::HSYNTHETICPOINTERDEVICE;
type FnInject = unsafe extern "system" fn(
	windows_sys::Win32::UI::Controls::HSYNTHETICPOINTERDEVICE,
	*const windows_sys::Win32::UI::Controls::POINTER_TYPE_INFO,
	u32,
) -> i32;
type FnDestroy = unsafe extern "system" fn(windows_sys::Win32::UI::Controls::HSYNTHETICPOINTERDEVICE);
type FnDpi = unsafe extern "system" fn(isize) -> isize;

fn user32_proc(name: &[u8]) -> Option<unsafe extern "system" fn() -> isize> {
	use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
	let dll: Vec<u16> = "user32.dll\0".encode_utf16().collect();
	// SAFETY: NUL-terminated names; user32 is loaded (the helper links SendInput from it).
	unsafe {
		let h = GetModuleHandleW(dll.as_ptr());
		if h.is_null() {
			return None;
		}
		GetProcAddress(h, name.as_ptr())
	}
}

/// The synthetic pointer devices of one connection (Windows 10 1809+, resolved at run time
/// so the helper still starts on older systems). Same injection as the host's in-process
/// `pulsar_core::input::pen_touch::LocalDevices`, kept here because the SYSTEM helper does
/// not link pulsar-core.
struct Devices {
	create: FnCreate,
	inject: FnInject,
	destroy: FnDestroy,
	pen: Option<windows_sys::Win32::UI::Controls::HSYNTHETICPOINTERDEVICE>,
	touch: Option<windows_sys::Win32::UI::Controls::HSYNTHETICPOINTERDEVICE>,
	frame: u32,
	logged: bool,
}

impl Devices {
	fn load() -> Option<Self> {
		let create = user32_proc(b"CreateSyntheticPointerDevice\0")?;
		let inject = user32_proc(b"InjectSyntheticPointerInput\0")?;
		let destroy = user32_proc(b"DestroySyntheticPointerDevice\0")?;
		// SAFETY: the documented signatures of these user32 exports (see the aliases).
		unsafe {
			Some(Self {
				create: std::mem::transmute::<_, FnCreate>(create),
				inject: std::mem::transmute::<_, FnInject>(inject),
				destroy: std::mem::transmute::<_, FnDestroy>(destroy),
				pen: None,
				touch: None,
				frame: 0,
				logged: false,
			})
		}
	}

	/// Drop both devices (they belong to the desktop they were created on).
	fn reset(&mut self) {
		for h in [self.pen.take(), self.touch.take()].into_iter().flatten() {
			// SAFETY: a handle CreateSyntheticPointerDevice returned, destroyed once.
			unsafe { (self.destroy)(h) };
		}
	}

	fn inject(&mut self, pen: bool, contacts: &[PointerContact]) -> bool {
		use windows_sys::Win32::UI::Controls::POINTER_FEEDBACK_DEFAULT;
		use windows_sys::Win32::UI::WindowsAndMessaging::{PT_PEN, PT_TOUCH};
		if contacts.is_empty() {
			return true;
		}
		let slot = if pen { &mut self.pen } else { &mut self.touch };
		if slot.is_none() {
			let (ty, max) = if pen {
				(PT_PEN, 1)
			} else {
				(PT_TOUCH, pulsar_sysagent::proto::MAX_CONTACTS as u32)
			};
			// SAFETY: plain value arguments; null = failure.
			let h = unsafe { (self.create)(ty, max, POINTER_FEEDBACK_DEFAULT) };
			if h.is_null() {
				if !self.logged {
					self.logged = true;
					log!(
						"CreateSyntheticPointerDevice(pen={pen}) failed: {}",
						std::io::Error::last_os_error()
					);
				}
				return false;
			}
			*slot = Some(h);
		}
		let dev = slot.unwrap();
		self.frame = self.frame.wrapping_add(1);
		let infos: Vec<_> = contacts
			.iter()
			.map(|c| pointer_info(pen, self.frame, c))
			.collect();
		// SAFETY: `infos` is a valid slice of initialized POINTER_TYPE_INFO for this device.
		unsafe { (self.inject)(dev, infos.as_ptr(), infos.len() as u32) != 0 }
	}
}

impl Drop for Devices {
	fn drop(&mut self) {
		self.reset();
	}
}

/// A wire contact as the `POINTER_TYPE_INFO` `InjectSyntheticPointerInput` takes (the same
/// layout as `pulsar_core::input::pen_touch::pointer_info`).
fn pointer_info(
	pen: bool,
	frame: u32,
	c: &PointerContact,
) -> windows_sys::Win32::UI::Controls::POINTER_TYPE_INFO {
	use windows_sys::Win32::Foundation::RECT;
	use windows_sys::Win32::UI::Controls::{POINTER_TYPE_INFO, POINTER_TYPE_INFO_0};
	use windows_sys::Win32::UI::Input::Pointer::{POINTER_PEN_INFO, POINTER_TOUCH_INFO};
	use windows_sys::Win32::UI::WindowsAndMessaging::{
		PEN_MASK_PRESSURE, PEN_MASK_ROTATION, PEN_MASK_TILT_X, PEN_MASK_TILT_Y, PT_PEN, PT_TOUCH,
		TOUCH_MASK_CONTACTAREA, TOUCH_MASK_ORIENTATION, TOUCH_MASK_PRESSURE,
	};
	let at = POINT { x: c.x, y: c.y };
	if pen {
		let mut p: POINTER_PEN_INFO = unsafe { std::mem::zeroed() };
		p.pointerInfo.pointerType = PT_PEN;
		p.pointerInfo.pointerId = c.id as u32;
		p.pointerInfo.frameId = frame;
		p.pointerInfo.pointerFlags = c.flags;
		p.pointerInfo.ptPixelLocation = at;
		p.penMask = PEN_MASK_PRESSURE | PEN_MASK_ROTATION | PEN_MASK_TILT_X | PEN_MASK_TILT_Y;
		p.penFlags = c.pen_flags as u32;
		p.pressure = c.pressure as u32;
		p.rotation = c.rotation as u32;
		p.tiltX = c.tilt_x as i32;
		p.tiltY = c.tilt_y as i32;
		POINTER_TYPE_INFO {
			r#type: PT_PEN,
			Anonymous: POINTER_TYPE_INFO_0 { penInfo: p },
		}
	} else {
		let mut t: POINTER_TOUCH_INFO = unsafe { std::mem::zeroed() };
		t.pointerInfo.pointerType = PT_TOUCH;
		t.pointerInfo.pointerId = c.id as u32;
		t.pointerInfo.frameId = frame;
		t.pointerInfo.pointerFlags = c.flags;
		t.pointerInfo.ptPixelLocation = at;
		t.touchMask = TOUCH_MASK_CONTACTAREA | TOUCH_MASK_ORIENTATION | TOUCH_MASK_PRESSURE;
		t.rcContact = RECT {
			left: at.x - 4,
			top: at.y - 4,
			right: at.x + 4,
			bottom: at.y + 4,
		};
		t.orientation = 90;
		t.pressure = c.pressure as u32;
		POINTER_TYPE_INFO {
			r#type: PT_TOUCH,
			Anonymous: POINTER_TYPE_INFO_0 { touchInfo: t },
		}
	}
}

/// What this connection has down / in range, so a vanished or stuck host never leaves a
/// pen pressed or a finger on the lock screen. Pure, unit-tested.
#[derive(Default)]
pub(crate) struct PointerHeld {
	pen: Option<PointerContact>,
	touch: Vec<PointerContact>,
}

impl PointerHeld {
	pub(crate) fn note(&mut self, pen: bool, contacts: &[PointerContact]) {
		use pulsar_sysagent::proto::{POINTER_FLAG_INRANGE, POINTER_FLAG_UP};
		if pen {
			for c in contacts {
				self.pen = (c.flags & POINTER_FLAG_INRANGE != 0).then_some(*c);
			}
		} else {
			for c in contacts {
				self.touch.retain(|t| t.id != c.id);
				if c.flags & POINTER_FLAG_UP == 0 {
					self.touch.push(*c);
				}
			}
		}
	}

	pub(crate) fn active(&self) -> bool {
		self.pen.is_some() || !self.touch.is_empty()
	}

	/// The frames that lift everything (pen: up if in contact, then out of range; touch: one
	/// frame with every finger up), and forget it.
	pub(crate) fn lift(&mut self) -> Vec<(bool, Vec<PointerContact>)> {
		use pulsar_sysagent::proto::{
			POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE, POINTER_FLAG_UP, POINTER_FLAG_UPDATE,
		};
		let mut out = Vec::new();
		if let Some(p) = self.pen.take() {
			let base = PointerContact {
				pressure: 0,
				pen_flags: p.pen_flags & !pulsar_sysagent::proto::PEN_FLAG_ERASER,
				..p
			};
			if p.flags & POINTER_FLAG_INCONTACT != 0 {
				out.push((
					true,
					vec![PointerContact {
						flags: POINTER_FLAG_UP | POINTER_FLAG_INRANGE,
						..base
					}],
				));
			}
			out.push((
				true,
				vec![PointerContact {
					flags: POINTER_FLAG_UPDATE,
					..base
				}],
			));
		}
		if !self.touch.is_empty() {
			let ups = std::mem::take(&mut self.touch)
				.into_iter()
				.map(|t| PointerContact {
					flags: POINTER_FLAG_UP,
					..t
				})
				.collect();
			out.push((false, ups));
		}
		out
	}
}

/// Injects the host's synthetic pen / touch frames on the current input desktop (so pen and
/// touch reach UAC prompts and the lock screen). The devices are recreated after a desktop
/// switch; everything is lifted on ReleaseAll, on disconnect and after [`POINTER_SILENCE`].
fn pointer_worker(rx: std::sync::mpsc::Receiver<PointerCmd>) {
	use std::sync::mpsc::RecvTimeoutError;
	// Physical pixels, like the host's DPI-aware pen / touch worker that computed them.
	if let Some(f) = user32_proc(b"SetThreadDpiAwarenessContext\0") {
		// SAFETY: SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT); -4 = PER_MONITOR_AWARE_V2.
		unsafe { std::mem::transmute::<_, FnDpi>(f)(-4) };
	}
	let mut desk = DesktopFollower::new();
	desk.sync(true);
	let Some(mut devs) = Devices::load() else {
		log!("synthetic pointer API unavailable — pen/touch ignored");
		for _ in rx {}
		return;
	};
	let mut seen = desk.switches();
	let mut held = PointerHeld::default();
	let mut last = Instant::now();
	let mut send = |desk: &mut DesktopFollower, devs: &mut Devices, pen: bool, c: &[PointerContact]| {
		desk.sync(false);
		if desk.switches() != seen {
			seen = desk.switches();
			devs.reset();
		}
		if !devs.inject(pen, c) {
			// Usually: the input desktop switched since the last check.
			desk.sync(true);
			if desk.switches() != seen {
				seen = desk.switches();
				devs.reset();
				devs.inject(pen, c);
			}
		}
	};
	loop {
		let wait = if held.active() {
			POINTER_SILENCE.saturating_sub(last.elapsed())
		} else {
			Duration::from_secs(3600)
		};
		match rx.recv_timeout(wait) {
			Ok(PointerCmd::Frame { pen, contacts }) => {
				send(&mut desk, &mut devs, pen, &contacts);
				held.note(pen, &contacts);
				last = Instant::now();
			}
			Ok(PointerCmd::Lift) => {
				for (pen, c) in held.lift() {
					send(&mut desk, &mut devs, pen, &c);
				}
			}
			Err(RecvTimeoutError::Timeout) => {
				if held.active() && last.elapsed() >= POINTER_SILENCE {
					for (pen, c) in held.lift() {
						send(&mut desk, &mut devs, pen, &c);
					}
				}
			}
			Err(RecvTimeoutError::Disconnected) => {
				for (pen, c) in held.lift() {
					send(&mut desk, &mut devs, pen, &c);
				}
				return;
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use pulsar_sysagent::proto::{
		PEN_FLAG_ERASER, PEN_FLAG_INVERTED, POINTER_FLAG_DOWN, POINTER_FLAG_INCONTACT,
		POINTER_FLAG_INRANGE, POINTER_FLAG_UP, POINTER_FLAG_UPDATE,
	};

	const DOWN: u32 = POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT;

	#[test]
	fn a_pen_in_contact_is_lifted_then_leaves() {
		let mut h = PointerHeld::default();
		let p = PointerContact {
			flags: DOWN,
			x: 5,
			y: 6,
			pressure: 700,
			pen_flags: PEN_FLAG_INVERTED | PEN_FLAG_ERASER,
			..Default::default()
		};
		h.note(true, &[p]);
		assert!(h.active());
		let out = h.lift();
		let flags: Vec<u32> = out.iter().map(|(_, c)| c[0].flags).collect();
		assert_eq!(flags, vec![POINTER_FLAG_UP | POINTER_FLAG_INRANGE, POINTER_FLAG_UPDATE]);
		assert!(out.iter().all(|(pen, c)| *pen && c[0].pressure == 0 && c[0].pen_flags == PEN_FLAG_INVERTED && c[0].x == 5));
		assert!(!h.active() && h.lift().is_empty());
		// A pen that already left range needs nothing.
		h.note(true, &[PointerContact { flags: POINTER_FLAG_UPDATE, ..p }]);
		assert!(!h.active());
	}

	#[test]
	fn fingers_still_down_are_lifted_together() {
		let mut h = PointerHeld::default();
		let f = |id, flags| PointerContact { id, flags, ..Default::default() };
		h.note(false, &[f(0, DOWN), f(1, DOWN)]);
		h.note(false, &[f(0, POINTER_FLAG_UP)]);
		let out = h.lift();
		assert_eq!(out.len(), 1);
		assert_eq!(out[0].1, vec![f(1, POINTER_FLAG_UP)]);
		assert!(!h.active());
	}
}
