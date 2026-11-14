//! Wire protocol between the user-level Pulsar host (client) and the in-session SYSTEM
//! helper (server). Pure + cross-platform so it is unit-tested everywhere.
//!
//! Framing: `[u16 LE body length][body]`, body = `[u8 tag][payload]`, all integers
//! little-endian. Bodies are tiny (≤ 17 bytes, a v2 pointer frame ≤ 203); anything above
//! [`MAX_BODY`] is a protocol violation and the helper drops the connection.
//!
//! Versions: v1 = mouse / keyboard / pinch. v2 adds [`Request::Pointer`] (synthetic pen /
//! touch frames). A v2 helper accepts v1 and v2 clients and acks the client's version; a v2
//! client that a v1 helper rejects reconnects as v1 and keeps pen / touch in-process.
//!
//! The helper is a deliberately DUMB injector: the client resolves keycodes, pointer
//! geometry and scroll accumulation, and sends raw `SendInput`-level events. The helper only
//! validates each event against a flag whitelist and injects it. That keeps the code that
//! runs as SYSTEM small and its parser trivial.

use std::io::{self, Read, Write};

/// First four payload bytes of [`Request::Hello`].
pub const MAGIC: [u8; 4] = *b"PSAG";
/// This build's protocol version (see the module docs for what each adds).
pub const VERSION: u16 = 2;
/// Oldest client version the helper still serves.
pub const MIN_VERSION: u16 = 1;
/// First version with [`Request::Pointer`].
pub const VERSION_POINTER: u16 = 2;
/// Largest body either side accepts.
pub const MAX_BODY: usize = 256;

// `SendInput` flag values (stable Win32 ABI), duplicated here so the protocol stays
// cross-platform and testable without windows-sys.
pub const MOUSEEVENTF_MOVE: u32 = 0x0001;
pub const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
pub const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
pub const MOUSEEVENTF_RIGHTDOWN: u32 = 0x0008;
pub const MOUSEEVENTF_RIGHTUP: u32 = 0x0010;
pub const MOUSEEVENTF_MIDDLEDOWN: u32 = 0x0020;
pub const MOUSEEVENTF_MIDDLEUP: u32 = 0x0040;
pub const MOUSEEVENTF_WHEEL: u32 = 0x0800;
pub const MOUSEEVENTF_HWHEEL: u32 = 0x1000;
pub const MOUSEEVENTF_VIRTUALDESK: u32 = 0x4000;
pub const MOUSEEVENTF_ABSOLUTE: u32 = 0x8000;
/// Every mouse flag the helper will inject. Anything else (XBUTTON, MOVE_NOCOALESCE, …) is
/// rejected rather than passed through blindly.
pub const MOUSE_FLAGS_ALLOWED: u32 = MOUSEEVENTF_MOVE
	| MOUSEEVENTF_LEFTDOWN
	| MOUSEEVENTF_LEFTUP
	| MOUSEEVENTF_RIGHTDOWN
	| MOUSEEVENTF_RIGHTUP
	| MOUSEEVENTF_MIDDLEDOWN
	| MOUSEEVENTF_MIDDLEUP
	| MOUSEEVENTF_WHEEL
	| MOUSEEVENTF_HWHEEL
	| MOUSEEVENTF_VIRTUALDESK
	| MOUSEEVENTF_ABSOLUTE;

pub const KEYEVENTF_EXTENDEDKEY: u32 = 0x0001;
pub const KEYEVENTF_KEYUP: u32 = 0x0002;
pub const KEYEVENTF_UNICODE: u32 = 0x0004;
pub const KEY_FLAGS_ALLOWED: u32 = KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP | KEYEVENTF_UNICODE;

// Synthetic pointer flags (`POINTER_FLAG_*` / `PEN_FLAG_*`, stable Win32 ABI).
pub const POINTER_FLAG_INRANGE: u32 = 0x0002;
pub const POINTER_FLAG_INCONTACT: u32 = 0x0004;
pub const POINTER_FLAG_FIRSTBUTTON: u32 = 0x0010;
pub const POINTER_FLAG_SECONDBUTTON: u32 = 0x0020;
pub const POINTER_FLAG_DOWN: u32 = 0x0001_0000;
pub const POINTER_FLAG_UPDATE: u32 = 0x0002_0000;
pub const POINTER_FLAG_UP: u32 = 0x0004_0000;
/// Every pointer flag the helper will inject.
pub const POINTER_FLAGS_ALLOWED: u32 = POINTER_FLAG_INRANGE
	| POINTER_FLAG_INCONTACT
	| POINTER_FLAG_FIRSTBUTTON
	| POINTER_FLAG_SECONDBUTTON
	| POINTER_FLAG_DOWN
	| POINTER_FLAG_UPDATE
	| POINTER_FLAG_UP;
