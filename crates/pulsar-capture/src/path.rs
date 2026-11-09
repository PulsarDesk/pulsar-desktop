//! Which full-monitor capture source a build uses, and the measurement that picks it.
//!
//! Two sources feed the same NVENC encoder:
//! - **DXGI Desktop Duplication** (`dxgi.rs`) must run on the adapter that OWNS the monitor.
//!   On a hybrid laptop whose panel belongs to the iGPU, every frame then takes the encoder's
//!   CPU staging hop to reach NVENC (`encode/d3d.rs` `CrossAdapterBridge`: iGPU → RAM →
//!   NVIDIA, ~9 MB each way per 1920x1200 frame, behind a blocking `Map`).
//! - **Windows Graphics Capture** (`wgc.rs`) may run on ANY adapter. Created on the NVIDIA
//!   device, Windows hands the composed desktop over GPU-side and the encoder takes its
//!   same-device path (no staging hop). In exchange, WGC hands a frame over a little later
//!   than DXGI does (DWM copies it into the capture pool first).
//!
//! ## How the choice is measured
//! Measured 2026-11-08 on an RTX 3080 Laptop + Vega iGPU host with a paint-marker benchmark
//! (`examples/capbench.rs`: a window paints a counter, the capture thread reads it back, so
//! the clock runs from the paint to the encoded access unit — the same reference for both):
//!
//! | monitor (1200p/1440p @120)   | DXGI p50 / p90 | WGC p50 / p90 | encode tick DXGI / WGC |
//! |------------------------------|----------------|---------------|------------------------|
//! | panel on the iGPU (hybrid)   | 17.0 / 19.6 ms | 15.4 / 17.0   | 6.1 / 2.4 ms           |
//! | monitor on the NVIDIA GPU    |  9.3 / 12.1 ms | 11.0 / 13.9   | 3.0 / 3.3 ms           |
//!
//! The panel's own encoder (Vega VCN HEVC via Media Foundation, `dxgi-mf`): 16.0 / 17.6 ms,
//! tick 7.2 ms. The probe there: WGC 4.57, DXGI 6.48, DXGI-MF 8.40 → WGC.
//!
//! The APIs' own frame timestamps can NOT be compared: WGC's `SystemRelativeTime` marks a
//! moment ~6 ms later than DXGI's `LastPresentTime` for the same paint, so a timestamp race
//! would pick WGC everywhere — wrong for the NVIDIA monitor. What does compare is
//! - the encode tick on our own clock (it contains the CPU hop), and
//! - WGC's later hand-over, which came out the same on both monitors (+1.5 / +2.1 ms):
//!   `WGC_HANDOVER_US`.
//!
//! So on a hybrid host the first session runs `PROBE_SAMPLES` ticks on each source (~2 s
//! each at 120 fps; static content works too), keeps the lower `tick + hand-over` and caches
//! the verdict per adapter pair + driver versions + monitor mode + fps
//! (`%LOCALAPPDATA%\dev.pulsar.app\capture-paths.txt`). A driver update or another mode
//! measures again. A monitor on the NVIDIA GPU (or a host without one) keeps DXGI: same
//! device, nothing to bridge, and the faster hand-over.
//!
//! A third candidate encodes on the display GPU itself: DXGI capture there + its own hardware
//! encoder through Media Foundation (`encode/mf.rs`, e.g. AMD VCN) — no hop at all, but a
//! weaker encoder than NVENC, so it must win by `MF_MARGIN_US` to be picked.
//!
//! Knobs: `PULSAR_CAPTURE=dxgi|wgc|dxgi-mf` forces a path (no measuring);
//! `PULSAR_CAPTURE_REPROBE=1` ignores the cache.
#![cfg(windows)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::Interface;
use windows::Win32::Graphics::Dxgi::{
	CreateDXGIFactory1, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
	DXGI_ERROR_NOT_FOUND,
};

use crate::LiveStats;

/// PCI vendor id of NVIDIA (the only NVENC vendor).
const VENDOR_NVIDIA: u32 = 0x10DE;

