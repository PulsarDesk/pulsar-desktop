//! Pulsar Split seat control pipe, protocol 1 (pulsar-split
//! `docs/agents/pulsar-host-backend.md` §3). Transport-free: framing, the envelope and the
//! request state machine, driven through [`SeatHost`] so the tests can use a fake host and
//! an in-memory pipe.
//!
//! Framing: u32 little-endian length + UTF-8 JSON, at most 1 MiB, both ways. Envelope:
//! request `{id, type, body}` (service → host only), response `{id, ok, body | error{code,
//! detail}}`, event `{event, body}` (host → service).
//!
//! Secrets (`secrets.load`, `auth.otp.*`, `relay.auth`, `relay.key_issued`) are never logged;
//! raw frame buffers are wiped after use ([`Wiped`]). Parse errors carry only the serde error
//! category and position, never the offending text.

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub(crate) const PROTOCOL: u32 = 1;
pub(crate) const MAX_FRAME: usize = 1024 * 1024;

/// What `hello` advertises: only what this build really does (see `handle`).
pub(crate) const CAPABILITIES: &[&str] = &["approve", "connect_password", "otp", "trust_store"];

/// Requests this build answers with `not_supported` (contract §3.3; the seat works without
/// them, see status.md "Split seat host").
pub(crate) const NOT_SUPPORTED: &[&str] = &[
	"clients.list",
	"clients.kick",
	"clients.set_view_only",
	"apps.set",
	"app.launched",
	"display.mode.result",
	"pad.attach",
	"pad.denied",
	"pad.detach",
	"log.level",
];

/// A byte buffer that is overwritten with zeros when dropped (frames can carry secrets).
pub(crate) struct Wiped(pub Vec<u8>);

impl Drop for Wiped {
	fn drop(&mut self) {
		for b in self.0.iter_mut() {
			// Volatile so the wipe of a buffer about to be freed is not optimised away.
			unsafe { std::ptr::write_volatile(b, 0) };
		}
		std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
	}
}

impl std::ops::Deref for Wiped {
	type Target = [u8];
	fn deref(&self) -> &[u8] {
		&self.0
	}
}

fn too_large() -> std::io::Error {
	std::io::Error::new(std::io::ErrorKind::InvalidData, "frame larger than 1 MiB")
}

/// Length-prefixed frame bytes (one buffer, so a pipe never sees a split header).
pub(crate) fn encode_frame(payload: &[u8]) -> std::io::Result<Wiped> {
	if payload.len() > MAX_FRAME {
		return Err(too_large());
	}
	let mut buf = Vec::with_capacity(4 + payload.len());
	buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
	buf.extend_from_slice(payload);
	Ok(Wiped(buf))
}

pub(crate) async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Wiped> {
	let mut len = [0u8; 4];
	r.read_exact(&mut len).await?;
	let n = u32::from_le_bytes(len) as usize;
	if n > MAX_FRAME {
		return Err(too_large());
	}
	let mut buf = Wiped(vec![0u8; n]);
	r.read_exact(&mut buf.0).await?;
	Ok(buf)
}

pub(crate) async fn write_frame<W: AsyncWrite + Unpin>(
	w: &mut W,
	payload: &[u8],
) -> std::io::Result<()> {
	let frame = encode_frame(payload)?;
	w.write_all(&frame).await?;
	w.flush().await
}

/// An error answer: `code` from the contract's list (plus `not_supported`), `detail` free
/// text that never contains a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WireErr {
	pub code: &'static str,
	pub detail: String,
}

impl WireErr {
	pub(crate) fn new(code: &'static str, detail: impl Into<String>) -> Self {
		Self {
			code,
			detail: detail.into(),
		}
	}
}

fn json_err(e: &serde_json::Error) -> String {
	format!(
		"{:?} error at line {} column {}",
		e.classify(),
		e.line(),
		e.column()
	)
}

/// `secrets.load`, decoded. Held in memory only.
pub(crate) struct Secrets {
	pub identity: [u8; 32],
	pub connect_password: Option<String>,
	pub relay_key: Option<String>,
}

