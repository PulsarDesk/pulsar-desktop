//! macOS native renderer backend — the Metal/VideoToolbox analogue of `win/mod.rs`.
//!
//! Moonlight-style and zero-copy like the Windows one: the host's RTP stream (the shared
//! `stream::rtp` reorder buffer + depacketizer) is decoded by VideoToolbox into IOSurface-backed
//! `CVPixelBuffer`s (`decode.rs`), turned into Metal textures without a copy and drawn
//! YCbCr→RGB (+ FSR) onto a `CAMetalLayer` (`metal.rs`), with the SAME `overlay.rs` egui UI
//! painted on top. The layer lives in a borderless panel this process owns, kept over the app's
//! session tab (`window.rs` — macOS has no cross-process child windows).
//!
//! Usage: pulsar-render <stream.sdp> --wid <app windowNumber> [--mode game|remote]
//!        [--pace on|off] [--hold-reveal] [--lang tr|en]
//! IPC: the same stdin / stdout lines as `win/mod.rs` (render_stats.rs reads them), except:
//!   - `viewrect <x> <y> <w> <h>` is in POINTS from the app window's top-left (Cocoa's unit);
//!   - `reparent 0x<windowNumber>` moves the picture to another app window (tab tear-off);
//!   - no `key` / `hkey` / `ov kfocus` lines: the keyboard never leaves the app on macOS;
//!   - no `ov engage`: clicks on the picture fall through to the app's session view.
//!
//! Nothing here has run on a Mac yet (written and type-checked on Windows, see
//! docs/agents/client-video.md).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

pub mod decode; // VideoToolbox decode (AU → CVPixelBuffer NV12/P010)
mod bitstream; // Annex-B / OBU → VideoToolbox samples (pure)
mod ffi; // CoreFoundation / CoreMedia / CoreVideo / VideoToolbox / CG window list
mod metal; // CVPixelBuffer → Metal (YUV→RGB, FSR) + egui painter
pub mod view; // fit / rotation / upscale state + YUV constants (pure)
mod window; // the panel over the app window + display link

use crate::stream::{self, parse_sdp, AccessUnit, Codec};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSEvent, NSEventMask, NSEventType};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode};
use objc2_metal::MTLDevice;

// ---- shared control state (host → renderer over stdin) ---------------------------------
static OPEN: AtomicBool = AtomicBool::new(false);
// Frame pacing default OFF (lowest latency); the app pushes the user's setting on start.
static PACE: AtomicBool = AtomicBool::new(false);
/// Set by a `rotate` stdin line that changed the rotation: the next presented frame
/// re-reports `vidsink-dims` (displayed dims) and re-sizes the processor.
static ROT_DIRTY: AtomicBool = AtomicBool::new(false);
static STATS: Mutex<[f32; 4]> = Mutex::new([0.0; 4]); // fps, latency_ms, decode_ms, mbps
/// The app window (`windowNumber`) passed via `--wid`; the panel is kept over it.
static PARENT: AtomicU64 = AtomicU64::new(0);
/// A new app window to move over (stdin `reparent 0x<windowNumber>`: the session tab was dragged
/// into another Pulsar window). 0 = none pending. Applied on the render thread.
static NEW_PARENT: AtomicU64 = AtomicU64::new(0);
/// In-app embed rect (stdin `viewrect <x> <y> <w> <h>`, POINTS from the app window's
/// top-left on macOS): the frontend reports the session tab's content area so the video
/// renders INSIDE the app — chrome/tabs stay visible, like the Linux native container.
/// None = no report yet → fill the whole parent client area. A 0×0 rect hides the
/// child (tab inactive / session screen unmounted).
static VIEW_RECT: Mutex<Option<(i32, i32, i32, i32)>> = Mutex::new(None);

// ---- panel mouse → egui input plumbing -------------------------------------------------------
// `handle_mouse` collects input into these statics; `overlay_prims` drains them into a
// `egui::RawInput` each frame. The panel only takes the mouse while the overlay is OPEN.
static EGUI_EVENTS: Mutex<Vec<egui::Event>> = Mutex::new(Vec::new());
static POINTER: Mutex<(f32, f32)> = Mutex::new((0.0, 0.0));
/// Chat log fed over stdin (`chat in|out <text>`, same protocol as linux.rs) —
/// shown in the overlay's Chat view while it is open.
static CHAT_LOG: Mutex<Vec<(bool, String)>> = Mutex::new(Vec::new());
/// Host messages not yet seen in the overlay Chat view — badge on the open button.
static CHAT_UNREAD: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Recent host messages + arrival, for the closed-state bubbles (overlay::draw_chat_bubbles).
static CHAT_TOASTS: Mutex<Vec<(String, std::time::Instant)>> = Mutex::new(Vec::new());

/// The live closed-state chat bubbles as (text, age s), expiring old ones.
fn chat_toasts() -> Vec<(String, f32)> {
	let mut t = CHAT_TOASTS.lock().unwrap();
	t.retain(|(_, at)| at.elapsed().as_secs_f32() < crate::overlay::BUBBLE_SECS);
	t.iter().map(|(m, at)| (m.clone(), at.elapsed().as_secs_f32())).collect()
}
/// Files view, REMOTE pane: (HOME-relative path, rows) from `fsjson …` (same
/// one-line JSON as linux.rs) — copied into the overlay state each frame.
static FS_REMOTE: Mutex<(String, Vec<crate::overlay::FsRow>)> =
	Mutex::new((String::new(), Vec::new()));
/// Relayed Enter (`kin k enter`) — consumed by the Chat composer as "send".
static ENTER_IN: AtomicBool = AtomicBool::new(false);
/// Network RTT in tenths of ms (app stdin `rtt <ms>`, from the keepalive ping/pong) —
/// the overlay's "Gecikme" tile (same as linux.rs RTT_DMS).
static RTT_DMS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// That RTT's jitter in tenths of ms (the second number of the `rtt` line).
static JITTER_DMS: std::sync::atomic::AtomicU32 =
	std::sync::atomic::AtomicU32::new(crate::overlay::JITTER_UNKNOWN);
// ---- closed-state chrome (Linux parity: linux.rs statics of the same names) ----
/// Always-on mini stats HUD while the overlay is closed (`statshud 0|1` / caps seed).
static STATS_HUD: AtomicBool = AtomicBool::new(false);
/// "Show my pointer with my name on the host" (remote mode; synced via stdin `curlabel 0|1`
/// and the caps line, flippable from the Yayın row). Default ON, like the app's config.
static CUR_LABEL: AtomicBool = AtomicBool::new(true);
/// Parsec-style overlay-open button while closed (`ovbtn 0|1` / caps seed). Default ON
/// like linux.rs — the frontend only sends `ovbtn` when the user turned it OFF.
static OVERLAY_BTN: AtomicBool = AtomicBool::new(true);
/// Overlay-open button top-left in egui POINTS (`ovbtnpos x y` / caps `btnpos=`).
static OVBTN_POS: Mutex<(f32, f32)> = Mutex::new(crate::overlay::BTN_POS_DEFAULT);
/// Live engage state from the app (`engaged 0|1`) — drives cursor visibility.
static ENGAGED_R: AtomicBool = AtomicBool::new(false);
/// `--mode remote` (the host-pointer marker and its cursor un-hide are remote-only).
static REMOTE_MODE: AtomicBool = AtomicBool::new(false);

/// Session audio truth (app stdin `audio tx=1 mute=0 mic=0`) for the overlay's Ses
/// section — same statics + defaults as linux.rs; synced into `ostate` each frame so
/// the Ses toggles highlight the real state (the app re-seeds these on respawn).
static AUDIO_TX: AtomicBool = AtomicBool::new(true);
static AUDIO_MUTE: AtomicBool = AtomicBool::new(false);
static MIC_ON: AtomicBool = AtomicBool::new(false);
/// Transient helper tooltip / toast: (text, armed-at, visible-secs).
static HINT: Mutex<Option<(String, std::time::Instant, f32)>> = Mutex::new(None);
/// Host's active encode summary (`hostenc <label>` stdin) — the overlay's per-field
/// host stats ("kodlama ms", target Mbit) parse out of this (overlay::host_parts).
static HOST_ENC: Mutex<String> = Mutex::new(String::new());
/// Caps line payload (codecs, encoders, active codec, active encoder, conn label) —
/// applied to the overlay state on the next paint.
#[allow(clippy::type_complexity)]
static CAPS_SEED: Mutex<Option<(Vec<String>, Vec<String>, String, String, String)>> =
	Mutex::new(None);
/// Host monitors `(idx, label)` from the caps line `displays=` field — the overlay's
/// Display-section screen picker (mirrors linux.rs / desktop.rs DISPLAYS). Empty / single =
/// no picker. Copied into `ostate.displays` each frame (apply_caps_seed).
static DISPLAYS: Mutex<Vec<(u32, String)>> = Mutex::new(Vec::new());
/// Stream-selection respawn seeds (C14): pushed by the app over stdin after a codec/monitor
/// switch respawn so the fresh renderer's overlay shows the user's last picks, not defaults.
/// Take-once (Option/sentinel) so the overlay can update them live after the initial seed.
static RES_SEED: Mutex<Option<String>> = Mutex::new(None);
static FPS_SEL_SEED: Mutex<Option<String>> = Mutex::new(None);
static BITRATE_SEED: Mutex<Option<String>> = Mutex::new(None);
static QUALITY_SEED: Mutex<Option<String>> = Mutex::new(None);
/// `u32::MAX` = not set (sentinel); any real display index is < MAX.
static DISPLAY_IDX_SEED: std::sync::atomic::AtomicU32 =
	std::sync::atomic::AtomicU32::new(u32::MAX);
// Cursor side-channel state (Moonlight model, mirrors linux.rs): the host captured
// WITHOUT a hardware cursor (KMS zero-copy) and streams the pointer out-of-band; WE
// draw it over the video. Fed over stdin: `cursor <x> <y>` (normalized 0..1),
// `cursorimg w h hx hy <b64png>`, `cursorhide`.
static CURSOR_POS: Mutex<Option<(f32, f32)>> = Mutex::new(None);
static CURSOR_IMG: Mutex<Option<CursorImg>> = Mutex::new(None);
static CURSOR_IMG_GEN: AtomicU64 = AtomicU64::new(0);
/// True while the stream is really stopped: the app said so (stdin `stopped 1`) or its stdin
/// closed (the app is gone). It used to be set by 3 s without a decodable AU, which on a lossy
/// link (a keyframe that never arrives whole) released the input capture and showed "stream
/// stopped" over a session that was still alive — ending a session is the app's call. Cleared
/// by `stopped 0` or as soon as AUs arrive again. Mirrors video::STALLED on Linux — kept
/// separate because video.rs is unix-only.
static STALLED: AtomicBool = AtomicBool::new(false);
/// No decodable AU for a while although the session lives: the depacketizer waits for a
/// keyframe / RFI recovery frame (`stream::rtp::WAITING`, after `RECOVER_AFTER`), or nothing at
/// all arrived for `STALL_SECS` while the app's keepalive went quiet too. Shows the
/// non-blocking "recovering connection" pill; input capture stays as it is.
static RECOVERING: AtomicBool = AtomicBool::new(false);
/// When the last `rtt` stdin line arrived (the app's keepalive pong, ~every 2 s while the peer
/// answers). A static host screen can legitimately send no video for seconds; a fresh pong then
/// means the session is fine and no "recovering" pill is due (linux.rs RTT_AT_MS parity).
static RTT_AT: Mutex<Option<std::time::Instant>> = Mutex::new(None);
/// Connected controllers pushed by the app over stdin (`ctrls slot:kind:name[:uuid:target],...`).
/// Game mode only; empty list in remote mode or when no pads are connected. Copied
/// into `ostate.controllers` each frame (paint_overlay + paint_closed parity).
/// Tuple: (slot, kind_label, device_name, uuid, target, rumble, disabled). Legacy short lines
/// default the missing tail. `rumble` = per-pad vibration level; `disabled` = pad toggled off.
static CONTROLLERS: Mutex<Vec<(u8, String, String, String, String, String, bool)>> = Mutex::new(Vec::new());
/// SPLIT MODE: pad uuids LOCKED to THIS session (this renderer == one pane), parsed from the
/// `ctrls` line's 8th per-pad field. Copied into `ostate.controllers_locked` each frame. Stored
/// as a Vec (const-constructible — `HashSet::new()` is not const) and collected on read. Empty
/// when split mode is off. Mirrors linux.rs's CONTROLLERS_LOCKED.
static CONTROLLERS_LOCKED: Mutex<Vec<String>> = Mutex::new(Vec::new());

