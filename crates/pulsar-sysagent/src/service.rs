//! `pulsar-sysagent service` — the LocalSystem SCM service. Keeps exactly one helper per
//! logged-on interactive session, spawned with a copy of the service's own SYSTEM token
//! retargeted to that session (DuplicateTokenEx + SetTokenInformation(TokenSessionId) +
//! CreateProcessAsUserW on `winsta0\default`).

use std::collections::HashMap;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use pulsar_sysagent::policy::{backoff, quote_arg, wants_helper, HEALTHY_RUN};
use pulsar_sysagent::win::{self, Handle};
use pulsar_sysagent::SERVICE_NAME;
use windows_sys::core::PWSTR;
use windows_sys::Win32::Foundation::{ERROR_CALL_NOT_IMPLEMENTED, HANDLE, NO_ERROR, WAIT_OBJECT_0};
use windows_sys::Win32::Security::{
	DuplicateTokenEx, SecurityIdentification, SetTokenInformation, TokenPrimary, TokenSessionId,
	TOKEN_ALL_ACCESS,
};
use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows_sys::Win32::System::JobObjects::{
	AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
	SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
	JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::RemoteDesktop::{
	WTSEnumerateSessionsW, WTSFreeMemory, WTSQueryUserToken, WTS_SESSION_INFOW,
};
use windows_sys::Win32::System::Services::{
	RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW,
	SERVICE_ACCEPT_SESSIONCHANGE, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP,
	SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SESSIONCHANGE, SERVICE_CONTROL_SHUTDOWN,
	SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS,
	SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE, SERVICE_STOPPED, SERVICE_STOP_PENDING,
	SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
};
use windows_sys::Win32::System::Threading::{
	CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, ResumeThread,
	TerminateProcess, WaitForSingleObject, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
	CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, STARTUPINFOW,
};

use crate::log::{self, log};
use crate::AgentOpts;

enum Event {
	Stop,
	/// `SERVICE_CONTROL_SESSIONCHANGE` (logon/logoff/connect/…): reconcile now.
	SessionChange,
}

static EVENTS: OnceLock<Mutex<Sender<Event>>> = OnceLock::new();
static OPTS: OnceLock<AgentOpts> = OnceLock::new();
/// SERVICE_STATUS_HANDLE as usize (raw pointers are not Sync).
static STATUS: OnceLock<usize> = OnceLock::new();

pub fn run(opts: AgentOpts) -> i32 {
	log::init("service");
	let _ = OPTS.set(opts);
	let mut name = win::wide(SERVICE_NAME);
	let table = [
		SERVICE_TABLE_ENTRYW {
			lpServiceName: name.as_mut_ptr(),
			lpServiceProc: Some(service_main),
		},
		SERVICE_TABLE_ENTRYW {
			lpServiceName: std::ptr::null_mut(),
			lpServiceProc: None,
		},
	];
	// Blocks until the service stops. Fails with ERROR_FAILED_SERVICE_CONTROLLER_CONNECT when
	// started from a console instead of by the SCM.
	if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
		log!(
			"service: StartServiceCtrlDispatcher: {} (run via the SCM)",
			std::io::Error::last_os_error()
		);
		return 1;
	}
	0
}

fn set_state(state: SERVICE_STATUS_CURRENT_STATE, exit: u32) {
	let Some(&h) = STATUS.get() else { return };
	let accepts = if state == SERVICE_RUNNING {
		SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN | SERVICE_ACCEPT_SESSIONCHANGE
	} else {
		0
	};
	let st = SERVICE_STATUS {
		dwServiceType: SERVICE_WIN32_OWN_PROCESS,
		dwCurrentState: state,
		dwControlsAccepted: accepts,
		dwWin32ExitCode: exit,
		dwServiceSpecificExitCode: 0,
		dwCheckPoint: 0,
		dwWaitHint: if state == SERVICE_RUNNING || state == SERVICE_STOPPED {
			0
		} else {
			5000
		},
	};
	unsafe { SetServiceStatus(h as SERVICE_STATUS_HANDLE, &st) };
}