impl Drop for Secrets {
	fn drop(&mut self) {
		self.identity
			.iter_mut()
			.for_each(|b| unsafe { std::ptr::write_volatile(b, 0) });
	}
}

/// `config.apply` body (contract §3.3). Unknown keys are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct SeatConfig {
	pub display_name: String,
	#[serde(default)]
	pub relay: Option<String>,
	#[serde(default = "auto")]
	pub network_mode: String,
	#[serde(default)]
	pub node_port: u16,
	#[serde(default)]
	pub port_mapping: bool,
	#[serde(default = "yes")]
	pub lan_discovery: bool,
	#[serde(default = "approve_or_password")]
	pub access: String,
	#[serde(default = "yes")]
	pub transmit_audio: bool,
	#[serde(default)]
	pub audio_endpoint_id: Option<String>,
	#[serde(default)]
	pub render_adapter_luid: Option<u64>,
	#[serde(default = "auto")]
	pub capture: String,
	#[serde(default)]
	pub codecs: Vec<String>,
	#[serde(default = "one")]
	pub max_clients: u8,
}

fn auto() -> String {
	"auto".into()
}
fn approve_or_password() -> String {
	"approve_or_password".into()
}
fn yes() -> bool {
	true
}
fn one() -> u8 {
	1
}

/// `config.apply` answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Applied {
	pub applied: Vec<String>,
	pub restart_needed: Vec<String>,
}

/// The seat host behind the pipe. The real one drives the Pulsar host (`seat/mod.rs`);
/// tests use a fake.
pub(crate) trait SeatHost: Send + Sync {
	/// `started`: the host is already online (a live password change).
	fn load_secrets(&self, s: Secrets, started: bool) -> Result<(), WireErr>;
	fn apply_config(&self, c: SeatConfig, started: bool) -> Result<Applied, WireErr>;
	/// Both `secrets.load` and `config.apply` succeeded once: bind, beacon, register.
	fn start(&self);
	fn status(&self) -> Value;
	fn auth_decide(&self, request_id: u64, allow: bool, view_only: bool) -> Result<(), WireErr>;
	fn otp(&self, rotate: bool) -> Result<String, WireErr>;
	fn trust_list(&self) -> Value;
	fn trust_revoke(&self, key: &str) -> Result<(), WireErr>;
	fn trust_revoke_all(&self);
	fn relay_auth(&self, password: String, totp: Option<String>) -> Result<(), WireErr>;
}

/// What the transport does after a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
	Continue,
	/// `server.shutdown`: stop within `grace_ms`, exit 0.
	Shutdown {
		grace_ms: u64,
	},
	/// Handshake failed: close the pipe and exit.
	Close,
}

/// Per-connection protocol state.
pub(crate) struct Session {
	seat_id: String,
	hello: bool,
	secrets: bool,
	config: bool,
	started: bool,
}

#[derive(Deserialize)]
struct Head {
	#[serde(default)]
	id: Option<u64>,
	#[serde(rename = "type", default)]
	ty: Option<String>,
}

/// `{body: T}` view of a request frame, parsed straight from the bytes (secrets never pass
/// through a `serde_json::Value`).
#[derive(Deserialize)]
struct Body<T> {
	body: T,
}

#[derive(Deserialize)]
struct HelloBody {
	#[serde(default)]
	protocol: u32,
	#[serde(default)]
	seat_id: String,
}

#[derive(Deserialize)]
struct SecretsBody {
	identity: String,
	#[serde(default)]
	connect_password: Option<String>,
	#[serde(default)]
	relay_key: Option<String>,
}

#[derive(Deserialize)]
struct DecideBody {
	request_id: u64,
	allow: bool,
	#[serde(default)]
	view_only: bool,
}

#[derive(Deserialize)]
struct KeyBody {
	key: String,
}

#[derive(Deserialize)]
struct RelayAuthBody {
	password: String,
	#[serde(default)]
	totp: Option<String>,
}

#[derive(Deserialize, Default)]
struct ShutdownBody {
	#[serde(default = "grace")]
	grace_ms: u64,
}

fn grace() -> u64 {
	5000
}