#[derive(Clone)]
struct CursorImg {
	w: usize,
	h: usize,
	hot_x: f32,
	hot_y: f32,
	rgba: Vec<u8>,
}

/// Decode the side-channel cursor PNG (RGBA, ≤256², dims must match) — same
/// validation as linux.rs::decode_cursor_png.
fn decode_cursor_png(b64: &str, w: usize, h: usize) -> Option<Vec<u8>> {
	if w == 0 || h == 0 || w > 256 || h > 256 {
		return None;
	}
	let bytes = base64_decode(b64)?;
	let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
	let mut reader = decoder.read_info().ok()?;
	// Reject a PNG whose HEADER dimensions don't match the announced (already ≤256²)
	// w×h BEFORE allocating — `output_buffer_size()` is derived from the header's own
	// dimensions, so a tiny PNG declaring e.g. 40000×40000 would otherwise allocate
	// multiple GB and OOM/crash the renderer.
	let (hw, hh) = reader.info().size();
	if hw as usize != w || hh as usize != h {
		return None;
	}
	let mut buf = vec![0u8; reader.output_buffer_size()?];
	let info = reader.next_frame(&mut buf).ok()?;
	if info.color_type != png::ColorType::Rgba
		|| info.width as usize != w
		|| info.height as usize != h
	{
		return None;
	}
	buf.truncate(info.buffer_size());
	Some(buf)
}

/// Minimal standard-base64 decoder (same as linux.rs — no base64 crate for one payload).
fn base64_decode(s: &str) -> Option<Vec<u8>> {
	fn val(c: u8) -> Option<u8> {
		match c {
			b'A'..=b'Z' => Some(c - b'A'),
			b'a'..=b'z' => Some(c - b'a' + 26),
			b'0'..=b'9' => Some(c - b'0' + 52),
			b'+' => Some(62),
			b'/' => Some(63),
			_ => None,
		}
	}
	let mut out = Vec::with_capacity(s.len() * 3 / 4);
	let (mut acc, mut bits) = (0u32, 0u32);
	for &c in s.as_bytes() {
		if c == b'=' || c == b'\r' || c == b'\n' || c == b' ' {
			continue;
		}
		acc = (acc << 6) | val(c)? as u32;
		bits += 6;
		if bits >= 8 {
			bits -= 8;
			out.push((acc >> bits) as u8);
		}
	}
	Some(out)
}

/// Live overlay-button drag (closed state): (down.x, down.y, btn0.x, btn0.y, moved) in
/// egui points. The webview drag hotspot is buried under this child on Windows, so the
/// renderer owns the drag: ≤3 pt = click (toggle), more = move + persist via
/// `ov set btnpos x,y`.
static BTN_DRAG: Mutex<Option<(f32, f32, f32, f32, bool)>> = Mutex::new(None);
const HINT_SECS: f32 = 3.0;
const HINT_FADE: f32 = 0.5;
const TOAST_SECS: f32 = 6.0;
/// "banner" text never expires on its own — the app clears it explicitly.
const BANNER_SECS: f32 = 1.0e9;

fn arm_hint(kind: &str) {
	let text = crate::i18n::t(if kind == "engage" {
		"hint.engage"
	} else {
		"hint.click"
	});
	*HINT.lock().unwrap() = Some((text.to_string(), std::time::Instant::now(), HINT_SECS));
}

/// Overlay scale at 100 % display scaling (96 DPI): the Linux backend's ppp, so a standard
/// display looks the same on both platforms.
const OVERLAY_PPP_BASE: f32 = 1.25;
/// Live overlay scale (egui pixels per point) as f32 bits = BASE x the window's DPI / 96. A
/// fixed 1.25 drew the menu button + menu at the same PHYSICAL size on every display, so on
/// a 200 % laptop panel (and 4K/8K screens) they came out half the size of the app UI.
static OVERLAY_PPP_BITS: AtomicU32 = AtomicU32::new(0x3FA0_0000); // 1.25f32

/// Pacing queue: hard cap, whatever the adaptive depth says.
const PACE_QCAP: usize = 4;

/// The most pictures the pacer keeps while the queue keeps running dry: game 2 (latency),
/// remote 3 (smoothness) — the Linux pacer's ceilings.
fn pace_ceiling(mode: crate::overlay::Mode) -> usize {
	match mode {
		crate::overlay::Mode::Game => 2,
		crate::overlay::Mode::Remote => 3,
	}
}

/// The queue depth at each of the last 32 refreshes; [`PaceDepths::push`] returns their
/// minimum. A recent minimum of 0 or 1 = the queue keeps running dry (jitter), so the pacer may
/// hold up to the ceiling to bridge the next gap; above 1 = a standing backlog, trim to one.
#[derive(Default)]
struct PaceDepths {
	hist: [u16; 32],
	idx: usize,
}

impl PaceDepths {
	fn push(&mut self, depth: usize) -> usize {
		let n = self.hist.len();
		self.hist[self.idx % n] = depth.min(u16::MAX as usize) as u16;
		self.idx = self.idx.wrapping_add(1);
		self.hist.iter().copied().min().unwrap_or(0) as usize
	}
}

#[cfg(test)]
mod pace_tests {
	use super::*;
	#[test]
	fn the_depth_signal_is_the_recent_minimum() {
		let mut d = PaceDepths::default();
		// A queue that ran dry a moment ago keeps the minimum at 0 for the window.
		assert_eq!(d.push(0), 0);
		for _ in 0..31 {
			assert_eq!(d.push(2), 0);
		}
		// 32 refreshes with two waiting: a standing backlog → trim.
		assert_eq!(d.push(2), 2);
		assert_eq!(pace_ceiling(crate::overlay::Mode::Game), 2);
		assert_eq!(pace_ceiling(crate::overlay::Mode::Remote), 3);
	}
}

/// AU-backlog overflow reports, at most one line per [`OVERFLOW_LOG_EVERY`] with what happened
/// since the last one. Every overflow used to print its own line: a renderer that decoded
/// nothing (its D3D device gone while the display was off) wrote one per arriving frame —
/// 2.6 million lines, 415 MB of app log, in one night (2026-11-14).
#[derive(Default)]
struct OverflowLog {
	/// Stale AUs dropped ahead of a queued keyframe since the last line.
	skipped: u32,
	/// Oldest AUs shed with no keyframe to skip to (each asks for an IDR) since the last line.
	shed: u32,
	last: Option<std::time::Instant>,
}

const OVERFLOW_LOG_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

impl OverflowLog {
	/// Count one overflow — `dropped` AUs, `shed` = the oldest one went with no keyframe queued
	/// — and return the line to print when one is due (the first at once).
	fn note(&mut self, dropped: u32, shed: bool, now: std::time::Instant) -> Option<String> {
		if shed {
			self.shed += dropped;
		} else {
			self.skipped += dropped;
		}
		if self.last.is_some_and(|t| now.duration_since(t) < OVERFLOW_LOG_EVERY) {
			return None;
		}
		self.last = Some(now);
		let (skipped, shed) = (std::mem::take(&mut self.skipped), std::mem::take(&mut self.shed));
		let mut parts = Vec::new();
		if skipped > 0 {
			parts.push(format!("dropped {skipped} stale AU(s) ahead of newest keyframe"));
		}
		if shed > 0 {
			parts.push(format!("no keyframe queued; dropped the oldest AU {shed} time(s), keyframe requested"));
		}
		Some(format!("pulsar-render(mac): AU backlog overflow — {}", parts.join("; ")))
	}
}

#[cfg(test)]
mod overflow_log_tests {
	use super::*;

	#[test]
	fn one_line_per_two_seconds_with_the_counts_since_the_last() {
		let t0 = std::time::Instant::now();
		let at = |ms: u64| t0 + std::time::Duration::from_millis(ms);
		let mut log = OverflowLog::default();
		// The first overflow is reported at once.
		assert_eq!(
			log.note(40, false, at(0)).as_deref(),
			Some("pulsar-render(mac): AU backlog overflow — dropped 40 stale AU(s) ahead of newest keyframe")
		);
		// A frame every 8 ms shed for two seconds: silent, then one line with the count.
		let mut lines = Vec::new();
		for i in 1..=250u64 {
			lines.extend(log.note(1, true, at(i * 8)));
		}
		assert_eq!(lines.len(), 1, "{lines:?}");
		assert!(lines[0].contains("dropped the oldest AU 250 time(s), keyframe requested"), "{lines:?}");
		// Both kinds within one window share a line.
		assert_eq!(log.note(63, false, at(2100)), None);
		assert_eq!(log.note(1, true, at(2200)), None);
		let line = log.note(1, true, at(4100)).unwrap();
		assert!(line.contains("dropped 63 stale AU(s)") && line.contains("2 time(s)"), "{line}");
	}
}

