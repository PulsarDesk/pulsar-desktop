//! The Unix syscalls behind [`crate`]'s rules (Linux + macOS).

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::{Meta, Os, PrivError};

fn os() -> Os {
	Os::current().unwrap_or(Os::Linux)
}

/// The machine dir (`/var/lib/pulsar` · `/Library/Application Support/Pulsar`).
pub fn machine_dir() -> PathBuf {
	PathBuf::from(crate::machine_dir_for(os()))
}

/// The admin-group-only shared dir.
pub fn shared_dir() -> PathBuf {
	machine_dir().join(crate::SHARED_SUBDIR)
}

/// `lstat` → [`Meta`].
pub fn meta(p: &Path) -> io::Result<Meta> {
	let m = std::fs::symlink_metadata(p)?;
	let ft = m.file_type();
	Ok(Meta {
		is_symlink: ft.is_symlink(),
		is_dir: ft.is_dir(),
		is_file: ft.is_file(),
		uid: m.uid(),
		gid: m.gid(),
		mode: m.mode() & 0o7777,
	})
}

pub fn euid() -> u32 {
	// SAFETY: no arguments, cannot fail.
	unsafe { libc::geteuid() }
}

/// The effective gid + supplementary groups of this process.
pub fn my_gids() -> Vec<u32> {
	// SAFETY: getgroups(0, NULL) returns the count; the second call fills a buffer of that
	// size (+ slack in case a group is added in between, the result is truncated to `n`).
	let mut out = vec![unsafe { libc::getegid() }];
	unsafe {
		let n = libc::getgroups(0, std::ptr::null_mut());
		if n > 0 {
			let mut buf = vec![0 as libc::gid_t; n as usize + 8];
			let n = libc::getgroups(buf.len() as libc::c_int, buf.as_mut_ptr());
			if n > 0 {
				buf.truncate(n as usize);
				out.extend(buf.into_iter().map(|g| g as u32));
			}
		}
	}
	out
}

/// gid of a group by name (`getgrnam_r`).
pub fn group_gid(name: &str) -> Option<u32> {
	let c = CString::new(name).ok()?;
	let mut grp: libc::group = unsafe { std::mem::zeroed() };
	let mut res: *mut libc::group = std::ptr::null_mut();
	let mut buf = vec![0 as libc::c_char; 16 * 1024];
	// SAFETY: every pointer is valid for the call; `res` points at `grp` on success.
	let rc = unsafe {
		libc::getgrnam_r(
			c.as_ptr(),
			&mut grp,
			buf.as_mut_ptr(),
			buf.len(),
			&mut res,
		)
	};
	(rc == 0 && !res.is_null()).then_some(grp.gr_gid as u32)
}

/// The admin groups that exist on this machine, in preference order.
pub fn admin_groups() -> Vec<(String, u32)> {
	crate::admin_group_candidates(os())
		.iter()
		.filter_map(|n| group_gid(n).map(|g| (n.to_string(), g)))
		.collect()
}

/// The Unix "elevated": root, or a member of an admin group.
pub fn is_admin() -> bool {
	if euid() == 0 {
		return true;
	}
	let mine = my_gids();
	admin_groups().iter().any(|(_, g)| mine.contains(g))
}

/// The group the setup should give the shared dir.
pub fn setup_group() -> Option<(String, u32)> {
	crate::pick_group(&admin_groups(), &my_gids())
}

