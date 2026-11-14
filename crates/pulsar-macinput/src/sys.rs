//! Hand-written FFI for the few CoreFoundation / CoreGraphics / ApplicationServices / objc
//! calls the macOS capture makes. Types follow the C headers on 64-bit Apple targets:
//! `CFIndex` = `isize`, `CFOptionFlags` = `usize`, `Boolean` = `u8`, `boolean_t` = `i32`,
//! the `CG*` enums (`CGEventType`, `CGEventField`, tap location/placement/options,
//! `CGWindowListOption`, `CGWindowID`, `CGMouseButton`) = `u32`, `CGEventSourceStateID` = `i32`,
//! `CGEventFlags` / `CGEventMask` = `u64`, `CGError` = `i32`.

#![allow(non_upper_case_globals, non_snake_case, dead_code)]

use std::ffi::{c_char, c_void};

pub type CFTypeRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFArrayRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CFMachPortRef = *mut c_void;
pub type CFRunLoopRef = *mut c_void;
pub type CFRunLoopSourceRef = *mut c_void;
pub type CFRunLoopTimerRef = *mut c_void;
pub type CGEventRef = *mut c_void;
pub type CGEventTapProxy = *mut c_void;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CGPoint {
	pub x: f64,
	pub y: f64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CGSize {
	pub width: f64,
	pub height: f64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CGRect {
	pub origin: CGPoint,
	pub size: CGSize,
}

/// Only the address is used (passed to `CFDictionaryCreate`): six words, the size of
/// `CFDictionaryKeyCallBacks` (version + five callbacks).
#[repr(C)]
pub struct CFDictionaryKeyCallBacks([usize; 6]);
/// Five words: `CFDictionaryValueCallBacks` (version + four callbacks).
#[repr(C)]
pub struct CFDictionaryValueCallBacks([usize; 5]);

pub type CGEventTapCallBack =
	unsafe extern "C" fn(proxy: CGEventTapProxy, ty: u32, event: CGEventRef, user_info: *mut c_void) -> CGEventRef;
pub type CFRunLoopTimerCallBack = unsafe extern "C" fn(timer: CFRunLoopTimerRef, info: *mut c_void);

// CGEventTapLocation / Placement / Options.
pub const kCGHIDEventTap: u32 = 0;
pub const kCGSessionEventTap: u32 = 1;
pub const kCGHeadInsertEventTap: u32 = 0;
pub const kCGEventTapOptionDefault: u32 = 0;

// CGEventType.
pub const kCGEventLeftMouseDown: u32 = 1;
pub const kCGEventLeftMouseUp: u32 = 2;
pub const kCGEventRightMouseDown: u32 = 3;
pub const kCGEventRightMouseUp: u32 = 4;
pub const kCGEventMouseMoved: u32 = 5;
pub const kCGEventLeftMouseDragged: u32 = 6;
pub const kCGEventRightMouseDragged: u32 = 7;
pub const kCGEventKeyDown: u32 = 10;
pub const kCGEventKeyUp: u32 = 11;
pub const kCGEventFlagsChanged: u32 = 12;
pub const kCGEventScrollWheel: u32 = 22;
pub const kCGEventOtherMouseDown: u32 = 25;
pub const kCGEventOtherMouseUp: u32 = 26;
pub const kCGEventOtherMouseDragged: u32 = 27;
pub const kCGEventTapDisabledByTimeout: u32 = 0xFFFF_FFFE;
pub const kCGEventTapDisabledByUserInput: u32 = 0xFFFF_FFFF;

// CGEventField.
pub const kCGMouseEventButtonNumber: u32 = 3;
pub const kCGMouseEventDeltaX: u32 = 4;
pub const kCGMouseEventDeltaY: u32 = 5;
pub const kCGKeyboardEventAutorepeat: u32 = 8;
pub const kCGKeyboardEventKeycode: u32 = 9;
pub const kCGScrollWheelEventDeltaAxis1: u32 = 11;
pub const kCGScrollWheelEventDeltaAxis2: u32 = 12;
pub const kCGEventSourceUnixProcessID: u32 = 41;
pub const kCGScrollWheelEventIsContinuous: u32 = 88;
pub const kCGScrollWheelEventPointDeltaAxis1: u32 = 96;
pub const kCGScrollWheelEventPointDeltaAxis2: u32 = 97;

// CGWindowListOption.
pub const kCGWindowListOptionOnScreenOnly: u32 = 1 << 0;
pub const kCGWindowListExcludeDesktopElements: u32 = 1 << 4;
pub const kCGNullWindowID: u32 = 0;

// CGEventSourceStateID.
pub const kCGEventSourceStateCombinedSessionState: i32 = 0;

// CFNumberType.
pub const kCFNumberSInt64Type: isize = 4;
pub const kCFNumberFloat64Type: isize = 6;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
	pub static kCFRunLoopCommonModes: CFStringRef;
	pub static kCFBooleanTrue: CFTypeRef;
	pub static kCFTypeDictionaryKeyCallBacks: CFDictionaryKeyCallBacks;
	pub static kCFTypeDictionaryValueCallBacks: CFDictionaryValueCallBacks;

	pub fn CFRelease(cf: CFTypeRef);
	pub fn CFMachPortCreateRunLoopSource(allocator: CFTypeRef, port: CFMachPortRef, order: isize) -> CFRunLoopSourceRef;
	pub fn CFRunLoopGetCurrent() -> CFRunLoopRef;
	pub fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
	pub fn CFRunLoopAddTimer(rl: CFRunLoopRef, timer: CFRunLoopTimerRef, mode: CFStringRef);
	pub fn CFRunLoopRun();
	pub fn CFRunLoopTimerCreate(
		allocator: CFTypeRef,
		fire_date: f64,
		interval: f64,
		flags: usize,
		order: isize,
		callout: CFRunLoopTimerCallBack,
		context: *mut c_void,
	) -> CFRunLoopTimerRef;
	pub fn CFAbsoluteTimeGetCurrent() -> f64;
	pub fn CFArrayGetCount(array: CFArrayRef) -> isize;
	pub fn CFArrayGetValueAtIndex(array: CFArrayRef, idx: isize) -> *const c_void;
	pub fn CFDictionaryGetValue(dict: CFDictionaryRef, key: *const c_void) -> *const c_void;
	pub fn CFDictionaryCreate(
		allocator: CFTypeRef,
		keys: *const *const c_void,
		values: *const *const c_void,
		num_values: isize,
		key_callbacks: *const CFDictionaryKeyCallBacks,
		value_callbacks: *const CFDictionaryValueCallBacks,
	) -> CFDictionaryRef;
	pub fn CFNumberGetValue(number: CFTypeRef, the_type: isize, value_ptr: *mut c_void) -> u8;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
	pub static kCGWindowNumber: CFStringRef;
	pub static kCGWindowOwnerPID: CFStringRef;
	pub static kCGWindowLayer: CFStringRef;
	pub static kCGWindowBounds: CFStringRef;
	pub static kCGWindowAlpha: CFStringRef;

	pub fn CGEventTapCreate(
		tap: u32,
		place: u32,
		options: u32,
		events_of_interest: u64,
		callback: CGEventTapCallBack,
		user_info: *mut c_void,
	) -> CFMachPortRef;
	pub fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
	pub fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
	pub fn CGEventGetDoubleValueField(event: CGEventRef, field: u32) -> f64;
	pub fn CGEventGetFlags(event: CGEventRef) -> u64;
	pub fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
	pub fn CGEventSetLocation(event: CGEventRef, location: CGPoint);
	pub fn CGEventCreate(source: *mut c_void) -> CGEventRef;
	pub fn CGWarpMouseCursorPosition(new_cursor_position: CGPoint) -> i32;
	pub fn CGAssociateMouseAndMouseCursorPosition(connected: i32) -> i32;
	pub fn CGSetLocalEventsSuppressionInterval(seconds: f64) -> i32;
	pub fn CGEventSourceButtonState(state_id: i32, button: u32) -> bool;
	pub fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: u32) -> CFArrayRef;
	pub fn CGRectMakeWithDictionaryRepresentation(dict: CFDictionaryRef, rect: *mut CGRect) -> bool;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
	pub static kAXTrustedCheckOptionPrompt: CFStringRef;
	pub fn AXIsProcessTrusted() -> u8;
	pub fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> u8;
}

