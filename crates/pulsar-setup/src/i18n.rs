//! Setup-window i18n: every user-visible string lives in `lang/en.json` + `lang/tr.json`
//! (compiled in), looked up by key via [`t`]. The language follows the app's rule: the
//! user's saved choice (the app's `splash-theme` hint, then its `config.json`), else the OS
//! UI language when it is Turkish, else English. Missing keys fall back to English, then
//! to the key itself. Error details (the "ERROR DETAILS" box) stay English.

use std::collections::HashMap;
use std::sync::OnceLock;

static EN_JSON: &str = include_str!("../lang/en.json");
static TR_JSON: &str = include_str!("../lang/tr.json");

fn parse(json: &'static str) -> HashMap<String, &'static str> {
	let v: serde_json::Value = serde_json::from_str(json).expect("lang json is valid");
	v.as_object()
		.expect("lang json is an object")
		.iter()
		.filter_map(|(k, v)| Some((k.clone(), &*Box::leak(v.as_str()?.to_string().into_boxed_str()))))
		.collect()
}

fn en() -> &'static HashMap<String, &'static str> {
	static T: OnceLock<HashMap<String, &'static str>> = OnceLock::new();
	T.get_or_init(|| parse(EN_JSON))
}

fn tr() -> &'static HashMap<String, &'static str> {
	static T: OnceLock<HashMap<String, &'static str>> = OnceLock::new();
	T.get_or_init(|| parse(TR_JSON))
}

/// The setup window speaks Turkish (decided once per run).
pub fn turkish() -> bool {
	static TR: OnceLock<bool> = OnceLock::new();
	*TR.get_or_init(|| match saved_lang() {
		Some(l) => l == "tr",
		None => os_is_turkish(),
	})
}

/// Localized string for `key` (TR when [`turkish`], then EN, then the key).
pub fn t(key: &str) -> &'static str {
	lookup(key, turkish())
}

fn lookup(key: &str, turkish: bool) -> &'static str {
	if turkish {
		if let Some(s) = tr().get(key) {
			return s;
		}
	}
	if let Some(s) = en().get(key) {
		return s;
	}
	Box::leak(key.to_string().into_boxed_str())
}

/// `key` in every language — for matching a window another setup run titled.
pub fn all(key: &str) -> [&'static str; 2] {
	[lookup(key, false), lookup(key, true)]
}

/// The app's saved UI language (`tr`, `en`, …) for this user, if it ever ran.
fn saved_lang() -> Option<String> {
	let dir = std::path::PathBuf::from(std::env::var_os("APPDATA")?).join("dev.pulsar.app");
	// The UI's own choice (written by the frontend), then the backend config.
	let hint = std::fs::read_to_string(dir.join("splash-theme")).ok();
	if let Some(l) = hint.as_deref().and_then(|h| h.split_whitespace().find_map(|w| w.strip_prefix("lang="))) {
		return Some(l.to_string());
	}
	let cfg: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("config.json")).ok()?).ok()?;
	cfg.get("language")?.as_str().map(str::to_string)
}

fn os_is_turkish() -> bool {
	#[cfg(windows)]
	{
		// SAFETY: plain Win32 call, no arguments.
		let id = unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() };
		id & 0x3ff == 0x1f // PRIMARYLANGID == LANG_TURKISH
	}
	#[cfg(not(windows))]
	{
		["LC_ALL", "LC_MESSAGES", "LANG"]
			.iter()
			.find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
			.is_some_and(|v| v.to_ascii_lowercase().starts_with("tr"))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn catalogs_match_and_english_is_the_fallback() {
		for k in en().keys() {
			assert!(tr().contains_key(k), "lang/tr.json is missing {k}");
		}
		for k in tr().keys() {
			assert!(en().contains_key(k), "lang/en.json is missing {k}");
		}
		assert_eq!(lookup("btn.close", false), "Close");
		assert_eq!(lookup("btn.close", true), "Kapat");
		assert_eq!(lookup("no.such.key", true), "no.such.key");
	}
}
