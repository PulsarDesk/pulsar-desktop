//! X11 GStreamer host encode (RK3588 / Orange Pi 5 MPP: `ximagesrc ! … ! mpph26Xenc ! … !
//! udpsink`, and the `kmssrc` zero-copy variant) — the runners behind the gst branch of
//! `handlers::make_on_stream`.
//!
//! Two runners:
//! - IN-PROCESS (`pulsar_core::capture::start_pipeline`, 2026-11-15): the description runs inside
//!   the app, held in the session's `cap_slot`, so ABR steps (`bps`), keyframe requests
//!   (force-key-unit) and loss-recovery flips (the short-GOP key-unit timer) apply live with no
//!   restart. It STARTS on its own thread ([`X11InProcStart`]): MPP init plus the first encoded
//!   frame — up to 2 × 2.6 s when the BGRx-direct variant fails — must never block the session
//!   loop (client input, ping/pong and data messages run on it). The old pipeline's teardown
//!   (`set_state(Null)` joins the ximagesrc / MPP threads) runs off the loop too, and the start
//!   joins it first.
//! - `gst-launch` CHILD ([`X11GstChild`]): `kmssrc` (reading the scanout FB needs the
//!   CAP_SYS_ADMIN `~/pulsar-gst-launch` copy), `PULSAR_GST_INPROC=0`, and the fallback when
//!   the in-process start fails. It can't change its bitrate, so `handlers::cli_bitrate_absorbs`
//!   decides which steps restart it.
//!
//! TRADE-OFF (in-process): the Rockchip vendor stack (mpph26xenc + librga/libmpp, with the known
//! RGA >4G misbehaviour) runs inside the app process: a crash or abort there now takes the
//! whole host app down (UI, every session, audio) where it used to kill only the child. A crash
//! marker in the temp dir is written before each in-process start and cleared once it has run
//! [`MARKER_HOLD`]; a launch that finds it left behind runs the child for that whole run.
//! `PULSAR_GST_INPROC=0` forces the child. Unverified on the Orange Pi as of 2026-11-15.
//!
//! Linux-only. Type-checked from Windows through a harness (pulsar-tauri itself doesn't build
//! for Linux there); untested on hardware.

use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use pulsar_core::capture::{StartError, WaylandCapture};
use pulsar_core::service::{DataMsg, StreamReq};

use super::{AppHandle, Emitter, SessionEvent};

/// How long an in-process start waits for the encoder's first frame per variant. The RGA >4G
/// failure (BGRx-direct never delivers a frame) shows within this, as the child's watchdog
/// measured.
const FIRST_FRAME_WITHIN: Duration = Duration::from_millis(2600);
/// A still-STARTING pipeline absorbs same-shape requests at most this long (two variants of
/// [`FIRST_FRAME_WITHIN`] + teardown + slack); a start hung past it takes the full restart.
const START_GRACE: Duration = Duration::from_secs(8);
/// A running pipeline whose encoded-frame count stayed flat this long is wedged (ximagesrc
/// emits at the frame rate even on a still screen): the next request restarts it in full.
const STALL_AFTER: Duration = Duration::from_secs(2);
/// Consecutive in-process starts that failed (bus error / no frame, on the last variant) before
/// the in-process runner is turned off for the rest of the process.
const LATCH_AFTER: u32 = 2;
/// How long an in-process pipeline must run before its start's crash marker is cleared.
const MARKER_HOLD: Duration = Duration::from_secs(10);

/// In-process turned off for this process: [`LATCH_AFTER`] starts in a row failed.
static INPROC_BROKEN: AtomicBool = AtomicBool::new(false);
static INPROC_FAILS: AtomicU32 = AtomicU32::new(0);
/// BGRx-direct produced no frame in-process while the CPU-convert variant then did: the RGA
/// >4G mapping failure, a property of this box's memory — skip BGRx-direct for the process.
static BGRX_BROKEN: AtomicBool = AtomicBool::new(false);
/// Whether the previous run of the app left a crash marker behind (decided once per process).
static CRASHED_LAST_RUN: OnceLock<bool> = OnceLock::new();

fn marker_path() -> std::path::PathBuf {
	std::env::temp_dir().join("pulsar-gst-inproc-start.marker")
}

