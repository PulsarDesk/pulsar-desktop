//! The client's full-duplex hold-loop, extracted verbatim from `start_remote_play`.
//!
//! `hold_session` owns the control `Session` for the lifetime of a remote-play tab:
//! it forwards local input + side-channel data to the host, pings every 500 ms (keepalive —
//! UDP has no disconnect signal — and the RTT the adaptive controller runs on), receives
//! the host's chat/clipboard/stat pushes (surfacing them to the UI), watchdogs host
//! silence, runs the shared adaptive controller (`pulsar_core::adapt`) on 2 s windows, and re-requests
//! the stream when the session menu or the controller changes a stream parameter. On exit
//! it tells the frontend (`play-ended`) and sends a clean `Bye`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use pulsar_core::service::{
	decode_data, media, request_stream, send_data, send_input, send_keepalive, ClientStats,
	DataMsg, InputEvent, LossRecovery, QualityPref, StreamReq,
};
use pulsar_core::Session;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc::Receiver;

use pulsar_core::adapt::repair::{
	decoder_overloaded, fast_drop_on, goodput_kbps, FecShare, FrameClock, KeyframePacer, RepairTracker,
	REPAIR_SWEEP_MS,
};
use pulsar_core::adapt::caps::path_cap_kbps;
use pulsar_core::adapt::pong::PongMatcher;
use pulsar_core::adapt::Controller;
use crate::events::{
	AvatarPayload, DataPayload, FilePayload, FsEntriesPayload, PlayRtt, PlayStats,
};
use crate::state::Restream;

/// How long the renderer holds the last good frame after an unrepaired loss while the
/// keyframe is on its way (Phase 0.5). Bounded (a keyframe ends it sooner), and the renderer
/// arms it once per loss episode, so a run of losses can't chain holds into a long freeze.
const HOLD_MS: u32 = 300;
/// At most this many already-queued input events are taken per wake of the input arm (see
/// the backlog drain there): bounded so one busy moment can't starve the other arms.
const INPUT_BACKLOG_MAX: usize = 64;

/// Whether this host takes `ev`. Mouse side buttons (3 = back, 4 = forward) go only to hosts
/// that advertise `FEAT_MOUSE_X`: older hosts turned every button id they did not know into a
/// LEFT click, so a back-button press clicked on the remote. Ids past forward never go out.
fn host_takes(ev: &InputEvent, host_mouse_x: bool) -> bool {
	use pulsar_core::input::mouse_button;
	match *ev {
		InputEvent::PointerButton { button, .. } => {
			mouse_button::is_known(button) && (host_mouse_x || !mouse_button::is_side(button))
		}
		_ => true,
	}
}

/// The point a stream runs at: the controller's while it adapts; with "Otomatik ayar" off the
/// top rung — the picks (fitted to the monitor), else the native size.
fn stream_point(ctl: &Controller, auto: bool) -> pulsar_core::adapt::Point {
	if auto {
		ctl.point()
	} else {
		ctl.points().first().copied().unwrap_or_else(|| ctl.point())
	}
}

/// The fixed rate with "Otomatik ayar" off (kbit/s): the user's bitrate pick (`user_cap`, under
/// the transport's cap), else what the controller measured as good (`fixed_rate_kbps`).
fn fixed_rate(ctl: &Controller) -> u32 {
	if ctl.user_cap() > 0 {
		ctl.user_cap().min(ctl.cap())
	} else {
		ctl.fixed_rate_kbps()
	}
}

/// The renderer hold after an unrepaired loss in this recovery mode (ms), None = none.
/// Intra refresh: none. No keyframe is coming to end it early, and the decoder conceals the loss
/// while the rolling wave heals the picture within its period — a hold only froze a third of
/// that. Field log 2026-11-09 (Orange Pi client, 9.6 Mbit/s policer + 1 % loss, IR stream): a
/// `hold` on every sweep with a written-off packet, 1-1.7 s freezes, "renderer fps very low"
/// 11-23 times in 80 s.
/// RFI (`rfi`: the renderer runs `rfi 1`, see the stats-window `rfi` line): none either. The
/// depacketizer already withholds every access unit until the recovery frame or a keyframe, so
/// nothing smeared is shown — and the recovery frame is no keyframe: a hold armed by the sweep
/// (which writes the packet off after the renderer already reported it) froze the healed
/// picture for the full HOLD_MS (Linux; the other renderers ignore `hold`).
fn loss_hold_ms(recovery: LossRecovery, rfi: bool) -> Option<u32> {
	(recovery != LossRecovery::IntraRefresh && !rfi).then_some(HOLD_MS)
}

/// The recovery mode to stream in for the controller's flip to `want`, on the current codec.
/// An intra-refresh flip becomes the short GOP while RFI reaches (H.264 / HEVC from an RFI
/// host): the client turns RFI off under intra refresh (`rfi 0`), and a host whose encoder was
/// built without the wave (it cannot start one live) then left every loss smeared until its
/// long-GOP IDR. The short GOP keeps RFI, which the RFI hosts absorb. Re-derived on a codec
/// switch: AV1 has no recovery tag, so there the flip stays intra refresh (the Linux renderer
/// resumes on it). Only a FLIP is mapped — a session that started in intra refresh on purpose
/// (play.rs `ir_bad_link`) never flips.
fn flip_recovery(want: LossRecovery, host_rfi: bool, codec: &str) -> LossRecovery {
	if want == LossRecovery::IntraRefresh && rfi_reach(host_rfi, codec) {
		LossRecovery::ShortGop
	} else {
		want
	}
}

/// Reference-frame invalidation is the recovery: the host does it and the codec has the
/// recovery tag (H.264 / HEVC). Whether a reported hole still lies within the encoder's
/// reference buffer is the HOST's call — it knows its DPB depth and sends a keyframe itself
/// when the lost frame is out of reach or the invalidation fails. (The client used to gate on
/// RTT × fps ≤ 4 frames, which a 120 fps stream over any WAN never met: every loss became a
/// keyframe request.)
fn rfi_reach(host_rfi: bool, codec: &str) -> bool {
	host_rfi && codec != "av1"
}

/// Host-silence watchdog bounds. Nothing at all from the host (control OR media) for
/// `SILENT_ANY` = the link is dead. Control alone silent for `SILENT_CONTROL` while media still
/// flows = a dead control channel (a host that stopped answering Pings / a half-dead session):
/// end it eventually, but not after 5 s — on a 40 %-loss mobile link a run of lost Pongs is
/// ordinary, and ending the session on it was one of the field "stream stopped"s.
const SILENT_ANY: std::time::Duration = std::time::Duration::from_secs(5);
const SILENT_CONTROL: std::time::Duration = std::time::Duration::from_secs(15);

/// Whether the host has gone silent long enough to end the session: `since_control` = time since
/// the last control datagram (Pong / DataMsg), `since_media` = since the last media datagram.
fn host_silent(since_control: std::time::Duration, since_media: std::time::Duration) -> bool {
	since_control.min(since_media) > SILENT_ANY || since_control > SILENT_CONTROL
}

/// No video datagram at all for this long while the host still answers = the host stopped
/// sending video (stopped sharing, a capture rebuild that failed, a screen picker): the
/// renderer gets `stopped 1` (its "stream stopped" card + input-capture release). The renderer
/// no longer decides this itself — on a lossy link it saw "no decodable AU" while packets still
/// flowed and stopped a live session; packets still arriving (however mangled) is not a stop.
const VIDEO_STOPPED: std::time::Duration = std::time::Duration::from_secs(5);

/// Whether the stream really stopped (see `VIDEO_STOPPED`): only once video has flowed in this
/// session (`video_seen`; a session that never got video is still starting), and only while the
/// control channel is alive — a dead link is `host_silent`'s call, which ends the session.
fn video_stopped(video_seen: bool, since_video: std::time::Duration, since_control: std::time::Duration) -> bool {
	video_seen && since_video >= VIDEO_STOPPED && since_control < SILENT_ANY
}

/// A ladder point as (width, height, fps) — what the user sees, without the codec-dependent
/// minimum rate.
type PointKey = (u32, u32, u32);

fn point_key(p: &pulsar_core::adapt::Point) -> PointKey {
	(p.width, p.height, p.fps)
}

/// Minimum gap between two "the link can't carry your pick" notices, so a point that flaps
/// between two lighter rungs doesn't toast every window.
const SHORTFALL_NOTICE_GAP: std::time::Duration = std::time::Duration::from_secs(30);

/// Whether to tell the user about the current pick shortfall (`now` = `Controller::
/// pick_shortfall` as (picked, current)): when there is one and it differs from the last one
/// told (`last`, `since_last` ago), at most once per `SHORTFALL_NOTICE_GAP`. The same shortfall
/// coming back after the link recovered is not repeated.
fn shortfall_due(
	last: Option<(PointKey, PointKey)>,
	since_last: Option<std::time::Duration>,
	now: Option<(PointKey, PointKey)>,
) -> bool {
	match now {
		None => false,
		Some(n) => last != Some(n) && since_last.is_none_or(|e| e >= SHORTFALL_NOTICE_GAP),
	}
}

/// The shortfall notice text (Turkish unless the UI runs in English). Inline rather than in
/// `lang/*.json`: it carries two runtime labels and is the only stream notice built here.
fn shortfall_text(picked: &str, current: &str, english: bool) -> String {
	if english {
		format!("The connection can't carry {picked} — using {current}")
	} else {
		format!("Bağlantı {picked} taşımıyor — {current} kullanılıyor")
	}
}

