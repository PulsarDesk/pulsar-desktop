//! One host per Windows user, across sessions (per-user mode).
//!
//! The single-instance mutex is per session (`Local\`), so the same Windows account signed
//! in twice (Duo / Pulsar Seat seats, a policy that allows several sessions per user) used to
//! run two Pulsars with the SAME identity: both registered with the relay and fought over the
//! ID. Now the first to go online holds `host.lock` in this user's data dir (an exclusive
//! open, released when that process ends); the other one stays a client and its Home says
//! where the host runs, retrying until the lock is free.
//!
//! Shared mode has its own machine-wide lease (`shared_mode::lease`); a Pulsar Split seat
//! host has its own data dir. Linux / macOS need nothing: one Pulsar per uid already.

#[cfg(windows)]
use std::sync::Mutex;

/// Error string the UI recognizes (Home shows `host.otherSession`).
#[cfg(windows)]
pub(crate) const ELSEWHERE: &str = "USER_HOST_ELSEWHERE";

#[cfg(windows)]
static HELD: Mutex<Option<std::fs::File>> = Mutex::new(None);

/// Gate for `bring_online` (after the shared-mode lease check).
#[cfg(windows)]
pub(crate) fn may_host(app: &tauri::AppHandle) -> Result<(), String> {
	use std::os::windows::fs::OpenOptionsExt;
	if crate::seat::active() || crate::shared_mode::resolved().mode == crate::shared_mode::Mode::Shared {
		return Ok(());
	}
	let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
	if held.is_some() {
		return Ok(());
	}
	let path = crate::util::data_dir(app).join("host.lock");
	match std::fs::OpenOptions::new()
		.read(true)
		.write(true)
		.create(true)
		.truncate(false)
		.share_mode(0) // exclusive: a second open fails with a sharing violation
		.open(&path)
	{
		Ok(f) => {
			*held = Some(f);
			Ok(())
		}
		// ERROR_SHARING_VIOLATION: this user's Pulsar in another session hosts.
		Err(e) if e.raw_os_error() == Some(32) => {
			tracing::info!("another session of this user already hosts; staying a client");
			Err(ELSEWHERE.into())
		}
		// Anything else (read-only dir, …): never block hosting over the lock.
		Err(e) => {
			tracing::warn!(path = %path.display(), "host.lock not usable ({e}); hosting anyway");
			Ok(())
		}
	}
}

#[cfg(not(windows))]
pub(crate) fn may_host(_app: &tauri::AppHandle) -> Result<(), String> {
	Ok(())
}
