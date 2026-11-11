//! Same-host co-op INPUT ISOLATION (Windows): several split-pane sessions each play their own
//! instance of a game on ONE host, and each instance must see only its own player's input.
//!
//! Windows gives every process the same XInput pads, the single system keyboard/mouse and one
//! foreground window, so without help pane 1's player also drives pane 2's game. We use
//! ProtoInput (MIT, https://github.com/ilyaki/protoinput — the Pulsar fork adds an
//! `InjectRawInput` pipe message for synthetic network input) the way Nucleus Co-op does: its
//! hooks DLL is injected into each game process and makes that instance
//!   * see only its own XInput controller slot(s) (`SetControllerIndex`),
//!   * believe it always has focus (focus hooks + a focus message loop), and
//!   * read keyboard/mouse state only from what WE feed it (`InjectRawInput`), not the OS.
//!
//! This is an OPT-IN component (`Config::coop_isolation`): it injects code into game
//! processes, which anti-cheat may block or ban and antivirus may flag, so the ProtoInput
//! files are not part of the installer — they live in [`component_dir`] once the user
//! installs the component. Injection happens only when two or more sessions capture windows
//! of the SAME executable (true same-game co-op); a single pane never touches a game.
//!
//! Every entry point is best-effort and never panics into the input path: on any failure the
//! session keeps the existing per-window `PostMessage` / `SendInput` routing.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// Where the (optional) ProtoInput files live: `%LOCALAPPDATA%\dev.pulsar.app\coop\protoinput\`.
pub fn component_dir() -> Option<PathBuf> {
	std::env::var_os("LOCALAPPDATA")
		.map(PathBuf::from)
		.map(|d| d.join("dev.pulsar.app").join("coop").join("protoinput"))
}

/// The files the loader needs next to it (it picks the 32/64-bit pair per target process).
const REQUIRED: &[&str] = &[
	"ProtoInputLoader64.dll",
	"ProtoInputHooks64.dll",
	"ProtoInputHooks32.dll",
	"ProtoInputIJ64.exe",
	"ProtoInputIJ32.exe",
	"ProtoInputIJP64.dll",
	"ProtoInputIJP32.dll",
];

/// Whether the co-op component is installed (all files present).
pub fn component_installed() -> bool {
	component_dir().is_some_and(|d| REQUIRED.iter().all(|f| d.join(f).is_file()))
}

/// The component's missing files (for the Settings status line).
pub fn missing_files() -> Vec<&'static str> {
	match component_dir() {
		Some(d) => REQUIRED.iter().copied().filter(|f| !d.join(f).is_file()).collect(),
		None => REQUIRED.to_vec(),
	}
}

/// Which window each session currently captures (sid → hwnd), so a session can tell whether
/// another pane plays a second instance of the same game.
static CAPTURES: Mutex<Option<HashMap<u64, i64>>> = Mutex::new(None);

/// Record (or clear, `None`) the window session `sid` is capturing.
pub fn set_capture(sid: u64, hwnd: Option<i64>) {
	let mut g = CAPTURES.lock().unwrap();
	let m = g.get_or_insert_with(HashMap::new);
	match hwnd {
		Some(h) => {
			m.insert(sid, h);
		}
		None => {
			m.remove(&sid);
		}
	}
}

/// True when another session captures a window of a DIFFERENT process running the SAME
/// executable as `hwnd` — i.e. two instances of one game side by side.
pub fn has_same_game_peer(sid: u64, hwnd: i64) -> bool {
	let others: Vec<i64> = {
		let g = CAPTURES.lock().unwrap();
		g.as_ref()
			.map(|m| m.iter().filter(|(s, _)| **s != sid).map(|(_, h)| *h).collect())
			.unwrap_or_default()
	};
	let Some(pid) = imp::window_pid(hwnd) else { return false };
	let Some(exe) = imp::process_exe(pid) else { return false };
	others.into_iter().any(|h| {
		imp::window_pid(h).is_some_and(|p| p != pid && imp::process_exe(p).as_deref() == Some(exe.as_str()))
	})
}

/// One injected game instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Instance {
	handle: u32,
	pub pid: u32,
}

