//! Remote input that cannot reach this host (`DataMsg::InputUnavailable`, pulsar-core
//! `input::permission`): the client's in-session notice ("Your mouse and keyboard do not
//! reach the other side: …"), the host's toast, and when each is sent.
//!
//! * Host start ([`host_started`]) and session start ([`session_started`]): the preflight
//!   (Linux `/dev/uinput`, macOS Accessibility). The client gets the notice at once instead of
//!   finding out by clicking into a picture that does not react.
//! * The desktop injector failing to open on the first input event ([`SessionNotice::open_failed`]).
//! * Windows, live: an elevated window in front while the host is not elevated and the SYSTEM
//!   helper is not connected (UIPI drops the input, `SendInput` still reports success)
//!   ([`SessionNotice::check_window`]). No elevation flow is offered for it.
//!
//! Rate limits: the client gets each cause once per session start, and the Windows window
//! notice once per newly blocked window, at most every [`WINDOW_REPEAT`]. The host toast is
//! shown at most once per cause every [`TOAST_EVERY`] across all sessions.
//!
//! Strings live in `lang/{tr,en}.json` (`input.*`); an unknown cause token (a newer host)
//! falls back to the generic text.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use pulsar_core::input::InputFailure;
use pulsar_core::service::DataMsg;
use tauri::AppHandle;

/// The host toast for one cause at most this often (all sessions together).
const TOAST_EVERY: Duration = Duration::from_secs(10 * 60);
/// Windows: the same blocked window is re-announced to the client at most this often.
#[cfg_attr(not(windows), allow(dead_code))]
const WINDOW_REPEAT: Duration = Duration::from_secs(60);
/// Windows: the foreground window is looked at most this often (only on clicks / keys).
#[cfg_attr(not(windows), allow(dead_code))]
const WINDOW_POLL: Duration = Duration::from_millis(500);

fn reason_key(f: InputFailure, host: bool) -> &'static str {
	match (f, host) {
		(InputFailure::UinputDenied, false) => "input.client.uinput_denied",
		(InputFailure::MacAccessibility, false) => "input.client.mac_accessibility",
		(InputFailure::WinElevatedWindow, false) => "input.client.win_elevated",
		(InputFailure::AndroidControlOff, false) => "input.client.android_control_off",
		(InputFailure::Other, false) => "input.client.unavailable",
		(InputFailure::UinputDenied, true) => "input.host.uinput_denied",
		(InputFailure::MacAccessibility, true) => "input.host.mac_accessibility",
		(InputFailure::WinElevatedWindow, true) => "input.host.win_elevated",
		(InputFailure::AndroidControlOff, true) => "input.host.android_control_off",
		(InputFailure::Other, true) => "input.host.unavailable",
	}
}

/// The host's toast line (active language).
pub(crate) fn host_text(f: InputFailure) -> String {
	crate::i18n::t(reason_key(f, true)).to_string()
}

/// The client's notice for the cause token `reason` the host reported.
pub(crate) fn client_text(reason: &str, english: bool) -> String {
	let title = crate::i18n::t_in("input.client.title", english);
	let why = crate::i18n::t_in(reason_key(InputFailure::from_wire(reason), false), english);
	format!("{title}: {why}")
}

/// Whether the host toast for `f` may show now (and if so, records it).
fn toast_due(f: InputFailure, now: Instant) -> bool {
	static LAST: Mutex<Option<HashMap<InputFailure, Instant>>> = Mutex::new(None);
	let mut g = LAST.lock().unwrap_or_else(|e| e.into_inner());
	let m = g.get_or_insert_with(HashMap::new);
	match m.get(&f) {
		Some(t) if now.duration_since(*t) < TOAST_EVERY => false,
		_ => {
			m.insert(f, now);
			true
		}
	}
}

fn toast(app: &AppHandle, f: InputFailure) {
	if toast_due(f, Instant::now()) {
		crate::toast::input_unavailable(app, f, host_text(f));
	}
}

fn tell_client(out: &tokio::sync::mpsc::Sender<DataMsg>, f: InputFailure) {
	tracing::warn!(
		reason = f.as_wire(),
		"remote input cannot reach this host → client notice"
	);
	let _ = out.try_send(DataMsg::InputUnavailable {
		reason: f.as_wire().to_string(),
	});
}

/// The host went online: run the preflight off the caller's thread and toast a problem, so
/// the person at the host can fix it before anyone connects.
pub(crate) fn host_started(app: &AppHandle) {
	if crate::host::mock_capture() {
		return;
	}
	let app = app.clone();
	let _ = std::thread::Builder::new()
		.name("input-preflight".into())
		.spawn(move || {
			if let Some(f) = pulsar_core::input::permission::preflight() {
				toast(&app, f);
			}
		});
}

