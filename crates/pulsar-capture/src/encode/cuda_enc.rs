//! The NVENC SDK encoder on Linux: `libnvidia-encode.so.1` on a CUDA context, fed system-memory
//! NV12 frames (the host's x11grab capture after libavfilter's scale/convert), packetized by our
//! own RTP sender ([`RtpEgress`]).
//!
//! Why not libavcodec's `h264_nvenc` (the Linux host's previous NVENC path)? It has no way to
//! invalidate a reference frame, so a client-reported loss could only be healed with a keyframe.
//! Here the session is ours: the encoder config is the Windows one (`nvsession::configure` —
//! deep DPB with one prediction reference, CBR, one-frame VBV, the same RFI / intra-refresh /
//! low-budget choices), `invalidate_lost` calls `nvEncInvalidateRefFrames` exactly like the
//! Windows `Encoder`, and the recovery frame carries the same Pulsar SEI tag.
//!
//! Per frame: two `cuMemcpy2D` (luma, chroma) into one pitched NV12 device buffer registered
//! once with NVENC (`NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR`), map → encode (sync) → lock →
//! hand the Annex-B access unit to the RTP sender thread → unlock → unmap.
//!
//! Requirements: the NVIDIA proprietary driver with NVENC API 12.2 (driver 550 or newer);
//! anything less is an `Err` from `new` and the host keeps libavcodec. Untested on Linux
//! hardware.

use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use super::cuda::{Context, Cuda, CUdeviceptr};
use super::nvsession::{chk, configure, parse_rtp_dest, Configured, SessionGuard, SessionParams};
use super::policy::{self, EncWindow, RefTracker, RfiPlan};
use super::{cap_dbg, nvenc};
use crate::rtp::RtpEgress;
use crate::Codec;

/// What the encoder is built for.
#[derive(Clone, Debug)]
pub struct NvencCudaParams {
	/// Encode size (even; the frames handed to `encode` must be exactly this size).
	pub width: u32,
	pub height: u32,
	pub fps: u32,
	pub bitrate_kbps: u32,
	/// `rtp://host:port` or `host:port` — where the RTP packets go.
	pub dest: String,
	pub codec: Codec,
	/// Game profile (ULTRA_LOW_LATENCY tuning) vs remote desktop (LOW_LATENCY).
	pub low_latency: bool,
	/// Rolling intra refresh instead of scheduled IDRs.
	pub intra_refresh: bool,
	/// Reference-frame invalidation (the client asked, `StreamReq::rfi`).
	pub rfi: bool,
	/// CUDA device ordinal (0 = the first NVIDIA GPU).
	pub device: i32,
}

/// One NV12 picture in system memory: a luma plane and an interleaved CbCr plane at half
/// height, each with its own row pitch (libav's `AVFrame` `data[0..2]` / `linesize[0..2]`).
pub struct Nv12Frame<'a> {
	pub y: &'a [u8],
	pub y_pitch: usize,
	pub uv: &'a [u8],
	pub uv_pitch: usize,
}

impl Nv12Frame<'_> {
	/// The planes hold at least `w × h` (luma) and `w × h/2` (chroma) at their pitches.
	pub fn fits(&self, w: u32, h: u32) -> bool {
		plane_fits(self.y.len(), self.y_pitch, w as usize, h as usize)
			&& plane_fits(self.uv.len(), self.uv_pitch, w as usize, (h / 2) as usize)
	}
}

/// A plane of `rows` rows of `width` bytes at `pitch` fits in `len` bytes (the last row need not
/// be padded to the pitch).
fn plane_fits(len: usize, pitch: usize, width: usize, rows: usize) -> bool {
	if rows == 0 {
		return true;
	}
	pitch >= width && (rows - 1).checked_mul(pitch).and_then(|n| n.checked_add(width)).is_some_and(|n| n <= len)
}

/// The NVENC function table and the library it came from (the table's pointers live as long as
/// the library stays loaded).
struct Api {
	fns: Box<nvenc::NV_ENCODE_API_FUNCTION_LIST>,
	_lib: libloading::Library,
}

