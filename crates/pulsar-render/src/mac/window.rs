//! The renderer's window on macOS and how it stays over the app's session tab.
//!
//! Windows and X11 let another process' window be a CHILD of the app window (`--wid`). macOS
//! does not: an `NSView` cannot be put into another process' window, and `addChildWindow:`
//! takes an `NSWindow` of the same process. (Sharing the picture as an IOSurface/remote layer
//! with the app instead would need private CoreAnimation API or a Metal compositor inside the
//! Tauri process.) So the renderer owns a borderless, non-activating `NSPanel` and keeps it
//! exactly over the session tab:
//!   - `--wid` is the app window's `windowNumber`; its frame and on-screen state come from the
//!     window server (`CGWindowListCopyWindowInfo`, which needs no permission for bounds), the
//!     tab's rect inside it from the app (`viewrect`, in POINTS from the window's top-left);
//!   - ordering: the panel is ordered directly above the app window
//!     (`orderWindow:NSWindowAbove relativeTo:`) and re-ordered whenever anything comes between
//!     them (the app being clicked to the front, its own dialogs stay above);
//!   - it hides with the app window (minimized, another Space, closed) and on a 0×0 rect;
//!   - non-activating + never key: the keyboard stays with the app (its webview keeps focus);
//!   - mouse: the panel ignores the mouse (it falls through to the app's session view, which
//!     already handles the click-to-control and the remote pointer on macOS) except while the
//!     overlay is open or the pointer is on the closed-state overlay button.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};

use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{
	NSBackingStoreType, NSColor, NSEvent, NSPanel, NSView, NSWindowCollectionBehavior, NSWindowOrderingMode,
	NSWindowStyleMask,
};
use objc2_core_foundation::CGSize;
use objc2_foundation::{NSPoint, NSRect, NSSize};
use objc2_quartz_core::CAMetalLayer;

use super::ffi::*;

/// Display refreshes seen by the display link (the macOS counterpart of DWM's `cRefresh`).
static REFRESH: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" fn on_refresh(
	_l: CVDisplayLinkRef,
	_now: *const CVTimeStamp,
	_out: *const CVTimeStamp,
	_fi: u64,
	_fo: *mut u64,
	_ctx: *mut c_void,
) -> CVReturn {
	REFRESH.fetch_add(1, Ordering::Relaxed);
	0
}

/// A `CVDisplayLink` that counts the refreshes of the display the panel is on.
pub struct DisplayLink {
	link: CVDisplayLinkRef,
	display: CGDirectDisplayID,
}

impl DisplayLink {
	pub fn new() -> Option<Self> {
		let mut link: CVDisplayLinkRef = std::ptr::null_mut();
		// SAFETY: out pointer; a static callback that only touches an atomic.
		unsafe {
			if CVDisplayLinkCreateWithActiveCGDisplays(&mut link) != 0 || link.is_null() {
				return None;
			}
			CVDisplayLinkSetOutputCallback(link, Some(on_refresh), std::ptr::null_mut());
			if CVDisplayLinkStart(link) != 0 {
				CVDisplayLinkRelease(link);
				return None;
			}
		}
		Some(Self { link, display: 0 })
	}

	/// Follow the panel to another display (a different refresh rate / clock).
	pub fn set_display(&mut self, d: CGDirectDisplayID) {
		if d != 0 && d != self.display {
			self.display = d;
			// SAFETY: our running link.
			unsafe { CVDisplayLinkSetCurrentCGDisplay(self.link, d) };
		}
	}

	/// The refresh counter (moves once per display refresh).
	pub fn refresh(&self) -> u64 {
		REFRESH.load(Ordering::Relaxed)
	}
}

impl Drop for DisplayLink {
	fn drop(&mut self) {
		// SAFETY: our link.
		unsafe {
			CVDisplayLinkStop(self.link);
			CVDisplayLinkRelease(self.link);
		}
	}
}

