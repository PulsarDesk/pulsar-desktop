//! "Kullanıcılar arası ortak Pulsar" (Settings → Genel): one Pulsar per PC instead of one per
//! Windows user.
//!
//! - **ON (shared):** one device identity / ID, config, pairings, TOFU pins, saved device
//!   passwords and saved-devices list for the whole machine, in `%ProgramData%\Pulsar\shared\`.
//!   That folder is SYSTEM + Administrators only (protected DACL), so only an ELEVATED Pulsar
//!   reads it: a machine identity readable by every local user would let any of them
//!   impersonate this host, and a user-writable `paired_devices.json` / `config.json` would let
//!   any user grant themselves access to whoever's session is being streamed. Pulsar already
//!   elevates by default (`request_admin`), and in shared mode it always asks for elevation.
//!   Only ONE host runs machine-wide: the `Global\PulsarSharedHost` mutex (the "lease"). The
//!   instance whose Windows session is Active takes it; when its session stops being Active
//!   (fast user switching, RDP takeover, disconnect) it goes offline and releases the lease,
//!   and the Pulsar of the session that is Active now takes over with the same ID. A Pulsar
//!   without the lease never registers: its `go_online` fails with `SHARED_STANDBY:<user>`.
//! - **OFF (per-user):** today's behaviour — every file in the per-user app config dir, a
//!   separate ID per Windows user (Duo multi-seat seats are hosted separately).
//!
//! The machine-wide switch lives in `%ProgramData%\Pulsar\mode.json` (`{"shared": bool}`,
//! readable by every user, writable by admins). It is read once per process: a change applies
//! after a restart (the toggle restarts the app itself; other users' Pulsar on their next
//! start). With no `mode.json` yet, [`decide`] picks the default: ON for a fresh machine, OFF
//! when any per-user Pulsar data already exists, so an existing install never silently
//! changes its ID. See `docs/agents/architecture.md` → "Shared (machine-wide) mode".
//!
//! Non-Windows: always per-user (the setting is hidden).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Tauri's app identifier = the per-user config dir name under `%APPDATA%`.
pub(crate) const APP_ID: &str = "dev.pulsar.app";

/// Files that live in the shared dir in shared mode (and are copied there on migration).
pub(crate) const SHARED_FILES: &[&str] = &[
	"identity.key",
	"config.json",
	"paired_devices.json",
	"known_peers.json",
	"device_creds.json",
	"peers.json",
];

/// Per-user marker: this user's Pulsar data was created while shared mode was undecided and
/// Pulsar was not elevated (it could not create the machine dir), so it ran per-user for that
/// launch only. Such data does not count as an "existing install" for the default decision.
pub(crate) const PROVISIONAL_MARKER: &str = ".shared-mode-provisional";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
	PerUser,
	Shared,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Decision {
	pub mode: Mode,
	/// Persist this as the machine marker (`None` = leave it as it is).
	pub write_marker: Option<bool>,
	/// This launch runs per-user only because the default could not be applied (mark the
	/// per-user data provisional).
	pub provisional: bool,
}

/// The mode for this launch.
///
/// - `marker`: `mode.json` (`None` = never decided on this machine).
/// - `existing_per_user`: some Windows user already has (non-provisional) per-user Pulsar data.
/// - `elevated`: this process may create / write the machine dir.
///
/// "Default ON" applies to fresh machines only: an existing per-user install keeps its ID
/// until someone turns the setting on.
pub(crate) fn decide(marker: Option<bool>, existing_per_user: bool, elevated: bool) -> Decision {
	let d = |mode, write_marker, provisional| Decision {
		mode,
		write_marker,
		provisional,
	};
	match marker {
		Some(true) => d(Mode::Shared, None, false),
		Some(false) => d(Mode::PerUser, None, false),
		None if existing_per_user => d(Mode::PerUser, elevated.then_some(false), false),
		None if elevated => d(Mode::Shared, Some(true), false),
		// Fresh machine, but no admin rights to set up the shared dir: per-user for now and
		// decide again on the next (elevated) launch.
		None => d(Mode::PerUser, None, true),
	}
}

