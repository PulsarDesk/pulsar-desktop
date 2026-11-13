//! Host side of device pairing: the trust store of paired client keys, the auth decision
//! that consults it, the `PairRequest` / `Unpair` handler and the Settings → Güvenlik
//! commands that list and revoke paired devices.
//!
//! A paired client is identified by its STATIC X25519 public key (`Session::peer_pubkey`) —
//! the same no-MITM-safe identity same-device pre-auth (`auth::preauth`) is keyed on, never
//! the spoofable relay id. Pairing is authorized once, never silently: a session that proved
//! a password / one-time password pairs at once; any other session (let in by the operator's
//! Allow, pre-authorized, unattended) first gets a pairing-specific Allow/Deny popup.
//!
//! A paired key skips the password and the Allow/Deny popup on later connections. It does
//! NOT skip the host's 2FA: with Settings → Güvenlik → 2FA on, a paired device still sends
//! the authenticator code (the safer default — pairing replaces the first factor, not both).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pulsar_core::service::{Credential, DataMsg};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

/// One trusted client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PairedDevice {
	/// The client's static X25519 public key, lowercase hex (64 chars).
	pub key: String,
	/// The client's display name at pairing time (its `PairRequest`).
	#[serde(default)]
	pub name: String,
	/// The peer label at pairing time (relay id "482 913 056" or a direct address).
	#[serde(default)]
	pub id: String,
	/// Epoch ms.
	#[serde(default)]
	pub paired_at: u64,
	/// Epoch ms of the last connection the pairing let in (0 = never since pairing).
	#[serde(default)]
	pub last_seen: u64,
}

/// The paired-devices list as stored in `<app_config_dir>/paired_devices.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TrustStore {
	#[serde(default)]
	pub devices: Vec<PairedDevice>,
}

/// Hex form of a public key (the store's key).
pub(crate) fn key_hex(key: &[u8; 32]) -> String {
	key.iter().map(|b| format!("{b:02x}")).collect()
}

/// Longest name / label kept per entry (a client chooses its name).
const NAME_MAX: usize = 64;
/// Most paired devices kept; pairing one more drops the least recently seen.
const MAX_DEVICES: usize = 64;

fn clip(s: &str) -> String {
	s.trim().chars().filter(|c| !c.is_control()).take(NAME_MAX).collect()
}

impl TrustStore {
	/// Read the store; a missing or corrupt file is an empty store (nobody trusted).
	pub fn load(path: &Path) -> Self {
		std::fs::read_to_string(path)
			.ok()
			.and_then(|raw| serde_json::from_str(&raw).ok())
			.unwrap_or_default()
	}

	pub fn save(&self, path: &Path) {
		if let Some(dir) = path.parent() {
			let _ = std::fs::create_dir_all(dir);
		}
		if let Ok(json) = serde_json::to_string_pretty(self) {
			crate::util::atomic_write_json(path, &json);
		}
	}

	pub fn is_trusted(&self, key: &[u8; 32]) -> bool {
		let k = key_hex(key);
		self.devices.iter().any(|d| d.key == k)
	}

	/// Trust `key` (or refresh its name/label if it already is). Returns true when it was new.
	pub fn add(&mut self, key: &[u8; 32], name: &str, id: &str, now_ms: u64) -> bool {
		let k = key_hex(key);
		if let Some(d) = self.devices.iter_mut().find(|d| d.key == k) {
			if !name.trim().is_empty() {
				d.name = clip(name);
			}
			d.id = clip(id);
			return false;
		}
		if self.devices.len() >= MAX_DEVICES {
			if let Some(i) = self
				.devices
				.iter()
				.enumerate()
				.min_by_key(|(_, d)| d.last_seen.max(d.paired_at))
				.map(|(i, _)| i)
			{
				self.devices.remove(i);
			}
		}
		self.devices.push(PairedDevice {
			key: k,
			name: clip(name),
			id: clip(id),
			paired_at: now_ms,
			last_seen: 0,
		});
		true
	}

