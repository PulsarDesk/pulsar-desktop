//! macOS: NSWorkspace's sleep / wake notifications (untested on macOS hardware).
//!
//! `NSWorkspaceWillSleepNotification` / `NSWorkspaceDidWakeNotification` = the system suspends
//! / resumes; `NSWorkspaceScreensDidSleepNotification` / `…ScreensDidWakeNotification` = the
//! displays turn off / on (display sleep without a system sleep, the common case while a
//! session plays audio). NSWorkspace posts them on the main thread; the observers are added
//! with no queue, so the block runs right there, on the main thread's run loop (the app's
//! event loop — no thread of our own is needed).
//!
//! `com.apple.screenIsLocked` / `com.apple.screenIsUnlocked` = the session locks / unlocks
//! (lock screen, screen-saver lock, fast user switch). loginwindow posts them as DISTRIBUTED
//! notifications (`NSDistributedNotificationCenter`); with no queue they too are delivered on
//! the main thread's run loop. Undocumented but stable since 10.x and what every app uses
//! (no public lock API): if a future macOS drops them, a locked client just falls back to the
//! display-off edge.
//!
//! Self-contained (only objc2 crates), so this file can be type-checked for
//! aarch64-apple-darwin on its own even where the whole app cannot be.

use std::ptr::NonNull;

use block2::RcBlock;
use objc2_app_kit::{
	NSWorkspace, NSWorkspaceDidWakeNotification, NSWorkspaceScreensDidSleepNotification,
	NSWorkspaceScreensDidWakeNotification, NSWorkspaceWillSleepNotification,
};
use objc2_foundation::{NSDistributedNotificationCenter, NSNotification, NSString};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Event {
	WillSleep,
	DidWake,
	ScreensDidSleep,
	ScreensDidWake,
	ScreenLocked,
	ScreenUnlocked,
}

/// Observe the notifications for the life of the process; `on` runs on the main thread.
pub(super) fn watch(on: impl Fn(Event) + Clone + 'static) {
	let center = NSWorkspace::sharedWorkspace().notificationCenter();
	// SAFETY: the notification names are AppKit's immutable global NSString constants.
	let names = unsafe {
		[
			(NSWorkspaceWillSleepNotification, Event::WillSleep),
			(NSWorkspaceDidWakeNotification, Event::DidWake),
			(NSWorkspaceScreensDidSleepNotification, Event::ScreensDidSleep),
			(NSWorkspaceScreensDidWakeNotification, Event::ScreensDidWake),
		]
	};
	for (name, ev) in names {
		let on = on.clone();
		let block = RcBlock::new(move |_note: NonNull<NSNotification>| on(ev));
		// SAFETY: no object filter, no queue (delivered synchronously on the posting thread —
		// the main thread for NSWorkspace); the block is copied by the center and stays valid.
		let observer = unsafe { center.addObserverForName_object_queue_usingBlock(Some(name), None, None, &block) };
		// The observation lasts as long as the process: the center keeps the block; the token
		// is only needed to remove it again, which never happens.
		std::mem::forget(observer);
	}
	let distributed = NSDistributedNotificationCenter::defaultCenter();
	for (name, ev) in [("com.apple.screenIsLocked", Event::ScreenLocked), ("com.apple.screenIsUnlocked", Event::ScreenUnlocked)] {
		let name = NSString::from_str(name);
		let on = on.clone();
		let block = RcBlock::new(move |_note: NonNull<NSNotification>| on(ev));
		// SAFETY: as above — no object filter, no queue (the main thread's run loop); the
		// center copies the block.
		let observer =
			unsafe { distributed.addObserverForName_object_queue_usingBlock(Some(&*name), None, None, &block) };
		std::mem::forget(observer);
	}
	tracing::info!("power: NSWorkspace sleep/wake + screens sleep/wake + screen lock notifications active");
}