pub const PEN_FLAG_BARREL: u8 = 0x1;
pub const PEN_FLAG_INVERTED: u8 = 0x2;
pub const PEN_FLAG_ERASER: u8 = 0x4;
pub const PEN_FLAGS_ALLOWED: u8 = PEN_FLAG_BARREL | PEN_FLAG_INVERTED | PEN_FLAG_ERASER;
/// Touch contacts per frame (the synthetic touch device's size).
pub const MAX_CONTACTS: usize = 10;
/// Wire size of one [`PointerContact`].
const CONTACT_LEN: usize = 20;

/// One contact of a synthetic pointer frame, in physical virtual-desktop pixels (the host
/// resolved the geometry; the helper only injects it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PointerContact {
	/// Pen: 0. Touch: the slot, 0..[`MAX_CONTACTS`].
	pub id: u8,
	/// `POINTER_FLAG_*` within [`POINTER_FLAGS_ALLOWED`].
	pub flags: u32,
	pub x: i32,
	pub y: i32,
	/// 0..=1024.
	pub pressure: u16,
	/// Pen rotation 0..360 (touch: 0).
	pub rotation: u16,
	/// Pen tilt -90..=90 (touch: 0).
	pub tilt_x: i8,
	pub tilt_y: i8,
	/// `PEN_FLAG_*` (touch: 0).
	pub pen_flags: u8,
}

/// A rectangle in virtual-desktop pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
	pub left: i32,
	pub top: i32,
	pub width: i32,
	pub height: i32,
}

/// Which desktop currently receives input in the helper's session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DesktopKind {
	/// `winsta0\Default` — the normal user desktop (UAC prompt with PromptOnSecureDesktop=0).
	Default = 0,
	/// `winsta0\Winlogon` — lock screen, Ctrl+Alt+Del screen, secure-desktop UAC prompt.
	Winlogon = 1,
	/// Any other desktop (screensaver `Screen-saver`, third-party desktops).
	Other = 2,
	/// Not determined yet / OpenInputDesktop failed.
	Unknown = 3,
}

impl DesktopKind {
	pub fn from_u8(v: u8) -> Self {
		match v {
			0 => Self::Default,
			1 => Self::Winlogon,
			2 => Self::Other,
			_ => Self::Unknown,
		}
	}

	/// Classify a desktop by its object name (case-insensitive, as Windows treats it).
	pub fn from_name(name: &str) -> Self {
		if name.eq_ignore_ascii_case("Default") {
			Self::Default
		} else if name.eq_ignore_ascii_case("Winlogon") {
			Self::Winlogon
		} else if name.is_empty() {
			Self::Unknown
		} else {
			Self::Other
		}
	}
}

/// Client (host) → helper.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Request {
	/// First frame of every connection: `MAGIC` + protocol version.
	Hello { version: u16 },
	/// One `MOUSEINPUT` (dx/dy/mouseData/dwFlags).
	Mouse {
		dx: i32,
		dy: i32,
		data: i32,
		flags: u32,
	},
	/// One `KEYBDINPUT` (wVk/wScan/dwFlags). For `KEYEVENTF_UNICODE` `vk` must be 0 and
	/// `scan` carries the UTF-16 unit.
	Key { vk: u16, scan: u16, flags: u32 },
	/// Snap the cursor back inside this rect (after a relative move).
	ClampCursor(Rect),
	/// Touch pinch at the cursor: phase 0 begin / 1 update / 2 end, cumulative scale.
	Pinch { phase: u8, scale: f64 },
	/// Release every key/button this connection holds down (and lift its pen / fingers).
	ReleaseAll,
	/// v2: one synthetic pointer frame — the pen device (`pen`, one contact) or the touch
	/// device (`n` contacts). Build it with [`Request::pointer`].
	Pointer {
		pen: bool,
		n: u8,
		contacts: [PointerContact; MAX_CONTACTS],
	},
	/// Liveness + desktop query; answered with [`Reply::Pong`].
	Ping { seq: u32 },
}

/// Helper → client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
	HelloAck {
		version: u16,
		session: u32,
	},
	/// Connection refused; see the `REJECT_*` codes. The helper closes the pipe after it.
	Reject {
		code: u8,
	},
	Pong {
		seq: u32,
		desktop: DesktopKind,
	},
}

