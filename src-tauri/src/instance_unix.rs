//! Linux / macOS transport for [`crate::instance`]: the per-uid launch socket.
//!
//! The first instance (it holds the single-instance flock) listens on a per-user Unix socket;
//! a second launch connects, sends one request line (`pulsar1 <json>`: show / background /
//! connect, with its XDG activation token or startup id so the compositor — KWin, Mutter:
//! xdg-activation on Wayland — accepts the raise) and exits once the running Pulsar answers
//! `ok`. Both ends check the other side's uid before a password crosses (the socket may sit
//! in a shared temp dir when there is no `XDG_RUNTIME_DIR`).

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::instance::{Forward, LaunchRequest};

/// Longest a second launch waits for the running Pulsar's socket (it may still be starting).
const WAIT_FOR_SERVER: Duration = Duration::from_secs(5);

fn socket_path() -> PathBuf {
	let uid = unsafe { libc::getuid() };
	let dir = std::env::var_os("XDG_RUNTIME_DIR")
		.map(PathBuf::from)
		.filter(|p| p.is_dir())
		.unwrap_or_else(std::env::temp_dir);
	dir.join(format!("pulsar-{uid}.sock"))
}

/// The token a desktop launcher handed this (second) process for focusing a window.
fn activation_token() -> String {
	std::env::var("XDG_ACTIVATION_TOKEN")
		.or_else(|_| std::env::var("DESKTOP_STARTUP_ID"))
		.unwrap_or_default()
}

/// The uid of the process at the other end of `s`.
fn peer_uid(s: &UnixStream) -> Option<u32> {
	let fd = s.as_raw_fd();
	#[cfg(target_os = "linux")]
	unsafe {
		let mut cred: libc::ucred = std::mem::zeroed();
		let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
		if libc::getsockopt(
			fd,
			libc::SOL_SOCKET,
			libc::SO_PEERCRED,
			(&mut cred as *mut libc::ucred).cast(),
			&mut len,
		) == 0
		{
			return Some(cred.uid);
		}
		None
	}
	#[cfg(not(target_os = "linux"))]
	unsafe {
		let mut uid: libc::uid_t = 0;
		let mut gid: libc::gid_t = 0;
		(libc::getpeereid(fd, &mut uid, &mut gid) == 0).then_some(uid)
	}
}

fn same_user(s: &UnixStream) -> bool {
	peer_uid(s) == Some(unsafe { libc::getuid() })
}

/// Send the request and wait (≤ 3 s) for `ok`.
fn send(req: &LaunchRequest) -> std::io::Result<bool> {
	let mut s = UnixStream::connect(socket_path())?;
	if !same_user(&s) && req.has_secret() {
		tracing::warn!("the launch socket belongs to another user; not sending the connect");
		return Ok(false);
	}
	s.set_write_timeout(Some(Duration::from_secs(1)))?;
	s.set_read_timeout(Some(Duration::from_secs(3)))?;
	s.write_all(req.to_line().as_bytes())?;
	let mut reply = [0u8; 3];
	let n = s.read(&mut reply).unwrap_or(0);
	// A pre-2026-11-26 Pulsar reads "show …" lines only and never answers: it did surface.
	Ok(n == 0 || reply[..n].starts_with(b"ok"))
}

/// Second launch (the flock is held by another process of this user): hand it the request.
pub(crate) fn forward(args: &[String]) -> Forward {
	let req = crate::instance::request_from_args(args, activation_token());
	let deadline = Instant::now() + WAIT_FOR_SERVER;
	loop {
		match send(&req) {
			Ok(true) => return Forward::Delivered,
			Ok(false) => return Forward::Unreachable,
			Err(e) if Instant::now() < deadline => {
				tracing::debug!("launch socket not ready yet: {e}");
				std::thread::sleep(Duration::from_millis(250));
			}
			Err(e) => {
				// Most likely the window-less host (`--headless-host`), which holds the lock
				// but serves no socket.
				tracing::warn!("couldn't reach the running Pulsar: {e}");
				return Forward::Headless;
			}
		}
	}
}

/// First instance: serve launch requests. Only called while holding the single-instance
/// lock, so a leftover socket file is always stale and safe to replace. Requests before
/// `setup` are queued (`instance::deliver`).
pub(crate) fn spawn_server() {
	let path = socket_path();
	let _ = std::fs::remove_file(&path);
	let listener = match UnixListener::bind(&path) {
		Ok(l) => l,
		Err(e) => {
			tracing::warn!(path = %path.display(), "single-instance socket bind failed: {e}");
			return;
		}
	};
	// Owner-only, also when it falls back to a shared temp dir.
	{
		use std::os::unix::fs::PermissionsExt;
		let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
	}
	std::thread::spawn(move || {
		for conn in listener.incoming() {
			let Ok(conn) = conn else { continue };
			if !same_user(&conn) {
				tracing::warn!("launch socket: a process of another user connected; ignored");
				continue;
			}
			let _ = conn.set_read_timeout(Some(Duration::from_secs(1)));
			let mut line = String::new();
			let mut reader = BufReader::new(&conn);
			let _ = reader.read_line(&mut line);
			let Some(req) = LaunchRequest::from_line(&line) else { continue };
			let _ = (&conn).write_all(b"ok\n");
			crate::instance::deliver(req);
		}
	});
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn peer_uid_of_a_socket_pair_is_ours() {
		let (a, _b) = UnixStream::pair().unwrap();
		assert_eq!(peer_uid(&a), Some(unsafe { libc::getuid() }));
		assert!(same_user(&a));
	}
}
