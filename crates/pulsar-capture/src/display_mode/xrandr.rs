//! Parse `xrandr --current` (X11) into outputs + their modes, and plan a screen-adaptation
//! mode switch on one output. Pure text handling — the app runs the `xrandr` binary.
//!
//! The format (xrandr 1.5):
//!
//! ```text
//! Screen 0: minimum 320 x 200, current 3840 x 1080, maximum 16384 x 16384
//! HDMI-1 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 527mm x 296mm
//!    1920x1080     60.00*+  50.00    59.94
//!    1280x720      60.00 +  50.00
//!    1920x1080i    60.00    50.00
//! DP-1 disconnected (normal left inverted right x axis y axis)
//! ```
//!
//! Each rate is printed as `%6.2f` followed by `*` (current) or a space and `+` (preferred)
//! or a space, so the markers can be glued to the number (`60.00*+`) or stand alone (`+`).

use super::Cand;

/// One rate of a mode line. `text` is what xrandr printed (passed back to `--rate`).
#[derive(Clone, Debug, PartialEq)]
pub struct XRate {
	pub text: String,
	pub hz: f64,
	pub current: bool,
	pub preferred: bool,
}

/// One mode line: the mode name (what `--mode` takes) and its rates.
#[derive(Clone, Debug, PartialEq)]
pub struct XMode {
	pub name: String,
	pub w: u32,
	pub h: u32,
	/// Interlaced (`1920x1080i`) — never picked for adaptation.
	pub interlaced: bool,
	pub rates: Vec<XRate>,
}

/// One output (connector) section.
#[derive(Clone, Debug, PartialEq)]
pub struct XOutput {
	pub name: String,
	pub connected: bool,
	pub primary: bool,
	/// `(w, h, x, y)` of the output in the root window, `None` when it is off.
	pub geom: Option<(u32, u32, i32, i32)>,
	pub modes: Vec<XMode>,
}

/// The mode an output runs in now: `(mode index, rate index)`.
pub fn current(out: &XOutput) -> Option<(usize, usize)> {
	out.modes.iter().enumerate().find_map(|(mi, m)| {
		m.rates.iter().position(|r| r.current).map(|ri| (mi, ri))
	})
}

/// True when the output is rotated a quarter turn: its geometry is the current mode's size
/// with width and height swapped.
pub fn rotated(out: &XOutput) -> bool {
	let (Some((gw, gh, _, _)), Some((mi, _))) = (out.geom, current(out)) else {
		return false;
	};
	let m = &out.modes[mi];
	gw != gh && (gw, gh) == (m.h, m.w)
}

/// What to run for an adaptation switch: `xrandr --output <output> --mode <mode> --rate <rate>`.
#[derive(Clone, Debug, PartialEq)]
pub struct XPlan {
	pub mode: String,
	pub rate: String,
	/// The captured size in that mode (already in the output's orientation).
	pub w: u32,
	pub h: u32,
}

/// Plan the switch of `out` to the mode that best fits a `target_w × target_h` pane. `None` =
/// nothing to do (output off / no current mode / already the best fit).
pub fn plan(out: &XOutput, target_w: u32, target_h: u32) -> Option<XPlan> {
	let (cur_m, cur_r) = current(out)?;
	let rot = rotated(out);
	let mut cands = Vec::new();
	let mut index = Vec::new();
	let mut cur_idx = None;
	for (mi, m) in out.modes.iter().enumerate() {
		// The current mode stays a candidate even if interlaced (it is the reference size).
		if m.interlaced && mi != cur_m {
			continue;
		}
		for (ri, r) in m.rates.iter().enumerate() {
			if (mi, ri) == (cur_m, cur_r) {
				cur_idx = Some(cands.len());
			}
			let (w, h) = if rot { (m.h, m.w) } else { (m.w, m.h) };
			cands.push(Cand { w, h, hz: r.hz, scale: 1 });
			index.push((mi, ri));
		}
	}
	let pick = super::plan(&cands, cur_idx?, target_w, target_h)?;
	let (mi, ri) = index[pick];
	Some(XPlan {
		mode: out.modes[mi].name.clone(),
		rate: out.modes[mi].rates[ri].text.clone(),
		w: cands[pick].w,
		h: cands[pick].h,
	})
}

/// `WxH` prefix of a mode name (`1920x1080`, `1920x1080i`, `1920x1080_60.00`).
fn mode_size(name: &str) -> Option<(u32, u32, bool)> {
	let (w, rest) = name.split_once('x')?;
	let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
	let suffix = &rest[digits.len()..];
	Some((w.parse().ok()?, digits.parse().ok()?, suffix.starts_with('i')))
}

/// `WxH+X+Y` (also with negative offsets, `WxH-10+0`).
fn geometry(tok: &str) -> Option<(u32, u32, i32, i32)> {
	let (w, rest) = tok.split_once('x')?;
	let split = rest.find(['+', '-'])?;
	let h = &rest[..split];
	let pos = &rest[split..];
	let second = pos[1..].find(['+', '-'])? + 1;
	let (x, y) = pos.split_at(second);
	Some((w.parse().ok()?, h.parse().ok()?, x.parse().ok()?, y.parse().ok()?))
}