/// Injected instances by PID (a process is injected once; its hooks stay until it exits).
static INSTANCES: Mutex<Option<HashMap<u32, Instance>>> = Mutex::new(None);

/// Inject ProtoInput into the process owning `hwnd` (once per process) and set it up for
/// isolated input. BLOCKING (the injection waits for the hooks DLL's pipe) — call it off the
/// input thread. `player` is the 1-based pane number (ProtoInput's instance index).
pub fn attach(hwnd: i64, player: u32) -> Option<Instance> {
	let pid = imp::window_pid(hwnd)?;
	if let Some(i) = INSTANCES.lock().unwrap().as_ref().and_then(|m| m.get(&pid).copied()) {
		imp::api()?.update_hwnd(i.handle, hwnd);
		return Some(i);
	}
	let api = imp::api()?;
	let dir = component_dir()?;
	let handle = api.inject(pid, &dir)?;
	api.setup(handle, hwnd, player);
	let inst = Instance { handle, pid };
	INSTANCES.lock().unwrap().get_or_insert_with(HashMap::new).insert(pid, inst);
	tracing::info!(pid, hwnd, player, handle, "co-op: input isolation attached to the game instance");
	Some(inst)
}

/// Point the instance at its player's XInput slots (0-based XInput user indices; at most 4).
/// An empty list = this instance sees no controller.
pub fn set_pads(inst: Instance, xinput_slots: &[u32]) {
	let Some(api) = imp::api() else { return };
	// ProtoInput is 1-based with 0 = "no controller".
	let mut c = [0u32; 4];
	for (i, s) in xinput_slots.iter().take(4).enumerate() {
		c[i] = s + 1;
	}
	api.set_controllers(inst.handle, c);
	tracing::info!(pid = inst.pid, slots = ?xinput_slots, "co-op: controller slots assigned");
}

/// Relative mouse motion (pixels).
pub fn mouse_move(inst: Instance, dx: i32, dy: i32) {
	if dx == 0 && dy == 0 {
		return;
	}
	if let Some(api) = imp::api() {
		api.raw_mouse(inst.handle, 0, 0, dx, dy);
	}
}

/// Mouse button (0 = left, 1 = right, 2 = middle, 3/4 = X1/X2) down/up.
pub fn mouse_button(inst: Instance, button: u8, down: bool) {
	// RI_MOUSE_* flags: each button has a DOWN bit and the UP bit right above it.
	let down_bit: u16 = match button {
		0 => 0x0001,
		1 => 0x0004,
		2 => 0x0010,
		3 => 0x0040,
		4 => 0x0100,
		_ => return,
	};
	let flags = if down { down_bit } else { down_bit << 1 };
	if let Some(api) = imp::api() {
		api.raw_mouse(inst.handle, flags, 0, 0, 0);
	}
}

/// Vertical / horizontal wheel, in WHEEL_DELTA (120) units per notch.
pub fn mouse_wheel(inst: Instance, delta_y: i16, delta_x: i16) {
	let Some(api) = imp::api() else { return };
	if delta_y != 0 {
		api.raw_mouse(inst.handle, 0x0400 /* RI_MOUSE_WHEEL */, delta_y, 0, 0);
	}
	if delta_x != 0 {
		api.raw_mouse(inst.handle, 0x0800 /* RI_MOUSE_HWHEEL */, delta_x, 0, 0);
	}
}

/// A key (client evdev code) down/up.
pub fn key(inst: Instance, evdev: u32, down: bool) {
	let Some(api) = imp::api() else { return };
	let Some((vk, scan, ext)) = imp::raw_key_for(evdev) else { return };
	let mut flags: u16 = if down { 0 } else { 1 /* RI_KEY_BREAK */ };
	if ext {
		flags |= 2; // RI_KEY_E0
	}
	// Alt-modified / F10 keys are WM_SYSKEY* in a real message stream.
	let msg: u32 = if down { 0x0100 } else { 0x0101 }; // WM_KEYDOWN / WM_KEYUP
	api.raw_key(inst.handle, scan, flags, vk, msg);
}

/// PIDs launched per host game id (to tell whether a copy is already running).
static LAUNCHED: Mutex<Option<HashMap<String, Vec<u32>>>> = Mutex::new(None);