fn parse<T: for<'de> Deserialize<'de>>(frame: &[u8]) -> Result<T, WireErr> {
	serde_json::from_slice::<Body<T>>(frame)
		.map(|b| b.body)
		.map_err(|e| WireErr::new("bad_request", json_err(&e)))
}

fn ok(id: u64, body: Value) -> Vec<u8> {
	json!({ "id": id, "ok": true, "body": body })
		.to_string()
		.into_bytes()
}

fn err(id: Option<u64>, e: &WireErr) -> Vec<u8> {
	json!({ "id": id, "ok": false, "error": { "code": e.code, "detail": e.detail } })
		.to_string()
		.into_bytes()
}

/// An event frame payload.
pub(crate) fn event(name: &str, body: Value) -> Vec<u8> {
	json!({ "event": name, "body": body })
		.to_string()
		.into_bytes()
}

/// Decode the base64 32-byte identity of `secrets.load`.
pub(crate) fn decode_identity(b64: &str) -> Result<[u8; 32], WireErr> {
	use base64::Engine as _;
	let raw = Wiped(
		base64::engine::general_purpose::STANDARD
			.decode(b64.trim())
			.map_err(|_| WireErr::new("bad_request", "identity is not base64"))?,
	);
	<[u8; 32]>::try_from(&raw[..])
		.map_err(|_| WireErr::new("bad_request", "identity is not 32 bytes"))
}

impl Session {
	pub(crate) fn new(seat_id: &str) -> Self {
		Self {
			seat_id: seat_id.to_string(),
			hello: false,
			secrets: false,
			config: false,
			started: false,
		}
	}

	#[cfg(test)]
	pub(crate) fn started(&self) -> bool {
		self.started
	}

	/// Handle one request frame. Returns the response payload (None for a frame that is
	/// not a request: the service sends none of those) and what to do next.
	pub(crate) fn handle(
		&mut self,
		frame: &[u8],
		host: &dyn SeatHost,
	) -> (Option<Vec<u8>>, Action) {
		let head: Head = match serde_json::from_slice(frame) {
			Ok(h) => h,
			Err(e) => {
				return (
					Some(err(None, &WireErr::new("bad_request", json_err(&e)))),
					Action::Continue,
				)
			}
		};
		let (Some(id), Some(ty)) = (head.id, head.ty) else {
			return (
				Some(err(
					head.id,
					&WireErr::new("bad_request", "request without id or type"),
				)),
				Action::Continue,
			);
		};
		let (res, action) = self.dispatch(&ty, frame, host);
		let out = match res {
			Ok(body) => ok(id, body),
			Err(e) => err(Some(id), &e),
		};
		(Some(out), action)
	}

