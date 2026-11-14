//! VideoToolbox real-time encoder (`VTCompressionSession`) for the macOS native host path.
//!
//! - H.264 High / HEVC Main, hardware encoder preferred (software allowed as VT's own fallback).
//! - Real-time, no frame reordering (no B-frames), `MaxFrameDelayCount = 0`, low-latency rate
//!   control (`EnableLowLatencyRateControl`, macOS 11.3+) when the OS accepts it for the codec.
//! - Bitrate changes live (`AverageBitRate` + `DataRateLimits`), a keyframe on request
//!   (`kVTEncodeFrameOptionKey_ForceKeyFrame`), a 10 s safety GOP otherwise.
//! - Output: Annex B access units with the parameter sets in front of every keyframe, handed
//!   to a callback on VideoToolbox's output thread.
//!
//! Hand-written bindings (no objc2-video-toolbox): VideoToolbox, CoreMedia and CoreFoundation
//! exist on every macOS the app runs on, but several property keys are newer than its minimum
//! macOS, so every key is resolved at run time and a missing one is skipped.
//!
//! Untested on macOS hardware.

#![allow(non_upper_case_globals, non_snake_case)]

use std::ffi::{c_void, CStr};
use std::sync::Mutex;

use super::annexb;

type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFArrayRef = *const c_void;
type OSStatus = i32;
type VTSessionRef = *mut c_void;
type CMSampleBufferRef = *mut c_void;
type CMBlockBufferRef = *mut c_void;
type CMFormatDescriptionRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CMTime {
	value: i64,
	timescale: i32,
	flags: u32,
	epoch: i64,
}

impl CMTime {
	fn new(value: i64, timescale: i32) -> Self {
		Self { value, timescale, flags: 1, epoch: 0 }
	}
	/// `kCMTimeInvalid`.
	fn invalid() -> Self {
		Self::default()
	}
}

#[repr(C)]
struct CFCallBacks {
	_private: [u8; 0],
}

type OutputCallback = unsafe extern "C" fn(
	output_ref_con: *mut c_void,
	source_frame_ref_con: *mut c_void,
	status: OSStatus,
	info_flags: u32,
	sample_buffer: CMSampleBufferRef,
);

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
	static kCFTypeDictionaryKeyCallBacks: CFCallBacks;
	static kCFTypeDictionaryValueCallBacks: CFCallBacks;
	static kCFTypeArrayCallBacks: CFCallBacks;
	static kCFBooleanTrue: CFTypeRef;
	static kCFBooleanFalse: CFTypeRef;
	fn CFRelease(cf: CFTypeRef);
	fn CFDictionaryCreateMutable(alloc: *const c_void, capacity: isize, k: *const CFCallBacks, v: *const CFCallBacks) -> *mut c_void;
	fn CFDictionarySetValue(d: *mut c_void, key: *const c_void, value: *const c_void);
	fn CFDictionaryGetValue(d: CFDictionaryRef, key: *const c_void) -> *const c_void;
	fn CFNumberCreate(alloc: *const c_void, the_type: isize, value_ptr: *const c_void) -> CFTypeRef;
	fn CFArrayCreate(alloc: *const c_void, values: *const *const c_void, num: isize, cb: *const CFCallBacks) -> CFArrayRef;
	fn CFArrayGetCount(a: CFArrayRef) -> isize;
	fn CFArrayGetValueAtIndex(a: CFArrayRef, idx: isize) -> *const c_void;
	fn CFBooleanGetValue(b: CFTypeRef) -> u8;
}

const kCFNumberSInt64Type: isize = 4;
const kCFNumberFloat64Type: isize = 6;

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
	static kCMSampleAttachmentKey_NotSync: CFStringRef;
	fn CMSampleBufferGetDataBuffer(sbuf: CMSampleBufferRef) -> CMBlockBufferRef;
	fn CMSampleBufferGetFormatDescription(sbuf: CMSampleBufferRef) -> CMFormatDescriptionRef;
	fn CMSampleBufferGetSampleAttachmentsArray(sbuf: CMSampleBufferRef, create: u8) -> CFArrayRef;
	fn CMSampleBufferGetPresentationTimeStamp(sbuf: CMSampleBufferRef) -> CMTime;
	fn CMBlockBufferGetDataLength(bb: CMBlockBufferRef) -> usize;
	fn CMBlockBufferCopyDataBytes(bb: CMBlockBufferRef, offset: usize, len: usize, dest: *mut c_void) -> OSStatus;
	fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
		desc: CMFormatDescriptionRef,
		index: usize,
		ptr_out: *mut *const u8,
		size_out: *mut usize,
		count_out: *mut usize,
		nal_header_len_out: *mut i32,
	) -> OSStatus;
	fn CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
		desc: CMFormatDescriptionRef,
		index: usize,
		ptr_out: *mut *const u8,
		size_out: *mut usize,
		count_out: *mut usize,
		nal_header_len_out: *mut i32,
	) -> OSStatus;
}