unsafe fn load_api() -> Result<Api, String> {
	let lib = libloading::Library::new("libnvidia-encode.so.1").map_err(|e| format!("libnvidia-encode.so.1: {e}"))?;
	let create_instance: libloading::Symbol<
		unsafe extern "C" fn(*mut nvenc::NV_ENCODE_API_FUNCTION_LIST) -> nvenc::NVENCSTATUS,
	> = lib
		.get(b"NvEncodeAPICreateInstance\0")
		.map_err(|e| format!("nvenc CreateInstance sym: {e}"))?;
	let mut fns: Box<nvenc::NV_ENCODE_API_FUNCTION_LIST> = Box::new(std::mem::zeroed());
	fns.version = nvenc::NV_ENCODE_API_FUNCTION_LIST_VER;
	chk(create_instance(&mut *fns), &fns, ptr::null_mut()).map_err(|e| format!("NvEncodeAPICreateInstance: {e}"))?;
	drop(create_instance);
	Ok(Api { fns, _lib: lib })
}

/// Open an encode session on `ctx` (the context must be current).
unsafe fn open_session(api: &Api, ctx: &Context) -> Result<*mut c_void, String> {
	let mut open: nvenc::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = std::mem::zeroed();
	open.version = nvenc::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
	open.deviceType = nvenc::NV_ENC_DEVICE_TYPE_CUDA;
	open.device = ctx.raw();
	open.apiVersion = nvenc::NVENCAPI_VERSION;
	let open_fn = api.fns.nvEncOpenEncodeSessionEx.ok_or("nvenc: nvEncOpenEncodeSessionEx missing")?;
	let mut enc: *mut c_void = ptr::null_mut();
	chk(open_fn(&mut open, &mut enc), &api.fns, ptr::null_mut()).map_err(|e| format!("nvEncOpenEncodeSessionEx (CUDA): {e}"))?;
	Ok(enc)
}

/// Which codecs this machine's NVENC encodes, asked of the SDK on a CUDA context (device 0).
/// Err = no NVIDIA driver, no CUDA device, or an NVENC API older than 12.2.
pub(crate) unsafe fn probe_codecs() -> Result<Vec<Codec>, String> {
	let cuda = Cuda::get()?;
	if cuda.device_count()? < 1 {
		return Err("no CUDA device".into());
	}
	let api = load_api()?;
	let ctx = Context::create(cuda, 0)?;
	let _cur = ctx.push()?;
	let enc = open_session(&api, &ctx)?;
	let fns = &api.fns;
	let guids = (|| -> Result<Vec<nvenc::GUID>, String> {
		let count_fn = fns.nvEncGetEncodeGUIDCount.ok_or("nvenc: nvEncGetEncodeGUIDCount missing")?;
		let list_fn = fns.nvEncGetEncodeGUIDs.ok_or("nvenc: nvEncGetEncodeGUIDs missing")?;
		let mut n: u32 = 0;
		chk(count_fn(enc, &mut n), fns, enc).map_err(|e| format!("GetEncodeGUIDCount: {e}"))?;
		let mut buf: Vec<nvenc::GUID> = vec![nvenc::GUID::zeroed(); n as usize];
		let mut got: u32 = 0;
		if n > 0 {
			chk(list_fn(enc, buf.as_mut_ptr(), n, &mut got), fns, enc).map_err(|e| format!("GetEncodeGUIDs: {e}"))?;
		}
		buf.truncate(got as usize);
		Ok(buf)
	})();
	if let Some(destroy) = fns.nvEncDestroyEncoder {
		let _ = destroy(enc);
	}
	let guids = guids?;
	cap_dbg(&format!("nvenc (CUDA) probe on {}: {} encode GUIDs", ctx.name, guids.len()));
	Ok(super::codecs_from_guids(&guids))
}