/// Encode ticks per source before a probe verdict (~2 s at 120 fps).
pub(crate) const PROBE_SAMPLES: usize = 240;
/// Frames skipped after a (re)build before sampling: the IDR burst and the first frames of a
/// new duplication/session are not steady state.
const WARMUP_FRAMES: u32 = 30;
/// How much later WGC hands a composed frame over than DXGI does (see the module doc).
const WGC_HANDOVER_US: u32 = 2000;
/// The display GPU's own encoder must be this much faster than NVENC to be picked: at the same
/// bitrate NVENC's picture is the cleaner one.
const MF_MARGIN_US: u32 = 1000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MonSource {
	/// DXGI capture → NVENC (a CPU hop when the monitor is on another GPU).
	Dxgi,
	/// WGC capture on the NVIDIA device → NVENC.
	Wgc,
	/// DXGI capture → the display GPU's own encoder (Media Foundation).
	DxgiMf,
}

impl MonSource {
	pub(crate) fn name(self) -> &'static str {
		match self {
			MonSource::Dxgi => "dxgi",
			MonSource::Wgc => "wgc",
			MonSource::DxgiMf => "dxgi-mf",
		}
	}
	/// Where to go when this path can't be built: the other capture source (DXGI refuses a
	/// monitor this process already duplicates; WGC can be missing), NVENC for the MF path.
	pub(crate) fn other(self) -> Self {
		match self {
			MonSource::Dxgi => MonSource::Wgc,
			MonSource::Wgc => MonSource::Dxgi,
			MonSource::DxgiMf => MonSource::Dxgi,
		}
	}
	/// The path encodes with Media Foundation instead of NVENC.
	pub(crate) fn is_mf(self) -> bool {
		self == MonSource::DxgiMf
	}
	fn parse(s: &str) -> Option<Self> {
		match s.trim() {
			"dxgi" => Some(MonSource::Dxgi),
			"wgc" => Some(MonSource::Wgc),
			"dxgi-mf" => Some(MonSource::DxgiMf),
			_ => None,
		}
	}
	/// The `LiveStats::source` code (see `CaptureStats::source`).
	pub(crate) fn code(self) -> u32 {
		match self {
			MonSource::Dxgi => 1,
			MonSource::Wgc => 2,
			MonSource::DxgiMf => 4,
		}
	}
	/// What this path adds on top of its encode tick: WGC's later hand-over, MF's margin.
	fn handover_us(self) -> u32 {
		match self {
			MonSource::Dxgi => 0,
			MonSource::Wgc => WGC_HANDOVER_US,
			MonSource::DxgiMf => MF_MARGIN_US,
		}
	}
}

/// `PULSAR_CAPTURE=dxgi|wgc`: a forced source, never measured or swapped.
pub(crate) fn forced_source() -> Option<MonSource> {
	std::env::var("PULSAR_CAPTURE").ok().and_then(|v| MonSource::parse(&v))
}

/// The source plan for one monitor.
pub(crate) struct Plan {
	/// Cache key (empty when nothing is measured: forced source, or not a hybrid topology).
	pub(crate) key: String,
	pub(crate) first: MonSource,
	/// Measure every path this session (hybrid host, no cached verdict).
	pub(crate) probe: bool,
	/// The paths still to measure after the current one.
	pub(crate) queue: Vec<MonSource>,
}

impl Plan {
	/// No choice to make (a window capture).
	pub(crate) fn none() -> Self {
		Plan { key: String::new(), first: MonSource::Dxgi, probe: false, queue: Vec::new() }
	}
	fn fixed(key: String, first: MonSource) -> Self {
		Plan { key, first, probe: false, queue: Vec::new() }
	}

	/// A path of the probe could not be built: measure the next one instead (Some), or — none
	/// left — finish with what was measured.
	pub(crate) fn skip(&mut self, results: &mut Vec<(MonSource, Score)>, next: &mut Option<MonSource>) -> Option<MonSource> {
		if !self.probe {
			return None;
		}
		if let Some(n) = (!self.queue.is_empty()).then(|| self.queue.remove(0)) {
			*next = Some(n);
			return Some(n);
		}
		self.probe = false;
		if results.is_empty() {
			return None;
		}
		let winner = decide(results);
		store_async(self.key.clone(), winner, results.clone());
		*next = Some(winner);
		Some(winner)
	}
}

