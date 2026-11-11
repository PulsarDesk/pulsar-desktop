//! One-time client hint about two local settings that hurt streaming latency and a
//! Bluetooth DualSense (both seen in the first real DS5 session):
//! - Wi-Fi on the 2.4 GHz band (shares the radio with Bluetooth on combo chips);
//! - running on battery with the WLAN adapter's power saving on (1.5 s stalls).
//!
//! Windows only for now, best-effort and cheap: `netsh wlan show interfaces` for the
//! band, `GetSystemPowerStatus` for AC/battery, `powercfg /query` for the "Wireless
//! Adapter Settings → Power Saving Mode" value of the active plan. Runs once per app run,
//! off the session loop, when a play session starts.

use std::sync::atomic::{AtomicBool, Ordering};

static SHOWN: AtomicBool = AtomicBool::new(false);

/// Check once per app run and queue the hint for play session `play_id`.
pub(crate) fn check_once(play_id: u64) {
	if SHOWN.swap(true, Ordering::Relaxed) {
		return;
	}
	let _ = std::thread::Builder::new()
		.name("link-hint".into())
		.spawn(move || {
			for key in hints() {
				tracing::info!(key, "link hint");
				crate::play_notice::push(play_id, crate::i18n::t(key));
			}
		});
}

#[cfg(windows)]
fn hints() -> Vec<&'static str> {
	let mut out = Vec::new();
	let Some(wlan) = run("netsh", &["wlan", "show", "interfaces"]) else {
		return out;
	};
	let Some(band) = wifi_band_ghz(&wlan) else {
		return out; // not on Wi-Fi
	};
	if band < 3.0 {
		out.push("hint.wifi24");
	}
	if on_battery() {
		let q = run(
			"powercfg",
			&[
				"/query",
				"SCHEME_CURRENT",
				"19cbb8fa-5279-450e-9fac-8a3d5fedd0c1",
				"12bbebe6-58d6-4636-95bb-3217ef867c1a",
			],
		);
		if q.as_deref().and_then(dc_index).is_some_and(|i| i >= 1) {
			out.push("hint.wlanPowerSave");
		}
	}
	out
}

#[cfg(not(windows))]
fn hints() -> Vec<&'static str> {
	Vec::new()
}

#[cfg(windows)]
fn run(exe: &str, args: &[&str]) -> Option<String> {
	let mut cmd = std::process::Command::new(exe);
	cmd.args(args);
	crate::process::no_window(&mut cmd);
	let out = cmd.output().ok()?;
	out.status
		.success()
		.then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(windows)]
fn on_battery() -> bool {
	use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
	let mut s: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
	// ACLineStatus: 0 = offline (battery), 1 = online, 255 = unknown.
	unsafe { GetSystemPowerStatus(&mut s) != 0 && s.ACLineStatus == 0 }
}

/// The connected Wi-Fi band in GHz from `netsh wlan show interfaces` (labels are
/// localised, so match the value: "2.4 GHz", "2,4 GHz", "5 GHz", "6 GHz").
#[cfg_attr(not(windows), allow(dead_code))]
fn wifi_band_ghz(netsh: &str) -> Option<f32> {
	netsh.lines().find_map(|l| {
		let (_, v) = l.split_once(':')?;
		let n = v.trim().strip_suffix("GHz")?.trim().replace(',', ".");
		n.parse().ok()
	})
}

/// The DC (battery) index from `powercfg /query <scheme> <sub> <setting>`: the last of the
/// two "Current … Power Setting Index: 0x…" lines (AC first, then DC; labels localised).
#[cfg_attr(not(windows), allow(dead_code))]
fn dc_index(powercfg: &str) -> Option<u32> {
	powercfg
		.lines()
		.filter_map(|l| {
			let h = l.trim().rsplit_once("0x")?.1;
			u32::from_str_radix(h.trim(), 16).ok()
		})
		.last()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_band_in_any_locale() {
		let en = "    State                  : connected\n    Band                   : 2.4 GHz\n    Channel : 6\n";
		assert_eq!(wifi_band_ghz(en), Some(2.4));
		let tr = "    Durum : bağlı\n    Bant  : 2,4 GHz\n";
		assert_eq!(wifi_band_ghz(tr), Some(2.4));
		assert_eq!(wifi_band_ghz("    Band : 6 GHz\n"), Some(6.0));
		assert_eq!(
			wifi_band_ghz("There is no wireless interface on the system.\n"),
			None
		);
	}

	#[test]
	fn parses_dc_power_saving_index() {
		let q = "      Possible Setting Index: 003\n      Possible Setting Friendly Name: Maximum Power Saving\n    Current AC Power Setting Index: 0x00000000\n    Current DC Power Setting Index: 0x00000002\n";
		assert_eq!(dc_index(q), Some(2));
		assert_eq!(dc_index("nothing"), None);
	}
}
