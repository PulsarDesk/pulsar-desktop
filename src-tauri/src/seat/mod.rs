//! Pulsar Split seat host: `pulsar --split-seat <id> --data-dir <abs> --log <abs>`.
//!
//! One headless Pulsar host per Pulsar Split seat, started by the Pulsar Split service as
//! SYSTEM in the seat's session. Contract: pulsar-split `docs/agents/pulsar-host-backend.md`
//! (§1 launch, §2 isolation, §3 control pipe). This module holds the process-wide seat
//! context and the seams the host code checks (`active()`, `data_dir()`, `identity()`, …);
//! `proto.rs` is the pipe protocol, `win.rs` the Windows pipe + mutex.
//!
//! Smallest viable seam instead of the full `HostEnv` extraction (contract §6 item 1): the
//! host still runs inside a Tauri app (for its `AppHandle`), but one with no window, no
//! tray and no webview (`headless_host.rs`). See docs/agents/modes.md for what is stubbed.

pub(crate) mod proto;
#[cfg(windows)]
pub(crate) mod win;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use crate::state::AppState;
use proto::{Applied, SeatConfig, SeatHost, Secrets, WireErr};

/// The seat this process serves (set once, before Tauri starts).
pub(crate) struct Ctx {
	pub seat_id: String,
	pub data_dir: PathBuf,
	identity: Mutex<Option<pulsar_core::crypto::Identity>>,
	display_name: Mutex<String>,
	lan_discovery: AtomicBool,
	relay_key: Mutex<Option<String>>,
	events: Mutex<Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>,
	/// Open operator requests → the operator's answer once `auth.decide` gave one.
	open_auth: Mutex<HashMap<u64, Option<&'static str>>>,
	state: Mutex<&'static str>,
	relay: Mutex<(&'static str, Option<String>)>,
}

static CTX: OnceLock<Ctx> = OnceLock::new();

/// Enter seat mode (once, from `headless_host::run`).
pub(crate) fn init(seat_id: &str, data_dir: PathBuf) {
	let _ = CTX.set(Ctx {
		seat_id: seat_id.to_string(),
		data_dir,
		identity: Mutex::new(None),
		display_name: Mutex::new(String::new()),
		lan_discovery: AtomicBool::new(true),
		relay_key: Mutex::new(None),
		events: Mutex::new(None),
		open_auth: Mutex::new(HashMap::new()),
		state: Mutex::new("starting"),
		relay: Mutex::new(("unreachable", None)),
	});
}

fn ctx() -> Option<&'static Ctx> {
	CTX.get()
}

/// This process is a Pulsar Split seat host.
pub(crate) fn active() -> bool {
	CTX.get().is_some()
}

/// Seat mode: the per-seat data dir (`--data-dir`), instead of the per-user / shared dir.
pub(crate) fn data_dir() -> Option<PathBuf> {
	ctx().map(|c| c.data_dir.clone())
}

/// Seat mode: the identity from `secrets.load` (never `identity.key` on disk).
pub(crate) fn identity() -> Option<pulsar_core::crypto::Identity> {
	ctx().and_then(|c| c.identity.lock().unwrap().clone())
}

/// Seat mode: the announced name is the seat display name, verbatim.
pub(crate) fn announce_name() -> Option<String> {
	ctx().map(|c| c.display_name.lock().unwrap().clone())
}

/// Seat mode honours `config.apply.lan_discovery`; everywhere else the beacon is on.
pub(crate) fn lan_discovery() -> bool {
	ctx().is_none_or(|c| c.lan_discovery.load(Ordering::Relaxed))
}

/// Send an event to the service (dropped when no pipe is up).
pub(crate) fn emit(name: &str, body: Value) {
	if let Some(c) = ctx() {
		if let Some(tx) = c.events.lock().unwrap().as_ref() {
			let _ = tx.send(proto::event(name, body));
		}
	}
}

/// The relay issued an access key: seat mode hands it to the service (it stores it, DPAPI)
/// instead of writing `config.json`. Returns true when handled.
pub(crate) fn relay_key_issued(key: &str) -> bool {
	if !active() {
		return false;
	}
	emit("relay.key_issued", json!({ "key": key }));
	true
}