/// stdin reader (same protocol as linux.rs/desktop.rs): `open`/`close` toggle the overlay,
/// `stat …` feeds the HUD, `pace 0|1` toggles frame pacing, `chat`/`fsjson`/`kin` feed the
/// overlay's Chat + Files views (the app sends these platform-agnostically).
fn stdin_control() {
	use std::io::BufRead;
	let stdin = std::io::stdin();
	for line in stdin.lock().lines() {
		let Ok(l) = line else { break };
		let l = l.trim();
		if l == "open" {
			OPEN.store(true, Ordering::SeqCst);
		} else if l == "close" {
			OPEN.store(false, Ordering::SeqCst);
		} else if let Some(rest) = l.strip_prefix("stat ") {
			let v: Vec<f32> = rest
				.split_whitespace()
				.filter_map(|x| x.parse().ok())
				.collect();
			if v.len() >= 4 {
				*STATS.lock().unwrap() = [v[0], v[1], v[2], v[3]];
			}
		} else if let Some(rest) = l.strip_prefix("pace ") {
			PACE.store(rest.trim() != "0", Ordering::SeqCst);
		} else if let Some(rest) = l.strip_prefix("rfi ") {
			// The host heals losses by reference-frame invalidation (stream/rtp.rs `RFI_MODE`).
			crate::stream::rtp::RFI_MODE.store(rest.trim() != "0", Ordering::SeqCst);
		} else if let Some(rest) = l.strip_prefix("ir ") {
			// The host now streams with a rolling intra refresh (no IDRs): resume after a gap
			// instead of waiting for a keyframe (stream/rtp.rs `IR_MODE`).
			crate::stream::rtp::IR_MODE.store(rest.trim() != "0", Ordering::SeqCst);
		} else if let Some(rest) = l.strip_prefix("cursor ") {
			// Side-channel cursor position (normalized 0..1 over the host frame).
			let v: Vec<f32> = rest
				.split_whitespace()
				.filter_map(|x| x.parse().ok())
				.collect();
			if v.len() >= 2 {
				*CURSOR_POS.lock().unwrap() = Some((v[0].clamp(0.0, 1.0), v[1].clamp(0.0, 1.0)));
			}
		} else if let Some(rest) = l.strip_prefix("cursorimg ") {
			let mut it = rest.split_whitespace();
			let w = it.next().and_then(|v| v.parse::<usize>().ok());
			let h = it.next().and_then(|v| v.parse::<usize>().ok());
			let hx = it.next().and_then(|v| v.parse::<f32>().ok());
			let hy = it.next().and_then(|v| v.parse::<f32>().ok());
			let b64 = it.next().unwrap_or("");
			if let (Some(w), Some(h), Some(hx), Some(hy)) = (w, h, hx, hy) {
				if let Some(rgba) = decode_cursor_png(b64, w, h) {
					*CURSOR_IMG.lock().unwrap() = Some(CursorImg {
						w,
						h,
						hot_x: hx,
						hot_y: hy,
						rgba,
					});
					CURSOR_IMG_GEN.fetch_add(1, Ordering::SeqCst);
				}
			}
		} else if l.trim() == "reveal" {
			REVEAL.store(true, Ordering::SeqCst);
		} else if l == "cursorhide" {
			*CURSOR_POS.lock().unwrap() = None;
		} else if let Some(rest) = l.strip_prefix("engaged ") {
			// Live engage state (cursor visibility) — app-side edges, like linux.rs.
			let v = rest.trim();
			ENGAGED_R.store(v == "1" || v == "on" || v == "true", Ordering::SeqCst);
		} else if let Some(rest) = l.strip_prefix("audio ") {
			// Session audio truth from the app (`audio tx=1 mute=0 mic=0`) so the
			// overlay's Ses toggles highlight the real state — same key=value parse
			// as linux.rs (the app sends this platform-agnostically + on respawn).
			for kv in rest.split_whitespace() {
				if let Some((k, v)) = kv.split_once('=') {
					let on = v == "1" || v == "on";
					match k {
						"tx" => AUDIO_TX.store(on, Ordering::SeqCst),
						"mute" => AUDIO_MUTE.store(on, Ordering::SeqCst),
						"q" => crate::overlay::set_audio_quality(v),
						"clip" => crate::overlay::set_clip_sync(on),
						"mic" => MIC_ON.store(on, Ordering::SeqCst),
						_ => {}
					}
				}
			}
		} else if let Some(rest) = l.strip_prefix("hint ") {
			arm_hint(rest.trim());
		} else if let Some(rest) = l.strip_prefix("toast ") {
			let rest = rest.trim();
			if !rest.is_empty() {
				*HINT.lock().unwrap() =
					Some((rest.to_string(), std::time::Instant::now(), TOAST_SECS));
			}
		} else if l.trim() == "banner" {
			*HINT.lock().unwrap() = None;
		} else if let Some(rest) = l.strip_prefix("banner ") {
			// Persistent status banner (empty text clears it): the app's no-video /
			// decode-failed diagnostics — the only text the viewer can see over the video.
			let rest = rest.trim();
			*HINT.lock().unwrap() = if rest.is_empty() {
				None
			} else {
				Some((rest.to_string(), std::time::Instant::now(), BANNER_SECS))
			};
		} else if let Some(rest) = l.strip_prefix("rotate ") {
			// Host display rotation (deg CW) the stream still carries — the Blt rotates it
			// upright (view::ROTATION). Flag the displayed dims for a re-report: a 90/270
			// change swaps the aspect the app letterboxes input over.
			if let Ok(d) = rest.trim().parse::<u32>() {
				if view::rotation() != d % 360 {
					view::set_rotation(d);
					ROT_DIRTY.store(true, Ordering::SeqCst);
				}
			}
		} else if let Some(rest) = l.strip_prefix("msens ") {
			crate::overlay::set_mouse_sens(rest);
		} else if let Some(rest) = l.strip_prefix("ptrfree ") {
			crate::overlay::set_ptr_free(rest.trim() == "1");
		} else if let Some(rest) = l.strip_prefix("hostptr ") {
			crate::overlay::host_ptr_line(rest);
		} else if let Some(rest) = l.strip_prefix("hostptrshow ") {
			crate::overlay::set_host_ptr_show(rest.trim() == "1");
		} else if let Some(rest) = l.strip_prefix("hostname ") {
			crate::overlay::set_host_name(rest);
		} else if let Some(rest) = l.strip_prefix("fit ") {
			// View-fit mode pushed by the frontend (persisted value / respawn re-seed).
			view::set_fit(rest.trim());
		} else if let Some(rest) = l.strip_prefix("hostenc ") {
			*HOST_ENC.lock().unwrap() = rest.trim().to_string();
		} else if let Some(rest) = l.strip_prefix("statshud ") {
			let v = rest.trim();
			STATS_HUD.store(v == "1" || v == "on" || v == "true", Ordering::SeqCst);
		} else if let Some(rest) = l.strip_prefix("curlabel ") {
			let v = rest.trim();
			CUR_LABEL.store(v == "1" || v == "on" || v == "true", Ordering::SeqCst);
		} else if let Some(rest) = l.strip_prefix("ovbtn ") {
			let v = rest.trim();
			OVERLAY_BTN.store(v == "1" || v == "on" || v == "true", Ordering::SeqCst);
		} else if let Some(rest) = l.strip_prefix("ovbtnpos ") {
			let v: Vec<f32> = rest
				.split_whitespace()
				.filter_map(|x| x.parse().ok())
				.collect();
			if v.len() >= 2 {
				*OVBTN_POS.lock().unwrap() = (v[0], v[1]);
			}
		} else if let Some(rest) = l.strip_prefix("caps ") {
			// Host caps + active request + persisted chrome seeds — same fields as
			// linux.rs: codecs/encoders filter the overlay menu rows, codec/encoder
			// preselect, conn labels the transport, statshud/ovbtn/btnpos re-seed
			// the closed-state chrome after a respawn.
			let (mut codecs, mut encoders) = (Vec::new(), Vec::new());
			let (mut codec, mut encoder, mut conn) = (String::new(), String::new(), String::new());
			for kv in rest.split_whitespace() {
				if let Some((k, v)) = kv.split_once('=') {
					let on = v == "1" || v == "on" || v == "true";
					match k {
						"codecs" => codecs = v.split(',').map(str::to_string).collect(),
						"encoders" => encoders = v.split(',').map(str::to_string).collect(),
						"codec" => codec = v.to_string(),
						"encoder" => encoder = v.to_string(),
						"conn" => conn = v.to_string(),
						"displays" => *DISPLAYS.lock().unwrap() = crate::overlay::parse_displays(v),
						"statshud" => STATS_HUD.store(on, Ordering::SeqCst),
						"curlabel" => CUR_LABEL.store(on, Ordering::SeqCst),
						"ovbtn" => OVERLAY_BTN.store(on, Ordering::SeqCst),
						"btnpos" => {
							if let Some((x, y)) = v.split_once(',') {
								if let (Ok(x), Ok(y)) = (x.parse::<f32>(), y.parse::<f32>()) {
									*OVBTN_POS.lock().unwrap() = (x, y);
								}
							}
						}
						_ => {}
					}
				}
			}
			*CAPS_SEED.lock().unwrap() = Some((codecs, encoders, codec, encoder, conn));
		} else if let Some(rest) = l.strip_prefix("maxdelay ") {
			// Adaptive streaming Phase 0.5: the app's RTT-derived RTP reorder wait (µs) for
			// the depacketizer's jitter buffer. (`hold` is not needed here: the depacketizer
			// already withholds everything after a gap until the next keyframe.)
			if let Ok(us) = rest.trim().parse::<u64>() {
				crate::stream::rtp::set_max_delay_us(us);
			}
		} else if let Some(rest) = l.strip_prefix("rtt ") {
			// Keepalive RTT from the app — overrides the latency tile (linux.rs parity), sizes
			// the reorder buffer's early give-up on an unrepairable hole, and proves the
			// session is alive (RTT_AT).
			// `rtt <ms> [<jitter ms>]`.
			let mut it = rest.split_whitespace();
			if let Some(ms) = it.next().and_then(|v| v.parse::<f32>().ok()) {
				RTT_DMS.store((ms * 10.0) as u32, Ordering::Relaxed);
				crate::stream::rtp::set_rtt_ms(ms);
				*RTT_AT.lock().unwrap() = Some(std::time::Instant::now());
				if let Some(j) = it.next().and_then(|v| v.parse::<f32>().ok()) {
					JITTER_DMS.store((j.max(0.0) * 10.0) as u32, Ordering::Relaxed);
				}
			}
		} else if let Some(rest) = l.strip_prefix("stopped ") {
			// The app's verdict that the stream really stopped (`stopped 1`) or is back
			// (`stopped 0`): the "stream stopped" card + input-capture release (see STALLED).
			STALLED.store(rest.trim() == "1", Ordering::SeqCst);
		} else if let Some(rest) = l.strip_prefix("upscale ") {
			// Picture upscaling mode (off|auto|fsr|ai) — see view::set_upscale.
			view::set_upscale(rest);
			crate::overlay::set_upscale_mode(rest);
		} else if let Some(rest) = l.strip_prefix("reparent ") {
			// The session moved to another app window: hide until its screen reports a rect.
			let h = parse_handle(rest);
			if h != 0 {
				*VIEW_RECT.lock().unwrap() = Some((0, 0, 0, 0));
				NEW_PARENT.store(h, Ordering::SeqCst);
			}
		} else if let Some(rest) = l.strip_prefix("viewrect ") {
			// In-app embed rect (physical px) — see VIEW_RECT above.
			let v: Vec<i32> = rest
				.split_whitespace()
				.filter_map(|x| x.parse().ok())
				.collect();
			if v.len() >= 4 {
				*VIEW_RECT.lock().unwrap() = Some((v[0], v[1], v[2], v[3]));
			}
		} else if let Some(rest) = l.strip_prefix("chat ") {
			// `chat in <text>` (host) / `chat out <text>` (our own, echoed by the app).
			let mut it = rest.splitn(2, ' ');
			let dir = it.next().unwrap_or("in");
			let text = it.next().unwrap_or("").trim().to_string();
			if !text.is_empty() {
				let mut log = CHAT_LOG.lock().unwrap();
				log.push((dir == "out", text));
				if log.len() > 200 {
					log.remove(0);
				}
				if dir != "out" {
					CHAT_UNREAD.fetch_add(1, Ordering::SeqCst);
					let mut t = CHAT_TOASTS.lock().unwrap();
					t.push((log.last().map(|m| m.1.clone()).unwrap_or_default(), std::time::Instant::now()));
					let over = t.len().saturating_sub(crate::overlay::BUBBLE_MAX);
					t.drain(..over);
				}
			}
		} else if let Some(json) = l.strip_prefix("fsjson ") {
			// Remote file listing for the Files view (same one-line JSON as linux.rs):
			// {"path":"…","entries":[{"name":…,"dir":…,"size":…},…]}.
			if let Ok(v) = serde_json::from_str::<serde_json::Value>(json) {
				let path = v["path"].as_str().unwrap_or("").to_string();
				let rows = v["entries"]
					.as_array()
					.map(|a| {
						a.iter()
							.filter_map(|e| {
								Some(crate::overlay::FsRow {
									name: e["name"].as_str()?.to_string(),
									dir: e["dir"].as_bool().unwrap_or(false),
									size: e["size"].as_u64().unwrap_or(0),
								})
							})
							.collect()
					})
					.unwrap_or_default();
				*FS_REMOTE.lock().unwrap() = (path, rows);
			}
		} else if let Some(rest) = l.strip_prefix("kin ") {
			// Relayed keyboard for the Chat composer (`kin t <text>` / `kin k <name>`,
			// same protocol as linux.rs) — wndproc translates no WM_CHAR/WM_KEYDOWN.
			let mut it = rest.splitn(2, ' ');
			match it.next() {
				Some("t") => {
					let text = it.next().unwrap_or("").to_string();
					if !text.is_empty() {
						EGUI_EVENTS.lock().unwrap().push(egui::Event::Text(text));
					}
				}
				Some("k") => {
					let key = match it.next().map(str::trim) {
						Some("backspace") => Some(egui::Key::Backspace),
						Some("enter") => {
							ENTER_IN.store(true, Ordering::SeqCst);
							None
						}
						Some("left") => Some(egui::Key::ArrowLeft),
						Some("right") => Some(egui::Key::ArrowRight),
						_ => None,
					};
					if let Some(key) = key {
						let mut q = EGUI_EVENTS.lock().unwrap();
						for pressed in [true, false] {
							q.push(egui::Event::Key {
								key,
								physical_key: None,
								pressed,
								repeat: false,
								modifiers: egui::Modifiers::default(),
							});
						}
					}
				}
				_ => {}
			}
		// Stream-selection respawn seeds (C14 — mirrors linux.rs): the app pushes these
		// after a codec/monitor-switch respawn so the fresh renderer's Stream/Display views
		// show the user's last picks instead of falling back to built-in defaults.
		} else if let Some(rest) = l.strip_prefix("res ") {
			*RES_SEED.lock().unwrap() = Some(rest.trim().to_string());
		} else if let Some(rest) = l.strip_prefix("fps ") {
			*FPS_SEL_SEED.lock().unwrap() = Some(rest.trim().to_string());
		} else if let Some(rest) = l.strip_prefix("bitrate ") {
			*BITRATE_SEED.lock().unwrap() = Some(rest.trim().to_string());
		} else if let Some(rest) = l.strip_prefix("quality ") {
			*QUALITY_SEED.lock().unwrap() = Some(rest.trim().to_string());
		} else if let Some(rest) = l.strip_prefix("display ") {
			if let Ok(idx) = rest.trim().parse::<u32>() {
				DISPLAY_IDX_SEED.store(idx, Ordering::SeqCst);
			}
		} else if let Some(tok) = l.strip_prefix("k ") {
			// Controller overlay-nav (`k up|down|left|right|go|escape`, emitted by play.rs
			// while the overlay is open; same protocol as linux.rs). Windows never handled
			// it, so a pad could not drive the menu here at all (2026-11-11).
			let key = match tok.trim() {
				"left" => Some(egui::Key::ArrowLeft),
				"right" => Some(egui::Key::ArrowRight),
				"up" | "shifttab" => Some(egui::Key::ArrowUp),
				"down" | "tab" => Some(egui::Key::ArrowDown),
				"go" => Some(egui::Key::Enter),
				"escape" => Some(egui::Key::Escape),
				_ => None,
			};
			if let Some(key) = key {
				let mut q = EGUI_EVENTS.lock().unwrap();
				for pressed in [true, false] {
					q.push(egui::Event::Key {
						key,
						physical_key: None,
						pressed,
						repeat: false,
						modifiers: egui::Modifiers::default(),
					});
				}
			}
		} else if l.trim_end() == "hpvol" || l.starts_with("hpvol ") {
			// DualSense headphone volume bars: `hpvol uuid:volume:rrggbb,...` (overlay.rs).
			crate::overlay::set_headsets(l.trim_end().strip_prefix("hpvol").unwrap_or(""));
		} else if let Some(rest) = l.strip_prefix("ctrls ") {
			// Connected controller list (game mode only):
			// `ctrls slot:kind:name:uuid:target:rumble,...`. 6-field (per-pad vibration);
			// 5/3-field legacy forms default the missing tail (rumble="medium", etc).
			// Underscores for spaces in kind/name; uuid/target/rumble are never underscored.
			// Mirrors linux.rs's CONTROLLERS static + parse (same protocol).
			let rest = rest.trim();
			let mut locked: Vec<String> = Vec::new();
			let list: Vec<(u8, String, String, String, String, String, bool)> = if rest.is_empty() {
				Vec::new()
			} else {
				rest.split(',')
					.filter_map(|e| {
						let mut p = e.splitn(8, ':');
						let slot: u8 = p.next()?.parse().ok()?;
						let kind = p.next()?.replace('_', " ");
						let name = p.next()?.replace('_', " ");
						let uuid = p.next().unwrap_or("").to_string();
						let target = p.next().unwrap_or("auto").to_string();
						let rumble = p.next().unwrap_or("medium").to_string();
						let disabled = p.next() == Some("1");
						// SPLIT MODE: 8th field = locked-to-this-session (1). Absent on
						// 7-field (split-off) lines → defaults unlocked.
						if p.next() == Some("1") {
							locked.push(uuid.clone());
						}
						Some((slot, kind, name, uuid, target, rumble, disabled))
					})
					.collect()
			};
			*CONTROLLERS.lock().unwrap() = list;
			*CONTROLLERS_LOCKED.lock().unwrap() = locked;
		}
		// `toast <text>` (and `hint …`) are drawn by the closed-state paint pass
		// (`paint_closed` via `draw_hint`), alongside the mini stats HUD and the
		// overlay-open button — see paint_closed below.
	}
	// stdin closed: the app is gone (or dropped us) — nothing will feed this stream again.
	STALLED.store(true, Ordering::SeqCst);
}

