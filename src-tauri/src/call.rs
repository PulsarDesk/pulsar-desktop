//! Voice calls over a session, like a phone (the protocol: `pulsar_core::call`). Either side may
//! call — the host from its connections window, the client from the in-session menu (Audio →
//! Voice call). The callee hears the ringtone (`ringtone`, picked in Settings) and sees an
//! incoming-call card: a Pulsar toast with Answer / Decline on both sides, and on the client
//! also a banner over the video plus the menu's Audio rows (`callstate` renderer line).
//!
//! This module carries out the core's [`Step`]s: it sends the signals, rings, opens the mics —
//! the client's through the mic switch (`io_cmds::set_client_mic`), the host's through its own
//! capture (`mic_capture`) into the session's outbound queue — and publishes every change as
//! the `call-state` event (connections window, session view, toast window). Each side plays
//! the other's mic with `pcm_out::voice_sink`. One ticker task drives the core's timers.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use pulsar_core::call::{Action, Call, Phase, Step};
use pulsar_core::service::{CallSignal, DataMsg};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::state::AppState;

/// Which session a call belongs to: as the host (`sid`), or as the client (play id).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "side", content = "id", rename_all = "lowercase")]
pub(crate) enum CallKey {
	Host(u64),
	Client(u64),
}

impl CallKey {
	pub(crate) fn parse(side: &str, id: u64) -> Option<CallKey> {
		match side {
			"host" => Some(CallKey::Host(id)),
			"client" => Some(CallKey::Client(id)),
			_ => None,
		}
	}
}

#[derive(Default)]
struct Entry {
	call: Call,
	/// Wall-clock ms the phase began (the talk timer in the UIs).
	since_ms: u64,
	ringer: Option<crate::ringtone::Ringer>,
	/// Host side: this host's mic while the call is up.
	host_mic: Option<crate::mic_capture::Mic>,
	/// The toast card that shows this call.
	toast: Option<u64>,
}

static CALLS: Mutex<Option<HashMap<CallKey, Entry>>> = Mutex::new(None);
/// Client side: the host's display name per play (`DataMsg::PeerName`).
static NAMES: Mutex<Option<HashMap<CallKey, String>>> = Mutex::new(None);

fn with_calls<R>(f: impl FnOnce(&mut HashMap<CallKey, Entry>) -> R) -> R {
	let mut g = CALLS.lock().unwrap_or_else(|e| e.into_inner());
	f(g.get_or_insert_with(HashMap::new))
}