/// Hold the control session open full-duplex: forward input + side-channel data,
/// keepalive every ~2s (UDP has no disconnect signal), and receive the host's
/// chat/clipboard pushes — surfacing them to the UI.
#[allow(clippy::too_many_arguments)]
pub(super) async fn hold_session(
	mut sess: Session,
	app_ev: AppHandle,
	send_flag: Arc<AtomicBool>,
	mut input_rx: Receiver<InputEvent>,
	mut data_rx: Receiver<DataMsg>,
	mut restream_rx: Receiver<Restream>,
	id: u64,
	video_port: u16,
	audio_port: u16,
	encoder_h: String,
	codec_h: String,
	game_mode: bool,
	decode_codecs: Vec<String>,
	// Codecs the renderer decodes in 4:4:4 (`play::yuv444_request`): a re-request with another
	// codec re-decides 4:4:4 for it.
	decode_yuv444: Vec<String>,
	render_stdin: std::sync::Arc<std::sync::Mutex<Option<std::process::ChildStdin>>>,
	mos: bool,
	host_nack: bool,
	// The host does reference-frame invalidation (see play.rs `host_rfi`).
	host_rfi: bool,
	req_w: u32,
	req_h: u32,
	req_fps: u32,
	base_kbps: u32,
	// Relay per-session bitrate cap (kbit/s, 0 = uncapped), already headroom-discounted by
	// play.rs. Caps the adaptive controller's ceiling AND any user-picked manual bitrate so a
	// relayed session never re-overshoots the relay's drop limit after a menu change / ramp-up.
	cap_kbps: u32,
	// The shared adaptive controller (`pulsar_core::adapt`), built by play.rs so the initial
	// request already carries its start point; this loop feeds and actuates it.
	mut ctl: Controller,
	// What identifies the peer for the per-peer memory (`adapt_memory`): id or address.
	peer_key: String,
	// The kind of path the session started on (the memory's second key); followed live below.
	path_class: crate::adapt_memory::PathClass,
	cursor_external: bool,
	// Decides hdr10 per codec (Settings, host FEAT_HDR, the 10-bit decoders, PULSAR_HDR).
	hdr_neg: super::HdrNeg,
	init_quality: QualityPref,
	// The resolved audio quality the session started with, what "auto" resolved to (a menu
	// pick of auto goes back to it), and whether the host can send Raw.
	init_audio_quality: pulsar_core::audio::AudioQuality,
	auto_audio_quality: pulsar_core::audio::AudioQuality,
	host_audio_raw: bool,
	// Initial host monitor (0 = primary). Seeds `cur_display` so a re-request (adaptive
	// bitrate step / menu change) preserves the monitor the session STARTED on — without
	// this, a same-host second pane that started on a free monitor would snap back to 0 on
	// the first adaptive step, re-colliding with the sibling pane's DXGI capture.
	init_display: u32,
	// Initial per-window capture target (Phase 2b co-op). Seeds `cur_window` so every
	// re-request (adaptive bitrate / menu change) preserves the WGC window the session
	// started on — without this, the first adaptive step would drop `window_hwnd` to None
	// and snap the host back to whole-monitor DXGI capture. `None` = the monitor path.
	init_window: Option<i64>,
	// On-demand host-window-list queries (Phase 2b co-op): the `host_window_list` command
	// sends a oneshot reply-sender here; we service it by asking the host (`query_windows`)
	// over the control session we own and answering on the oneshot. Routed through the hold
	// loop so the held `Session` stays single-owner.
	mut windows_query_rx: Receiver<tokio::sync::oneshot::Sender<Vec<pulsar_core::service::WindowInfo>>>,
	// Host → client rumble: each DataMsg::Rumble is forwarded here for the gilrs reader
	// thread to replay on the physical pad (force-feedback).
	rumble_tx: tokio::sync::mpsc::Sender<(u8, u8, u8)>,
	// Every host monitor `(idx, width, height, refresh Hz)` and the client's own fps target:
	// a monitor switch rebuilds the adaptive ladder for the new monitor (its size, and the fps
	// it can capture at, capped by what this display shows).
	mut host_displays: Vec<(u32, u32, u32, u32)>,
	client_fps: u32,
	// The adaptive cap for a DIRECT path (the user's configured bitrate, else the auto cap):
	// a relayed session that upgrades to direct mid-stream drops the relay's cap for this.
	direct_cap_kbps: u32,
	// The host injects real pen / touch (`FEAT_PEN` / `FEAT_TOUCH`); otherwise both go out as
	// mouse events (pulsar-core `pen_touch::client`).
	host_pen: bool,
	host_touch: bool,
	// "Otomatik ayar" off at the start: the fixed rate play.rs requested (None = adaptive).
	fixed_kbps: Option<u32>,
	// The host injects the mouse side buttons (`FEAT_MOUSE_X`, see `host_takes`).
	host_mouse_x: bool,
) {
	// Client scheduling while this session runs (crates/pulsar-prio, hidden-gaps 1.14): the
	// session read loop and this receive / forward loop are tokio tasks, and the runtime's
	// workers run raised while a session is on (Windows ABOVE_NORMAL, macOS QoS
	// user-initiated, Linux nice -5); EcoQoS (Windows) and App Nap (macOS) are off for the app.
	let _prio = pulsar_prio::session();
	tracing::info!(process = %pulsar_prio::session_report(), "client session priority on");
	// A relayed session keeps punching in the background (core `punch_in_background`) and may
	// upgrade to a direct path mid-stream; the adaptive rate then starts over on it.
	let mut on_relay = matches!(sess.transport(), pulsar_core::Transport::Relay);
	// DualSense pad mic → host: raw TAG_DS5 payloads straight onto the session.
	crate::ds5::client::Ds5Client::set_media_sender(id, sess.sender());
	let mut keep = tokio::time::interval(std::time::Duration::from_secs(2));
	keep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
	// The keepalive Ping fires at uneven gaps (`ping_gap_ms`, mean PING_MS); the first at once.
	let ping = tokio::time::sleep(std::time::Duration::ZERO);
	tokio::pin!(ping);
	let mut repair = tokio::time::interval(std::time::Duration::from_millis(REPAIR_SWEEP_MS));
	repair.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
	// Pen / touch: the gate (feature check + mouse fallback) and its timers — the keep-alive
	// re-send while a pen hovers / a finger rests, the fallback's press-and-hold.
	let mut pen_touch = pulsar_core::input::pen_touch::client::ClientPenTouch::new(
		host_pen,
		host_touch,
		std::time::Instant::now(),
	);
	// macOS / Linux webviews report a tablet pen as a mouse: with a native tablet sample at
	// hand (pulsar-core `pen_touch::tablet`), those mouse events go out as the pen.
	use pulsar_core::input::pen_touch::tablet;
	let mut tablet_aug = tablet::TabletAugment::default();
	if host_pen {
		tablet::start_native();
	}
	let mut pen_tick = tokio::time::interval(std::time::Duration::from_millis(50));
	pen_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
	// The client display stays awake while this session is in use: from now until
	// `keep_awake::client_hold` after the last real input (pads included). Every input the
	// session forwards passes the `input_rx` arm below; focus / lock / expiry are checked on
	// the 2 s `keep` tick (see keep_awake.rs).
	let mut awake = crate::keep_awake::ClientAwake::start(std::time::Instant::now());
	// Pointer motion / scroll batching (≤ one packet per ~1 ms per kind; the first event of
	// a burst goes out at once, a button or key flushes what is held first) — a 4-8 kHz
	// mouse otherwise sends one encrypted datagram per report. See pulsar-core
	// `input::coalesce`. The held-back tail leaves from the deadline arm in the loop below.
	let mut motion = pulsar_core::input::coalesce::MotionCoalescer::default();
	let mut motion_out: Vec<InputEvent> = Vec::new();
	// That tail waits on a tokio timer: at Windows' default 15.6 ms timer resolution it would
	// leave up to ~16 ms late, so the client keeps the 1 ms resolution while the session runs
	// (refcounted with the host side; Linux / macOS timers are already fine — timer_res.rs).
	let _timer = crate::timer_res::acquire();
	// ---- Media-over-session demux state (single-socket transport) ----
	// Media frames arrive INSIDE this session ([tag][rtp…]); forward each datagram to
	// the local consumer: the renderer's RTP port (video) / the viewer's audio port.
	let fwd_sock = if mos {
		tokio::net::UdpSocket::bind("127.0.0.1:0").await.ok()
	} else {
		None
	};
	let v_dest = std::net::SocketAddr::from(([127, 0, 0, 1], video_port));
	// Linux: video goes to the renderer's Unix datagram socket instead (no IP stack per
	// packet); UDP stays the path elsewhere and for consumers that don't bind it.
	let mut v_unix = crate::native_view::VideoSink::new(video_port);
	let a_dest = std::net::SocketAddr::from(([127, 0, 0, 1], audio_port));
	// One-shot diagnostic: did host media actually reach this client + get forwarded?
	let mut first_video_logged = false;
	// Video RTP gap tracking → NACK retransmit requests + the loss% the adaptive controller runs
	// on, the FEC decoders (Phase 2.1: XOR parity rebuilds a group's single missing packet,
	// Reed-Solomon several per frame), the window counters (incl. goodput and the parity share)
	// and the keepalive RTT that sizes the repair window — the shared repair loop
	// (`pulsar_core::adapt::repair::RepairTracker`, also the mobile client's).
	let mut repair_st = RepairTracker::new();
	// The repair window last pushed to the renderer (ms) — re-sent only on a real change.
	let mut sent_repair_ms: f32 = 0.0;
	// Loss-recovery mode currently requested from the host (flipped once by the controller).
	// DEBUG knob (env-gated): start in a given loss-recovery mode instead of switching on the
	// first sustained loss — `PULSAR_RECOVERY=ir|shortgop` (pairs with the renderer's
	// PULSAR_TEST_DROP to exercise the resume path).
	let mut cur_recovery = match std::env::var("PULSAR_RECOVERY").as_deref() {
		Ok("ir") => LossRecovery::IntraRefresh,
		Ok("shortgop") => LossRecovery::ShortGop,
		// What the session started in (intra refresh from the start on a game session over a
		// bad link — play.rs `ir_bad_link`; else Normal until the first sustained loss).
		_ => ctl.recovery(),
	};
	// The controller's loss-recovery flip, as it asked for it (`flip_recovery` maps it per codec).
	let mut flip_want: Option<LossRecovery> = None;
	// Game mode over a high-RTT path: drop a hole fast (see `FAST_DROP_RTT_ON_MS`); NACKs only
	// while it is off (their answer would come after the renderer gave up).
	let mut fast_drop = false;
	let mut nack_on = host_nack;
	// Delay-gradient sampling (Phase 3): per video FRAME (RTP timestamp), the arrival time of
	// its last packet on a session-local ms clock feeds the controller's trendline estimator.
	let epoch = std::time::Instant::now();
	let mono_ms = move || epoch.elapsed().as_secs_f64() * 1000.0;
	let mut frame_clock = FrameClock::default();
	// A Dynamic-refresh panel idles at its low rate for us: run it at its top rate while this
	// session streams faster than that (restored when the guard drops at the session's end).
	#[cfg(windows)]
	let _display_boost = crate::util::DisplayBoost::acquire(
		&app_ev,
		ctl.points().first().map(|p| p.fps).unwrap_or(req_fps),
	);
	// Opening phase (Windows `--hold-reveal`): reveal the video once the bitrate has settled.
	let mut revealed = false;
	let mut first_frames_at: Option<std::time::Instant> = None;
	// The host's display name (DataMsg::PeerName), for the "file arrived" toasts.
	let mut host_name = String::new();
	// Frame-clock probe (diagnostics): over ~2 s, how far the host's RTP clock advanced vs our
	// wall clock, how many frames arrived and the worst inter-arrival gap. A host that emits
	// fewer frames than its RTP clock claims shows rtp < wall (a fake, permanent delay trend);
	// a bursty radio link shows rtp ≈ wall with big gaps.
	let mut fc: Option<(u32, f64)> = None; // (first ts, first arrival) of the window
	let mut fc_n = 0u32;
	let mut fc_prev_arr = 0.0f64;
	let mut fc_max_gap = 0.0f64;
	// User pins from the session menu: an explicit resolution or fps switches the ladder off
	// (bitrate-only), "Otomatik" hands that dimension back to the controller.
	// Saved picks applied in play.rs before the first request start out pinned too.
	let mut manual_res = ctl.pins().0.is_some();
	let mut manual_fps = ctl.pins().1.is_some();
	// The last rate remembered for this peer (write-through when it changes).
	let mut remembered_kbps: u32 = 0;
	// Failed-repair keyframe request: when a NACKed packet never arrives (expired from
	// `missing`), the reference chain is broken until the next keyframe — with long host
	// GOPs that is seconds of smear. Ask the host for an immediate IDR instead (rate-limited).
	let mut kf_pacer = KeyframePacer::new(std::time::Instant::now());
	// When the last menu re-request (esp. a monitor switch) fired. The host rebuilds its encoder
	// for one → a brief packet-loss gap that is NOT congestion; the adaptive controller must
	// ignore it for a moment, or it steps the bitrate (a full-path restream that churns + slows
	// the very switch in progress). Seeded far in the past so startup isn't suppressed.
	let mut last_switch_at = std::time::Instant::now() - std::time::Duration::from_secs(60);
	// When each keepalive Ping was sent, to time the host's Pongs (see `PongMatcher`).
	let mut pings = PongMatcher::new();
	// Watchdog: a host-side kick / network drop doesn't always send a clean close over UDP,
	// so track the last time we heard ANYTHING from the host (incl. keepalive Pongs). If it
	// goes silent past the timeout, end the session — this fires `play-ended` so the client
	// tears down (releases the input grab + drops the tab) instead of freezing on the last
	// frame with the keyboard/mouse still captured.
	let mut last_inbound = std::time::Instant::now();
	// The last MEDIA datagram (video / audio / parity): liveness too — see `host_silent`.
	let mut last_media = std::time::Instant::now();
	// The last VIDEO datagram, and whether the renderer was told the stream stopped
	// (`video_stopped`; edge-triggered `stopped 1|0`).
	let mut last_video = std::time::Instant::now();
	let mut told_stopped = false;
	// When the measurement window started (its counters live in `repair_st`).
	let mut win_start = std::time::Instant::now();
	// The kind of path the media runs on now (`adapt_memory` key): the last good rate is
	// remembered under the class the session actually ran cleanly on.
	let mut cur_class = path_class;
	// The (transport, peer address) the class was last worked out for: the overlay check
	// enumerates interfaces / asks the routing table, so it runs only when the path itself
	// changed. Seeded with the session-start path, whose class `play.rs` also judged by the
	// measured round trip.
	let mut class_of: (pulsar_core::Transport, Option<std::net::IpAddr>) = (
		sess.transport(),
		if matches!(sess.transport(), pulsar_core::Transport::Direct) {
			sess.peer_addr().await.map(|a| a.ip())
		} else {
			None
		},
	);
	let start_path = class_of;
	// The last pick shortfall told to the user, and when (see `shortfall_due`).
	let mut shortfall_told: Option<(PointKey, PointKey)> = None;
	let mut shortfall_told_at: Option<std::time::Instant> = None;
	// Current stream state; a session-menu change updates one field and re-requests.
	// Geometry/fps seed from the INITIAL request (the Linux native path negotiates an
	// explicit startup size — e.g. the deliberate 960x540@30 mpv cap): a re-request
	// (adaptive-bitrate step / menu change) must preserve it. Seeding 0 instead meant
	// "host config default" (1080p60), flipping a capped stream to full size exactly
	// when the link was already lossy.
	let (mut cur_w, mut cur_h) = (req_w, req_h);
	let mut cur_encoder = encoder_h;
	let mut cur_codec = codec_h;
	let mut cur_fps = req_fps;
	let mut cur_transmit = true;
	// The renderer's decode numbers belong to the previous session until this one's arrive.
	crate::render_stats::clear_decode_stats();
	// Wi-Fi low-latency mode while this session receives (see wlan_lowlat.rs).
	let _wlan = crate::wlan_lowlat::acquire();
	// Consecutive stats windows in which the client decoder looked overloaded (see
	// `decoder_overloaded`); the fps steps down at 2.
	let mut dec_over_windows: u32 = 0;
	// Host-silent intent: game mode requests it by default (seeds the initial play.rs request);
	// the host honors it by REDIRECTING its default output to a virtual sink (never muting the
	// captured endpoint — see play.rs). Preserved across every re-request; the user can flip it
	// mid-session from the overlay (sets cur_mute below).
	let mut cur_mute = game_mode;
	// Host monitor index (0 = primary). Seeded from the INITIAL request — a same-host
	// second pane starts on a FREE monitor (not 0) — and changed live by the session menu's
	// monitor picker (Restream::Display); preserved across every re-request below.
	let mut cur_display: u32 = init_display;
	// Per-window capture target (Phase 2b co-op). Seeded from the INITIAL request so it
	// survives every re-request; a session-menu "window" pick changes it live via
	// Restream::Window. `Some(hwnd)` makes the host capture that single window via WGC
	// (overriding cur_display); `None` = the cur_display monitor path.
	let mut cur_window: Option<i64> = init_window;
	// Screen-ADAPTATION target (Parsec-style): `Some((w,h))` asks the host to switch the captured
	// monitor to the display mode best fitting a w×h pane (this renderer's window) and to REVERT it
	// on teardown; `None` = no change. Flipped live by the overlay's "Ekran uyarlama" toggle via
	// Restream::Adapt; preserved across every re-request. Ignored host-side when a window target is
	// set (per-window capture has no monitor mode).
	let mut cur_adapt: Option<(u32, u32)> = None;
	// The monitor's own size while screen adaptation runs (the ladder goes back to it when it is
	// turned off).
	let mut adapt_restore: Option<(u32, u32)> = None;
	// Requested audio channel layout. Stereo today (the client default in play.rs — the
	// audio paths expect opus/48000/2 and the host negotiates surround down anyway);
	// kept as state so it's preserved across every re-request and a future surround
	// picker can flip it without re-plumbing each StreamReq.
	let cur_audio_layout = pulsar_core::audio::ChannelLayout::Stereo;
	let mut cur_audio_quality = init_audio_quality;
	// Starts at the INITIAL request's bitrate (0 = host default) so a menu-driven
	// re-request doesn't silently reset an explicit starting target; the adaptive
	// controller (`pulsar_core::adapt`) moves it within [FLOOR_KBPS, cap].
	let mut cur_bitrate = base_kbps;
	// A user-picked limit (session menu / overlay) disables the adaptive controller;
	// picking "Otomatik" (0) re-enables it.
	// The controller needs the media path's seq/timestamps → media-over-session only.
	let adapt_enabled = mos;
	// "Otomatik ayar": the controller adapts the rate / point (true), or the stream stays fixed
	// at the picks (false; Moonlight-like — the controller only measures, FEC / NACK / recovery
	// frames go on). Flipped live by `Restream::AutoAdjust`.
	let mut auto_on = fixed_kbps.is_none();
	let _ = (base_kbps, cap_kbps);
	// Quality bias: seeded from the Settings → Display 'Varsayılan kalite' control
	// (already resolved in play.rs: "hq"→Quality, "fast"→Latency, "auto"→mode default).
	// The session overlay's quality picker (Restream::Quality) can override it live.
	let mut cur_quality = init_quality;
	// HDR10 for the CURRENT codec (Settings → Display + host FEAT_HDR + this renderer's 10-bit
	// decode of that codec; PULSAR_HDR is a debug override). Re-decided on every codec change:
	// a switch to a codec whose decoder failed the 10-bit probe must drop HDR, and a session
	// that started on H.264 gets HDR after a switch to HEVC.
	let mut cur_hdr = hdr_neg.want(&cur_codec);
	// Inbound file reassembly (file-manager downloads — the host streams an FsGet
	// back as FileBegin/Chunk/End): Begin → state, Chunk → store BY INDEX,
	// End → save in the received-files folder. The session transport is unordered UDP,
	// so chunks can arrive reordered or duplicated; keying by index (not appending
	// in arrival order) lets reorders/dups self-heal — only a genuinely lost chunk
	// fails the transfer. Each transfer carries a per-stream `id`, so concurrent
	// downloads keep separate reassembly state keyed by that id and interleaved
	// messages no longer clobber each other. Mirrors the host's make_on_file.
	/// Idle transfers older than this are swept from the map on the next FileBegin.
	const F_XFER_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
	/// Maximum concurrent in-flight transfers. If a new FileBegin would push us over
	/// this limit we first evict a headless entry (`expected == None` — FileBegin was
	/// lost, can never complete) and only fall back to oldest-by-last_activity among
	/// active entries, to avoid silently killing a slow-but-legitimate transfer.
	const F_MAX_CONCURRENT_XFERS: usize = 8;
	struct FileReasm {
		/// Set by FileBegin. `None` means chunks arrived before FileBegin (UDP
		/// reorder); the entry was lazily created to buffer them. A FileEnd with
		/// `expected == None` is treated as incomplete (Begin never arrived).
		name: String,
		expected: Option<u32>,
		chunks: std::collections::BTreeMap<u32, Vec<u8>>,
		// Running total of buffered bytes (sum of every stored chunk's len, dup-safe).
		// Capped against MAX_XFER_BYTES so a peer can't OOM us by announcing a huge
		// chunk count and streaming distinct-index chunks without ever sending FileEnd.
		received: u64,
		/// Updated on FileBegin and every FileChunk; entries idle beyond
		/// F_XFER_IDLE_TIMEOUT are swept on the next FileBegin so a lost FileEnd
		/// (UDP, no retransmit) can't leak buffered bytes for the session lifetime.
		last_activity: std::time::Instant,
	}
	let mut f_xfers: std::collections::HashMap<u32, FileReasm> = std::collections::HashMap::new();
	// Pending host-window-list query (Phase 2b co-op picker, Bug 4). When the
	// `host_window_list` command hands us a oneshot reply-sender we fire a `QueryWindows`
	// over the session and stash the sender + a deadline here — then resolve it from the
	// inbound demux below when the host's `Windows` reply arrives, instead of a blocking
	// read-loop that consumed (and discarded) the media/RTP racing on the same session. A
	// stale query (old / non-Windows host that never replies) resolves to an empty list at
	// the deadline. At most one is in flight; a superseding query answers the prior empty.
	let mut pending_windows: Option<(
		tokio::sync::oneshot::Sender<Vec<pulsar_core::service::WindowInfo>>,
		tokio::time::Instant,
	)> = None;
	// The current stream state as a request (every re-request — menu or controller — sends
	// the FULL state so nothing snaps back to a default).
	macro_rules! make_req {
		() => {
			StreamReq {
				port: video_port,
				codec: cur_codec.clone(),
				encoder: cur_encoder.clone(),
				width: cur_w,
				height: cur_h,
				fps: cur_fps,
				audio_port,
				transmit_audio: cur_transmit,
				mute_host: cur_mute,
				game_mode,
				bitrate_kbps: cur_bitrate,
				quality: cur_quality,
				// Never the legacy bit (see play.rs); HDR10 only through hdr10.
				hdr: false,
				hdr10: cur_hdr,
				yuv444: super::yuv444_request(&cur_codec, &decode_yuv444),
				decode_yuv444: decode_yuv444.clone(),
				decode_codecs: decode_codecs.clone(),
				media_over_session: mos,
				cursor_external,
				display_idx: cur_display,
				window_hwnd: cur_window,
				adapt: cur_adapt,
				audio_layout: cur_audio_layout,
				audio_quality: cur_audio_quality,
				loss_recovery: cur_recovery,
				fec: mos,
				fec_rs: mos,
				rfi: host_rfi && cur_codec != "av1",
				audio_fec: mos,
			}
		};
	}
	loop {
		if !send_flag.load(Ordering::SeqCst) {
			break;
		}
		// Deadline for a still-pending window-list query, so the select's fallback arm can
		// resolve it empty if the host never sends a `Windows` reply. `None` disables that arm.
		let win_deadline = pending_windows.as_ref().map(|(_, d)| *d);
		tokio::select! {
			ev = input_rx.recv() => match ev {
				Some(ev) if !host_takes(&ev, host_mouse_x) => {}
				Some(ev) => {
					let mut dead = false;
					let now = std::time::Instant::now();
					// Whatever else is already queued came in while this loop was busy in
					// another arm (media demux, a send_input await, file chunks) or from a hook
					// that delivers in batches: take it now (bounded) — it merges into fewer
					// packets, and `push_backlog` sends the merged tail at once instead of
					// holding already-late input for the coalescer's timer.
					let mut batch = vec![ev];
					while batch.len() < INPUT_BACKLOG_MAX {
						match input_rx.try_recv() {
							Ok(ev) => batch.push(ev),
							Err(_) => break,
						}
					}
					let mut evs = Vec::with_capacity(batch.len());
					for ev in batch {
						// Drained events pass the same side-button gate as the first one.
						if !host_takes(&ev, host_mouse_x) {
							continue;
						}
						awake.on_input(&ev, now);
						let aug = if host_pen { tablet_aug.on_event(now, ev, tablet::latest()) } else { vec![ev] };
						for ev in aug {
							evs.extend(pen_touch.on_event(now, ev));
						}
					}
					motion_out.clear();
					motion.push_backlog(now, evs, &mut motion_out);
					for ev in motion_out.drain(..) {
						if send_input(&mut sess, &ev).await.is_err() {
							dead = true;
							break;
						}
					}
					if dead {
						break;
					}
				}
				None => break,
			},
			_ = pen_tick.tick() => {
				let now = std::time::Instant::now();
				let mut evs = Vec::new();
				if host_pen {
					for ev in tablet_aug.tick(tablet::latest()) {
						evs.extend(pen_touch.on_event(now, ev));
					}
				}
				evs.extend(pen_touch.tick(now));
				motion_out.clear();
				for ev in evs {
					motion.push(now, ev, &mut motion_out);
				}
				for ev in motion_out.drain(..) {
					let _ = send_input(&mut sess, &ev).await;
				}
			}
			// The held-back motion / scroll tail of a burst, when its window closes.
			_ = tokio::time::sleep_until(
				motion.deadline().map(tokio::time::Instant::from_std).unwrap_or_else(tokio::time::Instant::now),
			), if motion.deadline().is_some() => {
				if let Some(ev) = motion.poll(std::time::Instant::now()) {
					let _ = send_input(&mut sess, &ev).await;
				}
			}
			d = data_rx.recv() => match d {
				// A side-channel send failure must NOT kill the session: an oversized
				// payload (e.g. an avatar past the datagram budget → EMSGSIZE) is a
				// one-message problem, and a genuinely dead link is already caught by
				// the keepalive watchdog below. Breaking here tore the whole session
				// down 0.7 s into the stream ("connected but no video").
				Some(dm) => if let Err(e) = send_data(&sess, &dm).await {
					tracing::warn!(%e, "side-channel send failed (message dropped)");
				},
				None => {}
			},
			inbound = sess.recv() => match inbound {
				Some(bytes) => {
					// Media-over-session fast path: [tag][rtp…] frames → forward the raw
					// RTP datagram to its local consumer (renderer / audio viewer port).
					// Checked FIRST — these are the highest-rate payloads on the session.
					if let Some((tag, rtp)) = media::parse(&bytes) {
						last_media = std::time::Instant::now();
						// DualSense passthrough haptics/speaker slots → the pad threads.
						if tag == pulsar_core::input::ds5::wire::TAG_DS5 {
							crate::ds5::client::on_media(id, &bytes);
							continue;
						}
						// FEC parity (Phase 2.1): rebuild the group's single missing packet — a
						// repair with ZERO round-trips — and hand it to the renderer as if it had
						// arrived (late: the reorder buffer slots it in). Also the only place we
						// learn the host's group size, which the controller deducts from the
						// encoder rate so the wire rate stays at the target.
						// A bandwidth-probe packet arriving after the probe ended (its tail can trail
						// the stream request): nothing to do with it — and it must not reach the
						// audio forward below.
						if tag == media::TAG_PROBE {
							continue;
						}
						// Audio parity: the native player rebuilds lost audio from it
						// (`pulsar_core::audio::AudioRepair`). Forwarded WITH its tag byte, which
						// can't be the first byte of an RTP v2 datagram, so the player tells the
						// two apart on the one audio port.
						if tag == media::TAG_AUDIO_FEC {
							if let Some(s) = fwd_sock.as_ref() {
								let _ = s.send_to(&bytes, a_dest).await;
							}
							continue;
						}
						if tag == media::TAG_FEC_RS || tag == media::TAG_FEC {
							let now = std::time::Instant::now();
							let po = if tag == media::TAG_FEC_RS {
								repair_st.on_rs_parity(rtp, now, nack_on)
							} else {
								repair_st.on_xor_parity(rtp, now, nack_on)
							};
							for (_, pkt) in &po.rebuilt {
								if !v_unix.try_send(pkt) {
									if let Some(s) = fwd_sock.as_ref() {
										let _ = s.send_to(pkt, v_dest).await;
									}
								}
							}
							// The block's parity is all here (or can't cover what is missing) and
							// packets of it are still missing: FEC is done with them — NACK now.
							if !po.nacks.is_empty() {
								let _ = send_data(&sess, &DataMsg::MediaNack(po.nacks)).await;
							}
							continue;
						}
						if let Some(s) = fwd_sock.as_ref() {
							let dest = if tag == media::TAG_VIDEO { v_dest } else { a_dest };
							if tag == media::TAG_VIDEO && !first_video_logged {
								first_video_logged = true;
								tracing::info!(%dest, "first video RTP datagram forwarded to renderer");
							}
							if !(tag == media::TAG_VIDEO && v_unix.try_send(rtp)) {
								let _ = s.send_to(rtp, dest).await;
							}
						} else if !first_video_logged {
							first_video_logged = true;
							tracing::warn!("media arrived but fwd_sock is None — nothing forwarded");
						}
						// Video gap tracking: count loss for the adaptive controller and
						// NACK freshly-missing seqs so the host retransmits them.
						if tag == media::TAG_VIDEO {
							last_video = std::time::Instant::now();
							if let Some(seq) = media::rtp_seq(rtp) {
								// Frame boundary = a new RTP timestamp: the previous frame is
								// complete, sample its send-vs-arrival delay for the estimator
								// (only a NEWER timestamp closes it, see `FrameClock`).
								if let Some(ts) = pulsar_core::adapt::repair::rtp_timestamp(rtp) {
									if let Some((prev, arr)) = frame_clock.on_packet(ts, mono_ms()) {
										ctl.on_frame(prev, arr);
										match fc {
											None => {
												fc = Some((prev, arr));
												fc_n = 0;
												fc_max_gap = 0.0;
											}
											Some((ts0, t0)) => {
												fc_n += 1;
												fc_max_gap = fc_max_gap.max(arr - fc_prev_arr);
												if arr - t0 >= 2000.0 {
													let rtp_ms = (prev.wrapping_sub(ts0) as i32) as f64 / 90.0;
													let wall_ms = arr - t0;
													let drift_pct = (rtp_ms / wall_ms - 1.0) * 100.0;
													let fps = fc_n as f64 * 1000.0 / wall_ms;
													// Info only when it's worth a look: the host clock drifting
													// from ours, or a long hole in the stream. (A stream restart
													// re-bases the host clock — that window is skipped.)
													if rtp_ms > 0.0 && (drift_pct.abs() > 2.0 || fc_max_gap > 250.0) {
														tracing::info!(
															frames = fc_n,
															fps = format!("{fps:.1}"),
															rtp_ms = format!("{rtp_ms:.0}"),
															wall_ms = format!("{wall_ms:.0}"),
															rtp_vs_wall_pct = format!("{drift_pct:.1}"),
															max_gap_ms = format!("{fc_max_gap:.0}"),
															"frame clock: host RTP clock drifts or the stream had a hole"
														);
													} else {
														tracing::debug!(
															frames = fc_n,
															fps = format!("{fps:.1}"),
															rtp_vs_wall_pct = format!("{drift_pct:.1}"),
															max_gap_ms = format!("{fc_max_gap:.0}"),
															"frame clock"
														);
													}
													fc = Some((prev, arr));
													fc_n = 0;
													fc_max_gap = 0.0;
												}
											}
										}
										fc_prev_arr = arr;
									}
								}
								// Gap tracking (`RepairTracker::on_video`): loss for the controller,
								// and each missing seq NACKed after the reorder wait (plus FEC's go
								// while parity flows). A big seq jump = the encoder restarted with a
								// fresh RTP sequence base (every monitor/codec switch): resync.
								let vo = repair_st.on_video(seq, rtp, std::time::Instant::now(), nack_on, cur_fps);
								if vo.restart {
									ctl.on_stream_restart();
									frame_clock.reset();
								}
								if !vo.nacks.is_empty() {
									let _ = send_data(&sess, &DataMsg::MediaNack(vo.nacks)).await;
								}
							}
						}
						continue;
					}
						// Reached only by NON-media (control) datagrams -- the media fast-path
						// continue-d above. Reset the grab-release watchdog HERE so it tracks CONTROL
						// liveness (keepalive Pong + any DataMsg), not the video flow: a dead control
						// link with live video must still trip the timeout below and tear the session
						// down instead of leaving it frozen-but-grabbed.
						last_inbound = std::time::Instant::now();
						// Host sent an explicit Bye (it kicked us / went offline / left): end the
						// session NOW. Over a relay our recv never returns None when the host
						// vanishes, so without this the client hangs on the frozen last frame and
						// shows the stall message until the silence watchdog eventually fires.
						// Breaking here runs the `play-ended` emit below — a clean, instant
						// disconnect the UI can act on.
						if pulsar_core::service::is_bye(&bytes) {
							tracing::info!(%id, "host sent Bye — ending play session");
							break;
						}
						if pulsar_core::service::is_pong(&bytes) {
						if let Some(rtt) = pings.on_pong(std::time::Instant::now()) {
							repair_st.on_rtt(rtt);
							let rtt = rtt as f64;
							// The latency and how much it varies, for the status bar and (below)
							// the renderer's tile and HUD.
							let jitter = repair_st.rtt().jitter() as f64;
							let _ = app_ev.emit("play-rtt", PlayRtt { id, rtt, jitter });
							// Feed the native overlay's "Gecikme" tile the real NETWORK
							// latency (it used to show the render present-gap), and size the
							// renderer's RTP reorder wait from it (Phase 0.5): a NACK retransmit
							// needs ~1 RTT to land, so a fixed 100 ms made every repair "too
							// late" on any route slower than that.
							{
								use std::io::Write as _;
								// The renderer waits one repair sweep LONGER than we do, so our
								// `hold` (sent when the sweep writes the repair off) reaches it
								// before its demuxer flushes past the hole and decodes the smear.
								// The controller sees a far sample at once (a stall: the fast reflex holds off).
								ctl.on_rtt(rtt as f32, mono_ms());
								let now_i = std::time::Instant::now();
								// Game mode on a high-RTT path with a keyframe-free recovery: drop a
								// hole fast instead of holding the frames behind it (`fast_drop_on`).
								let keyframe_free =
									cur_recovery == LossRecovery::IntraRefresh || rfi_reach(host_rfi, &cur_codec);
								let rtt_med = repair_st.rtt().median(rtt as f32);
								let fd = fast_drop_on(fast_drop, game_mode, keyframe_free, rtt_med);
								if fd != fast_drop {
									fast_drop = fd;
									nack_on = host_nack && !fd;
									tracing::info!(
										on = fd,
										rtt_med,
										wait_ms = repair_st.renderer_wait_ms(now_i, cur_fps, true),
										"game mode, high RTT: holes dropped fast (RFI / intra refresh heals them, no NACK)"
									);
								}
								let want = repair_st.renderer_wait_ms(now_i, cur_fps, fast_drop);
								let changed = (want - sent_repair_ms).abs() > sent_repair_ms * 0.15 + 5.0;
								if let Some(si) = render_stdin.lock().unwrap().as_mut() {
									let _ = writeln!(si, "rtt {rtt:.1} {jitter:.1}");
									if changed {
										let _ = writeln!(si, "maxdelay {}", (want * 1000.0) as u32);
									}
								}
								if changed {
									sent_repair_ms = want;
								}
							}
						}
					} else if let Some(dm) = decode_data(&bytes) {
						match dm {
							DataMsg::Clipboard(text) => {
								// Sync on: written to our clipboard here, silently (it is the
								// host's copy, not something to announce). Off: the UI copies it.
								if !crate::clipboard_sync::apply_remote(&text) {
									let _ = app_ev.emit("data-clip", DataPayload { peer: id.to_string(), text });
								}
							}
							DataMsg::Chat(text) => {
								let _ = app_ev.emit("chat-msg", DataPayload { peer: id.to_string(), text });
							}
							DataMsg::Stats(label) => {
								// Mirror the host's ACTIVE encode summary into the native
								// overlay ("hostenc <label>" stdin line) so it can show what's
								// really in use under the selectors.
								{
									use std::io::Write as _;
									if let Some(si) = render_stdin.lock().unwrap().as_mut() {
										let _ = writeln!(si, "hostenc {label}");
									}
								}
								let _ = app_ev.emit("host-stats", PlayStats { id, label });
							}
							DataMsg::HdrMeta { sdr_white_nits, peak_nits } => {
								// The HDR10 stream's brightness reference (host SDR content
								// brightness + monitor peak): the renderer tone-maps PQ to SDR
								// around the host's real SDR white instead of a fixed 203 nits.
								use std::io::Write as _;
								if let Some(si) = render_stdin.lock().unwrap().as_mut() {
									let _ = writeln!(si, "hdrref {sdr_white_nits} {peak_nits}");
								}
							}
							DataMsg::DisplayRotation(deg) => {
								// Render the video upright for a rotated host display (vidsink
								// path on Linux; the Windows pulsar-render rotates in its
								// VideoProcessor Blt via the `rotate <deg>` stdin line).
								#[cfg(all(unix, not(target_os = "macos")))]
								crate::render::apply_vidsink_rotation(&app_ev, id, deg);
								#[cfg(windows)]
								{
									use std::io::Write as _;
									if let Some(si) = render_stdin.lock().unwrap().as_mut() {
										let _ = writeln!(si, "rotate {}", deg % 360);
									}
								}
								#[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
								let _ = deg;
							}
							DataMsg::Avatar(png) => {
								tracing::info!(bytes = png.len(), "host identity image received");
								// The host's identity image for this session, addressed by play
								// id like the other client-side payloads.
								let _ = app_ev.emit("peer-avatar", AvatarPayload {
									peer: id.to_string(),
									data_url: crate::avatar::data_url(&png),
								});
							}
							DataMsg::PeerName(name) => {
								// The host's display name — the UI updates the session target
								// chip and the recents cache. Kept for the file toasts too.
								host_name = name.clone();
								// The renderer labels the host pointer with it (issue #7).
								{
									use std::io::Write as _;
									if let Some(si) = render_stdin.lock().unwrap().as_mut() {
										let _ = writeln!(si, "hostname {}", name.replace(char::is_control, " "));
									}
								}
								let _ = app_ev.emit("peer-name", (id.to_string(), name));
							}
							DataMsg::CursorPos { x, y } => {
								// Cursor side-channel: the host captured WITHOUT a hardware cursor
								// in the frame (KMS zero-copy) and streams the pointer position
								// out-of-band. Push it to the native renderer so it draws the cursor
								// over the video (Moonlight model). Tiny payload, ~60 Hz.
								use std::io::Write as _;
								if let Some(si) = render_stdin.lock().unwrap().as_mut() {
									let _ = writeln!(si, "cursor {x:.5} {y:.5}");
								}
							}
							DataMsg::CursorShape { w, h, hot_x, hot_y, rgba_png } => {
								// The host pointer's bitmap changed (caret/resize transitions).
								// Base64 the PNG onto one stdin line so the renderer swaps the drawn
								// cursor image + hotspot.
								use base64::Engine as _;
								use std::io::Write as _;
								let b64 = base64::engine::general_purpose::STANDARD.encode(&rgba_png);
								if let Some(si) = render_stdin.lock().unwrap().as_mut() {
									let _ = writeln!(si, "cursorimg {w} {h} {hot_x} {hot_y} {b64}");
								}
							}
							DataMsg::PointerLock(on) => {
								// The host hid its cursor (a game grabbed the mouse): send relative
								// motion while it lasts, absolute again once it shows.
								tracing::info!(on, "host pointer lock");
								crate::kbdhook::set_pointer_lock(id, on);
							}
							DataMsg::Displays(list) => {
								// The host's monitors changed mid-session: refresh the monitor
								// picker everywhere — the adaptive ladder's monitor table, the
								// renderer overlay (its caps line `displays=` token, re-sent) and
								// the webview session menu (`host-displays`).
								tracing::info!(count = list.len(), "host monitors changed");
								host_displays = list.iter().map(|d| (d.idx, d.width, d.height, d.refresh_hz)).collect();
								let caps_line = tauri::Manager::state::<crate::state::AppState>(&app_ev)
									.plays
									.lock()
									.unwrap()
									.get(&id)
									.map(|p| p.caps_line.clone());
								if let Some(caps_line) = caps_line {
									let token = format!("displays={}", super::fmt_displays(&list));
									let mut line = caps_line.lock().unwrap();
									if !line.is_empty() {
										*line = line
											.split_whitespace()
											.map(|kv| if kv.starts_with("displays=") { token.clone() } else { kv.to_string() })
											.collect::<Vec<_>>()
											.join(" ");
										use std::io::Write as _;
										if let Some(si) = render_stdin.lock().unwrap().as_mut() {
											let _ = writeln!(si, "{line}");
										}
									}
								}
								let _ = app_ev.emit("host-displays", (id, list));
							}
							DataMsg::StreamedDisplay(d) => {
								// The host streams another monitor than we asked for (Linux Wayland:
								// the person at the host picked it in the portal dialog). Follow it:
								// later requests (reconnect, ABR, codec) ask for what is shown, and
								// every picker (renderer overlay, webview menu) highlights it.
								tracing::info!(display = d, requested = cur_display, "host streams another monitor");
								if d != cur_display {
									cur_display = d;
									cur_window = None;
									// The ABR ladder's native size follows the monitor really
									// streamed; the running stream is not restarted for it.
									if let Some(&(_, w, h, hz)) = host_displays.iter().find(|m| m.0 == d) {
										let fps = if hz > 1 { client_fps.min(crate::util::nearest_fps(hz)) } else { client_fps };
										ctl.set_native(w, h, fps);
									}
								}
								if let Some(p) = tauri::Manager::state::<crate::state::AppState>(&app_ev)
									.plays
									.lock()
									.unwrap()
									.get(&id)
								{
									p.render_seed.lock().unwrap().display_idx = Some(d);
								}
								{
									use std::io::Write as _;
									if let Some(si) = render_stdin.lock().unwrap().as_mut() {
										let _ = writeln!(si, "display {d}");
										let _ = si.flush();
									}
								}
								let _ = app_ev.emit("host-display-streamed", (id, d));
							}
							DataMsg::HostPointer { active, x, y } => {
								// The person at the host is on their own mouse (remote mode): the
								// renderer draws the host pointer labelled "Host" + a notice and
								// shows the local cursor (issue #7). x/y < 0 = position unknown.
								use std::io::Write as _;
								if let Some(si) = render_stdin.lock().unwrap().as_mut() {
									if active {
										let _ = writeln!(si, "hostptr 1 {x:.5} {y:.5}");
									} else {
										let _ = writeln!(si, "hostptr 0");
									}
								}
								// The webview's canvas path (Linux/macOS) un-hides the local
								// cursor meanwhile.
								let _ = app_ev.emit("host-pointer", (id, active));
							}
							DataMsg::CursorHidden => {
								// The host pointer is hidden / left the screen — stop drawing it.
								use std::io::Write as _;
								if let Some(si) = render_stdin.lock().unwrap().as_mut() {
									let _ = writeln!(si, "cursorhide");
								}
							}
							DataMsg::Ds5Output { slot, report } => {
								crate::ds5::client::on_output(id, slot, &report);
							}
							DataMsg::Ds5Mic { slot, on } => {
								crate::ds5::client::on_mic(id, slot, on);
							}
							DataMsg::Rumble { slot, large, small } => {
								// The host forwarded the game's force-feedback — hand it to the
								// gilrs reader thread to replay on the physical pad at `slot`.
								tracing::info!(slot, large, small, "rumble: client received → reader");
								let _ = rumble_tx.try_send((slot, large, small));
							}
							DataMsg::PadUnavailable { slot, reason } => {
								// The host could not create a virtual pad for this controller
								// (e.g. a Mac host build without the virtual-HID entitlement): its
								// input goes nowhere — say so in-session (drained on the 2 s tick).
								tracing::warn!(slot, %reason, "host has no virtual pad for this controller");
								crate::play_notice::push(
									id,
									crate::pad_notice::client_text(slot, &reason, crate::i18n::lang() == "en"),
								);
							}
							DataMsg::InputUnavailable { reason } => {
								// The host cannot pass our mouse / keyboard to its desktop (no
								// /dev/uinput access, no Accessibility grant, an elevated window in
								// front of a non-elevated Windows host): say why in-session instead of
								// leaving a picture that ignores every click (drained on the 2 s tick).
								// The host rate-limits these.
								tracing::warn!(%reason, "host reports remote input does not reach its desktop");
								crate::play_notice::push(
									id,
									crate::input_notice::client_text(&reason, crate::i18n::lang() == "en"),
								);
							}
							DataMsg::FsEntries { path, entries } => {
								// A host directory listing for the file panel's remote pane.
								let _ = app_ev.emit("fs-entries", FsEntriesPayload { id, path, entries });
							}
							DataMsg::FileBegin { id: xfer, name, size: _, chunks } => {
								// Evict stale entries (lost FileEnd / lost middle chunk) so a
								// long-lived session over a lossy link can't accumulate unbounded
								// dead reassemblers.
								let now = std::time::Instant::now();
								f_xfers.retain(|_, r| now.duration_since(r.last_activity) < F_XFER_IDLE_TIMEOUT);
								// Hard cap: if still at the concurrent limit, evict the
								// least-harmful entry — but not the lazy placeholder for THIS
								// id (don't evict the entry we're about to complete).
								// Prefer headless entries (expected == None; their FileBegin
								// was lost so they can never complete) over active ones;
								// tie-break by oldest last_activity.
								if f_xfers.len() >= F_MAX_CONCURRENT_XFERS && !f_xfers.contains_key(&xfer) {
									let victim = f_xfers
										.iter()
										.min_by_key(|(_, r)| (r.expected.is_some() as u8, r.last_activity))
										.map(|(k, _)| *k);
									if let Some(vid) = victim {
										f_xfers.remove(&vid);
									}
								}
								// If early FileChunks created a lazy entry (UDP reorder), merge
								// the name + expected into it to preserve buffered chunks.
								// Otherwise insert a fresh entry.
								let sane_name = crate::files::sanitize_filename(&name);
								if let Some(r) = f_xfers.get_mut(&xfer) {
									r.name = sane_name.clone();
									r.expected = Some(chunks);
									r.last_activity = now;
									// Prune any pre-buffered chunks whose index is now >= chunks.
									// They arrived before FileBegin (expected was None) so the
									// in_range guard passed them all through; now that we know the
									// count, out-of-range indices must be removed so they cannot
									// substitute for a genuinely lost in-range chunk.
									r.chunks.retain(|&idx, data| {
										if idx < chunks {
											true
										} else {
											r.received = r.received.saturating_sub(data.len() as u64);
											false
										}
									});
								} else {
									f_xfers.insert(xfer, FileReasm {
										name: sane_name.clone(),
										expected: Some(chunks),
										chunks: std::collections::BTreeMap::new(),
										received: 0,
										last_activity: now,
									});
								}
								// Notify the client UI that the host has started streaming
								// this file — the concurrency guard must NOT release the
								// wire slot on the short wall-clock timeout once a FileBegin
								// has been seen (the transfer is legitimately in flight).
								// xfer_id lets the UI associate this event with the exact
								// download() call that triggered it (keyed by id, not name),
								// preventing a timed-out same-name download from draining a
								// different in-flight slot (C21 fix).
								let _ = app_ev.emit("file-begin", crate::events::FileBeginPayload {
									peer: id.to_string(),
									name: sane_name,
									xfer_id: xfer,
								});
							}
							DataMsg::FileChunk { id: xfer, index, data } => {
								// If no entry exists yet (FileBegin hasn't arrived — UDP reorder),
								// create a lazy placeholder so the chunk is buffered rather than
								// dropped. FileBegin will fill in `name` and `expected` when it
								// arrives, keeping the already-stored chunks intact.
								if !f_xfers.contains_key(&xfer) {
									// Mirror the FileBegin guard: sweep idle entries and enforce
									// the concurrent cap before inserting a new headless entry.
									// Without this a chunk-only flood bypasses both the retain()
									// and the cap, allowing unbounded HashMap growth.
									let now = std::time::Instant::now();
									f_xfers.retain(|_, r| now.duration_since(r.last_activity) < F_XFER_IDLE_TIMEOUT);
									if f_xfers.len() >= F_MAX_CONCURRENT_XFERS {
										let victim = f_xfers
											.iter()
											.min_by_key(|(_, r)| (r.expected.is_some() as u8, r.last_activity))
											.map(|(k, _)| *k);
										if let Some(vid) = victim {
											f_xfers.remove(&vid);
										}
									}
									f_xfers.insert(xfer, FileReasm {
										name: String::new(),
										expected: None,
										chunks: std::collections::BTreeMap::new(),
										received: 0,
										last_activity: now,
									});
								}
								// Ignore an index past the announced count (bogus); a re-sent
								// index just overwrites with the identical bytes. Before FileBegin
								// (expected == None) we buffer all indices — the overflow check
								// runs once expected is known.
								let overflow = if let Some(r) = f_xfers.get_mut(&xfer) {
									r.last_activity = std::time::Instant::now();
									let in_range = r.expected.map_or(true, |e| index < e);
									if in_range {
										let prev_len = r.chunks.get(&index).map(|p| p.len() as u64).unwrap_or(0);
										let projected = r.received - prev_len + data.len() as u64;
										if projected > crate::files::MAX_XFER_BYTES {
											true
										} else {
											r.chunks.insert(index, data);
											r.received = projected;
											false
										}
									} else {
										false
									}
								} else {
									false
								};
								if overflow {
									// Peer is overshooting the sane transfer ceiling — drop the
									// whole transfer so the buffer can't grow unbounded. Emit
									// file-recv{ok:false} BEFORE removing so the download UI
									// learns the transfer was rejected instead of hanging forever
									// on 'indiriliyor…' with no feedback.
									if let Some(r) = f_xfers.remove(&xfer) {
										let _ = app_ev.emit("file-recv", FilePayload {
											peer: id.to_string(),
											name: r.name.clone(),
											bytes: 0,
											ok: false,
											xfer_id: xfer,
										});
									}
								}
							}
							DataMsg::FileEnd { id: xfer } => {
								// End the transfer: a repeated/stray FileEnd (no matching
								// Begin) must not re-save — `remove` drops the state so a
								// second End for the same id is a no-op.
								let Some(r) = f_xfers.remove(&xfer) else { continue };
								// Save only complete transfers (unordered UDP — a missing
								// index means a lost chunk; report a failed transfer instead
								// of corrupting). Reorders/dups already self-healed above.
								// `expected == Some(0)` is a legitimate empty file. If
								// `expected` is still `None`, FileBegin never arrived (lost
								// or extremely delayed); treat that as failed.
								// Contiguous check: len == e is not sufficient — an
								// out-of-range pre-Begin chunk (now pruned on FileBegin)
								// could otherwise substitute for a lost in-range chunk.
								let complete = r.expected.map_or(false, |e| {
									r.chunks.len() == e as usize
										&& (e == 0 || r.chunks.contains_key(&0) && r.chunks.contains_key(&(e - 1)))
										&& (0..e).all(|i| r.chunks.contains_key(&i))
								});
								// Offload the blocking disk write to a thread-pool thread so the
								// session loop (video RTP forwarding + input) is never stalled by a
								// multi-GiB file flush. Move the reassembly state into the closure;
								// clone the Tauri handles before spawning.
								let app_ev_file = app_ev.clone();
								let peer_str = id.to_string();
								let name_fallback = r.name.clone();
								let toast_peer = host_name.clone();
								tokio::task::spawn_blocking(move || {
									// Write chunks directly to disk without building a contiguous
									// intermediate Vec — avoids ~2x peak memory at the MAX_XFER_BYTES
									// ceiling (C24 fix).
									let saved = if complete {
										crate::files::save_received_file_chunks(
											&r.name,
											r.chunks.values(),
											r.received,
										)
									} else {
										None
									};
									let written = saved.as_ref().map(|(_, b)| *b).unwrap_or(0);
									// Pulsar's own toast: "file arrived", click = its folder.
									if let Some((p, _)) = saved.as_ref() {
										crate::toast::file_received(&app_ev_file, &toast_peer, p);
									}
									// Use the actual on-disk filename (may have a dedup suffix like
									// " (1)") so the UI flash names the file the user can actually open.
									let saved_name = saved
										.as_ref()
										.and_then(|(p, _)| p.file_name())
										.and_then(|n| n.to_str())
										.unwrap_or(&name_fallback)
										.to_string();
									let _ = app_ev_file.emit("file-recv", FilePayload {
										peer: peer_str,
										name: saved_name,
										bytes: written,
										ok: saved.is_some(),
										xfer_id: xfer,
									});
								});
							}
							_ => {}
						}
					} else if let Some(list) = pulsar_core::service::decode_windows(&bytes) {
						// Host reply to our QueryWindows (Bug 4): resolve the pending
						// window-list picker query from the inbound demux instead of a
						// blocking read-loop that consumed the interleaved media/RTP.
						if let Some((reply, _)) = pending_windows.take() {
							let _ = reply.send(list);
						}
					}
				}
				None => break, // host closed the session
			},
			// Keepalive + RTT probe (~PING_MS, uneven gaps), sent whether or not the last one was
			// answered: a lost Ping or Pong must not stall the RTT samples (and the repair window
			// they size). Pongs are matched to Pings by `PongMatcher`; one older than `PING_LOST`
			// is dropped.
			_ = &mut ping => {
				if send_keepalive(&mut sess).await.is_err() { break }
				let gap = pings.on_ping(std::time::Instant::now());
				ping.as_mut().reset(tokio::time::Instant::now() + gap);
			},
			// Repair sweep (REPAIR_SWEEP_MS): re-NACK once after ~1 RTT, then write the seq off
			// as a FAILED repair after the RTT-derived window — the decoder's reference chain
			// is broken until a keyframe / refresh, so (a) ask the host for an IDR now (the
			// MediaNack([0]) sentinel, ≤ 1/s) and (b) tell the renderer to HOLD its last good
			// frame for a bounded moment instead of showing the smear (Phase 0.5) — neither on
			// an intra-refreshed stream, which heals by itself.
			_ = repair.tick() => {
				// Fast reflex (Phase 3): a queue that keeps filling between windows cuts the
				// rate now, without waiting for the 2 s tick.
				if adapt_enabled {
					if let Some(d) = ctl.poll_fast(mono_ms()) {
						let mut changed = false;
						if let Some(kbps) = d.bitrate {
							cur_bitrate = kbps;
							changed = true;
						}
						if let Some(p) = d.point {
							// The controller's point already honours the user's picks (a pick is a
							// preferred maximum it only goes under when the link can't carry it).
							cur_w = p.width;
							cur_h = p.height;
							cur_fps = p.fps;
							last_switch_at = std::time::Instant::now();
							changed = true;
						}
						if changed {
							tracing::info!(
								kbps = cur_bitrate,
								point = %ctl.point().label(),
								reason = d.reason,
								"abr fast step"
							);
							if request_stream(&mut sess, &make_req!()).await.is_err() {
								break;
							}
						}
					}
				}
				// Opening phase: the renderer has decoded its first frames but stays hidden behind
				// the preparing screen while the controller finds the path's rate; reveal once it
				// has settled (no cut for 2 windows, rate + point steady, clean) or after 8 s at most.
				if !revealed
					&& crate::render_stats::HOLD_REVEAL_ID.load(std::sync::atomic::Ordering::SeqCst) == id
					&& crate::render_stats::FIRST_FRAMES_ID.load(std::sync::atomic::Ordering::SeqCst) == id
				{
					let t = *first_frames_at.get_or_insert_with(std::time::Instant::now);
					let settled = ctl.startup_settled();
					if settled || t.elapsed() >= std::time::Duration::from_secs(8) {
						revealed = true;
						{
							use std::io::Write as _;
							if let Some(si) = render_stdin.lock().unwrap().as_mut() {
								let _ = writeln!(si, "reveal");
								let _ = si.flush();
							}
						}
						crate::tabwin::mark_ready(id);
						let _ = app_ev.emit("play-ready", id);
						tracing::info!(
							settled,
							waited_ms = t.elapsed().as_millis() as u64,
							kbps = ctl.target_kbps(),
							point = %ctl.point().label(),
							"stream settled → revealed"
						);
					}
				}
				// The renderer fell back to waiting for a keyframe (a gap its reorder window could
				// not wait out, or a resync) while this loop may count the loss as repaired: ask now.
				if crate::render_stats::RENDER_WANTS_IDR.swap(false, std::sync::atomic::Ordering::Relaxed)
					&& host_nack
					&& kf_pacer.try_request(std::time::Instant::now(), std::time::Duration::ZERO, repair_st.rtt().ema())
				{
					let _ = send_data(&sess, &DataMsg::MediaNack(vec![0])).await;
					tracing::info!("renderer lost its reference picture → keyframe request");
				}
				{
					// Held-back NACKs whose wait ran out while no packet came to send them, one
					// re-NACK ~1 RTT after a NACK, and the write-offs (`RepairTracker::sweep`).
					let sw = repair_st.sweep(std::time::Instant::now(), cur_fps, fast_drop, nack_on);
					if !sw.nacks.is_empty() {
						let _ = send_data(&sess, &DataMsg::MediaNack(sw.nacks)).await;
					}
					let expired = sw.expired;
					if expired > 0 {
						// An intra-refreshed stream heals itself within its refresh period: no
						// keyframe (whose size spike is what IR avoids) and no renderer hold
						// either (see `loss_hold_ms`) — it keeps decoding and presenting the
						// concealed, healing picture. (If the renderer itself loses the thread it
						// still asks, via RENDER_WANTS_IDR.)
						let ir = cur_recovery == LossRecovery::IntraRefresh;
						// RFI: the renderer already reported the hole and recovers without one (or
						// asks for one itself when the host could not reach the lost frame).
						let rfi = rfi_reach(host_rfi, &cur_codec);
						if !ir
							&& !rfi && host_nack
							&& kf_pacer.try_request(
								std::time::Instant::now(),
								std::time::Duration::from_secs(1),
								repair_st.rtt().ema(),
							) {
							let _ = send_data(&sess, &DataMsg::MediaNack(vec![0])).await;
						}
						// Sent on every sweep that writes a packet off: the renderer arms one hold per
						// loss episode and treats the rest as that episode going on.
						let hold = loss_hold_ms(cur_recovery, rfi && !ir);
						if let Some(ms) = hold {
							use std::io::Write as _;
							if let Some(si) = render_stdin.lock().unwrap().as_mut() {
								let _ = writeln!(si, "hold {ms}");
							}
						}
						tracing::info!(
							expired,
							ir,
							hold = hold.is_some(),
							"unrepaired video loss → renderer hold + keyframe request (neither on intra-refresh; no keyframe on RFI)"
						);
					}
				}
			},
			// The renderer saw a hole (reference-frame invalidation mode): tell the host at once,
			// while it can still reach the lost frame — it invalidates it and tags its recovery
			// frame, so no keyframe is needed (the NACK repair keeps running meanwhile).
			_ = crate::render_stats::RFI_WAKE.notified() => {
				let rfi_lost = crate::render_stats::RENDER_WANTS_RFI.lock().unwrap().take();
				if let Some((first, last)) = rfi_lost {
					let _ = send_data(&sess, &DataMsg::MediaRfi { first, last }).await;
					tracing::debug!(first, last, "renderer hole → reference-frame invalidation request");
				}
			},
			_ = keep.tick() => {
				awake.tick(std::time::Instant::now());
				// Host silent too long (a kick / drop without a clean UDP close) → end the
				// session so the client tears down instead of freezing with input grabbed.
				// Media counts as liveness (a lossy link drops Pongs long before it drops the
				// stream); a control channel dead for longer still ends it (`host_silent`).
				if host_silent(last_inbound.elapsed(), last_media.elapsed()) {
					tracing::info!(
						control_silent_ms = last_inbound.elapsed().as_millis() as u64,
						media_silent_ms = last_media.elapsed().as_millis() as u64,
						"host silent → ending play session"
					);
					// Say why (the frontend shows it): the tab would otherwise just vanish.
					let _ = app_ev.emit("play-error", (id, "hostSilent"));
					break;
				}
				let stopped = video_stopped(first_video_logged, last_video.elapsed(), last_inbound.elapsed());
				if stopped != told_stopped {
					told_stopped = stopped;
					tracing::info!(
						stopped,
						video_silent_ms = last_video.elapsed().as_millis() as u64,
						"host video stream {}",
						if stopped { "stopped" } else { "is back" }
					);
					if let Some(si) = render_stdin.lock().unwrap().as_mut() {
						use std::io::Write as _;
						let _ = writeln!(si, "stopped {}", stopped as u8);
						let _ = si.flush();
					}
				}
				// Notices from outside this loop (DualSense auto-degrade, link hint).
				for text in crate::play_notice::take(id) {
					tracing::info!(%text, "play notice");
					let mut native = false;
					if let Some(si) = render_stdin.lock().unwrap().as_mut() {
						use std::io::Write as _;
						native = true;
						let _ = writeln!(si, "toast {text}");
						let _ = si.flush();
					}
					if !native {
						let _ = app_ev.emit("play-notice", serde_json::json!({ "id": id, "text": text }));
					}
				}
				let live = sess.live_transport().await;
				// Relay → direct upgrade, or a direct path that died and fell back to the relay:
				// what the controller learned on the old path (a queue that collapsed it to the
				// floor, a relay-sized cap) does not describe the new one. Each path gets its own
				// cap (`path_cap_kbps`): the relay's forwarding limit only while relayed.
				let now_relay = matches!(live, pulsar_core::Transport::Relay);
				if now_relay != on_relay {
					on_relay = now_relay;
					ctl.path_changed(path_cap_kbps(on_relay, sess.rate_cap_kbps(), direct_cap_kbps));
					tracing::info!(
						to = if on_relay { "relay" } else { "direct" },
						cap_kbps = ctl.cap(),
						kbps = ctl.target_kbps(),
						"media path changed: the adaptive rate starts over on the new path"
					);
					// Onto the relay: a rate above its cap is dropped there from the next packet —
					// come under it now, not at the window's decision.
					let under = if auto_on { ctl.encoder_kbps() } else { fixed_rate(&ctl) };
					if adapt_enabled && on_relay && under < cur_bitrate {
						cur_bitrate = under;
						if request_stream(&mut sess, &make_req!()).await.is_err() {
							break;
						}
					}
				}
				// The path class the media runs on now (the transport may switch mid-session).
				// On a change, the last good rate learned so far belongs to the OLD path: mark it
				// as already remembered, so only a rate found clean on the new path is stored
				// under the new class.
				let now_path = (
					live,
					if matches!(live, pulsar_core::Transport::Direct) {
						sess.peer_addr().await.map(|a| a.ip())
					} else {
						None
					},
				);
				let class = if now_path == class_of {
					cur_class
				} else if now_path == start_path {
					// Back on the session-start path: keep its verdict (judged with the round
					// trip too), so an overlay the interface check misses stays Wan.
					class_of = now_path;
					path_class
				} else {
					class_of = now_path;
					// The measured round trip of the live path feeds the same RTT fallback as
					// the start-of-session class.
					let rtt = match sess.path_latency().await {
						Some((d, r)) => match live {
							pulsar_core::Transport::Direct => d,
							pulsar_core::Transport::Relay => r,
						},
						None => None,
					};
					crate::adapt_memory::classify(now_path.0, now_path.1, rtt)
				};
				if class != cur_class {
					tracing::info!(from = cur_class.as_str(), to = class.as_str(), "media path class changed");
					cur_class = class;
					remembered_kbps = ctl.last_good_kbps();
				}
				// The real window length (the interval skips missed ticks, and a busy loop can run
				// a tick late): the controller turns `recv_bytes` into a delivered rate with it.
				let window_ms = win_start.elapsed().as_millis().min(u32::MAX as u128) as u32;
				win_start = std::time::Instant::now();
				// The window's counters and RTT samples (reset for the next one).
				let win = repair_st.take_window();
				// Field measurement, one line per window: what actually arrived (first copies),
				// what is still missing, and the delivered video rate vs the target — tells
				// congestion (goodput pinned under the target, loss rising with the rate) from
				// policing / MTU trouble (loss at any rate) in the next field log.
				tracing::info!(
					recv = win.recv,
					fresh = win.recv_fresh,
					reordered = win.reordered,
					reorder_wait_ms = win.reorder_wait_ms,
					lost = win.lost,
					recv_bytes = win.recv_bytes,
					window_ms,
					goodput_kbps = goodput_kbps(win.recv_bytes, window_ms),
					target = ctl.target_kbps(),
					kbps = cur_bitrate,
					point = %ctl.point().label(),
					path = cur_class.as_str(),
					"net window"
				);
				// The FEC share follows the parity this window carried, and is released when the host
				// turns parity off (it sends nothing that would say so) — `update_fec_share`.
				if repair_st.update_fec_share(&mut ctl, std::time::Instant::now(), &win) == FecShare::Released {
					tracing::info!("no parity from the host lately → FEC share released");
				}
				// ---- Adaptive controller (`pulsar_core::adapt`): one decision per 2 s window from
				// loss (seq gaps after NACK repair) + the window's RTT samples. Actuates a
				// bitrate step and/or the loss-recovery mode flip by re-requesting the stream
				// (live NVENC reconfigure on the native path; an encoder restart elsewhere).
				let sample = win.sample(window_ms);
				if last_switch_at.elapsed() < std::time::Duration::from_millis(2500) {
					// A recent menu switch / point change caused an encoder-rebuild gap — skip
					// this window so that loss isn't mistaken for congestion (counters are
					// reset when the window was taken).
				} else if adapt_enabled {
					let mut d = ctl.tick(&sample);
					// Windows: the client's own decoder is the limit when it can't decode the
					// current fps in real time — step the fps down instead of freezing.
					if cfg!(windows) && d.point.is_none() && auto_on {
						use std::sync::atomic::Ordering::Relaxed;
						let dec_ms = f32::from_bits(crate::render_stats::DECODE_MS_BITS.load(Relaxed));
						let decoded = f32::from_bits(crate::render_stats::DECODED_FPS_BITS.load(Relaxed));
						let at = crate::render_stats::DECODE_AT_MS.load(Relaxed);
						let fresh = at > 0 && crate::render_stats::stats_clock_ms().saturating_sub(at) <= 2500;
						if fresh && decoder_overloaded(dec_ms, decoded, cur_fps) {
							dec_over_windows += 1;
						} else {
							dec_over_windows = 0;
						}
						if dec_over_windows >= 2 {
							let cb = ctl.client_decode_bound(dec_ms);
							if let Some(p) = cb.point {
								tracing::info!(decode_ms = dec_ms, decoded, to = %p.label(), "client decoder can't keep up → lower fps");
								d.point = Some(p);
								d.reason = cb.reason;
								dec_over_windows = 0;
							}
						}
					}
					let sig = ctl.last();
					let mut rerequest = false;
					if let Some(r) = d.recovery {
						flip_want = Some(r);
						cur_recovery = flip_recovery(r, host_rfi, &cur_codec);
						rerequest = true;
					}
					if let Some(kbps) = d.bitrate {
						cur_bitrate = kbps;
						rerequest = true;
					}
					if let Some(p) = d.point {
						// The controller's point already honours the user's picks (a pick is a
						// preferred maximum it only goes under when the link can't carry it).
						cur_w = p.width;
						cur_h = p.height;
						cur_fps = p.fps;
						// The host rebuilds its encoder for a size/fps change: ignore the gap.
						last_switch_at = std::time::Instant::now();
						rerequest = true;
					}
					if d.is_change() {
						tracing::info!(
							loss_pct = sig.loss * 100.0,
							eff_pct = sig.eff_loss * 100.0,
							rtt = sig.rtt_ms,
							excess = sig.excess_ms,
							jitter = sig.jitter_ms,
							delay = ?sig.delay,
							trend = sig.trend,
							target = ctl.target_kbps(),
							kbps = cur_bitrate,
							point = %ctl.point().label(),
							ceiling = ctl.ceiling(),
							probe_wait = ctl.probe_wait(),
							noise_pct = ctl.noise_loss() * 100.0,
							fec_pct = ctl.fec_overhead() * 100.0,
							recovery = ?cur_recovery,
							user_cap = ctl.user_cap(),
							reason = d.reason,
							"abr step"
						);
					} else {
						tracing::debug!(
							loss_pct = sig.loss * 100.0,
							rtt = sig.rtt_ms,
							excess = sig.excess_ms,
							jitter = sig.jitter_ms,
							delay = ?sig.delay,
							trend = sig.trend,
							target = ctl.target_kbps(),
							kbps = cur_bitrate,
							point = %ctl.point().label(),
							ceiling = ctl.ceiling(),
							nack_sent = sample.nack_sent,
							nack_ok = sample.nack_ok,
							fec_ok = sample.fec_ok,
							"abr window"
						);
					}
					// Per-peer memory: the rate that stayed clean for 30 s seeds the next
					// session to this peer (60 % of it, then a fast startup probe).
					let good = ctl.last_good_kbps();
					if good > 0 && good != remembered_kbps {
						remembered_kbps = good;
						crate::adapt_memory::remember(&app_ev, &peer_key, cur_class, good);
					}
					// The saved picks are a preferred maximum: when the link can't carry them and
					// the controller runs a lighter point, tell the user once (renderer toast; the
					// webview gets a `play-notice` when there is no native renderer).
					let shortfall = if auto_on { ctl.pick_shortfall() } else { None };
					let shortfall_key = shortfall.map(|(a, b)| (point_key(&a), point_key(&b)));
					if shortfall_due(shortfall_told, shortfall_told_at.map(|t| t.elapsed()), shortfall_key) {
						if let Some((picked, current)) = shortfall {
							shortfall_told = shortfall_key;
							shortfall_told_at = Some(std::time::Instant::now());
							let text = shortfall_text(&picked.label(), &current.label(), crate::i18n::lang() == "en");
							tracing::info!(picked = %picked.label(), current = %current.label(), "pick shortfall → notice");
							use std::io::Write as _;
							let mut native = false;
							if let Some(si) = render_stdin.lock().unwrap().as_mut() {
								native = true;
								let _ = writeln!(si, "toast {text}");
								let _ = si.flush();
							}
							if !native {
								let _ = app_ev.emit("play-notice", serde_json::json!({ "id": id, "text": text }));
							}
						}
					}
					// Client → host feedback (Phase 0.3 / 2.4): what we measured and what we
					// settled on, as JSON in the existing Stats side channel. Old hosts ignore
					// it; a new host sizes its FEC parity from the loss.
					let stats = ClientStats {
						v: ClientStats::VERSION,
						loss: sig.loss,
						rtt_ms: sig.rtt_ms,
						jitter_ms: sig.jitter_ms,
						kbps: cur_bitrate,
						recovery: cur_recovery,
						nack_sent: sample.nack_sent,
						nack_ok: sample.nack_ok,
						fec_ok: sample.fec_ok,
						// The controller's point even under user picks (a pick is a preferred
						// maximum it goes under when the link can't carry it): the host sizes its
						// FEC from the point's fps.
						point: stream_point(&ctl, auto_on).label(),
					};
					let _ = send_data(&sess, &DataMsg::Stats(stats.to_json())).await;
					// Keep the renderer's resume mode in step with the stream's recovery mode
					// (every window: a respawned renderer starts in the safe wait-for-IDR mode).
					{
						use std::io::Write as _;
						if let Some(si) = render_stdin.lock().unwrap().as_mut() {
							let _ = writeln!(si, "ir {}", u8::from(cur_recovery == LossRecovery::IntraRefresh));
							// The stream was asked with `rfi` (deep reference buffer, see make_req!).
							// The host decides per hole whether its reference buffer still reaches
							// the lost frame, and sends a keyframe itself when it doesn't.
							// (Off on an intra-refreshed stream: the wave heals it, the host declines RFI.)
							let rfi = rfi_reach(host_rfi, &cur_codec) && cur_recovery != LossRecovery::IntraRefresh;
							let _ = writeln!(si, "rfi {}", u8::from(rfi));
							// The "Otomatik ayar" row (a respawned renderer starts from its default).
							let _ = writeln!(si, "autoadj {}", u8::from(auto_on));
						}
					}
					if rerequest {
						if request_stream(&mut sess, &make_req!()).await.is_err() {
							break;
						}
					}
				}
			},
			q = restream_rx.recv() => match q {
				// Session menu changed the resolution or encoder: merge it into the
				// current state and re-request — the host kills the old ffmpeg and
				// restarts capture/encode with the new setting.
				Some(cmd) => {
					// A menu re-request (esp. a monitor switch) → the host rebuilds its encoder
					// (a brief gap). Mark it so the adaptive controller ignores that gap's loss.
					last_switch_at = std::time::Instant::now();
					// What makes the host rebuild its encoder (a fresh RTP sequence base).
					let encoder_key = |w: u32, h: u32, fps: u32, codec: &str, enc: &str, disp: u32, win: Option<i64>, adapt: Option<(u32, u32)>| {
						(w, h, fps, codec.to_owned(), enc.to_owned(), disp, win, adapt)
					};
					let before = encoder_key(cur_w, cur_h, cur_fps, &cur_codec, &cur_encoder, cur_display, cur_window, cur_adapt);
					match cmd {
						// An explicit size pins the ladder's resolution; (0, 0) = "Otomatik"
						// hands it back to the controller (which re-picks for its target).
						Restream::Resolution(w, h) => {
							manual_res = w != 0 && h != 0;
							if manual_res {
								cur_w = w;
								cur_h = h;
							}
							// The ladder keeps adapting the fps (and the rate) under a pinned size.
							ctl.set_pins(manual_res.then_some((cur_w, cur_h)), manual_fps.then_some(cur_fps));
							// With the controller running, a pick is a preferred maximum: stream its
							// point (lighter than the pick while the link can't carry it), as its
							// own decisions do.
							let p = stream_point(&ctl, auto_on);
							if adapt_enabled || !manual_res {
								cur_w = p.width;
								cur_h = p.height;
							}
							if adapt_enabled || !manual_fps {
								cur_fps = p.fps;
							}
						}
						Restream::Encoder(e) => { cur_encoder = e; }
						Restream::Codec(c) => {
							// The rungs' minimum bitrates follow the codec's efficiency.
							ctl.set_codec(pulsar_core::adapt::codec_from_wire(&c));
							cur_codec = c;
							// HDR10 follows the codec (its decoder may not do 10-bit).
							cur_hdr = hdr_neg.want(&cur_codec);
							// RFI reaches the new codec or not: the flip's mode follows.
							if let Some(w) = flip_want {
								cur_recovery = flip_recovery(w, host_rfi, &cur_codec);
							}
						}
						Restream::Fps(f) => {
							manual_fps = f != 0;
							if manual_fps {
								cur_fps = f;
							}
							// The ladder keeps adapting the size (and the rate) under a pinned fps.
							ctl.set_pins(manual_res.then_some((cur_w, cur_h)), manual_fps.then_some(cur_fps));
							// A preferred maximum under the controller (see Resolution above).
							let p = stream_point(&ctl, auto_on);
							if adapt_enabled || !manual_fps {
								cur_fps = p.fps;
							}
							if adapt_enabled || !manual_res {
								cur_w = p.width;
								cur_h = p.height;
							}
						}
						// A user-picked bandwidth limit (b > 0) is the adaptive controller's
						// CEILING, not a fixed rate: it keeps adapting under it (a 10 Mbit limit on a
						// 50 Mbit line still backs off when the line congests) and the ladder picks
						// the best point the limit buys. "Otomatik" (0) lifts it. A relayed
						// session's per-session cap still bounds it (the controller's own cap).
						Restream::Bitrate(b) => {
							ctl.set_user_cap(b);
							// Fixed ("Otomatik ayar" off): the pick itself, or the measured good rate.
							cur_bitrate = if auto_on { ctl.encoder_kbps() } else { fixed_rate(&ctl) };
							// Picks are a preferred maximum under the controller (see Resolution).
							let p = stream_point(&ctl, auto_on);
							if adapt_enabled || !manual_res {
								cur_w = p.width;
								cur_h = p.height;
							}
							if adapt_enabled || (!manual_res && !manual_fps) {
								cur_fps = p.fps;
							}
							tracing::info!(limit_kbps = b, cap = ctl.cap(), rate = cur_bitrate, point = %ctl.point().label(), "bandwidth limit set");
						}
						Restream::Quality(q) => { cur_quality = q; }
						Restream::AudioQuality(pref) => {
							cur_audio_quality =
								crate::play::audio_quality_for(&pref, auto_audio_quality, host_audio_raw);
							tracing::info!(pref, quality = ?cur_audio_quality, "audio quality change requested");
						}
						Restream::Audio(t, m) => {
							tracing::info!(transmit = t, mute = m, "audio restream requested");
							cur_transmit = t;
							cur_mute = m;
						}
						Restream::Display(d) => {
							tracing::info!(display = d, "host monitor switch requested");
							cur_display = d;
							// Rebuild the ladder for the new monitor: its native size and the fps it
							// can capture at (its refresh, capped by the client's own target).
							if let Some(&(_, w, h, hz)) = host_displays.iter().find(|m| m.0 == d) {
								let fps = if hz > 1 { client_fps.min(crate::util::nearest_fps(hz)) } else { client_fps };
								ctl.set_native(w, h, fps);
								// Picks are a preferred maximum under the controller (see Resolution).
								let p = stream_point(&ctl, auto_on);
								if adapt_enabled || !manual_res {
									cur_w = p.width;
									cur_h = p.height;
								}
								if adapt_enabled || !manual_fps {
									cur_fps = p.fps;
								}
								tracing::info!(display = d, native = %format!("{w}x{h}@{fps}"), point = %p.label(), "ladder rebuilt for the new monitor");
							}
							// Picking a monitor clears any per-window target: the two are
							// mutually exclusive (the host ignores display_idx when window_hwnd
							// is Some), so a Display switch means "go back to whole-monitor".
							cur_window = None;
						}
						Restream::AutoAdjust(on) => {
							if on != auto_on && adapt_enabled {
								auto_on = on;
								if on {
									// Back on: the controller resumes from the rate streamed now, under
									// the same picks (rebuilt so the point fits that rate).
									ctl.set_adaptive(true);
									ctl.set_target(cur_bitrate);
									let (pr, pf) = ctl.pins();
									ctl.set_pins(pr, pf);
									cur_bitrate = ctl.encoder_kbps();
								} else {
									ctl.set_adaptive(false);
									cur_bitrate = fixed_rate(&ctl);
								}
								let p = stream_point(&ctl, auto_on);
								cur_w = p.width;
								cur_h = p.height;
								cur_fps = p.fps;
								tracing::info!(on, kbps = cur_bitrate, point = %p.label(), "automatic adjustment (Otomatik ayar) toggled");
								use std::io::Write as _;
								if let Some(si) = render_stdin.lock().unwrap().as_mut() {
									let _ = writeln!(si, "autoadj {}", u8::from(auto_on));
									let _ = si.flush();
								}
							}
						}
						Restream::Window(w) => {
							tracing::info!(hwnd = ?w, "per-window capture target requested");
							cur_window = w;
						}
						Restream::Adapt(a) => {
							tracing::info!(adapt = ?a, "screen adaptation toggle");
							cur_adapt = a;
							// The host now runs its monitor at (about) this window's size: the
							// ladder must top out there, not at the monitor's old native size —
							// it stayed at 1920×1200 and a 2881×1801 desktop was sent scaled down
							// to 1200p (2026-11-15). Turning it off goes back to the monitor's own.
							let disp = host_displays.iter().find(|m| m.0 == cur_display).copied();
							let hz = disp.map(|m| m.3).unwrap_or(0);
							let fps = if hz > 1 { client_fps.min(crate::util::nearest_fps(hz)) } else { client_fps };
							let native = match a {
								Some((w, h)) => Some((w & !1, h & !1)),
								None => adapt_restore.take().or(disp.map(|m| (m.1, m.2))),
							};
							if a.is_some() && adapt_restore.is_none() {
								adapt_restore = disp.map(|m| (m.1, m.2));
							}
							if let Some((w, h)) = native {
								ctl.set_native(w, h, fps);
								let p = stream_point(&ctl, auto_on);
								if adapt_enabled || !manual_res {
									cur_w = p.width;
									cur_h = p.height;
								}
								if adapt_enabled || !manual_fps {
									cur_fps = p.fps;
								}
								tracing::info!(native = %format!("{w}x{h}@{fps}"), point = %p.label(), "ladder rebuilt for screen adaptation");
							}
						}
					}
					let req = make_req!();
					let rs = request_stream(&mut sess, &req).await;
					tracing::info!(display_idx = cur_display, ok = rs.is_ok(), "restream request_stream sent");
					if rs.is_err() {
						break;
					}
					// The host rebuilds its encoder on a fresh RTP base: forget the old stream's
					// gaps / FEC state and frame clock now (the mobile client does the same). The
					// seq-jump detection alone misses a new base that lands close to the old one —
					// bogus gaps, NACKs and counted loss.
					if encoder_key(cur_w, cur_h, cur_fps, &cur_codec, &cur_encoder, cur_display, cur_window, cur_adapt) != before {
						repair_st.reset_stream();
						frame_clock.reset();
					}
				}
				None => {}
			},
			// On-demand host-window-list query (Phase 2b co-op "window" capture picker, Bug 4):
			// FIRE the QueryWindows request and stash the reply-sender; the host's `Windows`
			// answer is resolved from the inbound demux above (out of the media/RTP racing on
			// this same session), NOT via a blocking read-loop whose 16-message budget the video
			// stream would eat. A still-pending prior query is superseded → answer it empty so its
			// caller never hangs. A send failure answers empty (picker shows no windows).
			q = windows_query_rx.recv() => match q {
				Some(reply) => {
					if let Some((old, _)) = pending_windows.take() {
						let _ = old.send(Vec::new());
					}
					if pulsar_core::service::send_query_windows(&sess).await.is_err() {
						let _ = reply.send(Vec::new());
					} else {
						pending_windows = Some((
							reply,
							tokio::time::Instant::now() + std::time::Duration::from_secs(3),
						));
					}
				}
				None => {}
			},
			// Fallback deadline for a pending window-list query the host never answered (old /
			// non-Windows host → no `Windows` reply): resolve it to an empty list so the picker
			// just shows no windows instead of the caller hanging. Disabled when nothing pends.
			_ = tokio::time::sleep_until(win_deadline.unwrap_or_else(tokio::time::Instant::now)),
				if win_deadline.is_some() =>
			{
				if let Some((reply, _)) = pending_windows.take() {
					let _ = reply.send(Vec::new());
				}
			},
		}
	}
	// Tell the host we're leaving so it tears down immediately (kills ffmpeg +
	// releases held input) instead of waiting out PEER_TIMEOUT.
	// Hold-loop ended (host closed the session, a network error, or we're leaving): tell the
	// frontend so it tears the session down — release the evdev/input grab + drop the tab —
	// instead of freezing on mpv's last frame with the keyboard/mouse still captured.
	let _ = app_ev.emit("play-ended", id);
	// The session's file-manager window (if open) dies with the session.
	crate::files_window::close(&app_ev, id);
	let _ = pulsar_core::service::send_bye(&mut sess).await;
	drop(sess);
}

