//! Pulsar native host capture+encode crate (Windows-only).
//!
//! Replaces the host `ddagrab → h264_nvenc` ffmpeg path with native DXGI Desktop
//! Duplication capture → the NVENC SDK → a hand-rolled RTP packetizer (the Sunshine
//! technique, no ffmpeg). ffmpeg stays as the fallback: this crate is *only* engaged
//! when the host's NVENC branch is selected, and any init failure makes the host fall
//! straight back to ffmpeg.
//!
//! ## Crate layout (the seam three implementers agree on)
//! - `lib.rs`  (this file) — public API + the capture/encode thread + the init handshake,
//!   and the **shared types** (`Frame`) that `dxgi.rs` ↔ `encode.rs` exchange.
//! - `dxgi.rs` — `CaptureDevice` (owns the D3D11 device) + the Sunshine pacing loop.
//! - `encode.rs` — `Encoder` (borrows the device) — NVENC SDK session + `rtp.rs` packetizer.
//!
//! The whole crate is gated `#![cfg(windows)]`-style: on non-Windows targets every public
//! item degrades to a stub so the Cargo workspace still builds everywhere (the host only
//! *calls* into it under `#[cfg(windows)]`, but keeping the symbols present means the crate
//! can live in `members` unconditionally without breaking `cargo check` on mac/Linux).
#![allow(clippy::missing_safety_doc)]

#[cfg(not(target_os = "macos"))]
use std::io;

// ===========================================================================
// Public, target-independent API surface (present on every platform)
// ===========================================================================

/// Everything the native path needs, derived 1:1 from `on_stream`'s effective values.
#[derive(Clone, Debug)]
pub struct CaptureConfig {
	/// `eff_w`  — 0 ⇒ use the duplicated output's native width.
	pub width: u32,
	/// `eff_h`  — 0 ⇒ native height.
	pub height: u32,
	/// `eff_fps` — client cadence; drives the pacing loop.
	pub fps: u32,
	/// `eff_bitrate` in kbps.
	pub bitrate_kbps: u32,
	/// e.g. `"rtp://10.0.0.5:9000"` — built by the caller (same string as `plan.dest`).
	pub dest: String,
	/// `H264` | `H265` | `Av1`. v1 implements H264; others should be rejected by the
	/// caller so the host takes the ffmpeg branch.
	pub codec: Codec,
	/// Monitor index, 0 (matches `ddagrab=output_idx=0`). Ignored when `window_hwnd` is set
	/// (the per-window WGC source captures a window, not a monitor).
	pub output_idx: u32,
	/// `plan.low_latency` → preset/tune/rc selection.
	pub low_latency: bool,
	/// Periodic intra refresh instead of scheduled IDRs (adaptive streaming: a lost
	/// reference heals within one refresh wave, no keyframe bitrate spike). Only for clients
	/// that resume on a partially-refreshed picture (`StreamReq::loss_recovery ==
	/// IntraRefresh`); the on-demand IDR (`request_idr`) keeps working either way.
	pub intra_refresh: bool,
	/// Reference-frame invalidation (`StreamReq::rfi`): a deeper reference buffer, and a loss
	/// the client reports (`CaptureHandle::request_rfi`) is healed without a keyframe.
	pub rfi: bool,
	/// Composite the cursor (v1: parsed but no-op unless `cursor.rs` landed).
	pub draw_mouse: bool,
	/// PER-WINDOW capture (Phase 2b): when `Some(hwnd)`, the native path captures that single
	/// window via Windows Graphics Capture (`wgc::WgcCaptureDevice`) instead of duplicating a
	/// whole monitor via DXGI — letting two panes share one monitor / target two app windows
	/// (the same-monitor co-op case). `None` (default) ⇒ the existing full-monitor DXGI source.
	/// The HWND is a raw window handle (`HWND.0 as isize`); the entire encode/RTP path is
	/// identical either way. Has no effect on non-Windows (the native path is Windows-only).
	pub window_hwnd: Option<isize>,
}

/// Video codec selector. The native NVENC path implements all three (`H264`, `H265`,
/// `Av1`); AV1 encode needs an Ada/Ampere+ NVENC (RTX 40) — on older GPUs init fails
/// cleanly and the host falls back to ffmpeg.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
	H264,
	H265,
	Av1,
}

/// Live capture-path numbers, refreshed about once a second by the capture thread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CaptureStats {
	/// `"dxgi"` (Desktop Duplication), `"wgc"` (whole monitor via Windows Graphics Capture),
	/// `"wgc-window"` (one window) — all NVENC — or `"dxgi-mf"` (Desktop Duplication + the
	/// display GPU's own encoder through Media Foundation); `"sck"` on macOS (ScreenCaptureKit +
	/// VideoToolbox); `""` before the first build.
	pub source: &'static str,
	/// A fresh frame composed → its access unit handed to RTP (capture wait + any cross-GPU
	/// hop + colour convert + encode): median / 90th percentile in µs. 0 on a static screen.
	pub latency_p50_us: u32,
	pub latency_p90_us: u32,
	/// One encode tick on the capture thread (hop + convert + NVENC), µs.
	pub submit_p50_us: u32,
	pub submit_p90_us: u32,
	/// Frames with new content encoded per second (the others re-encode the last surface).
	pub fresh_per_s: u32,
	/// Client-reported holes healed by reference-frame invalidation / left to the client (the
	/// lost frame was out of reach: its NACK repair, or else its keyframe request, heals it).
	/// Cumulative for the capture's life.
	pub rfi_recovered: u32,
	pub rfi_declined: u32,
	/// Access units encoded for the capture's life (wraps). 0 a few seconds into a stream =
	/// the source never delivered a frame (see the no-frame watchdog in `start`).
	pub encoded: u32,
}

/// The atomics behind [`CaptureStats`], written by the capture thread.
#[derive(Debug, Default)]
pub struct LiveStats {
	pub(crate) source: std::sync::atomic::AtomicU32,
	pub(crate) lat_p50: std::sync::atomic::AtomicU32,
	pub(crate) lat_p90: std::sync::atomic::AtomicU32,
	pub(crate) sub_p50: std::sync::atomic::AtomicU32,
	pub(crate) sub_p90: std::sync::atomic::AtomicU32,
	pub(crate) fresh_per_s: std::sync::atomic::AtomicU32,
	pub(crate) rfi_ok: std::sync::atomic::AtomicU32,
	pub(crate) rfi_no: std::sync::atomic::AtomicU32,
	/// Access units handed to the encoder without error, for the capture's life (wraps). The
	/// thread re-encodes the last surface on a static screen, so this keeps rising on a healthy
	/// pipeline; a flat count means the thread is stuck (see [`ProgressWatch`]).
	pub(crate) encoded: std::sync::atomic::AtomicU32,
}

/// Tells how long a monotonic counter has been flat. The host feeds it the capture thread's
/// encoded-frame count on each restream request: a pipeline that encodes nothing for seconds
/// is wedged (2026-11-11: both host processes' capture + NVENC threads froze inside the GPU
/// for 43 s and 81 s), and a live fast path that only writes atomics into it cannot help.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProgressWatch {
	last: Option<(u32, std::time::Instant)>,
}

impl ProgressWatch {
	/// Record `count` seen at `now`; returns how long it has been unchanged (zero on a change
	/// or the first sample).
	pub fn observe(&mut self, count: u32, now: std::time::Instant) -> std::time::Duration {
		match self.last {
			Some((c, since)) if c == count => now.saturating_duration_since(since),
			_ => {
				self.last = Some((count, now));
				std::time::Duration::ZERO
			}
		}
	}

	/// Forget the history (a new pipeline starts its own count).
	pub fn reset(&mut self) {
		self.last = None;
	}
}

