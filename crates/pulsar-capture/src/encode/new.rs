//! `Encoder::new` — build the whole native NVENC encode chain (fast path or the
//! cross-adapter HYBRID path). Split out of `encode.rs` unchanged. The struct itself
//! lives in `encoder.rs`; the free helpers it calls live in `d3d.rs`.

use std::ffi::c_void;
use std::ptr;

use windows::Win32::Graphics::Direct3D11::{
	ID3D11Device, ID3D11DeviceContext, ID3D11VideoContext, ID3D11VideoDevice,
};
use windows_core::Interface; // for `.cast::<T>()` and `.as_raw()`

use super::d3d::{
	build_video_processor, chk, create_nv12_texture, create_nvidia_device, device_vendor_id,
	parse_rtp_dest, CrossAdapterBridge, SessionGuard,
};
use super::encoder::Encoder;
use super::{cap_dbg, nvenc, policy, EncParams, VENDOR_NVIDIA};
use crate::rtp::RtpEgress;
use crate::Codec;

impl Encoder {
	/// Build the whole native NVENC encode chain. `device`/`ctx` are the CAPTURE device
	/// (the display owner — AMD iGPU on this laptop). We:
	///  - detect the capture adapter's vendor;
	///  - if it is NVIDIA, run the same-device fast path (NVENC + VideoProcessor + NV12
	///    on the capture device, no cross-adapter hop);
	///  - otherwise (AMD/Intel display owner) create a SEPARATE NVIDIA D3D11 device with
	///    `VIDEO_SUPPORT`, move the VideoProcessor + NV12 + NVENC session onto it, and
	///    build a shared keyed-mutex BGRA bridge from the capture device.
	///
	/// Then: open session → low-latency CBR H.264 config → register our NV12 texture →
	/// bitstream buffer, plus an `RtpEgress` (decoupled sender thread) to `p.dest`. Returns a
	/// human error string on ANY failure so the host can fall back to ffmpeg.
	pub unsafe fn new(
		device: &ID3D11Device,
		ctx: &ID3D11DeviceContext,
		p: &EncParams,
	) -> Result<Self, String> {
		// All three NVENC codecs (H.264 / HEVC / AV1) are implemented; the codec selects
		// the encode GUID + codec-specific config below. AV1 encode needs Ada/Ampere+
		// (RTX 40 for AV1 encode — an RTX 3080 is Ampere and does NOT support AV1 encode);
		// on an incapable GPU nvEncInitializeEncoder returns an error which we surface as
		// an Err so the host falls back to ffmpeg (no panic).
		// ENCODE/output size (NV12 needs even dims). For a 90°/270° host display the rotated
		// frame is portrait, so the encode surface swaps W↔H; 0°/180° keep dims. The BGRA→NV12
		// Blt (submit.rs) applies the actual rotation, mapping the native input → this output.
		let (pw, ph) = (p.width.max(2) & !1, p.height.max(2) & !1);
		let (w, h) = if p.rotation == 90 || p.rotation == 270 {
			(ph, pw)
		} else {
			(pw, ph)
		};
		let fps = p.fps.max(1);
		// Native CAPTURE size: the bridge + VideoProcessor INPUT are sized to this so the
		// native→encode downscale (e.g. 1440p→1080p) happens in the Blt. Falls back to the
		// encode size when unset (capture == encode → no scale).
		let cap_w = if p.capture_width >= 2 {
			p.capture_width & !1
		} else {
			w
		};
		let cap_h = if p.capture_height >= 2 {
			p.capture_height & !1
		} else {
			h
		};

		// -- 0a. What adapter owns the capture device? If it's already NVIDIA we can open
		//    NVENC on it directly (fast path); otherwise NVENC needs its own NVIDIA device.
		let cap_vendor = device_vendor_id(device).unwrap_or(0);
		let capture_is_nvidia = cap_vendor == VENDOR_NVIDIA;
		cap_dbg(&format!(
            "--- Encoder::new {w}x{h}@{fps} capture_vendor=0x{cap_vendor:04X} nvidia={capture_is_nvidia} ---"
        ));

		// -- 0b. Pick the NVENC (a.k.a. "encode") device + context. In the fast path it IS
		//    the capture device; in the hybrid path it's a fresh NVIDIA device we enumerate
		//    here. `bridge` carries the cross-adapter shared texture state when hybrid.
		//    `nv_device`/`nv_context` are the device the WHOLE NVENC chain runs on.
		let (nv_device, nv_context, bridge): (
			ID3D11Device,
			ID3D11DeviceContext,
			Option<CrossAdapterBridge>,
		) = if capture_is_nvidia {
			// Same-device fast path: capture and encode share the NVIDIA device. No hop,
			// no shared texture; `frame.texture` feeds the VideoProcessor directly.
			(device.clone(), ctx.clone(), None)
		} else {
			// HYBRID: create the NVIDIA D3D11 device (must have VIDEO_SUPPORT — the
			// VideoProcessor moves there). Err → host falls back to ffmpeg.
			let (nv_dev, nv_ctx) =
				create_nvidia_device().map_err(|e| format!("hybrid: create NVIDIA device: {e}"))?;
			// Build the AMD-side shared BGRA + open it on NVIDIA (keeps both mutexes/views).
			// Sized to the NATIVE CAPTURE surface (cap_w/cap_h), NOT the encode size: `submit`
			// CopyResource's the native `frame.texture` into `amd_shared`, so they MUST match
			// (a mismatch silently corrupts / fails on AMD+NVIDIA once encode != native). The
			// native→encode downscale then happens in the VideoProcessorBlt (input cap, output w/h).
			// Cross-adapter shared-resource creation can transiently fail E_INVALIDARG /
			// E_ACCESSDENIED on a hybrid laptop (the same GPU-reparenting flake `build_duplication`
			// already retries; the bridge had none → a single flake killed the whole NVENC path).
			// Retry with a short backoff before falling back to ffmpeg.
			let bridge = {
				let mut last = String::new();
				let mut got = None;
				for attempt in 0..4 {
					match CrossAdapterBridge::create(device, ctx, &nv_dev, cap_w, cap_h) {
						Ok(b) => {
							got = Some(b);
							break;
						}
						Err(e) => {
							cap_dbg(&format!("hybrid bridge attempt {attempt} failed: {e}"));
							last = e;
							if attempt < 3 {
								std::thread::sleep(std::time::Duration::from_millis(150));
							}
						}
					}
				}
				got.ok_or_else(|| format!("hybrid: shared bridge (4 tries): {last}"))?
			};
			(nv_dev, nv_ctx, Some(bridge))
		};

		// -- 0c. AddRef the NVENC device/context by cloning the windows-rs handles.
		//    Cloning an Interface == calling AddRef. We hand the RAW device pointer to
		//    NVENC (which does NOT AddRef it) and keep these clones to balance refcounts
		//    and drive the VideoProcessor ourselves. NVENC needs these alive until close.
		let kept_device = nv_device.clone();
		let kept_context = nv_context.clone();
		// The VideoProcessor wants the video device/context. Query them off the NVENC
		// (NVIDIA) device — NOT the AMD capture device, where NV12 conversion is illegal.
		let vdevice: ID3D11VideoDevice = nv_device
			.cast()
			.map_err(|e| format!("QI ID3D11VideoDevice: {e}"))?;
		let vcontext: ID3D11VideoContext = nv_context
			.cast()
			.map_err(|e| format!("QI ID3D11VideoContext: {e}"))?;
		let kept_vdevice = vdevice.clone();
		let kept_vcontext = vcontext.clone();

		// -- 1. ID3D11VideoProcessor for BGRA→NV12 (one GPU Blt/frame) on the NVENC device.
		let (vp_enum, vproc) = build_video_processor(&vdevice, cap_w, cap_h, w, h)?;

		// -- 2. Allocate OUR own NV12 texture (Sunshine single-texture model) on the NVENC
		//    device. NVENC has no hwframe pool — we Blt into THIS every frame, register once.
		let nv12_tex = create_nv12_texture(&kept_device, w, h)?;

		// -- 3. Parse `p.dest` ("rtp://host:port") → SocketAddr; spawn the decoupled RTP
		//    egress (sender thread owns the socket, so the blocking send never stalls the
		//    encode thread — see rtp.rs RtpEgress / the opi5 ~110 ms GOP-stall fix). ---
		let addr = parse_rtp_dest(&p.dest)?;
		// Shared live bitrate (kbps): seeded from the request, read by the RtpEgress pacing
		// thread (Stage 1) and updated by Stage-3 adaptive bitrate.
		let bitrate_kbps = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(p.bitrate_kbps));
		let rtp = RtpEgress::spawn(addr, p.codec, fps, bitrate_kbps.clone())
			.map_err(|e| format!("rtp bind: {e}"))?;

