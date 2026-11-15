//! Scheduling boosts for the latency-critical work of a streaming session, on every desktop OS.
//!
//! WHY: a host that runs a game, or a client on a loaded / hybrid / battery machine, put Pulsar's
//! capture, encode, pacer, forwarder, receive, decode and present work at the default priority
//! next to everything else. Measured on a Windows host before the process went to HIGH: a game
//! loading or quitting starved the forwarder and the pacer for 370 ms (a hole in the video, then
//! 17-19 frames at once, the bitrate cut to a third). Linux and macOS hosts and every client had
//! no boost at all, and Windows 11 may put a hidden / tray process (the normal state of a host) on
//! EcoQoS efficiency cores and ignore its 1 ms timer request (hidden-gaps 1.13 / 1.14).
//!
//! Three pieces, all reverted when the session (or the thread) ends:
//! - [`boost_current_thread`]: a dedicated media thread (capture, RTP send / receive, decode,
//!   present) raises ITSELF for its lifetime; RAII, restored on drop.
//! - [`session`]: refcounted, held while any session runs in this process. Process-wide opt-outs
//!   (Windows EcoQoS + ignored timer requests, macOS App Nap / timer coalescing) and the switch
//!   for the async workers below.
//! - [`on_worker_unpark`]: a hook for the app's tokio runtime. The forwarder, the wire pacer, the
//!   session read loop and the client's receive loop are tokio tasks on ordinary workers; while a
//!   session runs the workers run boosted too.
//!
//! Per OS (never a realtime class that can starve the system):
//!
//! | role      | Windows                               | macOS QoS        | Linux nice |
//! |-----------|---------------------------------------|------------------|------------|
//! | `Capture` | TIME_CRITICAL + MMCSS "Capture"       | USER_INTERACTIVE | -5         |
//! | `Present` | HIGHEST + MMCSS "Games"               | USER_INTERACTIVE | -10        |
//! | `Network` | HIGHEST                               | USER_INTERACTIVE | -10        |
//! | `Worker`  | ABOVE_NORMAL                          | USER_INITIATED   | -5         |
//!
//! - Windows: thread priorities stay in the dynamic range (TIME_CRITICAL is 15 in a non-realtime
//!   class, never REALTIME_PRIORITY_CLASS). MMCSS lifts a registered thread for at most
//!   `SystemResponsiveness` of the CPU (80 % by default) and drops it below normal threads for
//!   the rest, so it cannot starve the system. `PULSAR_MMCSS=0` skips the MMCSS join. The
//!   process opt-out is `SetProcessInformation(ProcessPowerThrottling)` with
//!   EXECUTION_SPEED | IGNORE_TIMER_RESOLUTION set in the control mask and cleared in the state
//!   mask: no EcoQoS, and the 1 ms timer request is honoured while the window is hidden.
//! - macOS: QoS classes are not realtime (the scheduler keeps fairness between them). The
//!   process holds an `NSProcessInfo` activity (UserInitiatedAllowingIdleSystemSleep |
//!   LatencyCritical) so App Nap never throttles a host sitting in the menu bar. Display / system
//!   sleep stays with the existing keep-awake code.
//! - Linux: nice values only (CFS weight), never SCHED_FIFO / SCHED_RR. `setpriority` on the
//!   thread when the user may (CAP_SYS_NICE or an RLIMIT_NICE from limits.conf), else rtkit's
//!   `MakeThreadHighPriority` over the system bus (what PipeWire / PulseAudio use; rtkit clamps
//!   at nice -15 and rate-limits per user, so only a bounded number of worker requests go to it).
//!   Raising the nice value back is always allowed, so every revert is a plain `setpriority`.
//!   Threads a boosted thread creates inherit its nice value (libav / GStreamer / dav1d threads
//!   started from a boosted thread run boosted too). Capture is -5, not -10, for that reason:
//!   software x264 threads inherit it and must not crowd out the desktop / the game.
//! - Android / iOS / others: no-ops (the Android app boosts its decoder threads in Kotlin).
//!
//! `PULSAR_PRIO=0` turns every boost of this crate off (A/B against the default).

use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