impl LiveStats {
	pub fn snapshot(&self) -> CaptureStats {
		use std::sync::atomic::Ordering::Relaxed;
		CaptureStats {
			source: match self.source.load(Relaxed) {
				1 => "dxgi",
				2 => "wgc",
				3 => "wgc-window",
				4 => "dxgi-mf",
				5 => "sck",
				_ => "",
			},
			latency_p50_us: self.lat_p50.load(Relaxed),
			latency_p90_us: self.lat_p90.load(Relaxed),
			submit_p50_us: self.sub_p50.load(Relaxed),
			submit_p90_us: self.sub_p90.load(Relaxed),
			fresh_per_s: self.fresh_per_s.load(Relaxed),
			rfi_recovered: self.rfi_ok.load(Relaxed),
			rfi_declined: self.rfi_no.load(Relaxed),
			encoded: self.encoded.load(Relaxed),
		}
	}
}

/// Owns the capture+encode thread. `stop()` or `Drop` tears everything down deterministically
/// (releases the NVENC session + DXGI duplication by joining the thread).
pub struct CaptureHandle {
	stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
	thread: Option<std::thread::JoinHandle<()>>,
	/// Stage-3 adaptive bitrate: the host controller writes the target kbps here; the capture
	/// thread reads it each tick and applies it live via `Encoder::reconfigure_bitrate` on change.
	requested_kbps: std::sync::Arc<std::sync::atomic::AtomicU32>,
	/// Live MONITOR switch: the host writes the desired output index here; the capture thread
	/// polls it each tick and, on change, rebuilds capture+encode on the new monitor in THIS
	/// thread (the new monitor may be on a different GPU — see start_capture_encode), instead of
	/// the host tearing the whole pipeline + audio down and re-spawning a fresh process-side
	/// thread. `u32::MAX` = no pending request (the sentinel).
	requested_output: std::sync::Arc<std::sync::atomic::AtomicU32>,
	/// The output index the capture thread is ACTUALLY streaming right now, written by the thread
	/// after each successful build (including reverts). The host reads this to know the confirmed
	/// capture output, which may differ from the REQUESTED output when a switch-build failed and
	/// the thread reverted to `prev_good_output`. Used by the host to keep `cur_display`
	/// (input-mapping) and `last_native_req.display_idx` (fast-path baseline) in sync with
	/// reality so input lands on the right monitor and a switch retry is not a silent no-op.
	current_output: std::sync::Arc<std::sync::atomic::AtomicU32>,
	/// Monotonic build counter: incremented by the capture thread after EVERY successful build
	/// (including same-index resolution-change rebuilds). The host's input path tracks this to
	/// detect a host-side resolution change and re-resolve the monitor geometry via
	/// `display_rect()` / `set_monitor()` even when the monitor INDEX is unchanged (C8).
	build_gen: std::sync::Arc<std::sync::atomic::AtomicU32>,
	/// EWMA of per-frame encode wall-time in microseconds (G2): the capture thread stores it
	/// around each `enc.submit`; the host reads it (`encode_us_arc`) to push the overlay's
	/// "kodlama" stat, which the native NVENC path otherwise lacked (only ffmpeg metered it).
	encode_us: std::sync::Arc<std::sync::atomic::AtomicU32>,
	/// One-shot client keyframe request (`MediaNack([0])` sentinel): the host sets it via
	/// `request_idr()`; the capture thread polls+clears it each tick and forces the next
	/// encoded frame to be an IDR. Without this, a client whose decoder was rebuilt
	/// mid-session stays frozen until the 10 s safety GOP.
	requested_idr: std::sync::Arc<std::sync::atomic::AtomicBool>,
	/// Liveness flag: `true` while the capture thread is running, cleared (via an RAII guard) when
	/// the thread exits for ANY reason — a both-outputs-unbuildable death (lib.rs revert-build
	/// failure), a `RunExit::Stop` from an unrecoverable reinit, or a panic. The host polls this on
	/// its live-restream fast path (`switch_output`/`set_bitrate`): once the thread is dead those
	/// calls only write atomics no thread reads, so a switch/bitrate restream would silently no-op
	/// on a dead pipeline. Seeing `is_alive()==false` lets the host fall through to a full capture
	/// restart instead. Distinct from `stop`: `stop` is host→thread intent; this is thread→host state.
	alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
	/// Live capture-path numbers (source, present→encoded latency, encode tick, fresh fps).
	stats: std::sync::Arc<LiveStats>,
	/// A client-reported loss (`request_rfi`), consumed by the capture thread before its next
	/// encode. Ranges arriving before it runs are merged.
	requested_rfi: std::sync::Arc<std::sync::Mutex<Option<(u16, u16)>>>,
}

impl CaptureHandle {
	/// The client saw the video packets `first..=last` missing (reported at once, while its NACK
	/// repair may still fill the hole): invalidate the frames they carried as references on the
	/// next encode, so the next frame is a tagged recovery point. A frame out of reach is left
	/// alone — the client then asks for a keyframe if the repair fails.
	pub fn request_rfi(&self, first: u16, last: u16) {
		let mut r = self.requested_rfi.lock().unwrap();
		*r = Some(match *r {
			Some((f, _)) => (f, last),
			None => (first, last),
		});
	}

	/// The capture path's live numbers.
	pub fn stats(&self) -> CaptureStats {
		self.stats.snapshot()
	}

	/// A clone of the live-stats Arc, to read without holding the handle.
	pub fn stats_arc(&self) -> std::sync::Arc<LiveStats> {
		self.stats.clone()
	}

	/// Set the live target bitrate (kbps). Lock-free: stores into the atom the capture thread
	/// polls each tick — never blocks the caller or stalls encode. No-op if unchanged.
	pub fn set_bitrate(&self, kbps: u32) {
		self.requested_kbps
			.store(kbps.max(1), std::sync::atomic::Ordering::SeqCst);
	}

	/// Switch which host monitor is captured, live (session-menu picker). Lock-free: stores the
	/// output index the capture thread picks up next tick and rebuilds capture+encode on it in
	/// the SAME thread (correct across GPUs — see the field doc) — no new OS thread, no init-
	/// handshake wait, and the host leaves audio + forwarders running. No-op if it equals the
	/// current output.
	pub fn switch_output(&self, idx: u32) {
		self.requested_output
			.store(idx, std::sync::atomic::Ordering::SeqCst);
	}

	/// Force the next encoded frame to be an IDR (client keyframe request — the
	/// `MediaNack([0])` sentinel). Lock-free one-shot; the capture thread consumes it
	/// on its next tick.
	pub fn request_idr(&self) {
		self.requested_idr
			.store(true, std::sync::atomic::Ordering::SeqCst);
	}

	/// The output index the capture thread is ACTUALLY streaming right now (written by the thread
	/// after every confirmed build, including reverts). May lag the last `switch_output` request
	/// by one build cycle while the thread is rebuilding. The host uses this to keep its
	/// input-mapping (`cur_display`) and fast-path baseline (`last_native_req.display_idx`) in
	/// sync with the actual streamed monitor rather than the optimistically-requested one.
	pub fn current_output(&self) -> u32 {
		self.current_output
			.load(std::sync::atomic::Ordering::SeqCst)
	}

	/// A clone of the Arc<AtomicU32> the capture thread writes after every confirmed build
	/// (including reverts). Callers that need to track the live confirmed output across a
	/// switch — in particular the input-mapping path — can clone this Arc once and then
	/// poll it directly without locking the CaptureHandle or the native_slot mutex. The
	/// atom's value is always the monitor the thread is ACTUALLY streaming, never the
	/// optimistically-requested one, so reading it from on_input gives the correct rect for
	/// absolute-pointer injection even during a mid-session monitor switch.
	pub fn current_output_arc(&self) -> std::sync::Arc<std::sync::atomic::AtomicU32> {
		self.current_output.clone()
	}

	/// A clone of the build-generation Arc. The capture thread increments this after every
	/// successful build — including same-index resolution-change rebuilds (C8). The host's
	/// input closure tracks this alongside the output index: when the generation advances the
	/// closure re-calls `display_rect(idx)` / `set_monitor()` to pick up the new monitor
	/// geometry even though the monitor INDEX has not changed.
	pub fn build_gen_arc(&self) -> std::sync::Arc<std::sync::atomic::AtomicU32> {
		self.build_gen.clone()
	}