		// -- 4. Load NVENC and create the function-pointer table. ANY failure → Err so
		//    the host falls back to ffmpeg (no NVIDIA GPU / no NVENC driver). ----------
		let lib =
			libloading::Library::new("nvEncodeAPI64.dll").map_err(|e| format!("nvenc dll: {e}"))?;
		// NvEncodeAPICreateInstance(NV_ENCODE_API_FUNCTION_LIST*) fills the fn table.
		let create_instance: libloading::Symbol<
			unsafe extern "C" fn(*mut nvenc::NV_ENCODE_API_FUNCTION_LIST) -> nvenc::NVENCSTATUS,
		> = lib.get(b"NvEncodeAPICreateInstance\0")
			.map_err(|e| format!("nvenc CreateInstance sym: {e}"))?;

		let mut fns: Box<nvenc::NV_ENCODE_API_FUNCTION_LIST> = Box::new(std::mem::zeroed());
		fns.version = nvenc::NV_ENCODE_API_FUNCTION_LIST_VER;
		chk(create_instance(&mut *fns), &fns, ptr::null_mut())
			.map_err(|e| format!("NvEncodeAPICreateInstance: {e}"))?;

		// -- 5. Open the encode session on our D3D11 device. -------------------------
		let mut open: nvenc::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = std::mem::zeroed();
		open.version = nvenc::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
		open.deviceType = nvenc::NV_ENC_DEVICE_TYPE_DIRECTX;
		// The raw pointer MUST come from the AddRef'd clone, not a temporary.
		open.device = kept_device.as_raw();
		open.apiVersion = nvenc::NVENCAPI_VERSION;
		let mut enc: *mut c_void = ptr::null_mut();
		let open_fn = fns
			.nvEncOpenEncodeSessionEx
			.ok_or("nvenc: nvEncOpenEncodeSessionEx missing")?;
		cap_dbg(&format!(
			"OpenSessionEx: device={:p} apiVersion=0x{:08X} ver=0x{:08X}",
			open.device, open.apiVersion, open.version
		));
		let open_rc = open_fn(&mut open, &mut enc);
		cap_dbg(&format!("OpenSessionEx → status {open_rc}"));
		chk(open_rc, &fns, ptr::null_mut())
			.map_err(|e| format!("nvEncOpenEncodeSessionEx: {e}"))?;
		// From here, a failure must DestroyEncoder before returning Err. The guard holds
		// a *copy* of the destroy fn pointer (not a borrow of `fns`) so `fns` can still be
		// moved into the returned Encoder once we disarm.
		let mut guard = SessionGuard {
			destroy: fns.nvEncDestroyEncoder,
			enc,
		};