/// Remember that game `id` was launched as `pid`.
pub fn note_launch(id: &str, pid: u32) {
	LAUNCHED.lock().unwrap().get_or_insert_with(HashMap::new).entry(id.to_string()).or_default().push(pid);
}

/// How many launched copies of game `id` are still running (dead PIDs are pruned).
pub fn running_copies(id: &str) -> usize {
	let mut g = LAUNCHED.lock().unwrap();
	let Some(v) = g.as_mut().and_then(|m| m.get_mut(id)) else { return 0 };
	v.retain(|p| imp::process_alive(*p));
	v.len()
}

/// Launch ANOTHER copy of a program game next to a running one: created suspended with
/// ProtoInput injected (handle renaming for `coop_mutexes`, the input-isolation setup) and,
/// when `coop_profile` is set, its own user-profile/AppData folders, then resumed. Returns the
/// new PID, or `None` (the caller then launches it the normal way).
pub fn launch_isolated(g: &crate::process::HostGame) -> Option<u32> {
	if g.kind != "program" || g.path.is_empty() {
		return None;
	}
	let api = imp::api()?;
	let dir = component_dir()?;
	let copy = running_copies(&g.id) as u32 + 1;
	let env = if g.coop_profile { Some(profile_env(&g.id, copy)?) } else { None };
	let (handle, pid) = api.inject_startup(&g.path, &g.args, &dir, env.as_deref())?;
	for name in g.coop_mutexes.split(',').map(str::trim).filter(|n| !n.is_empty()) {
		api.rename_handle(handle, name);
	}
	api.setup(handle, 0, copy);
	api.wake(handle);
	INSTANCES.lock().unwrap().get_or_insert_with(HashMap::new).insert(pid, Instance { handle, pid });
	tracing::info!(game = %g.id, pid, copy, profile = g.coop_profile, "co-op: launched an isolated copy");
	Some(pid)
}

/// The environment for copy `copy` of game `id`: USERPROFILE / APPDATA / LOCALAPPDATA pointed at
/// `%LOCALAPPDATA%\dev.pulsar.app\coop\profiles\<id>\p<copy>\` (created), so saves and
/// settings don't collide. Only games that read these variables honour it.
fn profile_env(id: &str, copy: u32) -> Option<Vec<(String, String)>> {
	let safe: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
	let root = component_dir()?.parent()?.join("profiles").join(safe).join(format!("p{copy}"));
	let roaming = root.join("AppData").join("Roaming");
	let local = root.join("AppData").join("Local");
	for d in [&roaming, &local, &root.join("Documents")] {
		std::fs::create_dir_all(d).ok()?;
	}
	let mut vars: Vec<(String, String)> = std::env::vars()
		.filter(|(k, _)| !matches!(k.to_ascii_uppercase().as_str(), "USERPROFILE" | "APPDATA" | "LOCALAPPDATA"))
		.collect();
	vars.push(("USERPROFILE".into(), root.display().to_string()));
	vars.push(("APPDATA".into(), roaming.display().to_string()));
	vars.push(("LOCALAPPDATA".into(), local.display().to_string()));
	vars.sort_by_key(|(k, _)| k.to_ascii_uppercase());
	Some(vars)
}

/// Per-session co-op routing state, owned by the session's input closure (host.rs).
pub struct SessionRouter {
	sid: u64,
	/// Filled by the (blocking) attach thread: the capture window + its injected instance.
	inst: std::sync::Arc<Mutex<Option<(i64, Instance)>>>,
	/// The window an attach is running / ran for (no second attempt for the same window).
	attempted: Option<i64>,
	/// Last same-game-peer check (cheap rate limit: it opens processes).
	checked_at: Option<std::time::Instant>,
	/// The controller slots last sent, per instance PID.
	pads_sent: Option<(u32, Vec<u32>)>,
	/// Sub-pixel remainder of relative mouse motion.
	frac: (f64, f64),
	/// Last absolute pointer position (client pixels) — absolute moves become deltas.
	last_abs: Option<(i32, i32)>,
	/// Sub-notch wheel remainder (client sends browser-wheel pixels; 100 px = 1 notch).
	wheel: (f64, f64),
}

