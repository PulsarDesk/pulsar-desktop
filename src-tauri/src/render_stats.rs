//! Linux native-renderer stdout readers: parse the `vidsink-fps …` perf-HUD lines
//! into `play-vstats` events and the `ov …` overlay-interaction lines into the
//! frontend overlay events. Split out of `render` to keep each file cohesive.
//!
//! Linux/X11 + Windows + macOS (the native `pulsar-render` stdout uses the same `vidsink-fps`/
//! `ov …` protocol on all three; on macOS the renderer is overlay-only, so it emits the `ov …`
//! interaction lines but no `vidsink-fps` video stats — both readers tolerate that) — see
//! `render` for the rest of the Linux renderer plumbing. The functions only use portable std
//! APIs (`ChildStdout` + `Emitter`), so they compile on every platform.
#![cfg(any(unix, target_os = "windows"))]

/// The native renderer lost its reference picture (`vidsink-needidr`): the hold loop asks the
/// host for a keyframe on its next repair tick (see play::hold).
pub(crate) static RENDER_WANTS_IDR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// The native renderer lost the video packets `first..=last` (`vidsink-rfi`, reference-frame
/// invalidation mode): the hold loop asks the host to heal it without a keyframe. Merged until
/// the hold loop takes it.
pub(crate) static RENDER_WANTS_RFI: std::sync::Mutex<Option<(u16, u16)>> = std::sync::Mutex::new(None);
/// Wakes the hold loop the moment `RENDER_WANTS_RFI` is set: the host can only reach a lost
/// frame for ~8 frames (≈ 67 ms at 120 fps), so the request must not wait for a repair tick.
pub(crate) static RFI_WAKE: tokio::sync::Notify = tokio::sync::Notify::const_new();
/// The play id whose renderer was started with `--hold-reveal` (Windows): its first frames do
/// NOT end the preparing screen — play::hold reveals + emits play-ready once the adaptive
/// bitrate has settled. `FIRST_FRAMES_ID` = that renderer has decoded its first frames.
pub(crate) static HOLD_REVEAL_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);
pub(crate) static FIRST_FRAMES_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);
/// The Windows renderer's average decode time per frame (ms, f32 bits) from its last
/// `vidsink-fps` line; play::hold steps the fps down when the decoder can't keep up.
pub(crate) static DECODE_MS_BITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tauri::AppHandle;

use crate::events::PlayVStats;

/// Read `pulsar-vidsink`'s stdout (`vidsink-fps <fps> <w>x<h>` lines, ~1 Hz) and emit real
/// client fps to the overlay HUD. This is the vidsink's stats channel (it replaces mpv and
/// has no JSON-IPC socket). Unlike mpv 0.34, the vidsink reports a TRUE client fps. Runs on a
/// blocking thread (line-buffered pipe reads); exits when the child closes stdout (killed on
/// overlay-open or session end).
/// Linux-only: the legacy standalone-vidsink stats channel (the single-surface `pulsar-render`
/// path uses `start_render_reader` instead). Gated to Linux so Windows/macOS — which never call
/// it — don't trip a dead-code warning now that this module compiles on every platform.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn start_vidsink_stats(
	app: &AppHandle,
	id: u64,
	stdout: std::process::ChildStdout,
	overlay_stdin: Arc<Mutex<Option<std::process::ChildStdin>>>,
) {
	use std::io::{BufRead, Write};
	use tauri::Emitter;
	let app = app.clone();
	std::thread::spawn(move || {
		let reader = std::io::BufReader::new(stdout);
		for line in reader.lines() {
			let Ok(line) = line else { break };
			// "vidsink-fps <fps> <w>x<h> <mbit> <ms>"
			if let Some(rest) = line.strip_prefix("vidsink-fps ") {
				let mut it = rest.split_whitespace();
				let fps = it.next().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
				let _dims = it.next();
				let mbps = it.next().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
				let ms = it.next().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
				let _ = app.emit(
					"play-vstats",
					PlayVStats {
						id,
						fps,
						drops: 0,
						mbps,
						decode_ms: ms,
					},
				);
				// Feed the native overlay HUD: stat <fps> <latency> <decode> <mbps>. The vidsink's
				// real metric is the pipeline-buffer depth (ms) — use it for both latency + decode.
				if let Some(si) = overlay_stdin.lock().unwrap().as_mut() {
					let _ = writeln!(si, "stat {fps:.0} {ms:.0} {ms:.0} {mbps:.1}");
				}
			}
		}
	});
}