/// The Linux NVENC SDK encoder (see the module doc). Lives on one thread (the host's capture
/// loop); `Send` so it can be built there.
pub struct NvencCuda {
	api: Api,
	ctx: Context,
	enc: *mut c_void,
	/// The pitched NV12 device buffer (luma rows, then chroma rows) and its row pitch.
	dev: CUdeviceptr,
	pitch: usize,
	registered: *mut c_void,
	bitstream: *mut c_void,
	rtp: RtpEgress,
	/// Shared with the RTP sender's pacing (when it paces).
	bitrate_kbps: Arc<AtomicU32>,
	init_params: Box<nvenc::NV_ENC_INITIALIZE_PARAMS>,
	enc_config: Box<nvenc::NV_ENC_CONFIG>,
	width: u32,
	height: u32,
	fps: u32,
	codec: Codec,
	idr_interval: u32,
	frame_idx: u64,
	force_idr_once: bool,
	rfi: bool,
	intra_refresh: bool,
	refs: RefTracker,
	win: EncWindow,
	mark_recovery: bool,
	low_budget: bool,
	thin_budget: bool,
	two_pass: bool,
	preset_kf_scale: u8,
	/// Losses healed by invalidation / declined (logged by the host).
	rfi_ok: u32,
	rfi_no: u32,
	closed: bool,
}

unsafe impl Send for NvencCuda {}

impl NvencCuda {
	/// Load the driver libraries, open a session on a fresh CUDA context, configure it like the
	/// Windows encoder, allocate + register the NV12 input buffer and the bitstream buffer, and
	/// start the RTP sender. Any failure is an `Err` (nothing left behind) so the caller can keep
	/// libavcodec.
	pub fn new(p: &NvencCudaParams) -> Result<Self, String> {
		unsafe { Self::build(p) }
	}