/// Which per-user files to copy into the shared dir when shared mode starts.
///
/// The shared set is kept whole: if the shared dir already has an identity (shared mode was on
/// before), nothing is copied — its pairings and pins belong to that identity. Otherwise the
/// current user's files are copied (their identity becomes the machine's). Other users' files
/// are never read or changed.
pub(crate) fn migration_plan<'a>(
	shared_has: &dyn Fn(&str) -> bool,
	user_has: &dyn Fn(&str) -> bool,
	files: &[&'a str],
) -> Vec<&'a str> {
	if shared_has("identity.key") {
		return Vec::new();
	}
	files
		.iter()
		.copied()
		.filter(|f| user_has(f) && !shared_has(f))
		.collect()
}

/// Parse `mode.json`.
pub(crate) fn parse_marker(raw: &str) -> Option<bool> {
	let v: serde_json::Value = serde_json::from_str(raw).ok()?;
	v.get("shared")?.as_bool()
}

/// Whether a per-user config dir holds Pulsar data that counts as an existing install.
pub(crate) fn has_user_data(dir: &Path) -> bool {
	(dir.join("identity.key").is_file() || dir.join("config.json").is_file())
		&& !dir.join(PROVISIONAL_MARKER).exists()
}

// ---------------------------------------------------------------------------
// Process-wide resolution
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub(crate) struct Resolved {
	pub mode: Mode,
	/// Shared mode only: the shared dir exists with the expected ACL and this process can use
	/// it (elevated). `false` → no hosting, Settings shows why.
	pub access: bool,
	pub elevated: bool,
	/// `mode.json` as read (`None` = undecided).
	pub marker: Option<bool>,
}

static RESOLVED: OnceLock<Resolved> = OnceLock::new();

/// The mode of this process (resolved on the first call, from `run()`).
pub(crate) fn resolved() -> &'static Resolved {
	// Unit tests never touch the machine's real ProgramData / profile dirs.
	#[cfg(test)]
	return RESOLVED.get_or_init(|| Resolved {
		mode: Mode::PerUser,
		access: false,
		elevated: false,
		marker: None,
	});
	#[cfg(not(test))]
	RESOLVED.get_or_init(resolve)
}

/// This process uses the machine-wide files (shared mode with access).
pub(crate) fn is_shared() -> bool {
	let r = resolved();
	r.mode == Mode::Shared && r.access
}

/// Where Pulsar's data files live for this process: the shared dir in shared mode, else
/// `per_user` (the Tauri app config dir). Shared mode WITHOUT access (not elevated) keeps the
/// per-user files for its UI settings; it never hosts (`lease::may_host`), so it never loads
/// or creates an identity there.
pub(crate) fn data_dir(per_user: PathBuf) -> PathBuf {
	let r = resolved();
	match (r.mode, r.access, imp::shared_dir()) {
		(Mode::Shared, true, Some(d)) => d,
		_ => per_user,
	}
}

/// Before Tauri starts (main.rs): shared mode always needs an elevated process.
pub fn requires_admin() -> bool {
	imp::machine_dir()
		.and_then(|d| std::fs::read_to_string(d.join("mode.json")).ok())
		.and_then(|raw| parse_marker(&raw))
		== Some(true)
}

/// The per-user app config dir without an `AppHandle` (same path Tauri resolves).
fn user_dir() -> Option<PathBuf> {
	#[cfg(windows)]
	{
		std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join(APP_ID))
	}
	#[cfg(not(windows))]
	{
		None
	}
}

fn resolve() -> Resolved {
	#[cfg(not(windows))]
	{
		Resolved {
			mode: Mode::PerUser,
			access: false,
			elevated: false,
			marker: None,
		}
	}
	#[cfg(windows)]
	{
		let elevated = imp::is_elevated();
		let marker = imp::read_marker();
		let user = user_dir();
		let existing = || {
			user.as_deref().is_some_and(has_user_data)
				|| imp::other_profiles_have_data(user.as_deref())
		};
		// Only look at the profiles when it matters (no marker yet).
		let d = if marker.is_some() {
			decide(marker, false, elevated)
		} else {
			decide(None, existing(), elevated)
		};
		let mut access = false;
		if d.mode == Mode::Shared {
			match imp::prepare_dirs() {
				Ok(()) => access = true,
				Err(e) => tracing::warn!("shared mode: machine dir unusable — not hosting: {e}"),
			}
		}
		if let Some(v) = d.write_marker {
			if d.mode == Mode::PerUser || access {
				if let Err(e) = imp::prepare_dirs().and_then(|_| imp::write_marker(v)) {
					tracing::warn!("shared mode: could not record the machine default: {e}");
				}
			}
		}
		if d.provisional {
			if let Some(u) = &user {
				let _ = std::fs::create_dir_all(u);
				let _ = std::fs::write(u.join(PROVISIONAL_MARKER), b"");
			}
		}
		if d.mode == Mode::Shared && access {
			// No machine identity yet (the setting was just defaulted ON, or turned on by
			// another user who had no Pulsar data): this user's files become the machine's —
			// including provisional data from an unelevated launch. A no-op once it exists.
			if let Some(u) = &user {
				migrate(u);
			}
		}
		let r = Resolved {
			mode: d.mode,
			access,
			elevated,
			marker: d.write_marker.or(marker),
		};
		tracing::info!(mode = ?r.mode, access = r.access, elevated = r.elevated, marker = ?r.marker, "shared mode resolved");
		r
	}
}

