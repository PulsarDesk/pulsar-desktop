//! `pulsar-sysagent helper` — runs in a user session with a SYSTEM token (spawned by the
//! service), serves `\\.\pipe\pulsar-sysagent-<session>` and injects for the session's
//! Pulsar host.

use std::fs::File;
use std::os::windows::io::FromRawHandle;
use std::path::PathBuf;

use pulsar_sysagent::policy::{allowed_client_dirs, client_image_allowed};
use pulsar_sysagent::proto::{self, Reply, Request, REJECT_BAD_HELLO, REJECT_VERSION};
use pulsar_sysagent::sddl::{pipe_sddl, MinIntegrity};
use pulsar_sysagent::win::{self, Handle, SecurityDescriptor};
use windows_sys::Win32::Foundation::{
	GetLastError, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::Pipes::{
	ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, GetNamedPipeClientSessionId,
	PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::RemoteDesktop::WTSQueryUserToken;

use crate::inject::Injector;
use crate::log::{self, log};
use crate::AgentOpts;

/// Simultaneous host connections per session (a restarting host can briefly overlap).
const MAX_INSTANCES: u32 = 4;

/// The user logged on to `session` (needs SeTcbPrivilege — SYSTEM has it).
fn session_user_sid(session: u32) -> std::io::Result<String> {
	let mut tok: HANDLE = std::ptr::null_mut();
	if unsafe { WTSQueryUserToken(session, &mut tok) } == 0 {
		return Err(std::io::Error::last_os_error());
	}
	let tok = Handle::new(tok)?;
	Ok(win::token_facts(tok.raw())?.user_sid)
}

struct Ctx {
	session: u32,
	user_sid: String,
	min: MinIntegrity,
	allowed_dirs: Vec<PathBuf>,
}

pub fn run(opts: AgentOpts) -> i32 {
	log::init("helper");
	let me = match win::current_process_facts() {
		Ok(f) => f,
		Err(e) => {
			log!("helper: cannot read own token: {e}");
			return 2;
		}
	};
	if !me.is_local_system {
		// Never serve input as anything but SYSTEM: a user-level copy would gain nothing
		// and only confuse clients (they also reject non-SYSTEM pipe owners).
		log!(
			"helper: refusing to run as {} (must be LocalSystem)",
			me.user_sid
		);
		return 2;
	}
	let session = match win::current_session() {
		Ok(s) if s != 0 => s,
		other => {
			log!("helper: not in an interactive session ({other:?})");
			return 2;
		}
	};
	// Derive the user ourselves instead of trusting arguments.
	let user_sid = match session_user_sid(session) {
		Ok(s) => s,
		Err(e) => {
			log!("helper: session {session} has no logged-on user: {e}");
			return 3;
		}
	};
	let min = if opts.allow_medium {
		MinIntegrity::Medium
	} else {
		MinIntegrity::High
	};
	let sddl = match pipe_sddl(&user_sid, min) {
		Ok(s) => s,
		Err(e) => {
			log!("helper: {e}");
			return 3;
		}
	};
	let sd = match SecurityDescriptor::from_sddl(&sddl) {
		Ok(sd) => sd,
		Err(e) => {
			log!("helper: SDDL rejected ({sddl}): {e}");
			return 3;
		}
	};
	let exe = std::env::current_exe().unwrap_or_default();
	let ctx = std::sync::Arc::new(Ctx {
		session,
		user_sid,
		min,
		allowed_dirs: allowed_client_dirs(&exe, &opts.client_dirs),
	});
	log!(
		"helper: session {session}, user {}, min IL {min:?}, clients from {:?}",
		ctx.user_sid,
		ctx.allowed_dirs
	);
	serve(&ctx, &sd)
}

fn serve(ctx: &std::sync::Arc<Ctx>, sd: &SecurityDescriptor) -> i32 {
	let name = win::wide(&proto::pipe_name(ctx.session));
	let sa = SECURITY_ATTRIBUTES {
		nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
		lpSecurityDescriptor: sd.as_ptr(),
		bInheritHandle: 0,
	};
	let mut first = true;
	loop {
		let open_mode = PIPE_ACCESS_DUPLEX
			| if first {
				FILE_FLAG_FIRST_PIPE_INSTANCE
			} else {
				0
			};
		let h = unsafe {
			CreateNamedPipeW(
				name.as_ptr(),
				open_mode,
				PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
				MAX_INSTANCES,
				4096,
				4096,
				0,
				&sa,
			)
		};
		if h == INVALID_HANDLE_VALUE {
			let e = std::io::Error::last_os_error();
			if first {
				// Someone else already owns the name: an old helper (service restart race) or
				// a squatter. Exit; the service retries with backoff. Clients reject a
				// non-SYSTEM owner, so a squatter only causes the SendInput fallback.
				log!("helper: cannot create pipe (already in use?): {e}");
				return 4;
			}
			log!("helper: CreateNamedPipe: {e}");
			std::thread::sleep(std::time::Duration::from_secs(1));
			continue;
		}
		first = false;
		let connected = unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) } != 0
			|| unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
		if !connected {
			drop(Handle(h));
			continue;
		}
		let ctx = ctx.clone();
		// SAFETY: `h` is a valid, connected pipe handle that we own from here on.
		let pipe = unsafe { File::from_raw_handle(h as _) };
		let _ = std::thread::Builder::new()
			.name("sysagent-conn".into())
			.spawn(move || {
				if let Err(e) = handle_client(&ctx, pipe) {
					log!("helper: connection ended: {e}");
				}
			});
	}
}

