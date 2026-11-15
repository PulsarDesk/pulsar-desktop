//! What this machine's hardware Media Foundation encoders can do: which codecs the display
//! GPU encodes (`mf_codecs`, for the host's native-path decision) and, for diagnosis, which
//! `ICodecAPI` knobs each MFT takes (`mf_report`, printed by `examples/mfprobe.rs`).
#![cfg(windows)]

use std::ffi::c_void;

use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Variant::VARIANT;

use crate::Codec;

/// One hardware encoder MFT: its activation object, friendly name and
/// `MFT_ENUM_HARDWARE_VENDOR_ID_Attribute` (`VEN_1002`, `VEN_QCOM`, …).
pub(crate) struct HwMft {
	pub(crate) activate: IMFActivate,
	pub(crate) name: String,
	pub(crate) vendor: String,
}

pub(crate) fn subtype(codec: Codec) -> windows::core::GUID {
	match codec {
		Codec::H264 => MFVideoFormat_H264,
		Codec::H265 => MFVideoFormat_HEVC,
		Codec::Av1 => MFVideoFormat_AV1,
	}
}

/// Every async hardware encoder MFT that takes NV12 and emits `codec`, in MF's preferred order.
pub(crate) unsafe fn hardware_mfts(codec: Codec) -> Vec<HwMft> {
	let out_info = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: subtype(codec) };
	let in_info = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
	let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
	let mut count = 0u32;
	if MFTEnumEx(
		MFT_CATEGORY_VIDEO_ENCODER,
		MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
		Some(&in_info),
		Some(&out_info),
		&mut list,
		&mut count,
	)
	.is_err()
		|| list.is_null()
	{
		return Vec::new();
	}
	let mut v = Vec::new();
	for act in std::slice::from_raw_parts(list, count as usize).iter().flatten() {
		v.push(HwMft {
			activate: act.clone(),
			name: attr_string(act, &MFT_FRIENDLY_NAME_Attribute).unwrap_or_default(),
			vendor: attr_string(act, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute).unwrap_or_default(),
		});
	}
	// The array's own references are released with it (the clones above keep theirs).
	for i in 0..count as usize {
		std::ptr::drop_in_place(list.add(i));
	}
	windows::Win32::System::Com::CoTaskMemFree(Some(list as *const c_void));
	v
}

/// Whether an MFT's vendor attribute names the DXGI adapter vendor `vendor`. PCI vendors are
/// `VEN_1002` (AMD) / `VEN_8086` (Intel); an ACPI vendor id (Qualcomm's 0x4D4F4351) is four
/// ASCII letters and the MFT says `VEN_QCOM`.
pub(crate) fn vendor_matches(attr: &str, vendor: u32) -> bool {
	let attr = attr.trim();
	if attr.eq_ignore_ascii_case(&format!("VEN_{vendor:04X}")) {
		return true;
	}
	let b = vendor.to_le_bytes();
	if vendor > 0xFFFF && b.iter().all(|c| c.is_ascii_alphanumeric()) {
		let fourcc: String = b.iter().map(|&c| c as char).collect();
		return attr.eq_ignore_ascii_case(&format!("VEN_{fourcc}"));
	}
	false
}

/// The hardware adapters' vendor ids (DXGI order).
pub(crate) unsafe fn hardware_vendors() -> Vec<u32> {
	let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else { return Vec::new() };
	let mut v = Vec::new();
	let mut i = 0;
	while let Ok(a) = factory.EnumAdapters1(i) {
		i += 1;
		let Ok(d) = a.GetDesc1() else { continue };
		if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 || d.VendorId == 0x1414 {
			continue; // WARP / Microsoft Basic Render
		}
		if !v.contains(&d.VendorId) {
			v.push(d.VendorId);
		}
	}
	v
}

/// The MFT for an adapter of `vendor`: the one whose vendor attribute names it, or — when the
/// adapter is the machine's only hardware GPU and no attribute matches (a driver that names
/// itself differently) — the first hardware MFT.
pub(crate) unsafe fn mft_for_vendor(mfts: Vec<HwMft>, vendor: u32, only_gpu: bool) -> Option<HwMft> {
	let mut first = None;
	for m in mfts {
		if vendor_matches(&m.vendor, vendor) {
			return Some(m);
		}
		if first.is_none() {
			first = Some(m);
		}
	}
	first.filter(|_| only_gpu)
}

/// The codecs a non-NVIDIA display GPU of this machine encodes through Media Foundation
/// (H.264 / HEVC / AV1; NVIDIA's own MFT is left to NVENC). Cached for the process.
pub fn mf_codecs() -> &'static [Codec] {
	use std::sync::OnceLock;
	static CACHE: OnceLock<Vec<Codec>> = OnceLock::new();
	CACHE.get_or_init(|| unsafe {
		let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
		if MFStartup(MF_VERSION, MFSTARTUP_FULL).is_err() {
			return Vec::new();
		}
		let vendors: Vec<u32> = hardware_vendors().into_iter().filter(|v| *v != 0x10DE).collect();
		let only = hardware_vendors().len() == 1;
		let mut out = Vec::new();
		for codec in [Codec::H264, Codec::H265, Codec::Av1] {
			if vendors.iter().any(|&ven| mft_for_vendor(hardware_mfts(codec), ven, only).is_some()) {
				out.push(codec);
			}
		}
		let _ = MFShutdown();
		super::cap_dbg(&format!("mf_codecs: {out:?} (adapters {vendors:04X?})"));
		out
	})
}

