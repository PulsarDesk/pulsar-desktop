//! Windows transport for [`crate::instance`]: the launch pipe.
//!
//! The running Pulsar serves `\\.\pipe\pulsar-launch-<session>-<user SID>`. Its security
//! descriptor allows only SYSTEM and this user, with a Medium label, so a normal (not
//! elevated) second launch reaches an elevated Pulsar: no UAC prompt just to show a window
//! or pass a `--connect`. The client checks that the server is this same Pulsar exe in this
//! same session before it sends a password (another user could create a pipe of that name
//! first; it could not make it run this exe under our SID).
//!
//! Wire: one line `pulsar1 <json LaunchRequest>\n` from the client, `ok\n` back.

use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
	CloseHandle, GetLastError, LocalFree, ERROR_ACCESS_DENIED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
	GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
	CreateFileW, FlushFileBuffers, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, OPEN_EXISTING,
	PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::Pipes::{
	ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeServerProcessId,
	PeekNamedPipe, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
	PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

use crate::instance::{Forward, LaunchRequest};

/// The GUI's single-instance mutex (`lib.rs`); the headless host takes it too.
pub(crate) const MUTEX_NAME: &str = "Local\\PulsarSingletonMutex";

/// Longest a second launch waits for a Pulsar that holds the lock but has no pipe yet (it is
/// still starting).
const WAIT_FOR_SERVER: Duration = Duration::from_secs(15);

/// Largest request line accepted.
const MAX_LINE: usize = 16 * 1024;

fn wide(s: &str) -> Vec<u16> {
	s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// This process's user SID as a string (`S-1-5-21-…`).
pub(crate) fn user_sid() -> Option<String> {
	use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
	use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
	use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
	unsafe {
		let mut tok: HANDLE = std::ptr::null_mut();
		if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) == 0 {
			return None;
		}
		let mut tbuf = [0u64; 64]; // u64: TOKEN_USER needs pointer alignment
		let mut need = 0u32;
		let ok = GetTokenInformation(
			tok,
			TokenUser,
			tbuf.as_mut_ptr() as _,
			std::mem::size_of_val(&tbuf) as u32,
			&mut need,
		) != 0;
		CloseHandle(tok);
		if !ok {
			return None;
		}
		let user = &*(tbuf.as_ptr() as *const TOKEN_USER);
		let mut s: windows_sys::core::PWSTR = std::ptr::null_mut();
		if ConvertSidToStringSidW(user.User.Sid, &mut s) == 0 {
			return None;
		}
		let mut n = 0;
		while *s.add(n) != 0 {
			n += 1;
		}
		let sid = String::from_utf16_lossy(std::slice::from_raw_parts(s, n));
		LocalFree(s as _);
		Some(sid)
	}
}

fn session_of(pid: u32) -> Option<u32> {
	use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
	let mut s = u32::MAX;
	(unsafe { ProcessIdToSessionId(pid, &mut s) } != 0).then_some(s)
}

fn own_session() -> u32 {
	session_of(std::process::id()).unwrap_or(0)
}

fn pipe_name(sid: &str) -> String {
	format!(r"\\.\pipe\pulsar-launch-{}-{sid}", own_session())
}

/// Does a named kernel object exist? `ACCESS_DENIED` counts: it exists, but belongs to a more
/// privileged (elevated) process of ours.
fn object_exists(open: impl FnOnce(*const u16) -> HANDLE, name: &str) -> bool {
	let n = wide(name);
	let h = open(n.as_ptr());
	if !h.is_null() {
		unsafe { CloseHandle(h) };
		return true;
	}
	unsafe { GetLastError() == ERROR_ACCESS_DENIED }
}

/// A Pulsar (GUI or window-less host) of this user runs in this session.
fn running() -> bool {
	use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
	use windows_sys::Win32::System::Threading::OpenMutexW;
	object_exists(|n| unsafe { OpenMutexW(SYNCHRONIZE, 0, n) }, MUTEX_NAME)
}

/// The running one is the window-less host (it alone creates its stop event).
fn headless_running() -> bool {
	use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
	use windows_sys::Win32::System::Threading::OpenEventW;
	object_exists(
		|n| unsafe { OpenEventW(SYNCHRONIZE, 0, n) },
		crate::headless_host::STOP_EVENT,
	)
}

/// The image path of `pid`, lowercased.
fn image_of(pid: u32) -> Option<String> {
	use windows_sys::Win32::System::Threading::{
		OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
	};
	unsafe {
		let p = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
		if p.is_null() {
			return None;
		}
		let mut buf = [0u16; 1024];
		let mut len = buf.len() as u32;
		let ok = QueryFullProcessImageNameW(p, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) != 0;
		CloseHandle(p);
		ok.then(|| String::from_utf16_lossy(&buf[..len as usize]).to_lowercase())
	}
}

/// The pipe's server is this exe, in this session.
fn server_is_us(pipe: HANDLE) -> bool {
	let mut pid = 0u32;
	if unsafe { GetNamedPipeServerProcessId(pipe, &mut pid) } == 0 {
		return false;
	}
	let Ok(me) = std::env::current_exe() else { return false };
	session_of(pid) == Some(own_session())
		&& image_of(pid).is_some_and(|img| img == me.to_string_lossy().to_lowercase())
}

enum SendErr {
	/// No pipe (yet) or every instance busy: try again shortly.
	Retry,
	/// The pipe is not this Pulsar's, or it broke mid-request.
	Fail(String),
}

/// Write the request and wait (≤ 3 s) for `ok`.
fn send(pipe: &str, req: &LaunchRequest) -> Result<(), SendErr> {
	let name = wide(pipe);
	unsafe {
		let h = CreateFileW(
			name.as_ptr(),
			GENERIC_READ | GENERIC_WRITE,
			0,
			std::ptr::null(),
			OPEN_EXISTING,
			// The server may only identify us, never act as us.
			SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
			std::ptr::null_mut(),
		);
		if h == INVALID_HANDLE_VALUE {
			let e = GetLastError();
			return Err(if e == ERROR_PIPE_BUSY || e == 2 {
				SendErr::Retry // 2 = ERROR_FILE_NOT_FOUND: no server yet
			} else {
				SendErr::Fail(format!("open: os error {e}"))
			});
		}
		let r = (|| {
			if !server_is_us(h) {
				if req.has_secret() {
					return Err(SendErr::Fail("the launch pipe is not this Pulsar's".into()));
				}
				tracing::warn!("launch pipe served by another Pulsar build; sending a plain show");
			}
			let line = req.to_line();
			let mut written = 0u32;
			if WriteFile(h, line.as_ptr(), line.len() as u32, &mut written, std::ptr::null_mut()) == 0
				|| written as usize != line.len()
			{
				return Err(SendErr::Fail("write failed".into()));
			}
			// Bounded wait for the reply: a wedged Pulsar must not hang this launch.
			let deadline = Instant::now() + Duration::from_secs(3);
			loop {
				let mut avail = 0u32;
				if PeekNamedPipe(h, std::ptr::null_mut(), 0, std::ptr::null_mut(), &mut avail, std::ptr::null_mut()) == 0 {
					return Err(SendErr::Fail("no reply (pipe closed)".into()));
				}
				if avail >= 3 {
					let mut buf = [0u8; 8];
					let mut got = 0u32;
					ReadFile(h, buf.as_mut_ptr(), buf.len() as u32, &mut got, std::ptr::null_mut());
					return if buf[..got as usize].starts_with(b"ok") {
						Ok(())
					} else {
						Err(SendErr::Fail("unexpected reply".into()))
					};
				}
				if Instant::now() > deadline {
					return Err(SendErr::Fail("no reply in 3 s".into()));
				}
				std::thread::sleep(Duration::from_millis(20));
			}
		})();
		CloseHandle(h);
		r
	}
}

/// Second launch: give this process's request to the running Pulsar of this user.
pub(crate) fn forward(args: &[String]) -> Forward {
	if !running() {
		return Forward::NotRunning;
	}
	if headless_running() {
		return Forward::Headless;
	}
	let Some(sid) = user_sid() else {
		return Forward::Unreachable;
	};
	let req = crate::instance::request_from_args(args, String::new());
	// Let the running Pulsar raise its window over ours (Windows' foreground lock).
	unsafe {
		windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(0xFFFF_FFFF);
	}
	let pipe = pipe_name(&sid);
	let deadline = Instant::now() + WAIT_FOR_SERVER;
	loop {
		match send(&pipe, &req) {
			Ok(()) => return Forward::Delivered,
			Err(SendErr::Fail(e)) => {
				tracing::warn!("couldn't hand the launch to the running Pulsar: {e}");
				return Forward::Unreachable;
			}
			Err(SendErr::Retry) if Instant::now() < deadline && running() => {
				std::thread::sleep(Duration::from_millis(250));
			}
			Err(SendErr::Retry) => {
				// It quit meanwhile: start normally after all.
				if !running() {
					return Forward::NotRunning;
				}
				tracing::warn!("the running Pulsar never opened its launch pipe");
				return Forward::Unreachable;
			}
		}
	}
}

/// `D:P` SYSTEM + this user, Medium label (no-write-up): a non-elevated launch of this user
/// may write to an elevated Pulsar's pipe; nobody else may open it.
fn pipe_sd(sid: &str) -> windows_sys::Win32::Security::PSECURITY_DESCRIPTOR {
	use windows_sys::Win32::Security::Authorization::{
		ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
	};
	let sddl = wide(&format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid})S:(ML;;NW;;;ME)"));
	let mut sd = std::ptr::null_mut();
	unsafe {
		if ConvertStringSecurityDescriptorToSecurityDescriptorW(
			sddl.as_ptr(),
			SDDL_REVISION_1,
			&mut sd,
			std::ptr::null_mut(),
		) == 0
		{
			return std::ptr::null_mut();
		}
	}
	sd
}