fn post(e: Event) {
	if let Some(tx) = EVENTS.get() {
		let _ = tx.lock().unwrap_or_else(|p| p.into_inner()).send(e);
	}
}

unsafe extern "system" fn handler(
	control: u32,
	_event_type: u32,
	_data: *mut core::ffi::c_void,
	_ctx: *mut core::ffi::c_void,
) -> u32 {
	match control {
		SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
			set_state(SERVICE_STOP_PENDING, 0);
			post(Event::Stop);
			NO_ERROR
		}
		SERVICE_CONTROL_SESSIONCHANGE => {
			post(Event::SessionChange);
			NO_ERROR
		}
		SERVICE_CONTROL_INTERROGATE => NO_ERROR,
		_ => ERROR_CALL_NOT_IMPLEMENTED,
	}
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
	let (tx, rx) = mpsc::channel();
	let _ = EVENTS.set(Mutex::new(tx));
	let name = win::wide(SERVICE_NAME);
	let h = RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(handler), std::ptr::null());
	if h.is_null() {
		log!(
			"service: RegisterServiceCtrlHandlerEx: {}",
			std::io::Error::last_os_error()
		);
		return;
	}
	let _ = STATUS.set(h as usize);
	set_state(SERVICE_START_PENDING, 0);
	let mut sup = match Supervisor::new(OPTS.get().cloned().unwrap_or_default()) {
		Ok(s) => s,
		Err(e) => {
			log!("service: init failed: {e}");
			set_state(SERVICE_STOPPED, 1);
			return;
		}
	};
	set_state(SERVICE_RUNNING, 0);
	log!("service: running");
	sup.run(rx);
	set_state(SERVICE_STOPPED, 0);
	log!("service: stopped");
}

struct Child {
	process: Handle,
	pid: u32,
	started: Instant,
}

#[derive(Default)]
struct Slot {
	child: Option<Child>,
	failures: u32,
	retry_at: Option<Instant>,
}

struct Supervisor {
	exe: std::path::PathBuf,
	opts: AgentOpts,
	/// Kill-on-close job: if the service dies, every SYSTEM helper dies with it.
	job: Handle,
	slots: HashMap<u32, Slot>,
}

impl Supervisor {
	fn new(opts: AgentOpts) -> std::io::Result<Self> {
		let job = Handle::new(unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) })?;
		let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
		info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
		let ok = unsafe {
			SetInformationJobObject(
				job.raw(),
				JobObjectExtendedLimitInformation,
				(&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
				std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
			)
		};
		if ok == 0 {
			return Err(std::io::Error::last_os_error());
		}
		Ok(Self {
			exe: std::env::current_exe()?,
			opts,
			job,
			slots: HashMap::new(),
		})
	}

	fn run(&mut self, rx: mpsc::Receiver<Event>) {
		loop {
			self.reconcile();
			match rx.recv_timeout(Duration::from_secs(2)) {
				Ok(Event::Stop) | Err(RecvTimeoutError::Disconnected) => break,
				Ok(Event::SessionChange) => {
					// A logon's user token can lag the notification by a moment: reconcile now
					// and again on the next 2 s tick.
				}
				Err(RecvTimeoutError::Timeout) => {}
			}
		}
		for (s, slot) in self.slots.drain() {
			if let Some(c) = slot.child {
				log!("service: stopping helper of session {s} (pid {})", c.pid);
				unsafe { TerminateProcess(c.process.raw(), 0) };
			}
		}
	}

	fn reconcile(&mut self) {
		let wanted = logged_on_sessions();
		let now = Instant::now();
		for &s in &wanted {
			let slot = self.slots.entry(s).or_default();
			if let Some(c) = &slot.child {
				if unsafe { WaitForSingleObject(c.process.raw(), 0) } == WAIT_OBJECT_0 {
					let mut code = 0u32;
					unsafe { GetExitCodeProcess(c.process.raw(), &mut code) };
					let ran = c.started.elapsed();
					log!(
						"service: helper of session {s} (pid {}) exited {code} after {ran:?}",
						c.pid
					);
					slot.failures = if ran >= HEALTHY_RUN {
						0
					} else {
						slot.failures.saturating_add(1)
					};
					slot.retry_at = Some(now + backoff(slot.failures));
					slot.child = None;
				}
			}
			if slot.child.is_none() && slot.retry_at.is_none_or(|t| now >= t) {
				match spawn_helper(&self.exe, &self.opts, self.job.raw(), s) {
					Ok(c) => {
						log!("service: helper for session {s} started (pid {})", c.pid);
						slot.child = Some(c);
					}
					Err(e) => {
						log!("service: spawn helper for session {s}: {e}");
						slot.failures = slot.failures.saturating_add(1);
						slot.retry_at = Some(now + backoff(slot.failures));
					}
				}
			}
		}
		self.slots.retain(|s, slot| {
			if wanted.contains(s) {
				return true;
			}
			if let Some(c) = &slot.child {
				log!("service: session {s} gone — stopping helper pid {}", c.pid);
				unsafe { TerminateProcess(c.process.raw(), 0) };
			}
			false
		});
	}
}