/// Copy the current user's files into the shared dir per [`migration_plan`].
#[cfg(windows)]
fn migrate(user: &Path) {
	let Some(shared) = imp::shared_dir() else {
		return;
	};
	let plan = migration_plan(
		&|f| shared.join(f).exists(),
		&|f| user.join(f).is_file(),
		SHARED_FILES,
	);
	for f in plan {
		let (src, dst) = (user.join(f), shared.join(f));
		let r = if f == "device_creds.json" {
			crate::device_creds::reseal_for_machine(&src, &dst);
			Ok(())
		} else {
			// Read + write (not fs::copy): the new file must take the shared dir's inherited
			// ACL, never the per-user file's.
			std::fs::read(&src).and_then(|b| std::fs::write(&dst, b))
		};
		match r {
			Ok(()) => tracing::info!(file = f, "shared mode: migrated from this user's data"),
			Err(e) => tracing::warn!(file = f, "shared mode: migrate failed: {e}"),
		}
	}
	let _ = std::fs::remove_file(user.join(PROVISIONAL_MARKER));
}

// ---------------------------------------------------------------------------
// Tauri commands (Settings → Genel)
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Status {
	/// The setting exists on this OS.
	supported: bool,
	/// The machine setting (what the toggle shows).
	enabled: bool,
	/// This process runs in shared mode (differs from `enabled` until a restart).
	active: bool,
	/// Shared mode is on but this process cannot use the shared files (not elevated).
	no_access: bool,
	elevated: bool,
	/// This instance holds the machine-wide host lease.
	held: bool,
	/// Windows user whose session is hosting (shared mode, another instance holds it).
	holder: Option<String>,
}

#[tauri::command]
pub(crate) fn shared_mode_status() -> Status {
	let r = resolved();
	let enabled = imp::read_marker().unwrap_or(r.mode == Mode::Shared);
	Status {
		supported: cfg!(windows),
		enabled,
		active: r.mode == Mode::Shared,
		no_access: r.mode == Mode::Shared && !r.access,
		elevated: r.elevated,
		held: lease::held(),
		holder: lease::holder_user(),
	}
}

/// Turn the machine setting on/off, then restart Pulsar so every path follows it. Needs an
/// elevated process (`SHARED_ELEVATION`). Turning it ON migrates the current user's files
/// when the shared dir has no identity yet; OFF leaves the shared dir in place (turning it on
/// again reuses that machine identity) and this user goes back to their own per-user files.
#[tauri::command]
pub(crate) fn set_shared_mode(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
	#[cfg(not(windows))]
	{
		let _ = (app, enabled);
		Err("SHARED_UNSUPPORTED".into())
	}
	#[cfg(windows)]
	{
		if !imp::is_elevated() {
			return Err("SHARED_ELEVATION".into());
		}
		imp::prepare_dirs().map_err(|e| format!("SHARED_DIR:{e}"))?;
		if enabled {
			if let Some(u) = user_dir() {
				migrate(&u);
			}
		}
		imp::write_marker(enabled).map_err(|e| format!("SHARED_DIR:{e}"))?;
		tracing::info!(enabled, "shared mode changed — restarting");
		app.restart();
	}
}

/// Saved-devices list (the UI's `pulsar.peers.v1`) in shared mode: `None` when per-user (the
/// webview's own storage is used).
#[tauri::command]
pub(crate) fn shared_peers_get() -> Option<String> {
	let r = resolved();
	if r.mode != Mode::Shared || !r.access {
		return None;
	}
	Some(
		std::fs::read_to_string(imp::shared_dir()?.join("peers.json"))
			.unwrap_or_else(|_| "[]".into()),
	)
}