/// Shared mode is usable by this process: both dirs pass the ownership / mode rules and this
/// process can read + write the shared dir (root, or in its admin group). Never creates
/// anything — only the privileged setup does.
pub fn check_access() -> Result<(), String> {
	let m = machine_dir();
	let mm = meta(&m).map_err(|e| format!("{}: {e}", m.display()))?;
	crate::check_machine_dir(&mm)?;
	let s = shared_dir();
	let sm = meta(&s).map_err(|e| format!("{}: {e}", s.display()))?;
	let gids: Vec<u32> = admin_groups().into_iter().map(|(_, g)| g).collect();
	crate::check_shared_dir(&sm, &gids)?;
	if euid() != 0 && !my_gids().contains(&sm.gid) {
		return Err(format!("not a member of the shared dir's group {}", sm.gid));
	}
	let c = CString::new(s.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
	// SAFETY: valid NUL-terminated path.
	if unsafe { libc::access(c.as_ptr(), libc::R_OK | libc::W_OK | libc::X_OK) } != 0 {
		return Err(format!("{}: {}", s.display(), io::Error::last_os_error()));
	}
	std::fs::read_dir(&s).map(|_| ()).map_err(|e| e.to_string())
}

/// `mode.json` as text (world-readable; `None` when absent or behind a bad machine dir).
pub fn read_marker_raw() -> Option<String> {
	let m = machine_dir();
	crate::check_machine_dir(&meta(&m).ok()?).ok()?;
	let f = m.join("mode.json");
	let fm = meta(&f).ok()?;
	// Only root writes it; a symlink or a user-owned file is ignored.
	if fm.is_symlink || !fm.is_file || fm.uid != 0 {
		return None;
	}
	std::fs::read_to_string(f).ok()
}

/// Give one shared file the shared mode (0660) when we own it. Never follows a symlink.
pub fn harden(path: &Path) {
	// Open with O_NOFOLLOW and decide on the fd's own fstat, then fchmod that fd: a path
	// swapped for a symlink between a check and a chmod can never redirect the chmod.
	let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
		return;
	};
	// SAFETY: valid path; the fd is closed by `OwnedFd`.
	let fd = unsafe {
		libc::open(
			c.as_ptr(),
			libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
		)
	};
	if fd < 0 {
		return;
	}
	let file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
	let Ok(m) = file.metadata() else {
		return;
	};
	let meta = Meta {
		is_symlink: false,
		is_dir: m.is_dir(),
		is_file: m.is_file(),
		uid: m.uid(),
		gid: m.gid(),
		mode: m.mode() & 0o7777,
	};
	if crate::needs_harden(&meta, euid()) {
		// SAFETY: valid fd.
		unsafe { libc::fchmod(file.as_raw_fd(), crate::SHARED_FILE_MODE as libc::mode_t) };
	}
}

/// [`harden`] every file in `dir` (one level; the shared dir is flat).
pub fn harden_dir(dir: &Path) {
	if let Ok(rd) = std::fs::read_dir(dir) {
		for e in rd.flatten() {
			harden(&e.path());
		}
	}
}

/// The machine-wide host lock: `flock(LOCK_EX)` on `<dir>/host.lock`. A lock belongs to the
/// open file description, so it is released when the holder exits or crashes — the Unix
/// equivalent of an abandoned Win32 mutex.
pub struct HostLock(OwnedFd);

impl HostLock {
	pub fn create(dir: &Path) -> io::Result<Self> {
		let p = dir.join(crate::LOCK_FILE);
		let c = CString::new(p.as_os_str().as_bytes()).map_err(io::Error::other)?;
		// Read-only is enough for flock; O_NOFOLLOW refuses a planted symlink.
		// SAFETY: valid path; the fd is owned by `OwnedFd` from here on.
		let fd = unsafe {
			libc::open(
				c.as_ptr(),
				libc::O_RDONLY | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
				crate::SHARED_FILE_MODE as libc::c_uint,
			)
		};
		if fd < 0 {
			return Err(io::Error::last_os_error());
		}
		let fd = unsafe { OwnedFd::from_raw_fd(fd) };
		harden(&p);
		Ok(Self(fd))
	}

	/// Non-blocking.
	pub fn try_acquire(&self) -> bool {
		// SAFETY: valid fd.
		unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
	}

	pub fn release(&self) {
		// SAFETY: valid fd.
		unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
	}
}

/// The login user's name (holder display).
pub fn user_name() -> String {
	if let Ok(u) = std::env::var("USER") {
		if !u.is_empty() {
			return u;
		}
	}
	let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
	let mut res: *mut libc::passwd = std::ptr::null_mut();
	let mut buf = vec![0 as libc::c_char; 16 * 1024];
	// SAFETY: every pointer is valid for the call.
	let rc = unsafe { libc::getpwuid_r(euid(), &mut pw, buf.as_mut_ptr(), buf.len(), &mut res) };
	if rc == 0 && !res.is_null() && !pw.pw_name.is_null() {
		return unsafe { CStr::from_ptr(pw.pw_name) }.to_string_lossy().into_owned();
	}
	euid().to_string()
}

/// This user's per-user Pulsar config dir (same path Tauri resolves).
pub fn user_config_dir(app_id: &str) -> Option<PathBuf> {
	let home = std::env::var("HOME").ok();
	let xdg = std::env::var("XDG_CONFIG_HOME").ok();
	crate::user_config_dir(os(), home.as_deref(), xdg.as_deref(), app_id)
}

