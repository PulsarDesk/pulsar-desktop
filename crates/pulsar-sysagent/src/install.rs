//! `install` / `uninstall` / `stop` — SCM registration, run elevated by the NSIS installer
//! hooks (see docs/agents/system-agent.md). Never run implicitly by the app.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use pulsar_sysagent::policy::{is_protected_install_dir, quote_arg};
use pulsar_sysagent::win::wide;
use pulsar_sysagent::SERVICE_NAME;
use windows_sys::Win32::Foundation::{
	GetLastError, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_EXISTS,
};
use windows_sys::Win32::System::Services::{
	ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ControlService,
	CreateServiceW, DeleteService, OpenSCManagerW, OpenServiceW, QueryServiceStatus, StartServiceW,
	SC_ACTION, SC_ACTION_RESTART, SC_HANDLE, SC_MANAGER_ALL_ACCESS, SERVICE_ALL_ACCESS,
	SERVICE_AUTO_START, SERVICE_CONFIG_DESCRIPTION, SERVICE_CONFIG_FAILURE_ACTIONS,
	SERVICE_CONTROL_STOP, SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL, SERVICE_FAILURE_ACTIONSW,
	SERVICE_STATUS, SERVICE_STOPPED, SERVICE_WIN32_OWN_PROCESS,
};

use crate::AgentOpts;

const DISPLAY_NAME: &str = "Pulsar Sistem Aracısı";
const DESCRIPTION: &str =
	"Pulsar uzak masaüstü: UAC onayı, kilit ekranı ve güvenli masaüstünde uzaktan klavye ve fare girişini sağlar.";

struct Sc(SC_HANDLE);

impl Drop for Sc {
	fn drop(&mut self) {
		unsafe { CloseServiceHandle(self.0) };
	}
}

fn err(ctx: &str) -> i32 {
	eprintln!(
		"pulsar-sysagent: {ctx}: {}",
		std::io::Error::last_os_error()
	);
	1
}

fn manager() -> Option<Sc> {
	let h = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_ALL_ACCESS) };
	(!h.is_null()).then_some(Sc(h))
}

fn program_files() -> Vec<PathBuf> {
	["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"]
		.iter()
		.filter_map(|v| std::env::var_os(v).map(PathBuf::from))
		.collect()
}

fn stop_and_wait(svc: &Sc) {
	let mut st: SERVICE_STATUS = unsafe { std::mem::zeroed() };
	unsafe { ControlService(svc.0, SERVICE_CONTROL_STOP, &mut st) };
	let t = Instant::now();
	while t.elapsed() < Duration::from_secs(15) {
		if unsafe { QueryServiceStatus(svc.0, &mut st) } == 0
			|| st.dwCurrentState == SERVICE_STOPPED
		{
			return;
		}
		std::thread::sleep(Duration::from_millis(200));
	}
}