/// A session was accepted: tell its client (and the host) when input cannot reach this
/// desktop. Skipped for a view-only session (its input is dropped on purpose anyway).
pub(crate) fn session_started(
	app: &AppHandle,
	out: &tokio::sync::mpsc::Sender<DataMsg>,
	view_only: bool,
) {
	if view_only || crate::host::mock_capture() {
		return;
	}
	if let Some(f) = pulsar_core::input::permission::preflight() {
		tell_client(out, f);
		toast(app, f);
	}
}

/// The session's live input notices (owned by its `on_input` closure).
pub(crate) struct SessionNotice {
	out: tokio::sync::mpsc::Sender<DataMsg>,
	app: AppHandle,
	#[cfg(windows)]
	window: WindowWatch,
}

impl SessionNotice {
	pub(crate) fn new(out: tokio::sync::mpsc::Sender<DataMsg>, app: AppHandle) -> Self {
		Self {
			out,
			app,
			#[cfg(windows)]
			window: WindowWatch::default(),
		}
	}

	/// The desktop injector could not be created (first input event of the session).
	pub(crate) fn open_failed(&mut self, e: &std::io::Error) {
		let f = if cfg!(target_os = "linux")
			&& matches!(
				e.kind(),
				std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
			) {
			InputFailure::UinputDenied
		} else {
			InputFailure::Other
		};
		tell_client(&self.out, f);
		toast(&self.app, f);
	}

	/// Windows: before a click / key / scroll / pen / touch through the desktop injector, see
	/// whether an elevated window in front swallows it. `helper` = the SYSTEM helper injects
	/// (it reaches every window, so nothing is blocked).
	#[cfg(windows)]
	pub(crate) fn check_window(&mut self, ev: &pulsar_core::service::InputEvent, helper: bool) {
		use pulsar_core::service::InputEvent;
		let discrete = match ev {
			InputEvent::Key { down, .. } | InputEvent::PointerButton { down, .. } => *down,
			InputEvent::Char(_)
			| InputEvent::Scroll { .. }
			| InputEvent::Pen(_)
			| InputEvent::Touch(_) => true,
			_ => false,
		};
		if !discrete || helper {
			return;
		}
		let now = Instant::now();
		if !self.window.poll_due(now) {
			return;
		}
		let blocked = pulsar_core::input::permission::foreground_window_blocked();
		if self.window.should_tell(blocked, now) {
			tell_client(&self.out, InputFailure::WinElevatedWindow);
			toast(&self.app, InputFailure::WinElevatedWindow);
		}
	}
}

/// Windows: which blocked window the client was last told about, and when.
#[derive(Default)]
#[cfg_attr(not(windows), allow(dead_code))]
struct WindowWatch {
	last_poll: Option<Instant>,
	told: Option<(isize, Instant)>,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl WindowWatch {
	fn poll_due(&mut self, now: Instant) -> bool {
		if self
			.last_poll
			.is_some_and(|t| now.duration_since(t) < WINDOW_POLL)
		{
			return false;
		}
		self.last_poll = Some(now);
		true
	}

	/// `blocked` = the blocked foreground window (or `None`). Tell the client about a window
	/// it was not told about, or the same one again after [`WINDOW_REPEAT`].
	fn should_tell(&mut self, blocked: Option<isize>, now: Instant) -> bool {
		let Some(hwnd) = blocked else {
			return false;
		};
		match self.told {
			Some((h, t)) if h == hwnd && now.duration_since(t) < WINDOW_REPEAT => false,
			_ => {
				self.told = Some((hwnd, now));
				true
			}
		}
	}
}

/// The host toast was clicked: fix what can be fixed from here. macOS asks for the
/// Accessibility grant (and opens the settings pane); Linux installs the uinput udev rule
/// through pkexec (the AppImage, or a package install that predates the rule). Windows has
/// nothing to offer (no elevation flow).
pub(crate) fn fix(f: InputFailure) {
	match f {
		InputFailure::MacAccessibility => crate::kbdhook::request_permission(),
		#[cfg(target_os = "linux")]
		InputFailure::UinputDenied => {
			let _ = std::thread::Builder::new()
				.name("uinput-rule".into())
				.spawn(linux::install_uinput_rule);
		}
		_ => {}
	}
}

#[cfg(target_os = "linux")]
mod linux {
	/// The same rule the .deb / .rpm ship (`linux/70-pulsar-uinput.rules`).
	pub(super) const RULE: &str = include_str!("../linux/70-pulsar-uinput.rules");
	const INSTALLED: [&str; 2] = [
		"/usr/lib/udev/rules.d/70-pulsar-uinput.rules",
		"/etc/udev/rules.d/70-pulsar-uinput.rules",
	];

