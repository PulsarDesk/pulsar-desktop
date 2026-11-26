//! A second launch of Pulsar while this user's Pulsar already runs.
//!
//! One Pulsar per user and session (Windows: `Local\PulsarSingletonMutex`, Unix: a per-uid
//! flock). A second process (a double-click, the Start menu, a `--connect` shortcut, an
//! autostart entry) never becomes a second app: it hands its request to the running one and
//! exits. Before 2026-11-26 it only said "show your window", so `--connect` did nothing when
//! Pulsar sat in the tray (issue #17), and on Windows it asked for UAC first every time.
//!
//! The request travels over a local channel only this user can use:
//! - Windows: the named pipe `\\.\pipe\pulsar-launch-<session>-<user SID>` (`instance_win.rs`).
//!   A second launch tries it BEFORE the elevation relaunch, so a running Pulsar is reached
//!   without a UAC prompt.
//! - Linux / macOS: the per-uid Unix socket (`instance_unix.rs`).
//!
//! What the running Pulsar does with it (see `docs/agents/modes.md` "Second launch"):
//! - `Show`: bring the window up (the splash while it still starts).
//! - `Background`: nothing (`--background` = "start without a window"; it is already running).
//! - `Connect`: queue the target for the UI (`take_cli_connects`), bring the window up and emit
//!   `cli-connect`. The UI connects like a `--connect` launch does (a new tab; an open session
//!   to the same device is focused instead). `return_to_tray` = the window was hidden, so the
//!   UI hides it again when that session ends.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::events::AutoConnect;

/// What a second launch asks the running Pulsar for.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum LaunchRequest {
	/// A plain launch: show the window. `token` = the launcher's activation token (Wayland /
	/// X11 startup id) so the compositor lets the window take focus.
	Show {
		#[serde(default)]
		token: String,
	},
	/// `--background`: start without a window. Already running, so nothing to do.
	Background,
	/// `--connect <id|ip> …`: connect to that device.
	Connect {
		target: AutoConnect,
		#[serde(default)]
		token: String,
	},
}

impl LaunchRequest {
	/// Carries a connect password: only ever sent to a verified Pulsar of this user.
	pub(crate) fn has_secret(&self) -> bool {
		matches!(self, Self::Connect { target, .. } if !target.pw.is_empty())
	}

	/// One line on the wire (`pulsar1 <json>`).
	pub(crate) fn to_line(&self) -> String {
		format!("pulsar1 {}\n", serde_json::to_string(self).unwrap_or_default())
	}

	/// Parse one line. The pre-2026-11-26 Unix form `show <token>` still counts as `Show`.
	pub(crate) fn from_line(line: &str) -> Option<Self> {
		let line = line.trim();
		if let Some(json) = line.strip_prefix("pulsar1 ") {
			return serde_json::from_str(json).ok();
		}
		line.strip_prefix("show").map(|t| Self::Show {
			token: t.trim().to_string(),
		})
	}
}

/// The value after flag `n` in `args`.
fn flag(args: &[String], n: &str) -> Option<String> {
	args.iter()
		.position(|a| a == n)
		.and_then(|i| args.get(i + 1))
		.cloned()
}

/// Parse the auto-connect CLI: `--connect <id|ip> [--connect-pw <pw>] [--mode game|remote]
/// [--app <name|id>] [--nofullscreen] [--kiosk]`. `--mode` defaults to remote (an unknown
/// value too); `--app` is the host app/game to launch in game mode (empty / "Desktop" =
/// stream the whole desktop); `--nofullscreen` (alias `--no-fullscreen`) keeps the session
/// windowed; `--kiosk` = an appliance that stays on its home screen after the session.
pub(crate) fn parse_auto_connect(args: &[String]) -> Option<AutoConnect> {
	let id = flag(args, "--connect").filter(|s| !s.trim().is_empty())?;
	Some(AutoConnect {
		id,
		pw: flag(args, "--connect-pw").unwrap_or_default(),
		mode: flag(args, "--mode")
			.map(|m| m.to_ascii_lowercase())
			.filter(|m| m == "game" || m == "remote")
			.unwrap_or_else(|| "remote".into()),
		app: flag(args, "--app").unwrap_or_default(),
		nofullscreen: args
			.iter()
			.any(|a| a == "--nofullscreen" || a == "--no-fullscreen"),
		kiosk: args.iter().any(|a| a == "--kiosk"),
	})
}

/// The request this process's command line makes of a running Pulsar.
pub(crate) fn request_from_args(args: &[String], token: String) -> LaunchRequest {
	if let Some(target) = parse_auto_connect(args) {
		return LaunchRequest::Connect { target, token };
	}
	if args.iter().any(|a| a == crate::autostart::BACKGROUND_FLAG) {
		return LaunchRequest::Background;
	}
	LaunchRequest::Show { token }
}

