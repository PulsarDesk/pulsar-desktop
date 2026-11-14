//! Linux screen adaptation: switch the captured monitor's mode for a split pane, restore it.
//!
//! * **X11** — `xrandr`: `--current` is parsed (`pulsar_capture::display_mode::xrandr`), the
//!   best fit is applied with `xrandr --output <o> --mode <m> --rate <r>` and the original mode
//!   + position come back with the same call on revert. `display_idx` is the index into
//!   [`crate::process::linux_displays`] — the list `host_displays` advertises and the capture
//!   uses — so it names the exact output the client picked.
//! * **Wayland** — there is no standard protocol for a client to change output modes; it is up
//!   to each compositor:
//!   - wlroots compositors (Sway, Hyprland, labwc, river, Wayfire…) expose
//!     `wlr-output-management-unstable-v1`, driven here through the `wlr-randr` tool;
//!   - KDE Plasma (KWin) exposes `kde-output-management`, driven through `kscreen-doctor`;
//!   - GNOME (Mutter) only offers its private D-Bus `org.gnome.Mutter.DisplayConfig`
//!     (`ApplyMonitorsConfig` with the whole layout) — not wired here (see status.md), so
//!     GNOME Wayland hosts keep their mode.
//!
//!   On Wayland the XDG ScreenCast portal decides which monitor is captured and does not say
//!   which output it is, so the switch is only made when exactly ONE output is enabled (the
//!   shared one is then certain). With several outputs nothing is changed.
//!
//! Crash safety: the original mode of every switched output is written to a small journal
//! (`$XDG_RUNTIME_DIR/pulsar/display-mode.json`) before the switch and removed after the
//! revert. If Pulsar dies with a mode applied, the next start ([`recover_stale`]) restores
//! what the journal holds (entries of a still-running Pulsar process are left alone).
//!
//! Untested on Linux hardware (X11, wlroots, KDE): written and type-checked on WSL only.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

use pulsar_capture::display_mode::{self as fit, xrandr, Cand};
use serde::{Deserialize, Serialize};

/// The monitor's mode before we changed it — what [`revert`] re-applies.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PrevMode {
	/// `xrandr --output <output> --mode <mode> --rate <rate> --pos <x>x<y>`.
	X11 { output: String, mode: String, rate: String, x: i32, y: i32 },
	/// `wlr-randr --output <output> --mode <mode> --pos <x>,<y>` (`mode` = `WxH@RHz`).
	Wlr { output: String, mode: String, x: i32, y: i32 },
	/// `kscreen-doctor output.<output>.mode.<mode_id>`.
	Kde { output: String, mode_id: String },
}

impl PrevMode {
	fn output(&self) -> &str {
		match self {
			PrevMode::X11 { output, .. } | PrevMode::Wlr { output, .. } | PrevMode::Kde { output, .. } => output,
		}
	}
}

fn run(prog: &str, args: &[String]) -> Result<String, String> {
	let out = Command::new(prog).args(args).output().map_err(|e| format!("{prog}: {e}"))?;
	if out.status.success() {
		Ok(String::from_utf8_lossy(&out.stdout).into_owned())
	} else {
		Err(format!("{prog} {}: {}", out.status, String::from_utf8_lossy(&out.stderr).trim()))
	}
}

fn apply(prev: &PrevMode) -> Result<(), String> {
	match prev {
		PrevMode::X11 { output, mode, rate, x, y } => run(
			"xrandr",
			&["--output".into(), output.clone(), "--mode".into(), mode.clone(), "--rate".into(), rate.clone(), "--pos".into(), format!("{x}x{y}")],
		)
		.map(drop),
		PrevMode::Wlr { output, mode, x, y } => run(
			"wlr-randr",
			&["--output".into(), output.clone(), "--mode".into(), mode.clone(), "--pos".into(), format!("{x},{y}")],
		)
		.map(drop),
		PrevMode::Kde { output, mode_id } => run("kscreen-doctor", &[format!("output.{output}.mode.{mode_id}")]).map(drop),
	}
}