/// Read one request line from a connected client (blocking; this connection's own thread).
fn read_line(h: HANDLE) -> Option<String> {
	let mut buf = Vec::with_capacity(512);
	let mut chunk = [0u8; 512];
	while buf.len() < MAX_LINE {
		let mut got = 0u32;
		let ok = unsafe { ReadFile(h, chunk.as_mut_ptr(), chunk.len() as u32, &mut got, std::ptr::null_mut()) };
		if ok == 0 && got == 0 {
			break;
		}
		buf.extend_from_slice(&chunk[..got as usize]);
		if buf.contains(&b'\n') {
			break;
		}
	}
	let end = buf.iter().position(|&b| b == b'\n')?;
	String::from_utf8(buf[..end].to_vec()).ok()
}

/// First instance: serve the launch pipe for the life of the process. Called right after the
/// single-instance lock is taken; requests before `setup` are queued (`instance::deliver`).
pub(crate) fn spawn_server() {
	let Some(sid) = user_sid() else {
		tracing::warn!("launch pipe: no user SID; later launches cannot reach this Pulsar");
		return;
	};
	serve(pipe_name(&sid), sid);
}

/// Serve `pipe` (secured for `sid`) on a background thread.
fn serve(pipe: String, sid: String) {
	std::thread::Builder::new()
		.name("launch-pipe".into())
		.spawn(move || {
			let name = wide(&pipe);
			let mut first = true;
			loop {
				let sd = pipe_sd(&sid);
				let sa = windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
					nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
					lpSecurityDescriptor: sd,
					bInheritHandle: 0,
				};
				let open_mode = PIPE_ACCESS_DUPLEX | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
				let h = unsafe {
					CreateNamedPipeW(
						name.as_ptr(),
						open_mode,
						PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
						PIPE_UNLIMITED_INSTANCES,
						4096,
						MAX_LINE as u32,
						0,
						if sd.is_null() { std::ptr::null() } else { &sa },
					)
				};
				if !sd.is_null() {
					unsafe { LocalFree(sd as _) };
				}
				if h == INVALID_HANDLE_VALUE {
					let e = unsafe { GetLastError() };
					tracing::warn!("launch pipe: create failed (os error {e}); later launches cannot reach this Pulsar");
					return;
				}
				first = false;
				let connected = unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) } != 0
					|| unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
				if !connected {
					unsafe { CloseHandle(h) };
					continue;
				}
				// One thread per client: a client that never writes must not block the next one.
				let hv = h as usize;
				std::thread::spawn(move || {
					let h = hv as HANDLE;
					let req = read_line(h).and_then(|l| LaunchRequest::from_line(&l));
					if req.is_some() {
						let mut w = 0u32;
						unsafe {
							WriteFile(h, b"ok\n".as_ptr(), 3, &mut w, std::ptr::null_mut());
							FlushFileBuffers(h);
						}
					}
					unsafe {
						DisconnectNamedPipe(h);
						CloseHandle(h);
					}
					match req {
						Some(r) => crate::instance::deliver(r),
						None => tracing::warn!("launch pipe: unreadable request ignored"),
					}
				});
			}
		})
		.ok();
}