#[link(name = "VideoToolbox", kind = "framework")]
extern "C" {
	fn VTCompressionSessionCreate(
		allocator: *const c_void,
		width: i32,
		height: i32,
		codec_type: u32,
		encoder_specification: CFDictionaryRef,
		source_image_buffer_attributes: CFDictionaryRef,
		compressed_data_allocator: *const c_void,
		output_callback: Option<OutputCallback>,
		output_callback_ref_con: *mut c_void,
		session_out: *mut VTSessionRef,
	) -> OSStatus;
	fn VTSessionSetProperty(session: VTSessionRef, key: CFStringRef, value: CFTypeRef) -> OSStatus;
	fn VTCompressionSessionPrepareToEncodeFrames(session: VTSessionRef) -> OSStatus;
	fn VTCompressionSessionEncodeFrame(
		session: VTSessionRef,
		image_buffer: *mut c_void,
		pts: CMTime,
		duration: CMTime,
		frame_properties: CFDictionaryRef,
		source_frame_ref_con: *mut c_void,
		info_flags_out: *mut u32,
	) -> OSStatus;
	fn VTCompressionSessionCompleteFrames(session: VTSessionRef, until: CMTime) -> OSStatus;
	fn VTCompressionSessionInvalidate(session: VTSessionRef);
}

/// `kVTEncodeInfo_FrameDropped`.
const INFO_FRAME_DROPPED: u32 = 1 << 1;
/// `kCMVideoCodecType_H264` ('avc1') / `kCMVideoCodecType_HEVC` ('hvc1').
const CODEC_H264: u32 = 0x6176_6331;
const CODEC_HEVC: u32 = 0x6876_6331;

/// A VideoToolbox key (`extern const CFStringRef`), looked up at run time.
fn key(name: &CStr) -> Option<CFStringRef> {
	// SAFETY: dlsym of an exported CFStringRef variable; a missing one is null.
	let p = unsafe { libc_dlsym(name) };
	if p.is_null() {
		return None;
	}
	// SAFETY: the symbol is the address of a CFStringRef.
	let v = unsafe { *(p as *const CFStringRef) };
	(!v.is_null()).then_some(v)
}

extern "C" {
	fn dlsym(handle: *mut c_void, symbol: *const std::ffi::c_char) -> *mut c_void;
}

/// `RTLD_DEFAULT` on Apple platforms.
const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

unsafe fn libc_dlsym(name: &CStr) -> *mut c_void {
	dlsym(RTLD_DEFAULT, name.as_ptr())
}

/// An owned CF object (+1), released on drop.
struct Cf(CFTypeRef);

impl Drop for Cf {
	fn drop(&mut self) {
		if !self.0.is_null() {
			// SAFETY: we own one reference.
			unsafe { CFRelease(self.0) };
		}
	}
}

fn cf_i64(v: i64) -> Cf {
	// SAFETY: CFNumberCreate copies the value.
	Cf(unsafe { CFNumberCreate(std::ptr::null(), kCFNumberSInt64Type, (&v as *const i64).cast()) })
}

fn cf_f64(v: f64) -> Cf {
	// SAFETY: CFNumberCreate copies the value.
	Cf(unsafe { CFNumberCreate(std::ptr::null(), kCFNumberFloat64Type, (&v as *const f64).cast()) })
}

fn cf_bool(v: bool) -> CFTypeRef {
	// SAFETY: immortal CF constants.
	unsafe {
		if v {
			kCFBooleanTrue
		} else {
			kCFBooleanFalse
		}
	}
}

/// A mutable CFDictionary with CF-typed keys and values.
struct Dict(Cf);

impl Dict {
	fn new() -> Self {
		// SAFETY: standard CF type callbacks.
		Dict(Cf(unsafe {
			CFDictionaryCreateMutable(std::ptr::null(), 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks)
		} as CFTypeRef))
	}
	/// Set `name` (a run-time looked-up key) when this macOS has it; `false` otherwise.
	fn set(&self, name: &CStr, value: CFTypeRef) -> bool {
		match key(name) {
			Some(k) if !self.0 .0.is_null() => {
				// SAFETY: retains key and value.
				unsafe { CFDictionarySetValue(self.0 .0 as *mut c_void, k, value) };
				true
			}
			_ => false,
		}
	}
	fn as_ptr(&self) -> CFDictionaryRef {
		self.0 .0
	}
}