	fn dispatch(
		&mut self,
		ty: &str,
		frame: &[u8],
		host: &dyn SeatHost,
	) -> (Result<Value, WireErr>, Action) {
		if ty == "hello" {
			return self.hello(frame);
		}
		if !self.hello {
			return (
				Err(WireErr::new("not_ready", "hello must be the first request")),
				Action::Continue,
			);
		}
		let ready = self.secrets && self.config;
		let not_ready = || {
			Err(WireErr::new(
				"not_ready",
				"secrets.load and config.apply first",
			))
		};
		let r = match ty {
			"secrets.load" => match parse::<SecretsBody>(frame) {
				Ok(b) => {
					let r = decode_identity(&b.identity).and_then(|identity| {
						host.load_secrets(
							Secrets {
								identity,
								connect_password: b.connect_password,
								relay_key: b.relay_key,
							},
							self.started,
						)
					});
					if r.is_ok() {
						self.secrets = true;
						self.maybe_start(host);
					}
					r.map(|_| json!({}))
				}
				Err(e) => Err(e),
			},
			"config.apply" => match parse::<SeatConfig>(frame) {
				Ok(c) => match host.apply_config(c, self.started) {
					Ok(a) => {
						self.config = true;
						self.maybe_start(host);
						Ok(json!({ "applied": a.applied, "restart_needed": a.restart_needed }))
					}
					Err(e) => Err(e),
				},
				Err(e) => Err(e),
			},
			"status" => Ok(host.status()),
			"server.shutdown" => {
				let grace_ms = serde_json::from_slice::<Body<ShutdownBody>>(frame)
					.map(|b| b.body.grace_ms)
					.unwrap_or_else(|_| grace());
				return (Ok(json!({})), Action::Shutdown { grace_ms });
			}
			_ if !ready => not_ready(),
			"auth.decide" => parse::<DecideBody>(frame)
				.and_then(|b| host.auth_decide(b.request_id, b.allow, b.allow && b.view_only))
				.map(|_| json!({})),
			"auth.otp.get" => host.otp(false).map(|otp| json!({ "otp": otp })),
			"auth.otp.rotate" => host.otp(true).map(|otp| json!({ "otp": otp })),
			"trust.list" => Ok(json!({ "devices": host.trust_list() })),
			"trust.revoke" => parse::<KeyBody>(frame)
				.and_then(|b| host.trust_revoke(&b.key))
				.map(|_| json!({})),
			"trust.revoke_all" => {
				host.trust_revoke_all();
				Ok(json!({}))
			}
			"relay.auth" => parse::<RelayAuthBody>(frame)
				.and_then(|b| host.relay_auth(b.password, b.totp))
				.map(|_| json!({})),
			t if NOT_SUPPORTED.contains(&t) => Err(WireErr::new(
				"not_supported",
				format!("{t} is not implemented by this pulsar-host build"),
			)),
			_ => Err(WireErr::new("bad_request", "unknown request type")),
		};
		(r, Action::Continue)
	}

	fn hello(&mut self, frame: &[u8]) -> (Result<Value, WireErr>, Action) {
		let b = match parse::<HelloBody>(frame) {
			Ok(b) => b,
			Err(e) => return (Err(e), Action::Close),
		};
		if b.protocol != PROTOCOL || b.seat_id != self.seat_id {
			return (
				Err(WireErr::new(
					"unsupported_protocol",
					format!("this host speaks protocol {PROTOCOL} for its own seat"),
				)),
				Action::Close,
			);
		}
		self.hello = true;
		(
			Ok(json!({
				"protocol": PROTOCOL,
				"host_version": env!("CARGO_PKG_VERSION"),
				"core_version": "",
				"seat_id": self.seat_id,
				"capabilities": CAPABILITIES,
			})),
			Action::Continue,
		)
	}

	fn maybe_start(&mut self, host: &dyn SeatHost) {
		if self.secrets && self.config && !self.started {
			self.started = true;
			host.start();
		}
	}
}