fn now_ms() -> u64 {
	SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// The event every UI follows.
#[derive(Clone, Serialize)]
pub(crate) struct CallState {
	#[serde(flatten)]
	key: CallKey,
	phase: Phase,
	/// The other side's display name ("" when unknown).
	peer: String,
	/// Wall-clock ms when this phase began.
	since: u64,
	/// The peer takes calls (said `Hello`): offer calling only then.
	supported: bool,
}

/// The client learned the host's name (`DataMsg::PeerName`).
pub(crate) fn set_peer_name(key: CallKey, name: &str) {
	let mut g = NAMES.lock().unwrap_or_else(|e| e.into_inner());
	g.get_or_insert_with(HashMap::new).insert(key, name.trim().to_string());
}

fn peer_name(app: &AppHandle, key: CallKey) -> String {
	match key {
		CallKey::Client(_) => NAMES
			.lock()
			.unwrap_or_else(|e| e.into_inner())
			.as_ref()
			.and_then(|m| m.get(&key).cloned())
			.unwrap_or_default(),
		CallKey::Host(sid) => {
			let state = app.state::<AppState>();
			let peer = state.host_out.lock().unwrap().get(&sid).map(|(p, _)| p.clone());
			let Some(peer) = peer else { return String::new() };
			let name = state.peer_meta.lock().unwrap().get(&peer).and_then(|m| m.0.clone());
			name.filter(|n| !n.trim().is_empty()).unwrap_or(peer)
		}
	}
}

/// The session's outbound side channel.
fn sender(app: &AppHandle, key: CallKey) -> Option<tokio::sync::mpsc::Sender<DataMsg>> {
	let state = app.state::<AppState>();
	match key {
		CallKey::Host(sid) => state.host_out.lock().unwrap().get(&sid).map(|(_, tx)| tx.clone()),
		CallKey::Client(id) => state.plays.lock().unwrap().get(&id).map(|p| p.data_tx.clone()),
	}
}

fn send(app: &AppHandle, key: CallKey, sig: CallSignal, times: usize) {
	if let Some(tx) = sender(app, key) {
		for _ in 0..times {
			let _ = tx.try_send(DataMsg::Call(sig));
		}
	}
}

/// Every session that takes calls or has one (a window that opens later seeds itself from this).
pub(crate) fn states(app: &AppHandle) -> Vec<CallState> {
	let list: Vec<(CallKey, Phase, u64, bool)> = with_calls(|m| {
		m.iter()
			.filter(|(_, e)| e.call.phase() != Phase::Idle || e.call.supported())
			.map(|(k, e)| (*k, e.call.phase(), e.since_ms, e.call.supported()))
			.collect()
	});
	list.into_iter()
		.map(|(key, phase, since, supported)| CallState { key, phase, peer: peer_name(app, key), since, supported })
		.collect()
}

/// The session came up: tell the peer we take calls (`Hello`, repeated on the core's schedule
/// until it answered or the session ended).
pub(crate) fn session_up(app: &AppHandle, key: CallKey) {
	with_calls(|m| {
		m.entry(key).or_default();
	});
	let app = app.clone();
	tauri::async_runtime::spawn(async move {
		let t0 = tokio::time::Instant::now();
		for at in pulsar_core::call::HELLO_AT {
			tokio::time::sleep_until(t0 + *at).await;
			let known = with_calls(|m| m.get(&key).map(|e| e.call.supported()));
			match known {
				Some(false) => send(&app, key, CallSignal::Hello, 1),
				_ => return, // answered, or the session is gone
			}
		}
	});
}

/// A UI action: `start`, `accept`, `decline` or `hangup`.
pub(crate) fn action(app: &AppHandle, key: CallKey, what: &str) {
	let Some(a) = Action::parse(what) else { return };
	// No session (any more): nothing to ring.
	if sender(app, key).is_none() {
		return;
	}
	// A peer that never said it takes calls would never answer.
	let supported = with_calls(|m| m.get(&key).is_some_and(|e| e.call.supported()));
	if a == Action::Start && !supported {
		tracing::info!(?key, "call: the peer does not take calls (older Pulsar)");
		return;
	}
	let step = with_calls(|m| m.entry(key).or_default().call.action(a, Instant::now()));
	tracing::info!(?key, what, ?step, "call: action");
	apply(app, key, step);
}

/// A signal from the peer.
pub(crate) fn on_signal(app: &AppHandle, key: CallKey, sig: CallSignal) {
	let (step, learned) = with_calls(|m| {
		let e = m.entry(key).or_default();
		let before = e.call.supported();
		(e.call.signal(sig, Instant::now()), !before)
	});
	tracing::info!(?key, ?sig, ?step, "call: signal");
	if learned && step.moved.is_none() {
		// The peer takes calls: the UIs enable calling (no phase change to publish).
		apply(app, key, step);
		let since = with_calls(|m| m.get(&key).map(|e| e.since_ms).unwrap_or(0));
		announce(app, key, Phase::Idle, since);
		return;
	}
	apply(app, key, step);
}

/// The `callstate` line for a (re)spawned renderer of play `id` (its menu's Audio rows).
pub(crate) fn seed_line(id: u64) -> String {
	let key = CallKey::Client(id);
	let (phase, since, supported) = with_calls(|m| {
		m.get(&key).map(|e| (e.call.phase(), e.since_ms, e.call.supported())).unwrap_or((Phase::Idle, 0, false))
	});
	let token = if phase == Phase::Idle && !supported { "na" } else { phase.token() };
	let name = NAMES
		.lock()
		.unwrap_or_else(|e| e.into_inner())
		.as_ref()
		.and_then(|m| m.get(&key).cloned())
		.unwrap_or_default()
		.replace(char::is_control, " ");
	format!("callstate {token} {since} {name}")
}

/// The `call-state` event and the renderer's menu row, nothing else.
fn announce(app: &AppHandle, key: CallKey, phase: Phase, since: u64) -> String {
	let peer = peer_name(app, key);
	let supported = with_calls(|m| m.get(&key).is_some_and(|e| e.call.supported()));
	let _ = app.emit("call-state", CallState { key, phase, peer: peer.clone(), since, supported });
	if let CallKey::Client(id) = key {
		let token = if phase == Phase::Idle && !supported { "na" } else { phase.token() };
		let name = peer.replace(char::is_control, " ");
		renderer_line(app, id, &format!("callstate {token} {since} {name}"));
	}
	peer
}

/// The session ended: drop its call without signalling.
pub(crate) fn session_gone(app: &AppHandle, key: CallKey) {
	let step = with_calls(|m| m.get_mut(&key).map(|e| e.call.gone(Instant::now())).unwrap_or_default());
	apply(app, key, step);
	with_calls(|m| m.remove(&key));
	if let Some(m) = NAMES.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
		m.remove(&key);
	}
}

