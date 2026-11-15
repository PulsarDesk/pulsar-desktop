//! Is a host monitor in HDR mode, and what does it report (Windows)?
//!
//! - HDR mode = the output's colour space is `DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020`
//!   (`IDXGIOutput6::GetDesc1`), i.e. Windows "Use HDR" is on for that monitor. Only then is
//!   the desktop captured as FP16 scRGB and streamed as HDR10; an SDR monitor always streams
//!   8-bit SDR, whatever the client asked.
//! - The SDR white level (Settings → HDR → "SDR content brightness") is what 1.0 of an SDR
//!   surface (the cursor we composite) maps to on that monitor:
//!   `DISPLAYCONFIG_SDR_WHITE_LEVEL` (units of 1/1000 of 80 nits).
//!
//! A DXGI factory caches output state, so callers that need the CURRENT state (after the user
//! toggled HDR) ask through a fresh factory (`outputs_hdr`, `hdr_for_monitor`; the capture
//! re-creates its factory when DXGI reports it stale).

use windows::core::Interface;
use windows::Win32::Devices::Display::{
	DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
	DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
	DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SDR_WHITE_LEVEL,
	DISPLAYCONFIG_SOURCE_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput1, IDXGIOutput6};

use crate::hdr::HdrMeta;

/// The HDR state of one output: `Some(meta)` while it is in HDR mode.
pub(crate) unsafe fn output_hdr(output: &IDXGIOutput1) -> Option<HdrMeta> {
	if std::env::var("PULSAR_HOST_HDR").map(|v| v == "0").unwrap_or(false) {
		return None; // diagnostics: treat every monitor as SDR
	}
	let o6: IDXGIOutput6 = output.cast().ok()?;
	let d = o6.GetDesc1().ok()?;
	if d.ColorSpace != DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020 {
		return None;
	}
	Some(
		HdrMeta {
			red: (d.RedPrimary[0], d.RedPrimary[1]),
			green: (d.GreenPrimary[0], d.GreenPrimary[1]),
			blue: (d.BluePrimary[0], d.BluePrimary[1]),
			white: (d.WhitePoint[0], d.WhitePoint[1]),
			min_nits: d.MinLuminance,
			max_nits: d.MaxLuminance,
			max_full_frame_nits: d.MaxFullFrameLuminance,
		}
		.sanitized(),
	)
}

/// The SDR white level of the output `output` in nits (80 – 480 typically; 80 for an SDR
/// monitor or when Windows does not say).
pub(crate) unsafe fn sdr_white_nits(output: &IDXGIOutput1) -> f32 {
	let Ok(desc) = output.GetDesc() else {
		return 80.0;
	};
	sdr_white_nits_for_name(&desc.DeviceName)
}

/// [`sdr_white_nits`] of the monitor `hmon` (GDI device name lookup, no DXGI factory): cheap
/// enough for the host to re-read every few seconds while an HDR stream runs (the user can
/// move the "SDR content brightness" slider mid-stream).
pub(crate) unsafe fn monitor_sdr_white_nits(hmon: windows::Win32::Graphics::Gdi::HMONITOR) -> f32 {
	use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFOEXW};
	let mut mi = MONITORINFOEXW::default();
	mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
	if !GetMonitorInfoW(hmon, &mut mi.monitorInfo).as_bool() {
		return 80.0;
	}
	sdr_white_nits_for_name(&mi.szDevice)
}

