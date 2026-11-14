//! Machine-wide ("shared mode", "Kullanıcılar arası ortak Pulsar") plumbing for Linux and
//! macOS. The Windows side lives in `src-tauri/src/shared_mode.rs`; this crate gives the Unix
//! side the same guarantees with the tools those systems have:
//!
//! | Windows                                   | Linux / macOS (this crate)                                     |
//! | ----------------------------------------- | -------------------------------------------------------------- |
//! | `%ProgramData%\Pulsar` (Users read)       | `/var/lib/pulsar` · `/Library/Application Support/Pulsar`, root, 0755 |
//! | `…\shared` SYSTEM + Administrators only   | `…/shared` root:`sudo`/`wheel`/`admin`, 2770, files 0660       |
//! | owner SYSTEM/Administrators, no reparse   | owner uid 0, `lstat` (no symlink), no group/world write on the machine dir |
//! | elevated token (UAC)                      | member of the admin group (or root)                            |
//! | DPAPI machine scope for saved passwords   | none — the admin-group-only file permissions are the protection |
//! | `Global\PulsarSharedHost` mutex           | `flock(LOCK_EX)` on `…/shared/host.lock`                       |
//! | WTS session Active                        | logind session `Active` · CGSession `kCGSSessionOnConsoleKey`  |
//! | elevated Pulsar writes the dirs + marker  | one `pkexec` / `osascript … with administrator privileges` run |
//!
//! Why admin-GROUP membership instead of "a root process": Pulsar's host captures the
//! user's graphical session (XDG portal / PipeWire, X11, ScreenCaptureKit with the user's
//! TCC grant), so it cannot run as root or as a system daemon (a systemd system unit or a
//! LaunchDaemon has no access to a user's Wayland session or a TCC screen-recording grant).
//! The admin group is the Unix analogue of "Administrators": its members can become root
//! anyway, a standard user cannot read the machine identity or rewrite the pairings.
//!
//! Everything that decides (paths, permission verdicts, the setup script, exit-code
//! classification) is pure and builds on every OS so it is unit-tested everywhere; the
//! syscalls are in [`sys`] (cfg(unix)).

/// The OS family this crate knows a machine dir for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
	Linux,
	MacOs,
}

impl Os {
	/// The running OS (`None` on Windows and other systems).
	pub const fn current() -> Option<Os> {
		if cfg!(target_os = "macos") {
			Some(Os::MacOs)
		} else if cfg!(all(unix, not(target_os = "macos"))) {
			Some(Os::Linux)
		} else {
			None
		}
	}
}

/// Machine dir on Linux (root-owned 0755; holds the world-readable `mode.json`).
pub const LINUX_MACHINE_DIR: &str = "/var/lib/pulsar";
/// Machine dir on macOS.
pub const MACOS_MACHINE_DIR: &str = "/Library/Application Support/Pulsar";
/// The admin-group-only subdir with the machine identity and the rest of the shared files.
pub const SHARED_SUBDIR: &str = "shared";
/// The lease lock file inside the shared dir.
pub const LOCK_FILE: &str = "host.lock";
/// Mode of every file in the shared dir: owner + admin group read/write, nobody else.
pub const SHARED_FILE_MODE: u32 = 0o660;
/// Mode of the shared dir: setgid (new files take the admin group on Linux; BSD semantics do
/// that anyway on macOS) + owner/group rwx, nothing for others.
pub const SHARED_DIR_MODE: u32 = 0o2770;
/// Mode of the machine dir: root writes, everyone reads `mode.json`.
pub const MACHINE_DIR_MODE: u32 = 0o755;

pub fn machine_dir_for(os: Os) -> &'static str {
	match os {
		Os::Linux => LINUX_MACHINE_DIR,
		Os::MacOs => MACOS_MACHINE_DIR,
	}
}

/// Groups whose members count as administrators, in order of preference. Debian/Ubuntu use
/// `sudo` (older Ubuntu `admin`), Fedora/Arch/openSUSE `wheel`; macOS `admin` (gid 80).
pub fn admin_group_candidates(os: Os) -> &'static [&'static str] {
	match os {
		Os::Linux => &["sudo", "wheel", "admin"],
		Os::MacOs => &["admin"],
	}
}

/// `lstat` facts about a path (built from `std::fs::symlink_metadata` on Unix).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
	pub is_symlink: bool,
	pub is_dir: bool,
	pub is_file: bool,
	pub uid: u32,
	pub gid: u32,
	/// Permission bits incl. setuid/setgid/sticky (`st_mode & 0o7777`).
	pub mode: u32,
}