/// Our login session is the active one on its seat (not switched away from with fast user
/// switching). A locked screen still counts as active, like a locked Windows session. When
/// it cannot be told, `true` (never drop a working host over a failed query).
pub fn session_active() -> bool {
	#[cfg(target_os = "macos")]
	{
		macos::on_console().unwrap_or(true)
	}
	#[cfg(not(target_os = "macos"))]
	{
		// logind: XDG_SESSION_ID names our session; `auto` lets loginctl find it.
		let id = std::env::var("XDG_SESSION_ID")
			.ok()
			.filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric()))
			.unwrap_or_else(|| "auto".into());
		std::process::Command::new("loginctl")
			.args(["show-session", &id, "-p", "Active", "--value"])
			.stdin(std::process::Stdio::null())
			.stderr(std::process::Stdio::null())
			.output()
			.ok()
			.filter(|o| o.status.success())
			.and_then(|o| crate::parse_loginctl_active(&String::from_utf8_lossy(&o.stdout)))
			.unwrap_or(true)
	}
}

/// Run the setup script as root behind the system's own password prompt (polkit `pkexec`
/// on Linux, the macOS administrator dialog via `osascript`). Blocks until it is answered.
pub fn run_setup(enable: bool) -> Result<(), PrivError> {
	let (name, _) = setup_group().ok_or_else(|| PrivError::Failed("no admin group (sudo/wheel/admin) on this system".into()))?;
	let script = crate::setup_script(os(), &name, enable)
		.ok_or_else(|| PrivError::Failed(format!("unsafe group name {name}")))?;
	#[cfg(target_os = "macos")]
	let out = std::process::Command::new("/usr/bin/osascript")
		.arg("-e")
		.arg(format!(
			"do shell script {} with administrator privileges",
			crate::applescript_quote(&script)
		))
		.stdin(std::process::Stdio::null())
		.output();
	#[cfg(not(target_os = "macos"))]
	let out = std::process::Command::new("pkexec")
		.args(["/bin/sh", "-c", &script])
		.stdin(std::process::Stdio::null())
		.output();
	let out = out.map_err(|e| PrivError::Unavailable(e.to_string()))?;
	let stderr = String::from_utf8_lossy(&out.stderr);
	#[cfg(target_os = "macos")]
	return crate::classify_osascript(out.status.code(), &stderr);
	#[cfg(not(target_os = "macos"))]
	crate::classify_pkexec(out.status.code(), &stderr)
}

#[cfg(target_os = "macos")]
mod macos {
	use std::ffi::c_void;
	use std::os::raw::c_char;

	type CFTypeRef = *const c_void;
	const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