/// Draw the side-channel cursor with egui's painter (own foreground layer, over the
/// video and under nothing) — same shape as linux.rs's paint_cursor closure.
fn paint_side_cursor(ctx: &egui::Context, draw: Option<(egui::Pos2, egui::TextureId, egui::Vec2)>) {
	if let Some((pos, id, size)) = draw {
		let painter = ctx.layer_painter(egui::LayerId::new(
			egui::Order::Foreground,
			egui::Id::unique("pulsar-cursor-layer"),
		));
		painter.image(
			id,
			egui::Rect::from_min_size(pos, size),
			egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
			egui::Color32::WHITE,
		);
	}
}

/// Apply one overlay command to the local overlay state and emit its `ov …` stdout line
/// (render_stats.rs parses them) — the Windows counterpart of linux.rs/desktop.rs `emit_cmd`.
fn apply_ov_cmd(ostate: &mut crate::overlay::OverlayState, c: crate::overlay::OverlayCmd) {
	match c {
		crate::overlay::OverlayCmd::Set(field, val) => {
			match field {
				"codec" => ostate.codec = val.clone(),
				"encoder" => ostate.encoder = val.clone(),
				"decoder" => ostate.decoder = val.clone(),
				"res" => ostate.res = val.clone(),
				"fps" => ostate.fps_sel = val.clone(),
				"bitrate" => ostate.bitrate = val.clone(),
				"quality" => ostate.quality = val.clone(),
				"pace" => {
					ostate.pace = val == "on";
					// Takes effect now (the app's `pace 0|1` echo only confirms it).
					PACE.store(ostate.pace, Ordering::SeqCst);
				}
				// Local echo for the closed-state chrome toggles: flip the statics
				// NOW so the row + HUD react instantly; the frontend's stdin echo
				// (`statshud`/`ovbtn`) just confirms the same value later.
				"statshud" => STATS_HUD.store(val == "on", Ordering::SeqCst),
				"curlabel" => CUR_LABEL.store(val == "on", Ordering::SeqCst),
				"ovbtn" => OVERLAY_BTN.store(val == "on", Ordering::SeqCst),
				// Audio toggles apply optimistically (linux.rs parity); the app's
				// `audio …` line re-syncs the truth after the host acknowledges.
				"atx" => AUDIO_TX.store(val == "on", Ordering::SeqCst),
				"amute" => AUDIO_MUTE.store(val == "on", Ordering::SeqCst),
				"aquality" => crate::overlay::set_audio_quality(&val),
				"mic" => MIC_ON.store(val == "on", Ordering::SeqCst),
				// Voice call = mic + host audio together (paired optimistic update).
				// ON enables BOTH; OFF drops ONLY the mic and leaves host audio
				// as-is (it has its own `atx` row). The overlay highlight derives
				// from MIC_ON alone (overlay::draw_audio) so it stays in sync.
				"call" => {
					let on = val == "on";
					MIC_ON.store(on, Ordering::SeqCst);
					if on {
						AUDIO_TX.store(true, Ordering::SeqCst);
					}
				}
				// View fit is renderer-local (instant Blt rect change) + forwarded
				// so the frontend mirrors/persists it — Linux-parity (linux.rs).
				"fit" => view::set_fit(&val),
				"msens" => crate::overlay::set_mouse_sens(&val),
				"ptrfree" => crate::overlay::set_ptr_free(val == "on"),
				"hostptr" => crate::overlay::set_host_ptr_show(val == "on"),
				// Upscaling picked in the menu: applied now, forwarded for the app to save.
				"upscale" => view::set_upscale(&val),
				// Optimistic controller swap: the overlay row ▲/▼ was clicked.
				// Swap the two entries in CONTROLLERS immediately so the list
				// re-renders at the next frame without waiting for the gilrs
				// reader's next `ctrls` line (~16 ms later). The frontend will
				// re-emit the canonical ctrls line via set_controller_order, which
				// confirms the same state.
				"ctrlswap" => {
					if let Some((ai, bi)) = val.split_once(',') {
						if let (Ok(ai), Ok(bi)) = (ai.parse::<usize>(), bi.parse::<usize>()) {
							let mut ctrls = CONTROLLERS.lock().unwrap();
							let len = ctrls.len();
							if ai < len && bi < len {
								ctrls.swap(ai, bi);
							}
						}
					}
				}
				// Optimistic emulation-target update: the overlay picker was clicked.
				// Update the matching row's target in CONTROLLERS by uuid so the
				// row re-renders immediately without waiting for the next ctrls line.
				"ctrlemu" => {
					if let Some((uuid, target)) = val.split_once(',') {
						let mut ctrls = CONTROLLERS.lock().unwrap();
						for row in ctrls.iter_mut() {
							if row.3 == uuid {
								row.4 = target.to_string();
								break;
							}
						}
					}
				}
				// Optimistic PER-PAD vibration update: `val` = "uuid,level". Reflect it
				// locally (the matching pad row's 6th field) so the highlight moves
				// immediately; the frontend confirms via set_controller_rumble → ctrls.
				"ctrlrumble" => {
					if let Some((uuid, level)) = val.split_once(',') {
						let mut ctrls = CONTROLLERS.lock().unwrap();
						for row in ctrls.iter_mut() {
							if row.3 == uuid {
								row.5 = level.to_string();
								break;
							}
						}
					}
				}
				// Optimistic enable/disable SET: `val` = "uuid,state" (1 = disabled,
				// 0 = enabled). Apply to the matching pad so the row updates immediately.
				"ctrldisable" => {
					if let Some((uuid, state)) = val.split_once(',') {
						let mut ctrls = CONTROLLERS.lock().unwrap();
						for row in ctrls.iter_mut() {
							if row.3 == uuid {
								row.6 = state == "1";
								break;
							}
						}
					}
				}
				// Optimistic SPLIT-MODE lock SET: `val` = "uuid,state" (1 = lock to this
				// session, 0 = unlock). Reflect locally so the row's Kilitli/Serbest
				// updates immediately; play.rs confirms via the ctrls line's 8th field.
				// The lock is applied app-side (set_controller_lock, via `ov set ctrllock`).
				"ctrllock" => {
					if let Some((uuid, state)) = val.split_once(',') {
						let mut locked = CONTROLLERS_LOCKED.lock().unwrap();
						locked.retain(|u| u != uuid);
						if state == "1" {
							locked.push(uuid.to_string());
						}
					}
				}
				_ => {}
			}
			println!("ov set {field} {val}");
		}
		crate::overlay::OverlayCmd::End => println!("ov end"),
		crate::overlay::OverlayCmd::Close => {
			OPEN.store(false, Ordering::SeqCst);
			println!("ov close");
		}
		// The click that closed the menu takes control too (follows `ov close`).
		crate::overlay::OverlayCmd::TakeControl => println!("ov control"),
		// Same wire lines as the Linux backend (render_stats.rs parses them).
		crate::overlay::OverlayCmd::Chat(t) => println!("ov chat {t}"),
		crate::overlay::OverlayCmd::FsLs(p) => println!("ov fsls {p}"),
		crate::overlay::OverlayCmd::FsGet(p) => println!("ov fsget {p}"),
		crate::overlay::OverlayCmd::FsSend(p) => println!("ov fssend {p}"),
		crate::overlay::OverlayCmd::OpenFiles => println!("ov files"),
	}
}
// ---- macOS-specific plumbing ---------------------------------------------------------------

