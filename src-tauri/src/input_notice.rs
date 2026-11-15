//! Remote input that cannot reach this host (`DataMsg::InputUnavailable`, pulsar-core
//! `input::permission`): the client's in-session notice ("Your mouse and keyboard do not
//! reach the other side: …"), the host's toast, and when each is sent.
//!
//! * Host start ([`host_started`], unattended access only, once per process): the preflight
//!   (Linux `/dev/uinput`, macOS Accessibility) as a host toast, so an unattended host can be
//!   fixed before anyone connects. Every Pulsar goes online as a host, so a client-only user
//!   is not nagged on every launch.
//! * In a session, on input that goes to the desktop injector while control is granted
//!   ([`SessionNotice::preflight`]). The client only sends input from its session loop, which
//!   shows the notice; a notice sent at accept time reached the client while it was still in
//!   its caps / probe exchange, which drops everything else, and it also fired for
//!   metadata-only connections (the mobile game-library fetch). A view-only session, or one
//!   whose input all goes to a captured window, never gets here.
//! * The desktop injector failing to open ([`SessionNotice::open_failed`]). It is retried at
//!   most every [`OPEN_RETRY`] ([`SessionNotice::open_due`]), so a fix (the uinput rule) also
//!   heals the running session.
//! * Windows, live: the target window belongs to an elevated process while the host is not
//!   elevated and the SYSTEM helper is not connected (UIPI drops the input, `SendInput` still
//!   reports success) — keys: the foreground window, clicks / scrolls / pen / touch: the window
//!   under the cursor — and the secure desktop (UAC prompt, lock screen)
//!   ([`SessionNotice::check_window`]). No elevation flow is offered for it.
//!
//! Rate limits: the client hears one cause at most every [`NOTICE_REPEAT`] (another cause at
//! once), and the Windows window notice once per newly blocked window, at most every
//! [`WINDOW_REPEAT`]. The host toast is shown at most once per cause every [`TOAST_EVERY`]
//! across all sessions.
//!
//! Strings live in `lang/{tr,en}.json` (`input.*`); an unknown cause token (a newer host)
//! falls back to the generic text.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use pulsar_core::input::InputFailure;
use pulsar_core::service::DataMsg;
use tauri::{AppHandle, Manager};

/// The host toast for one cause at most this often (all sessions together).
const TOAST_EVERY: Duration = Duration::from_secs(10 * 60);
/// The client hears the same cause at most this often (a still-failing preflight, a failed
/// injector open).
const NOTICE_REPEAT: Duration = Duration::from_secs(60);
/// A desktop injector that failed to open is retried at most this often (on input).
const OPEN_RETRY: Duration = Duration::from_secs(5);
/// Windows: the same blocked window is re-announced to the client at most this often.
#[cfg_attr(not(windows), allow(dead_code))]
const WINDOW_REPEAT: Duration = Duration::from_secs(60);
/// Windows: the target window is looked at most this often (only on clicks / keys).
#[cfg_attr(not(windows), allow(dead_code))]
const WINDOW_POLL: Duration = Duration::from_millis(500);

fn reason_key(f: InputFailure, host: bool) -> &'static str {
	match (f, host) {
		(InputFailure::UinputDenied, false) => "input.client.uinput_denied",
		(InputFailure::MacAccessibility, false) => "input.client.mac_accessibility",
		(InputFailure::WinElevatedWindow, false) => "input.client.win_elevated",
		(InputFailure::WinSecureDesktop, false) => "input.client.win_secure_desktop",
		(InputFailure::AndroidControlOff, false) => "input.client.android_control_off",
		(InputFailure::AndroidControlMissing, false) => "input.client.android_control_missing",
		(InputFailure::Other, false) => "input.client.unavailable",
		(InputFailure::UinputDenied, true) => "input.host.uinput_denied",
		(InputFailure::MacAccessibility, true) => "input.host.mac_accessibility",
		(InputFailure::WinElevatedWindow, true) => "input.host.win_elevated",
		(InputFailure::WinSecureDesktop, true) => "input.host.win_secure_desktop",
		(InputFailure::AndroidControlOff, true) => "input.host.android_control_off",
		(InputFailure::AndroidControlMissing, true) => "input.host.android_control_missing",
		(InputFailure::Other, true) => "input.host.unavailable",
	}
}