pub fn install(opts: &AgentOpts) -> i32 {
	let Ok(exe) = std::env::current_exe() else {
		return err("current_exe");
	};
	if !opts.allow_any_path && !is_protected_install_dir(&exe, &program_files()) {
		eprintln!(
			"pulsar-sysagent: refusing to register a LocalSystem service from {} — it must live under \
			 Program Files (a user-writable exe would be a privilege escalation). Use --allow-any-path \
			 only for a dev machine you control.",
			exe.display()
		);
		return 3;
	}
	let mut bin = format!("{} service", quote_arg(&exe.to_string_lossy()));
	for a in opts.forward() {
		bin.push(' ');
		bin.push_str(&quote_arg(&a));
	}
	let Some(scm) = manager() else {
		return err("OpenSCManager (run elevated)");
	};
	let (name, display, bin_w) = (wide(SERVICE_NAME), wide(DISPLAY_NAME), wide(&bin));
	let mut h = unsafe {
		CreateServiceW(
			scm.0,
			name.as_ptr(),
			display.as_ptr(),
			SERVICE_ALL_ACCESS,
			SERVICE_WIN32_OWN_PROCESS,
			SERVICE_AUTO_START,
			SERVICE_ERROR_NORMAL,
			bin_w.as_ptr(),
			std::ptr::null(),
			std::ptr::null_mut(),
			std::ptr::null(),
			std::ptr::null(), // LocalSystem
			std::ptr::null(),
		)
	};
	if h.is_null() {
		if unsafe { GetLastError() } != ERROR_SERVICE_EXISTS {
			return err("CreateService");
		}
		// Upgrade: stop the old one, point it at this exe.
		h = unsafe { OpenServiceW(scm.0, name.as_ptr(), SERVICE_ALL_ACCESS) };
		if h.is_null() {
			return err("OpenService");
		}
		let svc = Sc(h);
		stop_and_wait(&svc);
		let ok = unsafe {
			ChangeServiceConfigW(
				svc.0,
				SERVICE_WIN32_OWN_PROCESS,
				SERVICE_AUTO_START,
				SERVICE_ERROR_NORMAL,
				bin_w.as_ptr(),
				std::ptr::null(),
				std::ptr::null_mut(),
				std::ptr::null(),
				std::ptr::null(),
				std::ptr::null(),
				display.as_ptr(),
			)
		};
		if ok == 0 {
			return err("ChangeServiceConfig");
		}
		h = svc.0;
		std::mem::forget(svc);
	}
	let svc = Sc(h);
	let mut desc_w = wide(DESCRIPTION);
	let desc = SERVICE_DESCRIPTIONW {
		lpDescription: desc_w.as_mut_ptr(),
	};
	unsafe {
		ChangeServiceConfig2W(
			svc.0,
			SERVICE_CONFIG_DESCRIPTION,
			(&desc as *const SERVICE_DESCRIPTIONW).cast(),
		)
	};
	// Restart after 5 s on the first three crashes per day.
	let mut actions = [SC_ACTION {
		Type: SC_ACTION_RESTART,
		Delay: 5000,
	}; 3];
	let fa = SERVICE_FAILURE_ACTIONSW {
		dwResetPeriod: 86_400,
		lpRebootMsg: std::ptr::null_mut(),
		lpCommand: std::ptr::null_mut(),
		cActions: actions.len() as u32,
		lpsaActions: actions.as_mut_ptr(),
	};
	unsafe {
		ChangeServiceConfig2W(
			svc.0,
			SERVICE_CONFIG_FAILURE_ACTIONS,
			(&fa as *const SERVICE_FAILURE_ACTIONSW).cast(),
		)
	};
	if unsafe { StartServiceW(svc.0, 0, std::ptr::null()) } == 0 {
		return err("StartService");
	}
	println!("{SERVICE_NAME} installed and started ({bin})");
	0
}

fn open_existing() -> Result<Option<Sc>, i32> {
	let Some(scm) = manager() else {
		return Err(err("OpenSCManager (run elevated)"));
	};
	let name = wide(SERVICE_NAME);
	let h = unsafe { OpenServiceW(scm.0, name.as_ptr(), SERVICE_ALL_ACCESS) };
	if h.is_null() {
		if unsafe { GetLastError() } == ERROR_SERVICE_DOES_NOT_EXIST {
			return Ok(None);
		}
		return Err(err("OpenService"));
	}
	Ok(Some(Sc(h)))
}

pub fn stop() -> i32 {
	match open_existing() {
		Ok(Some(svc)) => {
			stop_and_wait(&svc);
			0
		}
		Ok(None) => 0,
		Err(code) => code,
	}
}

pub fn uninstall() -> i32 {
	match open_existing() {
		Ok(Some(svc)) => {
			stop_and_wait(&svc);
			if unsafe { DeleteService(svc.0) } == 0 {
				return err("DeleteService");
			}
			println!("{SERVICE_NAME} removed");
			0
		}
		Ok(None) => 0,
		Err(code) => code,
	}
}