#[tauri::command]
pub(crate) fn shared_peers_set(json: String) {
	let r = resolved();
	if r.mode != Mode::Shared
		|| !r.access
		|| serde_json::from_str::<serde_json::Value>(&json).is_err()
	{
		return;
	}
	if let Some(d) = imp::shared_dir() {
		crate::util::atomic_write_json(&d.join("peers.json"), &json);
	}
}

// ---------------------------------------------------------------------------
// Machine-wide host lease
// ---------------------------------------------------------------------------

/// Is this process elevated (admin token)? Windows only.
#[cfg(windows)]
pub(crate) fn is_elevated() -> bool {
	imp::is_elevated()
}

pub(crate) mod lease {
	use std::sync::atomic::{AtomicBool, Ordering};

	static HELD: AtomicBool = AtomicBool::new(false);

	pub(crate) fn held() -> bool {
		HELD.load(Ordering::SeqCst)
	}

	/// Gate for `bring_online`: in shared mode only the lease holder may register.
	pub(crate) fn may_host() -> Result<(), String> {
		let r = super::resolved();
		if r.mode != super::Mode::Shared {
			return Ok(());
		}
		if !r.access {
			return Err("SHARED_NO_ACCESS".into());
		}
		if held() {
			return Ok(());
		}
		Err(format!(
			"SHARED_STANDBY:{}",
			holder_user().unwrap_or_default()
		))
	}

	/// Holder info (`host.json` in the shared dir), for a standby instance's UI.
	#[derive(serde::Serialize, serde::Deserialize, Default)]
	struct Holder {
		user: String,
		session: u32,
		#[serde(default)]
		id: String,
	}

	fn holder_path() -> Option<std::path::PathBuf> {
		super::imp::shared_dir().map(|d| d.join("host.json"))
	}

	pub(crate) fn holder_user() -> Option<String> {
		if super::resolved().mode != super::Mode::Shared {
			return None;
		}
		let raw = std::fs::read_to_string(holder_path()?).ok()?;
		let h: Holder = serde_json::from_str(&raw).ok()?;
		(!h.user.is_empty()).then_some(h.user)
	}

	fn write_holder(id: &str) {
		let Some(p) = holder_path() else { return };
		let h = Holder {
			user: std::env::var("USERNAME").unwrap_or_default(),
			session: super::imp::own_session(),
			id: id.to_string(),
		};
		if let Ok(json) = serde_json::to_string(&h) {
			crate::util::atomic_write_json(&p, &json);
		}
	}

	/// `bring_online` registered: record the ID next to the holder.
	pub(crate) fn note_registered(id: &str) {
		if held() {
			write_holder(id);
		}
	}

	/// Pure release rule: give the lease up after `RELEASE_TICKS` consecutive ticks with our
	/// session not Active (a lock screen keeps the session Active, so a locked host stays up).
	pub(crate) const RELEASE_TICKS: u32 = 2;

	pub(crate) fn next_inactive(ticks: u32, active: bool) -> (u32, bool) {
		if active {
			(0, false)
		} else {
			let t = ticks.saturating_add(1);
			(t, t >= RELEASE_TICKS)
		}
	}

	#[derive(Clone, serde::Serialize)]
	struct Event {
		held: bool,
		holder: Option<String>,
	}

	/// Start the lease thread (shared mode with access, Windows only). It owns the mutex (a
	/// Win32 mutex belongs to the thread that acquired it), polls every 2 s, and:
	/// - not held + own session Active → try to acquire; on success emit `shared-host`
	///   `{held:true}` (the UI calls `go_online`; with no UI the headless loop is restarted);
	/// - held + session not Active for `RELEASE_TICKS` → mark not held (so no new
	///   `go_online` gets through), tear the node down, release, emit `{held:false}`.
	pub(crate) fn start(app: tauri::AppHandle) {
		#[cfg(not(windows))]
		let _ = app;
		#[cfg(windows)]
		{
			let r = super::resolved();
			if r.mode != super::Mode::Shared || !r.access {
				return;
			}
			std::thread::Builder::new()
				.name("shared-host-lease".into())
				.spawn(move || run(app))
				.ok();
		}
	}