impl SessionRouter {
	pub fn new(sid: u64) -> Self {
		Self {
			sid,
			inst: std::sync::Arc::new(Mutex::new(None)),
			attempted: None,
			checked_at: None,
			pads_sent: None,
			frac: (0.0, 0.0),
			last_abs: None,
			wheel: (0.0, 0.0),
		}
	}

	/// The isolated instance to route this session's input to, if any. `target` = the window
	/// this session's stream is CONFIRMED to capture; `enabled` = the user's opt-in. Starts the
	/// injection (off-thread, once per window) when another pane plays the same game.
	pub fn poll(&mut self, target: Option<i64>, enabled: bool) -> Option<Instance> {
		let hwnd = target?;
		if let Some((h, i)) = *self.inst.lock().unwrap() {
			if h == hwnd {
				return Some(i);
			}
		}
		if !enabled || self.attempted == Some(hwnd) || !component_installed() {
			return None;
		}
		let now = std::time::Instant::now();
		if self.checked_at.is_some_and(|t| now.duration_since(t) < std::time::Duration::from_secs(2)) {
			return None;
		}
		self.checked_at = Some(now);
		if !has_same_game_peer(self.sid, hwnd) {
			return None;
		}
		self.attempted = Some(hwnd);
		let slot = self.inst.clone();
		let player = INSTANCES.lock().unwrap().as_ref().map_or(0, |m| m.len()) as u32 + 1;
		std::thread::spawn(move || {
			if let Some(i) = attach(hwnd, player) {
				*slot.lock().unwrap() = Some((hwnd, i));
			}
		});
		None
	}

	/// Tell the instance which XInput slots are this session's (only when they changed).
	pub fn sync_pads(&mut self, inst: Instance, mut slots: Vec<u32>) {
		slots.sort_unstable();
		if self.pads_sent.as_ref().is_some_and(|(p, s)| *p == inst.pid && *s == slots) {
			return;
		}
		set_pads(inst, &slots);
		self.pads_sent = Some((inst.pid, slots));
	}

	/// Deliver one keyboard/mouse event to the isolated instance. `false` = not a kb/mouse
	/// event this path handles (the caller routes it as before).
	pub fn route(&mut self, inst: Instance, hwnd: i64, ev: &pulsar_core::service::InputEvent) -> bool {
		use pulsar_core::service::InputEvent as E;
		match *ev {
			E::Key { code, down } => key(inst, code, down),
			E::PointerRelative { dx, dy } => {
				let (fx, fy) = (self.frac.0 + dx, self.frac.1 + dy);
				let (ix, iy) = (fx.trunc() as i32, fy.trunc() as i32);
				self.frac = (fx - ix as f64, fy - iy as f64);
				mouse_move(inst, ix, iy);
			}
			E::PointerMotion { x, y } => {
				let Some((w, h)) = imp::client_size(hwnd) else { return true };
				let (px, py) = ((x.clamp(0.0, 1.0) * w as f64) as i32, (y.clamp(0.0, 1.0) * h as f64) as i32);
				if let Some((lx, ly)) = self.last_abs {
					mouse_move(inst, px - lx, py - ly);
				}
				self.last_abs = Some((px, py));
			}
			E::PointerButton { button, down } => mouse_button(inst, button, down),
			E::Scroll { dx, dy } => {
				self.wheel.0 += dx / 100.0;
				self.wheel.1 += -dy / 100.0;
				let (h, v) = (self.wheel.0.trunc(), self.wheel.1.trunc());
				self.wheel = (self.wheel.0 - h, self.wheel.1 - v);
				mouse_wheel(inst, (v as i32 * 120).clamp(-32768, 32767) as i16, (h as i32 * 120).clamp(-32768, 32767) as i16);
			}
			_ => return false,
		}
		true
	}
}

#[cfg(windows)]
mod imp {
	use std::path::Path;
	use std::sync::OnceLock;