#[cfg(test)]
mod input_gate_tests {
	use super::*;

	#[test]
	fn side_buttons_only_reach_hosts_that_inject_them() {
		let b = |button| InputEvent::PointerButton { button, down: true };
		for button in 0..=2 {
			assert!(host_takes(&b(button), false));
			assert!(host_takes(&b(button), true));
		}
		assert!(!host_takes(&b(3), false));
		assert!(!host_takes(&b(4), false));
		assert!(host_takes(&b(3), true));
		assert!(host_takes(&b(4), true));
		assert!(!host_takes(&b(5), true));
		assert!(host_takes(&InputEvent::Key { code: 30, down: true }, false));
	}
}

#[cfg(test)]
mod kf_tests {
	use super::*;
	use std::time::Duration;

	#[test]
	fn the_stream_stops_only_when_video_packets_stop_on_a_live_session() {
		let s = Duration::from_secs;
		// Packets still arriving (however mangled — a lossy link with no decodable AU): not stopped.
		assert!(!video_stopped(true, Duration::from_millis(300), s(1)));
		assert!(!video_stopped(true, s(4), s(1)));
		// No video datagram for 5 s while the host still answers: the host stopped sending.
		assert!(video_stopped(true, s(5), s(1)));
		// A session that never had video is still starting (screen picker, encoder start).
		assert!(!video_stopped(false, s(30), s(1)));
		// Control dead too: that is the session watchdog's call, not a "stopped" card.
		assert!(!video_stopped(true, s(30), s(6)));
	}