/// The app window as the window server reports it: CG global bounds (points, top-left origin)
/// and whether it is on screen. None = the window is gone.
pub fn window_info(wid: u32) -> Option<(CGRect, bool)> {
	// SAFETY: CF calls on the returned (+1) array, released by `Cf`.
	unsafe {
		let arr = Cf::owned(CGWindowListCopyWindowInfo(kCGWindowListOptionIncludingWindow, wid))?;
		if CFArrayGetCount(arr.ptr()) < 1 {
			return None;
		}
		let d = CFArrayGetValueAtIndex(arr.ptr(), 0);
		let b = CFDictionaryGetValue(d, kCGWindowBounds);
		let mut r = CGRect::default();
		if b.is_null() || !CGRectMakeWithDictionaryRepresentation(b, &mut r) {
			return None;
		}
		let on = cf_bool(CFDictionaryGetValue(d, kCGWindowIsOnscreen)).unwrap_or(false);
		Some((r, on))
	}
}

/// The window directly above `wid` in the on-screen order (0 = none).
fn window_above(wid: u32) -> u32 {
	// SAFETY: CGWindowListCreate's array holds CGWindowIDs cast to pointers.
	unsafe {
		let Some(arr) = Cf::owned(CGWindowListCreate(kCGWindowListOptionOnScreenAboveWindow, wid)) else { return 0 };
		let n = CFArrayGetCount(arr.ptr());
		if n < 1 {
			return 0;
		}
		// Front-to-back: the last entry is the closest one above `wid`.
		CFArrayGetValueAtIndex(arr.ptr(), n - 1) as usize as u32
	}
}

/// CG global (top-left origin) → Cocoa screen coordinates (bottom-left of the main display).
fn cg_to_cocoa(x: f64, y: f64, w: f64, h: f64) -> NSRect {
	// SAFETY: plain display queries.
	let main_h = unsafe { CGDisplayBounds(CGMainDisplayID()).size.height };
	NSRect::new(NSPoint::new(x, main_h - (y + h)), NSSize::new(w, h))
}

fn display_at(x: f64, y: f64) -> CGDirectDisplayID {
	let mut d: CGDirectDisplayID = 0;
	let mut n = 0u32;
	// SAFETY: one-element out array.
	unsafe { CGGetDisplaysWithPoint(CGPoint { x, y }, 1, &mut d, &mut n) };
	if n == 0 {
		// SAFETY: plain query.
		unsafe { CGMainDisplayID() }
	} else {
		d
	}
}

pub struct Panel {
	pub panel: Retained<NSPanel>,
	pub layer: Retained<CAMetalLayer>,
	number: isize,
	pub parent: u32,
	/// The panel's frame in CG global points `(x, y, w, h)` — also what the mouse is mapped by.
	frame: (f64, f64, f64, f64),
	pub shown: bool,
	ignoring: bool,
	pub scale: f64,
	/// Drawable size in pixels.
	pub size_px: (u32, u32),
	/// Display under the panel (follows moves between screens).
	pub display: CGDirectDisplayID,
	last_order_check: std::time::Instant,
	/// The app window vanished from the window server (closed / its process died).
	pub parent_gone: bool,
	/// The last window-server answer for the app window and when it was asked (the render loop
	/// runs every few ms; the window server is asked at most every `INFO_EVERY`).
	info: Option<(CGRect, bool)>,
	info_at: Option<std::time::Instant>,
	info_key: (u32, Option<(i32, i32, i32, i32)>),
}

const INFO_EVERY: std::time::Duration = std::time::Duration::from_millis(8);