/// One task for the whole app: every 500 ms, the core's timers (ring repeat, ring-outs).
fn ensure_ticker(app: &AppHandle) {
	static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
	if STARTED.set(()).is_err() {
		return;
	}
	let app = app.clone();
	tauri::async_runtime::spawn(async move {
		loop {
			tokio::time::sleep(std::time::Duration::from_millis(500)).await;
			let now = Instant::now();
			let steps: Vec<(CallKey, Step)> = with_calls(|m| {
				m.iter_mut()
					.filter(|(_, e)| e.call.phase() != Phase::Idle)
					.map(|(k, e)| (*k, e.call.tick(now)))
					.collect()
			});
			for (key, step) in steps {
				apply(&app, key, step);
			}
		}
	});
}

/// Carry out a step: the signal, then the side effects of a phase change.
fn apply(app: &AppHandle, key: CallKey, step: Step) {
	if let Some((sig, times)) = step.send {
		send(app, key, sig, times);
	}
	let Some((from, to)) = step.moved else { return };
	ensure_ticker(app);
	let since_ms = now_ms();
	let (host_mic, old_toast) = with_calls(|m| {
		let e = m.entry(key).or_default();
		e.since_ms = since_ms;
		// Ringing only while a call comes in.
		e.ringer = (to == Phase::Incoming).then(crate::ringtone::ring);
		let mic = if to != Phase::Active { e.host_mic.take() } else { None };
		(mic, e.toast.take())
	});
	drop(host_mic);
	if let Some(id) = old_toast {
		crate::toast::remove(app, id);
	}
	if to == Phase::Active && from != Phase::Active {
		mics(app, key, true);
	} else if from == Phase::Active && to != Phase::Active {
		mics(app, key, false);
	}
	if let Some(why) = step.ended {
		tracing::info!(?key, ?from, ?why, "call: ended");
	}
	publish(app, key, to, since_ms, step.ended.and_then(|w| w.notice()));
}