/// Sessions with a logged-on user (WTSQueryUserToken succeeds) in a state worth serving.
fn logged_on_sessions() -> Vec<u32> {
	let mut p: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
	let mut n = 0u32;
	if unsafe { WTSEnumerateSessionsW(std::ptr::null_mut(), 0, 1, &mut p, &mut n) } == 0 {
		log!(
			"service: WTSEnumerateSessions: {}",
			std::io::Error::last_os_error()
		);
		return Vec::new();
	}
	let infos = unsafe { std::slice::from_raw_parts(p, n as usize) };
	let mut out = Vec::new();
	for i in infos {
		if !wants_helper(i.SessionId, i.State) {
			continue;
		}
		let mut tok: HANDLE = std::ptr::null_mut();
		if unsafe { WTSQueryUserToken(i.SessionId, &mut tok) } != 0 {
			drop(Handle(tok));
			out.push(i.SessionId);
		}
	}
	unsafe { WTSFreeMemory(p.cast()) };
	out
}

fn spawn_helper(
	exe: &std::path::Path,
	opts: &AgentOpts,
	job: HANDLE,
	session: u32,
) -> std::io::Result<Child> {
	match spawn_helper_with(exe, opts, job, session, None) {
		Err(e) if e.raw_os_error() == Some(5) => {
			// Some session hosts (Duo's multi-seat RDP sessions, 2026-11-11) refuse
			// CreateProcessAsUserW with the service's own token moved into the session
			// (ERROR_ACCESS_DENIED). Use the token of that session's own winlogon.exe — also
			// LocalSystem, already in the session — the way PsExec -i -s does.
			match winlogon_pid(session) {
				Some(pid) => spawn_helper_with(exe, opts, job, session, Some(pid))
					.map_err(|e2| std::io::Error::new(e2.kind(), format!("{e} (own token); {e2} (winlogon {pid} token)"))),
				None => Err(std::io::Error::new(e.kind(), format!("{e} (own token); no winlogon.exe in session {session}"))),
			}
		}
		r => r,
	}
}

/// PID of `winlogon.exe` in `session`.
fn winlogon_pid(session: u32) -> Option<u32> {
	use windows_sys::Win32::System::Diagnostics::ToolHelp::{
		CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
	};
	use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
	let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
	if snap == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
		return None;
	}
	let snap = Handle(snap);
	let mut e: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
	e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
	let mut ok = unsafe { Process32FirstW(snap.raw(), &mut e) } != 0;
	while ok {
		let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
		let name = String::from_utf16_lossy(&e.szExeFile[..n]);
		if name.eq_ignore_ascii_case("winlogon.exe") {
			let mut s = u32::MAX;
			if unsafe { ProcessIdToSessionId(e.th32ProcessID, &mut s) } != 0 && s == session {
				return Some(e.th32ProcessID);
			}
		}
		ok = unsafe { Process32NextW(snap.raw(), &mut e) } != 0;
	}
	None
}

