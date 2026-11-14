//! Headless host entry points (main.rs, before Tauri / the single-instance guard / the
//! elevation relaunch):
//!
//! - `pulsar --headless-host`: host only, no window, no tray, no webview. Existing config,
//!   identity and pairings of this user (or shared mode), logs to the normal log file and
//!   stdout. Stops cleanly on Ctrl+C / Ctrl+Break / console close / logoff / shutdown
//!   (Windows console handler — only when started from a console, which it attaches to;
//!   SIGINT / SIGTERM / SIGHUP elsewhere) and on
//!   `pulsar --headless-stop` (Windows: the `Local\PulsarHeadlessHostStop` event).
//!   Takes `Local\PulsarSingletonMutex` like the GUI, so it never runs next to a GUI
//!   instance of the same user/session (both would register the same identity).
//! - `pulsar --split-seat <id> --data-dir <abs> --log <abs>`: one Pulsar Split seat host
//!   (`seat/`), Windows only.
//!
//! The host still lives in a Tauri app (the host code takes an `AppHandle`), but one that
//! creates no window and no tray; `ui_start::set_headless` makes every popup / toast /
//! overlay window builder a no-op.

use std::path::{Path, PathBuf};

pub const EXIT_OK: i32 = 0;
pub const EXIT_ARGS: i32 = 2;
/// Another instance already serves this session / seat.
pub const EXIT_RUNNING: i32 = 3;
/// The seat control pipe could not be opened or its server is not the service.
pub const EXIT_PIPE: i32 = 4;
/// The seat control pipe broke while running.
pub const EXIT_PIPE_LOST: i32 = 5;
/// The seat mutex could not be created (not SYSTEM / not elevated).
pub const EXIT_MUTEX: i32 = 6;
/// `--headless-stop`: a headless host runs but this process may not signal it.
pub const EXIT_DENIED: i32 = 7;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cli {
	Headless,
	Stop,
	Seat {
		seat_id: String,
		data_dir: PathBuf,
		log: PathBuf,
	},
}

/// Lowercase GUID, the Pulsar Split seat id format (`split_stream::validate_seat_id`).
pub fn valid_seat_id(id: &str) -> bool {
	id.len() == 36
		&& id.char_indices().all(|(i, c)| match i {
			8 | 13 | 18 | 23 => c == '-',
			_ => c.is_ascii_digit() || ('a'..='f').contains(&c),
		})
}

fn absolute(p: &str) -> bool {
	let path = Path::new(p);
	// `Path::is_absolute` on Windows wants a drive or UNC prefix; on Unix a leading `/`.
	path.is_absolute()
}

/// Parse argv (with argv[0]). `Ok(None)` = not a headless-host launch. Strict: a headless
/// launch accepts no other flag, so nothing secret can ride along (`--connect-pw`, …).
pub fn parse(args: &[String]) -> Result<Option<Cli>, String> {
	let rest = args.get(1..).unwrap_or(&[]);
	if rest.iter().any(|a| a == "--split-seat") {
		let (mut seat, mut data, mut log) = (None, None, None);
		let mut it = rest.iter();
		while let Some(flag) = it.next() {
			let slot = match flag.as_str() {
				"--split-seat" => &mut seat,
				"--data-dir" => &mut data,
				"--log" => &mut log,
				other => {
					// Never echo a value: it might be a secret.
					let name = if other.starts_with("--") {
						other.split('=').next().unwrap_or("")
					} else {
						"<value>"
					};
					return Err(format!(
						"--split-seat accepts only --data-dir and --log (got {name})"
					));
				}
			};
			let Some(v) = it.next() else {
				return Err(format!("{flag} needs a value"));
			};
			if slot.replace(v.clone()).is_some() {
				return Err(format!("{flag} given twice"));
			}
		}
		let seat_id = seat.ok_or("--split-seat needs a seat id")?;
		if !valid_seat_id(&seat_id) {
			return Err("--split-seat: the seat id must be a lowercase GUID".into());
		}
		let data_dir = data.ok_or("--split-seat needs --data-dir")?;
		let log = log.ok_or("--split-seat needs --log")?;
		if !absolute(&data_dir) || !absolute(&log) {
			return Err("--data-dir and --log must be absolute paths".into());
		}
		return Ok(Some(Cli::Seat {
			seat_id,
			data_dir: data_dir.into(),
			log: log.into(),
		}));
	}
	let headless = rest.iter().any(|a| a == "--headless-host");
	let stop = rest.iter().any(|a| a == "--headless-stop");
	if !headless && !stop {
		return Ok(None);
	}
	if headless && stop {
		return Err("--headless-host and --headless-stop exclude each other".into());
	}
	if let Some(other) = rest
		.iter()
		.find(|a| *a != "--headless-host" && *a != "--headless-stop")
	{
		let name = if other.starts_with("--") {
			other.split('=').next().unwrap_or("")
		} else {
			"<value>"
		};
		return Err(format!("--headless-host takes no other flag (got {name})"));
	}
	Ok(Some(if stop { Cli::Stop } else { Cli::Headless }))
}