/// Whether an X11 gst stream should try the in-process pipeline first. `PULSAR_GST_INPROC=0`
/// forces the old `gst-launch` child (bisect knob); so do a latched failure and a crash marker
/// the previous run left behind (the marker is consumed: the launch after this one tries again).
pub(super) fn in_process_wanted() -> bool {
	if std::env::var("PULSAR_GST_INPROC").as_deref() == Ok("0") || INPROC_BROKEN.load(Ordering::Relaxed) {
		return false;
	}
	let crashed = *CRASHED_LAST_RUN.get_or_init(|| {
		let m = marker_path();
		let left = m.exists();
		if left {
			let _ = std::fs::remove_file(&m);
			tracing::warn!(
				"an in-process X11 gst start did not finish last run (crash in the vendor encoder?) — \
				 gst-launch child for this run"
			);
		}
		left
	});
	!crashed
}

/// Per-session state of the X11 gst runners, shared by `make_on_stream` and the start thread.
#[derive(Default)]
pub(super) struct X11GstRun {
	/// The in-process pipeline (running in `cap_slot`, or still starting).
	pub(super) inproc: Option<X11InProc>,
	/// The `gst-launch` child: its request, when it started, its pid (the RGA fallback swaps it).
	pub(super) child: Option<(StreamReq, Instant, Arc<AtomicU32>)>,
}