/// What a boosted thread does; picks the per-OS level (see the crate docs table).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
	/// Host capture / encode loop.
	Capture,
	/// Client decode / present loop.
	Present,
	/// RTP send / receive, forwarders: little CPU, must run the moment data is there.
	Network,
	/// An async-runtime worker while a session runs (see [`on_worker_unpark`]).
	Worker,
}

impl Role {
	/// Linux nice level for this role.
	pub const fn nice(self) -> i32 {
		match self {
			Role::Capture | Role::Worker => -5,
			Role::Present | Role::Network => -10,
		}
	}

	/// macOS QoS class (`qos_class_t`) for this role.
	pub const fn qos(self) -> u32 {
		match self {
			Role::Worker => QOS_USER_INITIATED,
			_ => QOS_USER_INTERACTIVE,
		}
	}

	/// Windows thread priority and MMCSS task for this role.
	pub const fn windows(self) -> (i32, Option<&'static str>) {
		match self {
			Role::Capture => (WIN_TIME_CRITICAL, Some("Capture")),
			Role::Present => (WIN_HIGHEST, Some("Games")),
			Role::Network => (WIN_HIGHEST, None),
			Role::Worker => (WIN_ABOVE_NORMAL, None),
		}
	}
}

// Documented values, kept here so the table above is testable on every OS.
const QOS_USER_INTERACTIVE: u32 = 0x21;
const QOS_USER_INITIATED: u32 = 0x19;
#[allow(dead_code)]
const QOS_DEFAULT: u32 = 0x15;
const WIN_ABOVE_NORMAL: i32 = 1;
const WIN_HIGHEST: i32 = 2;
const WIN_TIME_CRITICAL: i32 = 15;

/// `PULSAR_PRIO=0` disables every boost (read once).
fn disabled() -> bool {
	static OFF: OnceLock<bool> = OnceLock::new();
	*OFF.get_or_init(|| std::env::var("PULSAR_PRIO").is_ok_and(|v| v == "0"))
}

/// A raised current-thread priority; restored when dropped (on the same thread: not `Send`).
#[must_use = "the boost ends when this guard is dropped"]
pub struct ThreadBoost {
	inner: Option<imp::Thread>,
	_not_send: PhantomData<*const ()>,
}

impl ThreadBoost {
	/// How the boost was applied ("setpriority", "rtkit", "qos", "priority+mmcss", ...), or
	/// "none" when nothing changed (already at / above the level, not permitted, disabled).
	pub fn how(&self) -> &'static str {
		self.inner.as_ref().map_or("none", |t| t.how())
	}
}

/// Raise the calling thread for `role` until the returned guard drops. Best effort: a refused
/// call leaves the thread as it was. Never lowers a thread that already runs higher.
pub fn boost_current_thread(role: Role) -> ThreadBoost {
	let inner = if disabled() { None } else { imp::Thread::boost(role) };
	ThreadBoost { inner, _not_send: PhantomData }
}

// ---- session (process-wide) ------------------------------------------------------------------

static SESSIONS: Mutex<usize> = Mutex::new(0);
/// True while at least one session runs; read by the worker hook.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Bumped on every on/off flip so each worker re-applies its state once.
static EPOCH: AtomicU32 = AtomicU32::new(0);

/// Held while a session runs in this process; the last one dropped reverts the process-wide
/// state. Cheap to take more than once (refcounted).
#[must_use = "the session boost ends when this guard is dropped"]
pub struct Session(());

impl Drop for Session {
	fn drop(&mut self) {
		let mut n = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
		*n = n.saturating_sub(1);
		if *n == 0 && !disabled() {
			ACTIVE.store(false, Ordering::SeqCst);
			EPOCH.fetch_add(1, Ordering::SeqCst);
			imp::process(false);
		}
	}
}

/// Start (or join) the process-wide session boost. Returns what changed for logging.
pub fn session() -> Session {
	let mut n = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
	*n += 1;
	if *n == 1 && !disabled() {
		ACTIVE.store(true, Ordering::SeqCst);
		EPOCH.fetch_add(1, Ordering::SeqCst);
		imp::process(true);
	}
	Session(())
}