/// Which codec the session encodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VtCodec {
	H264,
	Hevc,
}

/// Session parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VtSpec {
	pub codec: VtCodec,
	pub width: u32,
	pub height: u32,
	pub fps: u32,
	pub bitrate_kbps: u32,
	/// Favour encode speed over quality (game mode / the client's latency preference).
	pub low_latency: bool,
}

/// One encoded access unit (Annex B).
pub struct EncodedAu<'a> {
	pub data: &'a [u8],
	pub keyframe: bool,
	/// The frame's presentation time as given to [`VtEncoder::encode`], µs.
	pub pts_us: i64,
}

/// What the output callback calls (on VideoToolbox's thread).
pub type AuSink = Box<dyn FnMut(EncodedAu<'_>) + Send>;

struct OutCtx {
	codec: VtCodec,
	sink: Mutex<AuSink>,
	/// Raw (length-prefixed) bytes and the Annex B output, reused per frame.
	scratch: Mutex<(Vec<u8>, Vec<u8>)>,
	/// Encoder errors / dropped frames reported to the callback.
	errors: std::sync::atomic::AtomicU32,
}

unsafe extern "C" fn on_output(
	refcon: *mut c_void,
	_src: *mut c_void,
	status: OSStatus,
	flags: u32,
	sample: CMSampleBufferRef,
) {
	let ctx = &*(refcon as *const OutCtx);
	if status != 0 || sample.is_null() || flags & INFO_FRAME_DROPPED != 0 {
		ctx.errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
		return;
	}
	let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| deliver(ctx, sample)));
}

unsafe fn deliver(ctx: &OutCtx, sample: CMSampleBufferRef) {
	// A frame is a keyframe unless its first attachment dictionary says NotSync = true.
	let mut keyframe = true;
	let atts = CMSampleBufferGetSampleAttachmentsArray(sample, 0);
	if !atts.is_null() && CFArrayGetCount(atts) > 0 {
		let d = CFArrayGetValueAtIndex(atts, 0);
		let v = CFDictionaryGetValue(d, kCMSampleAttachmentKey_NotSync);
		if !v.is_null() && CFBooleanGetValue(v) != 0 {
			keyframe = false;
		}
	}
	let bb = CMSampleBufferGetDataBuffer(sample);
	if bb.is_null() {
		return;
	}
	let len = CMBlockBufferGetDataLength(bb);
	let Ok(mut scratch) = ctx.scratch.lock() else { return };
	let (raw, out) = &mut *scratch;
	raw.resize(len, 0);
	if CMBlockBufferCopyDataBytes(bb, 0, len, raw.as_mut_ptr().cast()) != 0 {
		return;
	}
	out.clear();
	let desc = CMSampleBufferGetFormatDescription(sample);
	let mut nal_len = 4usize;
	if !desc.is_null() {
		let get = match ctx.codec {
			VtCodec::H264 => CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
			VtCodec::Hevc => CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
		};
		let mut count = 0usize;
		let mut hdr: i32 = 4;
		let mut p: *const u8 = std::ptr::null();
		let mut sz = 0usize;
		if get(desc, 0, &mut p, &mut sz, &mut count, &mut hdr) == 0 && hdr > 0 {
			nal_len = hdr as usize;
		}
		if keyframe {
			let mut sets: Vec<&[u8]> = Vec::with_capacity(count);
			for i in 0..count {
				let mut p: *const u8 = std::ptr::null();
				let mut sz = 0usize;
				if get(desc, i, &mut p, &mut sz, std::ptr::null_mut(), std::ptr::null_mut()) == 0 && !p.is_null() {
					sets.push(std::slice::from_raw_parts(p, sz));
				}
			}
			annexb::push_parameter_sets(&sets, out);
		}
	}
	if annexb::length_prefixed_to_annexb(raw, nal_len, out).is_err() {
		ctx.errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
		return;
	}
	let pts = CMSampleBufferGetPresentationTimeStamp(sample);
	let pts_us = if pts.timescale > 0 { pts.value.saturating_mul(1_000_000) / pts.timescale as i64 } else { 0 };
	if let Ok(mut sink) = ctx.sink.lock() {
		sink(EncodedAu { data: out, keyframe, pts_us });
	}
}

