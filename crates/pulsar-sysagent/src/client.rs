//! `SysInput` — the host-side client of the in-session SYSTEM helper (Windows only).
//!
//! Same method set as `pulsar_core::input::DesktopInput`, but every method returns a
//! `Result`: on `Err` the caller drops the `SysInput` and falls back to `DesktopInput`
//! (plain SendInput). Methods never block on the helper — frames go through a bounded queue
//! to a writer thread; a stuck or dead helper shows up as `Err(Disconnected)`.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Duration;

use windows_sys::Win32::Foundation::{LocalFree, ERROR_PIPE_BUSY};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
use windows_sys::Win32::Security::{OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID};
use windows_sys::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION;
use windows_sys::Win32::System::Pipes::{
	GetNamedPipeServerProcessId, GetNamedPipeServerSessionId, WaitNamedPipeW,
};

use crate::geom::{absolute_move, button_flag, MonitorRect, ScrollAcc, WHEEL_DELTA};
use crate::proto::{
	self, DesktopKind, PointerContact, Reply, Request, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
	MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
	MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL,
};
use crate::win;

/// evdev keycode → `(virtual key, scan code, extended)`. Pass
/// `pulsar_core::input::evdev_to_raw_key` so both injectors share one key table.
pub type KeyMap = fn(u32) -> Option<(u16, u16, bool)>;

#[derive(Debug)]
pub enum SysInputError {
	/// No helper pipe for this session (service not installed / not running / helper
	/// starting). Fall back to SendInput and retry later.
	NotRunning,
	/// A pipe exists but is not owned by LocalSystem in our session — possible squatting.
	/// Never send input to it.
	Untrusted(&'static str),
	/// The helper refused us (version mismatch) or sent garbage.
	Handshake(String),
	/// The helper went away or stopped draining its queue.
	Disconnected,
	Io(io::Error),
}

impl std::fmt::Display for SysInputError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::NotRunning => f.write_str("SYSTEM input helper not running for this session"),
			Self::Untrusted(why) => write!(f, "untrusted helper pipe: {why}"),
			Self::Handshake(m) => write!(f, "helper handshake failed: {m}"),
			Self::Disconnected => f.write_str("SYSTEM input helper disconnected"),
			Self::Io(e) => write!(f, "helper pipe I/O: {e}"),
		}
	}
}

impl std::error::Error for SysInputError {}

impl From<io::Error> for SysInputError {
	fn from(e: io::Error) -> Self {
		Self::Io(e)
	}
}

pub type Result<T> = std::result::Result<T, SysInputError>;

/// Frames queued before the helper is declared stuck (~1 s of 1 kHz mouse).
const QUEUE: usize = 1024;
/// Idle ping period; also how often `desktop()` refreshes.
const PING_EVERY: Duration = Duration::from_millis(500);

enum Msg {
	Frame(Vec<u8>),
	Close,
}

struct Shared {
	alive: AtomicBool,
	desktop: AtomicU8,
}

pub struct SysInput {
	tx: SyncSender<Msg>,
	shared: Arc<Shared>,
	session: u32,
	/// Protocol version the helper acked (1 = an older helper without pointer frames).
	version: u16,
	keymap: KeyMap,
	held_buttons: HashSet<u8>,
	/// vk → (scan, extended) of keys currently down.
	held_keys: HashMap<u16, (u16, bool)>,
	pinching: bool,
	scroll: ScrollAcc,
	monitor: Option<MonitorRect>,
}

fn open_pipe(name: &str) -> Result<File> {
	for attempt in 0..3 {
		let r = std::fs::OpenOptions::new()
			.access_mode(crate::sddl::CLIENT_OPEN_ACCESS)
			// SQOS: the server may only IDENTIFY us, never impersonate our (elevated) token —
			// matters if a squatter ever got a client to connect.
			.security_qos_flags(SECURITY_IDENTIFICATION)
			.open(name);
		match r {
			Ok(f) => return Ok(f),
			Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(SysInputError::NotRunning),
			Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) && attempt < 2 => {
				let w = win::wide(name);
				unsafe { WaitNamedPipeW(w.as_ptr(), 1000) };
			}
			Err(e) => return Err(e.into()),
		}
	}
	Err(SysInputError::NotRunning)
}