/// Who is on the other end? Same session, same user, ≥ required integrity, trusted image.
fn check_client(ctx: &Ctx, h: HANDLE) -> Result<(), String> {
	let mut s = u32::MAX;
	if unsafe { GetNamedPipeClientSessionId(h, &mut s) } == 0 || s != ctx.session {
		return Err(format!("client session {s} != {}", ctx.session));
	}
	let mut pid = 0u32;
	if unsafe { GetNamedPipeClientProcessId(h, &mut pid) } == 0 {
		return Err("no client pid".into());
	}
	let (facts, image) = win::process_facts(pid).map_err(|e| format!("client {pid}: {e}"))?;
	if !facts.user_sid.eq_ignore_ascii_case(&ctx.user_sid) {
		return Err(format!(
			"client {pid} user {} is not the session user",
			facts.user_sid
		));
	}
	if facts.integrity_rid < ctx.min.rid() {
		return Err(format!(
			"client {pid} integrity {:#x} below {:?}",
			facts.integrity_rid, ctx.min
		));
	}
	if !client_image_allowed(&image, &ctx.allowed_dirs) {
		return Err(format!(
			"client {pid} image {} not in an allowed dir",
			image.display()
		));
	}
	Ok(())
}

fn handle_client(ctx: &Ctx, mut pipe: File) -> std::io::Result<()> {
	use std::os::windows::io::AsRawHandle;
	if let Err(why) = check_client(ctx, pipe.as_raw_handle() as HANDLE) {
		log!("helper: rejected client: {why}");
		return Ok(());
	}
	// Every version from MIN_VERSION up is served; the ack echoes the client's version (a v1
	// host checks for exactly 1), and only a v2+ client may send pointer frames.
	let version = match proto::read_request(&mut pipe) {
		Ok(Some(Request::Hello { version }))
			if (proto::MIN_VERSION..=proto::VERSION).contains(&version) =>
		{
			proto::write_frame(
				&mut pipe,
				&Reply::HelloAck {
					version,
					session: ctx.session,
				}
				.encode(),
			)?;
			version
		}
		Ok(Some(Request::Hello { version })) => {
			log!(
				"helper: client protocol v{version}, want v{}",
				proto::VERSION
			);
			let _ = proto::write_frame(
				&mut pipe,
				&Reply::Reject {
					code: REJECT_VERSION,
				}
				.encode(),
			);
			return Ok(());
		}
		_ => {
			let _ = proto::write_frame(
				&mut pipe,
				&Reply::Reject {
					code: REJECT_BAD_HELLO,
				}
				.encode(),
			);
			return Ok(());
		}
	};
	log!("helper: client connected (protocol v{version})");
	let mut inj = Injector::new();
	loop {
		// Any decode error drops the connection (and releases held input via Drop).
		let Some(req) = proto::read_request(&mut pipe)? else {
			log!("helper: client disconnected");
			return Ok(());
		};
		match req {
			Request::Mouse {
				dx,
				dy,
				data,
				flags,
			} => inj.mouse(dx, dy, data, flags),
			Request::Key { vk, scan, flags } => inj.key(vk, scan, flags),
			Request::ClampCursor(r) => inj.clamp(r),
			Request::Pinch { phase, scale } => inj.pinch(phase, scale),
			Request::ReleaseAll => inj.release_all(),
			Request::Pointer { pen, n, contacts } if version >= proto::VERSION_POINTER => {
				inj.pointer(pen, &contacts[..n as usize])
			}
			Request::Pointer { .. } => {
				return Err(std::io::Error::other("pointer frame from a v1 client"))
			}
			Request::Ping { seq } => {
				let desktop = inj.desktop();
				proto::write_frame(&mut pipe, &Reply::Pong { seq, desktop }.encode())?;
			}
			Request::Hello { .. } => return Err(std::io::Error::other("duplicate hello")),
		}
	}
}