/// Parse the whole `xrandr --current` text.
pub fn parse(text: &str) -> Vec<XOutput> {
	let mut outs: Vec<XOutput> = Vec::new();
	for line in text.lines() {
		if line.trim().is_empty() || line.starts_with("Screen ") {
			continue;
		}
		if !line.starts_with(char::is_whitespace) {
			// Output header: `NAME connected|disconnected [primary] [WxH+X+Y] (...)`.
			let mut it = line.split_whitespace();
			let Some(name) = it.next() else { continue };
			let state = it.next().unwrap_or("");
			let head = line.split('(').next().unwrap_or(line);
			outs.push(XOutput {
				name: name.to_string(),
				connected: state == "connected",
				primary: head.split_whitespace().any(|t| t == "primary"),
				geom: head.split_whitespace().skip(2).find_map(geometry),
				modes: Vec::new(),
			});
			continue;
		}
		// Mode line of the last output. Lines of `--verbose` (`h: width ...`) don't parse as
		// modes and are skipped.
		let Some(out) = outs.last_mut() else { continue };
		let mut it = line.split_whitespace();
		let Some(name) = it.next() else { continue };
		let Some((w, h, interlaced)) = mode_size(name) else { continue };
		let mut rates: Vec<XRate> = Vec::new();
		for tok in it {
			let current = tok.contains('*');
			let preferred = tok.contains('+');
			let num = tok.trim_end_matches(['*', '+']);
			if num.is_empty() {
				// A stand-alone marker belongs to the rate before it.
				if let Some(r) = rates.last_mut() {
					r.current |= current;
					r.preferred |= preferred;
				}
				continue;
			}
			let Ok(hz) = num.parse::<f64>() else { continue };
			rates.push(XRate { text: num.to_string(), hz, current, preferred });
		}
		if !rates.is_empty() {
			out.modes.push(XMode { name: name.to_string(), w, h, interlaced, rates });
		}
	}
	outs
}

#[cfg(test)]
mod tests {
	use super::*;

	const SAMPLE: &str = "Screen 0: minimum 320 x 200, current 4480 x 1440, maximum 16384 x 16384
HDMI-1 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 527mm x 296mm
   1920x1080     60.00*+  50.00    59.94
   1920x1200     59.95
   1280x720      60.00 +  50.00
   1920x1080i    60.00    50.00
DP-1 disconnected (normal left inverted right x axis y axis)
DP-2 connected 1440x2560+1920+0 left (normal left inverted right x axis y axis) 597mm x 336mm
   2560x1440    143.97*  59.95 +
   1920x1080    143.9    60.00
HDMI-2 connected (normal left inverted right x axis y axis)
   1024x768      60.00
";

	#[test]
	fn parses_outputs_and_modes() {
		let o = parse(SAMPLE);
		assert_eq!(o.len(), 4);
		assert_eq!(o[0].name, "HDMI-1");
		assert!(o[0].connected && o[0].primary);
		assert_eq!(o[0].geom, Some((1920, 1080, 0, 0)));
		assert_eq!(o[0].modes.len(), 4);
		assert!(o[0].modes[3].interlaced);
		assert!(o[0].modes[2].rates[0].preferred);
		assert_eq!(current(&o[0]), Some((0, 0)));
		assert!(!o[1].connected);
		assert_eq!(o[2].geom, Some((1440, 2560, 1920, 0)));
		assert!(o[2].modes[0].rates[1].preferred, "a stand-alone + marks the rate before it");
		assert_eq!(current(&o[2]), Some((0, 0)));
		assert!(rotated(&o[2]));
		assert_eq!(o[3].geom, None, "connected but off");
		assert_eq!(current(&o[3]), None);
	}

	#[test]
	fn plans_a_16_10_switch() {
		let o = parse(SAMPLE);
		let p = plan(&o[0], 1280, 800).unwrap();
		assert_eq!((p.mode.as_str(), p.rate.as_str(), p.w, p.h), ("1920x1200", "59.95", 1920, 1200));
		// Already the best 16:9 fit → no switch.
		assert_eq!(plan(&o[0], 1600, 900), None);
		// Off output → nothing.
		assert_eq!(plan(&o[3], 1280, 800), None);
	}

	#[test]
	fn rotated_output_plans_in_its_orientation() {
		let o = parse(SAMPLE);
		// Portrait 9:16 pane on the left-rotated 2560x1440: 1920x1080 at the same ~144 Hz.
		let p = plan(&o[2], 540, 960).unwrap();
		assert_eq!((p.mode.as_str(), p.rate.as_str(), p.w, p.h), ("1920x1080", "143.9", 1080, 1920));
	}

	#[test]
	fn geometry_with_negative_offsets() {
		assert_eq!(geometry("1920x1080-1920+0"), Some((1920, 1080, -1920, 0)));
		assert_eq!(geometry("800x600+10-20"), Some((800, 600, 10, -20)));
		assert_eq!(geometry("primary"), None);
	}
}