/// Whether a [`session`] guard is alive in this process.
pub fn session_active() -> bool {
	ACTIVE.load(Ordering::SeqCst)
}

/// What the process-wide session boost did last (for one log line), e.g.
/// "ecoqos-off+timer" / "activity" / "workers 8/8".
pub fn session_report() -> String {
	imp::report()
}

// ---- async workers ---------------------------------------------------------------------------

/// Hook for `tokio::runtime::Builder::on_thread_unpark`: tokio calls it on a WORKER thread each
/// time it wakes (blocking-pool threads never call it). While a [`session`] runs the worker
/// raises itself to [`Role::Worker`]; after the session it drops back. One atomic load per wake
/// when nothing changed. Linux registers the worker's thread id instead and the session switch
/// renices the registered workers from a helper thread (rtkit is a blocking D-Bus call, not
/// something to run inside the runtime).
pub fn on_worker_unpark() {
	if disabled() {
		return;
	}
	imp::worker_unpark(EPOCH.load(Ordering::Relaxed));
}

// ---- children / misc -------------------------------------------------------------------------

/// Raise a media child process (the ffmpeg encoder / audio capture) for the session.
/// Windows: ABOVE_NORMAL_PRIORITY_CLASS + the same EcoQoS opt-out. ABOVE_NORMAL, not HIGH: it
/// already wins over a NORMAL-class game, and a software x264 child at HIGH on every core would
/// stall the host's own desktop. Unix: `setpriority(child, -5)` when the user may (Linux: only
/// the child's main thread and the threads it starts later; macOS needs root, so it is usually
/// refused). Returns whether anything was raised.
pub fn boost_child(child: &std::process::Child) -> bool {
	if disabled() {
		return false;
	}
	imp::boost_child(child)
}

/// Windows: `DwmEnableMMCSS(TRUE)` — the compositor schedules its own work with MMCSS while
/// this process presents (Moonlight's d3d11va renderer does the same). No-op elsewhere.
pub fn enable_dwm_mmcss() -> bool {
	if disabled() {
		return false;
	}
	imp::enable_dwm_mmcss()
}

// ==============================================================================================
// Windows
// ==============================================================================================
#[cfg(windows)]
mod imp {
	use super::Role;
	use std::cell::RefCell;
	use std::sync::Mutex;
	use windows_sys::Win32::Foundation::HANDLE;
	use windows_sys::Win32::System::Threading::{
		AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, GetCurrentProcess,
		GetCurrentThread, GetThreadPriority, ProcessPowerThrottling, SetPriorityClass,
		SetProcessInformation, SetThreadPriority, ABOVE_NORMAL_PRIORITY_CLASS,
		PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
		PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
	};

	/// GetThreadPriority's failure sentinel (THREAD_PRIORITY_ERROR_RETURN).
	const ERR_RETURN: i32 = 0x7fff_ffff;

	pub(super) struct Thread {
		/// Priority to restore, when we changed it.
		prev: Option<i32>,
		/// MMCSS registration (null = none).
		mmcss: HANDLE,
		how: &'static str,
	}

	impl Thread {
		pub(super) fn boost(role: Role) -> Option<Self> {
			let (prio, task) = role.windows();
			// SAFETY: the current-thread pseudo handle; plain Win32 calls.
			unsafe {
				let me = GetCurrentThread();
				let cur = GetThreadPriority(me);
				let prev = (cur != ERR_RETURN && cur < prio && SetThreadPriority(me, prio) != 0).then_some(cur);
				let mmcss = match task {
					Some(t) if std::env::var("PULSAR_MMCSS").map_or(true, |v| v != "0") => {
						let name: Vec<u16> = t.encode_utf16().chain(std::iter::once(0)).collect();
						let mut idx = 0u32;
						AvSetMmThreadCharacteristicsW(name.as_ptr(), &mut idx)
					}
					_ => std::ptr::null_mut(),
				};
				let how = match (prev.is_some(), !mmcss.is_null()) {
					(true, true) => "priority+mmcss",
					(true, false) => "priority",
					(false, true) => "mmcss",
					(false, false) => return None,
				};
				Some(Self { prev, mmcss, how })
			}
		}