	/// A clone of the encode-time EWMA atom (microseconds). The host reads it ~2 Hz to push
	/// the overlay's encode-ms stat for the native NVENC path (G2). 0 = no frame timed yet.
	pub fn encode_us_arc(&self) -> std::sync::Arc<std::sync::atomic::AtomicU32> {
		self.encode_us.clone()
	}

	/// Whether the capture+encode thread is still running. Becomes `false` once the thread exits
	/// for any reason (unbuildable output, unrecoverable reinit, or panic). The host checks this on
	/// its live-restream fast path so a `switch_output`/`set_bitrate` restream against a DEAD thread
	/// falls through to a full capture restart instead of silently writing atomics no thread reads.
	pub fn is_alive(&self) -> bool {
		self.alive.load(std::sync::atomic::Ordering::SeqCst)
	}

	/// A clone of the liveness Arc (see [`is_alive`]) so a caller can poll the capture thread's
	/// liveness without holding the `CaptureHandle` / its owning lock.
	pub fn alive_arc(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
		self.alive.clone()
	}

	/// Access units encoded so far (wrapping). Feed it to a [`ProgressWatch`] to spot a stuck
	/// capture thread.
	pub fn frames_encoded(&self) -> u32 {
		self.stats.encoded.load(std::sync::atomic::Ordering::Relaxed)
	}

	/// Signal the thread and wait at most `wait` for it to exit. Returns `true` when it exited
	/// and was joined. When it is stuck inside a driver call, it is detached instead (returns
	/// `false`): it sees the stop flag and releases its NVENC session + capture once the call
	/// returns, and the caller is not blocked behind it.
	pub fn stop_within(mut self, wait: std::time::Duration) -> bool {
		use std::sync::atomic::Ordering::SeqCst;
		self.stop.store(true, SeqCst);
		let t0 = std::time::Instant::now();
		while self.alive.load(SeqCst) && t0.elapsed() < wait {
			std::thread::sleep(std::time::Duration::from_millis(10));
		}
		let exited = !self.alive.load(SeqCst);
		if let Some(t) = self.thread.take() {
			if exited {
				let _ = t.join();
			}
			// else: dropping the JoinHandle detaches the thread.
		}
		exited
	}

	/// Signal the thread, join it (releases the NVENC session + DXGI duplication), return.
	pub fn stop(mut self) {
		self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
		if let Some(t) = self.thread.take() {
			let _ = t.join();
		}
	}
}

impl Drop for CaptureHandle {
	fn drop(&mut self) {
		self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
		if let Some(t) = self.thread.take() {
			let _ = t.join();
		}
	}
}

// Pure-Rust RTP/H.264 packetizer (RFC 6184), byte-for-byte compatible with the
// client depacketizer in `src/lib/h264.ts`. Platform-independent (std::net + rand),
// so it builds and is unit-tested on every target.
pub mod rtp;
pub use rtp::{RtpEgress, RtpSender};

// Top-level window enumeration for the per-window (WGC) capture target (Phase 2b co-op).
// Cross-platform module: Windows enumerates real windows; the non-Windows arms are stubs so
// the workspace builds everywhere and the host uses its display path off Windows.
pub mod window;

// macOS native path: ScreenCaptureKit (pulsar-core) → VideoToolbox → `rtp.rs`. The pure parts
// (Annex B rewrite, sizes/rates/timestamps) build and are unit-tested on every target.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod mac;
#[cfg(target_os = "macos")]
pub use mac::{display_rect, list_displays, start_capture_encode};
pub use window::{
	find_window_for_launch, list_windows as list_capture_windows, window_pid, WindowDesc,
};

// ===========================================================================
// Windows implementation
// ===========================================================================
#[cfg(windows)]
pub(crate) mod dxgi;
#[cfg(windows)]
mod encode;
#[cfg(windows)]
pub use encode::{gpu_vendors, nvenc_codecs, nvenc_supported};
/// Rare diagnostic lines (capture source picks, probe verdicts) — the encoder's init log file.
#[cfg(windows)]
pub(crate) use encode::cap_dbg as encode_log;
// Which full-monitor source (DXGI / WGC) a build uses, and the measurement that picks it.
#[cfg(windows)]
mod path;
// Process-wide GPU scheduling priority class for the capture/encode work.
#[cfg(windows)]
mod gpu_sched;
// Glass-to-encode measurement hooks for the capbench example (inert unless PULSAR_BENCH_PIXEL=1).
#[cfg(windows)]
pub mod bench;
/// Off-Windows stubs — the native NVENC SDK path is Windows-only.
#[cfg(not(windows))]
pub fn nvenc_supported() -> bool {
	false
}
#[cfg(not(windows))]
pub fn nvenc_codecs() -> &'static [Codec] {
	&[]
}
/// Off Windows nothing asks (the GPU vendor gate is for the Windows encoder probes).
#[cfg(not(windows))]
pub fn gpu_vendors() -> Vec<u32> {
	Vec::new()
}
// Per-window Windows Graphics Capture source (Phase 2b) — an alternative to `dxgi` that
// produces the same per-frame BGRA ID3D11Texture2D the encoder consumes (see wgc.rs).
#[cfg(windows)]
pub(crate) mod wgc;

// Shared frame type lives here so both dxgi.rs (producer) and encode.rs (consumer) import
// it from the crate root and cannot drift. Only meaningful on Windows (it borrows D3D11/DXGI
// interface types), so it is itself Windows-gated.
#[cfg(windows)]
pub use frame::Frame;

#[cfg(windows)]
mod frame {
	use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
	use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;

	/// One captured surface handed from `dxgi.rs` → `encode.rs` each paced tick.
	///
	/// The texture is owned by `dxgi.rs`'s pool and only valid for the duration of the
	/// `on_frame` callback. Because the pacing loop calls `on_frame` synchronously and
	/// `Encoder::submit` runs to completion before the loop advances, the borrow never
	/// outlives the producer's hold on the texture — so in v1 no keyed mutex is required
	/// (capture and encode share one device on one thread). See the locking note in dxgi.rs.
	pub struct Frame<'a> {
		/// Pool BGRA texture (`DEFAULT`, `RENDER_TARGET | SHADER_RESOURCE`); the source for
		/// the BGRA→NV12 `VideoProcessorBlt` that `encode.rs` performs.
		pub texture: &'a ID3D11Texture2D,
		/// `DXGI_FORMAT_B8G8R8A8_UNORM` (SDR). HDR is deferred to the ffmpeg fallback in v1.
		pub format: DXGI_FORMAT,
		/// Post-rotation presented width.
		pub width: u32,
		/// Post-rotation presented height.
		pub height: u32,
		/// `false` on the timeout-reuse path (the desktop was static — we still re-encode the
		/// last surface so the client sees a steady fps and NVENC emits a tiny P-frame).
		pub is_new: bool,
		/// Set on the first frame after a SAME-resolution duplication reinit (host Hz change /
		/// transient ACCESS_LOST). The encoder forces an IDR (+ SPS/PPS) for this frame so a
		/// client mid-GOP re-syncs immediately instead of freezing until the next safety GOP.
		pub force_idr: bool,
		/// When this content was composed (QPC ns, the `Qpc::now_ns` scale): DXGI's
		/// `LastPresentTime` / WGC's `SystemRelativeTime`. 0 on a reused surface or when
		/// unknown. Feeds the present→encoded latency (live stats + the source probe).
		pub present_ns: i64,
	}
}

/// The capture source the encode loop reads from: a full-monitor DXGI duplication, or a
/// per-window WGC source (Phase 2b). Both expose the same shape (`device`/`context` for the
/// encoder, `target_size`/`native_size`/`rotation_deg` for encode sizing, and a `run` pacing
/// loop), so the encode loop forwards through this enum and stays source-agnostic. Built once
/// per outer rebuild from `CaptureConfig::window_hwnd` (`Some` ⇒ WGC, `None` ⇒ DXGI).
#[cfg(windows)]
enum CapSource {
	Dxgi(dxgi::CaptureDevice),
	Wgc(wgc::WgcCaptureDevice),
}