pub const REJECT_VERSION: u8 = 1;
pub const REJECT_BAD_HELLO: u8 = 2;

const T_HELLO: u8 = 0x01;
const T_MOUSE: u8 = 0x02;
const T_KEY: u8 = 0x03;
const T_CLAMP: u8 = 0x04;
const T_PINCH: u8 = 0x05;
const T_RELEASE_ALL: u8 = 0x06;
const T_PING: u8 = 0x07;
const T_POINTER: u8 = 0x08;
const T_HELLO_ACK: u8 = 0x81;
const T_REJECT: u8 = 0x82;
const T_PONG: u8 = 0x83;

#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
	Empty,
	TooLong(usize),
	UnknownTag(u8),
	/// Body length does not match the tag's payload size.
	BadLength {
		tag: u8,
		len: usize,
	},
	BadMagic,
	/// A flag outside the whitelist, or a malformed value (e.g. Unicode key with vk != 0).
	Invalid(&'static str),
}

impl std::fmt::Display for DecodeError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{self:?}")
	}
}

impl std::error::Error for DecodeError {}

impl From<DecodeError> for io::Error {
	fn from(e: DecodeError) -> Self {
		io::Error::new(io::ErrorKind::InvalidData, e)
	}
}

struct Cur<'a>(&'a [u8]);

impl Cur<'_> {
	fn u8(&mut self) -> u8 {
		let v = self.0[0];
		self.0 = &self.0[1..];
		v
	}
	fn u16(&mut self) -> u16 {
		let v = u16::from_le_bytes([self.0[0], self.0[1]]);
		self.0 = &self.0[2..];
		v
	}
	fn u32(&mut self) -> u32 {
		let v = u32::from_le_bytes(self.0[..4].try_into().unwrap());
		self.0 = &self.0[4..];
		v
	}
	fn i32(&mut self) -> i32 {
		self.u32() as i32
	}
	fn f64(&mut self) -> f64 {
		let v = f64::from_le_bytes(self.0[..8].try_into().unwrap());
		self.0 = &self.0[8..];
		v
	}
}

fn expect_len(tag: u8, payload: &[u8], n: usize) -> Result<(), DecodeError> {
	if payload.len() == n {
		Ok(())
	} else {
		Err(DecodeError::BadLength {
			tag,
			len: payload.len(),
		})
	}
}

/// Range checks of one pointer contact (flags whitelist, pen-only fields on the pen only).
fn validate_contact(pen: bool, p: &PointerContact) -> Result<(), DecodeError> {
	let edges = p.flags & (POINTER_FLAG_DOWN | POINTER_FLAG_UPDATE | POINTER_FLAG_UP);
	if p.flags & !POINTER_FLAGS_ALLOWED != 0 || edges.count_ones() != 1 {
		return Err(DecodeError::Invalid("pointer flags"));
	}
	if p.pressure > 1024 {
		return Err(DecodeError::Invalid("pointer pressure"));
	}
	if pen {
		if p.id != 0
			|| p.rotation >= 360
			|| !(-90..=90).contains(&p.tilt_x)
			|| !(-90..=90).contains(&p.tilt_y)
			|| p.pen_flags & !PEN_FLAGS_ALLOWED != 0
		{
			return Err(DecodeError::Invalid("pen contact"));
		}
	} else if p.id as usize >= MAX_CONTACTS
		|| p.rotation != 0
		|| p.tilt_x != 0
		|| p.tilt_y != 0
		|| p.pen_flags != 0
	{
		return Err(DecodeError::Invalid("touch contact"));
	}
	Ok(())
}

/// Prefix a body with its u16 length.
fn frame(body: Vec<u8>) -> Vec<u8> {
	debug_assert!(body.len() <= MAX_BODY);
	let mut out = Vec::with_capacity(body.len() + 2);
	out.extend_from_slice(&(body.len() as u16).to_le_bytes());
	out.extend_from_slice(&body);
	out
}

impl Request {
	/// A [`Request::Pointer`]; `None` when `contacts` is empty or too long.
	pub fn pointer(pen: bool, contacts: &[PointerContact]) -> Option<Self> {
		if contacts.is_empty() || contacts.len() > if pen { 1 } else { MAX_CONTACTS } {
			return None;
		}
		let mut all = [PointerContact::default(); MAX_CONTACTS];
		all[..contacts.len()].copy_from_slice(contacts);
		Some(Request::Pointer {
			pen,
			n: contacts.len() as u8,
			contacts: all,
		})
	}