/// The pipe must be served from OUR session by a process whose pipe object is owned by
/// LocalSystem. A non-admin user cannot create an object owned by SYSTEM, so this defeats
/// pipe-name squatting by another local user.
fn verify_server(f: &File, session: u32) -> Result<()> {
	let h = f.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
	let mut srv_session = u32::MAX;
	if unsafe { GetNamedPipeServerSessionId(h, &mut srv_session) } == 0 {
		return Err(io::Error::last_os_error().into());
	}
	if srv_session != session {
		return Err(SysInputError::Untrusted("server in another session"));
	}
	let mut pid = 0u32;
	if unsafe { GetNamedPipeServerProcessId(h, &mut pid) } == 0
		|| win::session_of(pid).ok() != Some(session)
	{
		return Err(SysInputError::Untrusted(
			"server process not in our session",
		));
	}
	let mut owner: PSID = std::ptr::null_mut();
	let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
	let rc = unsafe {
		GetSecurityInfo(
			h,
			SE_KERNEL_OBJECT,
			OWNER_SECURITY_INFORMATION,
			&mut owner,
			std::ptr::null_mut(),
			std::ptr::null_mut(),
			std::ptr::null_mut(),
			&mut sd,
		)
	};
	if rc != 0 {
		return Err(io::Error::from_raw_os_error(rc as i32).into());
	}
	let system = !owner.is_null() && unsafe { win::sid_is_local_system(owner) };
	unsafe { LocalFree(sd as _) };
	if !system {
		return Err(SysInputError::Untrusted("pipe not owned by LocalSystem"));
	}
	Ok(())
}

use HandshakeError::{Failed, Rejected};

enum HandshakeError {
	/// The helper answered `Reject { code }` (and closed the pipe).
	Rejected(u8),
	Failed(SysInputError),
}

impl From<SysInputError> for HandshakeError {
	fn from(e: SysInputError) -> Self {
		Failed(e)
	}
}

impl From<io::Error> for HandshakeError {
	fn from(e: io::Error) -> Self {
		Failed(e.into())
	}
}

/// Open the session's pipe, check its owner and say hello as `version`.
fn handshake(session: u32, version: u16) -> std::result::Result<(File, u16), HandshakeError> {
	let mut pipe = open_pipe(&proto::pipe_name(session))?;
	verify_server(&pipe, session)?;
	proto::write_frame(&mut pipe, &Request::Hello { version }.encode())?;
	match proto::read_reply(&mut pipe)? {
		Some(Reply::HelloAck { version: v, session: s }) if v == version && s == session => {
			Ok((pipe, version))
		}
		Some(Reply::Reject { code }) => Err(Rejected(code)),
		other => Err(Failed(SysInputError::Handshake(format!(
			"unexpected reply {other:?}"
		)))),
	}
}

/// Synthetic pen / touch frames to the helper (see [`SysInput::pen_touch`]). Cheap to clone
/// and `Send`: the host's pen / touch worker thread owns one.
#[derive(Clone)]
pub struct SysPointer {
	tx: SyncSender<Msg>,
	shared: Arc<Shared>,
}

impl SysPointer {
	pub fn is_alive(&self) -> bool {
		self.shared.alive.load(Ordering::Acquire)
	}

	/// One frame on the pen device (`pen`, one contact) or the touch device (≤ 10).
	pub fn inject(&self, pen: bool, contacts: &[PointerContact]) -> Result<()> {
		if !self.is_alive() {
			return Err(SysInputError::Disconnected);
		}
		let Some(r) = Request::pointer(pen, contacts) else {
			return Ok(());
		};
		match self.tx.try_send(Msg::Frame(r.encode())) {
			Ok(()) => Ok(()),
			Err(TrySendError::Full(_)) => {
				self.shared.alive.store(false, Ordering::Release);
				Err(SysInputError::Disconnected)
			}
			Err(TrySendError::Disconnected(_)) => Err(SysInputError::Disconnected),
		}
	}
}