/// main.rs entry. Returns the process exit code.
pub fn main(args: &[String]) -> Option<i32> {
	match parse(args) {
		Ok(None) => None,
		Ok(Some(cli)) => Some(run(cli)),
		Err(e) => {
			eprintln!("pulsar: {e}");
			Some(EXIT_ARGS)
		}
	}
}

fn run(cli: Cli) -> i32 {
	match cli {
		Cli::Stop => stop_signal(),
		Cli::Headless => run_headless(),
		Cli::Seat {
			seat_id,
			data_dir,
			log,
		} => run_seat(&seat_id, data_dir, &log),
	}
}

#[cfg(windows)]
const STOP_EVENT: &str = "Local\\PulsarHeadlessHostStop";

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
	s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `pulsar --headless-stop`: ask the headless host of this session to stop. 0 = signalled,
/// 1 = none running, 7 = one runs but may not be signalled from here.
fn stop_signal() -> i32 {
	#[cfg(windows)]
	unsafe {
		use windows_sys::Win32::Foundation::CloseHandle;
		use windows_sys::Win32::System::Threading::{OpenEventW, SetEvent, EVENT_MODIFY_STATE};
		let name = wide(STOP_EVENT);
		let h = OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr());
		if h.is_null() {
			if windows_sys::Win32::Foundation::GetLastError()
				== windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED
			{
				eprintln!("pulsar: a headless host runs, but this shell may not stop it (started by another account?) — run --headless-stop from an elevated shell");
				return EXIT_DENIED;
			}
			eprintln!("pulsar: no headless host is running in this session");
			return 1;
		}
		SetEvent(h);
		CloseHandle(h);
		0
	}
	#[cfg(not(windows))]
	{
		eprintln!("pulsar: --headless-stop is Windows only; send SIGTERM to the headless host");
		EXIT_ARGS
	}
}

static APP: std::sync::OnceLock<tauri::AppHandle> = std::sync::OnceLock::new();
static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Ask the app to stop (signal / console / stop event). Before the app exists: exit now.
fn request_stop(why: &str) {
	tracing::info!(why, "headless host: stop requested");
	match APP.get() {
		Some(a) => a.exit(0),
		None => std::process::exit(0),
	}
}

#[cfg(windows)]
unsafe extern "system" fn on_console_ctrl(_ctrl: u32) -> windows_sys::core::BOOL {
	request_stop("console control event");
	// Close / logoff / shutdown end the process once this returns: wait for the teardown.
	let t = std::time::Instant::now();
	while !DONE.load(std::sync::atomic::Ordering::SeqCst)
		&& t.elapsed() < std::time::Duration::from_secs(4)
	{
		std::thread::sleep(std::time::Duration::from_millis(50));
	}
	1
}

