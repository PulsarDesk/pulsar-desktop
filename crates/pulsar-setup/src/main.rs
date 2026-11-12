//! Pulsar Setup — Discord-style branded bootstrapper (see Cargo.toml header).
//!
//! Modes:
//!   (none)          branded UI install
//!   /S, --silent    headless install/update into the registered (or default) dir
//!   --uninstall     remove the install (branded confirm → progressless removal)
//!   --updating <ver> --old-pid <pid> --app <exe>
//!                   the in-app updater's progress window (the app runs the NSIS installer
//!                   quietly and exits): waits for the new build to start, then closes;
//!                   offers to reopen Pulsar if the install never ran or failed
//!   --mock          UI walkthrough that touches NOTHING (no files, registry, shortcuts or
//!                   app launch): a fake payload is "extracted" with a real progress bar.
//!                   Combine with --uninstall or the --mock-* flags (see mock.rs) to reach
//!                   every screen. `PULSAR_SETUP_MOCK=1` does the same as --mock.
//!
//! The look follows the "Pulsar Kurulum" design (ui.rs): a frameless 520 × 380 window, light or
//! dark by the Windows app theme, and no animation.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod drivers;
mod install;
mod mock;
mod ui;

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};

use eframe::egui::{self, Align2, Color32, CornerRadius, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};

use ui::{Btn, Icon, Kind, Tok, Tone, H, PAD, W};

fn main() {
	let args: Vec<String> = std::env::args().collect();
	let has = |f: &str| args.iter().any(|a| a.eq_ignore_ascii_case(f));

	if has("/S") || has("--silent") {
		// Headless: updater / scripted install. Exit code carries the outcome.
		std::process::exit(match install::silent_install() {
			Ok(()) => 0,
			Err(e) => {
				eprintln!("pulsar-setup: {e}");
				1
			}
		});
	}

	// `--updating`: the in-app updater's branded progress window (no payload needed).
	let arg_after = |f: &str| {
		args.iter()
			.position(|a| a.eq_ignore_ascii_case(f))
			.and_then(|i| args.get(i + 1))
			.cloned()
	};
	let updating = has("--updating").then(|| Updating {
		version: arg_after("--updating").unwrap_or_default(),
		old_pid: arg_after("--old-pid").and_then(|p| p.parse().ok()).unwrap_or(0),
		app: arg_after("--app").map(PathBuf::from),
		mock: has("--mock-updating"),
	});

	let uninstall = has("--uninstall");
	let mock = (has("--mock") || std::env::var("PULSAR_SETUP_MOCK").is_ok_and(|v| v == "1")).then(|| mock::Mock {
		fail: has("--mock-fail"),
		no_webview2: has("--mock-no-webview2"),
		wv2_fail: has("--mock-wv2-fail"),
		update: has("--mock-update"),
		running: has("--mock-running"),
		no_payload: has("--mock-no-payload"),
		at: args.iter().find_map(|a| a.strip_prefix("--mock-at=")).and_then(mock::At::parse),
	});
	let title = if uninstall {
		"Pulsar Kaldırma"
	} else if updating.is_some() {
		"Pulsar Güncelleme"
	} else {
		"Pulsar Kurulum"
	};
	// One setup window at a time: a second double-click (or a second copy of the installer)
	// used to open another window that raced the first over the same files. The later one
	// brings the open window forward and quits. Mock runs (UI walkthroughs) are exempt.
	#[cfg(windows)]
	let _single = if mock.is_none() {
		match single_instance() {
			Some(guard) => Some(guard),
			None => {
				focus_existing();
				return;
			}
		}
	} else {
		None
	};
	let opts = eframe::NativeOptions {
		viewport: egui::ViewportBuilder::default()
			.with_title(title)
			.with_inner_size([W, H])
			.with_resizable(false)
			.with_decorations(false)
			.with_maximize_button(false)
			.with_minimize_button(false),
		centered: true,
		..Default::default()
	};
	let _ = eframe::run_native(
		title,
		opts,
		Box::new(move |cc| {
			cc.egui_ctx.set_fonts(ui::fonts());
			#[cfg(windows)]
			round_corners(cc);
			let mut app = SetupApp::new(uninstall, mock);
			if let Some(u) = updating {
				app.start_updating(u);
			}
			Ok(Box::new(app))
		}),
	);
}

/// Holds the "a setup window is open" mutex for the process lifetime. `None` = another setup
/// window (install, uninstall or the update window) is already open in this session.
#[cfg(windows)]
struct SingleInstance(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for SingleInstance {
	fn drop(&mut self) {
		unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
	}
}

#[cfg(windows)]
fn single_instance() -> Option<SingleInstance> {
	use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
	use windows_sys::Win32::System::Threading::CreateMutexW;
	// Per session (`Local\\`): the install is per user, and another user's setup is not ours.
	let name: Vec<u16> = "Local\\PulsarSetupWindow\0".encode_utf16().collect();
	// SAFETY: plain Win32 call; the handle is closed by SingleInstance's Drop.
	let h = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
	if h.is_null() {
		return Some(SingleInstance(h)); // can't tell → don't block the user
	}
	if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
		unsafe { windows_sys::Win32::Foundation::CloseHandle(h) };
		return None;
	}
	Some(SingleInstance(h))
}