/// `--hold-reveal`: stay hidden past the first frame until the app's `reveal` line.
static HOLD_REVEAL: AtomicBool = AtomicBool::new(false);
static REVEAL: AtomicBool = AtomicBool::new(false);
/// Safety net: show anyway this long after the first drawn frame if `reveal` never comes.
const REVEAL_FALLBACK: std::time::Duration = std::time::Duration::from_secs(10);

fn overlay_ppp() -> f32 {
	f32::from_bits(OVERLAY_PPP_BITS.load(Ordering::Relaxed))
}

/// Overlay scale = the Linux/Windows 100 % scale × the display's backing scale (Retina 2×),
/// so the menu has the size of the app UI around it.
fn refresh_overlay_ppp(backing: f64) -> bool {
	let ppp = OVERLAY_PPP_BASE * backing.max(1.0) as f32;
	OVERLAY_PPP_BITS.swap(ppp.to_bits(), Ordering::Relaxed) != ppp.to_bits()
}

/// The closed-state overlay button as fractions of the panel (None while hidden / open).
fn ovbtn_frac(size_px: (u32, u32)) -> Option<(f32, f32, f32, f32)> {
	if !OVERLAY_BTN.load(Ordering::SeqCst) || OPEN.load(Ordering::SeqCst) {
		return None;
	}
	let ppp = overlay_ppp();
	let (sw, sh) = (size_px.0 as f32 / ppp, size_px.1 as f32 / ppp);
	if sw <= 0.0 || sh <= 0.0 {
		return None;
	}
	let bs = crate::overlay::BTN_SIZE;
	let (bx, by) = *OVBTN_POS.lock().unwrap();
	let bx = bx.clamp(0.0, (sw - bs).max(0.0));
	let by = by.clamp(0.0, (sh - bs).max(0.0));
	Some((bx / sw, by / sh, ((bx + bs) / sw).min(1.0), ((by + bs) / sh).min(1.0)))
}

/// The overlay button's rect in egui points (clamped like `overlay::draw_open_button`).
fn ovbtn_rect(sw: f32, sh: f32) -> (f32, f32, f32) {
	let bs = crate::overlay::BTN_SIZE;
	let (bx, by) = *OVBTN_POS.lock().unwrap();
	(bx.clamp(0.0, (sw - bs).max(0.0)), by.clamp(0.0, (sh - bs).max(0.0)), bs)
}

/// Parse a window number written as decimal or `0x`-hex.
fn parse_handle(s: &str) -> u64 {
	let s = s.trim();
	if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
		u64::from_str_radix(hex, 16).unwrap_or(0)
	} else {
		s.parse().unwrap_or(0)
	}
}

fn out_line(line: &str) {
	use std::io::Write as _;
	let mut o = std::io::stdout();
	let _ = writeln!(o, "{line}");
	let _ = o.flush();
}

/// The Metal device driving `display` (retained), else the system default.
fn device_for(display: u32) -> Option<Retained<ProtocolObject<dyn MTLDevice>>> {
	// SAFETY: a +1 `id<MTLDevice>` (or null); `from_raw` takes that reference over.
	unsafe {
		let p = ffi::CGDirectDisplayCopyCurrentMetalDevice(display) as *mut ProtocolObject<dyn MTLDevice>;
		Retained::from_raw(p).or_else(|| objc2_metal::MTLCreateSystemDefaultDevice())
	}
}

/// A pointer event of the panel, in egui points.
fn event_pos(ev: &NSEvent, win: &window::Panel) -> egui::Pos2 {
	let p = ev.locationInWindow();
	let (_, h) = win.size_pt();
	let k = win.scale as f32 / overlay_ppp();
	egui::pos2(p.x as f32 * k, (h - p.y) as f32 * k)
}

/// One mouse event that reached the panel (it only takes the mouse while the overlay is open or
/// the pointer is on the closed-state button): the overlay button's click / drag, or egui input.
fn handle_mouse(ev: &NSEvent, win: &window::Panel) {
	let t = ev.r#type();
	let pos = event_pos(ev, win);
	let ppp = overlay_ppp();
	let (sw, sh) = (win.size_px.0 as f32 / ppp, win.size_px.1 as f32 / ppp);
	if !OPEN.load(Ordering::SeqCst) {
		if t == NSEventType::LeftMouseDown && OVERLAY_BTN.load(Ordering::SeqCst) {
			let (bx, by, bs) = ovbtn_rect(sw, sh);
			if pos.x >= bx && pos.x <= bx + bs && pos.y >= by && pos.y <= by + bs {
				*BTN_DRAG.lock().unwrap() = Some((pos.x, pos.y, bx, by, false));
			}
		} else if t == NSEventType::LeftMouseDragged {
			if let Some((dx0, dy0, bx0, by0, moved)) = BTN_DRAG.lock().unwrap().as_mut() {
				let (dx, dy) = (pos.x - *dx0, pos.y - *dy0);
				if dx.abs() + dy.abs() > 3.0 {
					*moved = true;
				}
				if *moved {
					let bs = crate::overlay::BTN_SIZE;
					*OVBTN_POS.lock().unwrap() =
						((*bx0 + dx).clamp(0.0, (sw - bs).max(0.0)), (*by0 + dy).clamp(0.0, (sh - bs).max(0.0)));
				}
			}
		} else if t == NSEventType::LeftMouseUp {
			let taken = BTN_DRAG.lock().unwrap().take();
			if let Some((_, _, _, _, moved)) = taken {
				if moved {
					let (x, y) = *OVBTN_POS.lock().unwrap();
					out_line(&format!("ov set btnpos {x:.0},{y:.0}"));
				} else {
					out_line("ov toggle");
				}
			}
		}
		return;
	}
	// A drag that started while closed and ends after the overlay opened: just disarm.
	if t == NSEventType::LeftMouseUp && BTN_DRAG.lock().unwrap().take().is_some() {
		return;
	}
	let m = egui::Modifiers::default();
	let mut q = EGUI_EVENTS.lock().unwrap();
	match t {
		NSEventType::MouseMoved | NSEventType::LeftMouseDragged | NSEventType::RightMouseDragged => {
			*POINTER.lock().unwrap() = (pos.x, pos.y);
			q.push(egui::Event::PointerMoved(pos));
		}
		NSEventType::LeftMouseDown | NSEventType::LeftMouseUp => q.push(egui::Event::PointerButton {
			pos,
			button: egui::PointerButton::Primary,
			pressed: t == NSEventType::LeftMouseDown,
			modifiers: m,
		}),
		NSEventType::RightMouseDown | NSEventType::RightMouseUp => q.push(egui::Event::PointerButton {
			pos,
			button: egui::PointerButton::Secondary,
			pressed: t == NSEventType::RightMouseDown,
			modifiers: m,
		}),
		NSEventType::ScrollWheel => {
			let dy = ev.scrollingDeltaY() as f32;
			let (unit, dy) = if ev.hasPreciseScrollingDeltas() {
				(egui::MouseWheelUnit::Point, dy)
			} else {
				(egui::MouseWheelUnit::Line, dy)
			};
			q.push(egui::Event::MouseWheel {
				unit,
				delta: egui::vec2(0.0, dy),
				phase: egui::TouchPhase::Move,
				source: egui::MouseWheelSource::Wheel,
				modifiers: m,
			});
		}
		_ => {}
	}
}

/// Should the panel take the mouse right now (instead of letting it reach the app)?
fn wants_mouse(win: &window::Panel) -> bool {
	if OPEN.load(Ordering::SeqCst) || BTN_DRAG.lock().unwrap().is_some() {
		return true;
	}
	if !OVERLAY_BTN.load(Ordering::SeqCst) || !win.shown {
		return false;
	}
	let Some((x, y)) = win.pointer() else { return false };
	let ppp = overlay_ppp();
	let k = win.scale as f32 / ppp;
	let (px, py) = (x as f32 * k, y as f32 * k);
	let (sw, sh) = (win.size_px.0 as f32 / ppp, win.size_px.1 as f32 / ppp);
	let (bx, by, bs) = ovbtn_rect(sw, sh);
	px >= bx && px <= bx + bs && py >= by && py <= by + bs
}

/// Exit when the app is gone (macOS has no job object / PDEATHSIG): the parent process id
/// changes when it dies (we are re-parented to launchd).
fn watch_parent() {
	extern "C" {
		fn getppid() -> i32;
	}
	// SAFETY: plain syscall.
	let first = unsafe { getppid() };
	std::thread::spawn(move || loop {
		std::thread::sleep(std::time::Duration::from_secs(1));
		// SAFETY: plain syscall.
		if unsafe { getppid() } != first {
			std::process::exit(0);
		}
	});
}

/// Metal + the decode → present chain + the egui overlay (the panel lives outside it, so a GPU
/// rebuild keeps the window).
struct Renderer {
	gpu: metal::Gpu,
	decoder: Option<decode::Decoder>,
	codec: Codec,
	// Decoded (unrotated) picture size; drives the placement + `vidsink-dims`.
	src_w: u32,
	src_h: u32,
	// Last picture shown, re-drawn under the overlay while the video stalls.
	last: Option<decode::Frame>,
	// Newest picture of an AU burst decoded without presenting (see `on_access_unit`).
	held_back: Option<decode::Frame>,
	// Frame pacing: pictures leave ONE per display refresh (see `pace_tick`).
	pace_q: std::collections::VecDeque<decode::Frame>,
	pace_refresh: u64,
	pace_depths: PaceDepths,
	mode: crate::overlay::Mode,
	frames: u32,
	last_fps_at: std::time::Instant,
	bytes: u64,
	dec_ms_acc: f32,
	dec_n: u32,
	// The decoder announced to the app (`vidsink-dec`), and whether it ran in hardware.
	dec_announced: Option<bool>,
	revealed: bool,
	last_vrect: (f32, f32, f32, f32),
	last_ovbtn: Option<(f32, f32, f32, f32)>,
	first_drawn: Option<std::time::Instant>,
	cursor_tex: Option<egui::TextureHandle>,
	cursor_gen: u64,
	cursor_hot: (f32, f32),
	cursor_dims: (f32, f32),
	egui_ctx: egui::Context,
	ostate: crate::overlay::OverlayState,
	ov_ui: crate::overlay::UiState,
	was_open: bool,
}

fn new_egui(backing: f64) -> egui::Context {
	let ctx = egui::Context::default();
	crate::overlay::apply_theme(&ctx);
	refresh_overlay_ppp(backing);
	ctx.set_pixels_per_point(overlay_ppp());
	ctx
}

