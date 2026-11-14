//! Texts for a virtual controller the host could not create (`DataMsg::PadUnavailable`):
//! the client's in-session notice ("Player N's controller does nothing on the host: …")
//! and the host's toast. The cause is a `PadFailure` wire token; an unknown token (a newer
//! host) falls back to the generic text. Strings live in `lang/{tr,en}.json` (`pad.*`).

use pulsar_core::input::PadFailure;

fn reason_key(f: PadFailure, host: bool) -> &'static str {
	match (f, host) {
		(PadFailure::MacEntitlement, false) => "pad.client.mac_entitlement",
		(PadFailure::MacUnsupported, false) => "pad.client.mac_unsupported",
		(PadFailure::VigemMissing, false) => "pad.client.vigem_missing",
		(PadFailure::UinputDenied, false) => "pad.client.uinput_denied",
		(PadFailure::Other, false) => "pad.client.unavailable",
		(PadFailure::MacEntitlement, true) => "pad.host.mac_entitlement",
		(PadFailure::MacUnsupported, true) => "pad.host.mac_unsupported",
		(PadFailure::VigemMissing, true) => "pad.host.vigem_missing",
		(PadFailure::UinputDenied, true) => "pad.host.uinput_denied",
		(PadFailure::Other, true) => "pad.host.unavailable",
	}
}

/// The host's toast line (active language).
pub(crate) fn host_text(f: PadFailure) -> String {
	crate::i18n::t(reason_key(f, true)).to_string()
}

/// The client's notice for the pad at `slot` (0-based) the host reported with `reason`.
pub(crate) fn client_text(slot: u8, reason: &str, english: bool) -> String {
	let title = crate::i18n::t_in("pad.client.title", english).replace("{n}", &(slot as u32 + 1).to_string());
	let why = crate::i18n::t_in(reason_key(PadFailure::from_wire(reason), false), english);
	format!("{title}: {why}")
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn every_reason_has_both_languages() {
		for f in [
			PadFailure::MacEntitlement,
			PadFailure::MacUnsupported,
			PadFailure::VigemMissing,
			PadFailure::UinputDenied,
			PadFailure::Other,
		] {
			for host in [false, true] {
				let k = reason_key(f, host);
				assert_ne!(crate::i18n::t_in(k, false), k, "tr missing {k}");
				assert_ne!(crate::i18n::t_in(k, true), crate::i18n::t_in(k, false), "en missing {k}");
			}
		}
	}

	#[test]
	fn client_text_numbers_players_from_one_and_tolerates_unknown_reasons() {
		let s = client_text(0, "mac_entitlement", false);
		assert!(s.contains('1'), "{s}");
		assert!(s.ends_with(crate::i18n::t_in("pad.client.mac_entitlement", false)), "{s}");
		let s = client_text(3, "from_the_future", true);
		assert!(s.contains('4'), "{s}");
		assert!(s.ends_with(crate::i18n::t_in("pad.client.unavailable", true)), "{s}");
	}
}