	/// Stop trusting the key given as hex. Returns true when an entry was removed.
	pub fn remove_hex(&mut self, key_hex: &str) -> bool {
		let k = key_hex.trim().to_ascii_lowercase();
		let before = self.devices.len();
		self.devices.retain(|d| d.key != k);
		self.devices.len() != before
	}

	/// Stamp a connection the pairing let in.
	pub fn touch(&mut self, key: &[u8; 32], now_ms: u64) -> bool {
		let k = key_hex(key);
		match self.devices.iter_mut().find(|d| d.key == k) {
			Some(d) => {
				d.last_seen = now_ms;
				true
			}
			None => false,
		}
	}
}

// ---------------------------------------------------------------------------
// The auth decision (pure, unit-tested)
// ---------------------------------------------------------------------------

/// The first step of the host's auth decision for an incoming session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Gate {
	/// Unattended access is on: no password asked.
	Open,
	/// Same-device pre-auth (a live or very recent authorized session from this key).
	PreAuth,
	/// A paired key: no password, no popup.
	Paired,
	/// This peer is locked out for wrong guesses: reject without a popup.
	Locked,
	/// Ask: up-front password, then the Allow/Deny popup raced against the password prompt.
	Challenge,
}

/// Decide how an incoming session is authorized. A paired key is checked BEFORE the
/// per-peer lockout: the lockout throttles password guesses (keyed on the relay id or
/// address), and a paired device guesses nothing — so a stranger's wrong guesses under a
/// shared address can't lock a paired device out. An unpaired peer gets the lockout as before.
pub(crate) fn gate(require_auth: bool, preauthorized: bool, paired: bool, locked: bool) -> Gate {
	if !require_auth {
		Gate::Open
	} else if preauthorized {
		Gate::PreAuth
	} else if paired {
		Gate::Paired
	} else if locked {
		Gate::Locked
	} else {
		Gate::Challenge
	}
}

/// What let an accepted session in — drives the 2FA and the pairing decisions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuthVia {
	Unattended,
	PreAuth,
	Paired,
	/// The one-time password or the persistent connect password.
	Password,
	/// The host user clicked Allow on the popup.
	Operator,
}

/// Does an up-front password attempt authorize the session? `otp_consumed` = it proved the
/// one-time password AND that code was still current (consumed atomically); `configured` =
/// the host has at least one password. A wrong (or empty) attempt never does.
pub(crate) fn password_ok(cred: Credential, otp_consumed: bool, configured: bool) -> bool {
	configured
		&& match cred {
			Credential::Fixed => true,
			Credential::OneTime => otp_consumed,
			_ => false,
		}
}

/// Must this session also send the 2FA code? Only when 2FA is on, and not for a session
/// the host user approved on the popup (a human vouched for it) or a pre-authorized 2nd
/// pane of a device that already passed it. A PAIRED device still sends it.
pub(crate) fn needs_totp(via: AuthVia, totp_on: bool) -> bool {
	totp_on && matches!(via, AuthVia::Unattended | AuthVia::Paired | AuthVia::Password)
}

/// How the host answers a `PairRequest`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PairDecision {
	/// Trust the key now: this session proved a password / OTP (or is already paired —
	/// re-pairing only refreshes the name).
	Accept,
	/// Ask the host user on a pairing popup first.
	AskOperator,
}

pub(crate) fn pair_decision(via: AuthVia) -> PairDecision {
	match via {
		AuthVia::Password | AuthVia::Paired => PairDecision::Accept,
		AuthVia::Operator | AuthVia::PreAuth | AuthVia::Unattended => PairDecision::AskOperator,
	}
}

// ---------------------------------------------------------------------------
// The app's store (file-backed)
// ---------------------------------------------------------------------------

/// Serializes read-modify-write of the store file.
static STORE_LOCK: Mutex<()> = Mutex::new(());

fn store_path(app: &AppHandle) -> PathBuf {
	crate::util::data_dir(app).join("paired_devices.json")
}

