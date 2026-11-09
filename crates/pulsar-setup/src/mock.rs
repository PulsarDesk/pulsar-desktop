//! `--mock`: a UI walkthrough of the installer that touches nothing on the system — the
//! progress screen runs over a made-up payload listing, every system step (extract,
//! shortcuts, registry, uninstaller copy, WebView2 download, app launch, uninstall) is
//! skipped. For trying the screens and their copy without a payload build or a real install.

use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::install::Msg;

/// Which mock path to walk (each a `--mock-*` flag).
#[derive(Clone, Copy)]
pub struct Mock {
	/// `--mock-fail`: fail half way through the extract (the "Kurulum tamamlanamadı" screen).
	pub fail: bool,
	/// `--mock-no-webview2`: report WebView2 as missing on the done screen.
	pub no_webview2: bool,
	/// `--mock-wv2-fail`: the WebView2 download fails (with --mock-no-webview2).
	pub wv2_fail: bool,
	/// `--mock-update`: a previous version (v0.15.2) is installed — the update screen.
	pub update: bool,
	/// `--mock-running`: Pulsar is running (the update screen says it will be closed).
	pub running: bool,
	/// `--mock-no-payload`: the dev build without an embedded payload.
	pub no_payload: bool,
	/// `--mock-at=<screen>`: open straight on a screen (design review / screenshots).
	pub at: Option<At>,
}

/// A screen `--mock-at` opens on.
#[derive(Clone, Copy, PartialEq)]
pub enum At {
	Installing,
	Finishing,
	Done,
	Webview2,
	Webview2Downloading,
	Webview2Failed,
	Failed,
	Uninstalled,
}

impl At {
	pub fn parse(s: &str) -> Option<Self> {
		Some(match s {
			"installing" => Self::Installing,
			"finishing" => Self::Finishing,
			"done" => Self::Done,
			"wv2" => Self::Webview2,
			"wv2-downloading" => Self::Webview2Downloading,
			"wv2-failed" => Self::Webview2Failed,
			"failed" => Self::Failed,
			"uninstalled" => Self::Uninstalled,
			_ => return None,
		})
	}
}

/// The error `--mock-fail` / `--mock-at=failed` shows.
pub const ERROR: &str =
	r"(mock) Dosya yazılamadı: C:\Users\kullanici\AppData\Local\Programs\Pulsar\resources\ffmpeg.exe — erişim engellendi (os error 5)";

/// The fake WebView2 download: a couple of seconds, then success (or `--mock-wv2-fail`).
pub fn install_webview2(m: Mock) -> Result<(), String> {
	std::thread::sleep(Duration::from_millis(2500));
	if m.wv2_fail {
		Err("0x80072EE7".to_string())
	} else {
		Ok(())
	}
}

/// What a real payload roughly holds (names for the progress line only).
const ENTRIES: &[&str] = &[
	"pulsar.exe",
	"pulsar-render.exe",
	"resources/ffmpeg.exe",
	"resources/ffplay.exe",
	"resources/icons/icon.ico",
	"resources/interception/install-interception.exe",
	"resources/interception/x64/interception.dll",
	"resources/vigembus/ViGEmBus_Setup_x64.msi",
	"resources/sdl/SDL3.dll",
	"resources/opus/opus.dll",
	"resources/lang/tr.json",
	"resources/lang/en.json",
	"resources/lang/ru.json",
	"resources/lang/kk.json",
	"WebView2Loader.dll",
	"LICENSE",
];

/// The fake extract: real progress messages at a human pace (~2.5 s), no I/O.
pub fn install(m: Mock, tx: &Sender<Msg>) {
	let total = ENTRIES.len() * 3;
	let _ = tx.send(Msg::Total(total));
	for i in 0..total {
		std::thread::sleep(Duration::from_millis(55));
		if m.fail && i == total / 2 {
			let _ = tx.send(Msg::Err(ERROR.to_string()));
			return;
		}
		let name = ENTRIES[i % ENTRIES.len()];
		let _ = tx.send(Msg::Entry(i, name.to_string()));
	}
	let _ = tx.send(Msg::Finishing);
	std::thread::sleep(Duration::from_millis(600));
	let _ = tx.send(Msg::Done);
}