/// The SDR white level of the display source with GDI device name `name` (`\\.\DISPLAYn`,
/// NUL-terminated UTF-16) in nits; 80 when Windows does not say.
unsafe fn sdr_white_nits_for_name(name: &[u16]) -> f32 {
	let want: Vec<u16> = name.iter().copied().take_while(|c| *c != 0).collect();
	let (mut np, mut nm) = (0u32, 0u32);
	if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm).is_err() {
		return 80.0;
	}
	let mut paths: Vec<DISPLAYCONFIG_PATH_INFO> = (0..np).map(|_| std::mem::zeroed()).collect();
	let mut modes: Vec<DISPLAYCONFIG_MODE_INFO> = (0..nm).map(|_| std::mem::zeroed()).collect();
	if QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None)
		.is_err()
	{
		return 80.0;
	}
	for p in paths.iter().take(np as usize) {
		let mut src: DISPLAYCONFIG_SOURCE_DEVICE_NAME = std::mem::zeroed();
		src.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
		src.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
		src.header.adapterId = p.sourceInfo.adapterId;
		src.header.id = p.sourceInfo.id;
		if DisplayConfigGetDeviceInfo(&mut src.header) != 0 {
			continue;
		}
		let name: Vec<u16> = src.viewGdiDeviceName.iter().copied().take_while(|c| *c != 0).collect();
		if name != want {
			continue;
		}
		let mut white: DISPLAYCONFIG_SDR_WHITE_LEVEL = std::mem::zeroed();
		white.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL;
		white.header.size = std::mem::size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32;
		white.header.adapterId = p.targetInfo.adapterId;
		white.header.id = p.targetInfo.id;
		if DisplayConfigGetDeviceInfo(&mut white.header) == 0 && white.SDRWhiteLevel > 0 {
			return (white.SDRWhiteLevel as f32 / 1000.0 * 80.0).clamp(80.0, 1000.0);
		}
		break;
	}
	80.0
}

/// [`output_hdr`] of the DXGI output that drives `hmon` (a window's monitor), fresh factory.
pub(crate) unsafe fn hdr_for_monitor(hmon: windows::Win32::Graphics::Gdi::HMONITOR) -> Option<HdrMeta> {
	let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
	hdr_for_monitor_in(&factory, hmon)
}

/// [hdr_for_monitor] through an existing factory (current only while IsCurrent()).
unsafe fn hdr_for_monitor_in(factory: &IDXGIFactory1, hmon: windows::Win32::Graphics::Gdi::HMONITOR) -> Option<HdrMeta> {
	let mut i = 0;
	while let Ok((_, out, actual)) = super::CaptureDevice::find_output(factory, i) {
		if actual != i || i > 16 {
			break;
		}
		if out.GetDesc().map(|d| d.Monitor == hmon).unwrap_or(false) {
			return output_hdr(&out);
		}
		i += 1;
	}
	None
}

/// Per attached output (DXGI order): in HDR mode now? For `StreamCaps::displays`.
pub(crate) unsafe fn outputs_hdr() -> Vec<bool> {
	let mut v = Vec::new();
	let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
		return v;
	};
	let mut i = 0;
	while let Ok((_, out, actual)) = super::CaptureDevice::find_output(&factory, i) {
		if actual != i {
			break; // find_output fell back to output 0: past the end
		}
		v.push(output_hdr(&out).is_some());
		i += 1;
		if i > 16 {
			break;
		}
	}
	v
}

/// Re-checks, for a RUNNING capture, whether a monitor is in HDR mode, without the cost of a
/// fresh DXGI factory + full output walk on every check: the factory is kept and the outputs
/// are walked again only when DXGI says the factory is stale (`IsCurrent()` false: an HDR
/// toggle or any other display-configuration change) or the monitor itself changed (a window
/// moved to another one). At most once per `every`; between checks it costs a clock read.
pub(crate) struct HdrWatch {
	factory: Option<IDXGIFactory1>,
	mon: windows::Win32::Graphics::Gdi::HMONITOR,
	hdr: bool,
	at: std::time::Instant,
}

impl HdrWatch {
	/// `hdr`: the state the capture was built with (the first walk waits for `every`).
	pub(crate) fn new(mon: windows::Win32::Graphics::Gdi::HMONITOR, hdr: bool) -> Self {
		Self { factory: None, mon, hdr, at: std::time::Instant::now() }
	}

	/// `Some(in HDR mode now)` when a check ran, `None` before `every` elapsed.
	pub(crate) unsafe fn poll(&mut self, mon: windows::Win32::Graphics::Gdi::HMONITOR, every: std::time::Duration) -> Option<bool> {
		if self.at.elapsed() < every {
			return None;
		}
		self.at = std::time::Instant::now();
		let stale = self.factory.as_ref().is_none_or(|f| !f.IsCurrent().as_bool());
		if stale {
			self.factory = CreateDXGIFactory1::<IDXGIFactory1>().ok();
		}
		if stale || mon != self.mon {
			self.mon = mon;
			self.hdr = self.factory.as_ref().is_some_and(|f| hdr_for_monitor_in(f, mon).is_some());
		}
		Some(self.hdr)
	}
}