fn writer(mut pipe: File, rx: mpsc::Receiver<Msg>, shared: Arc<Shared>) {
	let mut seq = 0u32;
	let dead = |shared: &Shared| shared.alive.store(false, Ordering::Release);
	loop {
		match rx.recv_timeout(PING_EVERY) {
			Ok(Msg::Frame(f)) => {
				if proto::write_frame(&mut pipe, &f).is_err() {
					return dead(&shared);
				}
			}
			Ok(Msg::Close) | Err(RecvTimeoutError::Disconnected) => return dead(&shared),
			Err(RecvTimeoutError::Timeout) => {
				// Request/response on the same thread: a synchronous pipe handle serializes
				// reads and writes, so a parallel blocking reader would stall the writer.
				seq = seq.wrapping_add(1);
				if proto::write_frame(&mut pipe, &Request::Ping { seq }.encode()).is_err() {
					return dead(&shared);
				}
				match proto::read_reply(&mut pipe) {
					Ok(Some(Reply::Pong { seq: s, desktop })) if s == seq => {
						shared.desktop.store(desktop as u8, Ordering::Release);
					}
					_ => return dead(&shared),
				}
			}
		}
	}
}

impl SysInput {
	/// Connect to the helper of the caller's session. Fast (no retries beyond a busy pipe):
	/// call it when a session starts; on `Err` use `DesktopInput`.
	///
	/// Speaks [`proto::VERSION`]; a helper of an older build rejects that, and the connection
	/// is retried as v1 (mouse / keyboard only — [`SysInput::pen_touch`] is then `None`).
	pub fn connect(keymap: KeyMap) -> Result<Self> {
		let session = win::current_session()?;
		let (pipe, version) = match handshake(session, proto::VERSION) {
			Err(Rejected(proto::REJECT_VERSION)) => handshake(session, proto::MIN_VERSION),
			r => r,
		}
		.map_err(|e| match e {
			Rejected(code) => SysInputError::Handshake(format!("rejected (code {code})")),
			Failed(e) => e,
		})?;
		let shared = Arc::new(Shared {
			alive: AtomicBool::new(true),
			desktop: AtomicU8::new(DesktopKind::Unknown as u8),
		});
		let (tx, rx) = mpsc::sync_channel(QUEUE);
		let sh = shared.clone();
		std::thread::Builder::new()
			.name("pulsar-sysinput".into())
			.spawn(move || writer(pipe, rx, sh))?;
		Ok(Self {
			tx,
			shared,
			session,
			version,
			keymap,
			held_buttons: HashSet::new(),
			held_keys: HashMap::new(),
			pinching: false,
			scroll: ScrollAcc::default(),
			monitor: None,
		})
	}

	pub fn session(&self) -> u32 {
		self.session
	}

	/// A sender for synthetic pen / touch frames, when the helper speaks protocol v2. The
	/// frames go through the same queue as the mouse and keyboard.
	pub fn pen_touch(&self) -> Option<SysPointer> {
		(self.version >= proto::VERSION_POINTER).then(|| SysPointer {
			tx: self.tx.clone(),
			shared: self.shared.clone(),
		})
	}

	pub fn is_alive(&self) -> bool {
		self.shared.alive.load(Ordering::Acquire)
	}

	/// The desktop that currently receives input in this session (refreshed every ~0.5 s).
	/// `Winlogon` = lock screen / secure-desktop UAC: capture from the user process is black
	/// there until phase 2 (see the design doc).
	pub fn desktop(&self) -> DesktopKind {
		DesktopKind::from_u8(self.shared.desktop.load(Ordering::Acquire))
	}

	fn send(&self, r: Request) -> Result<()> {
		if !self.is_alive() {
			return Err(SysInputError::Disconnected);
		}
		match self.tx.try_send(Msg::Frame(r.encode())) {
			Ok(()) => Ok(()),
			Err(TrySendError::Full(_)) => {
				// The helper stopped draining: treat as dead so the host falls back now
				// instead of buffering a second of stale input.
				self.shared.alive.store(false, Ordering::Release);
				Err(SysInputError::Disconnected)
			}
			Err(TrySendError::Disconnected(_)) => Err(SysInputError::Disconnected),
		}
	}

	fn mouse(&self, dx: i32, dy: i32, data: i32, flags: u32) -> Result<()> {
		self.send(Request::Mouse {
			dx,
			dy,
			data,
			flags,
		})
	}