#[cfg(windows)]
impl CapSource {
	/// The D3D11 device the encoder opens its NVENC session on (encode.rs AddRefs it).
	fn device(&self) -> &windows::Win32::Graphics::Direct3D11::ID3D11Device {
		match self {
			CapSource::Dxgi(c) => &c.device,
			CapSource::Wgc(c) => &c.device,
		}
	}
	/// The shared immediate context (encode's pre-Blt copy runs on it).
	fn context(&self) -> &windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext {
		match self {
			CapSource::Dxgi(c) => &c.context,
			CapSource::Wgc(c) => &c.context,
		}
	}
	fn target_size(&self, w: u32, h: u32) -> (u32, u32) {
		match self {
			CapSource::Dxgi(c) => c.target_size(w, h),
			CapSource::Wgc(c) => c.target_size(w, h),
		}
	}
	fn native_size(&self) -> (u32, u32) {
		match self {
			CapSource::Dxgi(c) => c.native_size(),
			CapSource::Wgc(c) => c.native_size(),
		}
	}
	fn rotation_deg(&self) -> u32 {
		match self {
			CapSource::Dxgi(c) => c.rotation_deg(),
			CapSource::Wgc(c) => c.rotation_deg(),
		}
	}
	/// Forward the pacing loop to the underlying source. Both `run` implementations share the
	/// exact signature (fps, draw_cursor, requested_output, stop, on_frame) and return `RunExit`.
	unsafe fn run(
		&mut self,
		fps: u32,
		draw_mouse: bool,
		req_out: &std::sync::atomic::AtomicU32,
		rebuild: &std::sync::atomic::AtomicBool,
		stop: &std::sync::atomic::AtomicBool,
		on_frame: impl FnMut(&Frame),
	) -> dxgi::RunExit {
		match self {
			CapSource::Dxgi(c) => c.run(fps, draw_mouse, req_out, rebuild, stop, on_frame),
			CapSource::Wgc(c) => c.run(fps, draw_mouse, req_out, rebuild, stop, on_frame),
		}
	}
}

/// The encoder a build streams with: NVENC, or the display GPU's own via Media Foundation.
#[cfg(windows)]
enum AnyEncoder {
	Nv(encode::Encoder),
	Mf(encode::mf::MfEncoder),
}

#[cfg(windows)]
impl AnyEncoder {
	unsafe fn submit(&mut self, frame: &Frame, pts: i64, rtp_ts: u32) -> Result<(), String> {
		match self {
			AnyEncoder::Nv(e) => e.submit(frame, pts, rtp_ts),
			AnyEncoder::Mf(e) => e.submit(frame, pts, rtp_ts),
		}
	}
	unsafe fn reconfigure_bitrate(&mut self, kbps: u32) -> Result<(), String> {
		match self {
			AnyEncoder::Nv(e) => e.reconfigure_bitrate(kbps),
			AnyEncoder::Mf(e) => e.reconfigure_bitrate(kbps),
		}
	}
	fn request_idr(&mut self) {
		match self {
			AnyEncoder::Nv(e) => e.request_idr(),
			AnyEncoder::Mf(e) => e.request_idr(),
		}
	}
	unsafe fn invalidate_lost(&mut self, first: u16, last: u16) -> Result<usize, &'static str> {
		match self {
			AnyEncoder::Nv(e) => e.invalidate_lost(first, last),
			AnyEncoder::Mf(e) => e.invalidate_lost(first, last),
		}
	}
	unsafe fn flush_and_close(&mut self) {
		match self {
			AnyEncoder::Nv(e) => e.flush_and_close(),
			AnyEncoder::Mf(e) => e.flush_and_close(),
		}
	}
}