fn now_ms() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_millis() as u64)
		.unwrap_or(0)
}

/// Is this client key paired with this host?
pub(crate) fn is_paired(app: &AppHandle, key: &[u8; 32]) -> bool {
	let _g = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
	TrustStore::load(&store_path(app)).is_trusted(key)
}

/// Stamp `last_seen` for a connection the pairing let in.
pub(crate) fn touch(app: &AppHandle, key: &[u8; 32]) {
	let path = store_path(app);
	let _g = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
	let mut s = TrustStore::load(&path);
	if s.touch(key, now_ms()) {
		s.save(&path);
	}
}

fn add(app: &AppHandle, key: &[u8; 32], name: &str, id: &str) {
	let path = store_path(app);
	{
		let _g = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
		let mut s = TrustStore::load(&path);
		s.add(key, name, id, now_ms());
		s.save(&path);
	}
	tracing::info!(%id, "pairing: client key trusted");
	let _ = app.emit("paired-devices-changed", ());
	crate::seat::trust_changed();
}

fn remove(app: &AppHandle, key_hex: &str) -> bool {
	let path = store_path(app);
	let removed = {
		let _g = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
		let mut s = TrustStore::load(&path);
		let removed = s.remove_hex(key_hex);
		if removed {
			s.save(&path);
		}
		removed
	};
	if removed {
		// Revocation must also end the short pre-auth grace for this key, or a reconnect
		// within `preauth::RECENT_TTL` would still skip the password.
		if let Some(k) = parse_key(key_hex) {
			crate::auth::preauth::forget_recent(&k);
		}
		tracing::info!("pairing: client key revoked");
		let _ = app.emit("paired-devices-changed", ());
		crate::seat::trust_changed();
	}
	removed
}

fn parse_key(hex: &str) -> Option<[u8; 32]> {
	let hex = hex.trim();
	if hex.len() != 64 {
		return None;
	}
	let mut k = [0u8; 32];
	for (i, b) in k.iter_mut().enumerate() {
		*b = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
	}
	Some(k)
}

/// Settings → Güvenlik → Eşleşmiş cihazlar.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PairedView {
	key: String,
	/// Short fingerprint for display (first 16 hex chars, grouped).
	fingerprint: String,
	name: String,
	id: String,
	paired_at: u64,
	last_seen: u64,
}

#[tauri::command]
pub(crate) fn list_paired_devices(app: AppHandle) -> Vec<PairedView> {
	let _g = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
	TrustStore::load(&store_path(&app))
		.devices
		.into_iter()
		.map(|d| PairedView {
			fingerprint: d
				.key
				.get(..16)
				.unwrap_or(&d.key)
				.as_bytes()
				.chunks(4)
				.map(|c| String::from_utf8_lossy(c).into_owned())
				.collect::<Vec<_>>()
				.join(" "),
			key: d.key,
			name: d.name,
			id: d.id,
			paired_at: d.paired_at,
			last_seen: d.last_seen,
		})
		.collect()
}

#[tauri::command]
pub(crate) fn revoke_paired_device(app: AppHandle, key: String) -> bool {
	remove(&app, &key)
}

/// The trust store's devices (seat host `trust.list`).
pub(crate) fn devices(app: &AppHandle) -> Vec<PairedDevice> {
	let _g = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
	TrustStore::load(&store_path(app)).devices
}

/// Revoke one paired key (seat host `trust.revoke`). False when it was not paired.
pub(crate) fn revoke(app: &AppHandle, key_hex: &str) -> bool {
	remove(app, key_hex)
}

// ---------------------------------------------------------------------------
// The PairRequest / Unpair handler for one accepted session
// ---------------------------------------------------------------------------

/// One session's pairing state: one decision per session, repeated requests re-send it.
enum PairState {
	Idle,
	Pending,
	Done(bool, String),
}