/// Security descriptor for the stop event: full access for SYSTEM, Administrators and this
/// process's user, Medium mandatory label. Null on failure (caller LocalFree's it).
#[cfg(windows)]
unsafe fn stop_event_sd() -> windows_sys::Win32::Security::PSECURITY_DESCRIPTOR {
	use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
	use windows_sys::Win32::Security::Authorization::{
		ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
		SDDL_REVISION_1,
	};
	use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
	use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
	let mut tok: HANDLE = std::ptr::null_mut();
	if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) == 0 {
		return std::ptr::null_mut();
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
		return std::ptr::null_mut();
	}
	let user = &*(tbuf.as_ptr() as *const TOKEN_USER);
	let mut s: windows_sys::core::PWSTR = std::ptr::null_mut();
	if ConvertSidToStringSidW(user.User.Sid, &mut s) == 0 {
		return std::ptr::null_mut();
	}
	let mut n = 0;
	while *s.add(n) != 0 {
		n += 1;
	}
	let sid = String::from_utf16_lossy(std::slice::from_raw_parts(s, n));
	LocalFree(s as _);
	let sddl = wide(&format!(
		"D:(A;;0x1F0003;;;SY)(A;;0x1F0003;;;BA)(A;;0x1F0003;;;{sid})S:(ML;;NW;;;ME)"
	));
	let mut sd = std::ptr::null_mut();
	if ConvertStringSecurityDescriptorToSecurityDescriptorW(
		sddl.as_ptr(),
		SDDL_REVISION_1,
		&mut sd,
		std::ptr::null_mut(),
	) == 0
	{
		return std::ptr::null_mut();
	}
	sd
}

/// Stop paths for `--headless-host`, installed before anything else spawns threads.
fn install_stop_handlers() {
	#[cfg(windows)]
	unsafe {
		use windows_sys::Win32::System::Console::{
			AttachConsole, SetConsoleCtrlHandler, ATTACH_PARENT_PROCESS,
		};
		use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};
		// Release builds are GUI-subsystem (no console): join the launching terminal's console
		// so Ctrl+C reaches us and stdout logging shows there. Fails harmlessly otherwise.
		AttachConsole(ATTACH_PARENT_PROCESS);
		SetConsoleCtrlHandler(Some(on_console_ctrl), 1);
		let name = wide(STOP_EVENT);
		// An elevated host's event would get a High label and the creator's default DACL, so
		// `--headless-stop` from a normal shell of the same user would be refused: give this
		// user EVENT_MODIFY_STATE and a Medium label (no-write-up). Default security if the
		// descriptor cannot be built.
		let sd = stop_event_sd();
		let sa = windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
			nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>()
				as u32,
			lpSecurityDescriptor: sd,
			bInheritHandle: 0,
		};
		let psa = if sd.is_null() {
			std::ptr::null()
		} else {
			&sa as *const _
		};
		let ev = CreateEventW(psa, 1, 0, name.as_ptr()) as usize;
		if !sd.is_null() {
			windows_sys::Win32::Foundation::LocalFree(sd as _);
		}
		if ev != 0 {
			std::thread::spawn(move || {
				if WaitForSingleObject(ev as _, INFINITE) == 0 {
					request_stop("--headless-stop");
				}
			});
		}
	}
	#[cfg(unix)]
	unsafe {
		// Self-pipe: the handler only write(2)s a byte (async-signal-safe), a thread turns it
		// into a clean stop. Not a blocked signal mask + sigwait: a mask is inherited across
		// fork/exec, so every child (ffmpeg, capture helpers) would ignore SIGTERM/SIGINT.
		// A caught signal's disposition resets to default on exec, so children are unaffected.
		let mut fds = [0 as libc::c_int; 2];
		if libc::pipe(fds.as_mut_ptr()) != 0 {
			tracing::warn!("headless host: no signal pipe — stop it with SIGKILL");
			return;
		}
		for fd in fds {
			libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
		}
		SIG_PIPE_W.store(fds[1], std::sync::atomic::Ordering::SeqCst);
		let mut sa: libc::sigaction = std::mem::zeroed();
		sa.sa_sigaction = on_stop_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
		sa.sa_flags = libc::SA_RESTART;
		libc::sigemptyset(&mut sa.sa_mask);
		for s in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
			libc::sigaction(s, &sa, std::ptr::null_mut());
		}
		let r = fds[0];
		std::thread::spawn(move || {
			let mut b = 0u8;
			loop {
				let n = libc::read(r, (&mut b as *mut u8).cast(), 1);
				if n == 1 {
					request_stop("signal");
					return;
				}
				if n == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
					return;
				}
			}
		});
	}
}

#[cfg(unix)]
static SIG_PIPE_W: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

#[cfg(unix)]
extern "C" fn on_stop_signal(_sig: libc::c_int) {
	let fd = SIG_PIPE_W.load(std::sync::atomic::Ordering::Relaxed);
	if fd >= 0 {
		let b = 1u8;
		unsafe { libc::write(fd, (&b as *const u8).cast(), 1) };
	}
}