	type FnInject = unsafe extern "C" fn(u32, *const u16) -> u32;
	type FnHook = unsafe extern "C" fn(u32, u32);
	type FnState = unsafe extern "C" fn(u32, i32);
	type FnMsgs = unsafe extern "C" fn(u32, bool, bool, bool, bool, bool);
	type FnFocus = unsafe extern "C" fn(u32, i32, bool, bool, bool, bool, bool);
	type FnBool = unsafe extern "C" fn(u32, bool);
	type FnHwnd = unsafe extern "C" fn(u32, u64);
	type FnCtrl = unsafe extern "C" fn(u32, u32, u32, u32, u32);
	type FnRaw = unsafe extern "C" fn(u32, u32, u16, u16, i16, i32, i32, u16, u16, u16, u32);
	type FnStartup =
		unsafe extern "C" fn(*const u16, *const u16, u32, *const u16, *mut u32, *mut std::ffi::c_void) -> u32;
	type FnName = unsafe extern "C" fn(u32, *const u16);
	type FnWake = unsafe extern "C" fn(u32);

	pub struct Api {
		_lib: libloading::Library,
		easyhook_inject: FnInject,
		remote_inject: FnInject,
		install_hook: FnHook,
		enable_filter: FnHook,
		setup_state: FnState,
		setup_messages: FnMsgs,
		start_focus: FnFocus,
		draw_fake_cursor: FnBool,
		update_main_hwnd: FnHwnd,
		set_controller_index: FnCtrl,
		inject_raw: FnRaw,
		inject_startup: FnStartup,
		add_handle_to_rename: FnName,
		wake_up: FnWake,
	}

	// The loader's exported functions are plain C calls guarded by its own instance map; we
	// only ever call them from our threads, one call at a time per instance.
	unsafe impl Send for Api {}
	unsafe impl Sync for Api {}

	static API: OnceLock<Option<Api>> = OnceLock::new();