/// Spawn the dedicated capture+encode thread.
///
/// All fallible init (DXGI device + duplication + NVENC open + RTP header) happens *inside*
/// the thread, but its `Result` is sent back over a bounded channel that this function blocks
/// on (≤ ~3s). So `Ok(handle)` means "streaming has started"; `Err(e)` means "init failed —
/// the host should fall back to ffmpeg".
#[cfg(windows)]
pub fn start_capture_encode(cfg: CaptureConfig) -> io::Result<CaptureHandle> {
	use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
	use std::sync::Arc;

	let stop = Arc::new(AtomicBool::new(false));
	// Bounded (capacity 1) sync channel carries the one-shot init result back to the caller.
	let (init_tx, init_rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);
	let stop_t = stop.clone();
	// Stage-3 adaptive bitrate: shared target the host controller writes (set_bitrate) and the
	// capture thread polls each tick, applying changes live via Encoder::reconfigure_bitrate.
	let requested_kbps = Arc::new(AtomicU32::new(cfg.bitrate_kbps.max(1)));
	let req_kbps_t = requested_kbps.clone();
	// Live monitor switch: u32::MAX = no pending request. The host writes a new output index
	// via CaptureHandle::switch_output; the capture thread re-points DXGI in place (see run).
	let requested_output = Arc::new(AtomicU32::new(u32::MAX));
	let req_out_t = requested_output.clone();
	// G2: per-frame encode wall-time EWMA (µs), written by the capture thread around submit.
	let encode_us = Arc::new(AtomicU32::new(0));
	let encode_us_t = encode_us.clone();
	// Client keyframe request (MediaNack([0]) sentinel) — polled+cleared per tick.
	let requested_idr = Arc::new(AtomicBool::new(false));
	let req_idr_t = requested_idr.clone();
	// Actual output the thread is streaming — written after each confirmed build (including
	// reverts). The host reads this via CaptureHandle::current_output() to reconcile its
	// input-mapping and fast-path baseline after a failed switch revert.
	let current_output = Arc::new(AtomicU32::new(cfg.output_idx));
	let cur_out_t = current_output.clone();
	// Monotonic build counter: incremented after every successful build (including same-index
	// resolution-change rebuilds so the host input path can detect geometry changes — C8).
	let build_gen = Arc::new(AtomicU32::new(0));
	let build_gen_t = build_gen.clone();
	// Liveness flag: true while the thread runs, cleared on ANY exit (death / stop / panic) via
	// the RAII guard below so the host can detect a dead capture thread on its fast path.
	let alive = Arc::new(AtomicBool::new(true));
	let alive_t = alive.clone();
	let stats = Arc::new(LiveStats::default());
	let stats_t = stats.clone();
	let requested_rfi = Arc::new(std::sync::Mutex::new(None::<(u16, u16)>));
	let req_rfi_t = requested_rfi.clone();

	let thread = std::thread::Builder::new()
		.name("pulsar-capture".into())
		.spawn(move || unsafe {
			// Clear the liveness flag on EVERY exit path of this thread — the several `return`s
			// below (unbuildable output, nvenc init fail, revert-build fail), the normal
			// `RunExit::Stop` break, and a panic. RAII covers them all in one place so no exit can
			// leave `alive` stale-true; the host polls `CaptureHandle::is_alive()` to fall through
			// to a full capture restart when a live-restream fast path would hit a dead thread.
			struct AliveGuard(std::sync::Arc<std::sync::atomic::AtomicBool>);
			impl Drop for AliveGuard {
				fn drop(&mut self) {
					self.0.store(false, std::sync::atomic::Ordering::SeqCst);
				}
			}
			let _alive = AliveGuard(alive_t);
			// 1. Thread priority + display-keepalive (both RAII / restored on scope exit).
			//    TIME_CRITICAL keeps the pacing loop off the scheduler's slow path; the
			//    display-keepalive prevents the monitor sleeping mid-stream.
			let _prio = ThreadPriorityGuard::time_critical();
			let _wake = DisplayKeepAlive::engage();
			// A18: also join the MMCSS "Capture" task class so Windows gives this capture/pacing
			// thread glitch-resistant scheduling on top of TIME_CRITICAL (the technique Sunshine/
			// OBS use for the capture thread). RAII — reverted on scope exit. Opt-out: PULSAR_MMCSS=0.
			let _mmcss = MmcssGuard::engage();

			// Live MONITOR switch is handled by REBUILDING capture+encode on the new output in
			// THIS thread (the outer loop below), NOT by mutating the live device in place. A
			// monitor can sit on a DIFFERENT GPU (MUX laptops: each panel on the iGPU or the
			// dGPU), and a D3D11 device can only DuplicateOutput a monitor on its OWN adapter —
			// so `CaptureDevice::create` must re-pick the adapter per output. Rebuilding here
			// still skips everything the host-side full restart pays (new OS thread, the init
			// handshake wait, the audio-pipeline restart, the ffmpeg encoder re-probe), so a
			// switch is much faster than a fresh `start_capture_encode` and works across GPUs.
			let mut output_idx = cfg.output_idx;
			let mut announced = false;
			// pts is monotonic across switches (the rebuilt encoder forces an IDR on its first
			// frame, so the client re-syncs regardless); last_kbps is re-seeded per build.
			let mut pts: i64 = 0;
			// RTP timestamps come from the CAPTURE TIME, not from `pts` (a frame counter): the
			// client's delay-gradient estimator compares their spacing with the arrival spacing,
			// and a host that falls a few % short of its nominal fps (measured 2026-11-08: 113-116
			// frames/s at 120 on a hybrid-GPU host) made frame-count timestamps run
			// 4-6 % slow — read as a queue growing on the path, which cut the bitrate on an idle
			// 40 Mbit link and stepped the stream down to 1080p30. Monotonic across monitor switches.
			let rtp_epoch = std::time::Instant::now();
			let mut build_no: u32 = 0;
			// The last output index that successfully built+streamed — a failed SWITCH build
			// reverts here so the stream survives (instead of the thread dying → packet drought).
			let mut prev_good_output = cfg.output_idx;
			// Full-monitor capture source (DXGI or WGC) and, on a hybrid host, the measurement
			// that picks it (path.rs). A window capture always uses WGC on its window.
			let forced_source = path::forced_source();
			let plan_for = |idx: u32| {
				if cfg.window_hwnd.is_some() {
					path::Plan::none()
				} else {
					path::plan(idx, cfg.fps, forced_source)
				}
			};
			let mut plan = plan_for(output_idx);
			let mut plan_output = output_idx;
			let mut source = plan.first;
			let mut probe_results: Vec<(path::MonSource, path::Score)> = Vec::new();
			let mut next_source: Option<path::MonSource> = None;
			// Set by the probe to rebuild on the same output (the run loop returns Switch).
			let rebuild = AtomicBool::new(false);
			// No-frame watchdog (2026-11-12: a host's WGC monitor capture built fine but never
			// delivered a frame — "preparing stream" forever). A build that gets no frame within
			// NO_FRAME_SWITCH rebuilds on the other source, once per output; if that one is dry
			// too it stays (the host tells the client — CaptureStats::encoded stays 0).
			let mut dry_switched_on: Option<u32> = None;
			let qpc = dxgi::platform::Qpc::new();
			loop {
				// Another monitor → plan again; a probe step / fallback may have picked the source.
				if output_idx != plan_output {
					plan = plan_for(output_idx);
					plan_output = output_idx;
					source = plan.first;
					probe_results.clear();
					next_source = None;
				}
				if let Some(next) = next_source.take() {
					source = next;
				}
				rebuild.store(false, Ordering::Relaxed);
				build_no += 1;
				let build_t0 = std::time::Instant::now();
				cap_log(&format!("=== build #{build_no} output={output_idx} ==="));
				// 2. Build capture (DXGI) on the CURRENT output. `create` enumerates the adapter
				//    that owns this monitor and makes the D3D11 device there — so a cross-GPU
				//    switch lands on the right adapter. May fail (no display owner / locked
				//    desktop / session-0). On the FIRST build a failure is reported so the host
				//    falls back to ffmpeg; on a later (switch) build it tears the stream down (the
				//    host can re-request).
				// `announced` is the "already streaming" flag — true ⇒ this is an in-session SWITCH
				// (or revert) build, so normally we use the SHORT transient-retry budget: a switch to
				// a fullscreen monitor must not freeze video for ~5 s (and the revert would pay it
				// again). The initial build (announced=false) keeps the long budget. (B30)
				//
				// EXCEPTION (C3): a same-index rebuild triggered by RunExit::Switch(output_idx) is a
				// RESOLUTION CHANGE on the SAME monitor (pacing.rs:147-150), NOT a cross-monitor
				// switch. There is nowhere to revert (prev_good_output == output_idx), so the short
				// budget converts a transient >600ms unavailability (e.g. a fullscreen-exclusive game
				// taking the output) into permanent thread death. Use the LONG budget for same-index
				// rebuilds so the mode transition resolves within ~5 s and capture recovers.
				let fast_transient = announced && output_idx != prev_good_output;
				// PER-WINDOW (WGC) vs full-monitor (DXGI) source selection (Phase 2b). When the
				// host requested a window capture, build the WGC source; it follows ITS window
				// regardless of output_idx (no monitor switching). Otherwise the existing DXGI
				// duplication on `output_idx`. Both produce the same BGRA Frame for the encoder.
				let build_result = if let Some(hwnd) = cfg.window_hwnd {
					wgc::WgcCaptureDevice::create(hwnd, output_idx, stop_t.clone(), fast_transient)
						.map(CapSource::Wgc)
				} else {
					let build = |s: path::MonSource| match s {
						path::MonSource::Dxgi | path::MonSource::DxgiMf => {
							dxgi::CaptureDevice::create(output_idx, stop_t.clone(), fast_transient)
								.map(CapSource::Dxgi)
						}
						path::MonSource::Wgc => {
							wgc::WgcCaptureDevice::create_monitor(output_idx, stop_t.clone())
								.map(CapSource::Wgc)
						}
					};
					match build(source) {
						Ok(c) => Ok(c),
						// The other source before giving up: DXGI refuses a monitor this process
						// already duplicates (E_INVALIDARG — a second session on the same screen)
						// and WGC captures it again; WGC can be missing where DXGI works.
						Err(e) if forced_source.is_none() && !stop_t.load(Ordering::Relaxed) => {
							let alt = source.other();
							encode_log(&format!(
								"capture source {} failed on output {output_idx} ({e:?}), trying {}",
								source.name(),
								alt.name()
							));
							match build(alt) {
								Ok(c) => {
									source = alt;
									plan.probe = false;
									probe_results.clear();
									Ok(c)
								}
								Err(_) => Err(e),
							}
						}
						Err(e) => Err(e),
					}
				};
				let mut cap = match build_result {
					Ok(c) => c,
					Err(e) => {
						if !announced {
							cap_log(&format!("BUILD #{build_no} cap FAILED output={output_idx}: {e:?} — initial, fall to ffmpeg"));
							let _ = init_tx.send(Err(format!("dxgi init: {e:?}")));
							return;
						}
						// A SWITCH-build failed (e.g. target monitor mid-fullscreen, DuplicateOutput
						// still unavailable after the long retry). DON'T kill the stream — revert to
						// the last good monitor so video keeps flowing; the user can retry the switch.
						if output_idx != prev_good_output {
							cap_log(&format!("BUILD #{build_no} cap FAILED output={output_idx}: {e:?} — REVERT to {prev_good_output}"));
							output_idx = prev_good_output;
							continue;
						}
						cap_log(&format!("BUILD #{build_no} cap FAILED output={output_idx} (last-good too): {e:?} — THREAD DYING"));
						return;
					}
				};
				let (w, h) = cap.target_size(cfg.width, cfg.height);
				// Native (full) capture size — the encoder sizes the cross-adapter bridge +
				// VideoProcessor input to this, scaling native→encode in the Blt when they differ.
				let (cap_w, cap_h) = cap.native_size();
				let rotation = cap.rotation_deg();

				// 3. Build the encoder on the SAME device + immediate context as capture
				//    (Strategy A: the NVENC session is opened on this D3D11 device; the
				//    driver does any cross-adapter copy internally on hybrid GPUs). Seed the
				//    bitrate from the LIVE target so a switch preserves any adaptive-bitrate step.
				let mut last_kbps = req_kbps_t.load(Ordering::Relaxed).max(1);
				let params = encode::EncParams {
					width: w,
					height: h,
					capture_width: cap_w,
					capture_height: cap_h,
					fps: cfg.fps.max(1),
					bitrate_kbps: last_kbps,
					dest: cfg.dest.clone(),
					codec: cfg.codec,
					low_latency: cfg.low_latency,
					intra_refresh: cfg.intra_refresh,
					rotation,
					rfi: cfg.rfi,
				};
				let mf_path = cfg.window_hwnd.is_none() && source.is_mf();
				let built = if mf_path {
					encode::mf::MfEncoder::new(cap.device(), cap.context(), &params).map(AnyEncoder::Mf)
				} else {
					encode::Encoder::new(cap.device(), cap.context(), &params).map(AnyEncoder::Nv)
				};
				let mut enc = match built {
					Ok(e) => e,
					// The display GPU has no usable encoder for this codec: measure / use the next
					// path (NVENC) instead — never a reason to leave the native path.
					Err(e) if mf_path => {
						encode_log(&format!("capture path dxgi-mf: encoder unavailable ({e})"));
						if plan.skip(&mut probe_results, &mut next_source).is_none() {
							next_source = Some(source.other());
						}
						drop(cap);
						continue;
					}
					Err(e) => {
						if !announced {
							cap_log(&format!("BUILD #{build_no} enc FAILED output={output_idx}: {e} — initial, fall to ffmpeg"));
							let _ = init_tx.send(Err(format!("nvenc init: {e}")));
							return;
						}
						if output_idx != prev_good_output {
							cap_log(&format!("BUILD #{build_no} enc FAILED output={output_idx}: {e} — REVERT to {prev_good_output}"));
							output_idx = prev_good_output;
							continue;
						}
						cap_log(&format!("BUILD #{build_no} enc FAILED output={output_idx} (last-good too): {e} — THREAD DYING"));
						return;
					}
				};
				// Asked to stop while this build sat inside a driver call (2026-11-13: device
				// creation on a wedged NVIDIA driver returned after 75 min): the host gave up on
				// it long ago and streams another way — leave without sending a single packet.
				if stop_t.load(Ordering::Relaxed) {
					cap_log(&format!("build #{build_no} finished after stop — discarded"));
					enc.flush_and_close();
					drop(enc);
					drop(cap);
					break;
				}
				prev_good_output = output_idx;
				stats_t.source.store(
					if cfg.window_hwnd.is_some() { 3 } else { source.code() },
					Ordering::Relaxed,
				);
				if cfg.window_hwnd.is_none() {
					encode_log(&format!(
						"capture source {} output={output_idx} {w}x{h}@{} probe={}",
						source.name(),
						cfg.fps,
						plan.probe
					));
				}
				// Publish the confirmed output so the host can read current_output() and
				// reconcile cur_display / last_native_req after a failed switch revert.
				cur_out_t.store(output_idx, Ordering::SeqCst);
				// Bump the build generation so the host input path re-resolves monitor geometry
				// even when the index is unchanged (same-index resolution-change rebuild — C8).
				build_gen_t.fetch_add(1, Ordering::SeqCst);
				cap_log(&format!(
					"build #{build_no} STREAMING output={output_idx} {w}x{h} kbps={last_kbps} rebuilt_in={}ms",
					build_t0.elapsed().as_millis(),
				));

				// 4. First build OK — unblock the caller (Ok ⇒ streaming started). Later (switch)
				//    builds skip this; the handshake is a one-shot.
				if !announced {
					let _ = init_tx.send(Ok(()));
					announced = true;
				}

				// 4b. Drain a switch requested DURING this rebuild's blind window (between the prior
				//     run() exit and now) — `run` won't poll the atom until its first tick, so a
				//     rapid second click would otherwise wait a whole rebuild+GOP. Rebuild straight
				//     onto it. A request for the monitor we just built is consumed (no-op).
				let pending = req_out_t.swap(u32::MAX, Ordering::AcqRel);
				if pending != u32::MAX && pending != output_idx {
					enc.flush_and_close();
					drop(enc);
					drop(cap);
					output_idx = pending;
					continue;
				}

				// 5. Pacing loop (Sunshine technique). `on_frame` converts BGRA→NV12, encodes via
				//    NVENC, and hands the Annex-B AU to the RTP sender thread. `run` returns either
				//    Stop (teardown) or Switch(idx) when the host asked for a new monitor.
				let mut submit_errs: u64 = 0;
				let mut frames_sent: u64 = 0;
				let mut meter = path::Meter::new();
				let mut pixel = bench::PixelProbe::enabled().then(bench::PixelProbe::new);
				let build_frames = std::sync::atomic::AtomicU32::new(0);
				let run_done = AtomicBool::new(false);
				let went_dry = AtomicBool::new(false);
				let watch = cfg.window_hwnd.is_none()
					&& forced_source.is_none()
					&& dry_switched_on != Some(output_idx);
				let exit = std::thread::scope(|sc| {
				if watch {
					sc.spawn(|| {
						let t0 = std::time::Instant::now();
						while !run_done.load(Ordering::Relaxed) && !stop_t.load(Ordering::Relaxed) {
							if build_frames.load(Ordering::Relaxed) > 0 {
								return;
							}
							if t0.elapsed() >= NO_FRAME_SWITCH {
								went_dry.store(true, Ordering::Relaxed);
								rebuild.store(true, Ordering::Relaxed);
								return;
							}
							std::thread::sleep(std::time::Duration::from_millis(50));
						}
					});
				}
				let exit = cap.run(
					cfg.fps.max(1),
					cfg.draw_mouse,
					&req_out_t,
					&rebuild,
					&stop_t,
					|frame: &Frame| {
						build_frames.fetch_add(1, Ordering::Relaxed);
						// Stage-3 adaptive bitrate: apply any pending target BEFORE encoding this
						// tick — a cheap live nvEncReconfigureEncoder (no re-init). Lock-free poll;
						// on a reconfigure error keep the old bitrate so the stream never stalls.
						let want = req_kbps_t.load(Ordering::Relaxed);
						if want != last_kbps {
							match enc.reconfigure_bitrate(want) {
								Ok(()) => last_kbps = want,
								Err(e) => dbg_capture(&format!("reconfigure {want} kbps: {e}")),
							}
						}
						// First frame after a same-res reinit (Hz change / transient ACCESS_LOST):
						// force a keyframe so the client re-syncs instead of waiting a safety GOP.
						// Same for an explicit client keyframe request (MediaNack([0]) → the host
						// calls CaptureHandle::request_idr, e.g. after the phone rebuilt its
						// decoder mid-session).
						if frame.force_idr || req_idr_t.swap(false, Ordering::AcqRel) {
							enc.request_idr();
						}
						// A client-reported hole: invalidate the lost frames as references (the next
						// frame is the tagged recovery point). Out of reach → nothing here: the
						// client's repair or keyframe request handles it.
						let lost = req_rfi_t.lock().unwrap().take();
						if let Some((first, last)) = lost {
							match enc.invalidate_lost(first, last) {
								Ok(n) => {
									stats_t.rfi_ok.fetch_add(1, Ordering::Relaxed);
									cap_log(&format!("rfi seq {first}..={last}: {n} frame(s) invalidated"));
								}
								Err(why) => {
									// The first few per build go to the release log too (why RFI
									// could not help is the thing to know on a new machine).
									if stats_t.rfi_no.fetch_add(1, Ordering::Relaxed) < 20 {
										encode_log(&format!("rfi seq {first}..={last}: declined ({why})"));
									}
								}
							}
						}
						let enc_t0 = std::time::Instant::now();
						// 90 kHz ticks since the capture thread started (wraps after ~13 h, as RTP does).
						let rtp_ts = (rtp_epoch.elapsed().as_micros() as u64 * 9 / 100) as u32;
						if let Err(e) = enc.submit(frame, pts, rtp_ts) {
							submit_errs += 1;
							if submit_errs <= 3 || submit_errs % 240 == 0 {
								cap_log(&format!("build #{build_no} submit err #{submit_errs}: {e}"));
							}
						} else {
							frames_sent += 1;
							stats_t.encoded.fetch_add(1, Ordering::Relaxed);
							// G2: smooth per-frame encode wall-time (µs) into the EWMA atom.
							let us = enc_t0.elapsed().as_micros().min(u32::MAX as u128) as u32;
							let prev = encode_us_t.load(Ordering::Relaxed);
							encode_us_t.store(if prev == 0 { us } else { prev / 8 * 7 + us / 8 }, Ordering::Relaxed);
							// present→encoded latency: the live stats and, while probing, this source's score.
							let done_ns = qpc.now_ns();
							let present_ns = match pixel.as_mut() {
								Some(p) if frame.is_new => p.paint_ns(frame.texture),
								Some(_) => 0,
								None => frame.present_ns,
							};
							meter.record(frame.is_new, present_ns, done_ns, us, &stats_t);
							if plan.probe && meter.samples() >= path::PROBE_SAMPLES {
								path::probe_step(&mut plan, &mut probe_results, &mut next_source, source, meter.score(), &rebuild);
							}
						}
pts += 1;
					},
				);
				run_done.store(true, Ordering::Relaxed);
				exit
				});
				if went_dry.load(Ordering::Relaxed) {
					let alt = source.other();
					encode_log(&format!(
						"capture source {} gave no frame on output {output_idx} in {} ms — rebuilding on {}",
						source.name(),
						NO_FRAME_SWITCH.as_millis(),
						alt.name()
					));
					dry_switched_on = Some(output_idx);
					next_source = Some(alt);
					plan.probe = false;
					probe_results.clear();
				}
				// Release log too (2026-11-13: a host whose WGC capture gave no frame and no
				// fallback left nothing to say why — this line is the first clue next time).
				encode_log(&format!(
					"build #{build_no} run exited ({}) frames_sent={frames_sent} submit_errs={submit_errs} dry={}",
					match exit { dxgi::RunExit::Stop => "stop".to_string(), dxgi::RunExit::Switch(i) => format!("switch->{i}") },
					went_dry.load(Ordering::Relaxed),
				));
				cap_log(&format!(
					"build #{build_no} run exited ({}) frames_sent={frames_sent} submit_errs={submit_errs}",
					match exit { dxgi::RunExit::Stop => "stop".to_string(), dxgi::RunExit::Switch(i) => format!("switch->{i}") },
				));

				// 6. Tear down NVENC (no drain — SYNC, no B-frames) + release the AddRef'd device
				//    LAST. Dropping the Encoder drops its RtpEgress (closes the socket, joins the
				//    sender thread). Then drop `cap` (releases the DXGI duplication + device).
				enc.flush_and_close();
				drop(enc);
				drop(cap);
				match exit {
					dxgi::RunExit::Stop => break,
					// Rebuild on the requested monitor (its adapter may differ — see above).
					dxgi::RunExit::Switch(idx) => output_idx = idx,
				}
			}
		})?;

	// Block on the init handshake (bounded). If the thread died before sending, treat as Err.
	match init_rx.recv_timeout(std::time::Duration::from_secs(3)) {
		Ok(Ok(())) => Ok(CaptureHandle {
			stop,
			thread: Some(thread),
			requested_kbps,
			requested_output,
			current_output,
			build_gen,
			encode_us,
			requested_idr,
			alive,
			stats,
			requested_rfi,
		}),
		Ok(Err(msg)) => {
			// Thread reported an init error and is returning; join to reap it.
			let _ = thread.join();
			Err(io::Error::new(io::ErrorKind::Other, msg))
		}
		Err(_) => {
			// Timed out (or the sender was dropped without a value): tell the thread to stop and
			// report so the host falls back to ffmpeg. The thread is joined only if it exits
			// soon: one stuck inside a driver call (device creation on a wedged NVIDIA driver
			// blocks for hours, uninterruptibly) would hold the caller — the host's stream
			// handler — just as long. Detached, it sees the stop flag when the call returns.
			stop.store(true, std::sync::atomic::Ordering::SeqCst);
			let t0 = std::time::Instant::now();
			while alive.load(std::sync::atomic::Ordering::SeqCst) && t0.elapsed() < INIT_STOP_WAIT {
				std::thread::sleep(std::time::Duration::from_millis(10));
			}
			if !alive.load(std::sync::atomic::Ordering::SeqCst) {
				let _ = thread.join();
			}
			Err(io::Error::new(
				io::ErrorKind::TimedOut,
				"capture init timed out",
			))
		}
	}
}