pub(crate) unsafe fn plan(output_idx: u32, fps: u32, forced: Option<MonSource>) -> Plan {
	if let Some(f) = forced {
		return Plan::fixed(String::new(), f);
	}
	let Some(t) = topology(output_idx) else {
		return Plan::none();
	};
	let Some((nv_device, nv_umd)) = t.nvidia else {
		return Plan::none();
	};
	if t.out_vendor == VENDOR_NVIDIA {
		return Plan::none();
	}
	let key = format!(
		"{:04x}:{:04x}@{:x}>{:04x}@{:x} {}x{}@{}",
		t.out_vendor, t.out_device, t.out_umd, nv_device, nv_umd, t.width, t.height, fps
	);
	let reprobe = std::env::var("PULSAR_CAPTURE_REPROBE").map(|v| v == "1").unwrap_or(false);
	match (!reprobe).then(|| cache_get(&key)).flatten() {
		Some(winner) => Plan::fixed(key, winner),
		// Unmeasured: start on WGC (the expected winner — no CPU hop), then DXGI + the hop, then
		// the display GPU's own encoder.
		None => Plan { key, first: MonSource::Wgc, probe: true, queue: vec![MonSource::Dxgi, MonSource::DxgiMf] },
	}
}

/// One path of a probe has its samples: measure the next one (rebuild), or — all measured —
/// cache the fastest and rebuild onto it if it is not the current one.
pub(crate) fn probe_step(
	plan: &mut Plan,
	results: &mut Vec<(MonSource, Score)>,
	next: &mut Option<MonSource>,
	current: MonSource,
	score: Score,
	rebuild: &AtomicBool,
) {
	plan.probe = false;
	results.push((current, score));
	if !plan.queue.is_empty() {
		plan.probe = true;
		*next = Some(plan.queue.remove(0));
		rebuild.store(true, Ordering::Release);
	} else {
		let winner = decide(results);
		store_async(plan.key.clone(), winner, results.clone());
		if winner != current {
			*next = Some(winner);
			rebuild.store(true, Ordering::Release);
		}
	}
}

struct Topology {
	out_vendor: u32,
	out_device: u32,
	out_umd: i64,
	nvidia: Option<(u32, i64)>,
	width: i32,
	height: i32,
}

unsafe fn topology(output_idx: u32) -> Option<Topology> {
	let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
	let (adapter, output, _) = crate::dxgi::CaptureDevice::find_output(&factory, output_idx).ok()?;
	let d = adapter.GetDesc1().ok()?;
	let r = output.GetDesc().ok()?.DesktopCoordinates;
	let nvidia = nvidia_adapter(&factory).and_then(|a| {
		let nd = a.GetDesc1().ok()?;
		Some((nd.DeviceId, umd_version(&a)))
	});
	Some(Topology {
		out_vendor: d.VendorId,
		out_device: d.DeviceId,
		out_umd: umd_version(&adapter),
		nvidia,
		width: r.right - r.left,
		height: r.bottom - r.top,
	})
}

/// The adapter's user-mode driver version (changes on a driver update → measure again).
unsafe fn umd_version(a: &IDXGIAdapter1) -> i64 {
	a.CheckInterfaceSupport(&IDXGIDevice::IID).unwrap_or(0)
}

/// The first hardware NVIDIA adapter, if any.
pub(crate) unsafe fn nvidia_adapter(factory: &IDXGIFactory1) -> Option<IDXGIAdapter1> {
	let mut i = 0u32;
	loop {
		let a = match factory.EnumAdapters1(i) {
			Ok(a) => a,
			Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => return None,
			Err(_) => return None,
		};
		i += 1;
		let Ok(d) = a.GetDesc1() else { continue };
		if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
			continue;
		}
		if d.VendorId == VENDOR_NVIDIA {
			return Some(a);
		}
	}
}

// ---------------------------------------------------------------- measurement

/// One source's probe result: its encode tick (our clock, contains any CPU hop).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Score {
	pub(crate) p50_us: u32,
	pub(crate) p90_us: u32,
}

/// Lower is better: the tick's median with half its tail on top, plus the source's hand-over.
fn cost(m: MonSource, s: Score) -> u32 {
	s.p50_us / 2 + s.p90_us / 2 + m.handover_us()
}

/// The faster source of a finished probe.
pub(crate) fn decide(results: &[(MonSource, Score)]) -> MonSource {
	results
		.iter()
		.min_by_key(|(m, s)| cost(*m, *s))
		.map(|(m, _)| *m)
		.unwrap_or(MonSource::Dxgi)
}

/// Per-build meter: the probe's encode ticks, plus the live stats published to `LiveStats`
/// once a second (fresh-frame latency from the source's own timestamp — comparable over
/// time within one source, NOT across sources; the encode tick; the fresh-frame rate).
pub(crate) struct Meter {
	probe: Vec<u32>,
	lat_win: Vec<u32>,
	submit_win: Vec<u32>,
	seen: u32,
	fresh_since_pub: u32,
	last_pub_ns: i64,
}