pub(super) struct X11InProc {
	/// The latest request applied to it (a starting pipeline applies it once up).
	pub(super) req: StreamReq,
	pub(super) since: Instant,
	pub(super) starting: bool,
	/// `frames_encoded()` at the last look, and when it last moved.
	pub(super) frames: u64,
	pub(super) frames_at: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Live {
	Starting,
	Running,
	Stalled,
	Gone,
}

/// The in-process fast path: `true` when `req` was applied live (bitrate → `bps`, recovery →
/// the key-unit timer) or handed to a pipeline still starting, so the caller returns without a
/// restart. `false` → full restart: another shape, a host-ordered restart of a running
/// pipeline, a dead or stalled one, or a start hung past [`START_GRACE`].
pub(super) fn try_live(
	run: &Mutex<X11GstRun>,
	cap_slot: &Mutex<Option<WaylandCapture>>,
	req: &StreamReq,
	host_ordered: bool,
) -> bool {
	let mut run = run.lock().unwrap();
	let Some(ip) = run.inproc.as_mut() else { return false };
	let mut probe = ip.req.clone();
	probe.bitrate_kbps = req.bitrate_kbps;
	probe.loss_recovery = req.loss_recovery;
	// A step to 0 ("auto" → the host default) re-resolves the default in full.
	let same_shape = probe == *req && (req.bitrate_kbps > 0 || ip.req.bitrate_kbps == 0);
	let slot = cap_slot.lock().unwrap();
	let state = match slot.as_ref() {
		None if ip.starting && ip.since.elapsed() < START_GRACE => Live::Starting,
		None => Live::Gone,
		Some(cap) => {
			let now = Instant::now();
			let n = cap.frames_encoded();
			if n != ip.frames {
				ip.frames = n;
				ip.frames_at = now;
			}
			if !cap.is_alive() {
				Live::Gone
			} else if now.duration_since(ip.frames_at) >= STALL_AFTER {
				Live::Stalled
			} else {
				Live::Running
			}
		}
	};
	// Starting: even a host-ordered restart is absorbed — the pipeline about to come up was
	// given its geometry a moment ago and opens on an IDR.
	if same_shape && (state == Live::Starting || (state == Live::Running && !host_ordered)) {
		let mut applied = false;
		if let Some(cap) = slot.as_ref() {
			applied = req.bitrate_kbps > 0
				&& req.bitrate_kbps != ip.req.bitrate_kbps
				&& cap.set_bitrate(req.bitrate_kbps);
			cap.set_short_gop(req.loss_recovery.is_active());
		}
		ip.req = req.clone();
		tracing::info!(
			bitrate_kbps = req.bitrate_kbps,
			applied,
			starting = state == Live::Starting,
			recovery = ?req.loss_recovery,
			"x11 gst restream applied live on the in-process pipeline"
		);
		return true;
	}
	if state == Live::Stalled {
		tracing::warn!(
			frames = ip.frames,
			flat_ms = ip.frames_at.elapsed().as_millis() as u64,
			"in-process x11 gst pipeline stopped encoding → full restart"
		);
	}
	false
}

/// The child absorb: `Some(running kbps)` when `req` only changes the bitrate of a live child
/// and `absorbs(running, new, since_start)` keeps it (the caller pins the pacer to that rate).
pub(super) fn child_absorbs(
	run: &Mutex<X11GstRun>,
	procs: &Mutex<Vec<Child>>,
	req: &StreamReq,
	absorbs: impl Fn(u32, u32, Duration) -> bool,
) -> Option<u32> {
	let (prev, at, pid) = run.lock().unwrap().child.clone()?;
	let mut probe = prev.clone();
	probe.bitrate_kbps = req.bitrate_kbps;
	if probe != *req || !absorbs(prev.bitrate_kbps, req.bitrate_kbps, at.elapsed()) {
		return None;
	}
	let pid = pid.load(Ordering::Relaxed);
	let alive = procs
		.lock()
		.unwrap()
		.iter_mut()
		.find(|c| c.id() == pid)
		.is_some_and(|c| matches!(c.try_wait(), Ok(None)));
	alive.then_some(prev.bitrate_kbps)
}

/// Full restart: retire the in-process pipeline (a `cap_gen` bump makes a start still running
/// drop what it brings up). The capture is taken in its OWN statement — the slot lock is
/// released at once (`on_nack`, `on_rfi` and the input path take it on every event) — and torn
/// down on a helper thread. The handle lets an in-process start join that teardown first.
pub(super) fn retire(
	run: &Mutex<X11GstRun>,
	cap_slot: &Mutex<Option<WaylandCapture>>,
	cap_gen: &AtomicU64,
) -> Option<std::thread::JoinHandle<()>> {
	cap_gen.fetch_add(1, Ordering::SeqCst);
	*run.lock().unwrap() = X11GstRun::default();
	let old = cap_slot.lock().unwrap().take();
	old.map(|cap| std::thread::spawn(move || drop(cap)))
}

/// The in-process pipeline's encode meter → the client's stats line ("… · 4.2 ms kodlama"),
/// the same line the `gst-launch` pace meter feeds. Runs on a GStreamer streaming thread
/// about once a second; `try_send` never blocks it.
fn encode_ms_reporter(stats: tokio::sync::mpsc::Sender<DataMsg>, label: String) -> pulsar_core::capture::EncodeMsFn {
	Box::new(move |ms: f32| {
		let _ = stats.try_send(DataMsg::Stats(format!("{label} · {ms:.1} {}", crate::i18n::t("stream.msEncode"))));
	})
}

/// The `gst-launch` child runner of one stream.
pub(super) struct X11GstChild {
	pub(super) procs: Arc<Mutex<Vec<Child>>>,
	pub(super) proc_epoch: crate::process::ProcEpoch,
	/// The `procs` generation of the stream this belongs to: once a drain bumped it, the stream
	/// moved on and nothing may be spawned for it any more.
	pub(super) stream_epoch: u64,
	/// The BGRx-direct variant (MPP via the RGA blitter), when the stream can take it.
	pub(super) desc_bgrx: Option<String>,
	/// The plain (CPU-convert / KMS / window) variant.
	pub(super) desc_plain: String,
	pub(super) stats: tokio::sync::mpsc::Sender<DataMsg>,
	pub(super) label: String,
}

impl X11GstChild {
	/// Spawn the child (BGRx-direct when `bgrx_ok` and the stream has it, with the RGA >4G
	/// watchdog that respawns the plain variant). `None`: the spawn failed, or the stream this
	/// belongs to was drained meanwhile. `PULSAR_GST_METER=0` bypasses the pace meter (plain
	/// `-q` spawn, no watchdog) — the regression-bisect knob for the encode-ms instrumentation.
	pub(super) fn spawn(&self, bgrx_ok: bool) -> Option<Arc<AtomicU32>> {
		let bgrx = self.desc_bgrx.as_ref().filter(|_| bgrx_ok && !BGRX_BROKEN.load(Ordering::Relaxed));
		let desc = bgrx.unwrap_or(&self.desc_plain);
		// Under the `procs` lock, against the drains (which bump the epoch under it).
		let mut g = self.procs.lock().unwrap();
		if self.proc_epoch.load(Ordering::SeqCst) != self.stream_epoch {
			return None;
		}
		if std::env::var("PULSAR_GST_METER").as_deref() == Ok("0") {
			return spawn_gst_tracked_locked(&mut g, desc).ok().map(|pid| Arc::new(AtomicU32::new(pid)));
		}
		let (stats, label) = (self.stats.clone(), self.label.clone());
		let (pid, ticked) = spawn_gst_paced_locked(&mut g, desc, move |ms| {
			let _ = stats.try_send(DataMsg::Stats(format!("{label} · {ms:.1} {}", crate::i18n::t("stream.msEncode"))));
		})
		.ok()?;
		drop(g);
		let pid_cell = Arc::new(AtomicU32::new(pid));
		if bgrx.is_some() {
			// RGA >4G watchdog. The BGRx-direct path hands ximagesrc's malloc'd pool straight to
			// the RGA blitter, and the RGA2 core can't map memory above 4 GiB phys (dmesg:
			// "RGA_MMU unsupported Memory larger than 4G!"). The pool is allocated once per
			// spawn, so it's allocation luck: same pipeline sometimes runs at 76 fps, sometimes
			// never emits a single frame (black screen). No first frame within 2.6 s → kill +
			// respawn with the CPU-convert (I420) variant, which feeds the encoder RGA-safe
			// buffers.
			let fb_pipeline = self.desc_plain.clone();
			let procs_fb = self.procs.clone();
			let stats_fb = self.stats.clone();
			let label_fb = self.label.clone();
			let pid_cell = pid_cell.clone();
			std::thread::spawn(move || {
				std::thread::sleep(FIRST_FRAME_WITHIN);
				if ticked.load(Ordering::Relaxed) {
					return;
				}
				// The whole kill-and-respawn runs under the procs lock, keyed to the ORIGINAL
				// child: teardown and a re-stream drain `procs` under this same lock, so once our
				// child is gone from it the session moved on — a fallback spawned past that point
				// would run as an untracked orphan (or duplicate the new encode's RTP).
				let mut g = procs_fb.lock().unwrap();
				let Some(idx) = g.iter().position(|c| c.id() == pid) else {
					return;
				};
				tracing::warn!(
					pid,
					"gst BGRx-direct produced no frames in 2.6 s (RGA >4G map failure) — respawning with CPU convert"
				);
				let mut old = g.remove(idx);
				let _ = old.kill();
				let _ = old.wait();
				if let Ok((new_pid, _)) = spawn_gst_paced_locked(&mut g, &fb_pipeline, move |ms| {
					let _ = stats_fb.try_send(DataMsg::Stats(format!(
						"{label_fb} · {ms:.1} {}",
						crate::i18n::t("stream.msEncode")
					)));
				}) {
					pid_cell.store(new_pid, Ordering::Relaxed);
				}
			});
		}
		Some(pid_cell)
	}
}

/// One in-process start, run on its own thread (see the module doc).
pub(super) struct X11InProcStart {
	pub(super) cap_slot: Arc<Mutex<Option<WaylandCapture>>>,
	pub(super) cap_gen: Arc<AtomicU64>,
	/// The `cap_gen` this start belongs to; a re-stream / teardown bumps it.
	pub(super) my_gen: u64,
	pub(super) x11_run: Arc<Mutex<X11GstRun>>,
	/// The previous pipeline's teardown, joined before this one starts.
	pub(super) teardown: Option<std::thread::JoinHandle<()>>,
	/// In-process descriptions (NORMAL GOP: recovery runs through the key-unit timer only).
	pub(super) desc_bgrx: Option<String>,
	pub(super) desc_plain: String,
	/// The request it starts with (its bitrate is baked into the descriptions).
	pub(super) start_req: StreamReq,
	pub(super) stats: tokio::sync::mpsc::Sender<DataMsg>,
	pub(super) label: String,
	/// The fallback: the `gst-launch` child of the same stream.
	pub(super) child: X11GstChild,
	pub(super) app_h: AppHandle,
	pub(super) peer: String,
	pub(super) sid: u64,
	/// "MPP · 1080p" — the session line, for a failure notice.
	pub(super) detail: String,
}

impl X11InProcStart {
	fn current(&self) -> bool {
		self.cap_gen.load(Ordering::SeqCst) == self.my_gen
	}

