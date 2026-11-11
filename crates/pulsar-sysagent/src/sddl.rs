//! Security descriptor for the helper's named pipe, built as an SDDL string (pure +
//! testable; the helper turns it into a real descriptor with
//! `ConvertStringSecurityDescriptorToSecurityDescriptorW`).
//!
//! ```text
//! O:SYG:SY                        owner/group LocalSystem (a squatter cannot fake this)
//! D:P                             protected DACL (no inherited ACEs)
//!   (D;;GA;;;NU)                  deny NETWORK logons outright (belt + PIPE_REJECT_REMOTE_CLIENTS)
//!   (A;;GA;;;SY)                  LocalSystem: full control
//!   (A;;0x12019b;;;<user SID>)    the session's user: read + write DATA, but NOT
//!                                 FILE_APPEND_DATA (= FILE_CREATE_PIPE_INSTANCE), so the
//!                                 user cannot add a rogue server instance of this pipe
//! S:(ML;;NWNR;;;HI)               mandatory label: Medium-IL processes of the same user
//!                                 (non-elevated malware) cannot open it — only the
//!                                 elevated (High IL) Pulsar host can
//! ```
//!
//! No ACE for Administrators / Everyone / Authenticated Users: another interactive user on
//! a multi-seat machine gets ACCESS_DENIED at `CreateFile`.

/// `FILE_GENERIC_READ (0x120089) | FILE_WRITE_DATA (0x2) | FILE_WRITE_EA (0x10) |
/// FILE_WRITE_ATTRIBUTES (0x100)` — everything a pipe client needs, without
/// `FILE_APPEND_DATA` (0x4, which on a pipe means FILE_CREATE_PIPE_INSTANCE).
pub const USER_PIPE_ACCESS: u32 = 0x0012_019B;
/// What the client asks for at `CreateFile` (must be a subset of [`USER_PIPE_ACCESS`];
/// plain `GENERIC_WRITE` would map to FILE_GENERIC_WRITE and include FILE_APPEND_DATA →
/// ACCESS_DENIED).
pub const CLIENT_OPEN_ACCESS: u32 = 0x0012_0089 | 0x2;
const FILE_APPEND_DATA: u32 = 0x4;

/// Minimum integrity level a client process needs to open the pipe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MinIntegrity {
	/// Any process of the user (only for a standard, non-admin user whose Pulsar cannot be
	/// elevated — see the doc's security section before enabling).
	Medium,
	/// Elevated processes only (default).
	High,
}

impl MinIntegrity {
	fn sddl(self) -> &'static str {
		match self {
			MinIntegrity::Medium => "ME",
			MinIntegrity::High => "HI",
		}
	}

	/// The `SECURITY_MANDATORY_*_RID` a client token must reach.
	pub fn rid(self) -> u32 {
		match self {
			MinIntegrity::Medium => 0x2000,
			MinIntegrity::High => 0x3000,
		}
	}
}

#[derive(Debug, PartialEq, Eq)]
pub enum SddlError {
	/// Not a plain `S-1-…` string SID (rejects SDDL aliases like `WD`/`BA` and anything
	/// that could smuggle extra ACE syntax such as `)` or `;`).
	BadSid(String),
	/// A well-known group/system SID instead of a user account SID.
	NotAUserSid(String),
}

impl std::fmt::Display for SddlError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			SddlError::BadSid(s) => write!(f, "not a string SID: {s:?}"),
			SddlError::NotAUserSid(s) => write!(f, "not a user account SID: {s}"),
		}
	}
}

impl std::error::Error for SddlError {}

/// Strict string-SID check: `S-1-<authority>-<sub>…`, decimal components only, 1..=15
/// sub-authorities (Windows' SID_MAX_SUB_AUTHORITIES).
pub fn is_valid_sid_string(s: &str) -> bool {
	let mut parts = s.split('-');
	if parts.next() != Some("S") || parts.next() != Some("1") {
		return false;
	}
	let rest: Vec<&str> = parts.collect();
	if rest.len() < 2 || rest.len() > 16 {
		return false;
	}
	let decimal = |p: &str, max: u64| {
		!p.is_empty()
			&& p.len() <= 15
			&& p.bytes().all(|b| b.is_ascii_digit())
			&& p.parse::<u64>().is_ok_and(|v| v <= max)
	};
	// Identifier authority is 48-bit; every sub-authority is a u32.
	decimal(rest[0], (1 << 48) - 1) && rest[1..].iter().all(|p| decimal(p, u32::MAX as u64))
}

/// Account SIDs that must never be granted pipe access as "the session user": LocalSystem,
/// LocalService, NetworkService, and builtin/world/authority groups.
fn is_non_user_sid(s: &str) -> bool {
	const DENY: &[&str] = &[
		"S-1-5-18", "S-1-5-19", "S-1-5-20", "S-1-1-0", "S-1-5-11", "S-1-5-4", "S-1-5-2", "S-1-5-7",
	];
	DENY.contains(&s) || s.starts_with("S-1-5-32-")
}

