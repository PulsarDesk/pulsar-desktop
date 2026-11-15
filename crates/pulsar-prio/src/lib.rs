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
//! - [`on_worker_unpark`] (+ [`on_thread_start`] / [`on_thread_stop`]): hooks for the app's tokio
//!   runtime. The forwarder, the wire pacer, the session read loop and the client's receive loop
//!   are tokio tasks on ordinary workers; while a session runs the workers run boosted too.
//!
//! Per OS (never a realtime class that can starve the system):
//!
//! | role      | Windows                               | macOS QoS        | Linux nice |
//! |-----------|---------------------------------------|------------------|------------|
//! | `Capture` | TIME_CRITICAL + MMCSS "Capture"       | USER_INTERACTIVE | -5         |
//! | `Present` | HIGHEST + MMCSS "Games"               | USER_INTERACTIVE | -10        |
//! | `Decode`  | HIGHEST + MMCSS "Games"               | USER_INTERACTIVE | -5         |
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
//!   sleep stays with the existing keep-awake code. A worker drops back to the QoS it had before
//!   its first session (or DEFAULT), also when it inherited a raised class from its creator.
//! - Linux: nice values only (CFS weight), never SCHED_FIFO / SCHED_RR. `setpriority` on the
//!   thread when the user may (CAP_SYS_NICE or an RLIMIT_NICE from limits.conf). Without that,
//!   the few dedicated media threads (and the media children) ask rtkit's
//!   `MakeThreadHighPriority` over the system bus (what PipeWire / PulseAudio use; rtkit clamps
//!   at nice -15). rtkit rate-limits per user (25 actions / 20 s by default) and PipeWire's
//!   realtime requests share that budget, so: the async workers never use rtkit (setpriority
//!   only), every rtkit request goes through one process-wide budget (6 / 20 s, extra requests
//!   are skipped), and a request is fire-and-forget on a helper thread (a media thread never
//!   waits on D-Bus before it starts its work). Raising the nice value back is always allowed,
//!   so every revert is a plain `setpriority`.
//!   Threads a boosted thread creates inherit its nice value (libav / GStreamer / dav1d / Mesa
//!   threads started from a boosted thread run boosted too). Capture and Decode are -5, not
//!   -10, for that reason: software x264 / HEVC / AV1 slice threads inherit it and must not
//!   crowd out the compositor / the game. The same inheritance reaches tokio threads started
//!   from a boosted worker and programs forked from one: the runtime registers every thread it
//!   starts and the session end drops any of them that is below the process's own nice level
//!   (read once, the main thread's), and [`unboost_command`] puts a launched game / app / shell
//!   back to that level.
//!   LIMIT: nice values only weigh threads inside the same CPU cgroup (or autogroup). On a
//!   systemd desktop that delegates the cpu controller to the user manager (KDE Plasma app
//!   slices, Fedora uresourced, any unit with CPUWeight=) a game in its own `app-*.scope`
//!   competes with Pulsar by `cpu.weight`, whatever the nice values; with autogroup (no systemd
//!   session) only the autogroup's nice counts. There the boost only orders Pulsar's own threads
//!   against each other and against its children; it does NOT put the host above the game.
//!   Not measured under a CPU-bound game yet; raising Pulsar's own scope weight through the user
//!   systemd manager while hosting is the follow-up.
//! - Android / iOS / others: no-ops (the Android app boosts its decoder threads in Kotlin).
//!
//! `PULSAR_PRIO=0` turns every boost of this crate off (A/B against the default).

use std::collections::VecDeque;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// What a boosted thread does; picks the per-OS level (see the crate docs table).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
	/// Host capture / encode loop.
	Capture,
	/// Client present loop (one core).
	Present,
	/// Client decode thread, or a present thread that starts many threads of its own (Linux:
	/// libavcodec slice threads, Mesa worker queues inherit the nice value): -5 on Linux.
	Decode,
	/// RTP send / receive, forwarders: little CPU, must run the moment data is there.
	Network,
	/// An async-runtime worker while a session runs (see [`on_worker_unpark`]).
	Worker,
}