		pub(super) fn how(&self) -> &'static str {
			self.how
		}
	}

	impl Drop for Thread {
		fn drop(&mut self) {
			// SAFETY: reverts what `boost` did on this same thread (the guard is not Send).
			unsafe {
				if !self.mmcss.is_null() {
					AvRevertMmThreadCharacteristics(self.mmcss);
				}
				if let Some(p) = self.prev {
					SetThreadPriority(GetCurrentThread(), p);
				}
			}
		}
	}

	/// Opt `process` out of power throttling (EcoQoS + ignored timer requests) or hand the
	/// decision back to the system. IGNORE_TIMER_RESOLUTION exists since Windows 11 22H2; an
	/// older build rejects the unknown bit, so retry with EXECUTION_SPEED alone.
	fn power_throttling(process: HANDLE, opt_out: bool) -> &'static str {
		let set = |mask: u32| {
			let st = PROCESS_POWER_THROTTLING_STATE {
				Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
				ControlMask: mask,
				StateMask: 0,
			};
			// SAFETY: a valid process handle and a correctly sized, initialised struct.
			unsafe {
				SetProcessInformation(
					process,
					ProcessPowerThrottling,
					&st as *const _ as *const core::ffi::c_void,
					std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
				) != 0
			}
		};
		if !opt_out {
			return if set(0) { "system" } else { "failed" };
		}
		if set(PROCESS_POWER_THROTTLING_EXECUTION_SPEED | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION) {
			"ecoqos-off+timer"
		} else if set(PROCESS_POWER_THROTTLING_EXECUTION_SPEED) {
			"ecoqos-off"
		} else {
			"failed"
		}
	}

	static REPORT: Mutex<&'static str> = Mutex::new("");

	pub(super) fn process(on: bool) {
		// SAFETY: the current-process pseudo handle.
		let r = power_throttling(unsafe { GetCurrentProcess() }, on);
		*REPORT.lock().unwrap_or_else(|e| e.into_inner()) = r;
	}

	pub(super) fn report() -> String {
		REPORT.lock().unwrap_or_else(|e| e.into_inner()).to_string()
	}

	thread_local! {
		/// (epoch applied, boost held) for this worker.
		static WORKER: RefCell<(u32, Option<Thread>)> = const { RefCell::new((0, None)) };
	}

	pub(super) fn worker_unpark(epoch: u32) {
		let _ = WORKER.try_with(|w| {
			let Ok(mut w) = w.try_borrow_mut() else { return };
			if w.0 == epoch {
				return;
			}
			w.0 = epoch;
			// Drop (revert) first, then re-apply when a session is on.
			w.1 = None;
			if super::session_active() {
				w.1 = Thread::boost(Role::Worker);
			}
		});
	}

	pub(super) fn boost_child(child: &std::process::Child) -> bool {
		use std::os::windows::io::AsRawHandle;
		let h = child.as_raw_handle() as HANDLE;
		// SAFETY: the child's process handle, owned by `child` and valid for this call.
		let class = unsafe { SetPriorityClass(h, ABOVE_NORMAL_PRIORITY_CLASS) } != 0;
		let eco = power_throttling(h, true) != "failed";
		class || eco
	}

	pub(super) fn enable_dwm_mmcss() -> bool {
		// SAFETY: plain dwmapi call (process-wide flag).
		unsafe { windows_sys::Win32::Graphics::Dwm::DwmEnableMMCSS(1) >= 0 }
	}
}

// ==============================================================================================
// macOS
// ==============================================================================================
#[cfg(target_os = "macos")]
mod imp {
	use super::{Role, QOS_DEFAULT};
	use std::cell::RefCell;
	use std::sync::Mutex;

	extern "C" {
		fn pthread_set_qos_class_self_np(qos: u32, relative_priority: i32) -> i32;
		fn pthread_get_qos_class_np(thread: libc::pthread_t, qos: *mut u32, relative_priority: *mut i32) -> i32;
	}

	pub(super) struct Thread {
		prev: u32,
	}

