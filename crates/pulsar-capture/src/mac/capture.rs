//! The macOS capture+encode thread (see `mac/mod.rs`): one display, or one window
//! (`CaptureConfig::window_hwnd` = a `CGWindowID`) through the same VideoToolbox encoder,
//! RTP egress, live bitrate and keyframe-on-request.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use pulsar_core::macos_capture::{self as sck, ScError, ScStream, StreamSpec, VideoFrame, VideoSpec};

use super::policy;
use super::vt::{EncodedAu, VtCodec, VtEncoder, VtSpec};
use crate::{CaptureConfig, CaptureHandle, Codec, DisplayDesc, DisplayRect, LiveStats, RtpEgress};

/// `LiveStats::source` id of this path: "sck" (a display) / "sck-window" (one window).
const SOURCE_SCK: u32 = 5;
const SOURCE_SCK_WINDOW: u32 = 6;
/// How long `start_capture_encode` waits for the first build (SCK's content query + start).
const INIT_TIMEOUT: Duration = Duration::from_secs(8);
/// A stream SCK stopped on its own (display gone, sleep, permission revoked) is rebuilt at
/// most this many times in a row before the thread gives up (the host then restarts it).
const MAX_REBUILDS: u32 = 5;

/// The active displays, main first (`(idx, name, width, height, primary)`, pixels) — the
/// index the client's picker sends back as `output_idx`.
pub fn list_displays() -> Vec<DisplayDesc> {
	sck::displays()
		.into_iter()
		.enumerate()
		.map(|(i, d)| (i as u32, d.name, d.width, d.height, d.primary))
		.collect()
}

/// The display at `output_idx` and the bounding box of all displays, in global display
/// points (the CGEvent coordinate space).
pub fn display_rect(output_idx: u32) -> Option<DisplayRect> {
	let all = sck::displays();
	let d = all.get(output_idx as usize)?;
	let left = all.iter().map(|d| d.left).min()?;
	let top = all.iter().map(|d| d.top).min()?;
	let right = all.iter().map(|d| d.left + d.width_pt as i32).max()?;
	let bottom = all.iter().map(|d| d.top + d.height_pt as i32).max()?;
	Some(DisplayRect {
		mon_left: d.left,
		mon_top: d.top,
		mon_width: d.width_pt as i32,
		mon_height: d.height_pt as i32,
		virt_left: left,
		virt_top: top,
		virt_width: right - left,
		virt_height: bottom - top,
	})
}

/// Frames handed from SCK's queue to the encoder thread: only the newest one is kept.
struct FrameSlot {
	frame: Mutex<Option<VideoFrame>>,
	ready: Condvar,
	/// Set by SCK's stop callback.
	died: AtomicBool,
}

/// Numbers the output callback feeds back to the thread.
#[derive(Default)]
struct OutStats {
	latency_us: Vec<u32>,
}

/// A built pipeline on one display or window.
struct Built {
	stream: ScStream,
	enc: VtEncoder,
	slot: Arc<FrameSlot>,
	/// The display streamed (`None` for a window: the input path maps onto the window's own
	/// frame, `window_src::input_area`).
	display_id: Option<u32>,
}

/// The window this capture streams (`CaptureConfig::window_hwnd` as a `CGWindowID`), if any.
fn window_id(cfg: &CaptureConfig) -> Option<u32> {
	cfg.window_hwnd.and_then(|h| u32::try_from(h).ok()).filter(|id| *id > 0)
}

/// Publishes the streamed display for the macOS input injector (absolute pointer mapping) and
/// withdraws it on drop, unless a newer capture published its own since.
struct StreamedDisplay(u32);

impl StreamedDisplay {
	fn publish(display_id: u32) -> Self {
		sck::set_streamed_display(display_id);
		Self(display_id)
	}
}

impl Drop for StreamedDisplay {
	fn drop(&mut self) {
		sck::clear_streamed_display(self.0);
	}
}