	/// The root script: write the rule + the boot-time module load, load the module, and
	/// re-apply udev so the logged-in seat user gets the node's ACL now (no re-login).
	pub(super) fn script() -> String {
		format!(
			"set -e\n\
			 umask 022\n\
			 cat > /etc/udev/rules.d/70-pulsar-uinput.rules <<'PULSAR_RULE'\n{RULE}PULSAR_RULE\n\
			 mkdir -p /etc/modules-load.d\n\
			 printf 'uinput\\n' > /etc/modules-load.d/pulsar-uinput.conf\n\
			 modprobe uinput 2>/dev/null || true\n\
			 udevadm control --reload-rules\n\
			 udevadm trigger --subsystem-match=misc --sysname-match=uinput --action=change\n"
		)
	}

	pub(super) fn install_uinput_rule() {
		if let Some(p) = INSTALLED.iter().find(|p| std::path::Path::new(p).exists()) {
			// The rule is there, yet the node is not writable: usually no active local seat
			// (uaccess grants the logged-in seat user only — SSH / headless sessions do not
			// get it). A password prompt would not help; the docs say what to do.
			tracing::warn!(
				rule = p,
				"uinput rule present but /dev/uinput not writable (no active seat session?)"
			);
			return;
		}
		let status = std::process::Command::new("pkexec")
			.args(["/bin/sh", "-c", &script()])
			.status();
		match status {
			Ok(s) if s.success() => tracing::info!("uinput udev rule installed"),
			Ok(s) => {
				tracing::warn!(%s, "uinput udev rule not installed (cancelled or no polkit agent)")
			}
			Err(e) => tracing::warn!(%e, "uinput udev rule: pkexec unavailable"),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const ALL: [InputFailure; 5] = [
		InputFailure::UinputDenied,
		InputFailure::MacAccessibility,
		InputFailure::WinElevatedWindow,
		InputFailure::AndroidControlOff,
		InputFailure::Other,
	];

	#[test]
	fn every_reason_has_both_languages() {
		for f in ALL {
			for host in [false, true] {
				let k = reason_key(f, host);
				assert_ne!(crate::i18n::t_in(k, false), k, "tr missing {k}");
				assert_ne!(
					crate::i18n::t_in(k, true),
					crate::i18n::t_in(k, false),
					"en missing {k}"
				);
			}
		}
	}

	#[test]
	fn client_text_names_the_cause_and_tolerates_unknown_reasons() {
		let s = client_text("mac_accessibility", false);
		assert!(
			s.starts_with(crate::i18n::t_in("input.client.title", false)),
			"{s}"
		);
		assert!(
			s.ends_with(crate::i18n::t_in("input.client.mac_accessibility", false)),
			"{s}"
		);
		let s = client_text("from_the_future", true);
		assert!(
			s.ends_with(crate::i18n::t_in("input.client.unavailable", true)),
			"{s}"
		);
	}

	#[test]
	fn host_toast_is_rate_limited_per_cause() {
		let t0 = Instant::now();
		// Other causes in other tests share the global map; this cause is used only here.
		assert!(toast_due(InputFailure::AndroidControlOff, t0));
		assert!(!toast_due(
			InputFailure::AndroidControlOff,
			t0 + Duration::from_secs(60)
		));
		assert!(toast_due(
			InputFailure::AndroidControlOff,
			t0 + TOAST_EVERY + Duration::from_secs(1)
		));
	}

	#[test]
	fn window_notice_once_per_window_and_repeats_slowly() {
		let mut w = WindowWatch::default();
		let t0 = Instant::now();
		assert!(!w.should_tell(None, t0));
		assert!(w.should_tell(Some(1), t0));
		assert!(!w.should_tell(Some(1), t0 + Duration::from_secs(5)));
		assert!(!w.should_tell(None, t0 + Duration::from_secs(6)));
		assert!(
			w.should_tell(Some(2), t0 + Duration::from_secs(7)),
			"another window"
		);
		assert!(w.should_tell(Some(2), t0 + Duration::from_secs(7) + WINDOW_REPEAT));
		assert!(w.poll_due(t0));
		assert!(!w.poll_due(t0 + Duration::from_millis(100)));
		assert!(w.poll_due(t0 + WINDOW_POLL));
	}

	#[cfg(target_os = "linux")]
	#[test]
	fn linux_script_embeds_the_shipped_rule() {
		let s = linux::script();
		assert!(s.contains("TAG+=\"uaccess\""), "{s}");
		assert!(s.contains("/etc/udev/rules.d/70-pulsar-uinput.rules"));
		assert!(
			linux::RULE.ends_with('\n'),
			"heredoc terminator needs its own line"
		);
	}
}