	/// Encode as a complete frame (length prefix included).
	pub fn encode(&self) -> Vec<u8> {
		let mut b = Vec::with_capacity(20);
		match *self {
			Request::Hello { version } => {
				b.push(T_HELLO);
				b.extend_from_slice(&MAGIC);
				b.extend_from_slice(&version.to_le_bytes());
			}
			Request::Mouse {
				dx,
				dy,
				data,
				flags,
			} => {
				b.push(T_MOUSE);
				b.extend_from_slice(&dx.to_le_bytes());
				b.extend_from_slice(&dy.to_le_bytes());
				b.extend_from_slice(&data.to_le_bytes());
				b.extend_from_slice(&flags.to_le_bytes());
			}
			Request::Key { vk, scan, flags } => {
				b.push(T_KEY);
				b.extend_from_slice(&vk.to_le_bytes());
				b.extend_from_slice(&scan.to_le_bytes());
				b.extend_from_slice(&flags.to_le_bytes());
			}
			Request::ClampCursor(r) => {
				b.push(T_CLAMP);
				for v in [r.left, r.top, r.width, r.height] {
					b.extend_from_slice(&v.to_le_bytes());
				}
			}
			Request::Pinch { phase, scale } => {
				b.push(T_PINCH);
				b.push(phase);
				b.extend_from_slice(&scale.to_le_bytes());
			}
			Request::ReleaseAll => b.push(T_RELEASE_ALL),
			Request::Ping { seq } => {
				b.push(T_PING);
				b.extend_from_slice(&seq.to_le_bytes());
			}
			Request::Pointer { pen, n, contacts } => {
				b.push(T_POINTER);
				b.push(pen as u8);
				b.push(n);
				for c in &contacts[..(n as usize).min(MAX_CONTACTS)] {
					b.push(c.id);
					b.extend_from_slice(&c.flags.to_le_bytes());
					b.extend_from_slice(&c.x.to_le_bytes());
					b.extend_from_slice(&c.y.to_le_bytes());
					b.extend_from_slice(&c.pressure.to_le_bytes());
					b.extend_from_slice(&c.rotation.to_le_bytes());
					b.push(c.tilt_x as u8);
					b.push(c.tilt_y as u8);
					b.push(c.pen_flags);
				}
			}
		}
		frame(b)
	}

	/// Decode one body (without the length prefix) and validate it.
	pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
		let (&tag, payload) = body.split_first().ok_or(DecodeError::Empty)?;
		if body.len() > MAX_BODY {
			return Err(DecodeError::TooLong(body.len()));
		}
		let mut c = Cur(payload);
		let req = match tag {
			T_HELLO => {
				expect_len(tag, payload, 6)?;
				if payload[..4] != MAGIC {
					return Err(DecodeError::BadMagic);
				}
				c.0 = &payload[4..];
				Request::Hello { version: c.u16() }
			}
			T_MOUSE => {
				expect_len(tag, payload, 16)?;
				let (dx, dy, data, flags) = (c.i32(), c.i32(), c.i32(), c.u32());
				if flags == 0 || flags & !MOUSE_FLAGS_ALLOWED != 0 {
					return Err(DecodeError::Invalid("mouse flags"));
				}
				Request::Mouse {
					dx,
					dy,
					data,
					flags,
				}
			}
			T_KEY => {
				expect_len(tag, payload, 8)?;
				let (vk, scan, flags) = (c.u16(), c.u16(), c.u32());
				if flags & !KEY_FLAGS_ALLOWED != 0 {
					return Err(DecodeError::Invalid("key flags"));
				}
				if flags & KEYEVENTF_UNICODE != 0 {
					if vk != 0 || flags & KEYEVENTF_EXTENDEDKEY != 0 {
						return Err(DecodeError::Invalid("unicode key"));
					}
				} else if vk == 0 || vk > 0xFE {
					return Err(DecodeError::Invalid("virtual key"));
				}
				Request::Key { vk, scan, flags }
			}
			T_CLAMP => {
				expect_len(tag, payload, 16)?;
				let r = Rect {
					left: c.i32(),
					top: c.i32(),
					width: c.i32(),
					height: c.i32(),
				};
				if r.width <= 0 || r.height <= 0 {
					return Err(DecodeError::Invalid("clamp rect"));
				}
				Request::ClampCursor(r)
			}
			T_PINCH => {
				expect_len(tag, payload, 9)?;
				let (phase, scale) = (c.u8(), c.f64());
				if phase > 2 || !scale.is_finite() {
					return Err(DecodeError::Invalid("pinch"));
				}
				Request::Pinch { phase, scale }
			}
			T_RELEASE_ALL => {
				expect_len(tag, payload, 0)?;
				Request::ReleaseAll
			}
			T_PING => {
				expect_len(tag, payload, 4)?;
				Request::Ping { seq: c.u32() }
			}
			T_POINTER => {
				if payload.len() < 2 {
					return Err(DecodeError::BadLength {
						tag,
						len: payload.len(),
					});
				}
				let (pen, n) = (c.u8(), c.u8() as usize);
				expect_len(tag, payload, 2 + n * CONTACT_LEN)?;
				if pen > 1 || n == 0 || n > if pen == 1 { 1 } else { MAX_CONTACTS } {
					return Err(DecodeError::Invalid("pointer count"));
				}
				let mut contacts = [PointerContact::default(); MAX_CONTACTS];
				for slot in contacts.iter_mut().take(n) {
					let p = PointerContact {
						id: c.u8(),
						flags: c.u32(),
						x: c.i32(),
						y: c.i32(),
						pressure: c.u16(),
						rotation: c.u16(),
						tilt_x: c.u8() as i8,
						tilt_y: c.u8() as i8,
						pen_flags: c.u8(),
					};
					validate_contact(pen == 1, &p)?;
					*slot = p;
				}
				// Contact ids within a frame are unique.
				for i in 0..n {
					if contacts[..i].iter().any(|o| o.id == contacts[i].id) {
						return Err(DecodeError::Invalid("pointer id"));
					}
				}
				Request::Pointer {
					pen: pen == 1,
					n: n as u8,
					contacts,
				}
			}
			t => return Err(DecodeError::UnknownTag(t)),
		};
		Ok(req)
	}
}