pub(crate) fn otp_changed() {
	emit("otp.changed", json!({}));
}

pub(crate) fn trust_changed() {
	emit("trust.changed", json!({}));
}

fn now_ms() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_millis() as u64)
		.unwrap_or(0)
}

/// Seat mode: an incoming session / pairing needs the operator. Sends `auth.request` instead
/// of opening the Allow/Deny popup. False when not in seat mode (the caller opens the popup).
pub(crate) fn auth_request(
	id: u64,
	pair: bool,
	peer: &str,
	key: Option<&[u8; 32]>,
	name: &str,
	pw_state: &str,
) -> bool {
	let Some(c) = ctx() else { return false };
	c.open_auth.lock().unwrap().insert(id, None);
	let key_fp = key
		.map(|k| crate::pairing::key_hex(k)[..16].to_string())
		.unwrap_or_default();
	emit(
		"auth.request",
		json!({
			"request_id": id,
			"kind": if pair { "pair" } else { "connect" },
			"label": peer,
			"device_name": name,
			"key_fp": key_fp,
			"password_state": match pw_state { "wrong" => "wrong", "wait" => "wait", _ => "none" },
			"received_at": now_ms().to_string(),
			"expires_in_s": if pair { 40 } else { 120 },
		}),
	);
	true
}

/// The operator's answer recorded by `auth.decide` (consumed by `auth_finish`).
fn decision(id: u64) -> Option<&'static str> {
	ctx().and_then(|c| c.open_auth.lock().unwrap().get(&id).copied().flatten())
}

/// The request finished; `auth.ended` once (no-op when it was not a seat request).
pub(crate) fn auth_ended(id: u64, result: &'static str) {
	let Some(c) = ctx() else { return };
	if c.open_auth.lock().unwrap().remove(&id).is_some() {
		emit("auth.ended", json!({ "request_id": id, "result": result }));
	}
}

/// `race_host_auth` result → `auth.ended`.
pub(crate) fn auth_finish(id: u64, approved: bool, by_operator: bool, view_only: bool) {
	let result = match (approved, by_operator, view_only) {
		(true, true, true) => "allowed_view_only",
		(true, true, false) => "allowed",
		(true, false, _) => "password",
		_ => decision(id).unwrap_or("timeout"),
	};
	auth_ended(id, result);
}

/// The request's task was dropped (session gone, host restarting) before it finished.
pub(crate) fn auth_abandon(id: u64) {
	auth_ended(id, "client_gone");
}

fn set_state(s: &'static str) {
	if let Some(c) = ctx() {
		*c.state.lock().unwrap() = s;
	}
}

fn set_relay(state: &'static str, id: Option<String>) {
	if let Some(c) = ctx() {
		let changed = {
			let mut g = c.relay.lock().unwrap();
			let changed = g.0 != state || g.1 != id;
			*g = (state, id.clone());
			changed
		};
		if changed {
			emit("relay.state", json!({ "state": state, "id": id }));
		}
	}
}

/// Remove every `PULSAR_*` developer switch from this process (contract §6 item 18): the
/// capture source, elevation bypass, test hooks… come from `config.apply` or not at all.
pub(crate) fn strip_dev_env() {
	let keys: Vec<_> = std::env::vars_os()
		.filter_map(|(k, _)| {
			k.to_str()
				.filter(|k| k.starts_with("PULSAR_"))
				.map(str::to_string)
		})
		.collect();
	for k in keys {
		std::env::remove_var(k);
	}
}

/// The real seat host behind the pipe.
pub(crate) struct AppSeatHost {
	pub app: AppHandle,
}

/// Config keys that need a rebind (`bring_online`) once the host is online.
const REBIND_KEYS: &[&str] = &[
	"display_name",
	"relay",
	"network_mode",
	"node_port",
	"port_mapping",
	"lan_discovery",
];

fn parse_network_mode(s: &str) -> Result<pulsar_core::config::NetworkMode, WireErr> {
	serde_json::from_value(Value::String(s.to_string())).map_err(|_| {
		WireErr::new(
			"bad_request",
			"network_mode must be auto, p2p-only or relay-only",
		)
	})
}

