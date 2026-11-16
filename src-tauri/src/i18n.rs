//! Backend i18n: user-visible strings the RUST side produces (error messages shown
//! in frontend modals, dialog window titles) live in `lang/tr.json` + `lang/en.json`
//! and are looked up by key via [`t`] — same model as the frontend's i18n.tr.ts /
//! i18n.en.ts and the renderer's `lang/*.json`. The active language follows
//! `Config.language` ([`set_lang`] runs at config load and on every set_config).
//! English is the default; missing keys fall back to English, then to the key itself.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

static TR_JSON: &str = include_str!("../lang/tr.json");
static EN_JSON: &str = include_str!("../lang/en.json");

static ENGLISH: AtomicBool = AtomicBool::new(true);

/// Follow `Config.language` (Tr → Turkish; anything else English).
pub(crate) fn set_lang(language: &pulsar_core::config::Language) {
	ENGLISH.store(
		!matches!(language, pulsar_core::config::Language::Tr),
		Ordering::Relaxed,
	);
}

/// The OS UI language is Turkish. Decides the language of a NEW config (no saved choice):
/// Turkish on a Turkish OS, English everywhere else.
pub(crate) fn os_is_turkish() -> bool {
	#[cfg(windows)]
	{
		// SAFETY: plain Win32 call, no arguments.
		let id = unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() };
		id & 0x3ff == 0x1f // PRIMARYLANGID == LANG_TURKISH
	}
	#[cfg(not(windows))]
	{
		let env = ["LC_ALL", "LC_MESSAGES", "LANG", "LANGUAGE"]
			.iter()
			.find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()));
		#[cfg(target_os = "macos")]
		let env = env.or_else(|| {
			// GUI apps on macOS get no LANG: ask the user defaults for the first preferred language.
			std::process::Command::new("defaults")
				.args(["read", "-g", "AppleLanguages"])
				.output()
				.ok()
				.and_then(|o| String::from_utf8(o.stdout).ok())
				.and_then(|s| s.split('"').nth(1).map(str::to_string))
		});
		env.is_some_and(|v| v.to_ascii_lowercase().starts_with("tr"))
	}
}

/// The language for a freshly loaded config: the saved one when `config.json` exists,
/// else the OS language (Turkish) or English.
pub(crate) fn config_language(cfg: &pulsar_core::config::Config, path: &std::path::Path) -> pulsar_core::config::Language {
	if path.exists() {
		cfg.language
	} else if os_is_turkish() {
		pulsar_core::config::Language::Tr
	} else {
		pulsar_core::config::Language::En
	}
}

/// The active language code for child processes (the renderer's `--lang`).
pub(crate) fn lang() -> &'static str {
	if ENGLISH.load(Ordering::Relaxed) {
		"en"
	} else {
		"tr"
	}
}

fn parse(json: &'static str) -> HashMap<&'static str, &'static str> {
	let v: serde_json::Value = serde_json::from_str(json).expect("lang json is valid");
	let obj = v.as_object().expect("lang json is an object");
	obj.iter()
		.filter_map(|(k, v)| {
			let s = v.as_str()?;
			// Leak once: small, static-for-life catalogs.
			Some((
				&*Box::leak(k.clone().into_boxed_str()),
				&*Box::leak(s.to_string().into_boxed_str()),
			))
		})
		.collect()
}

fn tr() -> &'static HashMap<&'static str, &'static str> {
	static T: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
	T.get_or_init(|| parse(TR_JSON))
}

fn en() -> &'static HashMap<&'static str, &'static str> {
	static T: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
	T.get_or_init(|| parse(EN_JSON))
}

/// Localized string for `key` (active language → EN fallback → the key, so typos stay visible).
pub(crate) fn t(key: &str) -> &'static str {
	t_in(key, ENGLISH.load(Ordering::Relaxed))
}

/// [`t`] in an explicit language instead of the active one. Use `t_in(key, true)` for
/// text that leaves this machine (host → client): English is the project's wire language.
pub(crate) fn t_in(key: &str, english: bool) -> &'static str {
	if !english {
		if let Some(s) = tr().get(key) {
			return s;
		}
	}
	if let Some(s) = en().get(key) {
		return s;
	}
	Box::leak(key.to_string().into_boxed_str())
}

/// English text for `key`, whatever the UI language: for everything that travels to
/// another machine (host → client errors, HUD stats, window titles) or into a log.
pub(crate) fn wire(key: &str) -> &'static str {
	t_in(key, true)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn english_is_default_and_fallback() {
		// Every Turkish key has an English counterpart, so the fallback never shows Turkish.
		for k in tr().keys() {
			assert!(en().contains_key(k), "lang/en.json is missing {k}");
		}
		assert_eq!(t_in("no.such.key", false), "no.such.key");
	}
}