/// Switch the monitor at `display_idx` to the mode that best fits a `target_w × target_h`
/// pane. `Some` only when a change was applied (the caller reverts it on teardown).
pub fn set_best_mode(display_idx: u32, target_w: u32, target_h: u32) -> Option<PrevMode> {
	if target_w == 0 || target_h == 0 {
		return None;
	}
	let res = if pulsar_core::capture::is_wayland() {
		wayland_switch(display_idx, target_w, target_h)
	} else {
		x11_switch(display_idx, target_w, target_h)
	};
	match res {
		Ok(Some((prev, to))) => {
			tracing::info!(display_idx, target_w, target_h, output = prev.output(), to = %to, "screen-adapt: switched host display mode for split pane");
			Some(prev)
		}
		Ok(None) => None,
		Err(e) => {
			tracing::warn!(display_idx, target_w, target_h, "screen-adapt: {e}; leaving mode unchanged");
			None
		}
	}
}

/// Restore the mode recorded in `prev` (best-effort, logs on failure).
pub fn revert(prev: PrevMode) {
	match apply(&prev) {
		Ok(()) => {
			journal_remove(prev.output());
			tracing::info!(output = prev.output(), "screen-adapt: reverted host display mode");
		}
		Err(e) => tracing::warn!(output = prev.output(), "screen-adapt: revert failed (host left at adapted mode): {e}"),
	}
}

/// Apply `next` after journaling `prev`; on failure the journal entry is dropped again.
fn switch(prev: PrevMode, next: &PrevMode) -> Result<PrevMode, String> {
	journal_add(&prev);
	match apply(next) {
		Ok(()) => Ok(prev),
		Err(e) => {
			journal_remove(prev.output());
			Err(e)
		}
	}
}

fn x11_switch(display_idx: u32, target_w: u32, target_h: u32) -> Result<Option<(PrevMode, String)>, String> {
	let displays = crate::process::linux_displays();
	let Some((name, x, y, ..)) = displays.into_iter().nth(display_idx as usize) else {
		return Err(format!("no X11 output at index {display_idx}"));
	};
	let text = run("xrandr", &["--current".into()])?;
	let outs = xrandr::parse(&text);
	let out = outs.iter().find(|o| o.name == name).ok_or_else(|| format!("output {name} missing from xrandr"))?;
	let (cm, cr) = xrandr::current(out).ok_or_else(|| format!("output {name} has no current mode"))?;
	let Some(p) = xrandr::plan(out, target_w, target_h) else {
		return Ok(None);
	};
	let prev = PrevMode::X11 {
		output: name.clone(),
		mode: out.modes[cm].name.clone(),
		rate: out.modes[cm].rates[cr].text.clone(),
		x,
		y,
	};
	let next = PrevMode::X11 { output: name, mode: p.mode.clone(), rate: p.rate.clone(), x, y };
	Ok(Some((switch(prev, &next)?, format!("{} @ {} Hz", p.mode, p.rate))))
}

// ---------------------------------------------------------------------------
// Wayland (compositor-specific tools)
// ---------------------------------------------------------------------------

/// One output as a Wayland tool reports it, reduced to what the fit needs.
#[derive(Debug, PartialEq)]
pub(super) struct WlOutput {
	pub name: String,
	pub enabled: bool,
	pub pos: (i32, i32),
	/// `(cand, tool's mode handle)`: `WxH@RHz` for wlr-randr, the mode id for kscreen-doctor.
	pub modes: Vec<(Cand, String)>,
	pub current: Option<usize>,
}

fn quarter_turn_wlr(transform: &str) -> bool {
	transform.contains("90") || transform.contains("270")
}