/// The machine dir must be a real (non-symlink) dir owned by root that no group or other
/// user can write into: otherwise someone could swap `mode.json` or the `shared` subdir.
pub fn check_machine_dir(m: &Meta) -> Result<(), String> {
	if m.is_symlink {
		return Err("machine dir is a symlink".into());
	}
	if !m.is_dir {
		return Err("machine dir is not a directory".into());
	}
	if m.uid != 0 {
		return Err(format!("machine dir is owned by uid {}", m.uid));
	}
	if m.mode & 0o022 != 0 {
		return Err(format!("machine dir is writable by others (mode {:o})", m.mode & 0o7777));
	}
	Ok(())
}

/// The shared dir must be a real dir owned by root, group = one of the admin groups, with no
/// access at all for others (the machine identity and saved device passwords are in it).
pub fn check_shared_dir(m: &Meta, admin_gids: &[u32]) -> Result<(), String> {
	if m.is_symlink {
		return Err("shared dir is a symlink".into());
	}
	if !m.is_dir {
		return Err("shared dir is not a directory".into());
	}
	if m.uid != 0 {
		return Err(format!("shared dir is owned by uid {}", m.uid));
	}
	if !admin_gids.contains(&m.gid) {
		return Err(format!("shared dir group {} is not an admin group", m.gid));
	}
	if m.mode & 0o007 != 0 {
		return Err(format!("shared dir is open to other users (mode {:o})", m.mode & 0o7777));
	}
	if m.mode & 0o070 != 0o070 {
		return Err(format!("shared dir is not group-usable (mode {:o})", m.mode & 0o7777));
	}
	Ok(())
}

/// Whether a file in the shared dir needs its mode set to [`SHARED_FILE_MODE`]: a regular
/// file (never through a symlink) that we own (only the owner can chmod) and whose mode is
/// different — `identity.key` and `device_creds.json` are written 0600 and another admin's
/// Pulsar could not read them; anything with "other" bits must lose them.
pub fn needs_harden(m: &Meta, my_uid: u32) -> bool {
	!m.is_symlink && m.is_file && m.uid == my_uid && m.mode & 0o777 != SHARED_FILE_MODE
}

/// Which admin group the shared dir gets: the first candidate this user belongs to (so the
/// user who turns it on can use it), else the first one that exists (an admin password was
/// typed for a standard account: the setting still applies to the admins).
pub fn pick_group(existing: &[(String, u32)], my_gids: &[u32]) -> Option<(String, u32)> {
	existing
		.iter()
		.find(|(_, g)| my_gids.contains(g))
		.or_else(|| existing.first())
		.cloned()
}

/// A group name that is safe to splice into the setup script (POSIX portable name).
pub fn valid_group_name(s: &str) -> bool {
	let mut c = s.chars();
	matches!(c.next(), Some(ch) if ch.is_ascii_lowercase() || ch == '_')
		&& c.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
		&& s.len() <= 32
}

/// The `/bin/sh` script the privileged run executes (as root). It creates / verifies both
/// dirs with the same rules [`check_machine_dir`] / [`check_shared_dir`] apply — a path that
/// is a symlink or not a dir (exit 3), owned by someone else than root (exit 4) — re-applies
/// owner and mode, gives the regular files already in the shared dir the admin group + 0660
/// (e.g. an `identity.key` left 0600 by a Pulsar that exited before hardening it; `find
/// -type f` never matches a symlink), and atomically writes `mode.json`. `None` for an unsafe
/// group name.
pub fn setup_script(os: Os, group: &str, enable: bool) -> Option<String> {
	setup_script_at(os, machine_dir_for(os), group, enable)
}