/// `config.apply` onto the app config (pure, tested). Returns the keys that changed.
pub(crate) fn merge_config(
	cfg: &mut pulsar_core::config::Config,
	c: &SeatConfig,
	relay_key: Option<&str>,
) -> Result<Vec<&'static str>, WireErr> {
	if c.access != "approve_or_password" {
		return Err(WireErr::new(
			"not_supported",
			"access: only approve_or_password is implemented",
		));
	}
	if !matches!(c.capture.as_str(), "auto" | "dxgi" | "wgc") {
		return Err(WireErr::new(
			"not_supported",
			"capture: only auto, dxgi and wgc are implemented",
		));
	}
	if c.display_name.trim().is_empty() {
		return Err(WireErr::new("bad_request", "display_name is empty"));
	}
	let mode = parse_network_mode(&c.network_mode)?;
	let relay = c
		.relay
		.clone()
		.filter(|r| !r.trim().is_empty())
		.unwrap_or_else(|| pulsar_core::config::Config::default().relay);
	let mut changed = Vec::new();
	if cfg.device_name != c.display_name {
		changed.push("display_name");
	}
	if cfg.relay != relay {
		changed.push("relay");
	}
	if cfg.network_mode != mode {
		changed.push("network_mode");
	}
	if cfg.node_port != c.node_port {
		changed.push("node_port");
	}
	if cfg.port_mapping != c.port_mapping {
		changed.push("port_mapping");
	}
	if cfg.transmit_audio != c.transmit_audio {
		changed.push("transmit_audio");
	}
	cfg.device_name = c.display_name.clone();
	cfg.relay = relay.clone();
	cfg.network_mode = mode;
	cfg.node_port = c.node_port;
	cfg.port_mapping = c.port_mapping;
	cfg.transmit_audio = c.transmit_audio;
	// Fixed in seat mode (contract §2): no in-app relay, never unattended.
	cfg.use_local_relay = false;
	cfg.unattended_access = false;
	if let Some(k) = relay_key {
		cfg.set_relay_key(&relay, Some(k.to_string()));
	}
	Ok(changed)
}

impl SeatHost for AppSeatHost {
	fn load_secrets(&self, s: Secrets, started: bool) -> Result<(), WireErr> {
		let c = ctx().ok_or_else(|| WireErr::new("internal", "not in seat mode"))?;
		if !started {
			*c.identity.lock().unwrap() =
				Some(pulsar_core::crypto::Identity::from_secret_bytes(s.identity));
		}
		*c.relay_key.lock().unwrap() = s.relay_key.clone();
		let st = self.app.state::<AppState>();
		let mut cfg = st.config.lock().unwrap();
		// The persistent connect password is read per connection: a change applies live.
		cfg.connect_password = s.connect_password.clone().unwrap_or_default();
		if let Some(k) = &s.relay_key {
			let relay = cfg.relay.clone();
			cfg.set_relay_key(&relay, Some(k.clone()));
		}
		Ok(())
	}

	fn apply_config(&self, c: SeatConfig, started: bool) -> Result<Applied, WireErr> {
		let cx = ctx().ok_or_else(|| WireErr::new("internal", "not in seat mode"))?;
		let key = cx.relay_key.lock().unwrap().clone();
		let st = self.app.state::<AppState>();
		let mut changed = {
			let mut cfg = st.config.lock().unwrap();
			merge_config(&mut cfg, &c, key.as_deref())?
		};
		*cx.display_name.lock().unwrap() = c.display_name.clone();
		if cx.lan_discovery.swap(c.lan_discovery, Ordering::Relaxed) != c.lan_discovery {
			changed.push("lan_discovery");
		}
		// The capture crate reads its forced source from the environment (dev env stripped).
		match c.capture.as_str() {
			"dxgi" | "wgc" => std::env::set_var("PULSAR_CAPTURE", &c.capture),
			_ => std::env::remove_var("PULSAR_CAPTURE"),
		}
		if c.audio_endpoint_id.is_some()
			|| c.render_adapter_luid.is_some()
			|| !c.codecs.is_empty()
			|| c.max_clients != 1
		{
			tracing::warn!("config.apply: audio_endpoint_id / render_adapter_luid / codecs / max_clients are not implemented — ignored");
		}
		let mut out = Applied {
			applied: vec!["access".into(), "capture".into()],
			restart_needed: vec![],
		};
		for k in [
			"display_name",
			"relay",
			"network_mode",
			"node_port",
			"port_mapping",
			"lan_discovery",
			"transmit_audio",
		] {
			if started && changed.contains(&k) && REBIND_KEYS.contains(&k) {
				out.restart_needed.push(k.into());
			} else {
				out.applied.push(k.into());
			}
		}
		Ok(out)
	}