	unsafe fn build(p: &NvencCudaParams) -> Result<Self, String> {
		let (w, h) = (p.width.max(2) & !1, p.height.max(2) & !1);
		let fps = p.fps.max(1);
		cap_dbg(&format!("--- NvencCuda::new {w}x{h}@{fps} {:?} rfi={} ---", p.codec, p.rfi));
		let cuda = Cuda::get()?;
		if cuda.device_count()? <= p.device {
			return Err(format!("no CUDA device {}", p.device));
		}
		let api = load_api()?;
		let ctx = Context::create(cuda, p.device)?;
		let cur = ctx.push()?;
		let enc = open_session(&api, &ctx)?;
		let mut guard = SessionGuard { destroy: api.fns.nvEncDestroyEncoder, enc };
		let fns = &api.fns;

		let Configured {
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
		} = configure(
			fns,
			enc,
			&SessionParams {
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

		// One pitched allocation: h luma rows + h/2 chroma rows (NV12 = one surface to NVENC).
		let (dev, pitch) = ctx.alloc_pitch(w as usize, (h + h / 2) as usize)?;
		// Everything below must free `dev` on failure (the session guard destroys the session,
		// which drops its registrations and bitstream buffers with it).
		let rest = (|| -> Result<(*mut c_void, *mut c_void, RtpEgress, Arc<AtomicU32>), String> {
			let mut reg: nvenc::NV_ENC_REGISTER_RESOURCE = std::mem::zeroed();
			reg.version = nvenc::NV_ENC_REGISTER_RESOURCE_VER;
			reg.resourceType = nvenc::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR;
			reg.width = w;
			reg.height = h;
			reg.pitch = pitch as u32;
			reg.resourceToRegister = dev as usize as *mut c_void;
			reg.bufferFormat = nvenc::NV_ENC_BUFFER_FORMAT_NV12;
			reg.bufferUsage = nvenc::NV_ENC_INPUT_IMAGE;
			let reg_fn = fns.nvEncRegisterResource.ok_or("nvenc: nvEncRegisterResource missing")?;
			chk(reg_fn(enc, &mut reg), fns, enc).map_err(|e| format!("nvEncRegisterResource (CUDA): {e}"))?;
			let registered = reg.registeredResource;

			let mut cbb: nvenc::NV_ENC_CREATE_BITSTREAM_BUFFER = std::mem::zeroed();
			cbb.version = nvenc::NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
			let cbb_fn = fns.nvEncCreateBitstreamBuffer.ok_or("nvenc: nvEncCreateBitstreamBuffer missing")?;
			chk(cbb_fn(enc, &mut cbb), fns, enc).map_err(|e| format!("nvEncCreateBitstreamBuffer: {e}"))?;

			let addr = parse_rtp_dest(&p.dest)?;
			let bitrate_kbps = Arc::new(AtomicU32::new(p.bitrate_kbps));
			let rtp = RtpEgress::spawn(addr, p.codec, fps, bitrate_kbps.clone()).map_err(|e| format!("rtp bind: {e}"))?;
			Ok((registered, cbb.bitstreamBuffer, rtp, bitrate_kbps))
		})();
		let (registered, bitstream, rtp, bitrate_kbps) = match rest {
			Ok(v) => v,
			Err(e) => {
				drop(guard); // destroys the session (and what was registered on it)
				ctx.free(dev);
				return Err(e);
			}
		};
		guard.disarm();
		drop(cur);
		cap_dbg(&format!("NvencCuda ready on {} (pitch {pitch}, dpb {dpb}, rfi {rfi})", ctx.name));
		Ok(NvencCuda {
			api,
			ctx,
			enc,
			dev,
			pitch,
			registered,
			bitstream,
			rtp,
			bitrate_kbps,
			init_params,
			enc_config,
			width: w,
			height: h,
			fps,
			codec: p.codec,
			idr_interval: if intra_refresh { u32::MAX } else { gop },
			frame_idx: 0,
			force_idr_once: false,
			rfi,
			intra_refresh,
			refs: RefTracker::new(dpb),
			win: EncWindow::default(),
			mark_recovery: false,
			low_budget,
			thin_budget,
			two_pass,
			preset_kf_scale,
			rfi_ok: 0,
			rfi_no: 0,
			closed: false,
		})
	}

	/// The encode size (what `encode` expects).
	pub fn size(&self) -> (u32, u32) {
		(self.width, self.height)
	}

	/// Reference-frame invalidation is on for this stream (asked for, driver caps allow it,
	/// not AV1, not intra refresh).
	pub fn rfi_enabled(&self) -> bool {
		self.rfi
	}

	/// (losses healed by invalidation, losses declined) since the encoder was built.
	pub fn rfi_counts(&self) -> (u32, u32) {
		(self.rfi_ok, self.rfi_no)
	}

	/// The next encoded frame is an IDR (with parameter sets). Survives a failed encode.
	pub fn request_idr(&mut self) {
		self.force_idr_once = true;
	}

	/// The client lost the RTP packets `first..=last`: invalidate the frames they carried as
	/// references so the next frame predicts from an older intact one and is tagged as the
	/// recovery point. The same rules as the Windows encoder (`Encoder::invalidate_lost`): the
	/// host's `RefTracker` declines what the buffer can no longer reach, and NO keyframe is
	/// forced here (the client asks for one if its repair fails). `Ok(frames)` = invalidated.
	pub fn invalidate_lost(&mut self, first: u16, last: u16) -> Result<usize, &'static str> {
		let r = unsafe { self.invalidate(first, last) };
		match r {
			Ok(_) => self.rfi_ok = self.rfi_ok.wrapping_add(1),
			Err(_) => self.rfi_no = self.rfi_no.wrapping_add(1),
		}
		r
	}

	unsafe fn invalidate(&mut self, first: u16, last: u16) -> Result<usize, &'static str> {
		if self.closed {
			return Err("encoder closed");
		}
		if !self.rfi {
			return Err(if self.intra_refresh {
				"rfi off (intra refresh heals the loss)"
			} else {
				"rfi off (the client asks for a keyframe if its repair fails)"
			});
		}
		let lost = self.rtp.frames_in_seq_range(first, last);
		let pts = match self.refs.plan(&lost) {
			RfiPlan::Invalidate(pts) => pts,
			RfiPlan::Nothing(why) => return Err(why),
		};
		let f = self.api.fns.nvEncInvalidateRefFrames;
		let Ok(_cur) = self.ctx.push() else {
			self.refs.invalidate_failed(&pts);
			return Err("cuCtxPushCurrent failed");
		};
		let ok = !f.is_null() && {
			let f: nvenc::PFN_InvalidateRefFrames = std::mem::transmute(f);
			pts.iter().all(|p| f(self.enc, *p) == nvenc::NV_ENC_SUCCESS)
		};
		if !ok {
			self.refs.invalidate_failed(&pts);
			return Err("nvEncInvalidateRefFrames failed");
		}
		self.mark_recovery = true;
		Ok(pts.len())
	}