impl Reply {
	pub fn encode(&self) -> Vec<u8> {
		let mut b = Vec::with_capacity(8);
		match *self {
			Reply::HelloAck { version, session } => {
				b.push(T_HELLO_ACK);
				b.extend_from_slice(&version.to_le_bytes());
				b.extend_from_slice(&session.to_le_bytes());
			}
			Reply::Reject { code } => {
				b.push(T_REJECT);
				b.push(code);
			}
			Reply::Pong { seq, desktop } => {
				b.push(T_PONG);
				b.extend_from_slice(&seq.to_le_bytes());
				b.push(desktop as u8);
			}
		}
		frame(b)
	}

	pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
		let (&tag, payload) = body.split_first().ok_or(DecodeError::Empty)?;
		if body.len() > MAX_BODY {
			return Err(DecodeError::TooLong(body.len()));
		}
		let mut c = Cur(payload);
		Ok(match tag {
			T_HELLO_ACK => {
				expect_len(tag, payload, 6)?;
				Reply::HelloAck {
					version: c.u16(),
					session: c.u32(),
				}
			}
			T_REJECT => {
				expect_len(tag, payload, 1)?;
				Reply::Reject { code: c.u8() }
			}
			T_PONG => {
				expect_len(tag, payload, 5)?;
				Reply::Pong {
					seq: c.u32(),
					desktop: DesktopKind::from_u8(c.u8()),
				}
			}
			t => return Err(DecodeError::UnknownTag(t)),
		})
	}
}

/// Read one frame body (blocking). `Ok(None)` = clean EOF before a frame started.
pub fn read_body<R: Read>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
	let mut len = [0u8; 2];
	match r.read_exact(&mut len) {
		Ok(()) => {}
		Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
		// A closed pipe reads as BrokenPipe on Windows (ERROR_BROKEN_PIPE).
		Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(None),
		Err(e) => return Err(e),
	}
	let len = u16::from_le_bytes(len) as usize;
	if len == 0 || len > MAX_BODY {
		return Err(DecodeError::TooLong(len).into());
	}
	let mut body = vec![0u8; len];
	r.read_exact(&mut body)?;
	Ok(Some(body))
}

pub fn read_request<R: Read>(r: &mut R) -> io::Result<Option<Request>> {
	match read_body(r)? {
		Some(b) => Ok(Some(Request::decode(&b)?)),
		None => Ok(None),
	}
}

pub fn read_reply<R: Read>(r: &mut R) -> io::Result<Option<Reply>> {
	match read_body(r)? {
		Some(b) => Ok(Some(Reply::decode(&b)?)),
		None => Ok(None),
	}
}

pub fn write_frame<W: Write>(w: &mut W, frame: &[u8]) -> io::Result<()> {
	w.write_all(frame)?;
	w.flush()
}

/// Per-session pipe name. One helper per session owns it.
pub fn pipe_name(session: u32) -> String {
	format!(r"\\.\pipe\pulsar-sysagent-{session}")
}