	fn start(&self) {
		spawn_online(self.app.clone(), None, None);
	}

	fn status(&self) -> Value {
		let st = self.app.state::<AppState>();
		let clients = st.active.lock().unwrap().len();
		let (relay_state, relay_id) = ctx()
			.map(|c| c.relay.lock().unwrap().clone())
			.unwrap_or(("unreachable", None));
		let pending = ctx().is_some_and(|c| !c.open_auth.lock().unwrap().is_empty());
		json!({
			"state": ctx().map(|c| *c.state.lock().unwrap()).unwrap_or("starting"),
			"relay": { "state": relay_state, "id": relay_id },
			"node_port": st.node_port.load(Ordering::SeqCst),
			"clients_connected": clients,
			"stream_active": clients > 0,
			// Not tracked per seat yet (status.md): the capture/encoder picks live in the
			// per-session pipeline.
			"capture": { "source": Value::Null, "error": Value::Null },
			"encoder": "",
			"audio_state": if st.config.lock().unwrap().transmit_audio { "capturing" } else { "disabled" },
			"pending_auth": pending,
		})
	}

	fn auth_decide(&self, request_id: u64, allow: bool, view_only: bool) -> Result<(), WireErr> {
		let st = self.app.state::<AppState>();
		let tx = st.pending.lock().unwrap().remove(&request_id);
		let Some(tx) = tx else {
			return Err(WireErr::new(
				"no_pending_request",
				"no open request with that id",
			));
		};
		if let Some(c) = ctx() {
			if let Some(slot) = c.open_auth.lock().unwrap().get_mut(&request_id) {
				*slot = Some(if !allow {
					"denied"
				} else if view_only {
					"allowed_view_only"
				} else {
					"allowed"
				});
			}
		}
		let _ = tx.send(crate::auth::AuthDecision {
			allow,
			view_only: allow && view_only,
		});
		Ok(())
	}

	fn otp(&self, rotate: bool) -> Result<String, WireErr> {
		if rotate {
			crate::commands::rotate_session_password(&self.app);
		}
		let pw = crate::commands::current_otp(&self.app);
		if pw.is_empty() {
			return Err(WireErr::new(
				"not_ready",
				"no one-time password yet (host not online)",
			));
		}
		Ok(pw)
	}

	fn trust_list(&self) -> Value {
		Value::Array(
			crate::pairing::devices(&self.app)
				.into_iter()
				.map(|d| json!({ "key": d.key, "name": d.name, "label": d.id, "paired_at": d.paired_at, "last_seen": d.last_seen }))
				.collect(),
		)
	}

	fn trust_revoke(&self, key: &str) -> Result<(), WireErr> {
		if crate::pairing::revoke(&self.app, key) {
			Ok(())
		} else {
			Err(WireErr::new("not_found", "no paired device with that key"))
		}
	}

	fn trust_revoke_all(&self) {
		for d in crate::pairing::devices(&self.app) {
			crate::pairing::revoke(&self.app, &d.key);
		}
	}

	fn relay_auth(&self, password: String, totp: Option<String>) -> Result<(), WireErr> {
		spawn_online(self.app.clone(), Some(password), totp);
		Ok(())
	}
}

/// What a `bring_online` result means for the seat (pure, tested).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
	Online(String),
	AuthRequired {
		password: bool,
		totp: bool,
	},
	AuthFailed,
	VersionMismatch,
	/// Relay unreachable (LAN serving stays up): retry later.
	Retry,
	/// Anything else: retry later too.
	Failed,
}

