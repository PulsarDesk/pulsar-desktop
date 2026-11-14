//! Pen + touch from the renderer window (Windows). The video child window sits over the
//! webview, so a stylus or a finger on the video reaches only this window as `WM_POINTER*`.
//! While control is engaged (overlay closed) and the app said the host injects them
//! (`pentouch <pen> <touch>` on stdin), they are reported on stdout and NOT turned into
//! mouse messages:
//!
//! - `pen <x> <y> <pressure> <tilt_x> <tilt_y> <twist> <flags>` — position as 0..1 of the
//!   video, pressure 0..1, tilt / twist in degrees; flags bit 0 in range, 1 contact,
//!   2 eraser, 3 barrel button;
//! - `touch <id>,<x>,<y>,<pressure> …` — every finger of the frame (none = all lifted).
//!
//! The app parses them in `render_stats.rs` into `InputEvent::Pen` / `InputEvent::Touch`.
//! Untested on real pen / touch hardware.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;
use windows::Win32::Foundation::{HWND, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::UI::Input::Pointer::{
	GetPointerFrameTouchInfo, GetPointerPenInfo, GetPointerType, POINTER_FLAG_CANCELED, POINTER_FLAG_INCONTACT,
	POINTER_FLAG_INRANGE, POINTER_FLAG_UP, POINTER_PEN_INFO, POINTER_TOUCH_INFO,
};
use windows::Win32::UI::WindowsAndMessaging::{
	GetClientRect, PEN_FLAG_BARREL, PEN_FLAG_ERASER, PEN_FLAG_INVERTED, POINTER_INPUT_TYPE, PT_PEN, PT_TOUCH,
	WM_POINTERCAPTURECHANGED, WM_POINTERDOWN, WM_POINTERLEAVE, WM_POINTERUP, WM_POINTERUPDATE,
};

/// What the app wants forwarded (the host's `FEAT_PEN` / `FEAT_TOUCH`).
static PEN_ON: AtomicBool = AtomicBool::new(false);
static TOUCH_ON: AtomicBool = AtomicBool::new(false);
/// Something is down / in range on the host — released when control is let go.
static PEN_ACTIVE: AtomicBool = AtomicBool::new(false);
static TOUCH_ACTIVE: AtomicBool = AtomicBool::new(false);
/// The last touch frame reported (each contact of a frame gets its own message).
static LAST_FRAME: AtomicU32 = AtomicU32::new(u32::MAX);
/// Where the picture sits in the window, client fractions `(x0, y0, x1, y1)`.
static VIDEO: Mutex<(f32, f32, f32, f32)> = Mutex::new((0.0, 0.0, 1.0, 1.0));

/// Stdin `pentouch <pen> <touch>`.
pub(super) fn configure(rest: &str) {
	let mut it = rest.split_whitespace().map(|v| v == "1");
	let (pen, touch) = (it.next().unwrap_or(false), it.next().unwrap_or(false));
	PEN_ON.store(pen, Ordering::SeqCst);
	TOUCH_ON.store(touch, Ordering::SeqCst);
}

pub(super) fn set_video(f: (f32, f32, f32, f32)) {
	*VIDEO.lock().unwrap() = f;
}

/// A client point → 0..1 of the video (the letterbox bars clamp to the edge). `video` is the
/// presented picture's rect, which is already UPRIGHT for a rotated host display (the Blt
/// rotates it), and the host injects in its logical — upright — desktop coordinates: no
/// inverse rotation belongs here (it would double-correct, see host.rs). The mouse path
/// (`vidsink-vrect` → the app's hook) maps over the same rect.
pub(crate) fn to_video(client: (f32, f32), size: (f32, f32), video: (f32, f32, f32, f32)) -> (f32, f32) {
	let (fx, fy) = (client.0 / size.0.max(1.0), client.1 / size.1.max(1.0));
	let (x0, y0, x1, y1) = video;
	let nx = (fx - x0) / (x1 - x0).max(1e-6);
	let ny = (fy - y0) / (y1 - y0).max(1e-6);
	(nx.clamp(0.0, 1.0), ny.clamp(0.0, 1.0))
}

pub(crate) fn pen_line(x: f32, y: f32, pressure: f32, tilt: (i32, i32), twist: u32, flags: u8) -> String {
	format!("pen {x:.5} {y:.5} {pressure:.4} {} {} {twist} {flags}", tilt.0, tilt.1)
}

pub(crate) fn touch_line(points: &[(u32, f32, f32, f32)]) -> String {
	let mut s = String::from("touch");
	for (id, x, y, p) in points {
		s.push_str(&format!(" {id},{x:.5},{y:.5},{p:.3}"));
	}
	s
}

fn emit(line: &str) {
	use std::io::Write as _;
	println!("{line}");
	let _ = std::io::stdout().flush();
}

fn video_point(hwnd: HWND, screen: POINT) -> (f32, f32) {
	let mut p = screen;
	let mut rc = RECT::default();
	// SAFETY: plain window queries on our own window.
	unsafe {
		let _ = ScreenToClient(hwnd, &mut p);
		let _ = GetClientRect(hwnd, &mut rc);
	}
	let size = ((rc.right - rc.left) as f32, (rc.bottom - rc.top) as f32);
	to_video((p.x as f32, p.y as f32), size, *VIDEO.lock().unwrap())
}

/// Handle a pointer message while control is engaged. `true` = consumed (reported to the
/// app; no mouse is synthesized from it).
pub(super) fn on_pointer(hwnd: HWND, msg: u32, wp: WPARAM) -> bool {
	if !matches!(msg, WM_POINTERUPDATE | WM_POINTERDOWN | WM_POINTERUP | WM_POINTERLEAVE | WM_POINTERCAPTURECHANGED) {
		return false;
	}
	let id = (wp.0 & 0xffff) as u32;
	let mut ty = POINTER_INPUT_TYPE::default();
	// SAFETY: the id comes from this message.
	if unsafe { GetPointerType(id, &mut ty) }.is_err() {
		return false;
	}
	if ty == PT_PEN && PEN_ON.load(Ordering::SeqCst) {
		let mut info = POINTER_PEN_INFO::default();
		// SAFETY: as above; `info` is a valid out pointer.
		if unsafe { GetPointerPenInfo(id, &mut info) }.is_err() {
			return false;
		}
		let f = info.pointerInfo.pointerFlags;
		let leaving = matches!(msg, WM_POINTERLEAVE | WM_POINTERCAPTURECHANGED);
		let in_range = !leaving && (f.0 & POINTER_FLAG_INRANGE.0) != 0;
		let contact = in_range && (f.0 & POINTER_FLAG_INCONTACT.0) != 0 && msg != WM_POINTERUP;
		let eraser = info.penFlags & (PEN_FLAG_INVERTED | PEN_FLAG_ERASER) != 0;
		let barrel = info.penFlags & PEN_FLAG_BARREL != 0;
		let flags = in_range as u8 | (contact as u8) << 1 | (eraser as u8) << 2 | (barrel as u8) << 3;
		let (x, y) = video_point(hwnd, info.pointerInfo.ptPixelLocation);
		let pressure = if contact { (info.pressure as f32 / 1024.0).clamp(0.0, 1.0) } else { 0.0 };
		emit(&pen_line(x, y, pressure, (info.tiltX, info.tiltY), info.rotation, flags));
		PEN_ACTIVE.store(in_range, Ordering::SeqCst);
		return true;
	}
	if ty == PT_TOUCH && TOUCH_ON.load(Ordering::SeqCst) {
		let mut n: u32 = 16;
		let mut buf = vec![POINTER_TOUCH_INFO::default(); 16];
		// SAFETY: `buf` holds `n` entries; the call writes at most that many.
		if unsafe { GetPointerFrameTouchInfo(id, &mut n, Some(buf.as_mut_ptr())) }.is_err() {
			// More contacts than the buffer (ERROR_INSUFFICIENT_BUFFER sets `n` to the frame's
			// count): once more with room for all of them.
			if (n as usize) <= buf.len() || n > 256 {
				return msg != WM_POINTERCAPTURECHANGED;
			}
			buf = vec![POINTER_TOUCH_INFO::default(); n as usize];
			// SAFETY: as above, `buf` now holds `n` entries.
			if unsafe { GetPointerFrameTouchInfo(id, &mut n, Some(buf.as_mut_ptr())) }.is_err() {
				return msg != WM_POINTERCAPTURECHANGED;
			}
		}
		let frame = buf.first().map(|t| t.pointerInfo.frameId).unwrap_or(0);
		if frame == LAST_FRAME.swap(frame, Ordering::SeqCst) && msg != WM_POINTERCAPTURECHANGED {
			return true; // this frame was already reported for another of its contacts
		}
		let live: Vec<(u32, f32, f32, f32)> = buf[..(n as usize).min(buf.len())]
			.iter()
			.filter(|t| {
				let f = t.pointerInfo.pointerFlags.0;
				f & (POINTER_FLAG_UP.0 | POINTER_FLAG_CANCELED.0) == 0 && f & POINTER_FLAG_INCONTACT.0 != 0
			})
			.map(|t| {
				let (x, y) = video_point(hwnd, t.pointerInfo.ptPixelLocation);
				(t.pointerInfo.pointerId, x, y, (t.pressure as f32 / 1024.0).clamp(0.0, 1.0))
			})
			.collect();
		TOUCH_ACTIVE.store(!live.is_empty(), Ordering::SeqCst);
		emit(&touch_line(&live));
		return true;
	}
	false
}

/// Control let go (disengaged / overlay opened): lift whatever is down on the host.
pub(super) fn release() {
	if PEN_ACTIVE.swap(false, Ordering::SeqCst) {
		emit(&pen_line(0.5, 0.5, 0.0, (0, 0), 0, 0));
	}
	if TOUCH_ACTIVE.swap(false, Ordering::SeqCst) {
		emit(&touch_line(&[]));
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn client_points_map_onto_the_letterboxed_video() {
		// Pillarbox: the video fills the middle half of a 1000x500 window.
		let v = (0.25, 0.0, 0.75, 1.0);
		assert_eq!(to_video((500.0, 250.0), (1000.0, 500.0), v), (0.5, 0.5));
		assert_eq!(to_video((250.0, 0.0), (1000.0, 500.0), v), (0.0, 0.0));
		assert_eq!(to_video((10.0, 499.0), (1000.0, 500.0), v).0, 0.0); // the bar clamps
	}

	#[test]
	fn lines_have_the_documented_shape() {
		assert_eq!(pen_line(0.5, 0.25, 0.75, (30, -5), 90, 0b0011), "pen 0.50000 0.25000 0.7500 30 -5 90 3");
		assert_eq!(touch_line(&[]), "touch");
		assert_eq!(touch_line(&[(3, 0.5, 1.0, 0.0)]), "touch 3,0.50000,1.00000,0.000");
	}
}