/// This side's mic for the call: the client's mic switch, or the host's own capture.
fn mics(app: &AppHandle, key: CallKey, on: bool) {
	match key {
		CallKey::Client(id) => {
			let state = app.state::<AppState>();
			if let Err(e) = crate::io_cmds::set_client_mic(app, &state, id, on) {
				tracing::warn!(id, "call: client mic: {e}");
			}
			renderer_line(app, id, &format!("audio mic={}", u8::from(on)));
		}
		CallKey::Host(_) => {
			let Some(tx) = sender(app, key) else { return };
			if !on {
				// The capture was dropped in `apply`; tell the client's player to close.
				let _ = tx.try_send(DataMsg::AudioEnd);
				return;
			}
			match crate::mic_capture::start(tx, crate::process::ffmpeg_bin(app)) {
				Ok(mic) => with_calls(|m| m.entry(key).or_default().host_mic = Some(mic)),
				Err(e) => tracing::warn!(?key, "call: host mic failed to start: {e}"),
			}
		}
	}
}

fn renderer_line(app: &AppHandle, id: u64, line: &str) {
	use std::io::Write;
	let state = app.state::<AppState>();
	let stdin = state.plays.lock().unwrap().get(&id).map(|p| p.render_stdin.clone());
	if let Some(stdin) = stdin {
		if let Some(si) = stdin.lock().unwrap().as_mut() {
			let _ = writeln!(si, "{line}");
			let _ = si.flush();
		}
	}
}

/// Tell every UI. `notice`: the toast kind of a call that ended on the peer's side.
fn publish(app: &AppHandle, key: CallKey, phase: Phase, since: u64, notice: Option<&'static str>) {
	let peer = announce(app, key, phase, since);
	let card = match phase {
		Phase::Incoming => Some("call-in"),
		Phase::Outgoing => Some("call-out"),
		Phase::Active => Some("call-on"),
		Phase::Idle => None,
	};
	if let Some(kind) = card {
		let id = crate::toast::call_card(app, kind, &peer, key, since);
		with_calls(|m| m.entry(key).or_default().toast = Some(id));
	}
	if let Some(kind) = notice {
		crate::toast::call_notice(app, kind, &peer);
	}
	// The client's renderer: the menu's Audio rows, a banner while it rings, a toast when it ends.
	if let CallKey::Client(id) = key {
		let name = peer.replace(char::is_control, " ");
		let banner = if phase == Phase::Incoming {
			let who = if name.trim().is_empty() { crate::i18n::t("call.host").to_string() } else { name.clone() };
			crate::i18n::t("call.banner").replace("{name}", &who)
		} else {
			String::new()
		};
		renderer_line(app, id, &format!("banner {banner}"));
		if let Some(kind) = notice {
			renderer_line(app, id, &format!("toast {}", crate::i18n::t(&format!("call.{kind}"))));
		}
	}
}

/// UI: start / answer / decline / hang up the call of a session.
#[tauri::command]
pub(crate) fn call_action(app: AppHandle, side: String, id: u64, action: String) -> Result<(), String> {
	let key = CallKey::parse(&side, id).ok_or("bad side")?;
	self::action(&app, key, &action);
	Ok(())
}

/// UI: every ongoing call (windows seed themselves from this, then follow `call-state`).
#[tauri::command]
pub(crate) fn call_states(app: AppHandle) -> Vec<CallState> {
	states(&app)
}

/// Settings: the ringtone choice (`ui.ringtone` / `ui.ringtonePath`).
#[tauri::command]
pub(crate) fn set_ringtone(app: AppHandle, tone: String, path: String) {
	crate::ringtone::set(&tone, &path, crate::process::ffmpeg_bin(&app));
}

/// Settings: pick a custom ringtone file (`None` = cancelled).
#[tauri::command]
pub(crate) async fn pick_ringtone() -> Option<String> {
	rfd::AsyncFileDialog::new()
		.add_filter("Audio", &["mp3", "wav", "ogg", "opus", "flac", "m4a", "aac", "wma"])
		.pick_file()
		.await
		.map(|f| f.path().to_string_lossy().into_owned())
}

/// Settings: play the picked ringtone for a few seconds (`stop` = silence it).
#[tauri::command]
pub(crate) fn ringtone_preview(tone: String, path: String, stop: bool) {
	if stop {
		crate::ringtone::stop_preview();
	} else {
		crate::ringtone::preview(&tone, &path);
	}
}
