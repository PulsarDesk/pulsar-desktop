//! GNOME (Mutter) on Wayland: screen adaptation through Mutter's own D-Bus API.
//!
//! Wayland has no standard protocol for a client to change an output's mode, and Mutter does
//! not implement `wlr-output-management`. What it has is the private, but long-stable,
//! `org.gnome.Mutter.DisplayConfig` interface that GNOME Settings itself uses:
//!
//! * `GetCurrentState()` — every monitor with its modes (and which one is current), and the
//!   logical layout (position, scale, transform, primary, which monitors);
//! * `ApplyMonitorsConfig(serial, method, logical_monitors, props)` — apply a WHOLE layout.
//!   `method = 1` is "temporary": applied now, not written to `monitors.xml`, so a crashed
//!   Pulsar never leaves the mode in the saved user configuration (the crash journal in
//!   `linux.rs` still restores the live one at the next start; a re-login does too).
//!
//! The new layout is the current one with only the shared monitor's mode replaced; the scale is
//! kept when the new mode supports it, else the mode's preferred scale is used. Mirrored logical
//! monitors (two monitors in one) are refused. The same single-enabled-output rule as the other
//! Wayland paths applies (the ScreenCast portal does not say which monitor is shared).
//!
//! The D-Bus calls run on their own thread: zbus' blocking API spins its own runtime, which must
//! not happen on a tokio worker (the StreamReq handler may be one).
//!
//! Untested on GNOME hardware (written against Mutter's DisplayConfig XML, type-checked on WSL).

use std::collections::HashMap;

use pulsar_capture::display_mode::Cand;

use super::WlOutput;

/// One mode of a monitor, as `GetCurrentState` lists it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct GMode {
	pub id: String,
	pub w: u32,
	pub h: u32,
	pub hz: f64,
	pub preferred_scale: f64,
	pub scales: Vec<f64>,
	pub current: bool,
}

/// One physical monitor (`connector` = `DP-1`, `eDP-1`, …).
#[derive(Clone, Debug, PartialEq)]
pub(super) struct GMonitor {
	pub connector: String,
	pub modes: Vec<GMode>,
}

/// One logical monitor of the layout.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct GLogical {
	pub x: i32,
	pub y: i32,
	pub scale: f64,
	/// 0 normal, 1 90°, 2 180°, 3 270°, 4–7 the flipped variants.
	pub transform: u32,
	pub primary: bool,
	pub connectors: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct GState {
	pub serial: u32,
	pub monitors: Vec<GMonitor>,
	pub logical: Vec<GLogical>,
}

/// A logical monitor of an `ApplyMonitorsConfig` call: `(x, y, scale, transform, primary,
/// [(connector, mode id)])`.
pub(super) type GApplyLogical = (i32, i32, f64, u32, bool, Vec<(String, String)>);

/// The monitors in the [`WlOutput`] shape the shared Wayland planner takes. "Enabled" = part of
/// the logical layout; a mirrored logical monitor counts its monitors as disabled (refused).
pub(super) fn outputs(st: &GState) -> Vec<WlOutput> {
	st.monitors
		.iter()
		.map(|m| {
			let lm = st.logical.iter().find(|l| l.connectors.iter().any(|c| c == &m.connector));
			let enabled = lm.is_some_and(|l| l.connectors.len() == 1);
			let quarter = lm.is_some_and(|l| l.transform % 2 == 1);
			let mut current = None;
			let modes = m
				.modes
				.iter()
				.enumerate()
				.map(|(i, md)| {
					if md.current {
						current = Some(i);
					}
					let (w, h) = if quarter { (md.h, md.w) } else { (md.w, md.h) };
					(Cand { w, h, hz: md.hz, scale: 1 }, md.id.clone())
				})
				.collect();
			WlOutput {
				name: m.connector.clone(),
				enabled,
				pos: lm.map_or((0, 0), |l| (l.x, l.y)),
				modes,
				current,
			}
		})
		.collect()
}

/// The current scale of `connector`'s logical monitor.
pub(super) fn scale_of(st: &GState, connector: &str) -> Option<f64> {
	st.logical.iter().find(|l| l.connectors.iter().any(|c| c == connector)).map(|l| l.scale)
}