/// A `Connect` handed to the UI.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CliConnect {
	pub(crate) target: AutoConnect,
	/// The window was hidden (tray) when the request came: hide it again after the session.
	pub(crate) return_to_tray: bool,
}

/// Requests that arrived before `setup` had an AppHandle, and the handle once it has one.
struct Inbox {
	app: Option<AppHandle>,
	early: Vec<LaunchRequest>,
}

static INBOX: Mutex<Inbox> = Mutex::new(Inbox {
	app: None,
	early: Vec::new(),
});

/// Connect requests waiting for the UI to pick them up.
static PENDING: Mutex<Vec<CliConnect>> = Mutex::new(Vec::new());

/// A request from a second launch (any thread). Queued until [`attach`] when the app is not
/// built yet: the channel starts right after the single-instance lock, before Tauri.
pub(crate) fn deliver(req: LaunchRequest) {
	let mut inbox = INBOX.lock().unwrap_or_else(|e| e.into_inner());
	match inbox.app.clone() {
		Some(app) => {
			drop(inbox);
			let h = app.clone();
			let _ = app.run_on_main_thread(move || handle(&h, req));
		}
		None => inbox.early.push(req),
	}
}

/// Tests: the requests queued before an app was attached.
#[cfg(test)]
pub(crate) fn take_early() -> Vec<LaunchRequest> {
	std::mem::take(&mut INBOX.lock().unwrap_or_else(|e| e.into_inner()).early)
}

/// `setup`: from now on requests reach the app; run the ones that came earlier.
pub(crate) fn attach(app: &AppHandle) {
	let early = {
		let mut inbox = INBOX.lock().unwrap_or_else(|e| e.into_inner());
		inbox.app = Some(app.clone());
		std::mem::take(&mut inbox.early)
	};
	for req in early {
		let h = app.clone();
		let _ = app.run_on_main_thread(move || handle(&h, req));
	}
}

/// The main window is on screen (not hidden to the tray, not still to be built).
fn window_visible(app: &AppHandle) -> bool {
	crate::splash::is_done()
		&& app
			.get_webview_window("main")
			.and_then(|w| w.is_visible().ok())
			.unwrap_or(false)
}

/// Main thread: act on a second launch's request.
fn handle(app: &AppHandle, req: LaunchRequest) {
	match req {
		LaunchRequest::Show { token } => {
			tracing::info!("another launch asked to surface the window");
			surface(app, &token);
		}
		LaunchRequest::Background => {
			tracing::info!("another launch with --background: already running, nothing to do");
		}
		LaunchRequest::Connect { target, token } => {
			let return_to_tray = !target.kiosk && !window_visible(app);
			tracing::info!(id = %target.id, mode = %target.mode, return_to_tray, "another launch asked to connect");
			PENDING
				.lock()
				.unwrap_or_else(|e| e.into_inner())
				.push(CliConnect {
					target,
					return_to_tray,
				});
			surface(app, &token);
			let _ = app.emit("cli-connect", ());
		}
	}
}

/// Show + raise the main window, or the splash while the app still starts (the main window
/// follows on its own once the UI is ready). Builds the window first when a background start
/// never made it (`ui_start`).
pub(crate) fn surface(app: &AppHandle, token: &str) {
	crate::ui_start::request_now();
	if !crate::splash::is_done() {
		crate::splash::present(app, token);
		return;
	}
	let Some(w) = app.get_webview_window("main") else { return };
	#[cfg(target_os = "linux")]
	if !token.is_empty() {
		use gtk::prelude::GtkWindowExt;
		if let Ok(g) = w.gtk_window() {
			g.set_startup_id(token);
		}
	}
	#[cfg(not(target_os = "linux"))]
	let _ = token;
	let _ = w.show();
	let _ = w.unminimize();
	let _ = w.set_focus();
}

/// The UI takes the connect requests of later launches (on the `cli-connect` event and once
/// after boot, for any that came while it loaded).
#[tauri::command]
pub(crate) fn take_cli_connects() -> Vec<CliConnect> {
	std::mem::take(&mut *PENDING.lock().unwrap_or_else(|e| e.into_inner()))
}

/// The UI started a NEW session for a CLI connect: engage the keyboard capture right away,
/// as a `--connect` launch does (one-shot, `kbdhook`).
#[tauri::command]
pub(crate) fn arm_cli_engage() {
	crate::kbdhook::arm_kiosk_engage();
}