pub(crate) fn classify(r: &Result<String, String>) -> Outcome {
	match r {
		Ok(id) => Outcome::Online(id.clone()),
		Err(e) if e.starts_with("RELAY_AUTH_REQUIRED") => {
			let mut it = e.split(':').skip(1);
			Outcome::AuthRequired {
				password: it.next() == Some("true"),
				totp: it.next() == Some("true"),
			}
		}
		Err(e) if e == "RELAY_AUTH_FAILED" => Outcome::AuthFailed,
		Err(e) if e.contains("incompatible protocol version") => Outcome::VersionMismatch,
		Err(e) if e.contains("unreachable") || e.contains("timed out") || e.contains("resolve") => {
			Outcome::Retry
		}
		Err(_) => Outcome::Failed,
	}
}

/// Bring the seat host online and keep trying (3 s → 12 s) while the relay is unreachable;
/// stop on an outcome only the operator can fix. A retry rebinds the node, so it waits while
/// a client session is live (LAN serving keeps working meanwhile).
fn spawn_online(app: AppHandle, password: Option<String>, totp: Option<String>) {
	tauri::async_runtime::spawn(async move {
		let mut creds = Some((password, totp));
		let mut delay = Duration::from_secs(3);
		loop {
			set_state("registering");
			let (pw, code) = creds.take().unwrap_or((None, None));
			let res = {
				let st = app.state::<AppState>();
				let _g = st.go_online_lock.lock().await;
				crate::host::bring_online(app.clone(), app.state::<AppState>(), pw, code).await
			};
			match classify(&res) {
				Outcome::Online(id) => {
					tracing::info!(%id, "seat host online");
					set_state("online");
					set_relay("ok", Some(id));
					return;
				}
				Outcome::AuthRequired { password, totp } => {
					set_state("offline");
					set_relay("auth_required", None);
					emit(
						"relay.auth_required",
						json!({ "password": password, "totp": totp }),
					);
					return;
				}
				Outcome::AuthFailed => {
					set_state("offline");
					set_relay("auth_required", None);
					emit(
						"relay.auth_required",
						json!({ "password": true, "totp": false }),
					);
					return;
				}
				Outcome::VersionMismatch => {
					set_state("offline");
					set_relay("version_mismatch", None);
					return;
				}
				Outcome::Retry | Outcome::Failed => {
					set_state("offline");
					set_relay("unreachable", None);
					if let Err(e) = &res {
						tracing::warn!(retry_in = ?delay, "seat host offline: {e}");
					}
					tokio::time::sleep(delay).await;
					delay = (delay * 2).min(Duration::from_secs(12));
					while !app.state::<AppState>().active.lock().unwrap().is_empty() {
						tokio::time::sleep(Duration::from_secs(2)).await;
					}
				}
			}
		}
	});
}

/// Tear the host down (serve loop, sessions, node) so the crash-restore guards run.
pub(crate) async fn teardown(app: &AppHandle) {
	set_state("stopping");
	let st = app.state::<AppState>();
	let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
	if let Some(h) = st.serve_task.lock().unwrap().take() {
		h.abort();
		handles.push(h);
	}
	for h in st.session_tasks.lock().unwrap().drain(..) {
		h.abort();
		handles.push(h);
	}
	for h in handles {
		let _ = tokio::time::timeout(Duration::from_secs(2), h).await;
	}
	let _ = st.node.lock().unwrap().take();
}

/// Run the control pipe for the life of the process (spawned from the Tauri setup).
#[cfg(windows)]
pub(crate) fn spawn_pipe(app: AppHandle) {
	tauri::async_runtime::spawn(async move {
		let Some(c) = ctx() else { return };
		let pipe = match win::connect(&c.seat_id).await {
			Ok(p) => p,
			Err(e) => {
				tracing::error!("seat pipe: {e}");
				std::process::exit(crate::headless_host::EXIT_PIPE);
			}
		};
		let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
		*c.events.lock().unwrap() = Some(tx);
		let (r, w) = tokio::io::split(pipe);
		let host = AppSeatHost { app: app.clone() };
		match proto::serve(r, w, &c.seat_id, &host, rx).await {
			proto::Action::Shutdown { grace_ms } => {
				tracing::info!(grace_ms, "seat host: server.shutdown");
				// Hard deadline: never outlive the service's grace window.
				std::thread::spawn(move || {
					std::thread::sleep(Duration::from_millis(grace_ms.max(500)));
					std::process::exit(0);
				});
				let _ = tokio::time::timeout(Duration::from_millis(grace_ms), teardown(&app)).await;
				app.exit(0);
			}
			_ => {
				// §3.1 "Loss": never run uncontrolled.
				tracing::error!("seat pipe lost — exiting");
				std::process::exit(crate::headless_host::EXIT_PIPE_LOST);
			}
		}
	});
}

