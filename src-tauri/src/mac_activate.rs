//! macOS: bring Pulsar to the front from the background (pad wake, a reverse-direction
//! request). `NSWindow::makeKeyAndOrderFront` alone only reorders our windows inside our own
//! app — while another app is active Pulsar stays behind it. The app itself must be activated:
//! unhide it (the tray "hide" path), then activate ignoring other apps — the classic
//! `-[NSApp activateIgnoringOtherApps:]` plus `NSRunningApplication activateWithOptions:` —
//! and on macOS 14+ also the cooperative `-[NSApp activate]`. Finally order the window in
//! (deminiaturize if docked) and `orderFrontRegardless`, which raises it even if the system
//! declines to hand over activation (macOS 14+ may refuse a background app's request; the
//! window then still comes up above the others, just not key).
//!
//! Kept free of Tauri types and dependency-light so it type-checks for aarch64-apple-darwin
//! on its own (src-tauri as a whole cannot be checked for macOS off a Mac).
//! Untested on macOS hardware (written 2026-11-14).
#![cfg(target_os = "macos")]

use objc2::runtime::NSObjectProtocol;
use objc2::{sel, MainThreadMarker};
use objc2_app_kit::{NSApplication, NSApplicationActivationOptions, NSRunningApplication, NSWindow};

/// Activate the app and raise `ns_window` (an `NSWindow*`, may be null). Main thread only —
/// a no-op when called elsewhere.
pub(crate) fn activate(ns_window: *mut std::ffi::c_void) {
	let Some(mtm) = MainThreadMarker::new() else {
		return;
	};
	let app = NSApplication::sharedApplication(mtm);
	app.unhide(None);
	#[allow(deprecated)]
	app.activateIgnoringOtherApps(true);
	#[allow(deprecated)]
	let _ = NSRunningApplication::currentApplication().activateWithOptions(
		NSApplicationActivationOptions::ActivateIgnoringOtherApps | NSApplicationActivationOptions::ActivateAllWindows,
	);
	// macOS 14+: the cooperative activation API (absent before — check, or it would throw).
	if app.respondsToSelector(sel!(activate)) {
		app.activate();
	}
	if ns_window.is_null() {
		return;
	}
	// SAFETY: the caller passes the live NSWindow of a Tauri window, on the main thread.
	let win: &NSWindow = unsafe { &*(ns_window as *const NSWindow) };
	if win.isMiniaturized() {
		win.deminiaturize(None);
	}
	win.makeKeyAndOrderFront(None);
	win.orderFrontRegardless();
}