/// One enumerated host monitor: `(idx, name, width, height, primary)`. `idx` is the
/// 0-based position in the attached-to-desktop output list — the same index
/// `CaptureConfig::output_idx` / `CaptureDevice::create` expect, so the host can
/// advertise these and capture the chosen one with no mapping. `name` is the GDI
/// device name (`\\.\DISPLAY1`, trimmed to `DISPLAY1`); `primary` is the output
/// anchored at the virtual-desktop origin (0,0).
pub type DisplayDesc = (u32, String, u32, u32, bool);

/// Enumerate the host's attached monitors in DXGI output order (Windows). Empty on
/// non-Windows or when enumeration fails — the caller then advertises no picker and
/// streams the default output.
#[cfg(windows)]
pub fn list_displays() -> Vec<DisplayDesc> {
	unsafe { dxgi::CaptureDevice::list_outputs().unwrap_or_default() }
}

/// Non-Windows stub (see [`list_displays`]).
#[cfg(not(any(windows, target_os = "macos")))]
pub fn list_displays() -> Vec<DisplayDesc> {
	Vec::new()
}

/// Geometry needed to map a normalized (0..1) pointer onto the captured monitor's
/// place in the Windows virtual desktop: the chosen monitor's rect and the bounding
/// box of ALL attached monitors (== `SM_*VIRTUALSCREEN`). All in virtual-desktop
/// pixels. `mon_*` is the output at `output_idx` (DXGI order, same as `list_displays`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayRect {
	pub mon_left: i32,
	pub mon_top: i32,
	pub mon_width: i32,
	pub mon_height: i32,
	pub virt_left: i32,
	pub virt_top: i32,
	pub virt_width: i32,
	pub virt_height: i32,
}