	pub(super) fn run(mut self) {
		if let Some(h) = self.teardown.take() {
			let _ = h.join();
		}
		if !self.current() {
			return;
		}
		let marker = marker_path();
		let _ = std::fs::write(&marker, b"in-process gst start\n");
		let mut variants: Vec<(String, bool)> = Vec::with_capacity(2);
		if let Some(d) = self.desc_bgrx.clone().filter(|_| !BGRX_BROKEN.load(Ordering::Relaxed)) {
			variants.push((d, true));
		}
		variants.push((self.desc_plain.clone(), false));
		let mut bgrx_failed = false;
		let mut outcome: Result<WaylandCapture, StartError> = Err(StartError::NoFirstFrame);
		for (desc, bgrx) in variants {
			if !self.current() {
				let _ = std::fs::remove_file(&marker);
				return;
			}
			let reporter = encode_ms_reporter(self.stats.clone(), self.label.clone());
			outcome = pulsar_core::capture::start_pipeline(&desc, Some(reporter), FIRST_FRAME_WITHIN);
			match &outcome {
				Ok(_) => break,
				// Any BGRx-direct failure tries the CPU-convert variant next (an RGA import
				// error is as BGRx-specific as the silent >4G case).
				Err(e) if bgrx => {
					tracing::warn!("in-process gst BGRx-direct failed ({e}) — trying the CPU-convert variant");
					bgrx_failed = true;
				}
				Err(_) => {}
			}
		}
		match outcome {
			Ok(cap) => {
				INPROC_FAILS.store(0, Ordering::Relaxed);
				if bgrx_failed {
					tracing::warn!("BGRx-direct off for this process (RGA >4G); CPU convert from now on");
					BGRX_BROKEN.store(true, Ordering::Relaxed);
				}
				let mut run = self.x11_run.lock().unwrap();
				let mut slot = self.cap_slot.lock().unwrap();
				let stale = !self.current() || slot.is_some();
				match run.inproc.as_mut().filter(|_| !stale) {
					Some(ip) => {
						// Requests that came in while it started (absorbed by the fast path).
						if ip.req.bitrate_kbps > 0 && ip.req.bitrate_kbps != self.start_req.bitrate_kbps {
							cap.set_bitrate(ip.req.bitrate_kbps);
						}
						cap.set_short_gop(ip.req.loss_recovery.is_active());
						ip.starting = false;
						ip.frames = cap.frames_encoded();
						ip.frames_at = Instant::now();
						*slot = Some(cap);
						drop(slot);
						drop(run);
						tracing::info!(bgrx = !bgrx_failed && self.desc_bgrx.is_some(), "in-process x11 gst pipeline up");
						// Release the session handles (the stats sender's close ends the cursor
						// poller) before waiting out the marker hold.
						drop(self);
						std::thread::sleep(MARKER_HOLD);
						let _ = std::fs::remove_file(&marker);
					}
					None => {
						// The session moved on while it started: not ours to store.
						drop(slot);
						drop(run);
						drop(cap);
						let _ = std::fs::remove_file(&marker);
					}
				}
			}
			Err(e) => {
				let _ = std::fs::remove_file(&marker);
				if !matches!(e, StartError::Unavailable(_)) {
					let n = INPROC_FAILS.fetch_add(1, Ordering::Relaxed) + 1;
					if n >= LATCH_AFTER {
						tracing::warn!(n, "in-process x11 gst failed {n} starts in a row — gst-launch for this process");
						INPROC_BROKEN.store(true, Ordering::Relaxed);
					}
				}
				if !self.current() {
					return;
				}
				tracing::warn!("in-process x11 gst pipeline failed ({e}) — falling back to gst-launch");
				// The child gets BGRx-direct only when that wasn't what just failed.
				let pid = self.child.spawn(!bgrx_failed);
				let mut run = self.x11_run.lock().unwrap();
				if !self.current() {
					return;
				}
				run.inproc = None;
				run.child = pid.clone().map(|p| (self.start_req.clone(), Instant::now(), p));
				drop(run);
				if pid.is_none() {
					let _ = self.app_h.emit(
						"session",
						SessionEvent {
							kind: "stream".into(),
							peer: self.peer.clone(),
							sid: self.sid,
							detail: format!("{}{}", self.detail, crate::i18n::t("host.gstFailed")),
						},
					);
				}
			}
		}
	}
}

/// `gst-launch-1.0 -v` spawn with the ENCODE-PACE meter, `procs` lock already held (the RGA
/// watchdog needs its find-dead → kill → respawn → push sequence atomic against the
/// teardown/re-stream drains, which run under this same lock). The pipeline carries an
/// `identity name=encpace silent=false` right after the encoder, and `-v` makes gst-launch
/// print one `last-message = chain …` line per encoded frame. The parser thread times the line
/// gaps (Δwall per frame ≈ encode pace, same semantics as the ffmpeg `-progress` meter) and
/// calls `on_ms` about once a second — so a gst host (Pi MPP) feeds the client's "Kodlama ms"
/// tile. Returns the child's pid + a flag that flips on the FIRST encoded frame — the caller's
/// no-output watchdog reads it (the BGRx-direct RGA fallback).
fn spawn_gst_paced_locked(
	procs: &mut Vec<Child>,
	pipeline: &str,
	on_ms: impl Fn(f32) + Send + 'static,
) -> Result<(u32, Arc<AtomicBool>), String> {
	use std::os::unix::process::CommandExt;
	let mut cmd = std::process::Command::new(crate::process::gst_launch_bin());
	pulsar_core::capture::use_system_gst_env(&mut cmd);
	cmd.arg("-v").args(pipeline.split_whitespace());
	cmd.stdout(std::process::Stdio::piped());
	// stderr to a per-host scratch file (truncated each spawn): gst dying at start
	// is otherwise invisible — this is the only channel that says WHY.
	let errlog = std::fs::File::create(std::env::temp_dir().join("pulsar-gst-stderr.log")).ok();
	match errlog {
		Some(f) => {
			cmd.stderr(f);
		}
		None => {
			cmd.stderr(std::process::Stdio::null());
		}
	}
	unsafe {
		cmd.pre_exec(|| {
			// SAFETY: async-signal-safe libc calls only.
			libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong, 0, 0, 0);
			if libc::getppid() == 1 {
				libc::_exit(0);
			}
			Ok(())
		});
	}
	let ticked = Arc::new(AtomicBool::new(false));
	match cmd.spawn() {
		Ok(mut child) => {
			if let Some(mut stdout) = child.stdout.take() {
				let ticked = ticked.clone();
				std::thread::spawn(move || {
					use std::io::Read;
					// BYTE-safe line scan — `BufRead::lines()` returns Err on any
					// non-UTF-8 byte in the `-v` dump and a `break` there DROPS the
					// pipe: gst-launch then dies of SIGPIPE on its next print (the
					// "video never starts" regression). This reader only ends at EOF,
					// i.e. when gst itself exited.
					let mut buf = [0u8; 8192];
					let mut line: Vec<u8> = Vec::with_capacity(256);
					let mut last: Option<Instant> = None;
					let mut ema_ms: f32 = 0.0;
					let mut last_push = Instant::now();
					loop {
						let n = match stdout.read(&mut buf) {
							Ok(0) | Err(_) => break,
							Ok(n) => n,
						};
						for &b in &buf[..n] {
							if b != b'\n' {
								// Cap pathological unterminated lines; the marker fits well within.
								if line.len() < 4096 {
									line.push(b);
								}
								continue;
							}
							let is_tick = {
								let s = String::from_utf8_lossy(&line);
								s.contains("encpace") && s.contains("last-message")
							};
							line.clear();
							if !is_tick {
								continue;
							}
							ticked.store(true, Ordering::Relaxed);
							let now = Instant::now();
							if let Some(t0) = last {
								let ms = now.duration_since(t0).as_secs_f32() * 1000.0;
								ema_ms = if ema_ms == 0.0 { ms } else { ema_ms * 0.9 + ms * 0.1 };
								if last_push.elapsed().as_millis() >= 1000 {
									on_ms(ema_ms);
									last_push = now;
								}
							}
							last = Some(now);
						}
					}
				});
			}
			let pid = child.id();
			procs.push(child);
			Ok((pid, ticked))
		}
		Err(e) => Err(format!("gst-launch-1.0 {}: {e}", crate::i18n::t("err.spawn"))),
	}
}

/// Spawn a plain (`-q`, unmetered) `gst-launch-1.0` encode pipeline tracked in `procs` (killed
/// on (re-)stream/teardown like the ffmpeg children), `procs` lock already held; returns its
/// pid. `PR_SET_PDEATHSIG` mirrors the Wayland capture spawn so an app crash can never orphan
/// a streaming encoder.
fn spawn_gst_tracked_locked(procs: &mut Vec<Child>, pipeline: &str) -> Result<u32, String> {
	use std::os::unix::process::CommandExt;
	let mut cmd = std::process::Command::new(crate::process::gst_launch_bin());
	pulsar_core::capture::use_system_gst_env(&mut cmd);
	cmd.arg("-q").args(pipeline.split_whitespace());
	unsafe {
		cmd.pre_exec(|| {
			// SAFETY: async-signal-safe libc calls only.
			libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong, 0, 0, 0);
			if libc::getppid() == 1 {
				libc::_exit(0);
			}
			Ok(())
		});
	}
	match cmd.spawn() {
		Ok(child) => {
			let pid = child.id();
			procs.push(child);
			Ok(pid)
		}
		Err(e) => Err(format!("gst-launch-1.0 {}: {e}", crate::i18n::t("err.spawn"))),
	}
}