	#[test]
	fn media_keeps_the_session_alive_but_not_forever() {
		let s = Duration::from_secs;
		// Both flowing.
		assert!(!host_silent(s(1), Duration::ZERO));
		// Pongs lost on a lossy link while video still arrives: not silent...
		assert!(!host_silent(s(6), Duration::ZERO));
		assert!(!host_silent(s(14), Duration::from_millis(200)));
		// ...but a control channel dead for 15 s+ ends it even with live video.
		assert!(host_silent(s(16), Duration::ZERO));
		// Nothing at all for 5 s+ = dead link.
		assert!(host_silent(s(6), s(6)));
		// Control alive, media silent (host paused / choosing a screen): not silent.
		assert!(!host_silent(s(1), s(30)));
	}

	#[test]
	fn shortfall_notice_is_told_once_per_change() {
		let pick = (1920, 1080, 120);
		let a = Some((pick, (1920, 1080, 60)));
		let b = Some((pick, (1280, 720, 60)));
		let s = Duration::from_secs;
		// Nothing to tell.
		assert!(!shortfall_due(None, None, None));
		// First shortfall: told at once.
		assert!(shortfall_due(None, None, a));
		// Same one again: not repeated, however long ago.
		assert!(!shortfall_due(a, Some(s(1)), a));
		assert!(!shortfall_due(a, Some(s(600)), a));
		// A different point: told, but not within the gap of the last notice.
		assert!(!shortfall_due(a, Some(s(5)), b));
		assert!(shortfall_due(a, Some(SHORTFALL_NOTICE_GAP), b));
		// The link carries the pick again: nothing to tell.
		assert!(!shortfall_due(b, Some(s(60)), None));
	}