/// Build the session's `DataHandlers::on_pair`. `fallback_name` names the device when the
/// client sent none (its pushed display name, else the peer label).
#[allow(clippy::too_many_arguments)]
pub(crate) fn make_on_pair(
	app: AppHandle,
	peer: String,
	pubkey: Option<[u8; 32]>,
	via: AuthVia,
	out: tokio::sync::mpsc::Sender<DataMsg>,
	pending: Arc<Mutex<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<crate::auth::AuthDecision>>>>,
	next_req: Arc<std::sync::atomic::AtomicU64>,
	fallback_name: Arc<dyn Fn() -> Option<String> + Send + Sync>,
) -> impl FnMut(DataMsg) + Send + 'static {
	let state = Arc::new(Mutex::new(PairState::Idle));
	move |m: DataMsg| {
		let reply = |out: &tokio::sync::mpsc::Sender<DataMsg>, paired: bool, reason: &str| {
			let _ = out.try_send(DataMsg::PairResult { paired, reason: reason.into() });
		};
		let Some(key) = pubkey else {
			reply(&out, false, "unsupported");
			return;
		};
		match m {
			DataMsg::Unpair => {
				remove(&app, &key_hex(&key));
				*state.lock().unwrap() = PairState::Done(false, String::new());
				reply(&out, false, "");
			}
			DataMsg::PairRequest { name } => {
				let mut st = state.lock().unwrap();
				match &*st {
					PairState::Pending => return,
					PairState::Done(p, r) => {
						reply(&out, *p, r);
						return;
					}
					PairState::Idle => {}
				}
				let name = if name.trim().is_empty() {
					fallback_name().unwrap_or_else(|| peer.clone())
				} else {
					name
				};
				match pair_decision(via) {
					PairDecision::Accept => {
						add(&app, &key, &name, &peer);
						*st = PairState::Done(true, String::new());
						reply(&out, true, "");
					}
					PairDecision::AskOperator => {
						*st = PairState::Pending;
						drop(st);
						let (app, peer, out, state) = (app.clone(), peer.clone(), out.clone(), state.clone());
						let (pending, next_req) = (pending.clone(), next_req.clone());
						tokio::spawn(async move {
							let (paired, reason) =
								match crate::auth::ask_operator_pair(&app, &pending, &next_req, &peer, &key, &name).await {
									Some(true) => {
										add(&app, &key, &name, &peer);
										(true, "")
									}
									Some(false) => (false, "denied"),
									None => (false, "timeout"),
								};
							*state.lock().unwrap() = PairState::Done(paired, reason.into());
							let _ = out.try_send(DataMsg::PairResult { paired, reason: reason.into() });
						});
					}
				}
			}
			_ => {}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn tmp(name: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!("pulsar-pair-test-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		dir.join("paired_devices.json")
	}

	const A: [u8; 32] = [0xAB; 32];
	const B: [u8; 32] = [0x01; 32];

	#[test]
	fn store_add_persist_revoke() {
		let p = tmp("persist");
		let mut s = TrustStore::load(&p);
		assert!(s.devices.is_empty(), "missing file = nobody trusted");
		assert!(!s.is_trusted(&A));
		assert!(s.add(&A, "Laptop", "482 913 056", 10));
		assert!(!s.add(&A, "Laptop 2", "482 913 056", 20), "re-pair refreshes, not duplicates");
		assert_eq!(s.devices.len(), 1);
		assert_eq!(s.devices[0].name, "Laptop 2");
		assert_eq!(s.devices[0].paired_at, 10);
		s.save(&p);

		let mut back = TrustStore::load(&p);
		assert!(back.is_trusted(&A));
		assert!(!back.is_trusted(&B));
		assert!(back.touch(&A, 99));
		assert!(!back.touch(&B, 99));
		assert_eq!(back.devices[0].last_seen, 99);

		assert!(back.remove_hex(&key_hex(&A).to_uppercase()), "hex compare is case-insensitive");
		assert!(!back.remove_hex(&key_hex(&A)), "second revoke is a no-op");
		back.save(&p);
		assert!(!TrustStore::load(&p).is_trusted(&A), "revocation persists");
	}

	#[test]
	fn store_corrupt_file_trusts_nobody() {
		let p = tmp("corrupt");
		std::fs::create_dir_all(p.parent().unwrap()).unwrap();
		std::fs::write(&p, "{not json").unwrap();
		assert!(!TrustStore::load(&p).is_trusted(&A));
	}

	#[test]
	fn store_caps_and_sanitizes_names() {
		let mut s = TrustStore::default();
		s.add(&A, &format!("  x\u{7}{}", "n".repeat(200)), "id", 1);
		assert_eq!(s.devices[0].name.chars().count(), NAME_MAX);
		assert!(!s.devices[0].name.contains('\u{7}'));
		for i in 0..MAX_DEVICES as u8 + 3 {
			s.add(&[i.wrapping_add(2); 32], "d", "id", 100 + i as u64);
		}
		assert_eq!(s.devices.len(), MAX_DEVICES);
	}

	#[test]
	fn gate_paired_unpaired_revoked() {
		// Paired key: no password, no popup — even while its address is locked out.
		assert_eq!(gate(true, false, true, false), Gate::Paired);
		assert_eq!(gate(true, false, true, true), Gate::Paired);
		// Unpaired (or revoked = no longer in the store): the normal challenge / lockout.
		assert_eq!(gate(true, false, false, false), Gate::Challenge);
		assert_eq!(gate(true, false, false, true), Gate::Locked);
		// Pre-auth wins (a live authorized session of this device), unattended opens.
		assert_eq!(gate(true, true, false, false), Gate::PreAuth);
		assert_eq!(gate(false, false, false, true), Gate::Open);

		// Revoked: the store no longer trusts the key → the gate falls back to Challenge.
		let mut s = TrustStore::default();
		s.add(&A, "x", "y", 1);
		assert_eq!(gate(true, false, s.is_trusted(&A), false), Gate::Paired);
		s.remove_hex(&key_hex(&A));
		assert_eq!(gate(true, false, s.is_trusted(&A), false), Gate::Challenge);
	}

	#[test]
	fn wrong_password_never_authorizes() {
		assert!(!password_ok(Credential::None, false, true), "wrong password");
		assert!(!password_ok(Credential::None, true, true));
		assert!(!password_ok(Credential::OneTime, false, true), "stale OTP (already consumed)");
		assert!(password_ok(Credential::OneTime, true, true));
		assert!(password_ok(Credential::Fixed, false, true));
		assert!(!password_ok(Credential::Fixed, false, false), "no password configured");
	}

	#[test]
	fn paired_device_still_needs_2fa() {
		assert!(needs_totp(AuthVia::Paired, true));
		assert!(needs_totp(AuthVia::Password, true));
		assert!(needs_totp(AuthVia::Unattended, true));
		assert!(!needs_totp(AuthVia::Operator, true));
		assert!(!needs_totp(AuthVia::PreAuth, true));
		assert!(!needs_totp(AuthVia::Paired, false));
	}

	#[test]
	fn pairing_is_never_silent() {
		assert_eq!(pair_decision(AuthVia::Password), PairDecision::Accept);
		assert_eq!(pair_decision(AuthVia::Paired), PairDecision::Accept);
		// Allow on the CONNECT popup didn't consent to permanent trust; pre-auth and
		// unattended proved nothing in this session → the pairing popup asks.
		assert_eq!(pair_decision(AuthVia::Operator), PairDecision::AskOperator);
		assert_eq!(pair_decision(AuthVia::PreAuth), PairDecision::AskOperator);
		assert_eq!(pair_decision(AuthVia::Unattended), PairDecision::AskOperator);
	}

	#[test]
	fn parse_key_roundtrip() {
		assert_eq!(parse_key(&key_hex(&A)), Some(A));
		assert_eq!(parse_key("abc"), None);
		assert_eq!(parse_key(&"zz".repeat(32)), None);
	}
}