impl Meter {
	pub(crate) fn new() -> Self {
		Meter {
			probe: Vec::with_capacity(PROBE_SAMPLES),
			lat_win: Vec::with_capacity(256),
			submit_win: Vec::with_capacity(256),
			seen: 0,
			fresh_since_pub: 0,
			last_pub_ns: 0,
		}
	}

	/// One encoded frame: `present_ns` = when its content was composed by the source's clock
	/// (0 = reused/unknown), `done_ns` = when its access unit went to RTP, `submit_us` = our
	/// encode tick.
	pub(crate) fn record(&mut self, is_new: bool, present_ns: i64, done_ns: i64, submit_us: u32, live: &LiveStats) {
		self.seen += 1;
		self.submit_win.push(submit_us);
		if self.seen > WARMUP_FRAMES && self.probe.len() < PROBE_SAMPLES {
			self.probe.push(submit_us);
		}
		if is_new {
			self.fresh_since_pub += 1;
			let lat = done_ns - present_ns;
			// A negative or absurd span is a clock-domain mismatch, not a latency.
			if present_ns > 0 && (0..500_000_000).contains(&lat) {
				self.lat_win.push((lat / 1000) as u32);
			}
		}
		if self.last_pub_ns == 0 {
			self.last_pub_ns = done_ns;
		} else if done_ns - self.last_pub_ns >= 1_000_000_000 {
			let dt = (done_ns - self.last_pub_ns).max(1);
			live.lat_p50.store(pct(&mut self.lat_win, 0.5), Ordering::Relaxed);
			live.lat_p90.store(pct(&mut self.lat_win, 0.9), Ordering::Relaxed);
			live.sub_p50.store(pct(&mut self.submit_win, 0.5), Ordering::Relaxed);
			live.sub_p90.store(pct(&mut self.submit_win, 0.9), Ordering::Relaxed);
			live.fresh_per_s.store(
				(self.fresh_since_pub as i64 * 1_000_000_000 / dt) as u32,
				Ordering::Relaxed,
			);
			self.lat_win.clear();
			self.submit_win.clear();
			self.fresh_since_pub = 0;
			self.last_pub_ns = done_ns;
		}
	}

	pub(crate) fn samples(&self) -> usize {
		self.probe.len()
	}

	pub(crate) fn score(&self) -> Score {
		let mut v = self.probe.clone();
		Score { p50_us: pct(&mut v, 0.5), p90_us: pct(&mut v, 0.9) }
	}
}

fn pct(v: &mut [u32], p: f64) -> u32 {
	if v.is_empty() {
		return 0;
	}
	v.sort_unstable();
	v[((v.len() - 1) as f64 * p).round() as usize]
}

// ---------------------------------------------------------------- verdict cache

fn cache_path() -> Option<PathBuf> {
	std::env::var_os("LOCALAPPDATA")
		.map(|d| PathBuf::from(d).join("dev.pulsar.app").join("capture-paths.txt"))
}

fn cache_get(key: &str) -> Option<MonSource> {
	let text = std::fs::read_to_string(cache_path()?).ok()?;
	text.lines().find_map(|l| {
		let mut f = l.split('\t');
		(f.next()? == key).then_some(())?;
		MonSource::parse(f.next()?)
	})
}

