//! Moonlight-style tiered decoder selection (Linux).
//!
//! Instead of a hardcoded name list, candidates are enumerated per codec and each is
//! VALIDATED by really decoding a tiny canned keyframe (committed under `testdata/`,
//! embedded via `include_bytes!`). First success wins:
//!
//! - **Tier 0 — zero-copy SoC decoders**: any ffmpeg decoder with `AV_CODEC_CAP_HARDWARE`
//!   whose `pix_fmts` include `DRM_PRIME`, or whose hw configs offer DRM_PRIME with internal
//!   setup (ffmpeg-rockchip 7.x declares it only there) (rkmpp, v4l2m2m-drm, future SoCs —
//!   found by capability, not by name). Output feeds the existing dmabuf→EGLImage zero-copy path.
//! - **Tier 1 — generic hwaccels**: the stock decoder + a hw device context, the DISPLAY
//!   GPU first (gpu.rs): VAAPI on the display GPU → CUDA → VAAPI elsewhere → Vulkan → DRM.
//!   A decoder on the display GPU presents without a system-memory copy (VAAPI frames
//!   `av_hwframe_map`ped to DRM_PRIME, CUDA frames via CUDA↔GL interop); any other is
//!   `av_hwframe_transfer_data` read back (NV12 upload) by the caller.
//! - **Tier 2 — software**: the stock decoder, planar/NV12 GL upload. Always present —
//!   the absolute fallback on every platform.
//!
//! `--probe` mode reuses the same machinery headless and prints JSON for the app's
//! startup capability detection.

use crate::video::is_displayable_sw;
use ffmpeg_sys_next as ff;
use std::ffi::CString;
use std::os::raw::c_int;
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};

// Canned single-keyframe bitstreams (320×180 testsrc2; regenerate with
// `ffmpeg -f lavfi -i testsrc2=size=320x180:rate=30 -frames:v 1 -c:v <enc> -f <mux> …`).
// These are the BASELINE, decoder-friendly fixtures (ffmpeg-muxed, conformant).
const TEST_H264: &[u8] = include_bytes!("../testdata/test.h264");
const TEST_H265: &[u8] = include_bytes!("../testdata/test.h265");
const TEST_AV1: &[u8] = include_bytes!("../testdata/test.av1.ivf");

// REAL native-NVENC bitstreams (cut from a `PULSAR_DUMP_BITSTREAM` capture — the same
// path the host streams). A baseline fixture is not enough: the Pi's rkmpp decodes the
// conformant ffmpeg sample fine yet chokes on our native NVENC HEVC ("Multi-layer HEVC
// coding is not implemented", "Skipping NAL unit 30"). Probing the real encoder family's
// output catches that BEFORE negotiation picks a dead codec. An EMPTY file means "no
// NVENC fixture committed yet" and is skipped (so the probe degrades to baseline-only —
// it never falsely flags). See testdata/README.md to regenerate.
const TEST_NVENC_H264: &[u8] = include_bytes!("../testdata/test-nvenc.h264");
const TEST_NVENC_H265: &[u8] = include_bytes!("../testdata/test-nvenc.h265");
const TEST_NVENC_AV1: &[u8] = include_bytes!("../testdata/test-nvenc.av1.ivf");

// Canned 4:4:4 keyframes (H.264 High 4:4:4 / HEVC RExt, same 320×180 testsrc2; see
// testdata/README.md): `select_yuv444` decodes them before the app may ask a host for 4:4:4.
const TEST_444_H264: &[u8] = include_bytes!("../testdata/test-444.h264");
const TEST_444_H265: &[u8] = include_bytes!("../testdata/test-444.h265");

/// One probe sample: the encoder family it represents and the raw bitstream. `family`
/// is empty for the baseline/conformant fixture (a generic correctness check) and an
/// encoder wire-id (`nvenc`) for a family-specific real-world sample.
struct Fixture {
	family: &'static str,
	bytes: &'static [u8],
	/// File extension for the temp path avformat opens (probing wants a real path).
	ext: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
	/// Named/SoC hardware decoder emitting DRM_PRIME directly (rkmpp class).
	ZeroCopyHw,
	/// Stock decoder + hwaccel device context (VAAPI/CUDA/Vulkan/DRM).
	HwAccel,
	/// Plain software decode.
	Software,
}

impl Tier {
	pub fn as_str(self) -> &'static str {
		match self {
			Tier::ZeroCopyHw => "hw-zerocopy",
			Tier::HwAccel => "hwaccel",
			Tier::Software => "software",
		}
	}
}

/// A validated decoder choice for one codec. The raw pointers are 'static (ffmpeg
/// registry data), so this is safe to carry across the decode thread spawn.
#[derive(Clone, Debug)]
pub struct Selected {
	pub name: String,
	pub tier: Tier,
	/// Hwaccel device type for Tier::HwAccel (open a device ctx before decode).
	pub hwdev: Option<ff::AVHWDeviceType>,
	/// The hwaccel pixel format frames will arrive in for Tier::HwAccel.
	pub hw_fmt: ff::AVPixelFormat,
	/// The DRM render node the hw device is opened on (VAAPI only; `None` = the device
	/// type's default, or for VAAPI "any node but the display GPU's").
	pub node: Option<String>,
	/// The decoder runs on the DISPLAY GPU, so its frames can reach GL without a copy
	/// through system memory: VAAPI → dmabuf → EGLImage, CUDA → CUDA↔GL interop. Only an
	/// attempt — the presenter falls back to readback if the import fails (`ZC_FAILED`).
	pub zero_copy: bool,
}