fn spawn_helper_with(
	exe: &std::path::Path,
	opts: &AgentOpts,
	job: HANDLE,
	session: u32,
	token_from: Option<u32>,
) -> std::io::Result<Child> {
	let mut own: HANDLE = std::ptr::null_mut();
	if let Some(pid) = token_from {
		use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
		let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
		if h.is_null() {
			return Err(std::io::Error::last_os_error());
		}
		let h = Handle(h);
		if unsafe { OpenProcessToken(h.raw(), TOKEN_ALL_ACCESS, &mut own) } == 0 {
			return Err(std::io::Error::last_os_error());
		}
	} else if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut own) } == 0 {
		return Err(std::io::Error::last_os_error());
	}
	let own = Handle::new(own)?;
	let mut dup: HANDLE = std::ptr::null_mut();
	if unsafe {
		DuplicateTokenEx(
			own.raw(),
			TOKEN_ALL_ACCESS,
			std::ptr::null(),
			SecurityIdentification,
			TokenPrimary,
			&mut dup,
		)
	} == 0
	{
		return Err(std::io::Error::last_os_error());
	}
	let dup = Handle::new(dup)?;
	// Move the SYSTEM token into the user's session (needs SeTcbPrivilege).
	if unsafe {
		SetTokenInformation(
			dup.raw(),
			TokenSessionId,
			(&session as *const u32).cast(),
			4,
		)
	} == 0
	{
		let e = std::io::Error::last_os_error();
		return Err(std::io::Error::new(e.kind(), format!("SetTokenInformation(TokenSessionId): {e}")));
	}
	let mut env: *mut core::ffi::c_void = std::ptr::null_mut();
	if unsafe { CreateEnvironmentBlock(&mut env, dup.raw(), 0) } == 0 {
		env = std::ptr::null_mut();
	}
	let mut cmd = format!("{} helper", quote_arg(&exe.to_string_lossy()));
	for a in opts.forward() {
		cmd.push(' ');
		cmd.push_str(&quote_arg(&a));
	}
	let mut cmd_w = win::wide(&cmd);
	let exe_w = win::wide(&exe.to_string_lossy());
	let mut desktop = win::wide(r"winsta0\default");
	let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
	si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
	si.lpDesktop = desktop.as_mut_ptr();
	let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
	let ok = unsafe {
		CreateProcessAsUserW(
			dup.raw(),
			exe_w.as_ptr(),
			cmd_w.as_mut_ptr(),
			std::ptr::null(),
			std::ptr::null(),
			0,
			CREATE_UNICODE_ENVIRONMENT
				| CREATE_NO_WINDOW
				| CREATE_SUSPENDED
				| CREATE_NEW_PROCESS_GROUP,
			env,
			std::ptr::null(),
			&si,
			&mut pi,
		)
	};
	let err = std::io::Error::last_os_error();
	if !env.is_null() {
		unsafe { DestroyEnvironmentBlock(env) };
	}
	if ok == 0 {
		return Err(std::io::Error::new(err.kind(), format!("CreateProcessAsUserW: {err}")));
	}
	let process = Handle(pi.hProcess);
	let thread = Handle(pi.hThread);
	if unsafe { AssignProcessToJobObject(job, process.raw()) } == 0 {
		// Duo's multi-seat sessions refuse this (ERROR_ACCESS_DENIED, 2026-11-11): their
		// processes can't join our kill-on-close job. Run the helper without it — the service
		// still terminates it explicitly on stop / session end; only a hard service crash can
		// orphan it, and the orphan keeps serving its pipe until the session ends.
		let e = std::io::Error::last_os_error();
		log!("service: session {session}: helper not in kill-on-close job ({e}); tracking it by handle");
	}
	unsafe { ResumeThread(thread.raw()) };
	Ok(Child {
		process,
		pid: pi.dwProcessId,
		started: Instant::now(),
	})
}