/// Resolve the virtual-desktop geometry of the captured output (`output_idx`, DXGI
/// order) plus the full virtual-screen extent, so absolute-pointer injection can target
/// the streamed (possibly non-primary) monitor. `None` if enumeration fails or the index
/// is out of range. Windows-only (the native multi-monitor capture path).
#[cfg(windows)]
pub fn display_rect(output_idx: u32) -> Option<DisplayRect> {
	unsafe { dxgi::CaptureDevice::output_rect(output_idx) }
}

/// Stub where no native path exists (see [`display_rect`]; macOS has its own in `mac/`).
#[cfg(not(any(windows, target_os = "macos")))]
pub fn display_rect(_output_idx: u32) -> Option<DisplayRect> {
	None
}

/// Stub where no native path exists (Linux): callers `cfg`-gate the call site and this keeps
/// the symbol present for a clean cross-platform `cargo check`. Windows and macOS have real ones.
#[cfg(not(any(windows, target_os = "macos")))]
pub fn start_capture_encode(_cfg: CaptureConfig) -> io::Result<CaptureHandle> {
	Err(io::Error::new(
		io::ErrorKind::Unsupported,
		"native capture exists on Windows and macOS only",
	))
}

// ===========================================================================
// Windows helpers (thread priority, display keep-alive, debug logging)
// ===========================================================================