	#[test]
	fn shortfall_text_is_localized() {
		assert_eq!(shortfall_text("1080p120", "720p60", false), "Bağlantı 1080p120 taşımıyor — 720p60 kullanılıyor");
		assert_eq!(shortfall_text("1080p120", "720p60", true), "The connection can't carry 1080p120 — using 720p60");
	}








	#[test]
	fn an_intra_refreshed_stream_gets_no_loss_hold() {
		// IR: keep decoding + presenting through the loss — the wave heals it.
		assert_eq!(loss_hold_ms(LossRecovery::IntraRefresh, false), None);
		// The keyframe-recovered modes keep the bounded hold.
		assert_eq!(loss_hold_ms(LossRecovery::Normal, false), Some(HOLD_MS));
		assert_eq!(loss_hold_ms(LossRecovery::ShortGop, false), Some(HOLD_MS));
	}

	#[test]
	fn an_rfi_stream_gets_no_loss_hold() {
		// The recovery frame is no keyframe: a hold would freeze the healed picture.
		assert_eq!(loss_hold_ms(LossRecovery::Normal, true), None);
		assert_eq!(loss_hold_ms(LossRecovery::ShortGop, true), None);
		assert_eq!(loss_hold_ms(LossRecovery::IntraRefresh, true), None);
	}