#[allow(clippy::too_many_arguments)]
fn build(
	cfg: &CaptureConfig,
	output_idx: u32,
	kbps: u32,
	egress: &Arc<RtpEgress>,
	rtp_epoch: Instant,
	stats: &Arc<LiveStats>,
	out_stats: &Arc<Mutex<OutStats>>,
	encode_us: &Arc<AtomicU32>,
) -> Result<Built, String> {
	let window = window_id(cfg);
	let (display_id, w, h) = match window {
		// The host sized the frame to the window's aspect (`window_src::encode_size`); asked
		// for 0 it takes the window's own pixel size.
		Some(id) => {
			let (w, h) = policy::window_output_size(cfg.width, cfg.height, || {
				pulsar_core::window_capture::size(id as u64)
			});
			(None, w, h)
		}
		None => {
			let displays = sck::displays();
			let display = displays
				.get(output_idx as usize)
				.or_else(|| displays.first())
				.ok_or_else(|| "no active display".to_string())?;
			let (w, h) = policy::output_size(cfg.width, cfg.height, display.width, display.height);
			(Some(display.id), w, h)
		}
	};
	let fps = cfg.fps.max(1);
	let slot = Arc::new(FrameSlot { frame: Mutex::new(None), ready: Condvar::new(), died: AtomicBool::new(false) });
	let codec = match cfg.codec {
		Codec::H264 => VtCodec::H264,
		Codec::H265 => VtCodec::Hevc,
		Codec::Av1 => return Err("VideoToolbox has no AV1 encoder".into()),
	};
	let egress_cb = egress.clone();
	let stats_cb = stats.clone();
	let out_cb = out_stats.clone();
	let enc_us_cb = encode_us.clone();
	let enc = VtEncoder::new(
		VtSpec { codec, width: w, height: h, fps, bitrate_kbps: kbps, low_latency: cfg.low_latency },
		Box::new(move |au: EncodedAu<'_>| {
			egress_cb.send_access_unit(au.data, policy::rtp_ts_90k(au.pts_us));
			if au.keyframe {
				tracing::debug!(bytes = au.data.len(), "macOS capture: keyframe out");
			}
			stats_cb.encoded.fetch_add(1, Ordering::Relaxed);
			let now_us = rtp_epoch.elapsed().as_micros() as i64;
			let lat = (now_us - au.pts_us).clamp(0, u32::MAX as i64) as u32;
			enc_us_cb.store(policy::ewma_us(enc_us_cb.load(Ordering::Relaxed), lat), Ordering::Relaxed);
			if let Ok(mut s) = out_cb.lock() {
				if s.latency_us.len() < 4096 {
					s.latency_us.push(lat);
				}
			}
		}),
	)?;
	let slot_v = slot.clone();
	let slot_s = slot.clone();
	let video = VideoSpec { width: w, height: h, fps, cursor: cfg.draw_mouse };
	let on_video: pulsar_core::macos_capture::sck::VideoCallback = Box::new(move |f: VideoFrame| {
		if let Ok(mut g) = slot_v.frame.lock() {
			*g = Some(f);
		}
		slot_v.ready.notify_one();
	});
	let on_stop: pulsar_core::macos_capture::sck::StopCallback = Box::new(move |_why: String| {
		slot_s.died.store(true, Ordering::SeqCst);
		slot_s.ready.notify_one();
	});
	let stream = match (window, display_id) {
		(Some(id), _) => ScStream::start_window(id, video, Some(on_video), Some(on_stop)),
		(None, display_id) => ScStream::start(
			StreamSpec { display_id: display_id.unwrap_or(0), video: Some(video), audio: None },
			Some(on_video),
			None,
			Some(on_stop),
		),
	}
	.map_err(|e| match e {
		ScError::PermissionDenied => "screen recording permission denied".to_string(),
		e => e.to_string(),
	})?;
	tracing::info!(
		?display_id,
		?window,
		w,
		h,
		fps,
		low_latency_rc = enc.low_latency_rc,
		"macOS native capture built (ScreenCaptureKit + VideoToolbox)"
	);
	Ok(Built { stream, enc, slot, display_id })
}

/// A build error that no retry can fix: the streamed window is gone (closed). The thread then
/// ends at once; the host's window watcher restarts the stream on the screen.
fn window_gone(err: &str) -> bool {
	err.starts_with("no window")
}