	/// Live bitrate change (`nvEncReconfigureEncoder`, no reset, no keyframe), with the same
	/// rate-control class re-judgement as the Windows encoder.
	pub fn set_bitrate(&mut self, kbps: u32) -> Result<(), String> {
		if self.closed {
			return Err("reconfigure after close".into());
		}
		let _cur = self.ctx.push()?;
		unsafe {
			let bps = kbps.saturating_mul(1000).max(1);
			let low = policy::low_budget_live(self.low_budget, bps, self.width, self.height, self.fps);
			let thin = policy::thin_budget_live(self.thin_budget, bps, self.width, self.height, self.fps);
			let env = std::env::var("PULSAR_NVENC_MULTIPASS").ok();
			let two_pass = policy::two_pass_for(thin, self.width, self.height, self.fps, env.as_deref());
			let mut class_change = low != self.low_budget || two_pass != self.two_pass;
			let mut refused = false;
			if class_change {
				self.set_rc_class(low, two_pass);
			}
			let vbv = bps / self.fps.max(1);
			self.enc_config.rcParams.averageBitRate = bps;
			self.enc_config.rcParams.maxBitRate = bps;
			self.enc_config.rcParams.vbvBufferSize = vbv;
			self.enc_config.rcParams.vbvInitialDelay = vbv;
			self.init_params.encodeConfig = &mut *self.enc_config;
			let mut rc: nvenc::NV_ENC_RECONFIGURE_PARAMS = std::mem::zeroed();
			rc.version = nvenc::NV_ENC_RECONFIGURE_PARAMS_VER;
			rc.reInitEncodeParams = std::ptr::read(&*self.init_params);
			rc.set_reset_encoder(false);
			let f = self.api.fns.nvEncReconfigureEncoder.ok_or("nvenc: nvEncReconfigureEncoder missing")?;
			let mut res = chk(f(self.enc, &mut rc), &self.api.fns, self.enc);
			if res.is_err() && class_change {
				self.set_rc_class(self.low_budget, self.two_pass);
				class_change = false;
				refused = true;
				res = chk(f(self.enc, &mut rc), &self.api.fns, self.enc);
			}
			res.map_err(|e| format!("nvEncReconfigureEncoder({kbps} kbps): {e}"))?;
			if class_change {
				cap_dbg(&format!(
					"nvenc (CUDA) rc class {kbps} kbps: low_budget {} -> {low}, two_pass {} -> {two_pass}",
					self.low_budget, self.two_pass
				));
				self.low_budget = low;
				self.two_pass = two_pass;
			}
			if !refused {
				self.thin_budget = thin;
			}
		}
		self.bitrate_kbps.store(kbps, Ordering::Relaxed);
		Ok(())
	}

	fn set_rc_class(&mut self, low: bool, two_pass: bool) {
		let rc = &mut self.enc_config.rcParams;
		rc.multiPass = if two_pass { nvenc::NV_ENC_TWO_PASS_QUARTER_RESOLUTION } else { nvenc::NV_ENC_MULTI_PASS_DISABLED };
		rc.lowDelayKeyFrameScale = if low { 1 } else { self.preset_kf_scale };
	}

	/// Encode one picture and send it. `rtp_ts_90k` = the frame's capture time in 90 kHz ticks
	/// (stamped on its RTP packets). Returns the access unit's size in bytes. A failed frame is
	/// not fatal (the next one is tried); a pending keyframe request survives it.
	pub fn encode(&mut self, frame: &Nv12Frame<'_>, rtp_ts_90k: u32) -> Result<usize, String> {
		if self.closed {
			return Err("encode after close".into());
		}
		if !frame.fits(self.width, self.height) {
			return Err(format!("frame planes smaller than {}x{} NV12", self.width, self.height));
		}
		let _cur = self.ctx.push()?;
		unsafe {
			let (w, h) = (self.width as usize, self.height as usize);
			self.ctx.copy_host_to_device(frame.y.as_ptr(), frame.y_pitch, self.dev, self.pitch, w, h)?;
			self.ctx.copy_host_to_device(
				frame.uv.as_ptr(),
				frame.uv_pitch,
				self.dev + (self.pitch * h) as u64,
				self.pitch,
				w,
				h / 2,
			)?;
			self.encode_mapped(rtp_ts_90k)
		}
	}