/// A plain message box (a second launch has no window of its own).
pub(crate) fn message_box(text: &str) {
	use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_OK};
	let t = wide(text);
	let cap = wide("Pulsar");
	unsafe { MessageBoxW(std::ptr::null_mut(), t.as_ptr(), cap.as_ptr(), MB_OK | MB_ICONINFORMATION) };
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn pipe_name_is_per_session_and_user() {
		let sid = user_sid().expect("a user SID");
		let n = pipe_name(&sid);
		assert!(n.starts_with(r"\\.\pipe\pulsar-launch-"));
		assert!(n.ends_with(&sid));
	}

	#[test]
	fn pipe_security_descriptor_builds() {
		let sid = user_sid().unwrap();
		let sd = pipe_sd(&sid);
		assert!(!sd.is_null());
		unsafe { LocalFree(sd as _) };
	}

	/// The real pipe, end to end in this process: the server (this test exe) passes the
	/// client's "same exe, same session" check, a connect with a password goes through, and
	/// the request is queued for the app.
	#[test]
	fn pipe_round_trip_delivers_the_request() {
		let sid = user_sid().unwrap();
		// Own name: a Pulsar running on the dev box serves the real one.
		let pipe = format!("{}-test-{}", pipe_name(&sid), std::process::id());
		serve(pipe.clone(), sid.clone());
		let req = crate::instance::request_from_args(
			&["pulsar", "--connect", "123 456 789", "--connect-pw", "pw"].map(String::from),
			String::new(),
		);
		let deadline = Instant::now() + Duration::from_secs(5);
		loop {
			match send(&pipe, &req) {
				Ok(()) => break,
				Err(SendErr::Retry) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
				Err(SendErr::Retry) => panic!("server never came up"),
				Err(SendErr::Fail(e)) => panic!("{e}"),
			}
		}
		let deadline = Instant::now() + Duration::from_secs(2);
		let got = loop {
			let early = crate::instance::take_early();
			if !early.is_empty() || Instant::now() > deadline {
				break early;
			}
			std::thread::sleep(Duration::from_millis(20));
		};
		assert_eq!(got, vec![req]);
	}

	#[test]
	fn no_server_means_not_running_for_a_fresh_name() {
		// The test process holds no Pulsar mutex; a real Pulsar may run on the dev box,
		// so only check the helper's error path on a name nobody uses.
		use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
		use windows_sys::Win32::System::Threading::OpenMutexW;
		assert!(!object_exists(
			|n| unsafe { OpenMutexW(SYNCHRONIZE, 0, n) },
			"Local\\PulsarTestNoSuchMutex-7f3a"
		));
	}
}
