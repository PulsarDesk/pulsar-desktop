//! 1 ms system timer resolution while a session streams from this host (Windows).
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

/// Keeps the 1 ms timer resolution while alive.
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

#[cfg(not(windows))]
mod imp {
	// Linux / macOS timers are already fine-grained.
	pub(super) fn acquire() {}
	pub(super) fn release() {}
}