/// The layout to apply: the current one, with `connector` switched to `mode_id` at `scale`
/// (kept when the new mode supports it, else that mode's preferred scale). `None` when the
/// connector or mode is unknown, or a monitor has no current mode.
pub(super) fn config(st: &GState, connector: &str, mode_id: &str, scale: f64) -> Option<Vec<GApplyLogical>> {
	let mon = st.monitors.iter().find(|m| m.connector == connector)?;
	let mode = mon.modes.iter().find(|m| m.id == mode_id)?;
	let scale = if mode.scales.iter().any(|s| (s - scale).abs() < 1e-4) || mode.scales.is_empty() {
		scale
	} else {
		mode.preferred_scale
	};
	let mut out = Vec::new();
	for l in &st.logical {
		let mut mons = Vec::new();
		let mut lscale = l.scale;
		for c in &l.connectors {
			if c == connector {
				mons.push((c.clone(), mode_id.to_string()));
				lscale = scale;
			} else {
				let m = st.monitors.iter().find(|m| &m.connector == c)?;
				let cur = m.modes.iter().find(|md| md.current)?;
				mons.push((c.clone(), cur.id.clone()));
			}
		}
		out.push((l.x, l.y, lscale, l.transform, l.primary, mons));
	}
	Some(out)
}

// ---------------------------------------------------------------------------
// D-Bus
// ---------------------------------------------------------------------------

const DEST: &str = "org.gnome.Mutter.DisplayConfig";
const PATH: &str = "/org/gnome/Mutter/DisplayConfig";
/// `ApplyMonitorsConfig` method: 1 = temporary (not saved to monitors.xml).
const METHOD_TEMPORARY: u32 = 1;

type Props = HashMap<String, zbus::zvariant::OwnedValue>;
type RawMode = (String, i32, i32, f64, f64, Vec<f64>, Props);
type RawMonitor = ((String, String, String, String), Vec<RawMode>, Props);
type RawLogical = (i32, i32, f64, u32, bool, Vec<(String, String, String, String)>, Props);
type RawState = (u32, Vec<RawMonitor>, Vec<RawLogical>, Props);

fn flag(p: &Props, key: &str) -> bool {
	p.get(key).and_then(|v| bool::try_from(v).ok()).unwrap_or(false)
}

/// Run `f` on a fresh thread (zbus' blocking API must not run on a tokio worker).
fn off_thread<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
	std::thread::Builder::new()
		.name("mutter-dbus".into())
		.spawn(f)
		.map_err(|e| format!("thread: {e}"))?
		.join()
		.map_err(|_| "Mutter D-Bus thread panicked".to_string())?
}

fn read_state(conn: &zbus::blocking::Connection) -> Result<GState, String> {
	let reply = conn
		.call_method(Some(DEST), PATH, Some(DEST), "GetCurrentState", &())
		.map_err(|e| format!("Mutter GetCurrentState: {e}"))?;
	let (serial, monitors, logical, _): RawState =
		reply.body().deserialize().map_err(|e| format!("Mutter GetCurrentState reply: {e}"))?;
	Ok(GState {
		serial,
		monitors: monitors
			.into_iter()
			.map(|((connector, ..), modes, _)| GMonitor {
				connector,
				modes: modes
					.into_iter()
					.map(|(id, w, h, hz, preferred_scale, scales, p)| GMode {
						id,
						w: w.max(0) as u32,
						h: h.max(0) as u32,
						hz,
						preferred_scale,
						scales,
						current: flag(&p, "is-current"),
					})
					.collect(),
			})
			.collect(),
		logical: logical
			.into_iter()
			.map(|(x, y, scale, transform, primary, mons, _)| GLogical {
				x,
				y,
				scale,
				transform,
				primary,
				connectors: mons.into_iter().map(|(c, ..)| c).collect(),
			})
			.collect(),
	})
}

fn apply_config(conn: &zbus::blocking::Connection, serial: u32, cfg: Vec<GApplyLogical>) -> Result<(), String> {
	type MonProps = HashMap<String, zbus::zvariant::Value<'static>>;
	let logical: Vec<(i32, i32, f64, u32, bool, Vec<(String, String, MonProps)>)> = cfg
		.into_iter()
		.map(|(x, y, s, t, p, mons)| (x, y, s, t, p, mons.into_iter().map(|(c, m)| (c, m, MonProps::new())).collect()))
		.collect();
	conn.call_method(
		Some(DEST),
		PATH,
		Some(DEST),
		"ApplyMonitorsConfig",
		&(serial, METHOD_TEMPORARY, logical, MonProps::new()),
	)
	.map(drop)
	.map_err(|e| format!("Mutter ApplyMonitorsConfig: {e}"))
}