	/// Map the uploaded buffer, encode, send, unmap (the context is current).
	unsafe fn encode_mapped(&mut self, rtp_ts_90k: u32) -> Result<usize, String> {
		let map_fn = self.api.fns.nvEncMapInputResource.ok_or("nvEncMapInputResource missing")?;
		let mut map: nvenc::NV_ENC_MAP_INPUT_RESOURCE = std::mem::zeroed();
		map.version = nvenc::NV_ENC_MAP_INPUT_RESOURCE_VER;
		map.registeredResource = self.registered;
		chk(map_fn(self.enc, &mut map), &self.api.fns, self.enc).map_err(|e| format!("nvEncMapInputResource: {e}"))?;
		let mapped = map.mappedResource;
		let res = self.encode_picture(mapped, map.mappedBufferFmt, rtp_ts_90k);
		if let Some(unmap_fn) = self.api.fns.nvEncUnmapInputResource {
			let _ = unmap_fn(self.enc, mapped);
		}
		res
	}

	unsafe fn encode_picture(&mut self, input: *mut c_void, fmt: u32, rtp_ts_90k: u32) -> Result<usize, String> {
		let enc_fn = self.api.fns.nvEncEncodePicture.ok_or("nvEncEncodePicture missing")?;
		let pts = self.frame_idx;
		let mut pic: nvenc::NV_ENC_PIC_PARAMS = std::mem::zeroed();
		pic.version = nvenc::NV_ENC_PIC_PARAMS_VER;
		pic.inputWidth = self.width;
		pic.inputHeight = self.height;
		pic.inputPitch = self.pitch as u32;
		pic.inputBuffer = input;
		pic.bufferFmt = fmt;
		pic.pictureStruct = nvenc::NV_ENC_PIC_STRUCT_FRAME;
		pic.outputBitstream = self.bitstream;
		pic.inputTimeStamp = pts;
		// The Windows keyframe policy (see submit.rs): a rare safety IDR, a catch-up burst for a
		// fresh encoder unless the budget is low, and the client's request.
		let catch_up = !policy::low_budget(self.enc_config.rcParams.averageBitRate, self.width, self.height, self.fps);
		let force_idr =
			self.force_idr_once || policy::scheduled_idr(self.frame_idx, self.idr_interval.max(1) as u64, self.fps, catch_up);
		pic.encodePicFlags = if force_idr { nvenc::NV_ENC_PIC_FLAG_FORCEIDR | nvenc::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS } else { 0 };
		self.frame_idx += 1;
		chk(enc_fn(self.enc, &mut pic), &self.api.fns, self.enc).map_err(|e| format!("nvEncEncodePicture: {e}"))?;

		let lock_fn = self.api.fns.nvEncLockBitstream.ok_or("nvEncLockBitstream missing")?;
		let mut lock: nvenc::NV_ENC_LOCK_BITSTREAM = std::mem::zeroed();
		lock.version = nvenc::NV_ENC_LOCK_BITSTREAM_VER;
		lock.outputBitstream = self.bitstream;
		lock.set_flags(0);
		chk(lock_fn(self.enc, &mut lock), &self.api.fns, self.enc).map_err(|e| format!("nvEncLockBitstream: {e}"))?;
		self.force_idr_once = false;
		let idr = force_idr || lock.pictureType == nvenc::NV_ENC_PIC_TYPE_IDR;
		self.refs.record(rtp_ts_90k, pts, idr);
		let now = std::time::Instant::now();
		let size = lock.bitstreamSizeInBytes as usize;
		self.win.add(now, size, idr);
		if let Some(line) = self.win.take(now, self.enc_config.rcParams.averageBitRate, self.fps) {
			cap_dbg(&line);
		}
		let data = lock.bitstreamBufferPtr as *const u8;
		if size > 0 && !data.is_null() {
			let annexb = std::slice::from_raw_parts(data, size);
			if self.mark_recovery {
				let mut au = super::recovery_sei(self.codec);
				au.extend_from_slice(annexb);
				self.rtp.send_access_unit(&au, rtp_ts_90k);
				self.mark_recovery = false;
			} else {
				self.rtp.send_access_unit(annexb, rtp_ts_90k);
			}
		}
		if let Some(unlock_fn) = self.api.fns.nvEncUnlockBitstream {
			let _ = unlock_fn(self.enc, self.bitstream);
		}
		Ok(size)
	}

