//! The NVENC session pieces that need no graphics API: the status check, the half-built
//! session guard, the RTP destination parser, and `configure` — preset baseline, the
//! low-latency CBR config, the RFI / intra-refresh choices and `nvEncInitializeEncoder`.
//! Shared by the Windows D3D11 encoder (`new.rs`) and the Linux CUDA encoder (`cuda_enc.rs`),
//! so both stream with the same encoder settings.

use std::ffi::c_void;
use std::net::{SocketAddr, ToSocketAddrs};
use std::ptr;

use super::{cap_dbg, nvenc, policy};
use crate::Codec;

/// RAII guard that destroys a half-built NVENC session if `new()` bails between
/// `nvEncOpenEncodeSessionEx` and the final `Ok`. Holds a copy of the destroy fn
/// pointer (not a borrow of the fn table) so the table can be moved into the
/// returned `Encoder` once `disarm()` is called.
pub(super) struct SessionGuard {
	pub(super) destroy: Option<nvenc::PFN_DestroyEncoder>,
	pub(super) enc: *mut c_void,
}
impl SessionGuard {
	pub(super) fn disarm(&mut self) {
		self.enc = ptr::null_mut();
	}
}
impl Drop for SessionGuard {
	fn drop(&mut self) {
		if !self.enc.is_null() {
			if let Some(f) = self.destroy {
				unsafe {
					let _ = f(self.enc);
				}
			}
		}
	}
}

/// Parse an `"rtp://host:port"` (or bare `"host:port"`) destination into a
/// `SocketAddr`, resolving the host via `to_socket_addrs()`.
pub(super) fn parse_rtp_dest(dest: &str) -> Result<SocketAddr, String> {
	let hostport = dest.strip_prefix("rtp://").unwrap_or(dest);
	// Strip any trailing path/query the URL form might carry (rtp URLs normally don't).
	let hostport = hostport.split('/').next().unwrap_or(hostport);
	hostport
		.to_socket_addrs()
		.map_err(|e| format!("rtp dest resolve '{hostport}': {e}"))?
		.next()
		.ok_or_else(|| format!("rtp dest '{hostport}' resolved to no address"))
}

/// Map a non-success NVENC status to an error string, appending the driver's last
/// error message when available.
pub(super) unsafe fn chk(
	s: nvenc::NVENCSTATUS,
	fns: &nvenc::NV_ENCODE_API_FUNCTION_LIST,
	enc: *mut c_void,
) -> Result<(), String> {
	if s == nvenc::NV_ENC_SUCCESS {
		return Ok(());
	}
	// nvEncGetLastErrorString(enc) → const char*; only meaningful once a session exists.
	let mut msg = String::new();
	if !enc.is_null() {
		if let Some(f) = fns.nvEncGetLastErrorString {
			let p = f(enc);
			if !p.is_null() {
				let cstr = std::ffi::CStr::from_ptr(p);
				msg = cstr.to_string_lossy().into_owned();
			}
		}
	}
	if msg.is_empty() {
		Err(format!("nvenc status {s}"))
	} else {
		Err(format!("nvenc status {s} ({msg})"))
	}
}

/// What `configure` needs to know of the stream (the encode size, after any rotation).
pub(super) struct SessionParams {
	pub(super) codec: Codec,
	pub(super) width: u32,
	pub(super) height: u32,
	pub(super) fps: u32,
	pub(super) bitrate_kbps: u32,
	pub(super) low_latency: bool,
	pub(super) intra_refresh: bool,
	pub(super) rfi: bool,
}

/// An initialised encoder's retained config and the choices `configure` made.
pub(super) struct Configured {
	/// Retained for `nvEncReconfigureEncoder`; `init_params.encodeConfig` points into
	/// `enc_config` (both boxed: stable heap addresses).
	pub(super) init_params: Box<nvenc::NV_ENC_INITIALIZE_PARAMS>,
	pub(super) enc_config: Box<nvenc::NV_ENC_CONFIG>,
	/// RFI is on (asked for, not intra refresh, not AV1, and the driver's caps allow it).
	pub(super) rfi: bool,
	pub(super) intra_refresh: bool,
	/// Reference-buffer depth (`policy::rfi_dpb` with RFI, else 1).
	pub(super) dpb: u32,
	/// Safety GOP in frames.
	pub(super) gop: u32,
	pub(super) low_budget: bool,
	pub(super) thin_budget: bool,
	pub(super) two_pass: bool,
	pub(super) preset_kf_scale: u8,
}