/// Bring the already-open setup window to the front (whichever mode it is in).
#[cfg(windows)]
fn focus_existing() {
	use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE};
	for t in ["Pulsar Kurulum", "Pulsar Kaldırma", "Pulsar Güncelleme"] {
		let w: Vec<u16> = t.encode_utf16().chain(std::iter::once(0)).collect();
		// SAFETY: plain Win32 calls with a NUL-terminated title.
		let hwnd = unsafe { FindWindowW(std::ptr::null(), w.as_ptr()) };
		if !hwnd.is_null() {
			unsafe {
				ShowWindow(hwnd, SW_RESTORE);
				SetForegroundWindow(hwnd);
			}
			return;
		}
	}
}

/// Windows 11: rounded corners (and the system shadow) for the frameless window. Windows 10
/// ignores the attribute; the square window still works.
#[cfg(windows)]
fn round_corners(cc: &eframe::CreationContext<'_>) {
	use raw_window_handle::{HasWindowHandle, RawWindowHandle};
	use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE};
	let Ok(h) = cc.window_handle() else { return };
	if let RawWindowHandle::Win32(w) = h.as_raw() {
		let pref: i32 = 2; // DWMWCP_ROUND
		unsafe {
			DwmSetWindowAttribute(
				w.hwnd.get() as _,
				DWMWA_WINDOW_CORNER_PREFERENCE as _,
				&pref as *const i32 as *const _,
				std::mem::size_of::<i32>() as u32,
			);
		}
	}
}

/// `--updating` arguments (see the module doc).
struct Updating {
	version: String,
	old_pid: u32,
	app: Option<PathBuf>,
	/// `--mock-updating`: run the screen with a fake watcher (screenshots / testing).
	mock: bool,
}

enum Phase {
	/// The in-app update's progress window: the installer runs quietly in the background.
	Updating,
	/// The update never ran / failed; Pulsar can be reopened from here.
	UpdateFailed(String),
	Welcome,
	Installing,
	Done,
	Failed(String),
	ConfirmUninstall,
	Uninstalled,
}

/// The WebView2 runtime on the done screen (`None` = present / installed).
enum Wv2 {
	Missing,
	Downloading,
	Failed(String),
}

/// What a click (or Enter / Esc) asks for.
#[derive(Clone, Copy, PartialEq)]
enum Act {
	Install,
	Close,
	Launch,
	GetWebview2,
	Retry,
	CopyError,
	Uninstall,
	/// Reopen Pulsar after a failed update.
	Reopen,
}

struct SetupApp {
	phase: Phase,
	desktop_shortcut: bool,
	dir: PathBuf,
	total: usize,
	done_entries: usize,
	current: String,
	finishing: bool,
	/// The usbip-win2 installer runs (after the shortcuts/registry stage).
	driver: bool,
	rx: Option<Receiver<install::Msg>>,
	wv2: Option<Wv2>,
	wv2_rx: Option<Receiver<Result<(), String>>>,
	/// The version a previous install registered: the welcome screen becomes an update.
	update_from: Option<String>,
	/// The folder the user picked with "Değiştir" can't be written (shown under the box).
	dir_err: Option<String>,
	/// Pulsar is running (an update closes it first — the welcome screen says so).
	running: bool,
	have_payload: bool,
	copied: bool,
	/// The uninstall confirmation's default focus (Cancel) was set.
	focus_set: bool,
	dark: Option<bool>,
	/// `--updating`: target version, the app to reopen, start time and the watcher.
	upd_version: String,
	upd_app: Option<PathBuf>,
	upd_started: std::time::Instant,
	upd_rx: Option<Receiver<install::UpdMsg>>,
	/// UI walkthrough mode (`--mock`): every system-touching step is simulated.
	mock: Option<mock::Mock>,
}