/// A running `VTCompressionSession`.
pub struct VtEncoder {
	session: VTSessionRef,
	ctx: *mut OutCtx,
	spec: VtSpec,
	/// Whether low-latency rate control was accepted for this session.
	pub low_latency_rc: bool,
}

// SAFETY: a VTCompressionSession may be driven from any one thread at a time (we own it);
// `ctx` is only shared with VideoToolbox's output thread through the Mutexes inside.
unsafe impl Send for VtEncoder {}

impl VtEncoder {
	/// Create and configure a session; `sink` receives every encoded access unit.
	pub fn new(spec: VtSpec, sink: AuSink) -> Result<Self, String> {
		let ctx = Box::into_raw(Box::new(OutCtx {
			codec: spec.codec,
			sink: Mutex::new(sink),
			scratch: Mutex::new((Vec::new(), Vec::new())),
			errors: Default::default(),
		}));
		let codec = match spec.codec {
			VtCodec::H264 => CODEC_H264,
			VtCodec::Hevc => CODEC_HEVC,
		};
		let create = |low_latency_rc: bool| -> Result<VTSessionRef, OSStatus> {
			let enc_spec = Dict::new();
			enc_spec.set(c"kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder", cf_bool(true));
			if low_latency_rc && !enc_spec.set(c"kVTVideoEncoderSpecification_EnableLowLatencyRateControl", cf_bool(true)) {
				return Err(-1);
			}
			let mut s: VTSessionRef = std::ptr::null_mut();
			// SAFETY: plain session creation; `ctx` outlives the session (freed after invalidate).
			let st = unsafe {
				VTCompressionSessionCreate(
					std::ptr::null(),
					spec.width as i32,
					spec.height as i32,
					codec,
					enc_spec.as_ptr(),
					std::ptr::null(),
					std::ptr::null(),
					Some(on_output),
					ctx.cast(),
					&mut s,
				)
			};
			if st == 0 && !s.is_null() {
				Ok(s)
			} else {
				Err(st)
			}
		};
		let (session, low_latency_rc) = match create(true) {
			Ok(s) => (s, true),
			Err(_) => match create(false) {
				Ok(s) => (s, false),
				Err(st) => {
					// SAFETY: no session refers to it.
					drop(unsafe { Box::from_raw(ctx) });
					return Err(format!("VTCompressionSessionCreate failed ({st})"));
				}
			},
		};
		// Configure a fresh session; on failure the session is torn down (not `ctx`, which the
		// next attempt reuses) and the error comes back.
		let setup = |session: VTSessionRef, low_latency_rc: bool| -> Result<Self, String> {
			let enc = std::mem::ManuallyDrop::new(Self { session, ctx, spec, low_latency_rc });
			match enc.configure() {
				Ok(()) => Ok(std::mem::ManuallyDrop::into_inner(enc)),
				Err(e) => {
					// SAFETY: this session is ours and is used nowhere else.
					unsafe { discard_session(session) };
					Err(e)
				}
			}
		};
		let result = match setup(session, low_latency_rc) {
			Err(e) if low_latency_rc => {
				// The low-latency rate controller took the session but refused a property this
				// path needs (frame reordering off, prepare-to-encode, …): retry with the classic
				// rate control before giving the whole native path up for ffmpeg.
				tracing::warn!("VideoToolbox: low-latency session refused its setup ({e}) — retrying without");
				match create(false) {
					Ok(s) => setup(s, false),
					Err(st) => Err(format!("VTCompressionSessionCreate failed ({st})")),
				}
			}
			r => r,
		};
		if result.is_err() {
			// SAFETY: every session that referred to it was discarded above.
			drop(unsafe { Box::from_raw(ctx) });
		}
		result
	}

	fn set(&self, name: &CStr, value: CFTypeRef) -> bool {
		match key(name) {
			// SAFETY: a live session and a CF value.
			Some(k) => unsafe { VTSessionSetProperty(self.session, k, value) == 0 },
			None => false,
		}
	}