/// Keeps the displays awake while streaming (`IOPMAssertion` "PreventUserIdleDisplaySleep").
struct DisplayAwake(u32);

#[link(name = "IOKit", kind = "framework")]
extern "C" {
	fn IOPMAssertionCreateWithName(kind: *const std::ffi::c_void, level: u32, name: *const std::ffi::c_void, id: *mut u32) -> i32;
	fn IOPMAssertionRelease(id: u32) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
	fn CFStringCreateWithCString(alloc: *const std::ffi::c_void, s: *const std::ffi::c_char, encoding: u32) -> *const std::ffi::c_void;
	fn CFRelease(cf: *const std::ffi::c_void);
}

extern "C" {
	fn pthread_set_qos_class_self_np(qos: u32, relative_priority: i32) -> i32;
}

impl DisplayAwake {
	fn engage() -> Option<Self> {
		const UTF8: u32 = 0x0800_0100;
		// SAFETY: CF strings created and released here; the assertion id is released on drop.
		unsafe {
			let kind = CFStringCreateWithCString(std::ptr::null(), c"PreventUserIdleDisplaySleep".as_ptr(), UTF8);
			let name = CFStringCreateWithCString(std::ptr::null(), c"Pulsar streaming".as_ptr(), UTF8);
			let mut id = 0u32;
			let r = if kind.is_null() || name.is_null() { -1 } else { IOPMAssertionCreateWithName(kind, 255, name, &mut id) };
			if !kind.is_null() {
				CFRelease(kind);
			}
			if !name.is_null() {
				CFRelease(name);
			}
			(r == 0).then_some(Self(id))
		}
	}
}

impl Drop for DisplayAwake {
	fn drop(&mut self) {
		// SAFETY: releases the assertion created in `engage`.
		unsafe { IOPMAssertionRelease(self.0) };
	}
}

