//! Client side of saved devices' credentials: the custom connect password saved for a device
//! (Devices → add/edit) and whether this client is PAIRED with it (the host trusts our key).
//!
//! Storage: `<app_config_dir>/device_creds.json`, per OS user, next to `identity.key` and
//! `config.json` (which already holds this host's own connect password and 2FA secret). The
//! app has no OS-keychain dependency, so instead of adding one:
//! - Windows: the password is sealed with DPAPI (`CryptProtectData`, current-user scope) —
//!   only this Windows account can unseal it; the file alone (a backup, another account) is
//!   useless.
//! - macOS / Linux: stored as-is, like the rest of the app's secrets, and the file is made
//!   owner-only (0600).
//! The webview never persists it (no localStorage): it asks Rust per device.
//!
//! Entries are keyed per relay for relay ids (ids are minted per relay — the same scoping as
//! the TOFU pins in `known_peers.json`) and by address for direct (IP) targets.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::state::AppState;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CredEntry {
	/// How `pw` is encoded: `"dpapi"` (hex of the DPAPI blob) or `"plain"`.
	#[serde(default)]
	pub enc: String,
	#[serde(default)]
	pub pw: String,
	/// The host accepted our `PairRequest` (cleared on unpair).
	#[serde(default)]
	pub paired: bool,
}

pub(crate) type CredMap = HashMap<String, CredEntry>;

/// Store key for a connect target. Relay ids ("482 913 056" / "482913056") are scoped to
/// the relay; anything else (an IP / host:port) is keyed by the address.
pub(crate) fn entry_key(relay: &str, target: &str) -> String {
	let despaced: String = target.chars().filter(|c| !c.is_whitespace()).collect();
	if despaced.len() == 9 && despaced.chars().all(|c| c.is_ascii_digit()) {
		format!("{}/{}", relay.trim().to_ascii_lowercase(), despaced)
	} else {
		format!("addr/{}", target.trim().to_ascii_lowercase())
	}
}

pub(crate) fn load(path: &Path) -> CredMap {
	std::fs::read_to_string(path)
		.ok()
		.and_then(|raw| serde_json::from_str(&raw).ok())
		.unwrap_or_default()
}

pub(crate) fn save(path: &Path, map: &CredMap) {
	if let Some(dir) = path.parent() {
		let _ = std::fs::create_dir_all(dir);
	}
	if let Ok(json) = serde_json::to_string(map) {
		crate::util::atomic_write_json(path, &json);
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
		}
	}
}

fn to_hex(b: &[u8]) -> String {
	b.iter().map(|x| format!("{x:02x}")).collect()
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
	if s.len() % 2 != 0 {
		return None;
	}
	(0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// Seal a password for storage → (`enc`, `pw`).
pub(crate) fn seal(pw: &str) -> (String, String) {
	#[cfg(windows)]
	if let Some(blob) = dpapi::protect(pw.as_bytes()) {
		return ("dpapi".into(), to_hex(&blob));
	}
	("plain".into(), pw.to_string())
}

/// Unseal a stored entry's password (`None` = none saved, or it can't be unsealed here —
/// e.g. the file was copied from another Windows account).
pub(crate) fn unseal(e: &CredEntry) -> Option<String> {
	if e.pw.is_empty() {
		return None;
	}
	match e.enc.as_str() {
		#[cfg(windows)]
		"dpapi" => from_hex(&e.pw)
			.and_then(|b| dpapi::unprotect(&b))
			.and_then(|b| String::from_utf8(b).ok()),
		"plain" | "" => Some(e.pw.clone()),
		_ => {
			let _ = from_hex; // only DPAPI decodes hex
			None
		}
	}
}

#[cfg(windows)]
mod dpapi {
	use windows_sys::Win32::Foundation::LocalFree;
	use windows_sys::Win32::Security::Cryptography::{
		CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
	};

	/// Extra entropy so another app's DPAPI blob can't be swapped in.
	const ENTROPY: &[u8] = b"pulsar-device-creds-v1";

	fn run(data: &[u8], protect: bool) -> Option<Vec<u8>> {
		let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
		let entropy = CRYPT_INTEGER_BLOB { cbData: ENTROPY.len() as u32, pbData: ENTROPY.as_ptr() as *mut u8 };
		let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
		// SAFETY: every pointer is valid for the call; `out` is allocated by DPAPI and freed
		// with LocalFree below after copying.
		let ok = unsafe {
			if protect {
				CryptProtectData(
					&input,
					std::ptr::null(),
					&entropy,
					std::ptr::null(),
					std::ptr::null(),
					CRYPTPROTECT_UI_FORBIDDEN,
					&mut out,
				)
			} else {
				CryptUnprotectData(
					&input,
					std::ptr::null_mut(),
					&entropy,
					std::ptr::null(),
					std::ptr::null(),
					CRYPTPROTECT_UI_FORBIDDEN,
					&mut out,
				)
			}
		};
		if ok == 0 || out.pbData.is_null() {
			return None;
		}
		// SAFETY: DPAPI returned `cbData` bytes at `pbData`.
		let v = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
		unsafe { LocalFree(out.pbData as _) };
		Some(v)
	}

	pub(super) fn protect(data: &[u8]) -> Option<Vec<u8>> {
		run(data, true)
	}

	pub(super) fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
		run(data, false)
	}
}

// ---------------------------------------------------------------------------
// File-backed store for the app
// ---------------------------------------------------------------------------

static LOCK: Mutex<()> = Mutex::new(());

fn path(app: &AppHandle) -> PathBuf {
	app.path()
		.app_config_dir()
		.unwrap_or_else(|_| PathBuf::from("."))
		.join("device_creds.json")
}

fn relay(state: &State<'_, AppState>) -> String {
	state.config.lock().unwrap().relay.clone()
}

/// Read-modify-write one entry; an entry left with no password and not paired is dropped.
fn update(app: &AppHandle, key: &str, f: impl FnOnce(&mut CredEntry)) {
	let p = path(app);
	let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
	let mut map = load(&p);
	let mut e = map.remove(key).unwrap_or_default();
	f(&mut e);
	if !e.pw.is_empty() || e.paired {
		map.insert(key.to_string(), e);
	}
	save(&p, &map);
}

fn get(app: &AppHandle, key: &str) -> CredEntry {
	let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
	load(&path(app)).remove(key).unwrap_or_default()
}

/// The saved password for a connect target, if any (sent up front by `client_authenticate`).
pub(crate) fn saved_password(app: &AppHandle, relay: &str, target: &str) -> Option<String> {
	unseal(&get(app, &entry_key(relay, target)))
}

/// What the Devices UI shows for one device.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CredInfo {
	has_password: bool,
	paired: bool,
}