		// -- 6.-8. Preset baseline, low-latency config, RFI/intra-refresh, init (shared with the
		//    Linux CUDA encoder: `nvsession::configure`). ---------------------------------
		let configured = super::nvsession::configure(
			&fns,
			enc,
			&super::nvsession::SessionParams {
				codec: p.codec,
				width: w,
				height: h,
				fps,
				bitrate_kbps: p.bitrate_kbps,
				low_latency: p.low_latency,
				intra_refresh: p.intra_refresh,
				rfi: p.rfi,
			},
		)?;
		let super::nvsession::Configured {
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
		} = configured;

		// -- 9. Register our NV12 texture as an NVENC input resource. ------------------
		let mut reg: nvenc::NV_ENC_REGISTER_RESOURCE = std::mem::zeroed();
		reg.version = nvenc::NV_ENC_REGISTER_RESOURCE_VER;
		reg.resourceType = nvenc::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
		reg.width = w;
		reg.height = h;
		reg.pitch = 0; // ignored for DirectX resources
		reg.subResourceIndex = 0;
		reg.resourceToRegister = nv12_tex.as_raw();
		reg.bufferFormat = nvenc::NV_ENC_BUFFER_FORMAT_NV12;
		reg.bufferUsage = nvenc::NV_ENC_INPUT_IMAGE;
		let reg_fn = fns
			.nvEncRegisterResource
			.ok_or("nvenc: nvEncRegisterResource missing")?;
		chk(reg_fn(enc, &mut reg), &fns, enc).map_err(|e| format!("nvEncRegisterResource: {e}"))?;
		let registered = reg.registeredResource;