	/// The loaded ProtoInput loader (None when the component isn't installed / fails to load).
	pub fn api() -> Option<&'static Api> {
		API.get_or_init(|| {
			if !super::component_installed() {
				return None;
			}
			let dir = super::component_dir()?;
			match unsafe { load(&dir) } {
				Ok(a) => Some(a),
				Err(e) => {
					tracing::warn!(error = %e, "co-op: loading ProtoInputLoader64.dll failed");
					None
				}
			}
		})
		.as_ref()
	}

	unsafe fn load(dir: &Path) -> Result<Api, libloading::Error> {
		let lib = libloading::Library::new(dir.join("ProtoInputLoader64.dll"))?;
		macro_rules! sym {
			($name:literal, $t:ty) => {
				*lib.get::<$t>(concat!($name, "\0").as_bytes())?
			};
		}
		Ok(Api {
			easyhook_inject: sym!("EasyHookInjectRuntime", FnInject),
			remote_inject: sym!("RemoteLoadLibraryInjectRuntime", FnInject),
			install_hook: sym!("InstallHook", FnHook),
			enable_filter: sym!("EnableMessageFilter", FnHook),
			setup_state: sym!("SetupState", FnState),
			setup_messages: sym!("SetupMessagesToSend", FnMsgs),
			start_focus: sym!("StartFocusMessageLoop", FnFocus),
			draw_fake_cursor: sym!("SetDrawFakeCursor", FnBool),
			update_main_hwnd: sym!("UpdateMainWindowHandle", FnHwnd),
			set_controller_index: sym!("SetControllerIndex", FnCtrl),
			inject_raw: sym!("InjectRawInput", FnRaw),
			inject_startup: sym!("EasyHookInjectStartup", FnStartup),
			add_handle_to_rename: sym!("AddHandleToRename", FnName),
			wake_up: sym!("WakeUpProcess", FnWake),
			_lib: lib,
		})
	}

	// ProtoHookIDs (protoloader.h).
	const HOOKS: &[u32] = &[
		0,  // RegisterRawInput
		1,  // GetRawInputData
		2,  // MessageFilter
		3,  // GetCursorPos
		4,  // SetCursorPos
		5,  // GetKeyState
		6,  // GetAsyncKeyState
		7,  // GetKeyboardState
		8,  // CursorVisibilityState
		9,  // ClipCursor
		10, // FocusHooks
		12, // Xinput
	];
	// ProtoMessageFilterIDs 0..=7: raw input, mouse move/activate, window activate(app),
	// wheel, buttons, keyboard — drop the REAL ones; only what we inject reaches the game.
	const FILTERS: std::ops::RangeInclusive<u32> = 0..=7;

	impl Api {
		/// Inject the hooks DLL into `pid` (EasyHook first — what Nucleus uses — then a plain
		/// LoadLibrary injection). The loader needs the folder WITH a trailing backslash.
		pub fn inject(&self, pid: u32, dir: &Path) -> Option<u32> {
			let mut folder: Vec<u16> = dir.as_os_str().encode_wide_vec();
			if folder.last() != Some(&(b'\\' as u16)) {
				folder.push(b'\\' as u16);
			}
			folder.push(0);
			let h = unsafe { (self.easyhook_inject)(pid, folder.as_ptr()) };
			if h != 0 {
				return Some(h);
			}
			tracing::warn!(pid, "co-op: EasyHook injection failed, trying LoadLibrary injection");
			let h = unsafe { (self.remote_inject)(pid, folder.as_ptr()) };
			if h == 0 {
				tracing::warn!(pid, "co-op: injection failed (anti-cheat, elevation or bitness?)");
				return None;
			}
			Some(h)
		}

		pub fn setup(&self, h: u32, hwnd: i64, player: u32) {
			unsafe {
				for id in HOOKS {
					(self.install_hook)(h, *id);
				}
				for id in FILTERS {
					(self.enable_filter)(h, id);
				}
				(self.setup_state)(h, player as i32);
				// Post the injected input to the window as regular messages too (wheel,
				// buttons, moves, keys; no synthetic double-clicks).
				(self.setup_messages)(h, true, true, true, true, false);
				// Keep telling the instance it is active, every 5 ms.
				(self.start_focus)(h, 5, true, true, true, true, true);
				// The real cursor stays with the host; each instance draws its own fake one.
				(self.draw_fake_cursor)(h, true);
				(self.update_main_hwnd)(h, hwnd as u64);
			}
		}

		pub fn update_hwnd(&self, h: u32, hwnd: i64) {
			unsafe { (self.update_main_hwnd)(h, hwnd as u64) }
		}

		/// Create `exe` suspended with the hooks injected (call `wake` after setup). `env` =
		/// an optional full environment for the new process.
		pub fn inject_startup(
			&self,
			exe: &str,
			args: &str,
			dir: &Path,
			env: Option<&[(String, String)]>,
		) -> Option<(u32, u32)> {
			let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
			let exe_w = wide(exe);
			// The command line is the FULL one (program name first), as CreateProcess wants it.
			let cmd_w = wide(&format!("\"{exe}\" {args}"));
			let mut folder: Vec<u16> = dir.as_os_str().encode_wide_vec();
			if folder.last() != Some(&(b'\\' as u16)) {
				folder.push(b'\\' as u16);
			}
			folder.push(0);
			// CREATE_UNICODE_ENVIRONMENT when we pass a block: "K=V\0...\0\0" in UTF-16.
			let mut block: Vec<u16> = Vec::new();
			let mut flags = 0u32;
			if let Some(vars) = env {
				for (k, v) in vars {
					block.extend(format!("{k}={v}").encode_utf16());
					block.push(0);
				}
				block.push(0);
				flags |= 0x0000_0400;
			}
			let mut pid = 0u32;
			let env_ptr = if block.is_empty() { std::ptr::null_mut() } else { block.as_mut_ptr().cast() };
			let h = unsafe {
				(self.inject_startup)(exe_w.as_ptr(), cmd_w.as_ptr(), flags, folder.as_ptr(), &mut pid, env_ptr)
			};
			if h == 0 || pid == 0 {
				tracing::warn!(exe, "co-op: starting the isolated copy failed");
				return None;
			}
			Some((h, pid))
		}

		pub fn rename_handle(&self, h: u32, name: &str) {
			let w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
			unsafe {
				(self.install_hook)(h, 11); // RenameHandlesHookID (idempotent)
				(self.add_handle_to_rename)(h, w.as_ptr());
			}
		}

		pub fn wake(&self, h: u32) {
			unsafe { (self.wake_up)(h) }
		}

		pub fn set_controllers(&self, h: u32, c: [u32; 4]) {
			unsafe { (self.set_controller_index)(h, c[0], c[1], c[2], c[3]) }
		}

		pub fn raw_mouse(&self, h: u32, button_flags: u16, button_data: i16, dx: i32, dy: i32) {
			// type 0 = mouse; usFlags 0 = MOUSE_MOVE_RELATIVE.
			unsafe { (self.inject_raw)(h, 0, 0, button_flags, button_data, dx, dy, 0, 0, 0, 0) }
		}

		pub fn raw_key(&self, h: u32, scan: u16, flags: u16, vk: u16, msg: u32) {
			unsafe { (self.inject_raw)(h, 1, 0, 0, 0, 0, 0, scan, flags, vk, msg) }
		}
	}

	trait EncodeWideVec {
		fn encode_wide_vec(&self) -> Vec<u16>;
	}
	impl EncodeWideVec for std::ffi::OsStr {
		fn encode_wide_vec(&self) -> Vec<u16> {
			use std::os::windows::ffi::OsStrExt;
			self.encode_wide().collect()
		}
	}

	pub fn window_pid(hwnd: i64) -> Option<u32> {
		pulsar_capture::window_pid(hwnd as isize)
	}

	pub fn raw_key_for(evdev: u32) -> Option<(u16, u16, bool)> {
		pulsar_core::input::evdev_to_raw_key(evdev)
	}

	/// Whether `pid` is still running.
	pub fn process_alive(pid: u32) -> bool {
		use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
		use windows_sys::Win32::System::Threading::{
			GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
		};
		unsafe {
			let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
			if h.is_null() {
				return false;
			}
			let mut code = 0u32;
			let ok = GetExitCodeProcess(h, &mut code);
			CloseHandle(h);
			ok != 0 && code == STILL_ACTIVE as u32
		}
	}

	/// The window's client-area size in pixels.
	pub fn client_size(hwnd: i64) -> Option<(i32, i32)> {
		use windows_sys::Win32::Foundation::RECT;
		use windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect;
		let mut r: RECT = unsafe { std::mem::zeroed() };
		if unsafe { GetClientRect(hwnd as isize as _, &mut r) } == 0 {
			return None;
		}
		Some(((r.right - r.left).max(1), (r.bottom - r.top).max(1)))
	}

	/// Full image path of `pid` (lower-cased), to tell two instances of one game apart from
	/// two different apps.
	pub fn process_exe(pid: u32) -> Option<String> {
		use windows_sys::Win32::Foundation::CloseHandle;
		use windows_sys::Win32::System::Threading::{
			OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
		};
		unsafe {
			let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
			if h.is_null() {
				return None;
			}
			let mut buf = vec![0u16; 1024];
			let mut len = buf.len() as u32;
			let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len);
			CloseHandle(h);
			if ok == 0 {
				return None;
			}
			Some(String::from_utf16_lossy(&buf[..len as usize]).to_lowercase())
		}
	}
}

