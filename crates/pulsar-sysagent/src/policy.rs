//! Pure policy decisions used by the service / helper / installer, split out so they are
//! unit-tested on every platform.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Restart delay after `failures` consecutive quick helper crashes: 1 s, 2 s, 4 s … 60 s.
pub fn backoff(failures: u32) -> Duration {
	Duration::from_secs(1u64.checked_shl(failures.min(6)).unwrap_or(64).min(60))
}

/// A helper that ran at least this long before exiting is "healthy": the failure counter resets.
pub const HEALTHY_RUN: Duration = Duration::from_secs(30);

/// `WTS_CONNECTSTATE_CLASS` values that keep a helper alive (the user is logged on):
/// Active (0), Connected (1), Disconnected (4). Session 0 (services) never gets one.
pub fn wants_helper(session: u32, state: i32) -> bool {
	session != 0 && matches!(state, 0 | 1 | 4)
}

/// Quote one argument for a Windows command line (CommandLineToArgvW rules).
pub fn quote_arg(arg: &str) -> String {
	if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
		return arg.to_string();
	}
	let mut out = String::from('"');
	let mut backslashes = 0;
	for ch in arg.chars() {
		match ch {
			'\\' => backslashes += 1,
			'"' => {
				out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
				out.push('"');
				backslashes = 0;
			}
			c => {
				out.extend(std::iter::repeat_n('\\', backslashes));
				out.push(c);
				backslashes = 0;
			}
		}
	}
	out.extend(std::iter::repeat_n('\\', backslashes * 2));
	out.push('"');
	out
}

fn norm(p: &Path) -> String {
	let s = p.to_string_lossy().replace('/', "\\");
	let s = s.strip_prefix(r"\\?\").unwrap_or(&s).to_string();
	s.trim_end_matches('\\').to_ascii_lowercase()
}

/// `path` is `dir` itself or inside it (case-insensitive, Windows separators).
pub fn is_within(path: &Path, dir: &Path) -> bool {
	let (p, d) = (norm(path), norm(dir));
	!d.is_empty() && (p == d || p.starts_with(&(d + "\\")))
}

/// The service binary must live somewhere only administrators can write (Program Files):
/// a LocalSystem service whose exe a user can replace is a privilege escalation.
pub fn is_protected_install_dir(exe: &Path, program_files: &[PathBuf]) -> bool {
	!norm(exe).contains("\\..") && program_files.iter().any(|pf| is_within(exe, pf))
}

/// Directories a pipe client's image must live in: the helper's own directory, its parent
/// (Tauri bundles resources under `$INSTDIR\resources`, the app exe sits in `$INSTDIR`), plus
/// any admin-configured `--client-dir`.
pub fn allowed_client_dirs(helper_exe: &Path, extra: &[PathBuf]) -> Vec<PathBuf> {
	let mut v = Vec::new();
	if let Some(d) = helper_exe.parent() {
		v.push(d.to_path_buf());
		if d.file_name()
			.is_some_and(|n| n.eq_ignore_ascii_case("resources"))
		{
			if let Some(p) = d.parent() {
				v.push(p.to_path_buf());
			}
		}
	}
	v.extend(extra.iter().cloned());
	v
}

/// Is a client image allowed? Only its DIRECT parent counts (no sub-directories), so a
/// writable sub-folder of the install dir cannot host an impostor.
pub fn client_image_allowed(image: &Path, allowed: &[PathBuf]) -> bool {
	let Some(dir) = image.parent() else {
		return false;
	};
	allowed.iter().any(|a| norm(a) == norm(dir))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn backoff_grows_and_caps() {
		assert_eq!(backoff(0), Duration::from_secs(1));
		assert_eq!(backoff(1), Duration::from_secs(2));
		assert_eq!(backoff(3), Duration::from_secs(8));
		assert_eq!(backoff(6), Duration::from_secs(60));
		assert_eq!(backoff(u32::MAX), Duration::from_secs(60));
	}

	#[test]
	fn session_states() {
		assert!(wants_helper(1, 0)); // active
		assert!(wants_helper(2, 4)); // disconnected, user still logged on
		assert!(!wants_helper(0, 0)); // services session
		assert!(!wants_helper(3, 6)); // listen
		assert!(!wants_helper(3, 8)); // down
	}

	#[test]
	fn quoting() {
		assert_eq!(quote_arg("helper"), "helper");
		assert_eq!(quote_arg(""), "\"\"");
		assert_eq!(
			quote_arg(r"C:\Program Files\Pulsar"),
			r#""C:\Program Files\Pulsar""#
		);
		assert_eq!(quote_arg(r"C:\a b\"), r#""C:\a b\\""#);
		assert_eq!(quote_arg(r#"a"b"#), r#""a\"b""#);
	}

	#[test]
	fn install_dir_must_be_admin_only() {
		let pf = vec![
			PathBuf::from(r"C:\Program Files"),
			PathBuf::from(r"C:\Program Files (x86)"),
		];
		assert!(is_protected_install_dir(
			Path::new(r"C:\Program Files\Pulsar\resources\pulsar-sysagent.exe"),
			&pf
		));
		assert!(is_protected_install_dir(
			Path::new(r"\\?\c:\program files\Pulsar\pulsar-sysagent.exe"),
			&pf
		));
		assert!(!is_protected_install_dir(
			Path::new(r"C:\Users\x\AppData\Local\Programs\Pulsar\pulsar-sysagent.exe"),
			&pf
		));
		assert!(!is_protected_install_dir(
			Path::new(r"C:\Program FilesEvil\pulsar-sysagent.exe"),
			&pf
		));
		assert!(!is_protected_install_dir(
			Path::new(r"C:\Program Files\..\Users\x\a.exe"),
			&pf
		));
	}

	#[test]
	fn client_dirs() {
		let helper = Path::new(r"C:\Program Files\Pulsar\resources\pulsar-sysagent.exe");
		let allowed = allowed_client_dirs(helper, &[]);
		assert!(client_image_allowed(
			Path::new(r"C:\Program Files\Pulsar\pulsar.exe"),
			&allowed
		));
		assert!(client_image_allowed(
			Path::new(r"c:\program files\pulsar\resources\x.exe"),
			&allowed
		));
		assert!(!client_image_allowed(
			Path::new(r"C:\Program Files\Pulsar\plugins\pulsar.exe"),
			&allowed
		));
		assert!(!client_image_allowed(
			Path::new(r"C:\Users\x\pulsar.exe"),
			&allowed
		));
		let dev = allowed_client_dirs(
			helper,
			&[PathBuf::from(r"C:\dev\pulsar-desktop\target\debug\")],
		);
		assert!(client_image_allowed(
			Path::new(r"C:\dev\pulsar-desktop\target\debug\pulsar.exe"),
			&dev
		));
	}
}