/// [`setup_script`] for an explicit machine dir (tests run it on a scratch dir as root).
/// `m` must not contain a `'` (it is spliced into a single-quoted shell string).
pub fn setup_script_at(os: Os, m: &str, group: &str, enable: bool) -> Option<String> {
	if !valid_group_name(group) || m.contains('\'') || !m.starts_with('/') {
		return None;
	}
	let stat_uid = match os {
		Os::Linux => "stat -c %u",
		Os::MacOs => "stat -f %u",
	};
	let shared = if enable { "true" } else { "false" };
	Some(format!(
		"set -eu\n\
		 umask 022\n\
		 M='{m}'\n\
		 S=\"$M/{SHARED_SUBDIR}\"\n\
		 G='{group}'\n\
		 chk() {{ if [ -L \"$1\" ] || {{ [ -e \"$1\" ] && [ ! -d \"$1\" ]; }}; then echo \"$1: not a plain directory\" >&2; exit 3; fi; }}\n\
		 own() {{ o=$({stat_uid} \"$1\"); if [ \"$o\" != 0 ]; then echo \"$1: owned by uid $o\" >&2; exit 4; fi; }}\n\
		 chk \"$M\"; mkdir -p \"$M\"; chk \"$M\"; own \"$M\"\n\
		 chown 0:0 \"$M\"; chmod {MACHINE_DIR_MODE:o} \"$M\"\n\
		 chk \"$S\"; mkdir -p \"$S\"; chk \"$S\"; own \"$S\"\n\
		 chown 0:\"$G\" \"$S\"; chmod {SHARED_DIR_MODE:o} \"$S\"\n\
		 find \"$S\" -mindepth 1 -maxdepth 1 -type f -exec chgrp \"$G\" {{}} + -exec chmod {SHARED_FILE_MODE:o} {{}} +\n\
		 T=\"$M/mode.json.tmp\"; rm -f \"$T\"\n\
		 printf '{{\"shared\":{shared}}}\\n' > \"$T\"; chmod 644 \"$T\"; mv -f \"$T\" \"$M/mode.json\"\n"
	))
}

/// Quote `s` as an AppleScript string literal (for `do shell script "…"`).
pub fn applescript_quote(s: &str) -> String {
	let mut out = String::with_capacity(s.len() + 2);
	out.push('"');
	for ch in s.chars() {
		match ch {
			'\\' => out.push_str("\\\\"),
			'"' => out.push_str("\\\""),
			c => out.push(c),
		}
	}
	out.push('"');
	out
}

/// `loginctl show-session … -p Active --value` output → active?
pub fn parse_loginctl_active(out: &str) -> Option<bool> {
	match out.trim() {
		"yes" => Some(true),
		"no" => Some(false),
		_ => None,
	}
}

/// Why the privileged setup did not run / failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrivError {
	/// The user dismissed the password dialog.
	Cancelled,
	/// Authentication failed / the user is not allowed (polkit said no).
	Denied,
	/// No way to ask (no pkexec / osascript, no polkit agent).
	Unavailable(String),
	/// The script ran and refused (bad dir owner, symlink, …): its stderr.
	Failed(String),
}

impl std::fmt::Display for PrivError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			PrivError::Cancelled => write!(f, "cancelled"),
			PrivError::Denied => write!(f, "not authorized"),
			PrivError::Unavailable(s) => write!(f, "no privilege prompt available: {s}"),
			PrivError::Failed(s) => write!(f, "{s}"),
		}
	}
}

/// `pkexec` exit status → result (man pkexec: 126 = dialog dismissed, 127 = not authorized
/// or pkexec failed; anything else is the script's own status).
pub fn classify_pkexec(code: Option<i32>, stderr: &str) -> Result<(), PrivError> {
	match code {
		Some(0) => Ok(()),
		Some(126) => Err(PrivError::Cancelled),
		Some(127) if stderr.contains("No authentication agent") => {
			Err(PrivError::Unavailable(stderr.trim().to_string()))
		}
		Some(127) => Err(PrivError::Denied),
		_ => Err(PrivError::Failed(stderr.trim().to_string())),
	}
}

/// `osascript … with administrator privileges` exit status → result (-128 = user cancelled).
pub fn classify_osascript(code: Option<i32>, stderr: &str) -> Result<(), PrivError> {
	match code {
		Some(0) => Ok(()),
		_ if stderr.contains("(-128)") => Err(PrivError::Cancelled),
		_ if stderr.contains("(-60005)") || stderr.contains("(-60007)") => Err(PrivError::Denied),
		_ => Err(PrivError::Failed(stderr.trim().to_string())),
	}
}

/// The per-user app config dir Tauri resolves (`app_config_dir`), without an `AppHandle`:
/// Linux `$XDG_CONFIG_HOME/<id>` (absolute only) or `~/.config/<id>`; macOS
/// `~/Library/Application Support/<id>`.
pub fn user_config_dir(
	os: Os,
	home: Option<&str>,
	xdg_config_home: Option<&str>,
	app_id: &str,
) -> Option<std::path::PathBuf> {
	use std::path::PathBuf;
	match os {
		Os::Linux => match xdg_config_home.filter(|x| x.starts_with('/')) {
			Some(x) => Some(PathBuf::from(x).join(app_id)),
			None => home
				.filter(|h| !h.is_empty())
				.map(|h| PathBuf::from(h).join(".config").join(app_id)),
		},
		Os::MacOs => home.filter(|h| !h.is_empty()).map(|h| {
			PathBuf::from(h)
				.join("Library")
				.join("Application Support")
				.join(app_id)
		}),
	}
}