// NSWorkspace / NSWindow live in AppKit; the class lookups below need it linked (the Tauri
// app links it anyway).
#[link(name = "AppKit", kind = "framework")]
extern "C" {}

#[link(name = "objc")]
extern "C" {
	pub fn objc_getClass(name: *const c_char) -> *mut c_void;
	pub fn sel_registerName(name: *const c_char) -> *mut c_void;
	pub fn objc_msgSend();
	pub fn objc_autoreleasePoolPush() -> *mut c_void;
	pub fn objc_autoreleasePoolPop(pool: *mut c_void);
}

/// `[receiver selector]` returning an object pointer.
///
/// # Safety
/// `receiver` must be an object (or class) that answers `sel` with no arguments and an
/// object (or nil) result.
pub unsafe fn msg_id(receiver: *mut c_void, sel: &std::ffi::CStr) -> *mut c_void {
	if receiver.is_null() {
		return std::ptr::null_mut();
	}
	let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
		std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
	f(receiver, sel_registerName(sel.as_ptr()))
}

/// `[receiver selector]` returning an `NSInteger` / `pid_t` sized integer.
///
/// # Safety
/// As [`msg_id`], with an integer result of type `R` (`isize` for `NSInteger`, `i32` for
/// `pid_t`).
pub unsafe fn msg_int<R: Copy + Default>(receiver: *mut c_void, sel: &std::ffi::CStr) -> R {
	if receiver.is_null() {
		return R::default();
	}
	let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> R =
		std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
	f(receiver, sel_registerName(sel.as_ptr()))
}