/// How long a fresh capture build may go without a single frame before the no-frame watchdog
/// rebuilds it on the other source (DXGI ↔ WGC). A healthy build delivers its first frame in
/// well under 100 ms; DXGI's initial retry budget for a mode change stays far below this.
const NO_FRAME_SWITCH: std::time::Duration = std::time::Duration::from_millis(2500);
/// How long a capture thread that missed its init deadline gets to exit before it is detached.
#[cfg(windows)]
const INIT_STOP_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// Capture-thread rebuild lifecycle log. Debug-builds only (no-op in release).
/// Previously wrote to `C:\Users\Public\pulsar-capture-dbg.txt` unconditionally;
/// that was temporary instrumentation for the "switch works N times then stuck"
/// diagnosis and must not ship to release (world-readable fixed path, unbounded
/// growth, file I/O on the pacing-critical path).
#[cfg(windows)]
#[inline]
fn cap_log(msg: &str) {
	#[cfg(debug_assertions)]
	{
		use std::io::Write;
		if let Ok(mut f) = std::fs::OpenOptions::new()
			.create(true)
			.append(true)
			.open("C:\\Users\\Public\\pulsar-capture-dbg.txt")
		{
			let _ = writeln!(f, "[lib] {msg}");
		}
	}
	#[cfg(not(debug_assertions))]
	let _ = msg;
}

#[cfg(windows)]
#[inline]
fn dbg_capture(msg: &str) {
	#[cfg(debug_assertions)]
	eprintln!("[pulsar-capture] {msg}");
	#[cfg(not(debug_assertions))]
	let _ = msg;
}

/// RAII guard that bumps the calling thread to `THREAD_PRIORITY_TIME_CRITICAL` for the life of
/// the capture loop and restores the previous priority on drop.
///
/// Why: with the default priority, `thread::sleep`/timer waits land on the 15.6ms scheduler
/// tick and the documented "73fps irregular" symptom appears; TIME_CRITICAL keeps the paced
/// wakeups tight. (We also use a high-resolution waitable timer in dxgi.rs — both are needed.)
#[cfg(windows)]
struct ThreadPriorityGuard {
	prev: i32,
}

#[cfg(windows)]
impl ThreadPriorityGuard {
	unsafe fn time_critical() -> Self {
		use windows::Win32::System::Threading::{
			GetCurrentThread, GetThreadPriority, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
		};
		// GetThreadPriority returns THREAD_PRIORITY_ERROR_RETURN (0x7fffffff) on failure; we
		// store whatever it returns and only restore if it looked valid (see Drop).
		let prev = GetThreadPriority(GetCurrentThread());
		// SetThreadPriority returns windows_core::Result<()> in 0.59; ignore failure (e.g. on a
		// constrained job object) — the loop still runs, just with worse pacing.
		let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
		Self { prev }
	}
}

#[cfg(windows)]
impl Drop for ThreadPriorityGuard {
	fn drop(&mut self) {
		use windows::Win32::System::Threading::{
			GetCurrentThread, SetThreadPriority, THREAD_PRIORITY,
		};
		// THREAD_PRIORITY_ERROR_RETURN sentinel — don't try to restore a bogus value.
		const ERR_RETURN: i32 = 0x7fff_ffffu32 as i32;
		if self.prev != ERR_RETURN {
			unsafe {
				let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY(self.prev));
			}
		}
	}
}

/// RAII guard that asks Windows to keep the display on while we capture, and clears the request
/// on drop. Without this the monitor can power-save mid-stream, which both blanks the captured
/// surface and (on some drivers) drops Desktop Duplication.
#[cfg(windows)]
struct DisplayKeepAlive {
	engaged: bool,
}

#[cfg(windows)]
impl DisplayKeepAlive {
	unsafe fn engage() -> Self {
		use windows::Win32::System::Power::{
			SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED,
		};
		// ES_CONTINUOUS makes the state sticky for this thread until we clear it; combined with
		// ES_DISPLAY_REQUIRED it keeps the monitor awake. A 0 return means the call failed.
		let prev = SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED);
		Self {
			engaged: prev.0 != 0,
		}
	}
}

#[cfg(windows)]
impl Drop for DisplayKeepAlive {
	fn drop(&mut self) {
		if self.engaged {
			use windows::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS};
			// Clear our request (ES_CONTINUOUS alone with no other flags resets to the default
			// power policy) so we don't pin the display awake after the stream ends.
			unsafe {
				let _ = SetThreadExecutionState(ES_CONTINUOUS);
			}
		}
	}
}

/// RAII guard that joins the MMCSS (Multimedia Class Scheduler Service) "Capture" task class for
/// the calling thread and reverts it on drop. MMCSS gives a registered multimedia thread
/// glitch-resistant scheduling — the same boost Sunshine/OBS apply to their capture thread, on
/// top of TIME_CRITICAL. Tunable: `PULSAR_MMCSS=0` disables the join. Best-effort: if avrt/MMCSS
/// is unavailable the handle stays null and Drop does nothing — capture is unaffected.
#[cfg(windows)]
struct MmcssGuard {
	handle: windows::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl MmcssGuard {
	unsafe fn engage() -> Self {
		use windows::core::w;
		use windows::Win32::Foundation::HANDLE;
		use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
		// Opt-out knob (default on): PULSAR_MMCSS=0 skips the MMCSS join.
		if std::env::var("PULSAR_MMCSS")
			.map(|v| v == "0")
			.unwrap_or(false)
		{
			return Self {
				handle: HANDLE::default(),
			};
		}
		// AvSetMmThreadCharacteristicsW takes an in/out task index (0 in; the driver fills it).
		// "Capture" is the standard MMCSS task class for a screen/video capture thread. On
		// failure (constrained env / no MMCSS) we keep a null handle so Drop is a no-op.
		let mut task_index: u32 = 0;
		let handle =
			AvSetMmThreadCharacteristicsW(w!("Capture"), &mut task_index).unwrap_or_default();
		Self { handle }
	}
}

#[cfg(windows)]
impl Drop for MmcssGuard {
	fn drop(&mut self) {
		if !self.handle.is_invalid() {
			use windows::Win32::System::Threading::AvRevertMmThreadCharacteristics;
			unsafe {
				let _ = AvRevertMmThreadCharacteristics(self.handle);
			}
		}
	}
}

#[cfg(test)]
mod progress_watch_tests {
	use super::ProgressWatch;
	use std::time::{Duration, Instant};

	#[test]
	fn flat_count_reports_how_long_it_has_been_flat() {
		let t0 = Instant::now();
		let mut w = ProgressWatch::default();
		assert_eq!(w.observe(10, t0), Duration::ZERO);
		assert_eq!(w.observe(10, t0 + Duration::from_secs(2)), Duration::from_secs(2));
		assert_eq!(w.observe(10, t0 + Duration::from_secs(6)), Duration::from_secs(6));
	}

	#[test]
	fn a_moving_count_restarts_the_clock() {
		let t0 = Instant::now();
		let mut w = ProgressWatch::default();
		w.observe(10, t0);
		assert_eq!(w.observe(11, t0 + Duration::from_secs(5)), Duration::ZERO);
		assert_eq!(w.observe(11, t0 + Duration::from_secs(7)), Duration::from_secs(2));
		// A wrapped counter is still a change.
		assert_eq!(w.observe(0, t0 + Duration::from_secs(8)), Duration::ZERO);
	}

	#[test]
	fn reset_forgets_the_old_pipeline() {
		let t0 = Instant::now();
		let mut w = ProgressWatch::default();
		w.observe(5, t0);
		w.reset();
		assert_eq!(w.observe(5, t0 + Duration::from_secs(9)), Duration::ZERO);
	}
}
