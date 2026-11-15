//! Fine-grained timers while a session streams from this host (Windows: 1 ms resolution;
//! macOS: no App Nap; Linux: nothing to do).
//!
//! WHY: the video forwarder's wire pacer sleeps to the next send slot (tokio timers). At the
//! default 15.6 ms Windows timer resolution those wakes land late in coarse steps, so datagrams
//! waited 40-100 ms in the pacer instead of its 20 ms bound. That varying wait reaches the client
//! as one-way delay jitter, which its delay-based controller reads as a filling network queue:
//! it cut a clean 11.5 Mbit/s stream to 3.5 Mbit/s in windows without any loss (2026-11-15,
//! Snapdragon host on home Wi-Fi → a friend). Sunshine raises the resolution the same way while
//! streaming. Per process since Windows 10 2004; refcounted, released with the last session.
//! Windows 11 may ignore the request while the window is hidden / minimised (a host in the
//! tray): the session's `pulsar_prio::session()` guard opts the process out of that
//! (PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, 22H2+) together with EcoQoS.
//!
//! macOS: the timers are fine-grained, but App Nap coalesces and delays the timers of an app
//! whose windows are hidden or occluded (a host that streams with Pulsar minimized — the usual
//! case) by up to seconds, which would stall the pacer exactly like the coarse Windows timer.
//! While a session streams the process holds an `NSProcessInfo` activity
//! (`NSActivityUserInitiatedAllowingIdleSystemSleep | NSActivityLatencyCritical`), as
//! latency-sensitive media apps do. Untested on macOS hardware.
//!
//! Linux: hrtimers, a 50 µs default timer slack and tokio's 1 ms wheel — the pacer's sleeps
//! land on time; there is no per-process switch to make (and no App Nap equivalent).

/// Keeps the fine-grained timers while alive.
pub(crate) struct Guard(());

impl Drop for Guard {
	fn drop(&mut self) {
		imp::release();
	}
}

pub(crate) fn acquire() -> Guard {
	imp::acquire();
	Guard(())
}

#[cfg(windows)]
mod imp {
	use std::sync::Mutex;

	#[link(name = "winmm")]
	extern "system" {
		fn timeBeginPeriod(ms: u32) -> u32;
		fn timeEndPeriod(ms: u32) -> u32;
	}

	static HELD: Mutex<usize> = Mutex::new(0);

	pub(super) fn acquire() {
		let mut n = HELD.lock().unwrap();
		if *n == 0 {
			// SAFETY: plain winmm call, balanced by `release`.
			let r = unsafe { timeBeginPeriod(1) };
			tracing::info!(ok = r == 0, "timer: 1 ms resolution for the streaming session");
		}
		*n += 1;
	}

	pub(super) fn release() {
		let mut n = HELD.lock().unwrap();
		if *n == 0 {
			return;
		}
		*n -= 1;
		if *n == 0 {
			// SAFETY: matches the timeBeginPeriod(1) of the first `acquire`.
			unsafe { timeEndPeriod(1) };
			tracing::info!("timer: default resolution again (no session streams)");
		}
	}
}

#[cfg(target_os = "macos")]
mod imp {
	use std::sync::Mutex;

	use objc2::rc::Retained;
	use objc2::runtime::AnyObject;
	use objc2::{class, msg_send};

	/// `NSActivityUserInitiatedAllowingIdleSystemSleep` (0x00FFFFFF & ~(1 << 20)) |
	/// `NSActivityLatencyCritical` (0xFF00000000).
	const OPTIONS: u64 = (0x00FF_FFFF & !(1u64 << 20)) | 0xFF_0000_0000;

	/// The activity token (an opaque NSObject only `endActivity:` uses).
	struct Token(Retained<AnyObject>);
	// SAFETY: the token is immutable and only handed back to NSProcessInfo, whose activity
	// calls are thread-safe.
	unsafe impl Send for Token {}

	static HELD: Mutex<(usize, Option<Token>)> = Mutex::new((0, None));

	fn process_info() -> Option<Retained<AnyObject>> {
		// SAFETY: `+[NSProcessInfo processInfo]` returns the shared instance (any thread).
		unsafe { msg_send![class!(NSProcessInfo), processInfo] }
	}

	pub(super) fn acquire() {
		let mut st = HELD.lock().unwrap();
		st.0 += 1;
		if st.1.is_some() {
			return;
		}
		let reason = objc2_foundation::NSString::from_str("Pulsar is streaming");
		let token: Option<Retained<AnyObject>> = process_info().and_then(|pi| {
			// SAFETY: `-beginActivityWithOptions:reason:` (NSActivityOptions = uint64_t,
			// NSString *) returns a retained-by-us activity token.
			unsafe { msg_send![&*pi, beginActivityWithOptions: OPTIONS, reason: &*reason] }
		});
		tracing::info!(ok = token.is_some(), "timer: App Nap off for the streaming session");
		st.1 = token.map(Token);
	}

	pub(super) fn release() {
		let mut st = HELD.lock().unwrap();
		if st.0 == 0 {
			return;
		}
		st.0 -= 1;
		if st.0 == 0 {
			if let (Some(Token(t)), Some(pi)) = (st.1.take(), process_info()) {
				// SAFETY: ends the activity begun in `acquire`, with its own token.
				unsafe {
					let _: () = msg_send![&*pi, endActivity: &*t];
				}
				tracing::info!("timer: App Nap allowed again (no session streams)");
			}
		}
	}
}

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
	// Linux timers are already fine-grained (see the module doc).
	pub(super) fn acquire() {}
	pub(super) fn release() {}
}