/// The `ICodecAPI` knobs the encoder questions turn on, with their names.
fn knobs() -> Vec<(&'static str, windows::core::GUID)> {
	vec![
		("RateControlMode", CODECAPI_AVEncCommonRateControlMode),
		("MeanBitRate", CODECAPI_AVEncCommonMeanBitRate),
		("MaxBitRate", CODECAPI_AVEncCommonMaxBitRate),
		("BufferSize", CODECAPI_AVEncCommonBufferSize),
		("QualityVsSpeed", CODECAPI_AVEncCommonQualityVsSpeed),
		("LowLatencyMode", CODECAPI_AVLowLatencyMode),
		("ScenarioInfo", CODECAPI_AVScenarioInfo),
		("ContentType", CODECAPI_AVEncVideoContentType),
		("BPictureCount", CODECAPI_AVEncMPVDefaultBPictureCount),
		("GOPSize", CODECAPI_AVEncMPVGOPSize),
		("ForceKeyFrame", CODECAPI_AVEncVideoForceKeyFrame),
		("MaxNumRefFrame", CODECAPI_AVEncVideoMaxNumRefFrame),
		("LTRBufferControl", CODECAPI_AVEncVideoLTRBufferControl),
		("MarkLTRFrame", CODECAPI_AVEncVideoMarkLTRFrame),
		("UseLTRFrame", CODECAPI_AVEncVideoUseLTRFrame),
		("GradualIntraRefresh", CODECAPI_AVEncVideoGradualIntraRefresh),
		("TemporalLayerCount", CODECAPI_AVEncVideoTemporalLayerCount),
		("MaxTemporalLayers", CODECAPI_AVEncVideoMaxTemporalLayers),
		("SliceControlMode", CODECAPI_AVEncSliceControlMode),
		("EncodeQP", CODECAPI_AVEncVideoEncodeQP),
		("MinQP", CODECAPI_AVEncVideoMinQP),
		("MaxQP", CODECAPI_AVEncVideoMaxQP),
		("NumWorkerThreads", CODECAPI_AVEncNumWorkerThreads),
	]
}

pub(crate) fn variant_u64(v: &VARIANT) -> Option<u64> {
	use windows::Win32::System::Variant::*;
	unsafe {
		let inner = &v.Anonymous.Anonymous;
		Some(match inner.vt {
			VT_UI4 => inner.Anonymous.ulVal as u64,
			VT_UI8 => inner.Anonymous.ullVal,
			VT_I4 => inner.Anonymous.lVal as u64,
			VT_UI2 => inner.Anonymous.uiVal as u64,
			VT_BOOL => (inner.Anonymous.boolVal.0 != 0) as u64,
			_ => return None,
		})
	}
}

/// A human-readable list of every hardware encoder MFT and the knobs it supports.
pub fn mf_report() -> String {
	use std::fmt::Write;
	let mut s = String::new();
	unsafe {
		let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
		if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
			return format!("MFStartup: {e}");
		}
		let _ = writeln!(s, "hardware adapters: {:08X?}", hardware_vendors());
		for codec in [Codec::H264, Codec::H265, Codec::Av1] {
			let mfts = hardware_mfts(codec);
			let _ = writeln!(s, "{codec:?}: {} hardware MFT(s)", mfts.len());
			for m in mfts {
				let _ = writeln!(s, "  \"{}\" vendor={}", m.name, m.vendor);
				let t: IMFTransform = match m.activate.ActivateObject() {
					Ok(t) => t,
					Err(e) => {
						let _ = writeln!(s, "    activate failed: {e}");
						continue;
					}
				};
				if let Ok(a) = t.GetAttributes() {
					let d3d11 = a.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0);
					let _ = writeln!(s, "    D3D11 aware: {d3d11}");
				}
				match windows::core::Interface::cast::<ICodecAPI>(&t) {
					Ok(api) => {
						for (name, g) in knobs() {
							if api.IsSupported(&g).is_err() {
								continue;
							}
							let modifiable = api.IsModifiable(&g).is_ok();
							let cur = api.GetValue(&g).ok().and_then(|v| variant_u64(&v));
							let (mut lo, mut hi, mut st) = (VARIANT::default(), VARIANT::default(), VARIANT::default());
							let range = api
								.GetParameterRange(&g, &mut lo, &mut hi, &mut st)
								.ok()
								.map(|_| (variant_u64(&lo), variant_u64(&hi), variant_u64(&st)));
							let _ = writeln!(
								s,
								"    {name:<20} modifiable={modifiable} value={cur:?}{}",
								range.map(|r| format!(" range={r:?}")).unwrap_or_default()
							);
						}
					}
					Err(e) => {
						let _ = writeln!(s, "    no ICodecAPI: {e}");
					}
				}
				if let Ok(sd) = windows::core::Interface::cast::<IMFShutdown>(&t) {
					let _ = sd.Shutdown();
				}
				let _ = m.activate.ShutdownObject();
			}
		}
		let _ = MFShutdown();
	}
	s
}

unsafe fn attr_string(a: &IMFActivate, key: &windows::core::GUID) -> Option<String> {
	let mut p = windows::core::PWSTR::null();
	let mut n = 0u32;
	a.GetAllocatedString(key, &mut p, &mut n).ok()?;
	let s = p.to_string().ok();
	windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const c_void));
	s
}

#[cfg(test)]
mod tests {
	use super::vendor_matches;

	#[test]
	fn vendor_attribute_matching() {
		assert!(vendor_matches("VEN_1002", 0x1002));
		assert!(vendor_matches("ven_8086", 0x8086));
		assert!(vendor_matches("VEN_QCOM", 0x4D4F_4351));
		assert!(!vendor_matches("VEN_10DE", 0x1002));
		assert!(!vendor_matches("VEN_QCOM", 0x1002));
	}
}