#[cfg(unix)]
pub mod sys;

#[cfg(test)]
mod tests {
	use super::*;

	fn dir(uid: u32, gid: u32, mode: u32) -> Meta {
		Meta {
			is_symlink: false,
			is_dir: true,
			is_file: false,
			uid,
			gid,
			mode,
		}
	}

	#[test]
	fn machine_dir_rules() {
		assert!(check_machine_dir(&dir(0, 0, 0o755)).is_ok());
		assert!(check_machine_dir(&dir(1000, 0, 0o755)).is_err(), "user-owned");
		assert!(check_machine_dir(&dir(0, 0, 0o775)).is_err(), "group-writable");
		assert!(check_machine_dir(&dir(0, 0, 0o757)).is_err(), "world-writable");
		let mut l = dir(0, 0, 0o755);
		l.is_symlink = true;
		assert!(check_machine_dir(&l).is_err(), "symlink");
		let mut f = dir(0, 0, 0o755);
		f.is_dir = false;
		assert!(check_machine_dir(&f).is_err(), "not a dir");
	}

	#[test]
	fn shared_dir_rules() {
		let admins = [27, 10];
		assert!(check_shared_dir(&dir(0, 27, 0o2770), &admins).is_ok());
		assert!(check_shared_dir(&dir(0, 10, 0o770), &admins).is_ok());
		assert!(check_shared_dir(&dir(0, 100, 0o2770), &admins).is_err(), "group `users`");
		assert!(check_shared_dir(&dir(0, 27, 0o2775), &admins).is_err(), "world-readable");
		assert!(check_shared_dir(&dir(0, 27, 0o2700), &admins).is_err(), "admins locked out");
		assert!(check_shared_dir(&dir(1000, 27, 0o2770), &admins).is_err(), "user-owned");
		let mut l = dir(0, 27, 0o2770);
		l.is_symlink = true;
		assert!(check_shared_dir(&l, &admins).is_err(), "symlink");
	}

	#[test]
	fn harden_only_own_regular_files_with_other_modes() {
		let f = |uid, mode, sym| Meta {
			is_symlink: sym,
			is_dir: false,
			is_file: !sym,
			uid,
			gid: 27,
			mode,
		};
		assert!(needs_harden(&f(1000, 0o600, false), 1000), "identity.key 0600");
		assert!(needs_harden(&f(1000, 0o644, false), 1000), "default umask");
		assert!(!needs_harden(&f(1000, 0o660, false), 1000), "already right");
		assert!(!needs_harden(&f(1001, 0o600, false), 1000), "someone else's file");
		assert!(!needs_harden(&f(1000, 0o600, true), 1000), "never through a symlink");
	}

	#[test]
	fn group_choice_prefers_ones_the_user_is_in() {
		let ex = vec![("sudo".to_string(), 27), ("wheel".to_string(), 10)];
		assert_eq!(pick_group(&ex, &[1000, 10]), Some(("wheel".into(), 10)));
		assert_eq!(pick_group(&ex, &[1000, 27]), Some(("sudo".into(), 27)));
		assert_eq!(pick_group(&ex, &[1000]), Some(("sudo".into(), 27)));
		assert_eq!(pick_group(&[], &[1000]), None);
	}

	#[test]
	fn group_names_are_validated_before_the_script() {
		assert!(valid_group_name("sudo"));
		assert!(valid_group_name("wheel"));
		assert!(valid_group_name("_admin-2"));
		assert!(!valid_group_name(""));
		assert!(!valid_group_name("a'b"));
		assert!(!valid_group_name("x;rm -rf /"));
		assert!(!valid_group_name("Admin"));
		assert!(setup_script(Os::Linux, "a b", true).is_none());
	}