		// -- 10. Create the output bitstream buffer. ----------------------------------
		let mut cbb: nvenc::NV_ENC_CREATE_BITSTREAM_BUFFER = std::mem::zeroed();
		cbb.version = nvenc::NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
		let cbb_fn = fns
			.nvEncCreateBitstreamBuffer
			.ok_or("nvenc: nvEncCreateBitstreamBuffer missing")?;
		chk(cbb_fn(enc, &mut cbb), &fns, enc)
			.map_err(|e| format!("nvEncCreateBitstreamBuffer: {e}"))?;
		let bitstream = cbb.bitstreamBuffer;

		// All NVENC objects created — disarm the session guard; we own teardown now.
		guard.disarm();

		// Unpack the cross-adapter bridge (if hybrid) into the per-field state. The fast
		// path leaves all bridge fields None / a null handle, so `submit` Blt's directly.
		let (amd_shared, amd_context, nvidia_bgra, kept_amd_device, kept_amd_context) = match bridge
		{
			Some(b) => (
				Some(b.amd_shared),
				Some(b.amd_context),
				Some(b.nvidia_bgra),
				Some(b.kept_amd_device),
				Some(b.kept_amd_context),
			),
			None => (None, None, None, None, None),
		};

		Ok(Encoder {
			fns,
			_lib: lib,
			enc,
			registered,
			bitstream,
			nv12_tex,
			vctx: vcontext,
			vproc,
			vp_enum,
			amd_shared,
			amd_context,
			nvidia_bgra,
			rtp,
			bitrate_kbps,
			init_params,
			enc_config,
			width: w,
			height: h,
			rotation: p.rotation,
			fps,
			// With intra refresh there are no scheduled IDRs (the wave heals; the on-demand
			// IDR stays): make the periodic forced IDR effectively never fire (frame 0 and the
			// fast startup window still do).
			idr_interval: if intra_refresh { u32::MAX } else { gop },
			_kept_device: Some(kept_device),
			_kept_context: Some(kept_context),
			_kept_vdevice: Some(kept_vdevice),
			_kept_vcontext: Some(kept_vcontext),
			_kept_amd_device: kept_amd_device,
			_kept_amd_context: kept_amd_context,
			closed: false,
			frame_idx: 0,
			force_idr_once: false,
			codec: p.codec,
			rfi,
			intra_refresh,
			refs: policy::RefTracker::new(dpb),
			win: policy::EncWindow::default(),
			mark_recovery: false,
			low_budget,
			thin_budget,
			two_pass,
			preset_kf_scale,
			// B3: VideoProcessor view caches start empty; `blt_bgra_to_nv12` fills them lazily
			// (output view once, input view keyed by source-texture ptr) and sets the stream
			// rotation once. A rebuilt encoder (resolution/rotation change) gets fresh empties.
			cached_input_view: None,
			cached_output_view: None,
			cached_input_src: ptr::null_mut(),
			rotation_set: false,
		})
	}
}