	fn configure(&self) -> Result<(), String> {
		let s = self.spec;
		if !self.set(c"kVTCompressionPropertyKey_RealTime", cf_bool(true)) {
			tracing::warn!("VideoToolbox: RealTime not accepted");
		}
		if !self.set(c"kVTCompressionPropertyKey_AllowFrameReordering", cf_bool(false)) {
			return Err("VideoToolbox refused to turn frame reordering off".into());
		}
		let profile = match s.codec {
			VtCodec::H264 => key(c"kVTProfileLevel_H264_High_AutoLevel"),
			VtCodec::Hevc => key(c"kVTProfileLevel_HEVC_Main_AutoLevel"),
		};
		if let Some(p) = profile {
			self.set(c"kVTCompressionPropertyKey_ProfileLevel", p);
		}
		let fps = s.fps.max(1);
		self.set(c"kVTCompressionPropertyKey_ExpectedFrameRate", cf_i64(fps as i64).0);
		// Safety GOP: the client asks for a keyframe when it needs one (`request_idr`).
		self.set(c"kVTCompressionPropertyKey_MaxKeyFrameInterval", cf_i64(fps as i64 * 10).0);
		self.set(c"kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration", cf_f64(10.0).0);
		// Hand every frame out as soon as it is encoded (macOS 10.10+; optional).
		self.set(c"kVTCompressionPropertyKey_MaxFrameDelayCount", cf_i64(0).0);
		if s.low_latency {
			// macOS 12+; optional.
			self.set(c"kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality", cf_bool(true));
		}
		self.set_bitrate(s.bitrate_kbps);
		// SAFETY: a live session.
		let st = unsafe { VTCompressionSessionPrepareToEncodeFrames(self.session) };
		if st != 0 {
			return Err(format!("VTCompressionSessionPrepareToEncodeFrames failed ({st})"));
		}
		Ok(())
	}

	/// Change the target bitrate live.
	pub fn set_bitrate(&self, kbps: u32) {
		let bps = kbps.max(100) as i64 * 1000;
		let n = cf_i64(bps);
		self.set(c"kVTCompressionPropertyKey_AverageBitRate", n.0);
		// Hard cap: 1.5 × the average over any one second (bytes, seconds) — a keyframe may
		// overshoot the average but not flood the link. Some low-latency encoders refuse it.
		let limit = super::policy::data_rate_limit_bytes(kbps);
		let bytes = cf_i64(limit);
		let secs = cf_f64(1.0);
		let vals = [bytes.0, secs.0];
		// SAFETY: two CFNumbers into a CF-typed array.
		let arr = Cf(unsafe { CFArrayCreate(std::ptr::null(), vals.as_ptr(), 2, &kCFTypeArrayCallBacks) });
		self.set(c"kVTCompressionPropertyKey_DataRateLimits", arr.0);
	}

	/// Encode one frame (`pixel_buffer` = a `CVPixelBufferRef`). `pts_us` is any monotonic µs
	/// clock; it comes back in [`EncodedAu::pts_us`].
	pub fn encode(&self, pixel_buffer: *mut c_void, pts_us: i64, force_keyframe: bool) -> Result<(), String> {
		let props = if force_keyframe {
			let d = Dict::new();
			d.set(c"kVTEncodeFrameOptionKey_ForceKeyFrame", cf_bool(true));
			Some(d)
		} else {
			None
		};
		let mut info = 0u32;
		// SAFETY: a live session and pixel buffer (VT retains what it keeps).
		let st = unsafe {
			VTCompressionSessionEncodeFrame(
				self.session,
				pixel_buffer,
				CMTime::new(pts_us, 1_000_000),
				CMTime::new(1, self.spec.fps.max(1) as i32),
				props.as_ref().map_or(std::ptr::null(), |d| d.as_ptr()),
				std::ptr::null_mut(),
				&mut info,
			)
		};
		if st != 0 {
			return Err(format!("VTCompressionSessionEncodeFrame failed ({st})"));
		}
		Ok(())
	}

	/// Encoder errors and dropped frames so far.
	pub fn errors(&self) -> u32 {
		// SAFETY: `ctx` lives as long as `self`.
		unsafe { (*self.ctx).errors.load(std::sync::atomic::Ordering::Relaxed) }
	}
}

/// Flush and tear down a session that never became (or no longer is) a [`VtEncoder`]; the
/// output context it pointed to is left alone.
unsafe fn discard_session(session: VTSessionRef) {
	VTCompressionSessionCompleteFrames(session, CMTime::invalid());
	VTCompressionSessionInvalidate(session);
	CFRelease(session as CFTypeRef);
}

impl Drop for VtEncoder {
	fn drop(&mut self) {
		// SAFETY: flush pending frames (their callbacks still see `ctx`), then tear down.
		unsafe {
			VTCompressionSessionCompleteFrames(self.session, CMTime::invalid());
			VTCompressionSessionInvalidate(self.session);
			CFRelease(self.session as CFTypeRef);
			drop(Box::from_raw(self.ctx));
		}
	}
}