#[cfg(test)]
mod tests {
	use super::*;

	fn sc(v: Value) -> SeatConfig {
		serde_json::from_value(v).unwrap()
	}

	#[test]
	fn merge_maps_fields_and_forces_seat_rules() {
		let mut cfg = pulsar_core::config::Config {
			use_local_relay: true,
			unattended_access: true,
			..Default::default()
		};
		let changed = merge_config(
			&mut cfg,
			&sc(json!({ "display_name": "Salon PC", "relay": "r.example:21116", "network_mode": "relay-only", "node_port": 48100, "port_mapping": false })),
			Some("k"),
		)
		.unwrap();
		assert_eq!(cfg.device_name, "Salon PC");
		assert_eq!(cfg.relay, "r.example:21116");
		assert_eq!(
			cfg.network_mode,
			pulsar_core::config::NetworkMode::RelayOnly
		);
		assert_eq!(cfg.node_port, 48100);
		assert!(!cfg.use_local_relay && !cfg.unattended_access && !cfg.port_mapping);
		assert_eq!(cfg.relay_key_for("r.example:21116"), Some("k"));
		assert!(changed.contains(&"display_name") && changed.contains(&"node_port"));
		// Same again: nothing changed.
		let again = merge_config(
			&mut cfg,
			&sc(json!({ "display_name": "Salon PC", "relay": "r.example:21116", "network_mode": "relay-only", "node_port": 48100, "port_mapping": false })),
			None,
		)
		.unwrap();
		assert!(again.is_empty(), "{again:?}");
	}

	#[test]
	fn merge_default_relay_and_rejects() {
		let mut cfg = pulsar_core::config::Config::default();
		merge_config(&mut cfg, &sc(json!({ "display_name": "A" })), None).unwrap();
		assert_eq!(cfg.relay, pulsar_core::config::Config::default().relay);
		let e = merge_config(
			&mut cfg,
			&sc(json!({ "display_name": "A", "network_mode": "x" })),
			None,
		)
		.unwrap_err();
		assert_eq!(e.code, "bad_request");
		let e = merge_config(
			&mut cfg,
			&sc(json!({ "display_name": "A", "access": "approve" })),
			None,
		)
		.unwrap_err();
		assert_eq!(e.code, "not_supported");
		let e = merge_config(
			&mut cfg,
			&sc(json!({ "display_name": "A", "capture": "split_idd" })),
			None,
		)
		.unwrap_err();
		assert_eq!(e.code, "not_supported");
		let e = merge_config(&mut cfg, &sc(json!({ "display_name": " " })), None).unwrap_err();
		assert_eq!(e.code, "bad_request");
	}

	#[test]
	fn classify_outcomes() {
		assert_eq!(
			classify(&Ok("123 456 789".into())),
			Outcome::Online("123 456 789".into())
		);
		assert_eq!(
			classify(&Err("RELAY_AUTH_REQUIRED:true:false".into())),
			Outcome::AuthRequired {
				password: true,
				totp: false
			}
		);
		assert_eq!(
			classify(&Err("RELAY_AUTH_FAILED".into())),
			Outcome::AuthFailed
		);
		assert_eq!(
			classify(&Err("relay: incompatible protocol version (5 vs 4)".into())),
			Outcome::VersionMismatch
		);
		assert_eq!(
			classify(&Err("relay unreachable: timed out".into())),
			Outcome::Retry
		);
		assert_eq!(classify(&Err("boom".into())), Outcome::Failed);
	}
}