impl Renderer {
	fn new(gpu: metal::Gpu, codec: Codec, mode: crate::overlay::Mode, backing: f64) -> Self {
		Self {
			gpu,
			decoder: None,
			codec,
			src_w: 0,
			src_h: 0,
			last: None,
			held_back: None,
			pace_q: std::collections::VecDeque::new(),
			pace_refresh: 0,
			pace_depths: PaceDepths::default(),
			mode,
			frames: 0,
			last_fps_at: std::time::Instant::now(),
			bytes: 0,
			dec_ms_acc: 0.0,
			dec_n: 0,
			dec_announced: None,
			revealed: false,
			last_vrect: (-1.0, -1.0, -1.0, -1.0),
			last_ovbtn: None,
			first_drawn: None,
			cursor_tex: None,
			cursor_gen: 0,
			cursor_hot: (0.0, 0.0),
			cursor_dims: (0.0, 0.0),
			egui_ctx: new_egui(backing),
			ostate: crate::overlay::OverlayState { mode, conn_label: "P2P".into(), ..Default::default() },
			ov_ui: Default::default(),
			was_open: false,
		}
	}

	fn sync_cursor_tex(&mut self) {
		let gen = CURSOR_IMG_GEN.load(Ordering::SeqCst);
		if gen == self.cursor_gen {
			return;
		}
		self.cursor_gen = gen;
		if let Some(c) = CURSOR_IMG.lock().unwrap().clone() {
			let color = egui::ColorImage::from_rgba_unmultiplied([c.w, c.h], &c.rgba);
			self.cursor_tex = Some(self.egui_ctx.load_texture("pulsar-side-cursor", color, egui::TextureOptions::NEAREST));
			self.cursor_hot = (c.hot_x, c.hot_y);
			self.cursor_dims = (c.w as f32, c.h as f32);
		}
	}

	/// A host-screen point (normalized) on the panel, in egui points (fit mode applied).
	fn video_point(&self, size: (u32, u32), nx: f32, ny: f32) -> Option<egui::Pos2> {
		if self.src_w == 0 || self.src_h == 0 {
			return None;
		}
		let (vw, vh) = view::displayed_dims(self.src_w, self.src_h);
		let (iw, ih) = (vw as f32, vh as f32);
		let ((sx, sy, sw, sh), (dx, dy, dw, dh)) = view::fit_rects(iw, ih, size.0 as f32, size.1 as f32);
		let (kx, ky) = (dw / sw.max(1.0), dh / sh.max(1.0));
		Some(egui::pos2((dx + (nx * iw - sx) * kx) / overlay_ppp(), (dy + (ny * ih - sy) * ky) / overlay_ppp()))
	}

	fn cursor_draw(&self, size: (u32, u32)) -> Option<(egui::Pos2, egui::TextureId, egui::Vec2)> {
		let (nx, ny) = (*CURSOR_POS.lock().unwrap())?;
		let tex = self.cursor_tex.as_ref()?;
		if self.src_w == 0 || self.src_h == 0 {
			return None;
		}
		let (vw, vh) = view::displayed_dims(self.src_w, self.src_h);
		let (iw, ih) = (vw as f32, vh as f32);
		let ((sx, sy, sw, sh), (dx, dy, dw, dh)) = view::fit_rects(iw, ih, size.0 as f32, size.1 as f32);
		let (kx, ky) = (dw / sw.max(1.0), dh / sh.max(1.0));
		let px = dx + (nx * iw - sx) * kx;
		let py = dy + (ny * ih - sy) * ky;
		let pos = egui::pos2((px - self.cursor_hot.0 * kx) / overlay_ppp(), (py - self.cursor_hot.1 * ky) / overlay_ppp());
		let sz = egui::vec2(self.cursor_dims.0 * kx / overlay_ppp(), self.cursor_dims.1 * ky / overlay_ppp());
		Some((pos, tex.id(), sz))
	}

	fn apply_caps_seed(&mut self) {
		if let Some((codecs, encoders, codec, encoder, conn)) = CAPS_SEED.lock().unwrap().take() {
			self.ostate.host_codecs = codecs;
			self.ostate.host_encoders = encoders;
			if !codec.is_empty() {
				self.ostate.codec = codec;
			}
			if !encoder.is_empty() {
				self.ostate.encoder = encoder;
			}
			if !conn.is_empty() {
				self.ostate.conn_label = conn;
			}
		}
		if let Some(v) = RES_SEED.lock().unwrap().take() {
			self.ostate.res = v;
		}
		if let Some(v) = FPS_SEL_SEED.lock().unwrap().take() {
			self.ostate.fps_sel = v;
		}
		if let Some(v) = BITRATE_SEED.lock().unwrap().take() {
			self.ostate.bitrate = v;
		}
		if let Some(v) = QUALITY_SEED.lock().unwrap().take() {
			self.ostate.quality = v;
		}
		let idx = DISPLAY_IDX_SEED.load(Ordering::SeqCst);
		if idx != u32::MAX {
			self.ostate.display_idx = idx;
			DISPLAY_IDX_SEED.store(u32::MAX, Ordering::SeqCst);
		}
		self.ostate.displays = DISPLAYS.lock().unwrap().clone();
	}

	/// Shared live numbers for the overlay / HUD (open and closed).
	fn sync_stats(&mut self) {
		let s = *STATS.lock().unwrap();
		if s[0] > 0.0 {
			self.ostate.fps = s[0];
			self.ostate.latency_ms = s[1];
			self.ostate.decode_ms = s[2];
			self.ostate.mbps = s[3];
		}
		let rtt = RTT_DMS.load(Ordering::Relaxed) as f32 / 10.0;
		if rtt > 0.0 {
			self.ostate.latency_ms = rtt;
		}
		self.ostate.jitter_ms = crate::overlay::jitter_from_dms(JITTER_DMS.load(Ordering::Relaxed));
		self.ostate.host_active = HOST_ENC.lock().unwrap().clone();
		self.apply_caps_seed();
	}

	/// Run egui for this frame (the open menu, or the closed-state chrome) and return its
	/// tessellated output, the textures already uploaded. None = nothing to draw.
	fn overlay_prims(&mut self, size: (u32, u32)) -> Option<Vec<egui::ClippedPrimitive>> {
		let open = OPEN.load(Ordering::SeqCst);
		if open != self.was_open {
			if open {
				self.ov_ui.remote_requested = false;
			} else {
				EGUI_EVENTS.lock().unwrap().clear();
				ENTER_IN.store(false, Ordering::SeqCst);
				for c in crate::overlay::take_deferred(&mut self.ov_ui) {
					apply_ov_cmd(&mut self.ostate, c);
				}
				use std::io::Write;
				let _ = std::io::stdout().flush();
			}
			self.was_open = open;
		}
		self.sync_cursor_tex();
		let cursor_draw = self.cursor_draw(size);
		let screen = Some(egui::Rect::from_min_size(
			egui::pos2(0.0, 0.0),
			egui::vec2(size.0 as f32 / overlay_ppp(), size.1 as f32 / overlay_ppp()),
		));
		let stalled = STALLED.load(Ordering::Relaxed);
		let recovering = !stalled && RECOVERING.load(Ordering::Relaxed);
		let mut out = if open {
			self.sync_stats();
			self.ostate.open = true;
			self.ostate.pace = PACE.load(Ordering::SeqCst);
			self.ostate.stats_hud = STATS_HUD.load(Ordering::SeqCst);
			self.ostate.cur_label = CUR_LABEL.load(Ordering::SeqCst);
			self.ostate.overlay_btn = OVERLAY_BTN.load(Ordering::SeqCst);
			self.ostate.btn_pos = *OVBTN_POS.lock().unwrap();
			self.ostate.audio_tx = AUDIO_TX.load(Ordering::SeqCst);
			self.ostate.audio_mute = AUDIO_MUTE.load(Ordering::SeqCst);
			self.ostate.mic_on = MIC_ON.load(Ordering::SeqCst);
			self.ostate.fit = view::fit_label().to_string();
			let events = std::mem::take(&mut *EGUI_EVENTS.lock().unwrap());
			let raw = egui::RawInput { screen_rect: screen, events, ..Default::default() };
			let ostate = &mut self.ostate;
			ostate.chat = CHAT_LOG.lock().unwrap().clone();
			let in_chat = self.ov_ui.view == crate::overlay::View::Chat;
			let seen = if in_chat { CHAT_UNREAD.swap(0, Ordering::SeqCst) } else { 0 };
			if in_chat {
				CHAT_TOASTS.lock().unwrap().clear();
			}
			crate::overlay::mark_new_chat(&mut self.ov_ui, in_chat, ostate.chat.len(), seen);
			ostate.chat_unread = CHAT_UNREAD.load(Ordering::SeqCst);
			{
				let (p, rows) = &*FS_REMOTE.lock().unwrap();
				ostate.fs_remote_path = p.clone();
				ostate.fs_remote = rows.clone();
			}
			ostate.chat_enter = ENTER_IN.swap(false, Ordering::SeqCst);
			ostate.controllers = CONTROLLERS.lock().unwrap().clone();
			ostate.controllers_locked = CONTROLLERS_LOCKED.lock().unwrap().iter().cloned().collect();
			let ov_ui = &mut self.ov_ui;
			let mut cmds = Vec::new();
			let out = self.egui_ctx.run_pass(raw, |ctx| {
				cmds = crate::overlay::draw(ctx, ostate, ov_ui);
				if stalled {
					crate::overlay::draw_stalled(ctx);
				} else if recovering {
					crate::overlay::draw_recovering(ctx);
				}
				paint_side_cursor(ctx, cursor_draw);
			});
			for c in cmds {
				apply_ov_cmd(&mut self.ostate, c);
			}
			use std::io::Write;
			let _ = std::io::stdout().flush();
			out
		} else {
			crate::overlay::mark_new_chat(&mut self.ov_ui, false, 0, 0);
			let host_draw = crate::overlay::host_ptr_visible(self.ostate.mode)
				.map(|(p, a)| (p.and_then(|(x, y)| self.video_point(size, x, y)), a));
			let hint_text: Option<(String, f32)> = {
				let mut g = HINT.lock().unwrap();
				let mut out = None;
				if let Some((text, t0, secs)) = g.as_ref() {
					let el = t0.elapsed().as_secs_f32();
					if el >= *secs {
						*g = None;
					} else {
						out = Some((text.clone(), ((secs - el) / HINT_FADE).min(1.0)));
					}
				}
				out
			};
			let stats_hud = STATS_HUD.load(Ordering::SeqCst);
			let ovbtn = OVERLAY_BTN.load(Ordering::SeqCst);
			let toasts = chat_toasts();
			if !stats_hud
				&& !ovbtn
				&& hint_text.is_none()
				&& cursor_draw.is_none()
				&& host_draw.is_none()
				&& !stalled
				&& !recovering
				&& toasts.is_empty()
			{
				return None;
			}
			self.ostate.chat_toasts = toasts;
			self.sync_stats();
			self.ostate.open = false;
			self.ostate.stats_hud = stats_hud;
			self.ostate.overlay_btn = ovbtn;
			self.ostate.btn_pos = *OVBTN_POS.lock().unwrap();
			self.ostate.chat_unread = CHAT_UNREAD.load(Ordering::SeqCst);
			let raw = egui::RawInput { screen_rect: screen, ..Default::default() };
			let ostate = &self.ostate;
			self.egui_ctx.run_pass(raw, |ctx| {
				if stalled {
					crate::overlay::draw_stalled(ctx);
				} else if recovering {
					crate::overlay::draw_recovering(ctx);
				}
				if stats_hud {
					crate::overlay::draw_hud(ctx, ostate);
				}
				if ovbtn {
					let _ = crate::overlay::draw_open_button(ctx, ostate);
				}
				if let Some((text, alpha)) = &hint_text {
					crate::overlay::draw_hint(ctx, text, *alpha);
				}
				crate::overlay::draw_chat_bubbles(ctx, ostate);
				paint_side_cursor(ctx, cursor_draw);
				if let Some((p, a)) = host_draw {
					crate::overlay::draw_host_pointer(ctx, p, a);
				}
			})
		};
		self.gpu.update_textures(&mut out.textures_delta);
		Some(self.egui_ctx.tessellate(out.shapes, self.egui_ctx.pixels_per_point()))
	}