	impl Thread {
		pub(super) fn boost(role: Role) -> Option<Self> {
			let want = role.qos();
			let (mut cur, mut rel) = (0u32, 0i32);
			// SAFETY: queries / sets this thread's own QoS.
			unsafe {
				if pthread_get_qos_class_np(libc::pthread_self(), &mut cur, &mut rel) != 0 {
					cur = 0;
				}
				// QoS classes are ordered by value (BACKGROUND 0x09 < ... < USER_INTERACTIVE 0x21).
				if cur >= want || pthread_set_qos_class_self_np(want, 0) != 0 {
					return None;
				}
			}
			// UNSPECIFIED (0) cannot be set back; DEFAULT is what such a thread ran at.
			Some(Self { prev: if cur == 0 { QOS_DEFAULT } else { cur } })
		}

		pub(super) fn how(&self) -> &'static str {
			"qos"
		}
	}

	impl Drop for Thread {
		fn drop(&mut self) {
			// SAFETY: restores this thread's own QoS (the guard is not Send).
			unsafe { pthread_set_qos_class_self_np(self.prev, 0) };
		}
	}

	/// The `NSProcessInfo` activity token. NSProcessInfo is thread-safe; the token is only
	/// handed back to `endActivity:`.
	struct Activity(objc2::rc::Retained<objc2::runtime::AnyObject>);
	// SAFETY: see above — used only for begin/end on NSProcessInfo, which is thread-safe.
	unsafe impl Send for Activity {}

	static ACTIVITY: Mutex<Option<Activity>> = Mutex::new(None);

	/// NSActivityUserInitiatedAllowingIdleSystemSleep | NSActivityLatencyCritical.
	const ACTIVITY_OPTIONS: u64 = (0x00FF_FFFF & !(1u64 << 20)) | 0xFF_0000_0000;

	pub(super) fn process(on: bool) {
		use objc2::rc::Retained;
		use objc2::runtime::AnyObject;
		use objc2::{class, msg_send};
		let mut slot = ACTIVITY.lock().unwrap_or_else(|e| e.into_inner());
		objc2::rc::autoreleasepool(|_| {
			// SAFETY: documented NSProcessInfo API with matching argument / return types.
			unsafe {
				let info: *mut AnyObject = msg_send![class!(NSProcessInfo), processInfo];
				if info.is_null() {
					return;
				}
				if on {
					if slot.is_none() {
						let reason = objc2_foundation::NSString::from_str("Pulsar streaming session");
						let token: Option<Retained<AnyObject>> =
							msg_send![&*info, beginActivityWithOptions: ACTIVITY_OPTIONS, reason: &*reason];
						*slot = token.map(Activity);
					}
				} else if let Some(a) = slot.take() {
					let _: () = msg_send![&*info, endActivity: &*a.0];
				}
			}
		});
	}

	pub(super) fn report() -> String {
		let held = ACTIVITY.lock().unwrap_or_else(|e| e.into_inner()).is_some();
		if held { "latency-critical activity" } else { "no activity" }.into()
	}

	thread_local! {
		static WORKER: RefCell<(u32, Option<Thread>)> = const { RefCell::new((0, None)) };
	}

	pub(super) fn worker_unpark(epoch: u32) {
		let _ = WORKER.try_with(|w| {
			let Ok(mut w) = w.try_borrow_mut() else { return };
			if w.0 == epoch {
				return;
			}
			w.0 = epoch;
			w.1 = None;
			if super::session_active() {
				w.1 = Thread::boost(Role::Worker);
			}
		});
	}

	pub(super) fn boost_child(child: &std::process::Child) -> bool {
		// SAFETY: plain syscall on our own child's pid. Refused without root (expected).
		unsafe { libc::setpriority(libc::PRIO_PROCESS, child.id() as libc::id_t, -5) == 0 }
	}

	pub(super) fn enable_dwm_mmcss() -> bool {
		false
	}
}

// ==============================================================================================
// Linux
// ==============================================================================================
#[cfg(target_os = "linux")]
mod imp {
	use super::Role;
	use std::cell::Cell;
	use std::sync::atomic::{AtomicBool, Ordering};
	use std::sync::{Mutex, OnceLock};
	use std::time::Duration;