/// Badges for several devices at once (`targets` as the UI stores them).
#[tauri::command]
pub(crate) fn device_creds(app: AppHandle, state: State<'_, AppState>, targets: Vec<String>) -> HashMap<String, CredInfo> {
	let relay = relay(&state);
	let map = {
		let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
		load(&path(&app))
	};
	targets
		.into_iter()
		.map(|t| {
			let e = map.get(&entry_key(&relay, &t));
			let info = CredInfo {
				has_password: e.is_some_and(|e| !e.pw.is_empty()),
				paired: e.is_some_and(|e| e.paired),
			};
			(t, info)
		})
		.collect()
}

/// The saved password itself, for the edit dialog's (masked) field. Empty = none.
#[tauri::command]
pub(crate) fn get_device_password(app: AppHandle, state: State<'_, AppState>, target: String) -> String {
	saved_password(&app, &relay(&state), &target).unwrap_or_default()
}

/// Save (or, with an empty `password`, remove) a device's custom connect password.
#[tauri::command]
pub(crate) fn set_device_password(app: AppHandle, state: State<'_, AppState>, target: String, password: String) {
	let key = entry_key(&relay(&state), &target);
	update(&app, &key, |e| {
		if password.is_empty() {
			e.pw.clear();
			e.enc.clear();
		} else {
			(e.enc, e.pw) = seal(&password);
		}
	});
}

/// Drop everything saved for a device (it was removed from Devices, or its id changed).
#[tauri::command]
pub(crate) fn forget_device_creds(app: AppHandle, state: State<'_, AppState>, target: String) {
	let key = entry_key(&relay(&state), &target);
	let p = path(&app);
	let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
	let mut map = load(&p);
	if map.remove(&key).is_some() {
		save(&p, &map);
	}
}

/// The answer to a pairing request.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PairOutcome {
	paired: bool,
	/// `""` on success; `"denied"` (host user said no / the password was refused),
	/// `"timeout"`, `"unsupported"` (old host), or a connect error string.
	reason: String,
}

/// Connect to `target`, authenticate (saved password up front; otherwise the host's popup /
/// the password prompt as on any connect) and ask the host to pair with this client.
#[tauri::command]
pub(crate) async fn pair_device(app: AppHandle, state: State<'_, AppState>, target: String) -> Result<PairOutcome, String> {
	let (mut sess, relay) = match open_authed(&app, &state, &target).await? {
		Some(s) => s,
		None => return Ok(PairOutcome { paired: false, reason: "denied".into() }),
	};
	let name = crate::util::announce_name(&state.config.lock().unwrap().device_name);
	let res = pulsar_core::service::pair_exchange(
		&mut sess,
		pulsar_core::service::DataMsg::PairRequest { name },
		pulsar_core::service::PAIR_WAIT,
	)
	.await
	.map_err(|e| e.to_string())?;
	let _ = pulsar_core::service::send_bye(&mut sess).await;
	let out = match res {
		Some((paired, reason)) => PairOutcome { paired, reason },
		None => PairOutcome { paired: false, reason: "unsupported".into() },
	};
	if out.paired {
		update(&app, &entry_key(&relay, &target), |e| e.paired = true);
	}
	tracing::info!(%target, paired = out.paired, reason = %out.reason, "pair request finished");
	Ok(out)
}