	#[cfg(windows)]
	fn run(app: tauri::AppHandle) {
		use tauri::{Emitter, Manager};
		let Some(m) = super::imp::HostMutex::create() else {
			tracing::warn!("shared mode: cannot create the machine-wide host lock — not hosting");
			return;
		};
		let mut ticks = 0u32;
		let emit = |held: bool| {
			let _ = app.emit(
				"shared-host",
				Event {
					held,
					holder: holder_user(),
				},
			);
		};
		loop {
			let active = super::imp::own_session_active();
			if !held() {
				if active && m.try_acquire() {
					write_holder("");
					HELD.store(true, Ordering::SeqCst);
					ticks = 0;
					tracing::info!("shared mode: this session is the machine's host now");
					emit(true);
					crate::headless::restart(app.clone(), "shared host lease acquired");
				}
			} else {
				let (t, release) = next_inactive(ticks, active);
				ticks = t;
				if release {
					tracing::info!("shared mode: session no longer active — handing the host over");
					HELD.store(false, Ordering::SeqCst);
					tauri::async_runtime::block_on(async {
						let st = app.state::<crate::state::AppState>();
						let _g = st.go_online_lock.lock().await;
						crate::host::go_offline(&app).await;
					});
					if let Some(p) = holder_path() {
						let _ = std::fs::remove_file(p);
					}
					m.release();
					emit(false);
				}
			}
			std::thread::sleep(std::time::Duration::from_secs(2));
		}
	}
}

// ---------------------------------------------------------------------------
// Windows plumbing
// ---------------------------------------------------------------------------

#[cfg(not(windows))]
mod imp {
	use std::path::PathBuf;
	pub(super) fn machine_dir() -> Option<PathBuf> {
		None
	}
	pub(super) fn shared_dir() -> Option<PathBuf> {
		None
	}
	pub(super) fn read_marker() -> Option<bool> {
		None
	}
	pub(super) fn own_session() -> u32 {
		0
	}
}

#[cfg(windows)]
mod imp {
	use std::path::{Path, PathBuf};

	use windows_sys::Win32::Foundation::{
		CloseHandle, LocalFree, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0,
	};
	use windows_sys::Win32::Security::Authorization::{
		ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
		GetNamedSecurityInfoW, SetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
	};
	use windows_sys::Win32::Security::{
		GetSecurityDescriptorDacl, GetTokenInformation, TokenElevation, ACL,
		DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
		PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_ELEVATION, TOKEN_QUERY,
	};
	use windows_sys::Win32::Storage::FileSystem::{
		CreateDirectoryW, GetFileAttributesW, FILE_ATTRIBUTE_REPARSE_POINT, INVALID_FILE_ATTRIBUTES,
	};
	use windows_sys::Win32::System::RemoteDesktop::{
		ProcessIdToSessionId, WTSActive, WTSConnectState, WTSFreeMemory,
		WTSQuerySessionInformationW, WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION,
	};
	use windows_sys::Win32::System::Threading::{
		CreateMutexW, GetCurrentProcess, GetCurrentProcessId, OpenProcessToken, ReleaseMutex,
		WaitForSingleObject,
	};