	fn gettid() -> i32 {
		// SAFETY: plain syscall without arguments.
		unsafe { libc::syscall(libc::SYS_gettid) as i32 }
	}

	fn get_nice(tid: i32) -> Option<i32> {
		// SAFETY: errno is thread-local; getpriority may legitimately return -1, so clear it first.
		unsafe {
			*libc::__errno_location() = 0;
			let v = libc::getpriority(libc::PRIO_PROCESS, tid as libc::id_t);
			(v != -1 || *libc::__errno_location() == 0).then_some(v)
		}
	}

	fn set_nice(tid: i32, nice: i32) -> bool {
		// SAFETY: plain syscall; on Linux PRIO_PROCESS with a thread id targets that one thread.
		unsafe { libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, nice) == 0 }
	}

	/// rtkit unavailable (no system bus, not installed, timed out): stop asking.
	static RTKIT_DEAD: AtomicBool = AtomicBool::new(false);

	/// `MakeThreadHighPriority(tid, nice)` on a helper thread (the blocking zbus API must not
	/// run inside a tokio runtime, and this may be called from one), waited for at most 600 ms.
	fn rtkit(tid: i32, nice: i32) -> bool {
		if RTKIT_DEAD.load(Ordering::Relaxed) {
			return false;
		}
		let (tx, rx) = std::sync::mpsc::channel();
		let spawned = std::thread::Builder::new()
			.name("pulsar-rtkit".into())
			.spawn(move || {
				let _ = tx.send(rtkit_call(tid, nice));
			});
		if spawned.is_err() {
			return false;
		}
		match rx.recv_timeout(Duration::from_millis(600)) {
			Ok(Ok(())) => true,
			Ok(Err(dead)) => {
				if dead {
					RTKIT_DEAD.store(true, Ordering::Relaxed);
				}
				false
			}
			Err(_) => {
				RTKIT_DEAD.store(true, Ordering::Relaxed);
				false
			}
		}
	}

	/// Err(true) = rtkit is not there at all; Err(false) = this request was refused.
	fn rtkit_call(tid: i32, nice: i32) -> Result<(), bool> {
		static CONN: OnceLock<Option<zbus::blocking::Connection>> = OnceLock::new();
		let conn = CONN.get_or_init(|| {
			zbus::blocking::connection::Builder::system()
				.and_then(|b| b.method_timeout(Duration::from_millis(500)).build())
				.ok()
		});
		let Some(conn) = conn else { return Err(true) };
		match conn.call_method(
			Some("org.freedesktop.RealtimeKit1"),
			"/org/freedesktop/RealtimeKit1",
			Some("org.freedesktop.RealtimeKit1"),
			"MakeThreadHighPriority",
			&(tid as u64, nice),
		) {
			Ok(_) => Ok(()),
			Err(zbus::Error::MethodError(name, _, _))
				if name.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown"
					|| name.as_str() == "org.freedesktop.DBus.Error.NameHasNoOwner" =>
			{
				Err(true)
			}
			Err(_) => Err(false),
		}
	}

	/// Raise `tid` to `nice`: `setpriority` if permitted, else rtkit (when `allow_rtkit`).
	/// Returns the nice value to restore and how, or None when nothing was needed / possible.
	fn raise(tid: i32, nice: i32, allow_rtkit: bool) -> Option<(i32, &'static str)> {
		let prev = get_nice(tid)?;
		if prev <= nice {
			return None;
		}
		if set_nice(tid, nice) {
			return Some((prev, "setpriority"));
		}
		(allow_rtkit && rtkit(tid, nice)).then_some((prev, "rtkit"))
	}

	pub(super) struct Thread {
		tid: i32,
		prev: i32,
		how: &'static str,
	}

	impl Thread {
		pub(super) fn boost(role: Role) -> Option<Self> {
			let tid = gettid();
			raise(tid, role.nice(), true).map(|(prev, how)| Self { tid, prev, how })
		}

		pub(super) fn how(&self) -> &'static str {
			self.how
		}
	}

	impl Drop for Thread {
		fn drop(&mut self) {
			// Raising the nice value back is always permitted. Only when it is still boosted.
			if get_nice(self.tid).is_some_and(|n| n < self.prev) {
				set_nice(self.tid, self.prev);
			}
		}
	}

	/// Worker threads of the app's runtime: (tid, nice to restore while boosted).
	static WORKERS: Mutex<Vec<(i32, Option<i32>)>> = Mutex::new(Vec::new());
	/// Serialises `sync_workers` runs (an on and an off may race).
	static SYNC: Mutex<()> = Mutex::new(());
	/// rtkit is rate-limited per user (25 requests / 20 s by default, shared with PipeWire and
	/// the renderer's own threads): ask it for at most this many workers per session.
	const RTKIT_WORKERS: usize = 8;
	static REPORT: Mutex<String> = Mutex::new(String::new());

	/// Bring every registered worker to the wanted state (boosted while a session runs).
	fn sync_workers() {
		let _g = SYNC.lock().unwrap_or_else(|e| e.into_inner());
		let want = super::session_active();
		let snapshot: Vec<(i32, Option<i32>)> = WORKERS.lock().unwrap_or_else(|e| e.into_inner()).clone();
		let mut budget = RTKIT_WORKERS;
		let mut changes = Vec::new();
		for (tid, orig) in snapshot.iter().copied() {
			match (want, orig) {
				(true, None) => {
					if let Some((prev, how)) = raise(tid, Role::Worker.nice(), budget > 0) {
						if how == "rtkit" {
							budget -= 1;
						}
						changes.push((tid, Some(prev)));
					}
				}
				(false, Some(prev)) => {
					if get_nice(tid).is_some_and(|n| n < prev) {
						set_nice(tid, prev);
					}
					changes.push((tid, None));
				}
				_ => {}
			}
		}
		let mut w = WORKERS.lock().unwrap_or_else(|e| e.into_inner());
		for (tid, v) in changes {
			if let Some(e) = w.iter_mut().find(|e| e.0 == tid) {
				e.1 = v;
			}
		}
		let boosted = w.iter().filter(|e| e.1.is_some()).count();
		*REPORT.lock().unwrap_or_else(|e| e.into_inner()) = format!("workers {boosted}/{}", w.len());
	}

	fn sync_in_background() {
		let _ = std::thread::Builder::new().name("pulsar-prio".into()).spawn(sync_workers);
	}

	pub(super) fn process(on: bool) {
		if on {
			*REPORT.lock().unwrap_or_else(|e| e.into_inner()) = "workers: renice pending".into();
		}
		sync_in_background();
	}

	pub(super) fn report() -> String {
		REPORT.lock().unwrap_or_else(|e| e.into_inner()).clone()
	}

	thread_local! {
		static REGISTERED: Cell<bool> = const { Cell::new(false) };
	}

	pub(super) fn worker_unpark(_epoch: u32) {
		// Register once per worker (tokio's workers live as long as the runtime).
		let first = REGISTERED.try_with(|r| !r.replace(true)).unwrap_or(false);
		if first {
			WORKERS.lock().unwrap_or_else(|e| e.into_inner()).push((gettid(), None));
			if super::session_active() {
				sync_in_background();
			}
		}
	}

	pub(super) fn boost_child(child: &std::process::Child) -> bool {
		set_nice(child.id() as i32, -5)
	}

	pub(super) fn enable_dwm_mmcss() -> bool {
		false
	}
}