impl Role {
	/// Linux nice level for this role.
	pub const fn nice(self) -> i32 {
		match self {
			Role::Capture | Role::Decode | Role::Worker => -5,
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
			Role::Present | Role::Decode => (WIN_HIGHEST, Some("Games")),
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

/// At most `max` events inside any `window` (a sliding log of the accepted times). Linux uses
/// it for the process-wide rtkit budget; kept OS-independent so it is tested everywhere.
#[allow(dead_code)]
struct RateLimit {
	max: usize,
	window: Duration,
	times: VecDeque<Instant>,
}

#[allow(dead_code)]
impl RateLimit {
	const fn new(max: usize, window: Duration) -> Self {
		Self { max, window, times: VecDeque::new() }
	}

	/// Take one slot at `now`; false (nothing taken) when the window is full.
	fn allow(&mut self, now: Instant) -> bool {
		while self.times.front().is_some_and(|t| now.saturating_duration_since(*t) >= self.window) {
			self.times.pop_front();
		}
		if self.times.len() >= self.max {
			return false;
		}
		self.times.push_back(now);
		true
	}
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
	let inner = if disabled() {
		None
	} else {
		imp::init();
		imp::Thread::boost(role)
	};
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
		imp::init();
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
/// when nothing changed. Linux marks the thread as a worker once instead, and the session switch
/// renices the registered workers (setpriority only, no rtkit).
pub fn on_worker_unpark() {
	if disabled() {
		return;
	}
	// Acquire pairs with the SeqCst fetch_add after the ACTIVE store: a worker that sees the new
	// epoch also sees the new ACTIVE (a Relaxed load could read the new epoch, then the old
	// ACTIVE, and skip the boost for the whole session on ARM64).
	imp::worker_unpark(EPOCH.load(Ordering::Acquire));
}

/// Hook for `tokio::runtime::Builder::on_thread_start` (every runtime thread, workers and the
/// blocking pool). Linux: registers the thread, so a thread that inherited a boosted nice value
/// from the worker that started it (tokio's `block_in_place` hand-off, `spawn_blocking`) drops
/// back with the session. No-op elsewhere.
pub fn on_thread_start() {
	if disabled() {
		return;
	}
	imp::init();
	imp::thread_start();
}

/// Hook for `tokio::runtime::Builder::on_thread_stop`: forgets the thread (Linux: thread ids
/// are reused, a stale one must never be reniced). No-op elsewhere.
pub fn on_thread_stop() {
	if disabled() {
		return;
	}
	imp::thread_stop();
}

// ---- children / misc -------------------------------------------------------------------------

/// Raise a media child process (the ffmpeg encoder / audio capture) for the session.
/// Windows: ABOVE_NORMAL_PRIORITY_CLASS + the same EcoQoS opt-out. ABOVE_NORMAL, not HIGH: it
/// already wins over a NORMAL-class game, and a software x264 child at HIGH on every core would
/// stall the host's own desktop. Unix: nice -5 for the child's main thread (and the threads it
/// starts later): Linux via `setpriority` when the user may, else rtkit (process budget);
/// macOS needs root, so it is usually refused. Returns whether the child runs raised.
pub fn boost_child(child: &std::process::Child) -> bool {
	if disabled() {
		return false;
	}
	imp::boost_child(child)
}

/// Undo an inherited boost in a program Pulsar launches for the user (a game / app the client
/// starts through the host, a shell hook, an opener). Linux: a child forked from a boosted
/// thread inherits its nice value and would keep it for its whole life, next to the encoder;
/// this resets the child to the process's own level before exec (raising the nice value is
/// always allowed). No-op on other OSes (Windows priority classes and macOS QoS are not
/// inherited this way).
pub fn unboost_command(cmd: &mut std::process::Command) {
	if disabled() {
		return;
	}
	imp::init();
	imp::unboost_command(cmd);
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

	pub(super) fn init() {}

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

	// Windows threads start at NORMAL whatever their creator runs at: nothing to track.
	pub(super) fn thread_start() {}

	pub(super) fn thread_stop() {}

	// A child's priority class does not follow the creating thread's priority.
	pub(super) fn unboost_command(_cmd: &mut std::process::Command) {}

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
	use std::cell::Cell;
	use std::sync::Mutex;

	extern "C" {
		fn pthread_set_qos_class_self_np(qos: u32, relative_priority: i32) -> i32;
		fn pthread_get_qos_class_np(thread: libc::pthread_t, qos: *mut u32, relative_priority: *mut i32) -> i32;
	}

	pub(super) fn init() {}

	/// This thread's QoS class (0 = UNSPECIFIED, also on failure).
	fn current_qos() -> u32 {
		let (mut cur, mut rel) = (0u32, 0i32);
		// SAFETY: queries this thread's own QoS into two valid out-pointers.
		if unsafe { pthread_get_qos_class_np(libc::pthread_self(), &mut cur, &mut rel) } != 0 {
			return 0;
		}
		cur
	}

	fn set_qos(qos: u32) -> bool {
		// SAFETY: sets this thread's own QoS.
		unsafe { pthread_set_qos_class_self_np(qos, 0) == 0 }
	}

	pub(super) struct Thread {
		prev: u32,
	}

	impl Thread {
		pub(super) fn boost(role: Role) -> Option<Self> {
			let want = role.qos();
			let cur = current_qos();
			// QoS classes are ordered by value (BACKGROUND 0x09 < ... < USER_INTERACTIVE 0x21).
			if cur >= want || !set_qos(want) {
				return None;
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
			// Restores this thread's own QoS (the guard is not Send).
			set_qos(self.prev);
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
		/// (epoch applied, the QoS this worker drops back to; 0 = not seen yet).
		static WORKER: Cell<(u32, u32)> = const { Cell::new((u32::MAX, 0)) };
	}

	/// A worker's resting QoS is what it ran at when first seen outside a session (or DEFAULT
	/// when first seen inside one, or UNSPECIFIED). Session on: raise to USER_INITIATED when
	/// below. Off: drop to the resting class whenever above it — also when it was not raised
	/// here but inherited a higher class from the thread that created it.
	pub(super) fn worker_unpark(epoch: u32) {
		let _ = WORKER.try_with(|w| {
			let (seen, mut base) = w.get();
			if seen == epoch {
				return;
			}
			let on = super::session_active();
			let cur = current_qos();
			if base == 0 {
				base = if !on && cur != 0 { cur } else { QOS_DEFAULT };
			}
			if on {
				if cur < Role::Worker.qos() {
					set_qos(Role::Worker.qos());
				}
			} else if cur > base {
				set_qos(base);
			}
			w.set((epoch, base));
		});
	}

	pub(super) fn thread_start() {}

	pub(super) fn thread_stop() {}

	pub(super) fn unboost_command(_cmd: &mut std::process::Command) {}

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
	use super::{RateLimit, Role};
	use std::cell::Cell;
	use std::sync::atomic::{AtomicBool, Ordering};
	use std::sync::mpsc::Sender;
	use std::sync::{Mutex, OnceLock};
	use std::time::{Duration, Instant};

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

	/// Whether `tid` is still a thread of THIS process. Linux thread ids are pids from one global
	/// space and get reused; a registered id must never renice another process's thread.
	fn own_thread(tid: i32) -> bool {
		std::path::Path::new(&format!("/proc/self/task/{tid}")).exists()
			|| !std::path::Path::new("/proc/self/task").exists()
	}

	static BASELINE: OnceLock<i32> = OnceLock::new();

	/// The process's own nice level: its main thread's, read once on the first call into this
	/// crate. The app's main thread is never raised, and every boosting entry point calls
	/// [`init`] before it raises anything, so this is the level Pulsar was started at.
	fn baseline() -> i32 {
		// SAFETY: getpid has no failure mode; the main thread's tid is the pid.
		*BASELINE.get_or_init(|| get_nice(unsafe { libc::getpid() }).unwrap_or(0))
	}

	pub(super) fn init() {
		baseline();
	}

	// ---- rtkit ----

	/// rtkit is not there (no system bus, not installed): stop asking. Never set on a timeout or
	/// a refusal — a slow D-Bus activation is not a missing rtkit.
	static RTKIT_DEAD: AtomicBool = AtomicBool::new(false);
	/// Process-wide rtkit budget. rtkit allows 25 actions / 20 s per USER by default, shared with
	/// every PipeWire client's realtime request (Pulsar's own audio, the game's): keep well under
	/// it so a reconnect / renderer restart can never starve PipeWire of its RT slot (crackle).
	static RTKIT_BUDGET: Mutex<RateLimit> = Mutex::new(RateLimit::new(6, Duration::from_secs(20)));

	enum Req {
		/// MakeThreadHighPriority(thread of this process, nice).
		Thread(i32, i32),
		/// MakeThreadHighPriorityWithPID(child, its main thread, nice).
		Child(u32, i32),
	}

	/// Queue an rtkit request, fire-and-forget: the caller never waits on D-Bus (a media thread
	/// starts its work right away; the nice value changes when rtkit answers). False when rtkit
	/// is known to be missing, the budget is spent, or the helper thread could not start.
	fn rtkit(req: Req) -> bool {
		if RTKIT_DEAD.load(Ordering::Relaxed) {
			return false;
		}
		if !RTKIT_BUDGET.lock().unwrap_or_else(|e| e.into_inner()).allow(Instant::now()) {
			return false;
		}
		static TX: OnceLock<Option<Sender<Req>>> = OnceLock::new();
		let tx = TX.get_or_init(|| {
			let (tx, rx) = std::sync::mpsc::channel::<Req>();
			std::thread::Builder::new()
				.name("pulsar-rtkit".into())
				.spawn(move || {
					// Started from whichever thread asked first: never keep its boost.
					let me = gettid();
					if get_nice(me).is_some_and(|n| n < baseline()) {
						set_nice(me, baseline());
					}
					for r in rx {
						rtkit_call(r);
					}
				})
				.ok()
				.map(|_| tx)
		});
		tx.as_ref().is_some_and(|tx| tx.send(req).is_ok())
	}

	fn rtkit_call(req: Req) {
		static CONN: OnceLock<Option<zbus::blocking::Connection>> = OnceLock::new();
		let conn = CONN.get_or_init(|| {
			zbus::blocking::connection::Builder::system()
				.and_then(|b| b.method_timeout(Duration::from_secs(2)).build())
				.ok()
		});
		let Some(conn) = conn else {
			RTKIT_DEAD.store(true, Ordering::Relaxed);
			return;
		};
		const DEST: &str = "org.freedesktop.RealtimeKit1";
		const PATH: &str = "/org/freedesktop/RealtimeKit1";
		let r = match req {
			Req::Thread(tid, nice) => {
				conn.call_method(Some(DEST), PATH, Some(DEST), "MakeThreadHighPriority", &(tid as u64, nice))
			}
			Req::Child(pid, nice) => conn.call_method(
				Some(DEST),
				PATH,
				Some(DEST),
				"MakeThreadHighPriorityWithPID",
				&(pid as u64, pid as u64, nice),
			),
		};
		if let Err(zbus::Error::MethodError(name, _, _)) = &r {
			if name.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown"
				|| name.as_str() == "org.freedesktop.DBus.Error.NameHasNoOwner"
			{
				RTKIT_DEAD.store(true, Ordering::Relaxed);
			}
		}
	}

	// ---- dedicated threads ----

	pub(super) struct Thread {
		tid: i32,
		prev: i32,
		how: &'static str,
	}

	impl Thread {
		pub(super) fn boost(role: Role) -> Option<Self> {
			let tid = gettid();
			let want = role.nice();
			let prev = get_nice(tid)?;
			if prev <= want {
				return None;
			}
			let how = if set_nice(tid, want) {
				"setpriority"
			} else if rtkit(Req::Thread(tid, want)) {
				// Applied when rtkit answers; the revert below re-reads the nice value, so it is
				// right whether it landed or not.
				"rtkit"
			} else {
				return None;
			};
			Some(Self { tid, prev, how })
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

	// ---- runtime threads ----

	/// A thread of the app's tokio runtime (registered at start, removed at stop).
	struct Entry {
		tid: i32,
		/// Seen in `on_thread_unpark` (a worker, not a blocking-pool thread): raised while a
		/// session runs. Every entry drops back to the baseline when the session ends.
		worker: bool,
	}

	static THREADS: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
	static REPORT: Mutex<String> = Mutex::new(String::new());

	/// Bring one registered thread to the wanted state. setpriority only: rtkit's per-user
	/// budget is for the media threads and PipeWire, not for N workers per session.
	fn apply(e: &Entry, on: bool, base: i32) {
		let want = Role::Worker.nice();
		let Some(cur) = get_nice(e.tid) else { return };
		if on {
			if e.worker && cur > want {
				set_nice(e.tid, want);
			}
		} else if cur < base {
			// Raised here, or inherited a raised level from the worker that started it.
			set_nice(e.tid, base);
		}
	}

	fn sync(threads: &mut Vec<Entry>, on: bool) {
		let base = baseline();
		threads.retain(|e| own_thread(e.tid));
		for e in threads.iter() {
			apply(e, on, base);
		}
		let workers: Vec<&Entry> = threads.iter().filter(|e| e.worker).collect();
		let raised = workers.iter().filter(|e| get_nice(e.tid).is_some_and(|n| n < base)).count();
		*REPORT.lock().unwrap_or_else(|e| e.into_inner()) = format!("workers {raised}/{}", workers.len());
	}

	pub(super) fn process(on: bool) {
		// A handful of setpriority calls under the registry lock: the start / stop / unpark
		// hooks take the same lock, so no thread misses an on / off flip.
		let mut t = THREADS.lock().unwrap_or_else(|e| e.into_inner());
		sync(&mut t, on);
	}

	pub(super) fn report() -> String {
		REPORT.lock().unwrap_or_else(|e| e.into_inner()).clone()
	}

	pub(super) fn thread_start() {
		let tid = gettid();
		let mut t = THREADS.lock().unwrap_or_else(|e| e.into_inner());
		t.retain(|e| e.tid != tid);
		let e = Entry { tid, worker: false };
		if !super::session_active() {
			apply(&e, false, baseline());
		}
		t.push(e);
	}

	pub(super) fn thread_stop() {
		let tid = gettid();
		THREADS.lock().unwrap_or_else(|e| e.into_inner()).retain(|e| e.tid != tid);
	}

	#[cfg(test)]
	pub(super) fn registered(tid: i32) -> bool {
		THREADS.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|e| e.tid == tid)
	}

	thread_local! {
		static WORKER: Cell<bool> = const { Cell::new(false) };
	}

	pub(super) fn worker_unpark(_epoch: u32) {
		// Mark once per worker; the session switch renices the workers itself (process()).
		let first = WORKER.try_with(|r| !r.replace(true)).unwrap_or(false);
		if !first {
			return;
		}
		let tid = gettid();
		let mut t = THREADS.lock().unwrap_or_else(|e| e.into_inner());
		let idx = match t.iter().position(|e| e.tid == tid) {
			Some(i) => i,
			// A runtime without the start hook: register here (no stop hook either, so the
			// own_thread check in sync() prunes it).
			None => {
				t.push(Entry { tid, worker: false });
				t.len() - 1
			}
		};
		t[idx].worker = true;
		apply(&t[idx], super::session_active(), baseline());
	}

	pub(super) fn unboost_command(cmd: &mut std::process::Command) {
		use std::os::unix::process::CommandExt;
		let base = baseline();
		// SAFETY: the closure runs in the forked child before exec and only makes two
		// async-signal-safe syscalls (no allocation, no locks). Raising the nice value (back to
		// the process's own level) is always permitted; a child already at / above it is left.
		unsafe {
			cmd.pre_exec(move || {
				if libc::getpriority(libc::PRIO_PROCESS, 0) < base {
					libc::setpriority(libc::PRIO_PROCESS, 0, base);
				}
				Ok(())
			});
		}
	}

	pub(super) fn boost_child(child: &std::process::Child) -> bool {
		let want = Role::Capture.nice();
		let pid = child.id();
		match get_nice(pid as i32) {
			// Already there (forked from a boosted thread).
			Some(n) if n <= want => true,
			// rtkit allows the caller's own UID's processes; fire-and-forget, right after spawn,
			// so the threads ffmpeg starts once it initialises inherit it once it lands.
			Some(_) => set_nice(pid as i32, want) || rtkit(Req::Child(pid, want)),
			None => false,
		}
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

	pub(super) fn init() {}

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

	pub(super) fn thread_start() {}

	pub(super) fn thread_stop() {}

	pub(super) fn unboost_command(_cmd: &mut std::process::Command) {}

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
		for r in [Role::Capture, Role::Present, Role::Decode, Role::Network, Role::Worker] {
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
		// Threads that fan out to every core (software decode, Mesa queues) stay at -5.
		assert_eq!(Role::Decode.nice(), Role::Capture.nice());
	}

	#[test]
	fn rate_limit_is_a_sliding_window() {
		let mut l = RateLimit::new(3, Duration::from_secs(20));
		let t0 = Instant::now();
		assert!(l.allow(t0));
		assert!(l.allow(t0 + Duration::from_secs(1)));
		assert!(l.allow(t0 + Duration::from_secs(2)));
		assert!(!l.allow(t0 + Duration::from_secs(3)), "fourth inside the window");
		assert!(!l.allow(t0 + Duration::from_secs(19)));
		// The first slot frees up 20 s after it was taken, the next ones 1 s apart.
		assert!(l.allow(t0 + Duration::from_secs(20)));
		assert!(!l.allow(t0 + Duration::from_millis(20_500)));
		assert!(l.allow(t0 + Duration::from_secs(21)));
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

	#[test]
	fn unboost_command_still_spawns() {
		// The pre_exec hook (Linux) must never make a spawn fail.
		#[cfg(windows)]
		let mut c = std::process::Command::new("cmd");
		#[cfg(windows)]
		c.args(["/c", "exit 0"]);
		#[cfg(not(windows))]
		let mut c = std::process::Command::new("true");
		unboost_command(&mut c);
		let st = c.stdout(std::process::Stdio::null()).status().unwrap();
		assert!(st.success());
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
			.on_thread_start(on_thread_start)
			.on_thread_stop(on_thread_stop)
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

	/// Linux: runtime threads register at start and leave the registry at stop (a stale thread
	/// id must never be reniced later). Needs no privilege.
	#[cfg(target_os = "linux")]
	#[test]
	fn linux_runtime_threads_deregister() {
		let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
		let rt = tokio::runtime::Builder::new_multi_thread()
			.worker_threads(2)
			.on_thread_start(on_thread_start)
			.on_thread_stop(on_thread_stop)
			.on_thread_unpark(on_worker_unpark)
			.enable_all()
			.build()
			.unwrap();
		let tid = rt.block_on(async {
			tokio::spawn(async { unsafe { libc::syscall(libc::SYS_gettid) as i32 } }).await.unwrap()
		});
		assert!(imp::registered(tid));
		rt.shutdown_timeout(std::time::Duration::from_secs(5));
		// Worker threads run their stop hook as they exit; give them a moment.
		for _ in 0..100 {
			if !imp::registered(tid) {
				break;
			}
			std::thread::sleep(std::time::Duration::from_millis(10));
		}
		assert!(!imp::registered(tid));
	}
}