fn set_error_mode() {
	#[cfg(windows)]
	unsafe {
		use windows_sys::Win32::System::Diagnostics::Debug::{
			SetErrorMode, SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX,
		};
		SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX);
	}
}

/// A Tauri app with no window, no tray and no webview; `setup` brings the host up.
fn run_app(setup: impl FnOnce(&tauri::AppHandle) + Send + 'static) -> i32 {
	crate::ui_start::set_headless();
	let app = tauri::Builder::default()
		.manage(crate::local_relay::LocalRelay::default())
		.manage(crate::state::AppState::new())
		.setup(move |app| {
			let _ = APP.set(app.handle().clone());
			setup(app.handle());
			Ok(())
		})
		.build(crate::context());
	let app = match app {
		Ok(a) => a,
		Err(e) => {
			tracing::error!("headless host: Tauri failed to start: {e}");
			return 1;
		}
	};
	app.run(|h, ev| {
		if let tauri::RunEvent::Exit = ev {
			let h = h.clone();
			tauri::async_runtime::block_on(async move {
				let _ = tokio::time::timeout(
					std::time::Duration::from_secs(3),
					crate::seat::teardown(&h),
				)
				.await;
			});
			tracing::info!("headless host stopped");
			DONE.store(true, std::sync::atomic::Ordering::SeqCst);
		}
	});
	EXIT_OK
}

fn run_headless() -> i32 {
	install_stop_handlers();
	crate::logging::init();
	tracing::info!(
		version = env!("CARGO_PKG_VERSION"),
		"Pulsar headless host starting"
	);
	if crate::update_guard::blocked_by_update() {
		tracing::info!("an update is being installed — not starting the old build");
		return EXIT_OK;
	}
	set_error_mode();
	if !crate::acquire_single_instance_strict() {
		tracing::error!("another Pulsar is already running for this user/session — not starting the headless host");
		return EXIT_RUNNING;
	}
	let _ = crate::shared_mode::resolved();
	run_app(|app| {
		use tauri::Manager;
		let cfg = pulsar_core::config::Config::load(crate::util::config_path(app));
		tracing::info!(relay = %cfg.relay, "config loaded");
		crate::i18n::set_lang(&cfg.language);
		*app.state::<crate::state::AppState>().config.lock().unwrap() = cfg;
		#[cfg(windows)]
		crate::firewall::setup(app);
		// Linux: put back a display mode a crashed earlier run left adapted.
		#[cfg(target_os = "linux")]
		std::thread::spawn(crate::display_mode::recover_stale);
		crate::shared_mode::lease::start(app.clone());
		crate::headless::start(app.clone(), "--headless-host");
	})
}

#[cfg(windows)]
fn run_seat(seat_id: &str, data_dir: PathBuf, log: &Path) -> i32 {
	crate::seat::strip_dev_env();
	crate::seat::init(seat_id, data_dir);
	crate::logging::init_file(log);
	tracing::info!(
		version = env!("CARGO_PKG_VERSION"),
		seat = seat_id,
		"Pulsar seat host starting"
	);
	match crate::seat::win::take_seat_mutex(seat_id) {
		Ok(()) => {}
		Err(crate::seat::win::MutexErr::Exists) => {
			tracing::error!("another pulsar-host already serves this seat");
			return EXIT_RUNNING;
		}
		Err(crate::seat::win::MutexErr::Failed(code)) => {
			tracing::error!(code, "cannot create the seat mutex (needs SYSTEM)");
			return EXIT_MUTEX;
		}
	}
	set_error_mode();
	run_app(|app| {
		// No config file: everything arrives with config.apply over the pipe.
		crate::seat::spawn_pipe(app.clone());
	})
}

#[cfg(not(windows))]
fn run_seat(_seat_id: &str, _data_dir: PathBuf, _log: &Path) -> i32 {
	eprintln!("pulsar: --split-seat is Windows only");
	EXIT_ARGS
}

#[cfg(test)]
mod tests {
	use super::*;

	const ID: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";

	fn argv(a: &[&str]) -> Vec<String> {
		std::iter::once("pulsar")
			.chain(a.iter().copied())
			.map(String::from)
			.collect()
	}