#[cfg(test)]
mod tests {
	use super::*;

	fn roundtrip_req(r: Request) {
		let f = r.encode();
		let len = u16::from_le_bytes([f[0], f[1]]) as usize;
		assert_eq!(len, f.len() - 2, "length prefix of {r:?}");
		assert_eq!(Request::decode(&f[2..]), Ok(r));
		// And through the streaming reader.
		let mut cur = std::io::Cursor::new(f);
		assert_eq!(read_request(&mut cur).unwrap(), Some(r));
		assert_eq!(
			read_request(&mut cur).unwrap(),
			None,
			"clean EOF after the frame"
		);
	}

	#[test]
	fn requests_roundtrip() {
		roundtrip_req(Request::Hello { version: VERSION });
		roundtrip_req(Request::Mouse {
			dx: -12,
			dy: 65535,
			data: -240,
			flags: MOUSEEVENTF_WHEEL,
		});
		roundtrip_req(Request::Mouse {
			dx: 100,
			dy: 200,
			data: 0,
			flags: MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK | MOUSEEVENTF_MOVE,
		});
		roundtrip_req(Request::Key {
			vk: 0x26,
			scan: 0x48,
			flags: KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP,
		});
		roundtrip_req(Request::Key {
			vk: 0,
			scan: 0x00E7,
			flags: KEYEVENTF_UNICODE,
		});
		roundtrip_req(Request::ClampCursor(Rect {
			left: -1920,
			top: 0,
			width: 1920,
			height: 1080,
		}));
		roundtrip_req(Request::Pinch {
			phase: 1,
			scale: 1.25,
		});
		roundtrip_req(Request::ReleaseAll);
		roundtrip_req(Request::Ping { seq: u32::MAX });
	}

	#[test]
	fn replies_roundtrip() {
		for r in [
			Reply::HelloAck {
				version: VERSION,
				session: 3,
			},
			Reply::Reject {
				code: REJECT_VERSION,
			},
			Reply::Pong {
				seq: 7,
				desktop: DesktopKind::Winlogon,
			},
			Reply::Pong {
				seq: 8,
				desktop: DesktopKind::Default,
			},
		] {
			let f = r.encode();
			assert_eq!(Reply::decode(&f[2..]), Ok(r));
			let mut cur = std::io::Cursor::new(f);
			assert_eq!(read_reply(&mut cur).unwrap(), Some(r));
		}
	}

	#[test]
	fn exact_wire_bytes_are_stable() {
		// Pin the layout: a silent field reorder would break host↔helper of different builds.
		assert_eq!(
			Request::Key {
				vk: 0x41,
				scan: 0x1E,
				flags: KEYEVENTF_KEYUP
			}
			.encode(),
			vec![9, 0, T_KEY, 0x41, 0, 0x1E, 0, 2, 0, 0, 0]
		);
		assert_eq!(
			Request::Hello { version: 1 }.encode(),
			vec![7, 0, T_HELLO, b'P', b'S', b'A', b'G', 1, 0]
		);
		assert_eq!(
			Reply::Pong {
				seq: 1,
				desktop: DesktopKind::Winlogon
			}
			.encode(),
			vec![6, 0, T_PONG, 1, 0, 0, 0, 1]
		);
	}

	#[test]
	fn rejects_bad_hello() {
		let mut f = Request::Hello { version: 1 }.encode();
		f[3] = b'X';
		assert_eq!(Request::decode(&f[2..]), Err(DecodeError::BadMagic));
	}