impl Panel {
	pub fn new(mtm: MainThreadMarker, parent: u32) -> Self {
		let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(16.0, 16.0));
		let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
			mtm.alloc(),
			rect,
			NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
			NSBackingStoreType::Buffered,
			false,
		);
		// SAFETY: plain property setters on our own window.
		unsafe { panel.setReleasedWhenClosed(false) };
		panel.setFloatingPanel(false);
		panel.setBecomesKeyOnlyIfNeeded(true);
		panel.setHidesOnDeactivate(false);
		panel.setHasShadow(false);
		panel.setOpaque(true);
		panel.setBackgroundColor(Some(&NSColor::blackColor()));
		panel.setLevel(0); // NSNormalWindowLevel: ordered with the app window, not above everything
		// Joins the Space the app window is on (incl. a full-screen app's Space); it hides
		// itself whenever the app window is not on screen.
		panel.setCollectionBehavior(
			NSWindowCollectionBehavior::CanJoinAllSpaces
				| NSWindowCollectionBehavior::FullScreenAuxiliary
				| NSWindowCollectionBehavior::IgnoresCycle,
		);
		panel.setAcceptsMouseMovedEvents(true);
		panel.setIgnoresMouseEvents(true);
		let layer = CAMetalLayer::new();
		if let Some(view) = panel.contentView() {
			let view: Retained<NSView> = view;
			view.setLayer(Some(&layer));
			view.setWantsLayer(true);
		}
		let number = panel.windowNumber();
		let scale = panel.backingScaleFactor();
		Self {
			panel,
			layer,
			number,
			parent,
			frame: (0.0, 0.0, 0.0, 0.0),
			shown: false,
			ignoring: true,
			scale,
			size_px: (1, 1),
			display: 0,
			last_order_check: std::time::Instant::now(),
			parent_gone: false,
			info: None,
			info_at: None,
			info_key: (0, None),
		}
	}

	pub fn number(&self) -> isize {
		self.number
	}

	/// Place the panel: over `rect` (points from the app window's top-left; None = the whole
	/// window; 0×0 = hidden) when `visible`. Returns true when the drawable size changed.
	pub fn place(&mut self, rect: Option<(i32, i32, i32, i32)>, visible: bool) -> bool {
		let key = (self.parent, rect);
		if key != self.info_key || self.info_at.is_none_or(|t| t.elapsed() >= INFO_EVERY) {
			self.info = window_info(self.parent);
			self.info_at = Some(std::time::Instant::now());
			self.info_key = key;
		}
		let info = self.info;
		self.parent_gone = info.is_none();
		let target = match (info, rect) {
			(Some((_, false)), _) | (None, _) => None,
			(Some(_), Some((_, _, w, h))) if w <= 0 || h <= 0 => None,
			(Some((b, true)), Some((x, y, w, h))) => {
				Some((b.origin.x + x as f64, b.origin.y + y as f64, w as f64, h as f64))
			}
			(Some((b, true)), None) => Some((b.origin.x, b.origin.y, b.size.width, b.size.height)),
		};
		let Some(f) = target.filter(|_| visible) else {
			if self.shown {
				self.panel.orderOut(None);
				self.shown = false;
			}
			return false;
		};
		let mut resized = false;
		if f != self.frame {
			self.frame = f;
			self.panel.setFrame_display(cg_to_cocoa(f.0, f.1, f.2, f.3), false);
		}
		let scale = self.panel.backingScaleFactor();
		let px = ((f.2 * scale).round().max(1.0) as u32, (f.3 * scale).round().max(1.0) as u32);
		if scale != self.scale || px != self.size_px {
			self.scale = scale;
			self.size_px = px;
			self.layer.setContentsScale(scale);
			self.layer.setDrawableSize(CGSize { width: px.0 as f64, height: px.1 as f64 });
			resized = true;
		}
		self.display = display_at(f.0 + f.2 / 2.0, f.1 + f.3 / 2.0);
		if !self.shown {
			self.shown = true;
			self.panel.orderWindow_relativeTo(NSWindowOrderingMode::Above, self.parent as isize);
		} else if self.last_order_check.elapsed() >= std::time::Duration::from_millis(30) {
			// Something came between the app window and us (the app was clicked to the front,
			// a window of another app): back directly above it.
			self.last_order_check = std::time::Instant::now();
			if window_above(self.parent) != self.number as u32 {
				self.panel.orderWindow_relativeTo(NSWindowOrderingMode::Above, self.parent as isize);
			}
		}
		resized
	}

	/// The pointer over the panel, in panel points from its top-left (None when outside).
	pub fn pointer(&self) -> Option<(f64, f64)> {
		let m = NSEvent::mouseLocation();
		// Cocoa → CG global.
		// SAFETY: plain display query.
		let main_h = unsafe { CGDisplayBounds(CGMainDisplayID()).size.height };
		let (x, y) = (m.x - self.frame.0, (main_h - m.y) - self.frame.1);
		(x >= 0.0 && y >= 0.0 && x < self.frame.2 && y < self.frame.3).then_some((x, y))
	}

	/// The panel's size in points.
	pub fn size_pt(&self) -> (f64, f64) {
		(self.frame.2, self.frame.3)
	}

	/// Take the mouse (overlay open / on the overlay button) or let it fall through to the app.
	pub fn set_take_mouse(&mut self, take: bool) {
		if take == self.ignoring {
			self.ignoring = !take;
			self.panel.setIgnoresMouseEvents(!take);
		}
	}
}