/// Start the native capture+encode thread (see the module docs). Errors (no ScreenCaptureKit,
/// no permission, an encoder VideoToolbox refuses, AV1) come back before any packet is sent,
/// so the host falls back to the ffmpeg avfoundation path.
pub fn start_capture_encode(cfg: CaptureConfig) -> io::Result<CaptureHandle> {
	if cfg.codec == Codec::Av1 {
		return Err(io::Error::new(io::ErrorKind::Unsupported, "VideoToolbox has no AV1 encoder"));
	}
	if !sck::sck_available() {
		return Err(io::Error::new(io::ErrorKind::Unsupported, "ScreenCaptureKit needs macOS 12.3 or newer"));
	}
	if !sck::screen_capture_access(false) {
		return Err(io::Error::new(io::ErrorKind::PermissionDenied, "screen recording permission denied"));
	}
	let dest = policy::rtp_dest(&cfg.dest)
		.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("bad RTP destination {}", cfg.dest)))?;

	let stop = Arc::new(AtomicBool::new(false));
	let requested_kbps = Arc::new(AtomicU32::new(cfg.bitrate_kbps.max(1)));
	let requested_output = Arc::new(AtomicU32::new(u32::MAX));
	let encode_us = Arc::new(AtomicU32::new(0));
	let requested_idr = Arc::new(AtomicBool::new(false));
	let current_output = Arc::new(AtomicU32::new(cfg.output_idx));
	let build_gen = Arc::new(AtomicU32::new(0));
	let alive = Arc::new(AtomicBool::new(true));
	let stats = Arc::new(LiveStats::default());
	let requested_rfi = Arc::new(Mutex::new(None::<(u16, u16)>));
	let (init_tx, init_rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);

	let (stop_t, kbps_t, out_t, enc_us_t, idr_t, cur_t, gen_t, alive_t, stats_t, rfi_t) = (
		stop.clone(),
		requested_kbps.clone(),
		requested_output.clone(),
		encode_us.clone(),
		requested_idr.clone(),
		current_output.clone(),
		build_gen.clone(),
		alive.clone(),
		stats.clone(),
		requested_rfi.clone(),
	);
	let thread = std::thread::Builder::new().name("pulsar-capture-mac".into()).spawn(move || {
		struct AliveGuard(Arc<AtomicBool>);
		impl Drop for AliveGuard {
			fn drop(&mut self) {
				self.0.store(false, Ordering::SeqCst);
			}
		}
		let _alive = AliveGuard(alive_t);
		// SAFETY: sets this thread's own QoS class (user-interactive = 0x21).
		unsafe { pthread_set_qos_class_self_np(0x21, 0) };
		let _awake = DisplayAwake::engage();
		let window = window_id(&cfg);
		stats_t.source.store(if window.is_some() { SOURCE_SCK_WINDOW } else { SOURCE_SCK }, Ordering::Relaxed);
		let codec = match cfg.codec {
			Codec::H265 => Codec::H265,
			_ => Codec::H264,
		};
		let egress = match RtpEgress::spawn(dest, codec, cfg.fps.max(1), kbps_t.clone()) {
			Ok(e) => Arc::new(e),
			Err(e) => {
				let _ = init_tx.send(Err(format!("rtp: {e}")));
				return;
			}
		};
		let rtp_epoch = Instant::now();
		let out_stats = Arc::new(Mutex::new(OutStats::default()));
		let interval = Duration::from_micros(1_000_000 / cfg.fps.max(1) as u64);
		let mut output_idx = cfg.output_idx;
		let mut prev_good = cfg.output_idx;
		let mut announced = false;
		let mut rebuilds = 0u32;
		let mut last_pts: Option<i64> = None;
		// Lives until the thread ends; replaced after every build (the old one is dropped
		// first, so a rebuild on the same display does not clear what it just published).
		let mut published: Option<StreamedDisplay> = None;
		'build: loop {
			if stop_t.load(Ordering::SeqCst) {
				return;
			}
			let kbps = kbps_t.load(Ordering::SeqCst);
			let built = match build(&cfg, output_idx, kbps, &egress, rtp_epoch, &stats_t, &out_stats, &enc_us_t) {
				Ok(b) => b,
				Err(e) if !announced => {
					let _ = init_tx.send(Err(e));
					return;
				}
				Err(e) if output_idx != prev_good => {
					tracing::warn!(output_idx, "macOS capture: switch failed ({e}) — back to {prev_good}");
					output_idx = prev_good;
					continue 'build;
				}
				Err(e) if window_gone(&e) => {
					tracing::info!(?window, "macOS capture: the streamed window is gone ({e}) — ending");
					return;
				}
				Err(e) => {
					rebuilds += 1;
					tracing::warn!(output_idx, rebuilds, "macOS capture: rebuild failed ({e})");
					if rebuilds >= MAX_REBUILDS {
						return;
					}
					std::thread::sleep(Duration::from_millis(500));
					continue 'build;
				}
			};
			prev_good = output_idx;
			cur_t.store(output_idx, Ordering::SeqCst);
			gen_t.fetch_add(1, Ordering::SeqCst);
			if !announced {
				announced = true;
				let _ = init_tx.send(Ok(()));
			}
			let Built { stream, enc, slot, display_id } = built;
			drop(published.take());
			published = display_id.map(StreamedDisplay::publish);
			let mut last_kbps = kbps;
			let mut last_frame: Option<VideoFrame> = None;
			// A fresh encoder starts on a keyframe; the client needs one after a switch too.
			let mut force_key = true;
			let mut next_due = Instant::now() + interval;
			let mut window_t0 = Instant::now();
			let mut fresh_in_window = 0u32;
			let mut submit_us: Vec<u32> = Vec::with_capacity(256);
			let healthy_since = Instant::now();
			let mut errors_seen = enc.errors();
			let switch_to = loop {
				if stop_t.load(Ordering::SeqCst) {
					stream.stop();
					return;
				}
				if slot.died.load(Ordering::SeqCst) {
					break None;
				}
				let req = out_t.swap(u32::MAX, Ordering::SeqCst);
				if req != u32::MAX && req != output_idx {
					// A window stream has no display to switch: the window stays (a new window
					// or the screen comes through a new StreamReq / full restart).
					if window.is_some() {
						tracing::debug!(req, "macOS capture: display switch ignored on a window stream");
					} else {
						break Some(req);
					}
				}
				let k = kbps_t.load(Ordering::Relaxed);
				if k != last_kbps {
					enc.set_bitrate(k);
					last_kbps = k;
				}
				if idr_t.swap(false, Ordering::SeqCst) {
					force_key = true;
				}
				// VideoToolbox cannot invalidate references: decline, the client heals the loss
				// with its NACK repair or a keyframe request.
				if rfi_t.lock().map(|mut r| r.take()).ok().flatten().is_some() {
					stats_t.rfi_no.fetch_add(1, Ordering::Relaxed);
				}
				// Wait for a fresh frame until this slot's deadline.
				let fresh = {
					let mut g = slot.frame.lock().unwrap_or_else(|p| p.into_inner());
					let now = Instant::now();
					if g.is_none() && now < next_due {
						g = slot.ready.wait_timeout(g, next_due - now).map(|r| r.0).unwrap_or_else(|p| p.into_inner().0);
					}
					g.take()
				};
				let now = Instant::now();
				let (frame, capture_at) = match fresh {
					Some(f) => {
						fresh_in_window += 1;
						let at = f.arrived;
						last_frame = Some(f.clone());
						(Some(f), at)
					}
					// Static screen: SCK sends nothing, so the last frame is encoded again once
					// per fps slot (the stream and the host's progress watch keep moving).
					None if now >= next_due => (last_frame.clone(), now),
					None => (None, now),
				};
				if let Some(f) = frame {
					let pts = policy::next_pts(capture_at.saturating_duration_since(rtp_epoch).as_micros() as i64, last_pts);
					last_pts = Some(pts);
					let t0 = Instant::now();
					match enc.encode(f.pixel_buffer(), pts, force_key) {
						Ok(()) => force_key = false,
						Err(e) => tracing::warn!("macOS capture: {e}"),
					}
					submit_us.push(t0.elapsed().as_micros().min(u32::MAX as u128) as u32);
					next_due = now + interval;
				}
				if window_t0.elapsed() >= Duration::from_secs(1) {
					let secs = window_t0.elapsed().as_secs_f64();
					stats_t.fresh_per_s.store((fresh_in_window as f64 / secs).round() as u32, Ordering::Relaxed);
					let (s50, s90) = policy::p50_p90(&mut submit_us);
					stats_t.sub_p50.store(s50, Ordering::Relaxed);
					stats_t.sub_p90.store(s90, Ordering::Relaxed);
					if let Ok(mut o) = out_stats.lock() {
						let (l50, l90) = policy::p50_p90(&mut o.latency_us);
						stats_t.lat_p50.store(l50, Ordering::Relaxed);
						stats_t.lat_p90.store(l90, Ordering::Relaxed);
						o.latency_us.clear();
					}
					let errors = enc.errors();
					if errors != errors_seen {
						tracing::warn!(new = errors - errors_seen, "macOS capture: VideoToolbox dropped or failed frames");
						errors_seen = errors;
					}
					submit_us.clear();
					fresh_in_window = 0;
					window_t0 = Instant::now();
					if healthy_since.elapsed() > Duration::from_secs(10) {
						rebuilds = 0;
					}
				}
			};
			stream.stop();
			drop(enc);
			match switch_to {
				Some(idx) => {
					tracing::info!(from = output_idx, to = idx, "macOS capture: display switch");
					output_idx = idx;
				}
				None => {
					rebuilds += 1;
					tracing::warn!(output_idx, rebuilds, "macOS capture: ScreenCaptureKit stopped the stream — rebuilding");
					if rebuilds >= MAX_REBUILDS {
						return;
					}
					std::thread::sleep(Duration::from_millis(300));
				}
			}
		}
	})?;

	match init_rx.recv_timeout(INIT_TIMEOUT) {
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
		Ok(Err(e)) => {
			let _ = thread.join();
			let kind = if e.contains("permission") { io::ErrorKind::PermissionDenied } else { io::ErrorKind::Other };
			Err(io::Error::new(kind, e))
		}
		Err(_) => {
			// Still building (SCK did not answer): leave it; it sees `stop` and exits.
			stop.store(true, Ordering::SeqCst);
			drop(thread);
			Err(io::Error::new(io::ErrorKind::TimedOut, "ScreenCaptureKit did not start in time"))
		}
	}
}