/// Build the pipe SDDL for `user_sid` (the session's logged-on user).
pub fn pipe_sddl(user_sid: &str, min: MinIntegrity) -> Result<String, SddlError> {
	if !is_valid_sid_string(user_sid) {
		return Err(SddlError::BadSid(user_sid.to_string()));
	}
	if is_non_user_sid(user_sid) {
		return Err(SddlError::NotAUserSid(user_sid.to_string()));
	}
	debug_assert_eq!(USER_PIPE_ACCESS & FILE_APPEND_DATA, 0);
	Ok(format!(
		"O:SYG:SYD:P(D;;GA;;;NU)(A;;GA;;;SY)(A;;{:#x};;;{user_sid})S:(ML;;NWNR;;;{})",
		USER_PIPE_ACCESS,
		min.sddl()
	))
}

#[cfg(test)]
mod tests {
	use super::*;

	const USER: &str = "S-1-5-21-3623811015-3361044348-30300820-1013";

	#[test]
	fn sddl_shape() {
		let s = pipe_sddl(USER, MinIntegrity::High).unwrap();
		assert_eq!(
			s,
			"O:SYG:SYD:P(D;;GA;;;NU)(A;;GA;;;SY)(A;;0x12019b;;;S-1-5-21-3623811015-3361044348-30300820-1013)S:(ML;;NWNR;;;HI)"
		);
		let m = pipe_sddl(USER, MinIntegrity::Medium).unwrap();
		assert!(m.ends_with("S:(ML;;NWNR;;;ME)"));
	}

	#[test]
	fn sddl_grants_nobody_else() {
		let s = pipe_sddl(USER, MinIntegrity::High).unwrap();
		// Exactly three ACEs in the DACL: deny network, SYSTEM, the user.
		let dacl = &s[s.find("D:P").unwrap()..s.find("S:").unwrap()];
		assert_eq!(dacl.matches('(').count(), 3, "{dacl}");
		for alias in [";WD)", ";BA)", ";AU)", ";BU)", ";IU)", ";AN)"] {
			assert!(!s.contains(alias), "{alias} must not be granted: {s}");
		}
	}

	#[test]
	fn access_masks_exclude_create_pipe_instance() {
		assert_eq!(USER_PIPE_ACCESS & FILE_APPEND_DATA, 0);
		assert_eq!(CLIENT_OPEN_ACCESS & FILE_APPEND_DATA, 0);
		assert_eq!(
			CLIENT_OPEN_ACCESS & !USER_PIPE_ACCESS,
			0,
			"client asks only for granted bits"
		);
		assert_ne!(CLIENT_OPEN_ACCESS & 0x2, 0, "client can write data");
		assert_ne!(
			CLIENT_OPEN_ACCESS & 0x2_0000,
			0,
			"READ_CONTROL for the owner check"
		);
	}

	#[test]
	fn rejects_injection_and_aliases() {
		for bad in [
			"WD",
			"BA",
			"",
			"S-1",
			"S-1-5",
			"s-1-5-21-1-2-3-1001",
			"S-1-5-21-1-2-3-1001)(A;;GA;;;WD",
			"S-1-5-21-1-2-3-1001;",
			"S-1-5-21--1001",
			"S-1-5-21-1-2-3-x",
			"S-2-5-21-1-2-3-1001",
			"S-1-5-21-1-2-3-12345678901",
		] {
			assert!(
				matches!(
					pipe_sddl(bad, MinIntegrity::High),
					Err(SddlError::BadSid(_))
				),
				"{bad:?}"
			);
		}
	}

	#[test]
	fn rejects_service_and_group_sids() {
		for sid in [
			"S-1-5-18",
			"S-1-5-19",
			"S-1-5-20",
			"S-1-1-0",
			"S-1-5-11",
			"S-1-5-32-544",
			"S-1-5-4",
		] {
			assert_eq!(
				pipe_sddl(sid, MinIntegrity::High),
				Err(SddlError::NotAUserSid(sid.into())),
				"{sid}"
			);
		}
	}

	#[test]
	fn accepts_real_user_sids() {
		assert!(is_valid_sid_string(USER));
		assert!(is_valid_sid_string("S-1-5-21-1-2-3-1001"));
		// Microsoft account / Azure AD users.
		assert!(is_valid_sid_string(
			"S-1-12-1-1234567890-1234567890-1234567890-1234567890"
		));
		assert!(pipe_sddl(
			"S-1-12-1-1234567890-1234567890-1234567890-1234567890",
			MinIntegrity::High
		)
		.is_ok());
	}

	#[test]
	fn integrity_rids() {
		assert_eq!(MinIntegrity::High.rid(), 0x3000);
		assert_eq!(MinIntegrity::Medium.rid(), 0x2000);
	}

	/// The OS itself must accept the descriptor (parse only — creates nothing).
	#[cfg(windows)]
	#[test]
	fn windows_parses_the_sddl() {
		let s = pipe_sddl(USER, MinIntegrity::High).unwrap();
		let sd =
			crate::win::SecurityDescriptor::from_sddl(&s).expect("ConvertStringSecurityDescriptor");
		assert!(!sd.as_ptr().is_null());
		// And rejects garbage, so the test above is meaningful.
		assert!(crate::win::SecurityDescriptor::from_sddl("D:(Z;;;;;)").is_err());
	}
}