/// Read the layout (for planning).
pub(super) fn state() -> Result<GState, String> {
	off_thread(|| {
		let conn = zbus::blocking::Connection::session().map_err(|e| format!("session bus: {e}"))?;
		read_state(&conn)
	})
}

/// Put `connector` into `mode_id` (at `scale`, see [`config`]) against a freshly read layout
/// (its serial must be current, or Mutter refuses the call).
pub(super) fn set_mode(connector: &str, mode_id: &str, scale: f64) -> Result<(), String> {
	let (connector, mode_id) = (connector.to_string(), mode_id.to_string());
	off_thread(move || {
		let conn = zbus::blocking::Connection::session().map_err(|e| format!("session bus: {e}"))?;
		let st = read_state(&conn)?;
		let cfg = config(&st, &connector, &mode_id, scale)
			.ok_or_else(|| format!("Mutter: no mode {mode_id} on {connector}"))?;
		apply_config(&conn, st.serial, cfg)
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	fn mode(id: &str, w: u32, h: u32, scales: &[f64], current: bool) -> GMode {
		GMode { id: id.into(), w, h, hz: 60.0, preferred_scale: 1.0, scales: scales.to_vec(), current }
	}

	fn sample() -> GState {
		GState {
			serial: 7,
			monitors: vec![GMonitor {
				connector: "DP-1".into(),
				modes: vec![
					mode("2560x1440@59.951", 2560, 1440, &[1.0, 1.25, 1.5, 2.0], true),
					mode("1920x1200@59.950", 1920, 1200, &[1.0, 1.25], false),
					mode("1920x1080@60.000", 1920, 1080, &[1.0, 1.5, 2.0], false),
				],
			}],
			logical: vec![GLogical { x: 0, y: 0, scale: 1.5, transform: 0, primary: true, connectors: vec!["DP-1".into()] }],
		}
	}

	#[test]
	fn plans_through_the_shared_wayland_planner() {
		let st = sample();
		let outs = outputs(&st);
		assert!(outs[0].enabled);
		assert_eq!(outs[0].current, Some(0));
		let (out, cur, new, size) = super::super::wl_plan(&outs, 1280, 800).unwrap().unwrap();
		assert_eq!(out.name, "DP-1");
		assert_eq!((cur.as_str(), new.as_str(), size), ("2560x1440@59.951", "1920x1200@59.950", (1920, 1200)));
	}

	#[test]
	fn config_keeps_or_falls_back_the_scale() {
		let st = sample();
		// 1.5 is not offered by the 16:10 mode → its preferred 1.0.
		let c = config(&st, "DP-1", "1920x1200@59.950", 1.5).unwrap();
		assert_eq!(c, vec![(0, 0, 1.0, 0, true, vec![("DP-1".to_string(), "1920x1200@59.950".to_string())])]);
		// 1.5 is offered by 1920x1080 → kept.
		let c = config(&st, "DP-1", "1920x1080@60.000", 1.5).unwrap();
		assert_eq!(c[0].2, 1.5);
		assert!(config(&st, "DP-1", "nope", 1.0).is_none());
		assert_eq!(scale_of(&st, "DP-1"), Some(1.5));
	}

	#[test]
	fn mirrored_and_rotated() {
		let mut st = sample();
		st.logical[0].transform = 1;
		assert_eq!(outputs(&st)[0].modes[0].0, Cand { w: 1440, h: 2560, hz: 60.0, scale: 1 });
		st.monitors.push(GMonitor { connector: "HDMI-1".into(), modes: vec![mode("1920x1080@60.000", 1920, 1080, &[1.0], true)] });
		st.logical[0].connectors.push("HDMI-1".into());
		let outs = outputs(&st);
		assert!(!outs[0].enabled && !outs[1].enabled, "a mirrored logical monitor is never switched");
		// The other monitor of a layout keeps its current mode.
		let c = config(&st, "DP-1", "1920x1080@60.000", 1.0).unwrap();
		assert_eq!(c[0].5[1], ("HDMI-1".to_string(), "1920x1080@60.000".to_string()));
	}
}
