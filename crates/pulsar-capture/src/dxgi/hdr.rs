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
	let want: Vec<u16> = desc.DeviceName.iter().copied().take_while(|c| *c != 0).collect();
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
	let mut i = 0;
	while let Ok((_, out, actual)) = super::CaptureDevice::find_output(&factory, i) {
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