	fn close(&mut self) {
		if self.closed {
			return;
		}
		self.closed = true;
		let cur = self.ctx.push();
		unsafe {
			let fns = &self.api.fns;
			if !self.bitstream.is_null() {
				if let Some(f) = fns.nvEncDestroyBitstreamBuffer {
					let _ = f(self.enc, self.bitstream);
				}
				self.bitstream = ptr::null_mut();
			}
			if !self.registered.is_null() {
				if let Some(f) = fns.nvEncUnregisterResource {
					let _ = f(self.enc, self.registered);
				}
				self.registered = ptr::null_mut();
			}
			if !self.enc.is_null() {
				if let Some(f) = fns.nvEncDestroyEncoder {
					let _ = f(self.enc);
				}
				self.enc = ptr::null_mut();
			}
		}
		if cur.is_ok() {
			self.ctx.free(self.dev);
		}
		self.dev = 0;
		drop(cur);
		// `ctx` (cuCtxDestroy) and `api` (the library) drop with the struct, after this.
	}
}

impl Drop for NvencCuda {
	fn drop(&mut self) {
		self.close();
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn plane_fit_rules() {
		// Tight 4×2 plane.
		assert!(plane_fits(8, 4, 4, 2));
		// Pitch 8, the last row unpadded: 8 + 4 bytes.
		assert!(plane_fits(12, 8, 4, 2));
		assert!(!plane_fits(11, 8, 4, 2));
		// Pitch below the width is never a valid plane.
		assert!(!plane_fits(100, 3, 4, 2));
		assert!(plane_fits(0, 0, 4, 0));
	}

	#[test]
	fn nv12_frame_fits_both_planes() {
		let y = vec![0u8; 64 * 4];
		let uv = vec![0u8; 64 * 2];
		let f = Nv12Frame { y: &y, y_pitch: 64, uv: &uv, uv_pitch: 64 };
		assert!(f.fits(64, 4));
		assert!(!f.fits(64, 6));
		let short = Nv12Frame { y: &y, y_pitch: 64, uv: &uv[..64], uv_pitch: 64 };
		assert!(!short.fits(64, 4));
	}

	/// On a machine without the NVIDIA driver the encoder must fail cleanly (the host then keeps
	/// libavcodec); with one, it encodes a few grey frames into a local socket.
	#[test]
	fn builds_or_fails_cleanly() {
		let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
		let p = NvencCudaParams {
			width: 320,
			height: 240,
			fps: 30,
			bitrate_kbps: 2000,
			dest: format!("127.0.0.1:{}", sock.local_addr().unwrap().port()),
			codec: Codec::H264,
			low_latency: true,
			intra_refresh: false,
			rfi: true,
			device: 0,
		};
		match NvencCuda::new(&p) {
			Err(e) => eprintln!("no NVENC here (expected without the NVIDIA driver): {e}"),
			Ok(mut enc) => {
				let y = vec![128u8; 320 * 240];
				let uv = vec![128u8; 320 * 120];
				for i in 0..5u32 {
					let n = enc.encode(&Nv12Frame { y: &y, y_pitch: 320, uv: &uv, uv_pitch: 320 }, i * 3000).unwrap();
					assert!(n > 0);
				}
			}
		}
	}
}