	#[link(name = "CoreFoundation", kind = "framework")]
	extern "C" {
		fn CFStringCreateWithCString(alloc: CFTypeRef, s: *const c_char, enc: u32) -> CFTypeRef;
		fn CFDictionaryGetValue(d: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
		fn CFGetTypeID(cf: CFTypeRef) -> usize;
		fn CFBooleanGetTypeID() -> usize;
		fn CFBooleanGetValue(b: CFTypeRef) -> u8;
		fn CFRelease(cf: CFTypeRef);
	}

	#[link(name = "CoreGraphics", kind = "framework")]
	extern "C" {
		fn CGSessionCopyCurrentDictionary() -> CFTypeRef;
	}

	/// `kCGSessionOnConsoleKey` of our window-server session: false after fast user
	/// switching to another account; a locked screen keeps it true.
	pub(super) fn on_console() -> Option<bool> {
		// SAFETY: CF create/copy results are released; the value is borrowed from the dict.
		unsafe {
			let dict = CGSessionCopyCurrentDictionary();
			if dict.is_null() {
				return None;
			}
			let key = CFStringCreateWithCString(
				std::ptr::null(),
				c"kCGSSessionOnConsoleKey".as_ptr(),
				K_CF_STRING_ENCODING_UTF8,
			);
			let mut out = None;
			if !key.is_null() {
				let v = CFDictionaryGetValue(dict, key);
				if !v.is_null() && CFGetTypeID(v) == CFBooleanGetTypeID() {
					out = Some(CFBooleanGetValue(v) != 0);
				}
				CFRelease(key);
			}
			CFRelease(dict);
			out
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::os::unix::fs::PermissionsExt;

	fn tmp(name: &str) -> PathBuf {
		let d = std::env::temp_dir().join(format!("pulsar-machine-{name}-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&d);
		std::fs::create_dir_all(&d).unwrap();
		d
	}

	#[test]
	fn host_lock_is_exclusive_and_released() {
		let d = tmp("lock");
		let a = HostLock::create(&d).unwrap();
		let b = HostLock::create(&d).unwrap();
		assert!(a.try_acquire());
		assert!(!b.try_acquire(), "a second holder is refused");
		a.release();
		assert!(b.try_acquire(), "released → the other one takes it");
		drop(b);
		assert!(a.try_acquire(), "a closed holder releases (crash/exit)");
		let m = meta(&d.join(crate::LOCK_FILE)).unwrap();
		assert_eq!(m.mode & 0o777, crate::SHARED_FILE_MODE);
		let _ = std::fs::remove_dir_all(&d);
	}

	#[test]
	fn host_lock_refuses_a_symlink() {
		let d = tmp("locklink");
		std::os::unix::fs::symlink("/etc/passwd", d.join(crate::LOCK_FILE)).unwrap();
		assert!(HostLock::create(&d).is_err());
		let _ = std::fs::remove_dir_all(&d);
	}

	#[test]
	fn harden_sets_0660_and_skips_symlinks() {
		let d = tmp("harden");
		let f = d.join("identity.key");
		std::fs::write(&f, [0u8; 32]).unwrap();
		std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
		harden_dir(&d);
		assert_eq!(meta(&f).unwrap().mode & 0o777, 0o660);
		// A symlink in the dir is never followed to a file elsewhere.
		let other = tmp("harden-other");
		let t2 = other.join("secret");
		std::fs::write(&t2, b"x").unwrap();
		std::fs::set_permissions(&t2, std::fs::Permissions::from_mode(0o600)).unwrap();
		std::os::unix::fs::symlink(&t2, d.join("link2.json")).unwrap();
		harden_dir(&d);
		assert_eq!(meta(&t2).unwrap().mode & 0o777, 0o600, "symlink target untouched");
		let _ = std::fs::remove_dir_all(&d);
		let _ = std::fs::remove_dir_all(&other);
	}

	/// Runs the real setup script on a scratch machine dir. Needs root (chown 0:group), so it
	/// is a no-op otherwise; run it once with `sudo -E cargo test -p pulsar-machine`.
	#[test]
	fn setup_script_end_to_end_as_root() {
		if euid() != 0 {
			return;
		}
		let base = tmp("setup");
		let m = base.join("machine");
		let ms = m.to_str().unwrap().to_string();
		let group = if group_gid("wheel").is_some() { "wheel" } else { "root" };
		let gid = group_gid(group).unwrap();
		let run = |enable: bool| {
			let s = crate::setup_script_at(os(), &ms, group, enable).unwrap();
			std::process::Command::new("/bin/sh").args(["-c", &s]).output().unwrap()
		};
		let o = run(true);
		assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
		crate::check_machine_dir(&meta(&m).unwrap()).unwrap();
		crate::check_shared_dir(&meta(&m.join("shared")).unwrap(), &[gid]).unwrap();
		assert_eq!(meta(&m.join("shared")).unwrap().mode, crate::SHARED_DIR_MODE);
		let mj = m.join("mode.json");
		assert_eq!(std::fs::read_to_string(&mj).unwrap().trim(), r#"{"shared":true}"#);
		assert_eq!(meta(&mj).unwrap().mode & 0o777, 0o644);
		// A file left 0600 in the shared dir is repaired on the next run.
		let key = m.join("shared").join("identity.key");
		std::fs::write(&key, [0u8; 32]).unwrap();
		std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
		// Turning it off rewrites the marker, keeps the shared dir.
		assert!(run(false).status.success());
		let km = meta(&key).unwrap();
		assert_eq!((km.mode & 0o777, km.gid), (0o660, gid));
		assert_eq!(std::fs::read_to_string(&mj).unwrap().trim(), r#"{"shared":false}"#);
		assert!(m.join("shared").is_dir());
		// A user-owned shared dir is refused (exit 4) — never re-owned.
		std::os::unix::fs::chown(m.join("shared"), Some(12345), None).unwrap();
		assert_eq!(run(true).status.code(), Some(4));
		// A symlinked machine dir is refused (exit 3).
		let _ = std::fs::remove_dir_all(&m);
		std::os::unix::fs::symlink(&base, &m).unwrap();
		assert_eq!(run(true).status.code(), Some(3));
		let _ = std::fs::remove_dir_all(&base);
	}

	#[test]
	fn own_gids_include_the_effective_one() {
		assert!(my_gids().contains(&unsafe { libc::getegid() }));
		assert!(group_gid("root").is_some() || group_gid("wheel").is_some());
	}
}