	#[cfg(windows)]
	const DIR: &str = r"C:\ProgramData\PulsarSplit\seats\x\pulsar";
	#[cfg(not(windows))]
	const DIR: &str = "/var/lib/pulsar-split/x";
	#[cfg(windows)]
	const LOG: &str = r"C:\ProgramData\PulsarSplit\seats\x\pulsar\pulsar-host.log";
	#[cfg(not(windows))]
	const LOG: &str = "/var/lib/pulsar-split/x/pulsar-host.log";

	#[test]
	fn not_headless() {
		assert_eq!(parse(&argv(&[])), Ok(None));
		assert_eq!(parse(&argv(&["--background"])), Ok(None));
		assert_eq!(parse(&argv(&["--host-only"])), Ok(None));
		assert_eq!(parse(&argv(&["--connect", "123"])), Ok(None));
	}

	#[test]
	fn headless_host_and_stop() {
		assert_eq!(parse(&argv(&["--headless-host"])), Ok(Some(Cli::Headless)));
		assert_eq!(parse(&argv(&["--headless-stop"])), Ok(Some(Cli::Stop)));
		assert!(parse(&argv(&["--headless-host", "--headless-stop"])).is_err());
		let e = parse(&argv(&["--headless-host", "--connect-pw", "hunter2"])).unwrap_err();
		assert!(e.contains("--connect-pw") && !e.contains("hunter2"), "{e}");
		assert!(parse(&argv(&["--headless-host", "--background"])).is_err());
	}

	#[test]
	fn split_seat_ok_any_order() {
		let want = Cli::Seat {
			seat_id: ID.into(),
			data_dir: DIR.into(),
			log: LOG.into(),
		};
		assert_eq!(
			parse(&argv(&[
				"--split-seat",
				ID,
				"--data-dir",
				DIR,
				"--log",
				LOG
			])),
			Ok(Some(want.clone()))
		);
		assert_eq!(
			parse(&argv(&[
				"--log",
				LOG,
				"--split-seat",
				ID,
				"--data-dir",
				DIR
			])),
			Ok(Some(want))
		);
	}

	#[test]
	fn split_seat_rejects() {
		// Missing pieces.
		assert!(parse(&argv(&["--split-seat", ID, "--data-dir", DIR])).is_err());
		assert!(parse(&argv(&["--split-seat", ID, "--log", LOG])).is_err());
		assert!(parse(&argv(&["--split-seat"])).is_err());
		// Bad id.
		assert!(parse(&argv(&[
			"--split-seat",
			"Seat1",
			"--data-dir",
			DIR,
			"--log",
			LOG
		]))
		.is_err());
		assert!(parse(&argv(&[
			"--split-seat",
			&ID.to_uppercase(),
			"--data-dir",
			DIR,
			"--log",
			LOG
		]))
		.is_err());
		// Relative paths.
		assert!(parse(&argv(&[
			"--split-seat",
			ID,
			"--data-dir",
			"seat",
			"--log",
			LOG
		]))
		.is_err());
		// Twice.
		assert!(parse(&argv(&[
			"--split-seat",
			ID,
			"--split-seat",
			ID,
			"--data-dir",
			DIR,
			"--log",
			LOG
		]))
		.is_err());
		// Any extra flag, secret-carrying ones included, without echoing the value.
		for extra in [
			["--connect-pw", "hunter2"],
			["--connect", "123456789"],
			["--headless-host", "x"],
		] {
			let mut a = vec!["--split-seat", ID, "--data-dir", DIR, "--log", LOG];
			a.extend(extra);
			let e = parse(&argv(&a)).unwrap_err();
			assert!(!e.contains("hunter2") && !e.contains("123456789"), "{e}");
		}
		let e = parse(&argv(&[
			"--split-seat",
			ID,
			"--data-dir",
			DIR,
			"--log",
			LOG,
			"secret-value",
		]))
		.unwrap_err();
		assert!(!e.contains("secret-value"), "{e}");
	}

	#[test]
	fn seat_id_format() {
		assert!(valid_seat_id(ID));
		assert!(!valid_seat_id("0f8fad5b-d9cb-469f-a165-70867728950"));
		assert!(!valid_seat_id("0f8fad5b_d9cb-469f-a165-70867728950e"));
		assert!(!valid_seat_id("../../evil"));
	}
}