impl SetupApp {
	/// "Değiştir": pick the install folder. A picked folder not named Pulsar gets a `Pulsar`
	/// subfolder (installers install INTO a folder you choose the parent of). The choice must be
	/// writable without admin rights — this installer is per user (no UAC); Program Files is not.
	fn pick_dir(&mut self) {
		#[cfg(windows)]
		{
			let start = self.dir.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| self.dir.clone());
			let Some(picked) = rfd::FileDialog::new().set_title("Pulsar'ın kurulacağı klasörü seç").set_directory(&start).pick_folder()
			else {
				return;
			};
			let dir = if picked.file_name().is_some_and(|n| n.eq_ignore_ascii_case("Pulsar")) {
				picked
			} else {
				picked.join("Pulsar")
			};
			if self.mock.is_some() {
				self.dir = dir;
				self.dir_err = None;
				return;
			}
			match install::writable(&dir) {
				Ok(()) => {
					self.dir = dir;
					self.dir_err = None;
				}
				Err(e) => {
					self.dir_err = Some(format!(
						"Bu klasöre yazılamıyor ({e}). Yönetici izni gerektirmeyen bir yer seç, örneğin kullanıcı klasörün."
					));
				}
			}
		}
	}

	fn new(uninstall: bool, mock: Option<mock::Mock>) -> Self {
		let (update_from, running, have_payload) = match mock {
			Some(m) => (m.update.then(|| "0.15.2".to_string()), m.running, !m.no_payload),
			None if uninstall => (None, false, install::have_payload()),
			None => (
				install::registered_install_dir().and_then(|_| install::registered_version()),
				install::pulsar_running(),
				install::have_payload(),
			),
		};
		let mut app = Self {
			phase: if uninstall { Phase::ConfirmUninstall } else { Phase::Welcome },
			desktop_shortcut: true,
			dir: install::registered_install_dir().unwrap_or_else(install::default_install_dir),
			total: 0,
			done_entries: 0,
			current: String::new(),
			finishing: false,
			driver: false,
			rx: None,
			wv2: None,
			wv2_rx: None,
			update_from,
			dir_err: None,
			running,
			have_payload,
			copied: false,
			focus_set: false,
			upd_version: String::new(),
			upd_app: None,
			upd_started: std::time::Instant::now(),
			upd_rx: None,
			dark: None,
			mock,
		};
		if let Some(at) = mock.and_then(|m| m.at) {
			use mock::At;
			(app.total, app.done_entries) = (48, 23);
			app.current = "resources/interception/x64/interception.dll".into();
			app.phase = match at {
				At::Installing | At::Finishing => Phase::Installing,
				At::Failed => Phase::Failed(mock::ERROR.to_string()),
				At::Uninstalled => Phase::Uninstalled,
				_ => Phase::Done,
			};
			app.finishing = at == At::Finishing;
			app.wv2 = match at {
				At::Webview2 => Some(Wv2::Missing),
				At::Webview2Downloading => Some(Wv2::Downloading),
				At::Webview2Failed => Some(Wv2::Failed("0x80072EE7".into())),
				_ => None,
			};
		}
		app
	}

	fn start_updating(&mut self, u: Updating) {
		self.phase = Phase::Updating;
		self.upd_version = u.version;
		self.upd_app = u.app;
		self.upd_started = std::time::Instant::now();
		let (tx, rx) = std::sync::mpsc::channel();
		self.upd_rx = Some(rx);
		let (old_pid, mock) = (u.old_pid, u.mock);
		std::thread::spawn(move || {
			if mock {
				std::thread::sleep(std::time::Duration::from_secs(8));
				let _ = tx.send(install::UpdMsg::Failed("Deneme: kurulum başlamadı.".into()));
			} else {
				install::watch_update(old_pid, &tx);
			}
		});
	}

	fn start_install(&mut self) {
		let (tx, rx): (Sender<install::Msg>, Receiver<install::Msg>) = std::sync::mpsc::channel();
		self.rx = Some(rx);
		self.phase = Phase::Installing;
		(self.total, self.done_entries, self.finishing, self.driver, self.copied) = (0, 0, false, false, false);
		self.current.clear();
		let dir = self.dir.clone();
		let desktop = self.desktop_shortcut;
		if let Some(m) = self.mock {
			std::thread::spawn(move || mock::install(m, &tx));
			return;
		}
		std::thread::spawn(move || install::install(&dir, desktop, true, &tx));
	}

	fn start_webview2(&mut self) {
		let (tx, rx) = std::sync::mpsc::channel();
		self.wv2_rx = Some(rx);
		self.wv2 = Some(Wv2::Downloading);
		let mock = self.mock;
		std::thread::spawn(move || {
			let r = match mock {
				Some(m) => mock::install_webview2(m),
				None => install::install_webview2(),
			};
			let _ = tx.send(r);
		});
	}

	fn pump(&mut self) {
		if let Some(rx) = &self.rx {
			while let Ok(m) = rx.try_recv() {
				match m {
					install::Msg::Total(n) => self.total = n,
					install::Msg::Entry(i, name) => {
						self.done_entries = i + 1;
						self.current = name;
					}
					install::Msg::Finishing => self.finishing = true,
					install::Msg::Driver => self.driver = true,
					install::Msg::Done => {
						let missing = match self.mock {
							Some(m) => m.no_webview2,
							None => !install::webview2_present(),
						};
						self.wv2 = missing.then_some(Wv2::Missing);
						self.phase = Phase::Done;
					}
					install::Msg::Err(e) => self.phase = Phase::Failed(e),
				}
			}
		}
		if let Some(rx) = &self.upd_rx {
			if let Ok(m) = rx.try_recv() {
				self.upd_rx = None;
				match m {
					install::UpdMsg::Done => self.phase = Phase::Done,
					install::UpdMsg::Failed(e) => self.phase = Phase::UpdateFailed(e),
				}
			}
		}
		if let Some(rx) = &self.wv2_rx {
			if let Ok(r) = rx.try_recv() {
				self.wv2 = match r {
					Ok(()) => None,
					Err(code) => Some(Wv2::Failed(code)),
				};
				self.wv2_rx = None;
			}
		}
	}

	fn act(&mut self, a: Act, ctx: &egui::Context) {
		match a {
			Act::Install | Act::Retry if matches!(self.phase, Phase::Welcome | Phase::Failed(_)) => self.start_install(),
			Act::Retry | Act::GetWebview2 => self.start_webview2(),
			Act::Install => {}
			Act::Close => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
			Act::Launch => {
				if self.mock.is_none() {
					install::launch_app(&self.dir);
				}
				ctx.send_viewport_cmd(egui::ViewportCommand::Close);
			}
			Act::CopyError => {
				if let Phase::Failed(e) = &self.phase {
					ctx.copy_text(e.clone());
					self.copied = true;
				}
			}
			Act::Reopen => {
				if let Some(app) = &self.upd_app {
					// The app's update marker would make it exit as "updating" — tell it why.
					install::launch_exe(app, &["--update-failed"]);
				}
				ctx.send_viewport_cmd(egui::ViewportCommand::Close);
			}
			Act::Uninstall => {
				if self.mock.is_none() {
					install::uninstall();
				}
				self.phase = Phase::Uninstalled;
			}
		}
	}
}