/// The host's toast line (active language). Linux: when the uinput rule is already
/// installed, a click cannot install it, so the line says what is wrong instead.
pub(crate) fn host_text(f: InputFailure) -> String {
	#[cfg(target_os = "linux")]
	if f == InputFailure::UinputDenied && linux::rule_present().is_some() {
		return crate::i18n::t("input.host.uinput_no_seat").to_string();
	}
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
	// The secure desktop hides the toast anyway, and the person there sees the prompt.
	if f != InputFailure::WinSecureDesktop && toast_due(f, Instant::now()) {
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

/// Whether the client may hear about `f` now, given `told` (the last cause it heard, and
/// when); records it if so. Another cause at once, the same one after [`NOTICE_REPEAT`].
fn notice_due(told: &mut Option<(InputFailure, Instant)>, f: InputFailure, now: Instant) -> bool {
	match *told {
		Some((g, t)) if g == f && now.duration_since(t) < NOTICE_REPEAT => false,
		_ => {
			*told = Some((f, now));
			true
		}
	}
}

/// The host went online: with unattended access on (a machine meant to be reached while
/// nobody sits at it), run the preflight off the caller's thread and toast a problem, so it
/// can be fixed before anyone connects. Once per process: every Pulsar goes online as a host
/// (and the headless loop retries), so without the gate a client-only user would get the
/// toast on every launch. Everyone else hears about it on a session's first input.
pub(crate) fn host_started(app: &AppHandle) {
	static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
	let unattended = app
		.state::<crate::state::AppState>()
		.config
		.lock()
		.unwrap_or_else(|e| e.into_inner())
		.unattended_access;
	if !unattended
		|| crate::host::mock_capture()
		|| DONE.swap(true, std::sync::atomic::Ordering::Relaxed)
	{
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

/// The session's live input notices (owned by its `on_input` closure).
pub(crate) struct SessionNotice {
	out: tokio::sync::mpsc::Sender<DataMsg>,
	app: AppHandle,
	/// The last cause the client heard, and when ([`notice_due`]).
	told: Option<(InputFailure, Instant)>,
	/// The preflight passed in this session (it is not run again).
	preflight_ok: bool,
	preflight_at: Option<Instant>,
	/// The desktop injector's last failed open (retried after [`OPEN_RETRY`]).
	open_failed_at: Option<Instant>,
	#[cfg(windows)]
	window: WindowWatch,
}

impl SessionNotice {
	pub(crate) fn new(out: tokio::sync::mpsc::Sender<DataMsg>, app: AppHandle) -> Self {
		Self {
			out,
			app,
			told: None,
			preflight_ok: false,
			preflight_at: None,
			open_failed_at: None,
			#[cfg(windows)]
			window: WindowWatch::default(),
		}
	}

	/// Tell the client (and toast the host) about `f`, unless the client heard the same cause
	/// less than [`NOTICE_REPEAT`] ago.
	fn notify(&mut self, f: InputFailure, now: Instant) {
		if notice_due(&mut self.told, f, now) {
			tell_client(&self.out, f);
			toast(&self.app, f);
		}
	}

	/// Whether to (re)try opening the desktop injector now: never tried, or the last failure
	/// is [`OPEN_RETRY`] old (a fixed permission then heals the running session).
	pub(crate) fn open_due(&self, now: Instant) -> bool {
		self.open_failed_at
			.is_none_or(|t| now.duration_since(t) >= OPEN_RETRY)
	}

	/// The desktop injector opened; after a failure the cause was fixed meanwhile.
	pub(crate) fn opened(&mut self) {
		if self.open_failed_at.take().is_some() {
			tracing::info!("desktop input available again");
			self.told = None;
			self.preflight_ok = false;
			self.preflight_at = None;
		}
	}

	/// The desktop injector could not be created.
	pub(crate) fn open_failed(&mut self, e: &std::io::Error, now: Instant) {
		self.open_failed_at = Some(now);
		let f = if cfg!(target_os = "linux")
			&& matches!(
				e.kind(),
				std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
			) {
			InputFailure::UinputDenied
		} else {
			InputFailure::Other
		};
		self.notify(f, now);
	}

	/// Input is about to go to the desktop injector with control granted: run the preflight
	/// (Linux `/dev/uinput`; macOS Accessibility, where `DesktopInput::new` succeeds without the
	/// grant and every event is dropped). Once it passes it is not run again; while it fails it
	/// runs at most every [`NOTICE_REPEAT`].
	pub(crate) fn preflight(&mut self, now: Instant) {
		if self.preflight_ok
			|| self
				.preflight_at
				.is_some_and(|t| now.duration_since(t) < NOTICE_REPEAT)
		{
			return;
		}
		self.preflight_at = Some(now);
		match pulsar_core::input::permission::preflight() {
			None => self.preflight_ok = true,
			Some(f) => self.notify(f, now),
		}
	}

	/// Windows: before a click / key / scroll / pen / touch through the desktop injector, see
	/// whether the window it goes to swallows it: keys follow the foreground window, pointer
	/// input the window under the cursor (UIPI judges each by its target). `helper` = the
	/// SYSTEM helper injects (it reaches every window and the secure desktop).
	#[cfg(windows)]
	pub(crate) fn check_window(&mut self, ev: &pulsar_core::service::InputEvent, helper: bool) {
		use pulsar_core::input::permission;
		let Some(target) = window_target(ev) else {
			return;
		};
		if helper {
			return;
		}
		let now = Instant::now();
		if !self.window.poll_due(now) {
			return;
		}
		let blocked = match target {
			Target::Foreground => permission::foreground_window_blocked(),
			Target::Pointer => permission::pointer_target_blocked(),
		};
		if self.window.should_tell(blocked, now) {
			let f = if blocked == Some(permission::SECURE_DESKTOP) {
				InputFailure::WinSecureDesktop
			} else {
				InputFailure::WinElevatedWindow
			};
			tell_client(&self.out, f);
			toast(&self.app, f);
		}
	}
}

/// Which window an injected event goes to (Windows UIPI judges it by that window).
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
enum Target {
	Foreground,
	Pointer,
}

/// The window `ev` lands on, for the events worth a check (presses, not releases or moves).
#[cfg_attr(not(windows), allow(dead_code))]
fn window_target(ev: &pulsar_core::service::InputEvent) -> Option<Target> {
	use pulsar_core::service::InputEvent;
	match ev {
		InputEvent::Key { down: true, .. } | InputEvent::Char(_) => Some(Target::Foreground),
		InputEvent::PointerButton { down: true, .. }
		| InputEvent::Scroll { .. }
		| InputEvent::Pen(_)
		| InputEvent::Touch(_) => Some(Target::Pointer),
		_ => None,
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

	/// `blocked` = the blocked target window (or `None`). Tell the client about a window it
	/// was not told about, or the same one again after [`WINDOW_REPEAT`].
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
/// through pkexec (the AppImage, or a package install that predates the rule) and toasts
/// the result. Windows has nothing to offer (no elevation flow).
pub(crate) fn fix(app: &AppHandle, f: InputFailure) {
	match f {
		InputFailure::MacAccessibility => crate::kbdhook::request_permission(),
		#[cfg(target_os = "linux")]
		InputFailure::UinputDenied => {
			let app = app.clone();
			let _ = std::thread::Builder::new()
				.name("uinput-rule".into())
				.spawn(move || linux::install_uinput_rule(&app));
		}
		_ => {
			let _ = app;
		}
	}
}

#[cfg(target_os = "linux")]
mod linux {
	use pulsar_core::input::permission::preflight;

	/// The same rule the .deb / .rpm ship (`linux/70-pulsar-uinput.rules`).
	pub(super) const RULE: &str = include_str!("../linux/70-pulsar-uinput.rules");
	const INSTALLED: [&str; 2] = [
		"/usr/lib/udev/rules.d/70-pulsar-uinput.rules",
		"/etc/udev/rules.d/70-pulsar-uinput.rules",
	];

	/// The installed rule file, if any.
	pub(super) fn rule_present() -> Option<&'static str> {
		INSTALLED
			.iter()
			.copied()
			.find(|p| std::path::Path::new(p).exists())
	}

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

	fn fixed(app: &tauri::AppHandle) {
		// A running session's injector reopens on its next input (OPEN_RETRY).
		crate::toast::input_note(
			app,
			true,
			crate::i18n::t("input.host.uinput_fixed").to_string(),
		);
	}

	/// The rule is there, yet the node is not writable: usually no active local seat
	/// (uaccess grants the logged-in seat user only — SSH / headless sessions, a seat without
	/// uaccess do not get it). A password prompt would not help: say what would.
	fn no_seat(app: &tauri::AppHandle, rule: &str) {
		tracing::warn!(
			rule,
			"uinput rule present but /dev/uinput not writable (no active seat session?)"
		);
		crate::toast::input_note(
			app,
			false,
			crate::i18n::t("input.host.uinput_no_seat").to_string(),
		);
	}

	pub(super) fn install_uinput_rule(app: &tauri::AppHandle) {
		if preflight().is_none() {
			// Fixed meanwhile (a re-login, a package install).
			fixed(app);
			return;
		}
		if let Some(p) = rule_present() {
			no_seat(app, p);
			return;
		}
		let status = std::process::Command::new("pkexec")
			.args(["/bin/sh", "-c", &script()])
			.status();
		match status {
			Ok(s) if s.success() => {
				tracing::info!("uinput udev rule installed");
				// `udevadm trigger` applies the node's ACL asynchronously: give it a moment.
				for _ in 0..10 {
					if preflight().is_none() {
						return fixed(app);
					}
					std::thread::sleep(std::time::Duration::from_millis(300));
				}
				no_seat(app, INSTALLED[1]);
			}
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

	const ALL: [InputFailure; 7] = [
		InputFailure::UinputDenied,
		InputFailure::MacAccessibility,
		InputFailure::WinElevatedWindow,
		InputFailure::WinSecureDesktop,
		InputFailure::AndroidControlOff,
		InputFailure::AndroidControlMissing,
		InputFailure::Other,
	];

	#[test]
	fn every_reason_has_both_languages() {
		let mut keys: Vec<&str> = Vec::new();
		for f in ALL {
			for host in [false, true] {
				keys.push(reason_key(f, host));
			}
		}
		keys.extend(["input.host.uinput_no_seat", "input.host.uinput_fixed"]);
		for k in keys {
			assert_ne!(crate::i18n::t_in(k, false), k, "tr missing {k}");
			assert_ne!(
				crate::i18n::t_in(k, true),
				crate::i18n::t_in(k, false),
				"en missing {k}"
			);
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
	fn client_hears_a_cause_once_a_minute_and_another_cause_at_once() {
		let mut told = None;
		let t0 = Instant::now();
		assert!(notice_due(&mut told, InputFailure::UinputDenied, t0));
		// The failed injector open right after the preflight (same cause): not twice.
		assert!(!notice_due(
			&mut told,
			InputFailure::UinputDenied,
			t0 + Duration::from_secs(5)
		));
		assert!(notice_due(
			&mut told,
			InputFailure::Other,
			t0 + Duration::from_secs(6)
		));
		assert!(notice_due(
			&mut told,
			InputFailure::UinputDenied,
			t0 + Duration::from_secs(7)
		));
		assert!(notice_due(
			&mut told,
			InputFailure::UinputDenied,
			t0 + Duration::from_secs(7) + NOTICE_REPEAT
		));
	}

	#[test]
	fn keys_follow_the_foreground_window_and_pointer_input_the_window_under_it() {
		use pulsar_core::service::InputEvent;
		let key = |down| InputEvent::Key { code: 30, down };
		let btn = |down| InputEvent::PointerButton { button: 0, down };
		assert_eq!(window_target(&key(true)), Some(Target::Foreground));
		assert_eq!(window_target(&key(false)), None);
		assert_eq!(
			window_target(&InputEvent::Char('a')),
			Some(Target::Foreground)
		);
		assert_eq!(window_target(&btn(true)), Some(Target::Pointer));
		assert_eq!(window_target(&btn(false)), None);
		assert_eq!(
			window_target(&InputEvent::Scroll { dx: 0.0, dy: 1.0 }),
			Some(Target::Pointer)
		);
		assert_eq!(
			window_target(&InputEvent::PointerMotion { x: 0.5, y: 0.5 }),
			None
		);
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