	#[test]
	fn rejects_disallowed_flags() {
		// XBUTTONDOWN (0x80) and MOVE_NOCOALESCE (0x2000) are not whitelisted.
		for flags in [0x80u32, 0x2000, 0, MOUSEEVENTF_MOVE | 0x0100] {
			let f = Request::Mouse {
				dx: 0,
				dy: 0,
				data: 0,
				flags,
			}
			.encode();
			assert_eq!(
				Request::decode(&f[2..]),
				Err(DecodeError::Invalid("mouse flags")),
				"{flags:#x}"
			);
		}
		// KEYEVENTF_SCANCODE (0x8) is not used by Pulsar → rejected.
		let f = Request::Key {
			vk: 0x41,
			scan: 0,
			flags: 0x8,
		}
		.encode();
		assert_eq!(
			Request::decode(&f[2..]),
			Err(DecodeError::Invalid("key flags"))
		);
		// Unicode events must carry vk 0.
		let f = Request::Key {
			vk: 0x41,
			scan: 0x41,
			flags: KEYEVENTF_UNICODE,
		}
		.encode();
		assert_eq!(
			Request::decode(&f[2..]),
			Err(DecodeError::Invalid("unicode key"))
		);
		// A non-unicode key needs a real VK.
		let f = Request::Key {
			vk: 0,
			scan: 0x1E,
			flags: 0,
		}
		.encode();
		assert_eq!(
			Request::decode(&f[2..]),
			Err(DecodeError::Invalid("virtual key"))
		);
		let f = Request::Pinch {
			phase: 3,
			scale: 1.0,
		}
		.encode();
		assert_eq!(Request::decode(&f[2..]), Err(DecodeError::Invalid("pinch")));
		let f = Request::Pinch {
			phase: 1,
			scale: f64::NAN,
		}
		.encode();
		assert_eq!(Request::decode(&f[2..]), Err(DecodeError::Invalid("pinch")));
		let f = Request::ClampCursor(Rect {
			left: 0,
			top: 0,
			width: 0,
			height: 10,
		})
		.encode();
		assert_eq!(
			Request::decode(&f[2..]),
			Err(DecodeError::Invalid("clamp rect"))
		);
	}

	#[test]
	fn rejects_malformed_bodies() {
		assert_eq!(Request::decode(&[]), Err(DecodeError::Empty));
		assert_eq!(Request::decode(&[0x7F]), Err(DecodeError::UnknownTag(0x7F)));
		// Truncated key payload.
		assert_eq!(
			Request::decode(&[T_KEY, 0x41, 0]),
			Err(DecodeError::BadLength { tag: T_KEY, len: 2 })
		);
		// Trailing garbage after a ping.
		assert_eq!(
			Request::decode(&[T_PING, 1, 0, 0, 0, 9]),
			Err(DecodeError::BadLength {
				tag: T_PING,
				len: 5
			})
		);
		// Replies are not requests and vice versa.
		let pong = Reply::Pong {
			seq: 1,
			desktop: DesktopKind::Default,
		}
		.encode();
		assert_eq!(
			Request::decode(&pong[2..]),
			Err(DecodeError::UnknownTag(T_PONG))
		);
		let ping = Request::Ping { seq: 1 }.encode();
		assert_eq!(
			Reply::decode(&ping[2..]),
			Err(DecodeError::UnknownTag(T_PING))
		);
		let big = vec![T_PING; MAX_BODY + 1];
		assert_eq!(
			Request::decode(&big),
			Err(DecodeError::TooLong(MAX_BODY + 1))
		);
	}

	#[test]
	fn stream_reader_rejects_oversized_and_zero_frames() {
		let mut cur = std::io::Cursor::new(vec![0xFF, 0xFF, 1, 2, 3]);
		assert_eq!(
			read_body(&mut cur).unwrap_err().kind(),
			io::ErrorKind::InvalidData
		);
		let mut cur = std::io::Cursor::new(vec![0, 0]);
		assert_eq!(
			read_body(&mut cur).unwrap_err().kind(),
			io::ErrorKind::InvalidData
		);
		// A frame cut short mid-body is an error, not a clean EOF.
		let mut f = Request::Ping { seq: 1 }.encode();
		f.truncate(4);
		let mut cur = std::io::Cursor::new(f);
		assert_eq!(
			read_body(&mut cur).unwrap_err().kind(),
			io::ErrorKind::UnexpectedEof
		);
	}

	#[test]
	fn multiple_frames_back_to_back() {
		let reqs = [
			Request::Hello { version: VERSION },
			Request::Mouse {
				dx: 1,
				dy: 2,
				data: 0,
				flags: MOUSEEVENTF_MOVE,
			},
			Request::ReleaseAll,
			Request::Ping { seq: 42 },
		];
		let bytes: Vec<u8> = reqs.iter().flat_map(|r| r.encode()).collect();
		let mut cur = std::io::Cursor::new(bytes);
		for r in reqs {
			assert_eq!(read_request(&mut cur).unwrap(), Some(r));
		}
		assert_eq!(read_request(&mut cur).unwrap(), None);
	}

	fn pen(flags: u32) -> PointerContact {
		PointerContact {
			id: 0,
			flags,
			x: -1920,
			y: 1079,
			pressure: 512,
			rotation: 359,
			tilt_x: -90,
			tilt_y: 45,
			pen_flags: PEN_FLAG_INVERTED | PEN_FLAG_ERASER,
		}
	}