	/// Draw `frame` (or the idle background) + the overlay on the panel's layer.
	fn draw(&mut self, win: &window::Panel, frame: Option<&decode::Frame>) -> bool {
		let size = win.size_px;
		let prims = self.overlay_prims(size);
		let video = frame.map(|f| {
			let (iw, ih) = f.size();
			let (vw, vh) = view::displayed_dims(iw, ih);
			let (src, dst) = view::fit_rects(vw as f32, vh as f32, size.0 as f32, size.1 as f32);
			metal::VideoDraw { frame: f, src, dst }
		});
		let ppp = self.egui_ctx.pixels_per_point();
		self.gpu.render(size, video, view::upscale_on(), prims.as_deref().map(|p| (p, ppp)))
	}

	/// Decode one AU; `show` = present its picture(s) now (a burst's older AUs are decoded
	/// with `show == false`, their newest picture waits in `held_back`).
	fn on_access_unit(&mut self, win: &mut window::Panel, au: &AccessUnit, show: bool) {
		let dec = self.decoder.get_or_insert_with(|| decode::Decoder::new(self.codec));
		self.bytes += au.data.len() as u64;
		let t0 = std::time::Instant::now();
		let frames = match dec.decode(au) {
			Ok(f) => f,
			Err(e) => {
				// No session yet (waiting for parameter sets), a lost session or bad data: ask
				// for a keyframe (again every 500 ms while this repeats). A lost session is
				// rebuilt on the next AU from the kept parameter sets anyway, so an intra-refresh
				// or RFI stream heals even if the IDR never comes.
				if dec.failed || dec.hw.is_some() {
					eprintln!("pulsar-render(mac): decode: {e}");
				}
				dec.failed = false;
				stream::rtp::NEED_IDR.store(true, Ordering::Relaxed);
				return;
			}
		};
		if let Some(hw) = dec.hw {
			if self.dec_announced != Some(hw) {
				self.dec_announced = Some(hw);
				out_line(&format!("vidsink-dec videotoolbox {}", if hw { "hw" } else { "sw" }));
			}
		}
		self.dec_ms_acc += t0.elapsed().as_secs_f32() * 1000.0;
		self.dec_n += 1;
		if !show {
			if let Some(f) = frames.into_iter().last() {
				self.held_back = Some(f);
			}
			return;
		}
		if !frames.is_empty() {
			self.held_back = None;
		}
		for f in frames {
			self.deliver(win, f);
		}
	}

	fn deliver(&mut self, win: &mut window::Panel, f: decode::Frame) {
		if !PACE.load(Ordering::Relaxed) {
			self.show_frame(win, f);
			return;
		}
		self.pace_q.push_back(f);
		while self.pace_q.len() > PACE_QCAP {
			self.pace_q.pop_front();
		}
	}

	/// Frame pacing: one picture per display refresh, oldest first (`win/mod.rs` pace_tick;
	/// the refresh clock is the display link of the panel's display).
	fn pace_tick(&mut self, win: &mut window::Panel, refresh: Option<u64>) {
		if !PACE.load(Ordering::Relaxed) {
			if let Some(newest) = self.pace_q.pop_back() {
				self.pace_q.clear();
				self.show_frame(win, newest);
			}
			return;
		}
		if let Some(r) = refresh {
			if r == self.pace_refresh {
				return;
			}
			self.pace_refresh = r;
		} else if self.pace_q.is_empty() {
			return;
		}
		let recent_min = self.pace_depths.push(self.pace_q.len());
		if self.pace_q.is_empty() {
			return;
		}
		let target = if recent_min <= 1 { pace_ceiling(self.mode) } else { 1 };
		while self.pace_q.len() > target {
			self.pace_q.pop_front();
		}
		if let Some(next) = self.pace_q.pop_front() {
			self.show_frame(win, next);
		}
	}

	fn show_frame(&mut self, win: &mut window::Panel, f: decode::Frame) {
		let (sw, sh) = f.size();
		let rot_dirty = ROT_DIRTY.swap(false, Ordering::SeqCst);
		if sw != self.src_w || sh != self.src_h || rot_dirty {
			self.src_w = sw;
			self.src_h = sh;
			if sw > 0 && sh > 0 {
				let (vw, vh) = view::displayed_dims(sw, sh);
				crate::overlay::set_stream_dims(vw, vh);
				out_line(&format!("vidsink-dims {vw}x{vh}"));
			}
		}
		// First picture: the panel shows from now on (unless the app holds it behind its
		// preparing screen, see `reveal_due`), so the loading screen gives way to video, never
		// to a black panel.
		self.first_drawn.get_or_insert_with(std::time::Instant::now);
		if self.reveal_due() {
			self.revealed = true;
			let rect = *VIEW_RECT.lock().unwrap();
			win.place(rect, true);
		}
		// Hidden (held back, tab inactive, app window minimized): no drawable to draw on — a
		// hidden layer's drawables never come back — but the stream is still counted, the app
		// reveals the picture off these numbers.
		if win.shown && self.draw(win, Some(&f)) {
			let size = win.size_px;
			let (vw, vh) = view::displayed_dims(sw, sh);
			let (_, d) = view::fit_rects(vw as f32, vh as f32, size.0 as f32, size.1 as f32);
			let (w, h) = (size.0.max(1) as f32, size.1.max(1) as f32);
			let fr = (d.0 / w, d.1 / h, (d.0 + d.2) / w, (d.1 + d.3) / h);
			if fr != self.last_vrect {
				self.last_vrect = fr;
				out_line(&format!("vidsink-vrect {:.5} {:.5} {:.5} {:.5}", fr.0, fr.1, fr.2, fr.3));
			}
			let b = ovbtn_frac(size);
			if b != self.last_ovbtn {
				self.last_ovbtn = b;
				match b {
					Some(b) => out_line(&format!("vidsink-ovbtn {:.5} {:.5} {:.5} {:.5}", b.0, b.1, b.2, b.3)),
					None => out_line("vidsink-ovbtn -"),
				}
			}
		}
		self.last = Some(f);
		self.tick_fps();
	}

	/// The first picture is there and the app does not hold it back (any more).
	fn reveal_due(&self) -> bool {
		let Some(first) = self.first_drawn else { return false };
		!self.revealed
			&& !(HOLD_REVEAL.load(Ordering::SeqCst) && !REVEAL.load(Ordering::SeqCst) && first.elapsed() < REVEAL_FALLBACK)
	}

	/// Re-draw the last picture (or the idle background) with the overlay over it.
	fn repaint_idle(&mut self, win: &window::Panel) {
		if !win.shown {
			return;
		}
		let last = self.last.clone();
		self.draw(win, last.as_ref());
	}

	fn tick_fps(&mut self) {
		self.frames += 1;
		let dt = self.last_fps_at.elapsed();
		if dt.as_secs_f32() >= 1.0 {
			let fps = self.frames as f32 / dt.as_secs_f32();
			let mbps = (self.bytes as f32 * 8.0) / dt.as_secs_f32() / 1_000_000.0;
			let dec = if self.dec_n > 0 { self.dec_ms_acc / self.dec_n as f32 } else { 0.0 };
			let (vw, vh) = view::displayed_dims(self.src_w, self.src_h);
			out_line(&format!("vidsink-fps {fps:.0} {vw}x{vh} {mbps:.1} {dec:.1}"));
			*STATS.lock().unwrap() = [fps, 0.0, dec, mbps];
			self.frames = 0;
			self.bytes = 0;
			self.dec_ms_acc = 0.0;
			self.dec_n = 0;
			self.last_fps_at = std::time::Instant::now();
		}
	}
}

pub fn run() {
	let args: Vec<String> = std::env::args().collect();
	let mut wid: u64 = 0;
	let mut sdp: Option<String> = None;
	let mut mode = crate::overlay::Mode::Game;
	let mut i = 1;
	while i < args.len() {
		match args[i].as_str() {
			"--wid" => {
				if let Some(s) = args.get(i + 1) {
					wid = parse_handle(s);
					i += 1;
				}
			}
			"--pace" => {
				if let Some(s) = args.get(i + 1) {
					PACE.store(s != "off", Ordering::SeqCst);
					i += 1;
				}
			}
			"--hold-reveal" => HOLD_REVEAL.store(true, Ordering::SeqCst),
			"--mode" => {
				if let Some(s) = args.get(i + 1) {
					mode = if s == "remote" { crate::overlay::Mode::Remote } else { crate::overlay::Mode::Game };
					REMOTE_MODE.store(s == "remote", Ordering::SeqCst);
					i += 1;
				}
			}
			"--lang" => i += 1,
			a if !a.starts_with("--") && sdp.is_none() => sdp = Some(a.to_string()),
			_ => {}
		}
		i += 1;
	}
	if wid == 0 || wid > u32::MAX as u64 {
		eprintln!("pulsar-render(mac): --wid <app windowNumber> required");
		std::process::exit(2);
	}
	let Some(sdp) = sdp else {
		eprintln!("pulsar-render(mac): <stream.sdp> required");
		std::process::exit(2);
	};
	let (port, codec) = match parse_sdp(&sdp) {
		Ok(v) => v,
		Err(e) => {
			eprintln!("pulsar-render(mac): SDP parse failed: {e}");
			std::process::exit(2);
		}
	};
	let Some(mtm) = MainThreadMarker::new() else {
		eprintln!("pulsar-render(mac): must run on the main thread");
		std::process::exit(1);
	};
	PARENT.store(wid, Ordering::SeqCst);
	watch_parent();
	std::thread::spawn(stdin_control);
	if let Err(e) = event_loop(mtm, wid as u32, port, codec, mode) {
		eprintln!("pulsar-render(mac): fatal: {e}");
		std::process::exit(1);
	}
}

