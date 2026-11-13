//! Windows side of the seat host: the per-seat mutex and the client end of the
//! service-owned control pipe (pulsar-split `docs/agents/pulsar-host-backend.md` §2, §3.1).

use std::os::windows::io::AsRawHandle;
use std::time::{Duration, Instant};

use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use windows_sys::Win32::Foundation::{
	CloseHandle, GetLastError, LocalFree, ERROR_ALREADY_EXISTS, ERROR_PIPE_BUSY, HANDLE,
};

/// `\\.\pipe\PulsarSplit\seat\<seat-id>\pulsar-host` (the service creates it before launch).
pub(crate) fn pipe_name(seat_id: &str) -> String {
	format!(r"\\.\pipe\PulsarSplit\seat\{seat_id}\pulsar-host")
}

/// The service's image name; the pipe server must be it, as SYSTEM in session 0.
const SERVICE_EXE: &str = "pulsar-split-service.exe";

fn wide(s: &str) -> Vec<u16> {
	s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Why the seat mutex could not be taken.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MutexErr {
	/// Another `pulsar-host` already serves this seat (exit code 3).
	Exists,
	/// Could not create it at all (no SeCreateGlobalPrivilege: not SYSTEM / not elevated).
	Failed(u32),
}

/// Take `Global\PulsarSplit_Seat_<id>_PulsarHost` (DACL `D:P(A;;GA;;;SY)`), held for the
/// life of the process. Never `Local\PulsarSingletonMutex`: the seat user's own Pulsar runs
/// in the same session.
pub(crate) fn take_seat_mutex(seat_id: &str) -> Result<(), MutexErr> {
	use windows_sys::Win32::Security::Authorization::{
		ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
	};
	use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
	use windows_sys::Win32::System::Threading::CreateMutexW;
	let name = wide(&format!("Global\\PulsarSplit_Seat_{seat_id}_PulsarHost"));
	let sddl = wide("D:P(A;;GA;;;SY)");
	unsafe {
		let mut sd = std::ptr::null_mut();
		if ConvertStringSecurityDescriptorToSecurityDescriptorW(
			sddl.as_ptr(),
			SDDL_REVISION_1,
			&mut sd,
			std::ptr::null_mut(),
		) == 0
		{
			return Err(MutexErr::Failed(GetLastError()));
		}
		let sa = SECURITY_ATTRIBUTES {
			nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
			lpSecurityDescriptor: sd,
			bInheritHandle: 0,
		};
		let h = CreateMutexW(&sa, 0, name.as_ptr());
		let err = GetLastError();
		LocalFree(sd as _);
		if h.is_null() {
			// An existing SYSTEM-only mutex opened by a non-SYSTEM caller: access denied.
			return Err(if err == 5 {
				MutexErr::Exists
			} else {
				MutexErr::Failed(err)
			});
		}
		if err == ERROR_ALREADY_EXISTS {
			CloseHandle(h);
			return Err(MutexErr::Exists);
		}
		// Held (never closed) until the process exits.
		Ok(())
	}
}

/// Open the seat pipe (a few retries while it is busy or not there yet) and check that its
/// server is the Pulsar Split service.
pub(crate) async fn connect(seat_id: &str) -> Result<NamedPipeClient, String> {
	let name = pipe_name(seat_id);
	let deadline = Instant::now() + Duration::from_secs(5);
	let pipe = loop {
		match ClientOptions::new().open(&name) {
			Ok(p) => break p,
			Err(e)
				if Instant::now() < deadline
					&& (e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
						|| e.kind() == std::io::ErrorKind::NotFound) =>
			{
				tokio::time::sleep(Duration::from_millis(100)).await;
			}
			Err(e) => return Err(format!("cannot open the seat control pipe: {e}")),
		}
	};
	verify_server(pipe.as_raw_handle() as HANDLE)?;
	Ok(pipe)
}

/// §3.1 server check: `GetNamedPipeServerProcessId` = a SYSTEM process in session 0 whose
/// image is `pulsar-split-service.exe`.
fn verify_server(pipe: HANDLE) -> Result<(), String> {
	use windows_sys::Win32::Security::{
		GetTokenInformation, IsWellKnownSid, TokenUser, WinLocalSystemSid, TOKEN_QUERY, TOKEN_USER,
	};
	use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
	use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
	use windows_sys::Win32::System::Threading::{
		OpenProcess, OpenProcessToken, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
		PROCESS_QUERY_LIMITED_INFORMATION,
	};
	unsafe {
		let mut pid = 0u32;
		if GetNamedPipeServerProcessId(pipe, &mut pid) == 0 {
			return Err("pipe server pid unavailable".into());
		}
		let mut session = u32::MAX;
		if ProcessIdToSessionId(pid, &mut session) == 0 || session != 0 {
			return Err(format!("pipe server (pid {pid}) is not in session 0"));
		}
		let proc = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
		if proc.is_null() {
			return Err(format!("cannot open the pipe server (pid {pid})"));
		}
		let check = (|| {
			let mut buf = [0u16; 1024];
			let mut len = buf.len() as u32;
			if QueryFullProcessImageNameW(proc, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) == 0
			{
				return Err("pipe server image unavailable".to_string());
			}
			let path = String::from_utf16_lossy(&buf[..len as usize]);
			let file = path.rsplit('\\').next().unwrap_or_default();
			if !file.eq_ignore_ascii_case(SERVICE_EXE) {
				return Err(format!("pipe server is {file}, not {SERVICE_EXE}"));
			}
			let mut tok: HANDLE = std::ptr::null_mut();
			if OpenProcessToken(proc, TOKEN_QUERY, &mut tok) == 0 {
				return Err("pipe server token unavailable".to_string());
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
				return Err("pipe server user unavailable".to_string());
			}
			let user = &*(tbuf.as_ptr() as *const TOKEN_USER);
			if IsWellKnownSid(user.User.Sid, WinLocalSystemSid) == 0 {
				return Err("pipe server is not SYSTEM".to_string());
			}
			Ok(())
		})();
		CloseHandle(proc);
		check
	}
}