	fn finger(id: u8, flags: u32) -> PointerContact {
		PointerContact {
			id,
			flags,
			x: 100 + id as i32,
			y: 200,
			pressure: 512,
			..Default::default()
		}
	}

	#[test]
	fn pointer_frames_roundtrip() {
		let down = POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT;
		roundtrip_req(Request::pointer(true, &[pen(down | POINTER_FLAG_FIRSTBUTTON)]).unwrap());
		let all: Vec<_> = (0..MAX_CONTACTS as u8).map(|i| finger(i, down)).collect();
		let r = Request::pointer(false, &all).unwrap();
		assert_eq!(r.encode().len(), 2 + 3 + MAX_CONTACTS * CONTACT_LEN);
		assert!(r.encode().len() - 2 <= MAX_BODY);
		roundtrip_req(r);
		roundtrip_req(Request::pointer(false, &[finger(3, POINTER_FLAG_UP)]).unwrap());
		assert!(Request::pointer(true, &[pen(down), pen(down)]).is_none());
		assert!(Request::pointer(false, &[]).is_none());
	}

	#[test]
	fn pointer_frames_are_validated() {
		let bad = |r: Request, why: &str| {
			let f = r.encode();
			assert_eq!(Request::decode(&f[2..]), Err(DecodeError::Invalid(match why {
				"flags" => "pointer flags",
				"pen" => "pen contact",
				"touch" => "touch contact",
				"id" => "pointer id",
				_ => "pointer pressure",
			})), "{r:?}");
		};
		let mv = POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT;
		// No edge flag / two edge flags / a flag outside the whitelist (CANCELED 0x8000).
		bad(Request::pointer(false, &[finger(0, POINTER_FLAG_INRANGE)]).unwrap(), "flags");
		bad(Request::pointer(false, &[finger(0, POINTER_FLAG_DOWN | POINTER_FLAG_UP)]).unwrap(), "flags");
		bad(Request::pointer(false, &[finger(0, mv | 0x8000)]).unwrap(), "flags");
		bad(Request::pointer(false, &[PointerContact { pressure: 1025, ..finger(0, mv) }]).unwrap(), "pressure");
		bad(Request::pointer(true, &[PointerContact { id: 1, ..pen(mv) }]).unwrap(), "pen");
		bad(Request::pointer(true, &[PointerContact { rotation: 360, ..pen(mv) }]).unwrap(), "pen");
		bad(Request::pointer(true, &[PointerContact { tilt_x: -91, ..pen(mv) }]).unwrap(), "pen");
		bad(Request::pointer(true, &[PointerContact { pen_flags: 0x8, ..pen(mv) }]).unwrap(), "pen");
		bad(Request::pointer(false, &[finger(10, mv)]).unwrap(), "touch");
		bad(Request::pointer(false, &[PointerContact { pen_flags: PEN_FLAG_BARREL, ..finger(0, mv) }]).unwrap(), "touch");
		bad(Request::pointer(false, &[finger(2, mv), finger(2, mv)]).unwrap(), "id");
		// A pen frame with two contacts, a count that does not match the length.
		let mut f = Request::pointer(false, &[finger(0, mv), finger(1, mv)]).unwrap().encode();
		f[3] = 1; // pen
		assert_eq!(Request::decode(&f[2..]), Err(DecodeError::Invalid("pointer count")));
		let mut f = Request::pointer(false, &[finger(0, mv)]).unwrap().encode();
		f[4] = 2;
		assert_eq!(Request::decode(&f[2..]), Err(DecodeError::BadLength { tag: T_POINTER, len: 22 }));
		assert_eq!(Request::decode(&[T_POINTER, 0]), Err(DecodeError::BadLength { tag: T_POINTER, len: 1 }));
	}

	#[test]
	fn desktop_kind_names() {
		assert_eq!(DesktopKind::from_name("Default"), DesktopKind::Default);
		assert_eq!(DesktopKind::from_name("winlogon"), DesktopKind::Winlogon);
		assert_eq!(DesktopKind::from_name("Screen-saver"), DesktopKind::Other);
		assert_eq!(DesktopKind::from_name(""), DesktopKind::Unknown);
		for k in [
			DesktopKind::Default,
			DesktopKind::Winlogon,
			DesktopKind::Other,
			DesktopKind::Unknown,
		] {
			assert_eq!(DesktopKind::from_u8(k as u8), k);
		}
	}

	#[test]
	fn pipe_name_is_per_session() {
		assert_eq!(pipe_name(1), r"\\.\pipe\pulsar-sysagent-1");
		assert_ne!(pipe_name(1), pipe_name(2));
	}
}