// ==============================================================================================
// Everything else (Android, iOS, BSD): no-ops.
// ==============================================================================================
#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
mod imp {
	use super::Role;

	pub(super) struct Thread;

	impl Thread {
		pub(super) fn boost(_role: Role) -> Option<Self> {
			None
		}

		pub(super) fn how(&self) -> &'static str {
			"none"
		}
	}

	pub(super) fn process(_on: bool) {}

	pub(super) fn report() -> String {
		String::new()
	}

	pub(super) fn worker_unpark(_epoch: u32) {}

	pub(super) fn boost_child(_child: &std::process::Child) -> bool {
		false
	}

	pub(super) fn enable_dwm_mmcss() -> bool {
		false
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The session state is process-global: tests that flip it must not overlap.
	static SERIAL: Mutex<()> = Mutex::new(());

	#[test]
	fn role_levels_are_never_realtime() {
		for r in [Role::Capture, Role::Present, Role::Network, Role::Worker] {
			// Linux: nice only, inside rtkit's default floor (-15).
			assert!((-15..0).contains(&r.nice()), "{r:?}");
			// macOS: a QoS class, never above USER_INTERACTIVE.
			assert!(r.qos() <= QOS_USER_INTERACTIVE && r.qos() > QOS_DEFAULT, "{r:?}");
			// Windows: inside the dynamic range (TIME_CRITICAL at most).
			assert!((1..=WIN_TIME_CRITICAL).contains(&r.windows().0), "{r:?}");
		}
		// Workers and capture sit below the light network / present threads where it matters.
		assert!(Role::Worker.nice() > Role::Network.nice());
		assert!(Role::Capture.nice() > Role::Present.nice());
	}

	#[test]
	fn session_is_refcounted() {
		let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
		let a = session();
		let b = session();
		assert!(session_active());
		drop(a);
		assert!(session_active());
		drop(b);
		assert!(!session_active());
	}

	#[cfg(windows)]
	fn thread_priority() -> i32 {
		use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadPriority};
		// SAFETY: current-thread pseudo handle.
		unsafe { GetThreadPriority(GetCurrentThread()) }
	}

	#[cfg(windows)]
	#[test]
	fn windows_thread_boost_restores_on_drop() {
		std::thread::spawn(|| {
			let before = thread_priority();
			{
				let g = boost_current_thread(Role::Network);
				assert_ne!(g.how(), "none");
				assert_eq!(thread_priority(), WIN_HIGHEST);
			}
			assert_eq!(thread_priority(), before);
		})
		.join()
		.unwrap();
	}

	#[cfg(windows)]
	#[test]
	fn windows_session_opts_out_of_power_throttling() {
		let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
		let g = session();
		// Windows 11 22H2+ takes both bits; Windows 10 the execution-speed one.
		assert!(session_report().starts_with("ecoqos-off"), "{}", session_report());
		drop(g);
		assert_eq!(session_report(), "system");
	}

	#[cfg(windows)]
	#[test]
	fn windows_child_gets_above_normal() {
		use std::os::windows::io::AsRawHandle;
		use std::os::windows::process::CommandExt;
		use windows_sys::Win32::System::Threading::{GetPriorityClass, ABOVE_NORMAL_PRIORITY_CLASS};
		let mut child = std::process::Command::new("cmd")
			.args(["/c", "ping -n 3 127.0.0.1 >nul"])
			.creation_flags(0x0800_0000) // CREATE_NO_WINDOW
			.stdout(std::process::Stdio::null())
			.spawn()
			.unwrap();
		assert!(boost_child(&child));
		// SAFETY: the child's handle, valid while `child` lives.
		let class = unsafe { GetPriorityClass(child.as_raw_handle() as _) };
		let _ = child.kill();
		let _ = child.wait();
		assert_eq!(class, ABOVE_NORMAL_PRIORITY_CLASS);
	}

	#[cfg(windows)]
	#[test]
	fn windows_workers_follow_the_session() {
		let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
		let rt = tokio::runtime::Builder::new_multi_thread()
			.worker_threads(2)
			.on_thread_unpark(on_worker_unpark)
			.enable_all()
			.build()
			.unwrap();
		let prio = || rt.block_on(async { tokio::spawn(async { thread_priority() }).await.unwrap() });
		let idle = prio();
		let s = session();
		// The first wake after the flip applies it; the task runs right after that wake.
		let mut boosted = prio();
		for _ in 0..50 {
			if boosted == WIN_ABOVE_NORMAL {
				break;
			}
			std::thread::sleep(std::time::Duration::from_millis(5));
			boosted = prio();
		}
		assert_eq!(boosted, WIN_ABOVE_NORMAL);
		drop(s);
		let mut after = prio();
		for _ in 0..50 {
			if after == idle {
				break;
			}
			std::thread::sleep(std::time::Duration::from_millis(5));
			after = prio();
		}
		assert_eq!(after, idle);
	}
}