	/// See `DesktopInput::set_monitor`.
	pub fn set_monitor(&mut self, rect: Option<MonitorRect>) {
		self.monitor = rect;
	}

	pub fn pointer(&mut self, x: f64, y: f64) -> Result<()> {
		let (dx, dy, flags) = absolute_move(x, y, self.monitor);
		self.mouse(dx, dy, 0, flags)
	}

	/// Relative move, then (multi-monitor) snap the cursor back onto the streamed monitor.
	pub fn pointer_relative(&mut self, dx: f64, dy: f64) -> Result<()> {
		let (dx, dy) = (finite(dx).round() as i32, finite(dy).round() as i32);
		self.mouse(dx, dy, 0, MOUSEEVENTF_MOVE)?;
		if let Some(r) = self.monitor.and_then(|m| m.monitor()) {
			self.send(Request::ClampCursor(r))?;
		}
		Ok(())
	}

	pub fn button(&mut self, button: u8, down: bool) -> Result<()> {
		if down {
			self.held_buttons.insert(button);
		} else {
			self.held_buttons.remove(&button);
		}
		self.mouse(0, 0, 0, button_flag(button, down))
	}

	pub fn scroll(&mut self, dx: f64, dy: f64) -> Result<()> {
		let (v, h) = self.scroll.push(dx, dy);
		if v != 0 {
			self.mouse(0, 0, v.saturating_mul(WHEEL_DELTA), MOUSEEVENTF_WHEEL)?;
		}
		if h != 0 {
			self.mouse(0, 0, h.saturating_mul(WHEEL_DELTA), MOUSEEVENTF_HWHEEL)?;
		}
		Ok(())
	}

	pub fn key(&mut self, code: u32, down: bool) -> Result<()> {
		let Some((vk, scan, ext)) = (self.keymap)(code) else {
			return Ok(());
		};
		if down {
			self.held_keys.insert(vk, (scan, ext));
		} else {
			self.held_keys.remove(&vk);
		}
		let mut flags = if down { 0 } else { KEYEVENTF_KEYUP };
		if ext {
			flags |= KEYEVENTF_EXTENDEDKEY;
		}
		self.send(Request::Key { vk, scan, flags })
	}

	pub fn type_char(&mut self, c: char) -> Result<()> {
		let mut buf = [0u16; 2];
		for unit in c.encode_utf16(&mut buf) {
			for flags in [KEYEVENTF_UNICODE, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP] {
				self.send(Request::Key {
					vk: 0,
					scan: *unit,
					flags,
				})?;
			}
		}
		Ok(())
	}

	pub fn pinch(&mut self, phase: u8, scale: f64) -> Result<()> {
		let phase = phase.min(2);
		self.pinching = phase != 2;
		let scale = if scale.is_finite() && scale > 0.0 {
			scale
		} else {
			1.0
		};
		self.send(Request::Pinch { phase, scale })
	}

	/// Release everything held (control revoked / session end). The helper also releases a
	/// connection's held input when the pipe closes, so a crashed host cannot leave keys
	/// stuck on the secure desktop either.
	pub fn flush_held(&mut self) -> Result<()> {
		let mut res = Ok(());
		if std::mem::take(&mut self.pinching) {
			res = res.and(self.send(Request::Pinch {
				phase: 2,
				scale: 1.0,
			}));
		}
		for b in std::mem::take(&mut self.held_buttons) {
			let up = match b {
				1 => MOUSEEVENTF_RIGHTUP,
				2 => MOUSEEVENTF_MIDDLEUP,
				_ => MOUSEEVENTF_LEFTUP,
			};
			res = res.and(self.mouse(0, 0, 0, up));
		}
		for (vk, (scan, ext)) in std::mem::take(&mut self.held_keys) {
			let flags = KEYEVENTF_KEYUP | if ext { KEYEVENTF_EXTENDEDKEY } else { 0 };
			res = res.and(self.send(Request::Key { vk, scan, flags }));
		}
		res.and(self.send(Request::ReleaseAll))
	}
}

fn finite(v: f64) -> f64 {
	if v.is_finite() {
		v
	} else {
		0.0
	}
}

impl Drop for SysInput {
	fn drop(&mut self) {
		let _ = self.flush_held();
		let _ = self.tx.try_send(Msg::Close);
	}
}