#[cfg(not(windows))]
mod imp {
	pub struct Api;
	impl Api {
		pub fn inject(&self, _pid: u32, _dir: &std::path::Path) -> Option<u32> {
			None
		}
		pub fn setup(&self, _h: u32, _hwnd: i64, _player: u32) {}
		pub fn update_hwnd(&self, _h: u32, _hwnd: i64) {}
		pub fn set_controllers(&self, _h: u32, _c: [u32; 4]) {}
		pub fn raw_mouse(&self, _h: u32, _f: u16, _d: i16, _dx: i32, _dy: i32) {}
		pub fn raw_key(&self, _h: u32, _s: u16, _f: u16, _vk: u16, _m: u32) {}
		pub fn inject_startup(
			&self,
			_exe: &str,
			_args: &str,
			_dir: &std::path::Path,
			_env: Option<&[(String, String)]>,
		) -> Option<(u32, u32)> {
			None
		}
		pub fn rename_handle(&self, _h: u32, _name: &str) {}
		pub fn wake(&self, _h: u32) {}
	}
	pub fn api() -> Option<&'static Api> {
		None
	}
	pub fn window_pid(_hwnd: i64) -> Option<u32> {
		None
	}
	pub fn process_exe(_pid: u32) -> Option<String> {
		None
	}
	pub fn raw_key_for(_evdev: u32) -> Option<(u16, u16, bool)> {
		None
	}
	pub fn client_size(_hwnd: i64) -> Option<(i32, i32)> {
		None
	}
	pub fn process_alive(_pid: u32) -> bool {
		false
	}
}