	#[test]
	fn an_intra_refresh_flip_keeps_rfi_where_it_reaches() {
		use LossRecovery::*;
		// H.264 / HEVC from an RFI host: the short GOP (RFI stays on).
		assert_eq!(flip_recovery(IntraRefresh, true, "h265"), ShortGop);
		assert_eq!(flip_recovery(IntraRefresh, true, "h264"), ShortGop);
		// AV1 (no recovery tag) or a host without RFI: intra refresh as asked.
		assert_eq!(flip_recovery(IntraRefresh, true, "av1"), IntraRefresh);
		assert_eq!(flip_recovery(IntraRefresh, false, "h265"), IntraRefresh);
		// Other flips pass through.
		assert_eq!(flip_recovery(ShortGop, true, "h265"), ShortGop);
		assert_eq!(flip_recovery(ShortGop, false, "av1"), ShortGop);
	}

	#[test]
	fn rfi_is_the_hosts_call_at_any_rtt_and_fps() {
		// No RTT × fps gate any more: the host knows its reference buffer.
		assert!(rfi_reach(true, "h265"));
		assert!(rfi_reach(true, "h264"));
		// AV1 has no recovery tag; a host without RFI keeps the keyframe recovery.
		assert!(!rfi_reach(true, "av1"));
		assert!(!rfi_reach(false, "h265"));
	}









}