/// `wlr-randr --json` → outputs.
pub(super) fn parse_wlr(json: &str) -> Vec<WlOutput> {
	let Ok(serde_json::Value::Array(outs)) = serde_json::from_str::<serde_json::Value>(json) else {
		return Vec::new();
	};
	outs.iter()
		.filter_map(|o| {
			let name = o.get("name")?.as_str()?.to_string();
			let enabled = o.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
			let rot = quarter_turn_wlr(o.get("transform").and_then(|v| v.as_str()).unwrap_or("normal"));
			let pos = o.get("position").map_or((0, 0), |p| {
				(
					p.get("x").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
					p.get("y").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
				)
			});
			let mut modes = Vec::new();
			let mut current = None;
			for m in o.get("modes").and_then(|v| v.as_array()).into_iter().flatten() {
				let (Some(w), Some(h)) = (m.get("width").and_then(|v| v.as_u64()), m.get("height").and_then(|v| v.as_u64())) else {
					continue;
				};
				let hz = m.get("refresh").and_then(|v| v.as_f64()).unwrap_or(0.0);
				if m.get("current").and_then(|v| v.as_bool()).unwrap_or(false) {
					current = Some(modes.len());
				}
				let (cw, ch) = if rot { (h as u32, w as u32) } else { (w as u32, h as u32) };
				modes.push((Cand { w: cw, h: ch, hz, scale: 1 }, format!("{w}x{h}@{hz:.3}Hz")));
			}
			Some(WlOutput { name, enabled, pos, modes, current })
		})
		.collect()
}

/// `kscreen-doctor -j` → outputs (`rotation`: 1 none, 2 left, 4 inverted, 8 right).
pub(super) fn parse_kde(json: &str) -> Vec<WlOutput> {
	let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
		return Vec::new();
	};
	let Some(outs) = v.get("outputs").and_then(|o| o.as_array()) else {
		return Vec::new();
	};
	outs.iter()
		.filter_map(|o| {
			let name = o.get("name")?.as_str()?.to_string();
			let connected = o.get("connected").and_then(|v| v.as_bool()).unwrap_or(true);
			let enabled = connected && o.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
			let rot = matches!(o.get("rotation").and_then(|v| v.as_u64()), Some(2) | Some(8));
			let pos = o.get("pos").map_or((0, 0), |p| {
				(
					p.get("x").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
					p.get("y").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
				)
			});
			let id_str = |v: &serde_json::Value| match v {
				serde_json::Value::String(s) => Some(s.clone()),
				serde_json::Value::Number(n) => Some(n.to_string()),
				_ => None,
			};
			let cur_id = o.get("currentModeId").and_then(id_str);
			let mut modes = Vec::new();
			let mut current = None;
			for m in o.get("modes").and_then(|v| v.as_array()).into_iter().flatten() {
				let Some(id) = m.get("id").and_then(id_str) else { continue };
				let size = m.get("size");
				let (Some(w), Some(h)) = (
					size.and_then(|s| s.get("width")).and_then(|v| v.as_u64()),
					size.and_then(|s| s.get("height")).and_then(|v| v.as_u64()),
				) else {
					continue;
				};
				let hz = m.get("refreshRate").and_then(|v| v.as_f64()).unwrap_or(0.0);
				if cur_id.as_deref() == Some(id.as_str()) {
					current = Some(modes.len());
				}
				let (cw, ch) = if rot { (h as u32, w as u32) } else { (w as u32, h as u32) };
				modes.push((Cand { w: cw, h: ch, hz, scale: 1 }, id));
			}
			Some(WlOutput { name, enabled, pos, modes, current })
		})
		.collect()
}