/// Read the single-surface `pulsar-render`'s stdout: BOTH the perf-HUD stats (`vidsink-fps <fps>
/// <wxh> <mbit> <ms>`) → `play-vstats`, AND the overlay interactions (`ov set/end/close`) →
/// frontend events. One process, one stdout, both line kinds.
///
/// `live_id`: when `Some`, the reader uses it to look up the CURRENT session id on every line —
/// so a resident renderer (kept alive across reconnects) can have its stats attributed to the new
/// session by storing the new id into the Arc before the next session starts. When `None`, `id`
/// is fixed for the lifetime of the reader (freshly-spawned, single-session renderer).
pub(crate) fn start_render_reader(
	app: &AppHandle,
	id: u64,
	stdout: std::process::ChildStdout,
	live_id: Option<Arc<AtomicU64>>,
) {
	use std::io::BufRead;
	use tauri::Emitter;
	let app = app.clone();
	std::thread::spawn(move || {
		let reader = std::io::BufReader::new(stdout);
		// First REAL frames (fps/bitrate > 0) ⇒ the stream is actually up — the UI keeps
		// its Connecting screen until this fires (one-shot per renderer process / session).
		// Reset on each reconnect (live_id path): the resident renderer starts streaming fresh
		// data, so play-ready must fire again for the new session id.
		let mut ready_sent_for: Option<u64> = None;
		// When the last `vidsink-fps` line arrived: the Windows renderer only prints it after
		// drawing a frame, so a long gap = no picture on screen (what the UI shows as "stream
		// stopped"). Logged when the stats resume so freezes are visible in the log.
		let mut last_fps_line: Option<std::time::Instant> = None;
		let mut first_line_logged = false;
		'lines: for line in reader.lines() {
			let Ok(line) = line else { break };
			// Resolve the current session id: either live (resident reconnect) or fixed.
			let cur_id = live_id
				.as_ref()
				.map(|a| a.load(Ordering::Relaxed))
				.unwrap_or(id);
			if !first_line_logged {
				first_line_logged = true;
				tracing::info!(%line, "renderer first stdout line");
			}
			// Loss hold bookkeeping from the renderer (adaptive streaming Phase 0.5):
			// `vidsink-hold <keyframe|deadline> <frames> <ms>` — into the session log so the
			// validation script can count freezes and their lengths.
			if let Some(rest) = line.strip_prefix("vidsink-hold ") {
				let mut it = rest.split_whitespace();
				let end = it.next().unwrap_or("?").to_string();
				let frames = it.next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
				let ms = it.next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
				tracing::info!(id = cur_id, end = %end, frames, ms, "renderer loss hold");
				continue;
			}
			if let Some(rest) = line.strip_prefix("vidsink-fps ") {
				let mut it = rest.split_whitespace();
				let fps = it.next().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
				let now = std::time::Instant::now();
				if let Some(prev) = last_fps_line.replace(now) {
					let gap = now.duration_since(prev);
					if gap > std::time::Duration::from_millis(2000) {
						tracing::info!(id = cur_id, gap_ms = gap.as_millis() as u64, fps, "renderer frame gap (no picture)");
					}
				}
				if fps > 0.0 && fps < 5.0 {
					tracing::info!(id = cur_id, fps, "renderer fps very low");
				}
				// The stats line carries the live stream dims ("<w>x<h>") — re-emit them as
				// play-dims EVERY tick (1 Hz, a dozen bytes). The one-shot vidsink-dims print —
				// and any change-deduped variant — races listener registration: a webview that
				// (re)mounts after the frame arrived (session restart, HMR, slow load) would
				// never get dims, leaving the input letterbox mapping at 0×0 = whole-canvas
				// coords → the remote cursor lands way off / the bars count as video. An
				// unconditional 1 Hz emit guarantees any listener has dims within a second,
				// and a rotating phone host's dim change propagates the same way.
				let dims = it.next().and_then(|s| {
					let (w, h) = s.split_once('x')?;
					Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?))
				});
				if let Some((w, h)) = dims {
					if w > 1 && h > 1 {
						let _ = app.emit("play-dims", (cur_id, w, h));
					}
				}
				let mbps = it.next().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
				let ms = it.next().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
				tracing::debug!(fps, mbps, decode_ms = ms, "render stats");
				DECODE_MS_BITS.store((ms as f32).to_bits(), std::sync::atomic::Ordering::Relaxed);
				let _ = app.emit(
					"play-vstats",
					PlayVStats {
						id: cur_id,
						fps,
						drops: 0,
						mbps,
						decode_ms: ms,
					},
				);
				// Emit play-ready once per session id (reset when cur_id changes). Gate on
				// PRESENTED frames only: bytes arriving (mbps > 0) while the decoder emits
				// nothing (e.g. a wedged async MFT before the watchdog rebuild) used to drop
				// the Connecting screen onto a black session.
				if ready_sent_for != Some(cur_id) && fps > 0.0 {
					ready_sent_for = Some(cur_id);
					if HOLD_REVEAL_ID.load(std::sync::atomic::Ordering::SeqCst) == cur_id {
						FIRST_FRAMES_ID.store(cur_id, std::sync::atomic::Ordering::SeqCst);
						tracing::info!(id = cur_id, fps, "first frames decoded — preparing screen held until the stream settles");
						// The connecting screen moves on to its "optimizing stream" step.
						let _ = app.emit("play-optimizing", cur_id);
					} else {
						tracing::info!(id = cur_id, fps, "first presented frames, play-ready");
						let _ = app.emit("play-ready", cur_id);
					}
				}
			} else if line.trim() == "vidsink-needidr" {
				RENDER_WANTS_IDR.store(true, std::sync::atomic::Ordering::Relaxed);
			} else if let Some(rest) = line.strip_prefix("vidsink-rfi ") {
				let v: Vec<u16> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
				if let [first, last] = v[..] {
					let mut r = RENDER_WANTS_RFI.lock().unwrap();
					*r = Some(match *r {
						Some((f, _)) => (f, last),
						None => (first, last),
					});
					drop(r);
					RFI_WAKE.notify_one();
				}
			} else if let Some(rest) = line.strip_prefix("vidsink-vrect ") {
				// Where the video sits inside the renderer window (client fractions): the
				// Windows capture confines the cursor to it while engaged.
				let v: Vec<f32> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
				if v.len() == 4 {
					crate::kbdhook::set_video_frac(v[0], v[1], v[2], v[3]);
				}
			} else if let Some((rest, from_hook)) = line
				.strip_prefix("hkey ")
				.map(|r| (r, true))
				.or_else(|| line.strip_prefix("key ").map(|r| (r, false)))
			{
				// A key from the renderer while control is engaged (Windows): `hkey` = its
				// shortcut capture (Alt+Tab / Win reach the remote), `key` = its focused child
				// window's messages (fallback). `<vk> <0|1>`, same handler either way.
				let mut it = rest.split_whitespace();
				if let (Some(vk), Some(d)) = (it.next().and_then(|v| v.parse::<u16>().ok()), it.next()) {
					crate::kbdhook::render_key(vk, d == "1", from_hook);
				}
			} else if let Some(rest) = line.strip_prefix("vidsink-dims ") {
				// The STREAM's pixel size ("<w>x<h>", first frame / live res switch) — the
				// frontend sizes the windowed session to the host's aspect ratio from this.
				if let Some((w, h)) = rest.trim().split_once('x') {
					if let (Ok(w), Ok(h)) = (w.parse::<u32>(), h.parse::<u32>()) {
						let _ = app.emit("play-dims", (cur_id, w, h));
					}
				}
			} else if let Some(rest) = line.strip_prefix("vidsink-dec ") {
				// The renderer's ACTUAL decoder ("vidsink-dec <name> <hw|sw|na>") — the UI
				// shows it read-only; there is no decoder picker (selection is automatic).
				let mut it = rest.split_whitespace();
				if let Some(name) = it.next() {
					let hw = it.next().unwrap_or("na").to_string();
					let _ = app.emit("play-decoder", (cur_id, name.to_string(), hw));
				}
			} else {
				let mut it = line.split_whitespace();
				match (it.next(), it.next()) {
					(Some("ov"), Some("set")) => {
						if let (Some(field), Some(val)) = (it.next(), it.next()) {
							tracing::info!(field, val, "overlay-cmd from renderer");
							// Mirror stream-selection fields into RenderSeed so a codec-switch
							// respawn can replay the user's choices onto the fresh renderer
							// (C14: the overlay would otherwise snap back to defaults).
							match field {
								"res" | "fps" | "bitrate" | "quality" | "display" => {
									use tauri::Manager;
									let state = app.state::<crate::state::AppState>();
									let plays = state.plays.lock().unwrap();
									if let Some(p) = plays.get(&cur_id) {
										let mut seed = p.render_seed.lock().unwrap();
										match field {
											"res" => seed.res = Some(val.to_string()),
											"fps" => seed.fps_sel = Some(val.to_string()),
											"bitrate" => seed.bitrate = Some(val.to_string()),
											"quality" => seed.quality = Some(val.to_string()),
											"display" => {
												if let Ok(idx) = val.parse::<u32>() {
													seed.display_idx = Some(idx);
												}
											}
											_ => {}
										}
									}
								}
								// "Dosya gönder" from the egui overlay Tools box: the click
								// happened in the renderer process, so there is no webview
								// user-activation and both WebView2 and WebKitGTK silently
								// block a programmatic <input type=file>.click(). Open the
								// native OS file picker Rust-side (rfd) and stream the chosen
								// file directly over the session data channel — no webview
								// gesture needed (C7). The rfd GTK backend needs the GTK
								// main thread (macOS: NSOpenPanel likewise), so this Rust-side
								// path is restricted to the two confirmed-broken platforms;
								// on others the overlay-cmd falls through to the webview path.
								"pickfile" => {
									#[cfg(any(windows, target_os = "linux"))]
									{
										use tauri::Manager;
										let state = app.state::<crate::state::AppState>();
										let tx = state
											.plays
											.lock()
											.unwrap()
											.get(&cur_id)
											.map(|p| p.data_tx.clone());
										if let Some(tx) = tx {
											std::thread::spawn(move || {
												// rfd::FileDialog::pick_file() is synchronous (fine
												// on a dedicated thread) and opens a real native
												// dialog — no webview activation required.
												if let Some(path) = rfd::FileDialog::new().pick_file() {
													// The reader thread lives outside the Tauri async
													// runtime; build a minimal single-thread runtime to
													// drive the async send_file_abs call.
													let rt =
														tokio::runtime::Builder::new_current_thread()
															.enable_all()
															.build();
													if let Ok(rt) = rt {
														rt.block_on(async {
															if crate::fs_browse::send_file_abs(&tx, &path)
																.await
																.is_none()
															{
																tracing::warn!(
																	?path,
																	"pickfile: send_file_abs failed"
																);
															}
														});
													}
												}
											});
											// Consume the field — do NOT fall through to the
											// overlay-cmd emit (that would route pickfile to
											// the webview where .click() is silently blocked).
											continue 'lines;
										}
									}
								}
								_ => {}
							}
							let _ =
								app.emit("overlay-cmd", (cur_id, field.to_string(), val.to_string()));
						}
					}
					(Some("ov"), Some("end")) => {
						let _ = app.emit("overlay-end", cur_id);
					}
					(Some("ov"), Some("close")) => {
						let _ = app.emit("overlay-close", cur_id);
					}
					// The renderer's own overlay-open button was clicked (platforms where
					// the closed-state renderer receives pointer events).
					(Some("ov"), Some("toggle")) => {
						let _ = app.emit("overlay-toggle", ());
					}
					// Native Chat: the user sent a line from the overlay composer
					// (rest-of-line payload; the frontend forwards it to the host AND
					// echoes it back over stdin so the renderer's log stays canonical).
					(Some("ov"), Some("chat")) => {
						let text = line.splitn(3, ' ').nth(2).unwrap_or("").trim();
						if !text.is_empty() {
							let _ = app.emit("overlay-chat", (cur_id, text.to_string()));
						}
					}
					// The overlay's Files box: open the per-session file-manager window
					// (the frontend supplies the peer label and invokes the command).
					(Some("ov"), Some("files")) => {
						let _ = app.emit("overlay-files", cur_id);
					}
					// Native Files (remote pane): list / download / upload requests.
					(Some("ov"), Some(op @ ("fsls" | "fsget" | "fssend"))) => {
						let path = line.splitn(3, ' ').nth(2).unwrap_or("").trim();
						// fsls "" = the host's HOME — an empty path is valid there.
						if op == "fsls" || !path.is_empty() {
							let _ = app.emit("overlay-fs", (cur_id, op.to_string(), path.to_string()));
						}
					}
					// Standalone render window (no --wid embed, e.g. Wayland client): it is a
					// separate toplevel, so its focus + video clicks arrive here and feed the
					// evdev capture's focus/engage gates (see kbdhook::linux).
					(Some("ov"), Some("focus")) => {
						crate::kbdhook::set_render_focused(it.next() == Some("1"));
					}
					(Some("ov"), Some("engage")) => {
						crate::kbdhook::engage_render(&app);
					}
					// The renderer's "stream stopped" edge: release the capture (no picture to
					// control) and keep it released until the stream is back.
					(Some("ov"), Some("stalled")) => {
						crate::kbdhook::set_stream_stalled(&app, it.next() == Some("1"));
					}
					_ => {}
				}
			}
		}
	});
}