/// Persist a verdict off the capture thread (file I/O never runs on the pacing path).
pub(crate) fn store_async(key: String, winner: MonSource, results: Vec<(MonSource, Score)>) {
	let detail = results
		.iter()
		.map(|(m, s)| {
			format!(
				"{} tick p50 {:.2} p90 {:.2} ms + hand-over {:.1} = {:.2}",
				m.name(),
				s.p50_us as f32 / 1000.0,
				s.p90_us as f32 / 1000.0,
				m.handover_us() as f32 / 1000.0,
				cost(*m, *s) as f32 / 1000.0
			)
		})
		.collect::<Vec<_>>()
		.join("; ");
	crate::encode_log(&format!("capture path probe [{key}]: {detail} => {}", winner.name()));
	if key.is_empty() {
		return;
	}
	std::thread::spawn(move || {
		let Some(path) = cache_path() else { return };
		let old = std::fs::read_to_string(&path).unwrap_or_default();
		let mut lines: Vec<String> = old
			.lines()
			.filter(|l| l.split('\t').next() != Some(key.as_str()) && !l.trim().is_empty())
			.map(str::to_string)
			.collect();
		lines.push(format!("{key}\t{}\t{detail}", winner.name()));
		if let Some(dir) = path.parent() {
			let _ = std::fs::create_dir_all(dir);
		}
		let _ = std::fs::write(&path, lines.join("\n") + "\n");
	});
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_measured_hosts_pick_the_measured_winners() {
		// Hybrid panel: the CPU hop dwarfs WGC's later hand-over.
		let dxgi = Score { p50_us: 6100, p90_us: 6600 };
		let wgc = Score { p50_us: 2400, p90_us: 2700 };
		assert_eq!(decide(&[(MonSource::Wgc, wgc), (MonSource::Dxgi, dxgi)]), MonSource::Wgc);
		// Same GPU: near-equal ticks, so DXGI's earlier hand-over wins.
		let dxgi = Score { p50_us: 3000, p90_us: 3300 };
		let wgc = Score { p50_us: 3300, p90_us: 3500 };
		assert_eq!(decide(&[(MonSource::Wgc, wgc), (MonSource::Dxgi, dxgi)]), MonSource::Dxgi);
	}

	#[test]
	fn a_probe_measures_every_path_then_rebuilds_onto_the_winner() {
		let mut plan = Plan { key: String::new(), first: MonSource::Wgc, probe: true, queue: vec![MonSource::Dxgi, MonSource::DxgiMf] };
		let mut results = Vec::new();
		let mut next = None;
		let rebuild = AtomicBool::new(false);
		probe_step(&mut plan, &mut results, &mut next, MonSource::Wgc, Score { p50_us: 2400, p90_us: 2700 }, &rebuild);
		assert!(plan.probe && rebuild.swap(false, Ordering::Relaxed));
		assert_eq!(next.take(), Some(MonSource::Dxgi));
		probe_step(&mut plan, &mut results, &mut next, MonSource::Dxgi, Score { p50_us: 6100, p90_us: 6600 }, &rebuild);
		assert!(plan.probe && rebuild.swap(false, Ordering::Relaxed));
		assert_eq!(next.take(), Some(MonSource::DxgiMf));
		probe_step(&mut plan, &mut results, &mut next, MonSource::DxgiMf, Score { p50_us: 4000, p90_us: 4400 }, &rebuild);
		assert!(!plan.probe && rebuild.load(Ordering::Relaxed));
		assert_eq!(next, Some(MonSource::Wgc), "WGC 4.55 vs MF 4.2 + 1.0 margin");
	}

	/// The display GPU's own encoder wins only by a clear margin.
	#[test]
	fn the_mf_encoder_needs_a_clear_lead() {
		let nv = Score { p50_us: 3000, p90_us: 3200 };
		let close = Score { p50_us: 2500, p90_us: 2700 };
		let clear = Score { p50_us: 1500, p90_us: 1700 };
		assert_eq!(decide(&[(MonSource::Dxgi, nv), (MonSource::DxgiMf, close)]), MonSource::Dxgi);
		assert_eq!(decide(&[(MonSource::Dxgi, nv), (MonSource::DxgiMf, clear)]), MonSource::DxgiMf);
	}

	/// A path that can't be built is skipped; the last one finishes the probe.
	#[test]
	fn an_unbuildable_path_is_skipped() {
		let mut plan = Plan { key: String::new(), first: MonSource::Wgc, probe: true, queue: vec![MonSource::DxgiMf] };
		let mut results = vec![(MonSource::Wgc, Score { p50_us: 2400, p90_us: 2700 })];
		let mut next = None;
		assert_eq!(plan.skip(&mut results, &mut next), Some(MonSource::DxgiMf));
		assert_eq!(plan.skip(&mut results, &mut next), Some(MonSource::Wgc));
		assert!(!plan.probe);
	}

	#[test]
	fn the_meter_skips_warmup_and_rejects_bad_spans() {
		let live = LiveStats::default();
		let mut m = Meter::new();
		let mut t = 1_000_000_000i64;
		for _ in 0..WARMUP_FRAMES {
			m.record(false, 0, t, 2000, &live);
			t += 8_333_333;
		}
		assert_eq!(m.samples(), 0);
		for i in 0..10u32 {
			t += 8_333_333;
			m.record(i % 2 == 0, t - 4_000_000, t, 3000, &live);
		}
		assert_eq!(m.samples(), 10);
		assert_eq!(m.score().p50_us, 3000);
		// One second in: a clock-domain mismatch (composed "after" encoded) is not a latency.
		m.record(true, t + 1, t + 1_000_000_000, 3000, &live);
		assert_eq!(live.lat_p50.load(Ordering::Relaxed), 4000);
	}
}