/// The one enabled output and the mode to switch it to: `(output, current handle, new handle,
/// new size)`. `Err` when the output can't be determined (several enabled, none current).
pub(super) fn wl_plan(
	outs: &[WlOutput],
	target_w: u32,
	target_h: u32,
) -> Result<Option<(&WlOutput, String, String, (u32, u32))>, String> {
	let enabled: Vec<&WlOutput> = outs.iter().filter(|o| o.enabled).collect();
	let out = match enabled.as_slice() {
		[one] => *one,
		[] => return Err("no enabled Wayland output".into()),
		_ => {
			return Err(format!(
				"{} Wayland outputs enabled and the ScreenCast portal does not say which one is shared",
				enabled.len()
			))
		}
	};
	let cur = out.current.ok_or_else(|| format!("output {} has no current mode", out.name))?;
	let cands: Vec<Cand> = out.modes.iter().map(|m| m.0).collect();
	Ok(fit::plan(&cands, cur, target_w, target_h)
		.map(|i| (out, out.modes[cur].1.clone(), out.modes[i].1.clone(), (cands[i].w, cands[i].h))))
}

fn has_tool(name: &str) -> bool {
	std::env::var_os("PATH")
		.map(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
		.unwrap_or(false)
}

fn wayland_switch(display_idx: u32, target_w: u32, target_h: u32) -> Result<Option<(PrevMode, String)>, String> {
	// host_displays() advertises no list on Wayland, so the client always asks for idx 0.
	if display_idx != 0 {
		return Err(format!("Wayland: no monitor at index {display_idx}"));
	}
	let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_ascii_lowercase();
	if desktop.contains("kde") && has_tool("kscreen-doctor") {
		let outs = parse_kde(&run("kscreen-doctor", &["-j".into()])?);
		let Some((out, cur, new, (w, h))) = wl_plan(&outs, target_w, target_h)? else {
			return Ok(None);
		};
		let prev = PrevMode::Kde { output: out.name.clone(), mode_id: cur };
		let next = PrevMode::Kde { output: out.name.clone(), mode_id: new };
		return Ok(Some((switch(prev, &next)?, format!("{w}x{h} (KDE)"))));
	}
	if desktop.contains("gnome") {
		return Err("GNOME Wayland: mode switching (Mutter DisplayConfig D-Bus) is not supported".into());
	}
	if has_tool("wlr-randr") {
		// Non-wlroots compositors refuse here ("compositor doesn't support
		// wlr-output-management-unstable-v1") → an Err, mode unchanged.
		let outs = parse_wlr(&run("wlr-randr", &["--json".into()])?);
		let Some((out, cur, new, (w, h))) = wl_plan(&outs, target_w, target_h)? else {
			return Ok(None);
		};
		let (x, y) = out.pos;
		let prev = PrevMode::Wlr { output: out.name.clone(), mode: cur, x, y };
		let next = PrevMode::Wlr { output: out.name.clone(), mode: new, x, y };
		return Ok(Some((switch(prev, &next)?, format!("{w}x{h} (wlroots)"))));
	}
	Err(format!(
		"Wayland ({desktop}): no output-management tool (wlr-randr / kscreen-doctor) for this compositor"
	))
}

// ---------------------------------------------------------------------------
// Crash journal
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct Entry {
	pid: u32,
	prev: PrevMode,
}

static JOURNAL: Mutex<()> = Mutex::new(());

fn journal_path() -> PathBuf {
	let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
	base.join("pulsar").join("display-mode.json")
}

fn journal_read() -> Vec<Entry> {
	std::fs::read(journal_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn journal_write(entries: &[Entry]) {
	let path = journal_path();
	if entries.is_empty() {
		let _ = std::fs::remove_file(&path);
		return;
	}
	if let Some(dir) = path.parent() {
		let _ = std::fs::create_dir_all(dir);
	}
	if let Ok(b) = serde_json::to_vec(entries) {
		let _ = std::fs::write(&path, b);
	}
}

/// Remember `prev` (the FIRST original of an output wins: a second switch of an already
/// switched output must still restore the real original).
fn journal_add(prev: &PrevMode) {
	let _g = JOURNAL.lock().unwrap();
	let mut e = journal_read();
	if !e.iter().any(|x| x.prev.output() == prev.output()) {
		e.push(Entry { pid: std::process::id(), prev: prev.clone() });
		journal_write(&e);
	}
}

fn journal_remove(output: &str) {
	let _g = JOURNAL.lock().unwrap();
	let mut e = journal_read();
	let n = e.len();
	e.retain(|x| x.prev.output() != output);
	if e.len() != n {
		journal_write(&e);
	}
}

fn pid_alive(pid: u32) -> bool {
	// kill(pid, 0): 0 = exists; EPERM = exists but not ours.
	unsafe { libc::kill(pid as libc::pid_t, 0) == 0 || *libc::__errno_location() == libc::EPERM }
}

/// At startup: restore the modes a crashed Pulsar left switched (see the module docs).
pub fn recover_stale() {
	let entries = {
		let _g = JOURNAL.lock().unwrap();
		journal_read()
	};
	let me = std::process::id();
	for e in entries {
		if e.pid != me && pid_alive(e.pid) {
			continue; // another Pulsar still holds it and reverts it itself
		}
		tracing::info!(output = e.prev.output(), "screen-adapt: restoring a display mode left switched by a previous run");
		revert(e.prev);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn wlr_json_single_output() {
		let j = r#"[{"name":"DP-1","enabled":true,"transform":"normal","position":{"x":0,"y":0},
			"modes":[{"width":2560,"height":1440,"refresh":143.998,"preferred":true,"current":true},
			{"width":1920,"height":1200,"refresh":59.95,"preferred":false,"current":false},
			{"width":1920,"height":1080,"refresh":60.0,"preferred":false,"current":false}]},
			{"name":"HDMI-A-1","enabled":false,"modes":[]}]"#;
		let o = parse_wlr(j);
		assert_eq!(o.len(), 2);
		assert_eq!(o[0].current, Some(0));
		let (out, cur, new, size) = wl_plan(&o, 1280, 800).unwrap().unwrap();
		assert_eq!(out.name, "DP-1");
		assert_eq!(cur, "2560x1440@143.998Hz");
		assert_eq!(new, "1920x1200@59.950Hz");
		assert_eq!(size, (1920, 1200));
	}

	#[test]
	fn kde_json_and_rotation() {
		let j = r#"{"outputs":[{"name":"eDP-1","enabled":true,"connected":true,"rotation":2,
			"currentModeId":"1","pos":{"x":0,"y":0},
			"modes":[{"id":"1","refreshRate":60.0,"size":{"width":1920,"height":1080}},
			{"id":"2","refreshRate":60.0,"size":{"width":1280,"height":1024}}]}]}"#;
		let o = parse_kde(j);
		assert_eq!(o[0].modes[0].0, Cand { w: 1080, h: 1920, hz: 60.0, scale: 1 });
		// Portrait pane on a portrait (rotated) panel already at the best fit → no change.
		assert_eq!(wl_plan(&o, 540, 960).unwrap(), None);
		let (_, cur, new, _) = wl_plan(&o, 1024, 1280).unwrap().unwrap();
		assert_eq!((cur.as_str(), new.as_str()), ("1", "2"));
	}

	#[test]
	fn several_outputs_are_refused() {
		let j = r#"[{"name":"A","enabled":true,"modes":[{"width":1920,"height":1080,"refresh":60.0,"current":true}]},
			{"name":"B","enabled":true,"modes":[{"width":1920,"height":1080,"refresh":60.0,"current":true}]}]"#;
		assert!(wl_plan(&parse_wlr(j), 1280, 800).is_err());
	}

	#[test]
	fn prev_mode_round_trips_through_the_journal_format() {
		let p = PrevMode::X11 { output: "HDMI-1".into(), mode: "1920x1080".into(), rate: "60.00".into(), x: 0, y: 0 };
		let s = serde_json::to_string(&Entry { pid: 1, prev: p.clone() }).unwrap();
		let back: Entry = serde_json::from_str(&s).unwrap();
		assert_eq!(back.prev, p);
	}
}