impl Selected {
	/// Identity for the runtime skip list: two VAAPI candidates share a name but not a node.
	pub fn key(&self) -> String {
		key_of(&self.name, self.node.as_deref())
	}
}

fn key_of(name: &str, node: Option<&str>) -> String {
	match node {
		Some(n) => format!("{name}@{n}"),
		None => name.to_string(),
	}
}

struct Candidate {
	dec: *const ff::AVCodec,
	name: String,
	tier: Tier,
	hwdev: Option<ff::AVHWDeviceType>,
	hw_fmt: ff::AVPixelFormat,
	node: Option<String>,
	zero_copy: bool,
}

/// The hwaccel pixel format produced under a device type.
fn hw_fmt_for(dev: ff::AVHWDeviceType) -> ff::AVPixelFormat {
	use ff::AVHWDeviceType::*;
	match dev {
		AV_HWDEVICE_TYPE_VAAPI => ff::AVPixelFormat::AV_PIX_FMT_VAAPI,
		AV_HWDEVICE_TYPE_CUDA => ff::AVPixelFormat::AV_PIX_FMT_CUDA,
		AV_HWDEVICE_TYPE_VULKAN => ff::AVPixelFormat::AV_PIX_FMT_VULKAN,
		AV_HWDEVICE_TYPE_DRM => ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME,
		_ => ff::AVPixelFormat::AV_PIX_FMT_NONE,
	}
}

unsafe fn decoder_name(dec: *const ff::AVCodec) -> String {
	std::ffi::CStr::from_ptr((*dec).name)
		.to_string_lossy()
		.into_owned()
}

/// Does `dec` list `want` among its native output pixel formats?
///
/// ffmpeg 9 removed `AVCodec::pix_fmts`; its replacement,
/// `avcodec_get_supported_config()`, exists since 7.1. `ffmpeg_supported_config`
/// is set by build.rs from the libavcodec version ffmpeg-sys-next detected.
#[cfg(ffmpeg_supported_config)]
unsafe fn pix_fmts_contain(dec: *const ff::AVCodec, want: ff::AVPixelFormat) -> bool {
	let mut list: *const std::ffi::c_void = ptr::null();
	let mut n: std::ffi::c_int = 0;
	let rc = ff::avcodec_get_supported_config(
		ptr::null(),
		dec,
		ff::AVCodecConfig::AV_CODEC_CONFIG_PIX_FORMAT,
		0,
		&mut list,
		&mut n,
	);
	// NULL means "unconstrained" — the same case the old `pix_fmts == NULL` path
	// treated as "not a DRM_PRIME-native decoder"; keep that behaviour.
	if rc < 0 || list.is_null() || n <= 0 {
		return false;
	}
	std::slice::from_raw_parts(list as *const ff::AVPixelFormat, n as usize).contains(&want)
}

#[cfg(not(ffmpeg_supported_config))]
unsafe fn pix_fmts_contain(dec: *const ff::AVCodec, want: ff::AVPixelFormat) -> bool {
	let mut p = (*dec).pix_fmts;
	if p.is_null() {
		return false;
	}
	while *p != ff::AVPixelFormat::AV_PIX_FMT_NONE {
		if *p == want {
			return true;
		}
		p = p.add(1);
	}
	false
}

/// Does `dec` say, in its hw configs, that it outputs DRM_PRIME frames it can set up by
/// ITSELF (`METHOD_INTERNAL`: no device context from us, which tier 0 never creates)?
///
/// Why this exists next to [`pix_fmts_contain`]: a wrapper decoder may declare its output
/// formats ONLY through `hw_configs`. ffmpeg-rockchip 7.x does exactly that —
/// `DEFINE_RKMPP_DECODER` (libavcodec/rkmppdec.h) dropped `.p.pix_fmts` and lists DRM_PRIME
/// in `rkmpp_dec_hw_configs` (device type RKMPP, methods HW_DEVICE_CTX | HW_FRAMES_CTX |
/// INTERNAL) — so on a libavcodec built from it the pix_fmts check alone never offers
/// h264/hevc/av1_rkmpp and the board decodes in software. Still format-based, no names.
unsafe fn hw_config_drm_prime_internal(dec: *const ff::AVCodec) -> bool {
	let mut i = 0;
	loop {
		let cfg = ff::avcodec_get_hw_config(dec, i);
		if cfg.is_null() {
			return false;
		}
		if (*cfg).pix_fmt == ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME
			&& (*cfg).methods & (ff::AV_CODEC_HW_CONFIG_METHOD_INTERNAL as c_int) != 0
			&& (*cfg).device_type != ff::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE
		{
			return true;
		}
		i += 1;
	}
}