impl eframe::App for SetupApp {
	fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
		let ctx = ui.ctx().clone();
		self.pump();
		// Repaint only while something is in flight (the extract's progress, the WebView2
		// download); a static screen redraws on input alone.
		// The update window closes itself once the new build is up.
		if matches!(self.phase, Phase::Done) && self.upd_app.is_some() {
			ctx.send_viewport_cmd(egui::ViewportCommand::Close);
		}
		if matches!(self.phase, Phase::Installing | Phase::Updating) || self.wv2_rx.is_some() {
			ctx.request_repaint_after(std::time::Duration::from_millis(33));
		}
		// `PULSAR_SETUP_THEME=light|dark` forces one (screenshots / testing); otherwise the Windows
		// app theme decides, light when unknown.
		let dark = match std::env::var("PULSAR_SETUP_THEME").as_deref() {
			Ok("dark") => true,
			Ok("light") => false,
			_ => matches!(ctx.system_theme(), Some(egui::Theme::Dark)),
		};
		let t = if dark { ui::DARK } else { ui::LIGHT };
		if self.dark != Some(dark) {
			self.dark = Some(dark);
			let mut v = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
			v.panel_fill = t.bg;
			v.window_fill = t.bg;
			v.extreme_bg_color = t.sunken;
			v.override_text_color = Some(t.text);
			v.selection.bg_fill = t.accent.gamma_multiply(0.35);
			v.selection.stroke = Stroke::new(1.0, t.accent);
			ctx.set_visuals(v);
		}
		egui::CentralPanel::default().frame(egui::Frame::NONE.fill(t.bg)).show(ui, |ui| {
			let acts = self.draw(ui, &t);
			for a in acts {
				self.act(a, &ctx);
			}
		});
	}
}