/// Drive one pipe: answer requests in order, forward host events from `events`, return
/// the final action (`Close` on EOF / a broken pipe: the caller exits at once, §3.1 "Loss").
pub(crate) async fn serve<R, W>(
	mut r: R,
	mut w: W,
	seat_id: &str,
	host: &dyn SeatHost,
	mut events: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
) -> Action
where
	R: AsyncRead + Unpin,
	W: AsyncWrite + Unpin,
{
	let mut session = Session::new(seat_id);
	loop {
		tokio::select! {
			frame = read_frame(&mut r) => {
				let frame = match frame {
					Ok(f) => f,
					Err(e) => {
						tracing::warn!("seat pipe: read failed ({e}) — closing");
						return Action::Close;
					}
				};
				let (resp, action) = session.handle(&frame, host);
				if let Some(resp) = resp {
					let resp = Wiped(resp);
					if let Err(e) = write_frame(&mut w, &resp).await {
						tracing::warn!("seat pipe: write failed ({e}) — closing");
						return Action::Close;
					}
				}
				if action != Action::Continue {
					return action;
				}
			}
			ev = events.recv() => {
				// No sender left: keep answering requests until the pipe closes.
				let Some(ev) = ev else { continue };
				let ev = Wiped(ev);
				if let Err(e) = write_frame(&mut w, &ev).await {
					tracing::warn!("seat pipe: event write failed ({e}) — closing");
					return Action::Close;
				}
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::sync::Mutex;

	const SEAT: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";

	#[derive(Default)]
	struct Fake {
		log: Mutex<Vec<String>>,
	}

	impl Fake {
		fn calls(&self) -> Vec<String> {
			self.log.lock().unwrap().clone()
		}
		fn note(&self, s: impl Into<String>) {
			self.log.lock().unwrap().push(s.into());
		}
	}

	impl SeatHost for Fake {
		fn load_secrets(&self, s: Secrets, started: bool) -> Result<(), WireErr> {
			self.note(format!(
				"secrets {} pw={} started={started}",
				s.identity[0],
				s.connect_password.is_some()
			));
			Ok(())
		}
		fn apply_config(&self, c: SeatConfig, _started: bool) -> Result<Applied, WireErr> {
			self.note(format!("config {}", c.display_name));
			Ok(Applied {
				applied: vec!["display_name".into()],
				restart_needed: vec![],
			})
		}
		fn start(&self) {
			self.note("start");
		}
		fn status(&self) -> Value {
			json!({ "state": "starting" })
		}
		fn auth_decide(
			&self,
			request_id: u64,
			allow: bool,
			view_only: bool,
		) -> Result<(), WireErr> {
			if request_id != 7 {
				return Err(WireErr::new("no_pending_request", "none"));
			}
			self.note(format!("decide {allow} {view_only}"));
			Ok(())
		}
		fn otp(&self, rotate: bool) -> Result<String, WireErr> {
			Ok(if rotate { "new".into() } else { "old".into() })
		}
		fn trust_list(&self) -> Value {
			json!([])
		}
		fn trust_revoke(&self, key: &str) -> Result<(), WireErr> {
			self.note(format!("revoke {key}"));
			Ok(())
		}
		fn trust_revoke_all(&self) {
			self.note("revoke_all");
		}
		fn relay_auth(&self, _password: String, totp: Option<String>) -> Result<(), WireErr> {
			self.note(format!("relay_auth totp={}", totp.is_some()));
			Ok(())
		}
	}

	fn req(id: u64, ty: &str, body: Value) -> Vec<u8> {
		json!({ "id": id, "type": ty, "body": body })
			.to_string()
			.into_bytes()
	}

	fn call(s: &mut Session, h: &Fake, id: u64, ty: &str, body: Value) -> (Value, Action) {
		let (out, a) = s.handle(&req(id, ty, body), h);
		(serde_json::from_slice(&out.unwrap()).unwrap(), a)
	}

	fn hello_body() -> Value {
		json!({ "protocol": 1, "service_version": "0.1.0", "seat_id": SEAT })
	}

	fn identity_b64() -> String {
		use base64::Engine as _;
		base64::engine::general_purpose::STANDARD.encode([9u8; 32])
	}

	#[test]
	fn hello_first_and_checked() {
		let h = Fake::default();
		let mut s = Session::new(SEAT);
		let (r, _) = call(&mut s, &h, 1, "status", json!({}));
		assert_eq!(r["error"]["code"], "not_ready");
		let (r, a) = call(
			&mut s,
			&h,
			2,
			"hello",
			json!({ "protocol": 2, "seat_id": SEAT }),
		);
		assert_eq!(r["error"]["code"], "unsupported_protocol");
		assert_eq!(a, Action::Close);
		let mut s = Session::new(SEAT);
		let (r, a) = call(
			&mut s,
			&h,
			3,
			"hello",
			json!({ "protocol": 1, "seat_id": "other" }),
		);
		assert_eq!(r["error"]["code"], "unsupported_protocol");
		assert_eq!(a, Action::Close);
		let (r, a) = call(&mut s, &h, 4, "hello", hello_body());
		assert_eq!(a, Action::Continue);
		assert_eq!(r["id"], 4);
		assert_eq!(r["ok"], true);
		assert_eq!(r["body"]["protocol"], 1);
		assert_eq!(r["body"]["seat_id"], SEAT);
		assert!(r["body"]["capabilities"]
			.as_array()
			.unwrap()
			.iter()
			.any(|c| c == "approve"));
	}

	#[test]
	fn start_after_secrets_and_config_only() {
		let h = Fake::default();
		let mut s = Session::new(SEAT);
		call(&mut s, &h, 1, "hello", hello_body());
		let (r, _) = call(&mut s, &h, 2, "auth.otp.get", json!({}));
		assert_eq!(r["error"]["code"], "not_ready");
		let (r, _) = call(
			&mut s,
			&h,
			3,
			"secrets.load",
			json!({ "identity": identity_b64(), "connect_password": "pw" }),
		);
		assert_eq!(r["ok"], true, "{r}");
		assert!(!s.started());
		let (r, _) = call(
			&mut s,
			&h,
			4,
			"config.apply",
			json!({ "display_name": "Salon PC", "node_port": 48100 }),
		);
		assert_eq!(r["ok"], true);
		assert_eq!(r["body"]["applied"][0], "display_name");
		assert!(s.started());
		// A second secrets.load (password changed live) does not start again.
		call(
			&mut s,
			&h,
			5,
			"secrets.load",
			json!({ "identity": identity_b64() }),
		);
		assert_eq!(
			h.calls(),
			vec![
				"secrets 9 pw=true started=false",
				"config Salon PC",
				"start",
				"secrets 9 pw=false started=true"
			]
		);
		let (r, _) = call(&mut s, &h, 6, "auth.otp.rotate", json!({}));
		assert_eq!(r["body"]["otp"], "new");
	}

	#[test]
	fn bad_identity_is_rejected_without_echo() {
		let h = Fake::default();
		let mut s = Session::new(SEAT);
		call(&mut s, &h, 1, "hello", hello_body());
		let (r, _) = call(
			&mut s,
			&h,
			2,
			"secrets.load",
			json!({ "identity": "c2VjcmV0LXZhbHVl" }),
		);
		assert_eq!(r["error"]["code"], "bad_request");
		assert!(!r.to_string().contains("c2VjcmV0LXZhbHVl"));
		let (r, _) = call(
			&mut s,
			&h,
			3,
			"secrets.load",
			json!({ "identity": "not base64 !!" }),
		);
		assert_eq!(r["error"]["code"], "bad_request");
		assert!(!r.to_string().contains("not base64 !!"));
		assert!(h.calls().is_empty());
	}

	#[test]
	fn requests_after_start() {
		let h = Fake::default();
		let mut s = Session::new(SEAT);
		call(&mut s, &h, 1, "hello", hello_body());
		call(
			&mut s,
			&h,
			2,
			"secrets.load",
			json!({ "identity": identity_b64() }),
		);
		call(
			&mut s,
			&h,
			3,
			"config.apply",
			json!({ "display_name": "A" }),
		);
		let (r, _) = call(
			&mut s,
			&h,
			4,
			"auth.decide",
			json!({ "request_id": 7, "allow": true, "view_only": true }),
		);
		assert_eq!(r["ok"], true);
		let (r, _) = call(
			&mut s,
			&h,
			5,
			"auth.decide",
			json!({ "request_id": 8, "allow": false }),
		);
		assert_eq!(r["error"]["code"], "no_pending_request");
		let (r, _) = call(&mut s, &h, 6, "trust.list", json!({}));
		assert_eq!(r["body"]["devices"], json!([]));
		call(&mut s, &h, 7, "trust.revoke", json!({ "key": "ab" }));
		call(&mut s, &h, 8, "trust.revoke_all", json!({}));
		call(
			&mut s,
			&h,
			9,
			"relay.auth",
			json!({ "password": "x", "totp": "123456" }),
		);
		for (i, ty) in NOT_SUPPORTED.iter().enumerate() {
			let (r, _) = call(&mut s, &h, 100 + i as u64, ty, json!({}));
			assert_eq!(r["error"]["code"], "not_supported", "{ty}");
		}
		let (r, _) = call(&mut s, &h, 200, "bogus", json!({}));
		assert_eq!(r["error"]["code"], "bad_request");
		let (r, a) = call(
			&mut s,
			&h,
			201,
			"server.shutdown",
			json!({ "grace_ms": 1500 }),
		);
		assert_eq!(r["ok"], true);
		assert_eq!(a, Action::Shutdown { grace_ms: 1500 });
		assert!(h.calls().contains(&"decide true true".to_string()));
		assert!(h.calls().contains(&"relay_auth totp=true".to_string()));
	}

	#[test]
	fn view_only_needs_allow() {
		let h = Fake::default();
		let mut s = Session::new(SEAT);
		call(&mut s, &h, 1, "hello", hello_body());
		call(
			&mut s,
			&h,
			2,
			"secrets.load",
			json!({ "identity": identity_b64() }),
		);
		call(
			&mut s,
			&h,
			3,
			"config.apply",
			json!({ "display_name": "A" }),
		);
		call(
			&mut s,
			&h,
			4,
			"auth.decide",
			json!({ "request_id": 7, "allow": false, "view_only": true }),
		);
		assert!(h.calls().contains(&"decide false false".to_string()));
	}

	#[test]
	fn malformed_frames() {
		let h = Fake::default();
		let mut s = Session::new(SEAT);
		let (out, a) = s.handle(b"{not json", &h);
		let v: Value = serde_json::from_slice(&out.unwrap()).unwrap();
		assert_eq!(v["error"]["code"], "bad_request");
		assert_eq!(a, Action::Continue);
		let (out, _) = s.handle(br#"{"type":"hello"}"#, &h);
		let v: Value = serde_json::from_slice(&out.unwrap()).unwrap();
		assert_eq!(v["error"]["code"], "bad_request");
	}

	#[test]
	fn config_defaults() {
		let c: SeatConfig = serde_json::from_value(json!({ "display_name": "X" })).unwrap();
		assert_eq!(c.network_mode, "auto");
		assert_eq!(c.access, "approve_or_password");
		assert!(c.lan_discovery && c.transmit_audio && !c.port_mapping);
		assert_eq!(c.max_clients, 1);
		assert_eq!(c.capture, "auto");
	}

	#[test]
	fn frame_roundtrip_and_cap() {
		let f = encode_frame(b"abc").unwrap();
		assert_eq!(&f[..], &[3, 0, 0, 0, b'a', b'b', b'c']);
		assert!(encode_frame(&vec![0u8; MAX_FRAME + 1]).is_err());
	}

	#[tokio::test]
	async fn read_frame_rejects_oversize_header() {
		let (mut a, mut b) = tokio::io::duplex(64);
		a.write_all(&((MAX_FRAME as u32) + 1).to_le_bytes())
			.await
			.unwrap();
		assert!(read_frame(&mut b).await.is_err());
	}

	/// End to end over an in-memory pipe: the "service" side writes requests, reads
	/// responses and an event pushed by the host.
	#[tokio::test]
	async fn serve_over_fake_pipe() {
		let (svc, host_end) = tokio::io::duplex(1 << 16);
		let (hr, hw) = tokio::io::split(host_end);
		let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
		let fake = std::sync::Arc::new(Fake::default());
		let f2 = fake.clone();
		let task = tokio::spawn(async move { serve(hr, hw, SEAT, &*f2, rx).await });
		let (mut sr, mut sw) = tokio::io::split(svc);
		write_frame(&mut sw, &req(1, "hello", hello_body()))
			.await
			.unwrap();
		let r: Value = serde_json::from_slice(&read_frame(&mut sr).await.unwrap()).unwrap();
		assert_eq!(r["ok"], true);
		tx.send(event("otp.changed", json!({}))).unwrap();
		let e: Value = serde_json::from_slice(&read_frame(&mut sr).await.unwrap()).unwrap();
		assert_eq!(e["event"], "otp.changed");
		write_frame(&mut sw, &req(2, "server.shutdown", json!({})))
			.await
			.unwrap();
		let r: Value = serde_json::from_slice(&read_frame(&mut sr).await.unwrap()).unwrap();
		assert_eq!(r["id"], 2);
		assert_eq!(task.await.unwrap(), Action::Shutdown { grace_ms: 5000 });
	}

	#[tokio::test]
	async fn serve_closes_on_eof() {
		let (svc, host_end) = tokio::io::duplex(1024);
		let (hr, hw) = tokio::io::split(host_end);
		let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
		let fake = Fake::default();
		drop(svc);
		assert_eq!(serve(hr, hw, SEAT, &fake, rx).await, Action::Close);
	}
}
