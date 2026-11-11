//! Minimal append-only file log for the service + helpers: `<exe dir>\logs\sysagent.log`
//! (one file, all sessions, truncated past 2 MB). Falls back to stderr when the file cannot
//! be opened (e.g. run by hand without admin).
//!
//! Deliberately NOT under `%ProgramData%`: users may create files there, so a pre-planted
//! link would turn SYSTEM's log append into an arbitrary file write. The exe dir is
//! admin-only (`install` refuses anything outside Program Files).

use std::io::Write;
use std::sync::Mutex;

static FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);

pub fn init(tag: &str) {
	let Some(dir) = std::env::current_exe()
		.ok()
		.and_then(|e| e.parent().map(|d| d.join("logs")))
	else {
		return;
	};
	let _ = std::fs::create_dir_all(&dir);
	let path = dir.join("sysagent.log");
	if std::fs::metadata(&path)
		.map(|m| m.len() > 2 * 1024 * 1024)
		.unwrap_or(false)
	{
		let _ = std::fs::remove_file(&path);
	}
	if let Ok(f) = std::fs::OpenOptions::new()
		.create(true)
		.append(true)
		.open(&path)
	{
		*FILE.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
	}
	line(&format!(
		"--- {tag} start, pid {} v{}",
		std::process::id(),
		env!("CARGO_PKG_VERSION")
	));
}

pub fn line(msg: &str) {
	let ts = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_secs())
		.unwrap_or(0);
	let text = format!("{ts} [{}] {msg}\n", std::process::id());
	let mut g = FILE.lock().unwrap_or_else(|e| e.into_inner());
	match g.as_mut() {
		Some(f) => {
			let _ = f.write_all(text.as_bytes());
		}
		None => eprint!("{text}"),
	}
}

macro_rules! log {
	($($t:tt)*) => { $crate::log::line(&format!($($t)*)) };
}
pub(crate) use log;