/// A screen's content: hero texts, the right-hand pill, which footer note and buttons.
struct Screen<'a> {
	glow: bool,
	neutral: bool,
	title: String,
	sub: String,
	pill: Pill,
	note: Option<(Icon, &'a str)>,
	/// Footer buttons left → right; the LAST enabled primary/danger one is the Enter default.
	buttons: Vec<(Btn<'a>, Act)>,
	closable: bool,
}

enum Pill {
	None,
	Version(String),
	Update(String, String),
	Dev,
}

impl SetupApp {
	/// Draw the whole window; returns the actions clicked this frame.
	fn draw(&mut self, ui: &mut egui::Ui, t: &Tok) -> Vec<Act> {
		let o = ui.max_rect().min;
		let at = |x: f32, y: f32| o + Vec2::new(x, y);
		let p = ui.painter().clone();
		let ver = install::version();
		let mut acts = Vec::new();

		let launch = Btn::new("Pulsar'ı Başlat", Kind::Ghost);
		let sc = match &self.phase {
			Phase::Welcome if !self.have_payload => Screen {
				glow: true,
				neutral: false,
				title: "Pulsar'ı kur".into(),
				sub: "Uzak masaüstü · oyun akışı".into(),
				pill: Pill::Dev,
				note: None,
				buttons: vec![(Btn::new("Kapat", Kind::Ghost), Act::Close)],
				closable: true,
			},
			Phase::Welcome => {
				let upd = self.update_from.is_some();
				Screen {
					glow: true,
					neutral: false,
					title: if upd { "Pulsar'ı güncelle" } else { "Pulsar'ı kur" }.into(),
					sub: "Uzak masaüstü · oyun akışı".into(),
					pill: match &self.update_from {
						Some(old) => Pill::Update(format!("v{old}"), format!("v{ver}")),
						None => Pill::Version(format!("v{ver}")),
					},
					note: Some((Icon::User, "Yalnızca bu kullanıcı için · yönetici izni gerekmez")),
					buttons: vec![(Btn::new(if upd { "Güncelle" } else { "Kur" }, Kind::Primary), Act::Install)],
					closable: true,
				}
			}
			Phase::Installing => Screen {
				glow: true,
				neutral: false,
				title: "Kuruluyor…".into(),
				sub: format!("Pulsar v{ver} · birkaç saniye sürer"),
				pill: Pill::None,
				note: Some((Icon::Lock, "Kurulum bitene kadar pencere kapatılamaz.")),
				buttons: vec![],
				closable: false,
			},
			Phase::Done => match &self.wv2 {
				None => Screen {
					glow: true,
					neutral: false,
					title: "Pulsar hazır".into(),
					sub: format!("v{ver} kuruldu"),
					pill: Pill::None,
					note: None,
					buttons: vec![
						(Btn::new("Kapat", Kind::Quiet), Act::Close),
						(Btn::new("Pulsar'ı Başlat", Kind::Primary).icon_after(Icon::Arrow), Act::Launch),
					],
					closable: true,
				},
				Some(w) => Screen {
					glow: true,
					neutral: false,
					title: "Bir adım kaldı".into(),
					sub: "Pulsar kuruldu · WebView2 gerekli".into(),
					pill: Pill::None,
					note: None,
					buttons: match w {
						Wv2::Missing => vec![
							(launch, Act::Launch),
							(Btn::new("WebView2'yi indir ve kur", Kind::Primary).icon(Icon::Down), Act::GetWebview2),
						],
						Wv2::Downloading => vec![
							(launch.enabled(false), Act::Launch),
							(Btn::new("İndiriliyor…", Kind::Primary).icon(Icon::Down).enabled(false), Act::GetWebview2),
						],
						Wv2::Failed(_) => vec![
							(launch, Act::Launch),
							(Btn::new("Tekrar dene", Kind::Primary).icon(Icon::Retry), Act::Retry),
						],
					},
					closable: !matches!(w, Wv2::Downloading),
				},
			},
			Phase::Updating => Screen {
				glow: true,
				neutral: false,
				title: "Güncelleniyor…".into(),
				sub: if self.upd_version.is_empty() {
					"Pulsar · birkaç saniye sürer".into()
				} else {
					format!("Pulsar v{} · birkaç saniye sürer", self.upd_version)
				},
				pill: Pill::None,
				note: Some((Icon::Lock, "Bitince Pulsar kendiliğinden açılır.")),
				buttons: vec![],
				closable: false,
			},
			Phase::UpdateFailed(_) => Screen {
				glow: false,
				neutral: true,
				title: "Güncelleme tamamlanamadı".into(),
				sub: "Pulsar'ı açıp tekrar deneyebilirsin".into(),
				pill: Pill::None,
				note: None,
				buttons: vec![
					(Btn::new("Kapat", Kind::Quiet), Act::Close),
					(Btn::new("Pulsar'ı aç", Kind::Primary).icon_after(Icon::Arrow), Act::Reopen),
				],
				closable: true,
			},
			Phase::Failed(_) => Screen {
				glow: false,
				neutral: true,
				title: "Kurulum tamamlanamadı".into(),
				sub: format!("Pulsar v{ver} kurulamadı"),
				pill: Pill::None,
				note: None,
				buttons: vec![
					(Btn::new("Kapat", Kind::Quiet), Act::Close),
					(Btn::new("Tekrar dene", Kind::Primary).icon(Icon::Retry), Act::Retry),
				],
				closable: true,
			},
			Phase::ConfirmUninstall => Screen {
				glow: false,
				neutral: false,
				title: "Pulsar kaldırılsın mı?".into(),
				sub: format!("v{} · bu kullanıcı için kurulu", install::registered_version().unwrap_or_else(|| ver.to_string())),
				pill: Pill::None,
				note: None,
				buttons: vec![(Btn::new("Vazgeç", Kind::Ghost), Act::Close), (Btn::new("Kaldır", Kind::Danger), Act::Uninstall)],
				closable: true,
			},
			Phase::Uninstalled => Screen {
				glow: false,
				neutral: true,
				title: "Pulsar kaldırıldı".into(),
				sub: "Bu pencereyi kapatabilirsin.".into(),
				pill: Pill::None,
				note: None,
				buttons: vec![(Btn::new("Kapat", Kind::Primary), Act::Close)],
				closable: true,
			},
		};

		// ── Window, halo ─────────────────────────────────────────────────────────
		let full = Rect::from_min_size(o, Vec2::new(W, H));
		p.rect_filled(full, CornerRadius::ZERO, t.bg);
		if sc.glow {
			ui::glow(&p.with_clip_rect(full), at(54.0, 78.0), 150.0, t.accent_soft);
		}

		// ── Title bar: drag area + close ──────────────────────────────────────────
		let bar = Rect::from_min_size(o, Vec2::new(W, 40.0));
		let drag = ui.interact(bar, ui.scope_id().with("titlebar"), Sense::click_and_drag());
		if drag.drag_started() {
			ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
		}
		let win_title = if matches!(self.phase, Phase::ConfirmUninstall | Phase::Uninstalled) {
			"Pulsar Kaldırma"
		} else if self.upd_app.is_some() || matches!(self.phase, Phase::Updating | Phase::UpdateFailed(_)) {
			"Pulsar Güncelleme"
		} else {
			"Pulsar Kurulum"
		};
		p.text(at(PAD, 20.0), Align2::LEFT_CENTER, win_title, ui::semibold(12.0), t.muted);
		let close_rect = Rect::from_min_size(at(W - 6.0 - 32.0, 6.0), Vec2::new(32.0, 28.0));
		let close = ui.interact(close_rect, ui.scope_id().with("close"), if sc.closable { Sense::click() } else { Sense::hover() });
		if sc.closable {
			if close.hovered() {
				p.rect_filled(close_rect, CornerRadius::same(6), t.danger);
			}
			let c = if close.hovered() { Color32::WHITE } else { t.muted };
			ui::icon(&p, Icon::Close, close_rect.center() - Vec2::splat(8.0), 16.0, c, 1.4);
			if close.clicked() {
				acts.push(Act::Close);
			}
		} else {
			ui::icon(&p, Icon::Close, close_rect.center() - Vec2::splat(8.0), 16.0, t.border_strong, 1.4);
			close.on_hover_text("Kurulum sürerken kapatılamaz");
		}

		// ── Hero ───────────────────────────────────────────────────────────────────
		ui::mark(&p, Rect::from_min_size(at(PAD, 48.0), Vec2::splat(52.0)), t, sc.neutral);
		let pill_w = match &sc.pill {
			Pill::None => 0.0,
			Pill::Version(v) => ui::text_width(&p, v, ui::mono(11.5)) + 18.0,
			Pill::Update(a, b) => {
				ui::text_width(&p, a, ui::mono(11.5)) + ui::text_width(&p, " → ", ui::regular(11.5)) + ui::text_width(&p, b, ui::mono(11.5)) + 18.0
			}
			Pill::Dev => ui::text_width(&p, "dev", ui::mono(11.5)) + 18.0,
		};
		let tx = PAD + 52.0 + 16.0;
		let text_w = W - PAD - tx - if pill_w > 0.0 { pill_w + 12.0 } else { 0.0 };
		let mut title_job = ui::text_job(&sc.title, ui::display(20.0), t.text, 26.0, text_w, 1, false);
		title_job.sections[0].format.extra_letter_spacing = -0.4;
		ui::draw_job(&p, at(tx, 51.0), title_job);
		ui::draw_job(&p, at(tx, 79.0), ui::text_job(&sc.sub, ui::regular(13.0), t.muted, 18.0, text_w, 1, false));
		if pill_w > 0.0 {
			let r = Rect::from_min_size(at(W - PAD - pill_w, 51.0), Vec2::new(pill_w, 22.0));
			let (bg, fg) = if matches!(sc.pill, Pill::Dev) { (t.warn_soft, t.warn_text) } else { (t.track, t.muted) };
			p.rect_filled(r, CornerRadius::same(11), bg);
			let mut x = r.min.x + 9.0;
			let cy = r.center().y;
			match &sc.pill {
				Pill::Version(v) => {
					p.text(Pos2::new(x, cy), Align2::LEFT_CENTER, v, ui::mono(11.5), fg);
				}
				Pill::Dev => {
					p.text(Pos2::new(x, cy), Align2::LEFT_CENTER, "dev", ui::mono(11.5), fg);
				}
				Pill::Update(a, b) => {
					x += p.text(Pos2::new(x, cy), Align2::LEFT_CENTER, a, ui::mono(11.5), fg).width();
					x += p.text(Pos2::new(x, cy), Align2::LEFT_CENTER, " → ", ui::regular(11.5), t.faint).width();
					p.text(Pos2::new(x, cy), Align2::LEFT_CENTER, b, ui::mono(11.5), t.text);
				}
				Pill::None => {}
			}
		}

		// ── Body (x 28–492, from y 124, items 14 apart) ─────────────────────────────
		let bw = W - 2.0 * PAD;
		let mut y = 124.0;
		let gap = 14.0;
		let ver = install::version();
		match &self.phase {
			Phase::Welcome if !self.have_payload => {
				ui::banner(
					&p,
					at(PAD, y),
					bw,
					t,
					Tone::Warn,
					Icon::Warn,
					Some("Uygulama paketi yok"),
					"Bu derleme, içine gömülü uygulama paketi olmadan oluşturulmuş (geliştirme sürümü). Bu dosyayla kurulum yapılamaz.",
				);
			}
			Phase::Welcome => {
				if let Some(old) = &self.update_from {
					let mut s = format!("v{old} kurulu. Aynı klasöre güncellenecek.");
					if self.running {
						s.push_str(" Açık olan Pulsar, kurulum başlarken kapatılır.");
					}
					y += ui::banner(&p, at(PAD, y), bw, t, Tone::Info, Icon::Info, None, &s) + gap;
				}
				// A fresh install can go anywhere the user can write; an update stays put.
				let editable = self.update_from.is_none() && self.mock.is_none_or(|m| !m.update);
				let (h, change) = ui::path_box_edit(
					ui,
					at(PAD, y),
					bw,
					t,
					&self.dir.display().to_string(),
					editable,
					ui.scope_id().with("dir"),
				);
				y += h;
				if change {
					self.pick_dir();
				}
				if let Some(e) = &self.dir_err {
					y += 4.0;
					y += ui::draw_job(&p, at(PAD, y), ui::text_job(e, ui::regular(11.5), t.warn_text, 15.0, bw, 2, true)).y;
				}
				y += gap;
				let (_, clicked) =
					ui::checkbox(ui, at(PAD, y), t, "Masaüstüne kısayol ekle", self.desktop_shortcut, ui.scope_id().with("desktop"));
				if clicked {
					self.desktop_shortcut = !self.desktop_shortcut;
				}
			}
			Phase::Installing => {
				let (label, counter, frac, file) = if self.driver {
					(
						"Sanal DualSense sürücüsü (usbip-win2)…",
						format!("{0} / {0}", self.total),
						1.0,
						"Yönetici izni istenirse onayla.".to_string(),
					)
				} else if self.finishing {
					(
						"Kısayollar ve kayıt…",
						format!("{0} / {0}", self.total),
						1.0,
						if self.desktop_shortcut {
							"Başlat Menüsü · Masaüstü · Programlar ve Özellikler".to_string()
						} else {
							"Başlat Menüsü · Programlar ve Özellikler".to_string()
						},
					)
				} else {
					(
						"Dosyalar çıkarılıyor",
						format!("{} / {}", self.done_entries, self.total.max(1)),
						if self.total > 0 { self.done_entries as f32 / self.total as f32 } else { 0.0 },
						self.current.clone(),
					)
				};
				ui::progress(&p, at(PAD, y), bw, t, label, &counter, frac, &file);
			}
			Phase::Done => {
				let ok = |p: &egui::Painter, y: f32, text: &str, suffix: Option<String>| {
					ui::list_row(p, at(PAD, y), bw, Icon::Check, t.ok, text, t.text, suffix.as_deref().map(|s| (s, ui::regular(12.0))), t.muted)
				};
				match &self.wv2 {
					None => {
						y += ok(&p, y, "Uygulama dosyaları", Some(format!(" · {} dosya", self.total))) + 7.0;
						let shortcuts =
							if self.desktop_shortcut { "Başlat Menüsü ve masaüstü kısayolu" } else { "Başlat Menüsü kısayolu" };
						y += ok(&p, y, shortcuts, None) + 7.0;
						ok(&p, y, "Programlar ve Özellikler kaydı", None);
					}
					Some(w) => {
						y += ok(&p, y, &format!("Pulsar v{ver} kuruldu"), None) + gap;
						match w {
							Wv2::Missing => {
								ui::banner(
									&p,
									at(PAD, y),
									bw,
									t,
									Tone::Warn,
									Icon::Warn,
									Some("WebView2 çalışma zamanı bulunamadı"),
									"Pulsar'ın arayüzü için gerekli. Microsoft'tan indirip kurmak için internet bağlantısı gerekir.",
								);
							}
							Wv2::Downloading => {
								ui::banner(
									&p,
									at(PAD, y),
									bw,
									t,
									Tone::Info,
									Icon::Down,
									Some("WebView2 indiriliyor…"),
									"Microsoft sunucularından alınıyor. İnternet hızına göre bir dakika kadar sürebilir.",
								);
							}
							Wv2::Failed(code) => {
								ui::banner(
									&p,
									at(PAD, y),
									bw,
									t,
									Tone::Danger,
									Icon::Warn,
									Some("WebView2 indirilemedi"),
									&format!("İnternet bağlantını kontrol edip tekrar dene. ({code})"),
								);
							}
						}
					}
				}
			}
			Phase::Updating => {
				// The quiet installer reports no progress: ease towards 95 % over ~25 s, the
				// typical install time, so the bar keeps moving without claiming to be done.
				let s = self.upd_started.elapsed().as_secs_f32();
				let frac = 0.95 * (1.0 - (-s / 9.0).exp());
				ui::progress(&p, at(PAD, y), bw, t, "Dosyalar kuruluyor", "", frac, "Yönetici izni istenirse onayla.");
			}
			Phase::UpdateFailed(e) => {
				let e = e.clone();
				ui::banner(&p, at(PAD, y), bw, t, Tone::Warn, Icon::Warn, None, &e);
			}
			Phase::Failed(e) => {
				let e = e.clone();
				y += self.error_box(ui, at(PAD, y), bw, t, &e, &mut acts) + gap;
				ui::draw_job(
					&p,
					at(PAD, y),
					ui::text_job(
						"Dosya başka bir program tarafından kullanılıyor olabilir. Pulsar açıksa kapatıp tekrar dene.",
						ui::regular(12.5),
						t.muted,
						18.0,
						bw,
						0,
						false,
					),
				);
			}
			Phase::ConfirmUninstall => {
				y += ui::draw_job(
					&p,
					at(PAD, y),
					ui::text_job("Bu bilgisayardan şunlar silinecek:", ui::regular(13.0), t.muted, 19.0, bw, 0, false),
				)
				.y + 10.0;
				let rm = |p: &egui::Painter, y: f32, text: &str, suffix: Option<(&str, egui::FontId)>| {
					ui::list_row(p, at(PAD, y), bw, Icon::Minus, t.danger_text, text, t.text, suffix, t.muted)
				};
				y += rm(&p, y, "Uygulama dosyaları", Some(("  %LOCALAPPDATA%\\Programs\\Pulsar", ui::mono(11.0)))) + 7.0;
				y += rm(&p, y, "Başlat Menüsü ve masaüstü kısayolları", None) + 7.0;
				rm(&p, y, "Programlar ve Özellikler kaydı", None);
			}
			Phase::Uninstalled => {
				ui::list_row(
					&p,
					at(PAD, y),
					bw,
					Icon::Check,
					t.muted,
					"Uygulama dosyaları, kısayollar ve kayıt silindi.",
					t.muted,
					None,
					t.muted,
				);
			}
		}

		// ── Footer (y 316–380) ──────────────────────────────────────────────────────
		let foot = Rect::from_min_size(at(0.0, H - 64.0), Vec2::new(W, 64.0));
		p.rect_filled(foot, CornerRadius::ZERO, t.bar);
		p.line_segment([foot.left_top(), foot.right_top()], Stroke::new(1.0, t.border));
		let widths: Vec<f32> = sc.buttons.iter().map(|(b, _)| b.width(&p)).collect();
		let buttons_w: f32 = widths.iter().sum::<f32>() + 8.0 * widths.len().saturating_sub(1) as f32;
		if let Some((ic, note)) = sc.note {
			let nx = PAD + 14.0 + 8.0;
			let nw = W - 20.0 - buttons_w - 16.0 - nx;
			let g = p.layout_job(ui::text_job(note, ui::regular(12.0), t.muted, 16.0, nw, 2, false));
			ui::icon(&p, ic, at(PAD, foot.center().y - o.y - 7.0), 14.0, t.faint, 1.5);
			let gh = g.size().y;
			p.galley(Pos2::new(o.x + nx, foot.center().y - gh / 2.0), g, Color32::PLACEHOLDER);
		}
		let mut x = W - 20.0 - buttons_w;
		let default = sc.buttons.iter().rposition(|(b, _)| b.enabled && matches!(b.kind, Kind::Primary | Kind::Danger));
		let mut enter_act = default.map(|i| sc.buttons[i].1);
		for (i, ((b, a), w)) in sc.buttons.iter().zip(&widths).enumerate() {
			let rect = Rect::from_min_size(at(x, H - 64.0 + 15.0), Vec2::new(*w, 34.0));
			let id = ui.scope_id().with(("btn", i, b.label));
			let r = b.show(ui, rect, t, id);
			// Uninstall confirmation: Cancel holds the focus, so Enter can't remove by accident.
			if matches!(self.phase, Phase::ConfirmUninstall) && *a == Act::Close && !self.focus_set {
				r.request_focus();
				self.focus_set = true;
			}
			if r.has_focus() {
				enter_act = None; // egui turns Enter on a focused button into its click
			}
			if r.clicked() {
				acts.push(*a);
			}
			x += w + 8.0;
		}

		// ── Keyboard: Enter = the default action, Esc = close (not while installing) ───
		let (enter, esc) = ui.ctx().input(|i| (i.key_pressed(egui::Key::Enter), i.key_pressed(egui::Key::Escape)));
		if enter && ui.ctx().memory(|m| m.focused()).is_none() {
			if let Some(a) = enter_act {
				acts.push(a);
			}
		}
		if esc && sc.closable {
			acts.push(Act::Close);
		}

		// Window edge: separates the window from the desktop (most visible in dark).
		p.rect_stroke(full, CornerRadius::ZERO, Stroke::new(1.0, t.border_strong), StrokeKind::Inside);
		acts
	}

	/// The error box: header strip ("HATA AYRINTISI" + Copy) and the selectable, scrollable
	/// monospace detail (4 lines at most). Returns its height.
	fn error_box(&self, ui: &mut egui::Ui, top_left: Pos2, width: f32, t: &Tok, e: &str, acts: &mut Vec<Act>) -> f32 {
		let p = ui.painter().clone();
		let body_job = ui::text_job(e, ui::mono(11.5), t.text, 17.0, width - 24.0, 0, true);
		let body_h = p.layout_job(body_job).size().y.min(70.0).max(17.0);
		let h = 30.0 + 1.0 + body_h + 18.0;
		let rect = Rect::from_min_size(top_left, Vec2::new(width, h));
		p.rect_filled(rect, CornerRadius::same(8), t.sunken);
		p.rect_stroke(rect, CornerRadius::same(8), Stroke::new(1.0, t.border), StrokeKind::Inside);
		let mut head = ui::text_job("HATA AYRINTISI", ui::mono(10.5), t.faint, 14.0, width, 1, false);
		head.sections[0].format.extra_letter_spacing = 0.8;
		ui::draw_job(&p, top_left + Vec2::new(12.0, 8.0), head);
		p.line_segment(
			[top_left + Vec2::new(0.0, 30.0), top_left + Vec2::new(width, 30.0)],
			Stroke::new(1.0, t.border),
		);
		// "Kopyala" mini button.
		let label = if self.copied { "Kopyalandı" } else { "Kopyala" };
		let lw = ui::text_width(&p, label, ui::semibold(12.0));
		let mr = Rect::from_min_size(top_left + Vec2::new(width - 6.0 - (8.0 + 13.0 + 6.0 + lw + 8.0), 3.0), Vec2::new(8.0 + 13.0 + 6.0 + lw + 8.0, 24.0));
		let r = ui.interact(mr, ui.scope_id().with("copy"), Sense::click());
		let hov = r.hovered();
		if hov {
			p.rect_filled(mr, CornerRadius::same(6), t.track);
		}
		let c = if hov { t.text } else { t.muted };
		ui::icon(&p, if self.copied { Icon::Check } else { Icon::Copy }, mr.min + Vec2::new(8.0, 5.5), 13.0, c, 1.5);
		p.text(Pos2::new(mr.min.x + 8.0 + 13.0 + 6.0, mr.center().y), Align2::LEFT_CENTER, label, ui::semibold(12.0), c);
		if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
			acts.push(Act::CopyError);
		}
		// The detail: selectable, scrolls when longer than four lines.
		let body = Rect::from_min_size(top_left + Vec2::new(12.0, 31.0 + 9.0), Vec2::new(width - 24.0, body_h));
		ui.scope_builder(egui::UiBuilder::new().max_rect(body), |ui| {
			egui::ScrollArea::vertical().max_height(body_h).auto_shrink([false, true]).show(ui, |ui| {
				ui.add(
					egui::Label::new(egui::RichText::new(e).font(ui::mono(11.5)).color(t.text))
						.selectable(true)
						.wrap(),
				);
			});
		});
		h
	}
}