/// Build the stream's NVENC config on the open session `enc` and initialise the encoder.
/// Everything here is the same for every input resource type (D3D11 texture, CUDA memory).
pub(super) unsafe fn configure(
	fns: &nvenc::NV_ENCODE_API_FUNCTION_LIST,
	enc: *mut c_void,
	p: &SessionParams,
) -> Result<Configured, String> {
	let (w, h, fps) = (p.width, p.height, p.fps.max(1));
	// -- 6. Get the preset config as a baseline. `low_latency` picks the tradeoff,
	//    mirroring the old ffmpeg path: ULTRA_LOW_LATENCY tuning for the gaming profile,
	//    LOW_LATENCY for the quality-leaning one; the preset level comes from
	//    `policy::preset_level` (game P1; remote P4 while that is cheap, a faster one at
	//    high pixel rates). The presets above P4 are not offered (too slow for a live
	//    stream): a forced 5-7 runs as P4. ------
	let preset_n = policy::preset_level(
		p.low_latency,
		w,
		h,
		fps,
		std::env::var("PULSAR_NVENC_PRESET").ok().as_deref(),
	)
	.min(4);
	let preset_guid = match preset_n {
		1 => nvenc::NV_ENC_PRESET_P1_GUID,
		2 => nvenc::NV_ENC_PRESET_P2_GUID,
		3 => nvenc::NV_ENC_PRESET_P3_GUID,
		_ => nvenc::NV_ENC_PRESET_P4_GUID,
	};
	let tuning = if p.low_latency {
		nvenc::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY
	} else {
		nvenc::NV_ENC_TUNING_INFO_LOW_LATENCY
	};
	// Codec GUID + matching profile GUID for the requested codec. H.264 keeps its exact
	// previous values (byte-for-byte unchanged); HEVC/AV1 use their Main profile.
	let (codec_guid, profile_guid) = match p.codec {
		Codec::H264 => (
			nvenc::NV_ENC_CODEC_H264_GUID,
			nvenc::NV_ENC_H264_PROFILE_HIGH_GUID,
		),
		Codec::H265 => (
			nvenc::NV_ENC_CODEC_HEVC_GUID,
			nvenc::NV_ENC_HEVC_PROFILE_MAIN_GUID,
		),
		Codec::Av1 => (
			nvenc::NV_ENC_CODEC_AV1_GUID,
			nvenc::NV_ENC_AV1_PROFILE_MAIN_GUID,
		),
	};
	// RFI needs a driver that invalidates references and keeps several of them (the two caps
	// Sunshine checks too). A query the driver does not answer keeps the old behaviour
	// (assume support) — only an explicit 0 turns RFI off.
	let query_cap = |which: u32| -> Option<i32> {
		let f = fns.nvEncGetEncodeCaps?;
		let mut q: nvenc::NV_ENC_CAPS_PARAM = std::mem::zeroed();
		q.version = nvenc::NV_ENC_CAPS_PARAM_VER;
		q.capsToQuery = which;
		let mut v: i32 = 0;
		(f(enc, codec_guid, &mut q, &mut v) == nvenc::NV_ENC_SUCCESS).then_some(v)
	};
	let rfi_caps = query_cap(nvenc::NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION) != Some(0)
		&& query_cap(nvenc::NV_ENC_CAPS_SUPPORT_MULTIPLE_REF_FRAMES) != Some(0);

	let mut preset: nvenc::NV_ENC_PRESET_CONFIG = std::mem::zeroed();
	preset.version = nvenc::NV_ENC_PRESET_CONFIG_VER;
	preset.presetCfg.version = nvenc::NV_ENC_CONFIG_VER;
	let get_preset = fns
		.nvEncGetEncodePresetConfigEx
		.ok_or("nvenc: nvEncGetEncodePresetConfigEx missing")?;
	chk(
		get_preset(enc, codec_guid, preset_guid, tuning, &mut preset),
		&fns,
		enc,
	)
	.map_err(|e| format!("nvEncGetEncodePresetConfigEx: {e}"))?;

	// -- 7. Override the config for low-latency CBR H.264. Take the preset config
	//    as the baseline (so reserved RC bitfields stay valid — Sunshine's rule)
	//    and override ONLY named fields + specific flag bits. -----------------------
	let mut cfg: nvenc::NV_ENC_CONFIG = preset.presetCfg;
	cfg.version = nvenc::NV_ENC_CONFIG_VER;
	cfg.profileGUID = profile_guid;
	// Long GOP (Moonlight/Sunshine model: keyframes are rare — on connect + on request —
	// NOT a periodic tax). A short fps/4 (0.25 s) GOP at 1440p emitted a huge IDR 4×/s; each
	// big keyframe causes a send burst + an rkmpp decode spike on the Pi → ~150-240 ms
	// delivery stalls 4×/s → the cursor/typing "teleport" (changed regions freeze until the
	// next paced frame, static background unaffected). A 10 s safety GOP keeps late-join/
	// recovery bounded while making the periodic big-IDR latency spike rarer still. (env
	// PULSAR_IDR_SEC overrides.)
	// A8: chose the SAFE option (longer safety GOP, was 4 s) over switching to NVENC
	// intra-refresh. The client gates on a real IDR ("drop every packet until a keyframe
	// decodes"), and intra-refresh emits NO IDR in steady state (recovery is a rolling intra
	// wave the client's gate wouldn't recognize) — so intra-refresh risks breaking mid-stream
	// recovery. The connect IDR, the fast-window catch-up IDRs and the on-demand loss-recovery
	// IDR (submit.rs) are unchanged.
	// TODO(A8): revisit NVENC intra-refresh (enableIntraRefresh + intraRefreshPeriod=gop +
	// intraRefreshCnt≈fps) once the client can recover from a recovery-point SEI instead of
	// requiring a full IDR — it would remove the periodic keyframe spike entirely.
	let idr_sec = std::env::var("PULSAR_IDR_SEC")
		.ok()
		.and_then(|s| s.parse::<u32>().ok())
		.filter(|&v| v > 0)
		.unwrap_or(10);
	let gop = (fps * idr_sec).max(1);
	cfg.gopLength = gop;
	// Adaptive streaming: the client asked for a rolling intra refresh — NVENC requires
	// an infinite GOP for it (no scheduled IDRs; the wave refreshes the picture within
	// `intraRefreshPeriod` frames, spread over `intraRefreshCnt` frames). The on-demand
	// forced IDR (`request_idr`) still works, so a lost reference the wave cannot heal
	// in time is still recovered by the client's keyframe request.
	let intra_refresh = p.intra_refresh;
	// RFI needs references to fall back to; an intra-refreshed stream heals by itself.
	let rfi = p.rfi && !intra_refresh && p.codec != Codec::Av1 && rfi_caps;
	// As deep as the level allows at this size (16 at 1080p): the reach of an RFI request is
	// the buffer depth in frames, and at 120 fps over a mobile RTT the old fixed 8 was short.
	// Prediction still uses ONE reference (numRefL0 below), so encode cost is unchanged.
	let dpb = if rfi { policy::rfi_dpb(p.codec, w, h) } else { 1 };
	if intra_refresh {
		cfg.gopLength = nvenc::NVENC_INFINITE_GOPLENGTH;
	}
	let refresh_period = fps.max(1); // one full wave per second
	let refresh_cnt = (fps / 2).max(1); // spread over half a second
	cfg.frameIntervalP = 1; // 1 = no B-frames (IPPP)

	// Rate control: CBR at the requested bitrate, tiny VBV for low latency.
	let br = (p.bitrate_kbps as u32).saturating_mul(1000);
	cfg.rcParams.rateControlMode = nvenc::NV_ENC_PARAMS_RC_CBR;
	cfg.rcParams.averageBitRate = br;
	cfg.rcParams.maxBitRate = br;
	let vbv = br / fps; // ~1-frame VBV → emit each frame immediately
	cfg.rcParams.vbvBufferSize = vbv;
	cfg.rcParams.vbvInitialDelay = vbv;
	// B11: explicitly PIN the low-latency RC knobs instead of trusting the preset baseline.
	// multiPass = NV_ENC_MULTI_PASS_DISABLED (0): a multi-pass preset would add an extra
	// analysis pass per frame = latency. lookaheadDepth = 0: no frame lookahead (lookahead
	// buffers frames before emitting → latency). Both are public scalar fields, so we set
	// them directly. (AQ stays off via the ULTRA_LOW_LATENCY / LOW_LATENCY preset baseline —
	// those presets disable spatial/temporal AQ; the rcParams flags word only exposes an OR
	// setter, so we don't risk clearing the preset's reserved bits to force it.)
	cfg.rcParams.multiPass = nvenc::NV_ENC_MULTI_PASS_DISABLED;
	cfg.rcParams.lookaheadDepth = 0;
	// Low-budget stream (WAN / mobile — see `policy::LOW_BUDGET_BPP`): with a few KB per frame
	// the single-pass RC misjudges a complex frame, overshoots the one-frame VBV and starves
	// the next frames to pay it back. The quarter-resolution first pass (Sunshine's default,
	// within the same encode call — no lookahead, no frame of delay) measures the frame first;
	// and the keyframe target is pinned to 1× a P-frame budget (lowDelayKeyFrameScale, the
	// ULTRA_LOW_LATENCY default but 2 under LOW_LATENCY), so a keyframe takes the quality hit
	// instead of a multi-frame burst. A LAN-rate stream keeps exactly today's single pass and
	// preset scale (no added encode time). Chosen per build from the starting bitrate and
	// re-judged on a live bitrate change with hysteresis (`reconfigure_bitrate`), so a
	// cautious low start that climbs to a LAN rate leaves it. PULSAR_NVENC_MULTIPASS=0/1
	// overrides the pass choice.
	let low_budget = policy::low_budget(br, w, h, fps);
	let thin_budget = policy::thin_budget(br, w, h, fps);
	let two_pass = policy::use_two_pass(
		br,
		w,
		h,
		fps,
		std::env::var("PULSAR_NVENC_MULTIPASS").ok().as_deref(),
	);
	// The preset's own keyframe scale: what a stream that leaves the low-budget class live
	// goes back to.
	let preset_kf_scale = cfg.rcParams.lowDelayKeyFrameScale;
	if two_pass {
		cfg.rcParams.multiPass = nvenc::NV_ENC_TWO_PASS_QUARTER_RESOLUTION;
	}
	if low_budget {
		cfg.rcParams.lowDelayKeyFrameScale = 1;
	}
	// No minimum intra QP / keyframe size cap: at a starved budget the RC already runs the
	// keyframe at the top of the QP range, so a QP floor cannot shrink it (the one-frame VBV
	// is NVENC's only frame-size cap and is already set); on a LAN it would only blur the
	// first picture. A 1080p keyframe that cannot fit the budget is a point (resolution) the
	// ABR must not hold at that rate — the adaptive controller's job, not the encoder's.
	// RC flag bits (named, OR'd): zeroReorderDelay (no reorder), no lookahead, no AQ.
	// Bit positions are stable across NVENC versions (see nvenc consts).
	cfg.rcParams.set_flags(
		nvenc::NV_ENC_RC_FLAG_ZERO_REORDER_DELAY, // emit frames with no reorder delay
	);

	// Codec-specific config (in the encodeCodecConfig union). Branch on the codec;
	// each path sets up the same low-latency story as H.264: in-band parameter sets on
	// every IDR (so a late-joining RTP client always has them), a finite IDR period =
	// gop, single ref, 4:2:0 8-bit. The union member written MUST match the encodeGUID.
	match p.codec {
		Codec::H264 => {
			let h264 = &mut cfg.encodeCodecConfig.h264Config;
			// flag bit 12 = repeatSPSPPS: re-emit SPS/PPS in-band on each IDR so the RTP
			// client (no out-of-band SDP fmtp) always has parameter sets. Matches the CLI.
			h264.set_flags(1u32 << 12);
			h264.idrPeriod = gop; // IDR every gopLength frames (finite GOP)
			if intra_refresh {
				h264.set_flags(nvenc::NV_ENC_H264_FLAG_INTRA_REFRESH);
				h264.idrPeriod = nvenc::NVENC_INFINITE_GOPLENGTH;
				h264.intraRefreshPeriod = refresh_period;
				h264.intraRefreshCnt = refresh_cnt;
			}
			h264.set_entropyCodingMode(nvenc::NV_ENC_H264_ENTROPY_CODING_MODE_CABAC);
			h264.maxNumRefFrames = dpb;
			h264.set_numRefL0(nvenc::NV_ENC_NUM_REF_FRAMES_1);
			h264.set_chromaFormatIDC(1); // 4:2:0
		}
		Codec::H265 => {
			// NV_ENC_CONFIG_HEVC: repeatSPSPPS re-emits VPS/SPS/PPS in-band on each IDR
			// (the HEVC analog of H.264 repeatSPSPPS) so a mid-stream join has parameter
			// sets; chromaFormatIDC=1 → 4:2:0 8-bit (same NV12 input as H.264). Output is
			// Annex-B (00 00 00 01 start codes — identical to H.264; the RTP packetizer on
			// the client is its own Phase-2 concern). level/tier left AUTOSELECT (0).
			let hevc = &mut cfg.encodeCodecConfig.hevcConfig;
			hevc.set_flags(
				nvenc::NV_ENC_HEVC_FLAG_REPEAT_SPSPPS | nvenc::NV_ENC_HEVC_CHROMA_420,
			);
			hevc.idrPeriod = gop;
			hevc.maxNumRefFramesInDPB = dpb;
			if rfi {
				// Predict from ONE reference: the deep buffer is only the fallback an RFI
				// recovery frame reaches into (autoselect could search several = slower).
				hevc.numRefL0 = nvenc::NV_ENC_NUM_REF_FRAMES_1;
			}
			if intra_refresh {
				hevc.set_flags(nvenc::NV_ENC_HEVC_FLAG_INTRA_REFRESH);
				hevc.idrPeriod = nvenc::NVENC_INFINITE_GOPLENGTH;
				hevc.intraRefreshPeriod = refresh_period;
				hevc.intraRefreshCnt = refresh_cnt;
			}
		}
		Codec::Av1 => {
			// NV_ENC_CONFIG_AV1: repeatSeqHdr re-emits the sequence header OBU on each key
			// frame (late-join recovery); chromaFormatIDC=1 → 4:2:0 8-bit (NV12 input).
			// outputAnnexBFormat is left clear → NVENC emits the AV1 low-overhead OBU
			// stream (the client depacketizer handles OBUs). AV1 ENCODE requires Ada/
			// Ampere+ (RTX 40); on an unsupported GPU (e.g. RTX 3080) the config is valid
			// but nvEncInitializeEncoder below returns an Err → host falls back to ffmpeg.
			let av1 = &mut cfg.encodeCodecConfig.av1Config;
			av1.set_flags(nvenc::NV_ENC_AV1_FLAG_REPEAT_SEQ_HDR | nvenc::NV_ENC_AV1_CHROMA_420);
			av1.idrPeriod = gop;
			av1.maxNumRefFramesInDPB = 1;
			if intra_refresh {
				av1.set_flags(nvenc::NV_ENC_AV1_FLAG_INTRA_REFRESH);
				av1.idrPeriod = nvenc::NVENC_INFINITE_GOPLENGTH;
				av1.intraRefreshPeriod = refresh_period;
				av1.intraRefreshCnt = refresh_cnt;
			}
		}
	}

	// -- 8. Initialize the encoder. BOX `cfg` and `ip` so their heap addresses are STABLE:
	//    Stage-3 adaptive bitrate reuses these exact structs in nvEncReconfigureEncoder, and
	//    `ip.encodeConfig` is a self-referential pointer into the boxed cfg (a Box move keeps
	//    the heap address, so the pointer stays valid when the Encoder is moved to its thread).
	let mut enc_config: Box<nvenc::NV_ENC_CONFIG> = Box::new(cfg);
	let mut ip: Box<nvenc::NV_ENC_INITIALIZE_PARAMS> = Box::new(std::mem::zeroed());
	ip.version = nvenc::NV_ENC_INITIALIZE_PARAMS_VER;
	ip.encodeGUID = codec_guid;
	ip.presetGUID = preset_guid;
	ip.encodeWidth = w;
	ip.encodeHeight = h;
	ip.darWidth = w;
	ip.darHeight = h;
	ip.frameRateNum = fps;
	ip.frameRateDen = 1;
	ip.enableEncodeAsync = 0; // SYNC: lock the bitstream directly, no event.
	ip.set_enablePTD(1); // picture-type decision in the driver
	ip.tuningInfo = tuning;
	ip.encodeConfig = &mut *enc_config; // self-referential ptr into the boxed (stable) config
	let init_fn = fns
		.nvEncInitializeEncoder
		.ok_or("nvenc: nvEncInitializeEncoder missing")?;
	chk(init_fn(enc, &mut *ip), &fns, enc)
		.map_err(|e| format!("nvEncInitializeEncoder: {e}"))?;
	let init_params = ip; // retained on the Encoder for live bitrate reconfigure
	cap_dbg(&format!(
		"nvenc rc {w}x{h}@{fps} {} kbps: bpp={:.3} preset=P{preset_n} low_budget={low_budget} \
		 two_pass={two_pass} rfi={rfi} (caps {rfi_caps}) dpb={dpb}",
		br / 1000,
		policy::bits_per_pixel(br, w, h, fps),
	));

	Ok(Configured {
		init_params,
		enc_config,
		rfi,
		intra_refresh,
		dpb,
		gop,
		low_budget,
		thin_budget,
		two_pass,
		preset_kf_scale,
	})
}