/// A session a CLI connect started (non-kiosk) has ended and nothing else is open: go back
/// to the tray. Without a usable tray the window stays (no way to bring it back otherwise);
/// with "Run in system tray" off, closing means quitting, so quit — unless `quit_ok` is false
/// (someone is connected to this PC).
#[tauri::command]
pub(crate) fn cli_session_done(app: AppHandle, quit_ok: bool) {
	use std::sync::atomic::Ordering;
	let st = app.state::<crate::state::AppState>();
	if st.tray_missing.load(Ordering::Relaxed) {
		return;
	}
	if st.tray_disabled.load(Ordering::Relaxed) {
		if quit_ok {
			tracing::info!("CLI session ended, tray off: quitting");
			app.exit(0);
		}
		return;
	}
	tracing::info!("CLI session ended: back to the tray");
	if let Some(w) = app.get_webview_window("main") {
		let _ = w.hide();
	}
}

/// What became of a second launch's request.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Forward {
	/// No Pulsar of this user runs here: start normally.
	NotRunning,
	/// The running Pulsar took it: exit.
	Delivered,
	/// One runs but could not be reached (still starting for too long, or an old build): exit.
	Unreachable,
	/// This user's window-less host (`--headless-host`) runs: tell the user, exit.
	Headless,
}

/// Hand this process's request to a running Pulsar of this user, if there is one.
pub(crate) fn forward(args: &[String]) -> Forward {
	#[cfg(windows)]
	{
		crate::instance_win::forward(args)
	}
	#[cfg(unix)]
	{
		crate::instance_unix::forward(args)
	}
}

/// After [`forward`] says another Pulsar holds the lock: tell the user why nothing opens.
pub(crate) fn explain(f: &Forward) {
	if *f != Forward::Headless {
		return;
	}
	if crate::i18n::os_is_turkish() {
		crate::i18n::set_lang(&pulsar_core::config::Language::Tr);
	}
	let text = crate::i18n::t("launch.headlessRunning");
	tracing::warn!("a window-less Pulsar host runs for this user; not opening a window");
	#[cfg(windows)]
	crate::instance_win::message_box(text);
	#[cfg(unix)]
	eprintln!("pulsar: {text}");
}

#[cfg(test)]
mod tests {
	use super::*;

	fn argv(a: &[&str]) -> Vec<String> {
		std::iter::once("pulsar")
			.chain(a.iter().copied())
			.map(String::from)
			.collect()
	}

	#[test]
	fn plain_launch_asks_to_show() {
		assert_eq!(
			request_from_args(&argv(&[]), "tok".into()),
			LaunchRequest::Show { token: "tok".into() }
		);
	}

	#[test]
	fn background_launch_asks_for_nothing() {
		assert_eq!(
			request_from_args(&argv(&["--background"]), String::new()),
			LaunchRequest::Background
		);
	}

	#[test]
	fn connect_launch_carries_every_flag() {
		let r = request_from_args(
			&argv(&[
				"--connect", "192.168.1.10:63577", "--connect-pw", "example", "--mode", "GAME",
				"--app", "Desktop", "--nofullscreen", "--kiosk",
			]),
			String::new(),
		);
		let LaunchRequest::Connect { target, .. } = &r else {
			panic!("{r:?}")
		};
		assert_eq!(target.id, "192.168.1.10:63577");
		assert_eq!(target.pw, "example");
		assert_eq!(target.mode, "game");
		assert_eq!(target.app, "Desktop");
		assert!(target.nofullscreen && target.kiosk);
		assert!(r.has_secret());
	}

	#[test]
	fn connect_wins_over_background_and_defaults_to_remote() {
		let r = request_from_args(&argv(&["--background", "--connect", "123", "--mode", "x"]), String::new());
		let LaunchRequest::Connect { target, .. } = r else {
			panic!()
		};
		assert_eq!(target.mode, "remote");
		assert!(!target.kiosk);
	}

	#[test]
	fn empty_connect_target_is_no_connect() {
		assert_eq!(parse_auto_connect(&argv(&["--connect", " "])), None);
		assert_eq!(parse_auto_connect(&argv(&["--connect"])), None);
	}

	#[test]
	fn wire_lines_round_trip() {
		let reqs = [
			LaunchRequest::Show { token: "a b".into() },
			LaunchRequest::Background,
			request_from_args(&argv(&["--connect", "1", "--connect-pw", "p w"]), "t".into()),
		];
		for r in reqs {
			let line = r.to_line();
			assert!(line.ends_with('\n') && !line[..line.len() - 1].contains('\n'));
			assert_eq!(LaunchRequest::from_line(&line), Some(r));
		}
	}

	#[test]
	fn old_unix_show_line_still_parses() {
		assert_eq!(
			LaunchRequest::from_line("show xdg-token\n"),
			Some(LaunchRequest::Show { token: "xdg-token".into() })
		);
		assert_eq!(LaunchRequest::from_line("garbage"), None);
	}
}