	/// `%ProgramData%\Pulsar`: SYSTEM + Administrators full, Users read (for `mode.json`).
	/// Owner Administrators, protected (no inherited ACEs from ProgramData, which lets every
	/// user create files).
	const SDDL_MACHINE: &str = "O:BAD:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)";
	/// `%ProgramData%\Pulsar\shared`: SYSTEM + Administrators only.
	const SDDL_SHARED: &str = "O:BAD:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";
	/// The host lease mutex: SYSTEM + Administrators (only elevated Pulsar takes part).
	const SDDL_MUTEX: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)";
	const MUTEX_NAME: &str = "Global\\PulsarSharedHost";

	fn wide(s: &str) -> Vec<u16> {
		s.encode_utf16().chain(std::iter::once(0)).collect()
	}

	fn wide_path(p: &Path) -> Vec<u16> {
		use std::os::windows::ffi::OsStrExt;
		p.as_os_str()
			.encode_wide()
			.chain(std::iter::once(0))
			.collect()
	}

	/// `%ProgramData%\Pulsar`, from the known-folder API (not the env var a user can point
	/// anywhere).
	pub(super) fn machine_dir() -> Option<PathBuf> {
		use windows_sys::Win32::System::Com::CoTaskMemFree;
		use windows_sys::Win32::UI::Shell::{FOLDERID_ProgramData, SHGetKnownFolderPath};
		let mut p: windows_sys::core::PWSTR = std::ptr::null_mut();
		// SAFETY: out-pointer is valid; the returned buffer is freed with CoTaskMemFree.
		let hr =
			unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramData, 0, std::ptr::null_mut(), &mut p) };
		if hr < 0 || p.is_null() {
			if !p.is_null() {
				unsafe { CoTaskMemFree(p as _) };
			}
			return None;
		}
		let s = unsafe {
			let mut n = 0;
			while *p.add(n) != 0 {
				n += 1;
			}
			String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
		};
		unsafe { CoTaskMemFree(p as _) };
		Some(PathBuf::from(s).join("Pulsar"))
	}

	pub(super) fn shared_dir() -> Option<PathBuf> {
		machine_dir().map(|d| d.join("shared"))
	}

	pub(super) fn read_marker() -> Option<bool> {
		let raw = std::fs::read_to_string(machine_dir()?.join("mode.json")).ok()?;
		super::parse_marker(&raw)
	}

	pub(super) fn write_marker(shared: bool) -> std::io::Result<()> {
		let d = machine_dir().ok_or_else(|| std::io::Error::other("no ProgramData"))?;
		let json = serde_json::json!({ "shared": shared }).to_string();
		let tmp = d.join("mode.json.tmp");
		std::fs::write(&tmp, json)?;
		std::fs::rename(tmp, d.join("mode.json"))
	}

	pub(super) fn is_elevated() -> bool {
		// SAFETY: plain token query on our own process.
		unsafe {
			let mut token: HANDLE = std::ptr::null_mut();
			if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
				return false;
			}
			let mut elev = TOKEN_ELEVATION { TokenIsElevated: 0 };
			let mut len = 0u32;
			let ok = GetTokenInformation(
				token,
				TokenElevation,
				&mut elev as *mut _ as *mut _,
				std::mem::size_of::<TOKEN_ELEVATION>() as u32,
				&mut len,
			);
			CloseHandle(token);
			ok != 0 && elev.TokenIsElevated != 0
		}
	}

	/// Any OTHER Windows profile with (non-provisional) Pulsar data. Only an elevated process
	/// can look into other profiles; otherwise this is `false` (and nothing gets decided).
	pub(super) fn other_profiles_have_data(own: Option<&Path>) -> bool {
		let Some(profiles) = std::env::var_os("USERPROFILE")
			.and_then(|p| PathBuf::from(p).parent().map(Path::to_path_buf))
		else {
			return false;
		};
		let Ok(rd) = std::fs::read_dir(profiles) else {
			return false;
		};
		rd.flatten().any(|e| {
			let d = e.path().join("AppData").join("Roaming").join(super::APP_ID);
			Some(d.as_path()) != own && super::has_user_data(&d)
		})
	}

	struct Sd(PSECURITY_DESCRIPTOR);
	impl Drop for Sd {
		fn drop(&mut self) {
			unsafe { LocalFree(self.0 as _) };
		}
	}

	fn sd(sddl: &str) -> std::io::Result<Sd> {
		let w = wide(sddl);
		let mut p: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
		// SAFETY: valid NUL-terminated SDDL; the SD is LocalFree'd by `Sd`.
		if unsafe {
			ConvertStringSecurityDescriptorToSecurityDescriptorW(
				w.as_ptr(),
				SDDL_REVISION_1,
				&mut p,
				std::ptr::null_mut(),
			)
		} == 0
		{
			return Err(std::io::Error::last_os_error());
		}
		Ok(Sd(p))
	}

	/// Create `dir` with `sddl`, or — when it exists — check it is a real directory owned by
	/// SYSTEM/Administrators (a standard user could have pre-created it to read what we write)
	/// and re-apply the protected DACL.
	fn ensure_dir(dir: &Path, sddl: &str) -> std::io::Result<()> {
		let sd = sd(sddl)?;
		let w = wide_path(dir);
		let sa = SECURITY_ATTRIBUTES {
			nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
			lpSecurityDescriptor: sd.0,
			bInheritHandle: 0,
		};
		// SAFETY: valid path + attributes for the call.
		if unsafe { CreateDirectoryW(w.as_ptr(), &sa) } != 0 {
			return Ok(());
		}
		let attrs = unsafe { GetFileAttributesW(w.as_ptr()) };
		if attrs == INVALID_FILE_ATTRIBUTES {
			return Err(std::io::Error::last_os_error());
		}
		if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
			return Err(std::io::Error::other(format!(
				"{} is a link",
				dir.display()
			)));
		}
		let owner = owner_sid(&w)?;
		if owner != "S-1-5-18" && owner != "S-1-5-32-544" {
			return Err(std::io::Error::other(format!(
				"{} is owned by {owner}",
				dir.display()
			)));
		}
		let mut present = 0;
		let mut defaulted = 0;
		let mut dacl: *mut ACL = std::ptr::null_mut();
		// SAFETY: `sd` is a valid SD; the DACL pointer points into it.
		unsafe {
			if GetSecurityDescriptorDacl(sd.0, &mut present, &mut dacl, &mut defaulted) == 0 {
				return Err(std::io::Error::last_os_error());
			}
			let rc = SetNamedSecurityInfoW(
				w.as_ptr(),
				SE_FILE_OBJECT,
				DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
				std::ptr::null_mut(),
				std::ptr::null_mut(),
				dacl,
				std::ptr::null(),
			);
			if rc != 0 {
				return Err(std::io::Error::from_raw_os_error(rc as i32));
			}
		}
		Ok(())
	}

	fn owner_sid(w: &[u16]) -> std::io::Result<String> {
		let mut owner: PSID = std::ptr::null_mut();
		let mut psd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
		// SAFETY: out-pointers valid; `psd` is LocalFree'd, `owner` points into it.
		unsafe {
			let rc = GetNamedSecurityInfoW(
				w.as_ptr(),
				SE_FILE_OBJECT,
				OWNER_SECURITY_INFORMATION,
				&mut owner,
				std::ptr::null_mut(),
				std::ptr::null_mut(),
				std::ptr::null_mut(),
				&mut psd,
			);
			if rc != 0 {
				return Err(std::io::Error::from_raw_os_error(rc as i32));
			}
			let _guard = Sd(psd);
			let mut s: windows_sys::core::PWSTR = std::ptr::null_mut();
			if ConvertSidToStringSidW(owner, &mut s) == 0 {
				return Err(std::io::Error::last_os_error());
			}
			let mut n = 0;
			while *s.add(n) != 0 {
				n += 1;
			}
			let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, n));
			LocalFree(s as _);
			Ok(out)
		}
	}

	/// Create / verify `%ProgramData%\Pulsar` and `…\shared` with their ACLs.
	pub(super) fn prepare_dirs() -> std::io::Result<()> {
		let m = machine_dir().ok_or_else(|| std::io::Error::other("no ProgramData"))?;
		ensure_dir(&m, SDDL_MACHINE)?;
		ensure_dir(&m.join("shared"), SDDL_SHARED)?;
		// Readable at all (= elevated): a non-admin gets ACCESS_DENIED here.
		std::fs::read_dir(m.join("shared")).map(|_| ())
	}

	pub(super) fn own_session() -> u32 {
		let mut s = 0u32;
		// SAFETY: out-pointer valid.
		unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut s) };
		s
	}

	/// Our Windows session is Active (the console / the connected remote session). A locked
	/// session is still Active; a fast-user-switched-away or disconnected one is not.
	pub(super) fn own_session_active() -> bool {
		let mut buf: windows_sys::core::PWSTR = std::ptr::null_mut();
		let mut len = 0u32;
		// SAFETY: WTS allocates `buf` (freed below); WTSConnectState returns one i32.
		unsafe {
			if WTSQuerySessionInformationW(
				WTS_CURRENT_SERVER_HANDLE,
				WTS_CURRENT_SESSION,
				WTSConnectState,
				&mut buf,
				&mut len,
			) == 0 || buf.is_null()
			{
				// Can't tell: assume active (never drop a working host over a failed query).
				return true;
			}
			let state = *(buf as *const i32);
			WTSFreeMemory(buf as _);
			state == WTSActive
		}
	}

	/// The `Global\PulsarSharedHost` mutex. Owned (and released) by the lease thread only.
	pub(crate) struct HostMutex(HANDLE);

	impl HostMutex {
		pub(crate) fn create() -> Option<Self> {
			let sd = sd(SDDL_MUTEX).ok()?;
			let sa = SECURITY_ATTRIBUTES {
				nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
				lpSecurityDescriptor: sd.0,
				bInheritHandle: 0,
			};
			let name = wide(MUTEX_NAME);
			// SAFETY: valid name + attributes; not initially owned (acquired via WaitForSingleObject).
			let h = unsafe { CreateMutexW(&sa, 0, name.as_ptr()) };
			(!h.is_null()).then_some(Self(h))
		}

		/// Non-blocking. An abandoned mutex (the previous holder crashed or logged off) counts
		/// as acquired.
		pub(crate) fn try_acquire(&self) -> bool {
			let r = unsafe { WaitForSingleObject(self.0, 0) };
			r == WAIT_OBJECT_0 || r == WAIT_ABANDONED
		}

		pub(crate) fn release(&self) {
			unsafe { ReleaseMutex(self.0) };
		}
	}

	impl Drop for HostMutex {
		fn drop(&mut self) {
			unsafe { CloseHandle(self.0) };
		}
	}

	// SAFETY: only the lease thread uses it, but the handle is a plain kernel handle.
	unsafe impl Send for HostMutex {}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn marker_wins_over_everything() {
		assert_eq!(decide(Some(true), true, false).mode, Mode::Shared);
		assert_eq!(decide(Some(false), false, true).mode, Mode::PerUser);
		assert_eq!(decide(Some(true), false, true).write_marker, None);
	}

	#[test]
	fn fresh_machine_defaults_on() {
		let d = decide(None, false, true);
		assert_eq!(
			d,
			Decision {
				mode: Mode::Shared,
				write_marker: Some(true),
				provisional: false
			}
		);
	}

	#[test]
	fn existing_install_keeps_its_per_user_id() {
		// Elevated: record OFF so a later new Windows user doesn't flip the machine to shared.
		let d = decide(None, true, true);
		assert_eq!(
			d,
			Decision {
				mode: Mode::PerUser,
				write_marker: Some(false),
				provisional: false
			}
		);
		// Not elevated: per-user too, nothing written.
		let d = decide(None, true, false);
		assert_eq!(
			d,
			Decision {
				mode: Mode::PerUser,
				write_marker: None,
				provisional: false
			}
		);
	}

	#[test]
	fn fresh_but_unelevated_is_provisional() {
		let d = decide(None, false, false);
		assert_eq!(
			d,
			Decision {
				mode: Mode::PerUser,
				write_marker: None,
				provisional: true
			}
		);
	}

	#[test]
	fn migration_copies_this_users_files_into_an_empty_shared_dir() {
		let user = ["identity.key", "config.json", "paired_devices.json"];
		let plan = migration_plan(&|_| false, &|f| user.contains(&f), SHARED_FILES);
		assert_eq!(
			plan,
			vec!["identity.key", "config.json", "paired_devices.json"]
		);
	}

	#[test]
	fn migration_keeps_an_existing_shared_identity_whole() {
		let plan = migration_plan(&|f| f == "identity.key", &|_| true, SHARED_FILES);
		assert!(plan.is_empty());
	}

	#[test]
	fn migration_never_overwrites_a_shared_file() {
		// Shared dir has a config (e.g. written by an earlier unelevated run) but no identity.
		let plan = migration_plan(&|f| f == "config.json", &|_| true, SHARED_FILES);
		assert!(plan.contains(&"identity.key"));
		assert!(!plan.contains(&"config.json"));
	}

	#[test]
	fn marker_parsing() {
		assert_eq!(parse_marker(r#"{"shared":true}"#), Some(true));
		assert_eq!(parse_marker(r#"{"shared":false}"#), Some(false));
		assert_eq!(parse_marker(r#"{}"#), None);
		assert_eq!(parse_marker("garbage"), None);
	}

	#[test]
	fn user_data_detection() {
		let dir = std::env::temp_dir().join(format!("pulsar-shared-test-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		assert!(!has_user_data(&dir));
		std::fs::write(dir.join("identity.key"), [0u8; 32]).unwrap();
		assert!(has_user_data(&dir));
		std::fs::write(dir.join(PROVISIONAL_MARKER), b"").unwrap();
		assert!(
			!has_user_data(&dir),
			"provisional data is not an existing install"
		);
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn per_user_paths_unchanged_when_not_shared() {
		let p = PathBuf::from("C:/x/dev.pulsar.app");
		assert_eq!(resolved().mode, Mode::PerUser);
		assert_eq!(data_dir(p.clone()), p);
		assert!(
			lease::may_host().is_ok(),
			"per-user mode never gates hosting"
		);
	}

	#[test]
	fn lease_release_needs_consecutive_inactive_ticks() {
		assert_eq!(lease::next_inactive(0, true), (0, false));
		assert_eq!(lease::next_inactive(0, false), (1, false));
		assert_eq!(lease::next_inactive(1, false), (2, true));
		assert_eq!(lease::next_inactive(1, true), (0, false));
	}
}
