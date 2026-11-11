//! Small Win32 helpers shared by the client, the helper and the service (Windows only).

use std::io;

use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::Authorization::{
	ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
	GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, IsWellKnownSid,
	TokenIntegrityLevel, TokenUser, WinLocalSystemSid, PSECURITY_DESCRIPTOR, PSID,
	TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::{
	GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken,
	QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

pub fn wide(s: &str) -> Vec<u16> {
	s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Read a NUL-terminated UTF-16 string.
///
/// # Safety
/// `p` must be null or point to a NUL-terminated UTF-16 buffer.
pub unsafe fn from_wide_ptr(p: *const u16) -> String {
	if p.is_null() {
		return String::new();
	}
	let mut n = 0;
	while *p.add(n) != 0 {
		n += 1;
	}
	String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
}

/// A Win32 HANDLE closed on drop.
pub struct Handle(pub HANDLE);

// SAFETY: kernel handles are process-wide; the wrapper only closes it once.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Handle {
	pub fn new(h: HANDLE) -> io::Result<Self> {
		if h.is_null() || h == INVALID_HANDLE_VALUE {
			Err(io::Error::last_os_error())
		} else {
			Ok(Self(h))
		}
	}
	pub fn raw(&self) -> HANDLE {
		self.0
	}
}

impl Drop for Handle {
	fn drop(&mut self) {
		unsafe { CloseHandle(self.0) };
	}
}

/// A self-relative security descriptor from `ConvertStringSecurityDescriptor…` (LocalFree'd).
pub struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

unsafe impl Send for SecurityDescriptor {}

impl SecurityDescriptor {
	pub fn from_sddl(sddl: &str) -> io::Result<Self> {
		let w = wide(sddl);
		let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
		let ok = unsafe {
			ConvertStringSecurityDescriptorToSecurityDescriptorW(
				w.as_ptr(),
				SDDL_REVISION_1,
				&mut sd,
				std::ptr::null_mut(),
			)
		};
		if ok == 0 {
			Err(io::Error::last_os_error())
		} else {
			Ok(Self(sd))
		}
	}
	pub fn as_ptr(&self) -> PSECURITY_DESCRIPTOR {
		self.0
	}
}

impl Drop for SecurityDescriptor {
	fn drop(&mut self) {
		unsafe { LocalFree(self.0 as _) };
	}
}

/// `S-1-…` string of a SID.
///
/// # Safety
/// `sid` must point to a valid SID.
pub unsafe fn sid_to_string(sid: PSID) -> io::Result<String> {
	let mut s: *mut u16 = std::ptr::null_mut();
	if ConvertSidToStringSidW(sid, &mut s) == 0 {
		return Err(io::Error::last_os_error());
	}
	let out = from_wide_ptr(s);
	LocalFree(s as _);
	Ok(out)
}

/// # Safety
/// `sid` must point to a valid SID.
pub unsafe fn sid_is_local_system(sid: PSID) -> bool {
	IsWellKnownSid(sid, WinLocalSystemSid) != 0
}

fn token_info(token: HANDLE, class: i32) -> io::Result<Vec<u64>> {
	let mut len = 0u32;
	unsafe { GetTokenInformation(token, class, std::ptr::null_mut(), 0, &mut len) };
	if len == 0 {
		return Err(io::Error::last_os_error());
	}
	// u64 storage keeps the TOKEN_* structs aligned.
	let mut buf = vec![0u64; (len as usize).div_ceil(8)];
	if unsafe { GetTokenInformation(token, class, buf.as_mut_ptr().cast(), len, &mut len) } == 0 {
		return Err(io::Error::last_os_error());
	}
	Ok(buf)
}

/// What the helper / service needs to know about a token.
#[derive(Debug, Clone)]
pub struct TokenFacts {
	pub user_sid: String,
	pub is_local_system: bool,
	/// `SECURITY_MANDATORY_*_RID` (0x2000 medium, 0x3000 high, 0x4000 system).
	pub integrity_rid: u32,
}

pub fn token_facts(token: HANDLE) -> io::Result<TokenFacts> {
	let user = token_info(token, TokenUser)?;
	let tu = user.as_ptr() as *const TOKEN_USER;
	let sid = unsafe { (*tu).User.Sid };
	let user_sid = unsafe { sid_to_string(sid)? };
	let is_local_system = unsafe { sid_is_local_system(sid) };
	let il = token_info(token, TokenIntegrityLevel)?;
	let tml = il.as_ptr() as *const TOKEN_MANDATORY_LABEL;
	let integrity_rid = unsafe {
		let lsid = (*tml).Label.Sid;
		let n = *GetSidSubAuthorityCount(lsid);
		if n == 0 {
			0
		} else {
			*GetSidSubAuthority(lsid, (n - 1) as u32)
		}
	};
	Ok(TokenFacts {
		user_sid,
		is_local_system,
		integrity_rid,
	})
}

pub fn current_process_facts() -> io::Result<TokenFacts> {
	let mut tok: HANDLE = std::ptr::null_mut();
	if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) } == 0 {
		return Err(io::Error::last_os_error());
	}
	let tok = Handle::new(tok)?;
	token_facts(tok.raw())
}

/// Facts + image path of another process (the pipe client, seen from the SYSTEM helper).
pub fn process_facts(pid: u32) -> io::Result<(TokenFacts, std::path::PathBuf)> {
	let proc_ = Handle::new(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) })?;
	let mut tok: HANDLE = std::ptr::null_mut();
	if unsafe { OpenProcessToken(proc_.raw(), TOKEN_QUERY, &mut tok) } == 0 {
		return Err(io::Error::last_os_error());
	}
	let tok = Handle::new(tok)?;
	let facts = token_facts(tok.raw())?;
	let mut buf = vec![0u16; 32768];
	let mut n = buf.len() as u32;
	if unsafe {
		QueryFullProcessImageNameW(proc_.raw(), PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut n)
	} == 0
	{
		return Err(io::Error::last_os_error());
	}
	let path = std::path::PathBuf::from(String::from_utf16_lossy(&buf[..n as usize]));
	Ok((facts, path))
}

pub fn session_of(pid: u32) -> io::Result<u32> {
	let mut s = 0u32;
	if unsafe { ProcessIdToSessionId(pid, &mut s) } == 0 {
		Err(io::Error::last_os_error())
	} else {
		Ok(s)
	}
}

pub fn current_session() -> io::Result<u32> {
	session_of(unsafe { GetCurrentProcessId() })
}
