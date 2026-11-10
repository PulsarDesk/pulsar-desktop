//! Settings → Güvenlik → two-factor auth for incoming connections (RustDesk-style TOTP).
//!
//! Enrolling: `totp_begin` mints a secret and renders its QR / manual code; the user adds
//! it to an authenticator app and proves it with one code (`totp_verify`); only then the UI
//! stores the secret in the config (`Config::totp_secret`). The host side of the check is
//! `auth::host_totp`.

use serde::Serialize;

/// What the enrollment dialog shows.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TotpSetup {
	/// The base32 secret (what the UI saves once it is verified).
	secret: String,
	/// The same in 4-character groups, for typing it into an app by hand.
	secret_grouped: String,
	/// `otpauth://` URI (some authenticators accept it pasted).
	uri: String,
	/// The URI as an inline SVG QR code.
	qr_svg: String,
}

/// A fresh TOTP secret + its QR for this device (`Pulsar (<device name>)` in the app).
#[tauri::command]
pub(crate) fn totp_begin(state: tauri::State<'_, crate::state::AppState>) -> TotpSetup {
	let name = state.config.lock().unwrap().device_name.clone();
	let label = if name.trim().is_empty() { "Pulsar".to_string() } else { format!("Pulsar ({})", name.trim()) };
	let e = pulsar_relay::generate_totp(&label, "Pulsar");
	TotpSetup { secret: e.secret, secret_grouped: e.secret_grouped, uri: e.uri, qr_svg: e.qr_svg }
}

/// Whether `code` is the current authenticator code for `secret` (the enrollment proof).
#[tauri::command]
pub(crate) fn totp_verify(secret: String, code: String) -> bool {
	pulsar_core::totp::code_valid_now(&secret, &code)
}