	#[test]
	fn setup_script_linux() {
		let s = setup_script(Os::Linux, "sudo", true).unwrap();
		assert!(s.contains("M='/var/lib/pulsar'"));
		assert!(s.contains("G='sudo'"));
		assert!(s.contains("stat -c %u"));
		assert!(s.contains("chown 0:\"$G\" \"$S\"; chmod 2770 \"$S\""));
		assert!(s.contains("-type f -exec chgrp \"$G\" {} + -exec chmod 660 {} +"));
		assert!(s.contains("chmod 755 \"$M\""));
		assert!(s.contains(r#"printf '{"shared":true}\n'"#));
		assert!(s.contains("mv -f \"$T\" \"$M/mode.json\""));
		let off = setup_script(Os::Linux, "wheel", false).unwrap();
		assert!(off.contains(r#"{"shared":false}"#));
	}

	#[test]
	fn setup_script_macos() {
		let s = setup_script(Os::MacOs, "admin", true).unwrap();
		assert!(s.contains("M='/Library/Application Support/Pulsar'"));
		assert!(s.contains("stat -f %u"));
	}

	/// The script is valid `/bin/sh` (parse only: `sh -n`, nothing runs).
	#[cfg(unix)]
	#[test]
	fn setup_script_parses_in_sh() {
		for os in [Os::Linux, Os::MacOs] {
			for on in [true, false] {
				let s = setup_script(os, "admin", on).unwrap();
				let st = std::process::Command::new("/bin/sh")
					.args(["-n", "-c", &s])
					.status()
					.unwrap();
				assert!(st.success(), "sh -n rejected the {os:?} script:\n{s}");
			}
		}
	}

	#[test]
	fn applescript_quoting() {
		assert_eq!(applescript_quote(r#"a"b\c"#), r#""a\"b\\c""#);
		let s = setup_script(Os::MacOs, "admin", true).unwrap();
		let q = applescript_quote(&s);
		assert!(q.starts_with('"') && q.ends_with('"'));
		// Every inner quote is escaped.
		let inner = &q[1..q.len() - 1];
		let mut prev_backslashes = 0;
		for ch in inner.chars() {
			if ch == '"' {
				assert_eq!(prev_backslashes % 2, 1, "unescaped quote in {inner}");
			}
			prev_backslashes = if ch == '\\' { prev_backslashes + 1 } else { 0 };
		}
	}

	#[test]
	fn loginctl_parsing() {
		assert_eq!(parse_loginctl_active("yes\n"), Some(true));
		assert_eq!(parse_loginctl_active("no"), Some(false));
		assert_eq!(parse_loginctl_active(""), None);
	}

	#[test]
	fn privilege_exit_codes() {
		assert_eq!(classify_pkexec(Some(0), ""), Ok(()));
		assert_eq!(classify_pkexec(Some(126), ""), Err(PrivError::Cancelled));
		assert_eq!(classify_pkexec(Some(127), "Not authorized"), Err(PrivError::Denied));
		assert!(matches!(
			classify_pkexec(Some(127), "Error executing command as another user: No authentication agent found."),
			Err(PrivError::Unavailable(_))
		));
		assert_eq!(
			classify_pkexec(Some(4), "/var/lib/pulsar: owned by uid 1000\n"),
			Err(PrivError::Failed("/var/lib/pulsar: owned by uid 1000".into()))
		);
		assert_eq!(classify_osascript(Some(0), ""), Ok(()));
		assert_eq!(
			classify_osascript(Some(1), "0:10: execution error: User canceled. (-128)"),
			Err(PrivError::Cancelled)
		);
		assert!(matches!(classify_osascript(Some(1), "boom"), Err(PrivError::Failed(_))));
	}

	#[test]
	fn per_user_dirs_match_tauri() {
		let id = "dev.pulsar.app";
		assert_eq!(
			user_config_dir(Os::Linux, Some("/home/a"), None, id).unwrap(),
			std::path::PathBuf::from("/home/a/.config/dev.pulsar.app")
		);
		assert_eq!(
			user_config_dir(Os::Linux, Some("/home/a"), Some("/x/cfg"), id).unwrap(),
			std::path::PathBuf::from("/x/cfg/dev.pulsar.app")
		);
		// A relative XDG_CONFIG_HOME is ignored (XDG spec).
		assert_eq!(
			user_config_dir(Os::Linux, Some("/home/a"), Some("rel"), id).unwrap(),
			std::path::PathBuf::from("/home/a/.config/dev.pulsar.app")
		);
		assert_eq!(
			user_config_dir(Os::MacOs, Some("/Users/a"), None, id).unwrap(),
			std::path::PathBuf::from("/Users/a/Library/Application Support/dev.pulsar.app")
		);
		assert!(user_config_dir(Os::MacOs, None, None, id).is_none());
	}
}