/// Forget the pairing: always locally, and on the host too when it can be reached right now
/// (`true` = the host confirmed). An unreachable host keeps trusting this key until its user
/// revokes it in Settings → Güvenlik.
#[tauri::command]
pub(crate) async fn unpair_device(app: AppHandle, state: State<'_, AppState>, target: String) -> Result<bool, String> {
	let relay = relay(&state);
	update(&app, &entry_key(&relay, &target), |e| e.paired = false);
	let remote = async {
		let (mut sess, _) = open_authed(&app, &state, &target).await.ok()??;
		let r = pulsar_core::service::pair_exchange(
			&mut sess,
			pulsar_core::service::DataMsg::Unpair,
			std::time::Duration::from_secs(6),
		)
		.await
		.ok()?;
		let _ = pulsar_core::service::send_bye(&mut sess).await;
		r.map(|(paired, _)| !paired)
	};
	Ok(tokio::time::timeout(std::time::Duration::from_secs(45), remote).await.ok().flatten().unwrap_or(false))
}

/// Connect + authenticate a short control session. `Ok(None)` = the host refused us.
async fn open_authed(
	app: &AppHandle,
	state: &State<'_, AppState>,
	target: &str,
) -> Result<Option<(pulsar_core::Session, String)>, String> {
	let node = state.node.lock().unwrap().clone().ok_or(crate::i18n::t("err.online"))?;
	let (pw_pending, next_auth) = (state.pw_pending.clone(), state.next_auth.clone());
	let disc = state.discovery.lock().unwrap().clone();
	let (net_mode, relay) = {
		let cfg = state.config.lock().unwrap();
		(cfg.network_mode, cfg.relay.clone())
	};
	let (mut sess, peer_label) = crate::util::connect_target(app, &node, disc, target, net_mode, &relay).await?;
	let saved = saved_password(app, &relay, target);
	let ok = tokio::time::timeout(
		std::time::Duration::from_secs(40),
		crate::auth::client_authenticate(&mut sess, app, &pw_pending, &next_auth, &peer_label, saved.as_deref()),
	)
	.await
	.map_err(|_| "connect-timed-out".to_string())??;
	Ok(ok.then_some((sess, relay)))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn keys_scope_ids_per_relay_and_addresses_alone() {
		assert_eq!(entry_key("Relay.Example:21116 ", "482 913 056"), "relay.example:21116/482913056");
		assert_eq!(entry_key("relay.example:21116", "482913056"), "relay.example:21116/482913056");
		assert_ne!(entry_key("a:1", "482913056"), entry_key("b:1", "482913056"));
		assert_eq!(entry_key("a:1", "192.168.1.5:9000"), entry_key("b:1", " 192.168.1.5:9000"));
	}

	#[test]
	fn seal_roundtrip_and_never_plain_on_windows() {
		let (enc, pw) = seal("gizli şifre");
		if cfg!(windows) {
			assert_eq!(enc, "dpapi");
			assert!(!pw.contains("gizli"), "DPAPI output must not carry the password");
		}
		let e = CredEntry { enc, pw, paired: false };
		assert_eq!(unseal(&e).as_deref(), Some("gizli şifre"));
		assert_eq!(unseal(&CredEntry::default()), None);
		assert_eq!(unseal(&CredEntry { enc: "rot13".into(), pw: "x".into(), paired: false }), None);
	}

	#[test]
	fn store_roundtrip() {
		let dir = std::env::temp_dir().join(format!("pulsar-creds-test-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		let p = dir.join("device_creds.json");
		assert!(load(&p).is_empty());
		let mut m = CredMap::new();
		let (enc, pw) = seal("pw");
		m.insert(entry_key("r:1", "111222333"), CredEntry { enc, pw, paired: true });
		save(&p, &m);
		let back = load(&p);
		let e = &back[&entry_key("r:1", "111 222 333")];
		assert!(e.paired);
		assert_eq!(unseal(e).as_deref(), Some("pw"));
		let raw = std::fs::read_to_string(&p).unwrap();
		if cfg!(windows) {
			assert!(!raw.contains("\"pw\":\"pw\""), "no plaintext password on disk on Windows");
		}
	}
}