fn event_loop(mtm: MainThreadMarker, parent: u32, port: u16, codec: Codec, mode: crate::overlay::Mode) -> Result<(), String> {
	// An accessory app: no Dock icon, no menu bar, never activated (the panel is
	// non-activating) — the app keeps the focus.
	let app = NSApplication::sharedApplication(mtm);
	app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
	app.finishLaunching();

	let build = |win: &window::Panel| -> Result<metal::Gpu, String> {
		let device = device_for(win.display).ok_or("no Metal device")?;
		metal::Gpu::new(device, win.layer.clone())
	};
	// Startup inside a pool too: the panel, layer and pipeline setup autorelease temporaries
	// (see the main loop below for why that matters here).
	let (mut win, first) = objc2::rc::autoreleasepool(|_| {
		let mut win = window::Panel::new(mtm, parent);
		win.place(None, false);
		let first = build(&win).map(|gpu| Renderer::new(gpu, codec, mode, win.scale));
		(win, first)
	});
	let mut renderer: Option<Renderer> = Some(first?);
	let mut link = window::DisplayLink::new();
	if link.is_none() {
		eprintln!("pulsar-render(mac): no display link — frame pacing shows pictures as they come");
	}

	// RTP receive on its own thread; AUs land in a bounded queue (same overflow rule as
	// win/mod.rs: drop the stale lead-in up to the newest keyframe past AU_SOFT_CAP, keep an
	// intact keyframe-less chain up to AU_HARD_CAP, then shed the oldest and ask for an IDR).
	let queue: std::sync::Arc<Mutex<std::collections::VecDeque<AccessUnit>>> =
		std::sync::Arc::new(Mutex::new(std::collections::VecDeque::new()));
	const AU_SOFT_CAP: usize = 8;
	const AU_HARD_CAP: usize = 64;
	let stop = std::sync::Arc::new(AtomicBool::new(false));
	{
		let q = queue.clone();
		let stop2 = stop.clone();
		let mut overflow_log = OverflowLog::default();
		std::thread::spawn(move || {
			stream::rtp::recv_loop(
				port,
				codec,
				|au| {
					let mut g = q.lock().unwrap();
					if g.len() >= AU_SOFT_CAP {
						match g.iter().rposition(|au| au.key) {
							Some(k) if k > 0 => {
								g.drain(..k);
								if let Some(line) = overflow_log.note(k as u32, false, std::time::Instant::now()) {
									eprintln!("{line}");
								}
							}
							_ if g.len() < AU_HARD_CAP => {}
							_ => {
								g.pop_front();
								stream::rtp::NEED_IDR.store(true, Ordering::Relaxed);
								if let Some(line) = overflow_log.note(1, true, std::time::Instant::now()) {
									eprintln!("{line}");
								}
							}
						}
					}
					g.push_back(au);
				},
				&stop2,
			);
		});
	}

	let mut last_needidr = std::time::Instant::now() - std::time::Duration::from_secs(1);
	let mut last_idle_paint = std::time::Instant::now();
	const STALL_SECS: u64 = 3;
	const RECOVER_AFTER: std::time::Duration = std::time::Duration::from_millis(400);
	const RTT_FRESH: std::time::Duration = std::time::Duration::from_secs(4);
	const AU_BURST: usize = 3;
	let mut last_au_at = std::time::Instant::now();
	let mut stalled_sent = false;
	let mut recovering_sent = false;
	let mut live_announced = false;
	// GPU loss / a display driven by another GPU: rebuilt in place, retried until it works.
	const REBUILD_EVERY: std::time::Duration = std::time::Duration::from_millis(500);
	const REBUILD_LOG_EVERY: u32 = 120;
	let mut lost: Option<(crate::overlay::OverlayState, crate::overlay::UiState, Option<decode::Decoder>)> = None;
	let mut rebuild_at: Option<std::time::Instant> = None;
	let mut rebuild_fails: u32 = 0;
	let mut device_check = std::time::Instant::now();
	// `PULSAR_TEST_DEVICE_LOSS=<after_s>,<failed_attempts>`, as on Windows.
	let mut test_loss: Option<(std::time::Instant, u32)> = std::env::var("PULSAR_TEST_DEVICE_LOSS").ok().and_then(|v| {
		let (after, fails) = v.split_once(',')?;
		let after = std::time::Duration::from_secs_f32(after.trim().parse().ok()?);
		Some((std::time::Instant::now() + after, fails.trim().parse().ok()?))
	});
	let mut test_fails_left: u32 = 0;

	loop {
		// Each iteration drains its own autorelease pool. The process never runs
		// -[NSApplication run], so no pool exists on the main thread otherwise: the NSEvents of
		// the pump, the command buffers, encoders and render pass descriptors, and above all the
		// CAMetalLayer drawables are autoreleased. Drawables only go back to the layer's small
		// pool when this pool drains; without it `nextDrawable` blocks for its 1 s timeout and
		// returns nil after a few frames, and memory grows every frame.
		let quit = objc2::rc::autoreleasepool(|_| -> bool {
			// ---- AppKit events (the panel's mouse; everything else is AppKit's own business).
			// SAFETY: a framework static NSString.
			let mode_key = unsafe { NSDefaultRunLoopMode };
			while let Some(ev) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
				NSEventMask::Any,
				Some(&NSDate::distantPast()),
				mode_key,
				true,
			) {
				if ev.windowNumber() == win.number() {
					handle_mouse(&ev, &win);
				}
				app.sendEvent(&ev);
			}

			// ---- Where the picture goes.
			let np = NEW_PARENT.swap(0, Ordering::SeqCst);
			if np != 0 && np <= u32::MAX as u64 {
				win.parent = np as u32;
				PARENT.store(np, Ordering::SeqCst);
				eprintln!("pulsar-render(mac): moved to window {np}");
			}
			let rect = *VIEW_RECT.lock().unwrap();
			if let Some(r) = renderer.as_mut() {
				// `reveal` arrived (or its fallback ran out) while no new picture came.
				if r.reveal_due() {
					r.revealed = true;
				}
			}
			let revealed = renderer.as_ref().is_some_and(|r| r.revealed);
			let resized = win.place(rect, revealed);
			if win.parent_gone && STALLED.load(Ordering::Relaxed) {
				// The app window and the app's stdin are both gone: nothing will show us again.
				return true;
			}
			win.set_take_mouse(wants_mouse(&win));
			// Hover while the menu is open: AppKit sends mouse-moved events to the KEY window only,
			// and the panel never becomes key, so the pointer is polled instead.
			if OPEN.load(Ordering::SeqCst) {
				if let Some((x, y)) = win.pointer() {
					let k = win.scale as f32 / overlay_ppp();
					let p = egui::pos2(x as f32 * k, y as f32 * k);
					let mut last = POINTER.lock().unwrap();
					if (last.0 - p.x).abs() + (last.1 - p.y).abs() > 0.25 {
						*last = (p.x, p.y);
						EGUI_EVENTS.lock().unwrap().push(egui::Event::PointerMoved(p));
					}
				}
			}
			if let Some(l) = link.as_mut() {
				l.set_display(win.display);
			}
			if refresh_overlay_ppp(win.scale) {
				if let Some(r) = renderer.as_mut() {
					r.egui_ctx.set_pixels_per_point(overlay_ppp());
				}
			}

			// ---- GPU loss / GPU switch.
			let test_now = test_loss.is_some_and(|(at, _)| std::time::Instant::now() >= at);
			let mut need_rebuild = renderer.as_ref().is_some_and(|r| r.gpu.lost) || test_now;
			if !need_rebuild && device_check.elapsed() >= std::time::Duration::from_secs(1) {
				device_check = std::time::Instant::now();
				if let (Some(r), Some(d)) = (renderer.as_ref(), device_for(win.display)) {
					need_rebuild = Retained::as_ptr(&d) != Retained::as_ptr(&r.gpu.device);
				}
			}
			if need_rebuild {
				if test_now {
					test_fails_left = test_loss.take().map_or(0, |(_, f)| f);
				}
				eprintln!("pulsar-render(mac): Metal device lost or changed — rebuilding renderer");
				if let Some(mut r) = renderer.take() {
					lost = Some((std::mem::take(&mut r.ostate), std::mem::take(&mut r.ov_ui), r.decoder.take()));
				}
				rebuild_at = None;
				rebuild_fails = 0;
			}
			if renderer.is_none() && rebuild_at.is_none_or(|t| t.elapsed() >= REBUILD_EVERY) {
				rebuild_at = Some(std::time::Instant::now());
				let built = if test_fails_left > 0 {
					test_fails_left -= 1;
					Err("test device loss".to_string())
				} else {
					build(&win)
				};
				match built {
					Ok(gpu) => {
						let mut fresh = Renderer::new(gpu, codec, mode, win.scale);
						if let Some((o, u, d)) = lost.take() {
							fresh.ostate = o;
							fresh.ov_ui = u;
							fresh.decoder = d;
							fresh.revealed = true;
						}
						if rebuild_fails > 0 {
							eprintln!("pulsar-render(mac): renderer rebuilt after {rebuild_fails} failed attempt(s)");
						}
						rebuild_fails = 0;
						stream::rtp::NEED_IDR.store(true, Ordering::Relaxed);
						renderer = Some(fresh);
						last_au_at = std::time::Instant::now();
					}
					Err(e) => {
						rebuild_fails += 1;
						if rebuild_fails == 1 || rebuild_fails % REBUILD_LOG_EVERY == 0 {
							eprintln!("pulsar-render(mac): renderer rebuild failed: {e} — retrying (attempt {rebuild_fails})");
						}
					}
				}
			}
			let Some(r) = renderer.as_mut() else {
				queue.lock().unwrap().clear();
				std::thread::sleep(std::time::Duration::from_millis(40));
				return false;
			};
			if resized {
				// The picture must follow a resize at once, not on the next frame.
				r.repaint_idle(&win);
			}

			// ---- Requests to the app.
			if let Some((first, last)) = stream::rtp::take_rfi_request() {
				out_line(&format!("vidsink-rfi {first} {last}"));
			}
			if last_needidr.elapsed() >= std::time::Duration::from_millis(500) && stream::rtp::NEED_IDR.swap(false, Ordering::Relaxed) {
				last_needidr = std::time::Instant::now();
				out_line("vidsink-needidr");
			}

			// ---- Decode + present.
			let aus: Vec<AccessUnit> = queue.lock().unwrap().drain(..).collect();
			if aus.is_empty() {
				if r.decoder.is_none() || r.last.is_none() {
					if last_idle_paint.elapsed() >= std::time::Duration::from_millis(16) {
						r.repaint_idle(&win);
						last_idle_paint = std::time::Instant::now();
					}
				} else {
					let gap = last_au_at.elapsed();
					let rtt_fresh = RTT_AT.lock().unwrap().is_some_and(|t| t.elapsed() < RTT_FRESH);
					let recovering = (stream::rtp::WAITING.load(Ordering::Relaxed) && gap >= RECOVER_AFTER)
						|| (gap.as_secs() >= STALL_SECS && !rtt_fresh);
					RECOVERING.store(recovering, Ordering::Relaxed);
					let stalled = STALLED.load(Ordering::Relaxed);
					let chrome = OPEN.load(Ordering::SeqCst)
						|| STATS_HUD.load(Ordering::SeqCst)
						|| OVERLAY_BTN.load(Ordering::SeqCst)
						|| HINT.lock().unwrap().is_some()
						|| CURSOR_POS.lock().unwrap().is_some()
						|| stalled
						|| recovering;
					if chrome && last_idle_paint.elapsed() >= std::time::Duration::from_millis(16) {
						r.repaint_idle(&win);
						last_idle_paint = std::time::Instant::now();
					}
				}
				std::thread::sleep(std::time::Duration::from_millis(2));
			} else {
				STALLED.store(false, Ordering::Relaxed);
				RECOVERING.store(false, Ordering::Relaxed);
				if !live_announced {
					live_announced = true;
					out_line("ov stalled 0");
					stalled_sent = false;
				}
				last_au_at = std::time::Instant::now();
				let burst = aus.len() >= AU_BURST;
				if burst && aus.len() >= AU_SOFT_CAP {
					eprintln!("pulsar-render(mac): AU burst of {} — decoded, presenting the newest only", aus.len());
				}
				let n = aus.len();
				for (i, au) in aus.iter().enumerate() {
					r.on_access_unit(&mut win, au, !burst || i + 1 == n);
				}
				if let Some(f) = r.held_back.take() {
					if burst && PACE.load(Ordering::Relaxed) {
						r.pace_q.clear();
					}
					r.deliver(&mut win, f);
				}
			}
			r.pace_tick(&mut win, link.as_ref().map(|l| l.refresh()));

			let stalled = STALLED.load(Ordering::Relaxed);
			if stalled != stalled_sent {
				stalled_sent = stalled;
				out_line(&format!("ov stalled {}", stalled as u8));
			}
			let recovering = RECOVERING.load(Ordering::Relaxed) && !stalled;
			if recovering != recovering_sent {
				recovering_sent = recovering;
				out_line(&format!("ov recovering {}", recovering as u8));
			}
			false
		});
		if quit {
			break;
		}
	}
	stop.store(true, Ordering::SeqCst);
	Ok(())
}