/// Tier 0: `AV_CODEC_CAP_HARDWARE` decoders for `codec_id` that output DRM_PRIME, by
/// their pix_fmts or by a self-contained DRM_PRIME hw config (see
/// [`hw_config_drm_prime_internal`]). Registry order.
unsafe fn zero_copy_decoders(codec_id: ff::AVCodecID) -> Vec<*const ff::AVCodec> {
	let mut out = Vec::new();
	let mut it: *mut std::ffi::c_void = ptr::null_mut();
	loop {
		let dec = ff::av_codec_iterate(&mut it);
		if dec.is_null() {
			break;
		}
		if ff::av_codec_is_decoder(dec) == 0 || (*dec).id != codec_id {
			continue;
		}
		if (*dec).capabilities & (ff::AV_CODEC_CAP_HARDWARE as c_int) == 0 {
			continue;
		}
		if pix_fmts_contain(dec, ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME)
			|| hw_config_drm_prime_internal(dec)
		{
			out.push(dec);
		}
	}
	out
}

/// `--list-zerocopy`: the tier-0 decoder NAMES per codec, without opening any of them
/// (`{"h264":["h264_rkmpp"],"h265":[...],"av1":[...]}`). For build checks off the target
/// board: proves a libavcodec offers its SoC decoders to the selection at all, which
/// `--probe` cannot show where the hardware is missing.
pub fn list_zero_copy_json() -> String {
	let mut entries = Vec::new();
	for (label, id) in [
		("h264", ff::AVCodecID::AV_CODEC_ID_H264),
		("h265", ff::AVCodecID::AV_CODEC_ID_HEVC),
		("av1", ff::AVCodecID::AV_CODEC_ID_AV1),
	] {
		let names = unsafe { zero_copy_decoders(id) }
			.into_iter()
			.map(|d| format!(r#""{}""#, unsafe { decoder_name(d) }))
			.collect::<Vec<_>>()
			.join(",");
		entries.push(format!(r#""{label}":[{names}]"#));
	}
	format!("{{{}}}", entries.join(","))
}

/// Enumerate candidates for `codec_id`, tier order. No names anywhere — Tier 0 is
/// "hardware decoder that outputs DRM_PRIME", which is what makes a NEW SoC work
/// without Pulsar changes (Moonlight's format-not-device lesson).
unsafe fn candidates(codec_id: ff::AVCodecID) -> Vec<Candidate> {
	let mut out = Vec::new();

	// Tier 0: AV_CODEC_CAP_HARDWARE decoders with DRM_PRIME output.
	for dec in zero_copy_decoders(codec_id) {
		out.push(Candidate {
			dec,
			name: decoder_name(dec),
			tier: Tier::ZeroCopyHw,
			hwdev: None,
			hw_fmt: ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME,
			node: None,
			zero_copy: true,
		});
	}

	// Tier 1: the stock decoder's hwaccel configs, display GPU first.
	//
	// Order (each entry still has to decode the canned keyframe to win):
	//   1. VAAPI on the DISPLAY GPU's node — dmabuf straight into this GL, no copy. Covers
	//      a lone AMD/Intel iGPU or dGPU, nouveau, the iGPU of a hybrid laptop, and an
	//      NVIDIA display with nvidia-vaapi-driver installed.
	//   2. CUDA — zero-copy (CUDA↔GL interop) when the display GPU is NVIDIA, otherwise a
	//      readback; NVDEC is at least as capable as an iGPU's VAAPI block.
	//   3. VAAPI on any OTHER node (another GPU → readback).
	//   4. Vulkan, DRM.
	// With the display GPU unknown this is the old CUDA → VAAPI(any) → … order, all readback.
	let stock = ff::avcodec_find_decoder(codec_id);
	if !stock.is_null() {
		use ff::AVHWDeviceType::*;
		// A SoC zero-copy decoder (tier 0, e.g. rkmpp on the Orange Pi 5) already won the
		// order: keep its hwaccel fallbacks in the old order instead of opening a second EGL
		// display on the fragile Mali stack just to rank them.
		static UNKNOWN: crate::gpu::DisplayGpu = crate::gpu::DisplayGpu {
			render_node: None,
			vendor: None,
		};
		let disp = if out.is_empty() {
			crate::gpu::display_gpu()
		} else {
			&UNKNOWN
		};
		// `PULSAR_ZEROCOPY=0`: same decoder order, but every frame takes the readback path
		// (A/B against zero-copy, or a driver whose dmabuf/interop import misrenders).
		let zc_allowed = std::env::var("PULSAR_ZEROCOPY").map_or(true, |v| v != "0");
		let has = |want: ff::AVHWDeviceType| {
			let mut i = 0;
			loop {
				let cfg = ff::avcodec_get_hw_config(stock, i);
				if cfg.is_null() {
					return false;
				}
				if (*cfg).device_type == want
					&& (*cfg).methods & (ff::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as c_int) != 0
				{
					return true;
				}
				i += 1;
			}
		};
		let mut push = |want: ff::AVHWDeviceType, node: Option<String>, zero_copy: bool| {
			if has(want) {
				out.push(Candidate {
					dec: stock,
					name: format!("{}+{}", decoder_name(stock), hwdev_name(want)),
					tier: Tier::HwAccel,
					hwdev: Some(want),
					hw_fmt: hw_fmt_for(want),
					node,
					zero_copy: zero_copy && zc_allowed,
				});
			}
		};
		if let Some(node) = &disp.render_node {
			push(AV_HWDEVICE_TYPE_VAAPI, Some(node.clone()), true);
		}
		push(AV_HWDEVICE_TYPE_CUDA, None, disp.is_nvidia());
		push(AV_HWDEVICE_TYPE_VAAPI, None, false);
		push(AV_HWDEVICE_TYPE_VULKAN, None, false);
		push(AV_HWDEVICE_TYPE_DRM, None, false);
		// Tier 2: software (the stock decoder without hwaccel) — always last, always there.
		if (*stock).capabilities & (ff::AV_CODEC_CAP_HARDWARE as c_int) == 0 {
			out.push(Candidate {
				dec: stock,
				name: decoder_name(stock),
				tier: Tier::Software,
				hwdev: None,
				hw_fmt: ff::AVPixelFormat::AV_PIX_FMT_NONE,
				node: None,
				zero_copy: false,
			});
		}
	}

	out
}

fn hwdev_name(dev: ff::AVHWDeviceType) -> &'static str {
	use ff::AVHWDeviceType::*;
	match dev {
		AV_HWDEVICE_TYPE_VAAPI => "vaapi",
		AV_HWDEVICE_TYPE_CUDA => "cuda",
		AV_HWDEVICE_TYPE_VULKAN => "vulkan",
		AV_HWDEVICE_TYPE_DRM => "drm",
		_ => "hw",
	}
}

/// Create a hwaccel device context for `dev`, on `node` when one is given (VAAPI on the
/// display GPU). Without a node, VAAPI's default device is `/dev/dri/renderD128`, which on
/// an Optimus/hybrid laptop is the NVIDIA node — it has no VA driver, so the default fails
/// while the AMD iGPU's `radeonsi` VA driver lives on a higher-numbered render node that
/// libva never tries. So for VAAPI without a node: try the default, then EACH
/// `/dev/dri/renderD*` node except the display GPU's (that one has its own candidate), and
/// use the first that initialises. Other device types (CUDA/Vulkan/DRM) use their own
/// default. Returns null when no device could be created.
pub(crate) unsafe fn create_hwdevice(
	dev: ff::AVHWDeviceType,
	node: Option<&str>,
) -> *mut ff::AVBufferRef {
	use ff::AVHWDeviceType::*;
	let open = |path: Option<&str>| -> *mut ff::AVBufferRef {
		let c = path.and_then(|p| CString::new(p).ok());
		let p = c.as_ref().map_or(ptr::null(), |c| c.as_ptr());
		let mut hwctx: *mut ff::AVBufferRef = ptr::null_mut();
		if ff::av_hwdevice_ctx_create(&mut hwctx, dev, p, ptr::null_mut(), 0) >= 0 {
			hwctx
		} else {
			ptr::null_mut()
		}
	};
	if node.is_some() {
		return open(node);
	}
	if dev != AV_HWDEVICE_TYPE_VAAPI {
		return open(None);
	}
	// Only skip the display GPU's node when the walk ranked it (decode.rs candidates).
	let display = crate::gpu::known().and_then(|g| g.render_node.as_deref());
	// libva's default node — unless that IS the display GPU's (it has its own candidate).
	if display != Some("/dev/dri/renderD128") {
		let h = open(None);
		if !h.is_null() {
			return h;
		}
	}
	// VAAPI default node had no driver — walk the render nodes and use the first that works.
	if let Ok(rd) = std::fs::read_dir("/dev/dri") {
		let mut nodes: Vec<std::path::PathBuf> = rd
			.filter_map(|e| e.ok().map(|e| e.path()))
			.filter(|p| {
				p.file_name()
					.and_then(|n| n.to_str())
					.is_some_and(|n| n.starts_with("renderD"))
			})
			.collect();
		nodes.sort();
		for node in nodes {
			let path = node.to_string_lossy();
			if Some(path.as_ref()) == display {
				continue;
			}
			let h = open(Some(&path));
			if !h.is_null() {
				eprintln!("pulsar-render: VAAPI device {}", node.display());
				return h;
			}
		}
	}
	ptr::null_mut()
}

/// The hw format the validate/get_format callback should accept for the candidate
/// being tried (callbacks can't capture, single-threaded selection → a global is fine).
static WANT_HW_FMT: AtomicI32 = AtomicI32::new(ff::AVPixelFormat::AV_PIX_FMT_NONE as i32);

pub(crate) fn set_wanted_hw_fmt(fmt: ff::AVPixelFormat) {
	WANT_HW_FMT.store(fmt as i32, Ordering::SeqCst);
}

/// Shared get_format: DRM_PRIME (tier 0) → the candidate's hwaccel format (tier 1,
/// only when a device ctx is armed) → a displayable software format. NEVER an
/// unrequested hwaccel entry (that's the "vaapi 'succeeds' → opaque frames → black
/// screen" bug class).
pub(crate) unsafe extern "C" fn get_format(
	_c: *mut ff::AVCodecContext,
	fmts: *const ff::AVPixelFormat,
) -> ff::AVPixelFormat {
	let want_hw = WANT_HW_FMT.load(Ordering::SeqCst);
	let mut p = fmts;
	while *p != ff::AVPixelFormat::AV_PIX_FMT_NONE {
		if *p == ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME {
			return *p;
		}
		p = p.add(1);
	}
	if want_hw != ff::AVPixelFormat::AV_PIX_FMT_NONE as i32 {
		let mut p = fmts;
		while *p != ff::AVPixelFormat::AV_PIX_FMT_NONE {
			if *p as i32 == want_hw {
				return *p;
			}
			p = p.add(1);
		}
	}
	let mut p = fmts;
	while *p != ff::AVPixelFormat::AV_PIX_FMT_NONE {
		if is_displayable_sw(*p as c_int) {
			return *p;
		}
		p = p.add(1);
	}
	// Last resort: any NON-hwaccel entry (e.g. yuv420p10le for a 10-bit stream — the
	// caller skips non-displayable sw frames instead of erroring). Returning *fmts
	// blind could hand back a hwaccel format with no device ctx armed → ff_get_format
	// rejects it → every frame errors (the bug class described above).
	let mut p = fmts;
	let mut sw = ff::AVPixelFormat::AV_PIX_FMT_NONE;
	while *p != ff::AVPixelFormat::AV_PIX_FMT_NONE {
		let d = ff::av_pix_fmt_desc_get(*p);
		if !d.is_null() && (*d).flags & (ff::AV_PIX_FMT_FLAG_HWACCEL as u64) == 0 {
			sw = *p; // keep the LAST sw entry (the decoder's native format)
		}
		p = p.add(1);
	}
	if sw != ff::AVPixelFormat::AV_PIX_FMT_NONE {
		return sw;
	}
	*fmts
}

/// All probe fixtures for `codec_id`: always the baseline (conformant) sample, plus any
/// committed encoder-family real-world samples (skipped when their file is empty). Empty
/// for codecs we have no fixture for at all.
fn fixtures_for(codec_id: ff::AVCodecID) -> Vec<Fixture> {
	let (baseline, ext, nvenc) = match codec_id {
		ff::AVCodecID::AV_CODEC_ID_H264 => (TEST_H264, "h264", TEST_NVENC_H264),
		ff::AVCodecID::AV_CODEC_ID_HEVC => (TEST_H265, "h265", TEST_NVENC_H265),
		ff::AVCodecID::AV_CODEC_ID_AV1 => (TEST_AV1, "av1.ivf", TEST_NVENC_AV1),
		_ => return Vec::new(),
	};
	let mut out = vec![Fixture { family: "", bytes: baseline, ext }];
	// Family-specific samples (only when a non-empty fixture is committed).
	if !nvenc.is_empty() {
		out.push(Fixture { family: "nvenc", bytes: nvenc, ext });
	}
	out
}

/// Write a fixture's bitstream to a uniquely-named temp file (avformat wants a path) and
/// return it. The name encodes codec+family so concurrent probes never collide.
fn write_fixture(fix: &Fixture) -> Option<std::path::PathBuf> {
	let tag = if fix.family.is_empty() { "base" } else { fix.family };
	let path = std::env::temp_dir().join(format!("pulsar-probe-{tag}.{}", fix.ext));
	std::fs::write(&path, fix.bytes).ok()?;
	Some(path)
}

/// Really decode the canned keyframe with this candidate. True ⇒ ≥1 frame came out
/// in an output class the presenter can actually show for the tier.
unsafe fn validate(cand: &Candidate, fixture: &std::path::Path) -> bool {
	let cpath = match CString::new(fixture.to_string_lossy().as_bytes()) {
		Ok(c) => c,
		Err(_) => return false,
	};
	let mut fmt: *mut ff::AVFormatContext = ptr::null_mut();
	if ff::avformat_open_input(&mut fmt, cpath.as_ptr(), ptr::null_mut(), ptr::null_mut()) < 0 {
		return false;
	}
	let mut ok = false;
	'done: {
		if ff::avformat_find_stream_info(fmt, ptr::null_mut()) < 0 {
			break 'done;
		}
		let vs = ff::av_find_best_stream(
			fmt,
			ff::AVMediaType::AVMEDIA_TYPE_VIDEO,
			-1,
			-1,
			ptr::null_mut(),
			0,
		);
		if vs < 0 {
			break 'done;
		}
		let st = *(*fmt).streams.add(vs as usize);

		let dc = ff::avcodec_alloc_context3(cand.dec);
		if dc.is_null() {
			break 'done;
		}
		ff::avcodec_parameters_to_context(dc, (*st).codecpar);
		set_wanted_hw_fmt(if cand.tier == Tier::HwAccel {
			cand.hw_fmt
		} else {
			ff::AVPixelFormat::AV_PIX_FMT_NONE
		});
		(*dc).get_format = Some(get_format);
		let mut hwdev_ok = true;
		if let Some(dev) = cand.hwdev {
			let hwctx = create_hwdevice(dev, cand.node.as_deref());
			if hwctx.is_null() {
				hwdev_ok = false;
			} else {
				(*dc).hw_device_ctx = hwctx;
			}
		}
		let mut dc_owned = dc;
		if hwdev_ok && ff::avcodec_open2(dc, cand.dec, ptr::null_mut()) >= 0 {
			let pkt = ff::av_packet_alloc();
			let frame = ff::av_frame_alloc();
			// Push every fixture packet, then flush; some HW decoders only emit
			// after the flush (single-keyframe input).
			while ff::av_read_frame(fmt, pkt) >= 0 {
				if (*pkt).stream_index == vs {
					let _ = ff::avcodec_send_packet(dc, pkt);
				}
				ff::av_packet_unref(pkt);
			}
			let _ = ff::avcodec_send_packet(dc, ptr::null()); // flush
													 // Drain with a few retries (async HW decoders return EAGAIN briefly).
			for _ in 0..50 {
				let r = ff::avcodec_receive_frame(dc, frame);
				if r == 0 {
					let f = (*frame).format;
					ok = match cand.tier {
						Tier::ZeroCopyHw => f == ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as c_int,
						Tier::HwAccel => f == cand.hw_fmt as c_int || is_displayable_sw(f),
						Tier::Software => is_displayable_sw(f),
					};
					ff::av_frame_unref(frame);
					if ok {
						break;
					}
				} else if r == ff::AVERROR(libc::EAGAIN) {
					std::thread::sleep(std::time::Duration::from_millis(10));
				} else {
					break;
				}
			}
			let mut pkt = pkt;
			let mut frame = frame;
			ff::av_packet_free(&mut pkt);
			ff::av_frame_free(&mut frame);
		}
		ff::avcodec_free_context(&mut dc_owned);
	}
	ff::avformat_close_input(&mut fmt);
	ok
}

/// The outcome of selecting (and family-probing) a decoder for one codec.
pub struct Probed {
	pub sel: Selected,
	/// Encoder families whose REAL bitstream this decoder could NOT decode, even though
	/// the baseline fixture decoded fine (e.g. `["nvenc"]` on the Pi's rkmpp HEVC). The
	/// negotiator uses this to avoid a host-encoder × client-decoder combo that is known
	/// to produce a dead stream, falling back to h264 instead.
	pub incompatible_with: Vec<String>,
}

/// Pick the first candidate that REALLY decodes the BASELINE keyframe (tier order =
/// zero-copy SoC → hwaccel → software), then probe that SAME decoder against every
/// committed encoder-family sample to learn which real-world bitstreams it rejects.
pub fn select(codec_id: ff::AVCodecID) -> Option<Probed> {
	select_skipping(codec_id, &[])
}

/// [`select`] with candidates excluded by name — the runtime zero-output fallback in
/// video.rs walks the same tier chain again with the decoder that just failed on the
/// live stream (and every one before it) left out.
pub fn select_skipping(codec_id: ff::AVCodecID, skip: &[String]) -> Option<Probed> {
	let fixtures = fixtures_for(codec_id);
	let baseline = fixtures.iter().find(|f| f.family.is_empty())?;
	let baseline_path = write_fixture(baseline)?;
	unsafe {
		for cand in candidates(codec_id) {
			if skip.iter().any(|s| *s == key_of(&cand.name, cand.node.as_deref())) {
				continue;
			}
			if !validate(&cand, &baseline_path) {
				eprintln!(
					"pulsar-render: decoder {} failed validation",
					key_of(&cand.name, cand.node.as_deref())
				);
				continue;
			}
			eprintln!(
				"pulsar-render: selected decoder {} ({}{}{})",
				cand.name,
				cand.tier.as_str(),
				cand.node.as_deref().map_or(String::new(), |n| format!(", {n}")),
				if cand.zero_copy { ", zero-copy" } else { "" }
			);
			// The winner decodes conformant input — now check the real encoder families.
			let mut incompatible_with = Vec::new();
			for fix in fixtures.iter().filter(|f| !f.family.is_empty()) {
				let Some(p) = write_fixture(fix) else { continue };
				if !validate(&cand, &p) {
					eprintln!(
						"pulsar-render: decoder {} rejects {} bitstream",
						cand.name, fix.family
					);
					incompatible_with.push(fix.family.to_string());
				}
			}
			return Some(Probed {
				sel: Selected {
					name: cand.name,
					tier: cand.tier,
					hwdev: cand.hwdev,
					hw_fmt: cand.hw_fmt,
					node: cand.node,
					zero_copy: cand.zero_copy,
				},
				incompatible_with,
			});
		}
	}
	None
}

/// The decoder for a **4:4:4** stream of `codec_id`: libavcodec in software, validated on the
/// canned 4:4:4 keyframe. Its yuv444p goes through the planar GL upload (`video.rs`) like any
/// software frame. The hardware tiers are left out on purpose: VAAPI hands 4:4:4 over as
/// packed VUYX/XV30 surfaces the dmabuf import and the NV12 readback cannot show, and the
/// CUDA↔GL interop copies NV12 only — a 4:4:4 stream there was a black screen until the
/// zero-output fallback walked down to software. `None`: no 4:4:4 fixture for this codec (AV1)
/// or no software decoder in this libavcodec. Only H.264 / HEVC.
///
/// This validates (writes the fixture, decodes it); the decode thread uses the per-process
/// cache [`yuv444_decoder`] instead.
pub fn select_yuv444(codec_id: ff::AVCodecID) -> Option<Probed> {
	let path = fixture_444(codec_id)?;
	unsafe {
		for cand in candidates(codec_id).into_iter().filter(|c| c.tier == Tier::Software) {
			if !validate(&cand, &path) {
				eprintln!("pulsar-render: decoder {} failed the 4:4:4 validation", cand.name);
				continue;
			}
			return Some(Probed {
				sel: Selected {
					name: cand.name,
					tier: cand.tier,
					hwdev: cand.hwdev,
					hw_fmt: cand.hw_fmt,
					node: cand.node,
					zero_copy: cand.zero_copy,
				},
				incompatible_with: Vec::new(),
			});
		}
	}
	None
}

/// The canned 4:4:4 keyframe of `codec_id` written to its temp file (H.264 / HEVC only).
fn fixture_444(codec_id: ff::AVCodecID) -> Option<std::path::PathBuf> {
	let (bytes, ext) = match codec_id {
		ff::AVCodecID::AV_CODEC_ID_H264 => (TEST_444_H264, "h264"),
		ff::AVCodecID::AV_CODEC_ID_HEVC => (TEST_444_H265, "h265"),
		_ => return None,
	};
	write_fixture(&Fixture { family: "444", bytes, ext })
}

/// The software 4:4:4 decoder for `codec_id` ([`select_yuv444`]), validated ONCE per process
/// and cached. The decode thread asks at a 4:4:4 keyframe: re-validating there (temp file +
/// avformat open + a full decode) delayed the session's first picture, and again after every
/// GL-context-loss reopen. `video.rs` warms it before the stream's first keyframe when 4:4:4
/// is wanted (`PULSAR_YUV444`) — on the decode thread itself, never concurrently with a
/// decoder open (the validation sets the process-wide wanted hw format).
pub fn yuv444_decoder(codec_id: ff::AVCodecID) -> Option<Selected> {
	static H264: std::sync::OnceLock<Option<Selected>> = std::sync::OnceLock::new();
	static HEVC: std::sync::OnceLock<Option<Selected>> = std::sync::OnceLock::new();
	let cell = match codec_id {
		ff::AVCodecID::AV_CODEC_ID_H264 => &H264,
		ff::AVCodecID::AV_CODEC_ID_HEVC => &HEVC,
		_ => return None,
	};
	cell.get_or_init(|| select_yuv444(codec_id).map(|p| p.sel)).clone()
}

/// Headroom between the measured single-thread 4:4:4 decode speed and the pixel rate the app
/// may ask a host for: the frame budget must also hold the three-plane GL upload and stream
/// frames that cost more than the fixture's.
const YUV444_DECODE_MARGIN: f64 = 2.0;

/// Below this usable pixel rate (720p30) 4:4:4 is not offered at all: such a CPU keeps no
/// session we stream in real time (e.g. an ARM board whose 4:2:0 runs on its rkmpp block).
const YUV444_MIN_PX_S: u64 = 1280 * 720 * 30;

/// Software decode speed of `sel` on the canned 4:4:4 keyframe at `fixture`, in pixels/s, on
/// ONE thread — what a real stream gets: `open_decoder` threads by slice only, and NVENC writes
/// one slice per picture. An intra picture at ~0.6 bit/pixel is a pessimistic stand-in for a
/// stream's average frame. Decodes for ~150 ms after a short warm-up. `None`: nothing decoded.
unsafe fn sw_decode_px_per_s(sel: &Selected, codec_id: ff::AVCodecID, fixture: &std::path::Path) -> Option<f64> {
	let cpath = CString::new(fixture.to_string_lossy().as_bytes()).ok()?;
	let mut fmt: *mut ff::AVFormatContext = ptr::null_mut();
	if ff::avformat_open_input(&mut fmt, cpath.as_ptr(), ptr::null_mut(), ptr::null_mut()) < 0 {
		return None;
	}
	let mut rate = None;
	'done: {
		if ff::avformat_find_stream_info(fmt, ptr::null_mut()) < 0 {
			break 'done;
		}
		let vs = ff::av_find_best_stream(fmt, ff::AVMediaType::AVMEDIA_TYPE_VIDEO, -1, -1, ptr::null_mut(), 0);
		if vs < 0 {
			break 'done;
		}
		let st = *(*fmt).streams.add(vs as usize);
		let dec = find_decoder(sel, codec_id);
		if dec.is_null() {
			break 'done;
		}
		let mut dc = ff::avcodec_alloc_context3(dec);
		if dc.is_null() {
			break 'done;
		}
		ff::avcodec_parameters_to_context(dc, (*st).codecpar);
		(*dc).flags |= ff::AV_CODEC_FLAG_LOW_DELAY as c_int;
		(*dc).thread_count = 1;
		let mut pkt = ff::av_packet_alloc();
		let mut key = ff::av_packet_alloc();
		let mut frame = ff::av_frame_alloc();
		if !pkt.is_null() && !key.is_null() && !frame.is_null() && ff::avcodec_open2(dc, dec, ptr::null_mut()) >= 0 {
			// The fixture's first video packet: its keyframe (parameter sets in-band).
			let mut have = false;
			while ff::av_read_frame(fmt, pkt) >= 0 {
				if (*pkt).stream_index == vs && !have {
					have = ff::av_packet_ref(key, pkt) >= 0;
				}
				ff::av_packet_unref(pkt);
			}
			if have {
				const WARMUP: u32 = 3;
				let mut t0: Option<std::time::Instant> = None;
				let mut px = 0f64;
				for i in 0..2000u32 {
					if i == WARMUP {
						t0 = Some(std::time::Instant::now());
						px = 0.0;
					}
					if ff::avcodec_send_packet(dc, key) < 0 {
						break;
					}
					while ff::avcodec_receive_frame(dc, frame) == 0 {
						px += f64::from((*frame).width.max(0)) * f64::from((*frame).height.max(0));
						ff::av_frame_unref(frame);
					}
					if t0.is_some_and(|t| t.elapsed() >= std::time::Duration::from_millis(150)) {
						break;
					}
				}
				if let Some(t) = t0 {
					let s = t.elapsed().as_secs_f64();
					if px > 0.0 && s > 0.0 {
						rate = Some(px / s);
					}
				}
			}
		}
		ff::av_packet_free(&mut pkt);
		ff::av_packet_free(&mut key);
		ff::av_frame_free(&mut frame);
		ff::avcodec_free_context(&mut dc);
	}
	ff::avformat_close_input(&mut fmt);
	rate
}

/// `--probe-444` (main.rs): the codecs a 4:4:4 stream decodes AND presents in here, as
/// `[{"codec":"h265","max_px_s":N}]`. Only libavcodec in software shows 4:4:4 on Linux
/// ([`select_yuv444`]), on one thread, so each entry carries the pixel rate (w×h×fps) it keeps
/// up with — measured, divided by [`YUV444_DECODE_MARGIN`] — and the app asks for 4:4:4 only
/// for a session within it; a CPU under [`YUV444_MIN_PX_S`] gets no entry (in practice the
/// RK3588 boards, whose 4:2:0 runs on rkmpp). Separate from `--probe`: the app runs it only
/// when 4:4:4 is wanted (`PULSAR_YUV444`), under its own timeout.
pub fn probe_444_json() -> String {
	let mut entries = Vec::new();
	for (label, id) in [("h264", ff::AVCodecID::AV_CODEC_ID_H264), ("h265", ff::AVCodecID::AV_CODEC_ID_HEVC)] {
		let Some(p) = select_yuv444(id) else { continue };
		let Some(path) = fixture_444(id) else { continue };
		let measured = unsafe { sw_decode_px_per_s(&p.sel, id, &path) }.unwrap_or(0.0);
		let max_px_s = (measured / YUV444_DECODE_MARGIN) as u64;
		eprintln!(
			"pulsar-render: 4:4:4 {label} in software ({}): {:.0} Mpx/s on one thread, offered up to {:.0} Mpx/s",
			p.sel.name,
			measured / 1e6,
			max_px_s as f64 / 1e6
		);
		if max_px_s < YUV444_MIN_PX_S {
			continue;
		}
		entries.push(format!(r#"{{"codec":"{label}","max_px_s":{max_px_s}}}"#));
	}
	format!("[{}]", entries.join(","))
}

/// Resolve a Selected back to its AVCodec for opening the real stream context.
/// (Pointers aren't carried in `Selected` so it stays Send/Sync-trivial.)
pub(crate) unsafe fn find_decoder(sel: &Selected, codec_id: ff::AVCodecID) -> *const ff::AVCodec {
	match sel.tier {
		Tier::ZeroCopyHw => {
			// Re-find by name (tier-0 candidates are specific named decoders).
			let base = sel.name.split('+').next().unwrap_or(&sel.name);
			let c = CString::new(base).unwrap();
			let dec = ff::avcodec_find_decoder_by_name(c.as_ptr());
			if dec.is_null() {
				ff::avcodec_find_decoder(codec_id)
			} else {
				dec
			}
		}
		_ => ff::avcodec_find_decoder(codec_id),
	}
}

/// `--probe`: run the chain per codec, headless, print one JSON array on stdout.
/// Consumed by the app's startup capability detection.
pub fn probe_json() -> String {
	let mut entries = Vec::new();
	for (label, id) in [
		("h264", ff::AVCodecID::AV_CODEC_ID_H264),
		("h265", ff::AVCodecID::AV_CODEC_ID_HEVC),
		("av1", ff::AVCodecID::AV_CODEC_ID_AV1),
	] {
		match select(id) {
			Some(p) => {
				// Back-compat: `incompatible_with` is an additive array (older consumers
				// ignore it; empty = no known-bad encoder family for this decoder).
				// `zero_copy` is additive too: the decoder sits on the display GPU, so frames
				// should reach GL without a system-memory copy (still verified at runtime).
				let incompat = p
					.incompatible_with
					.iter()
					.map(|f| format!(r#""{f}""#))
					.collect::<Vec<_>>()
					.join(",");
				entries.push(format!(
					r#"{{"codec":"{label}","ok":true,"decoder":"{}","tier":"{}","hw":{},"zero_copy":{},"incompatible_with":[{}],"ten_bit":{}}}"#,
					p.sel.name,
					p.sel.tier.as_str(),
					p.sel.tier != Tier::Software,
					p.sel.zero_copy,
					incompat,
					// HEVC Main10 / AV1 10-bit (a host's HDR10 stream): NOT advertised on Linux,
					// so the client never asks for HDR. Every present path here would make the
					// picture worse than the SDR stream, with no HDR benefit (the output is an
					// 8-bit SDR GL surface): software decode and every readback (NVDEC's CUDA
					// interop is NV12-only) go through a CPU 10 → 8-bit shift (ms per frame at
					// 1080p, and 8-bit PQ bands worse than 8-bit sRGB); rkmpp / v4l2m2m hand
					// 10-bit out as NV15 / AFBC with no import path, and the Orange Pi's KMS
					// plane has no PQ handling. Only VAAPI's two-layer R16 + GR1616 dmabuf keeps
					// 10 bits up to the tone-map, and that is unverified on hardware. The
					// decode + tone-map code stays for a stream that is HDR anyway (PULSAR_HDR
					// override, an old host). PULSAR_LINUX_TEN_BIT=1 advertises it for a hardware
					// test of that path. See docs/agents/client-video.md.
					label != "h264" && std::env::var("PULSAR_LINUX_TEN_BIT").is_ok_and(|v| v == "1")
				));
			}
			None => entries.push(format!(r#"{{"codec":"{label}","ok":false}}"#)),
		}
	}
	format!("[{}]", entries.join(","))
}
