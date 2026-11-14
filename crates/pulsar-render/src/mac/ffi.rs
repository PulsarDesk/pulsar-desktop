//! Raw bindings to the plain-C macOS frameworks the renderer uses: CoreFoundation, CoreMedia,
//! CoreVideo (pixel buffers, the Metal texture cache, the display link), VideoToolbox and the
//! CoreGraphics window list. Hand-written instead of the objc2-* framework crates so every
//! call site works on plain pointers (the decode path owns CF objects by hand) — and only the
//! symbols listed here are linked.

#![allow(non_upper_case_globals, non_snake_case, dead_code)]

use std::ffi::c_void;

pub type CFTypeRef = *const c_void;
pub type CFAllocatorRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CFMutableDictionaryRef = *mut c_void;
pub type CFArrayRef = *const c_void;
pub type CFNumberRef = *const c_void;
pub type CFBooleanRef = *const c_void;
pub type CFDataRef = *const c_void;
pub type CFIndex = isize;
pub type Boolean = u8;
pub type OSStatus = i32;
pub type CVReturn = i32;

pub type CMFormatDescriptionRef = *const c_void;
pub type CMBlockBufferRef = *mut c_void;
pub type CMSampleBufferRef = *mut c_void;
pub type CVImageBufferRef = *mut c_void;
pub type CVPixelBufferRef = *mut c_void;
pub type CVMetalTextureCacheRef = *mut c_void;
pub type CVMetalTextureRef = *mut c_void;
pub type CVDisplayLinkRef = *mut c_void;
pub type VTDecompressionSessionRef = *mut c_void;
pub type CGDirectDisplayID = u32;
pub type CGWindowID = u32;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct CGPoint {
	pub x: f64,
	pub y: f64,
}
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct CGSize {
	pub width: f64,
	pub height: f64,
}
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct CGRect {
	pub origin: CGPoint,
	pub size: CGSize,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CMTime {
	pub value: i64,
	pub timescale: i32,
	pub flags: u32,
	pub epoch: i64,
}
pub const kCMTimeFlags_Valid: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CMSampleTimingInfo {
	pub duration: CMTime,
	pub presentationTimeStamp: CMTime,
	pub decodeTimeStamp: CMTime,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CMVideoDimensions {
	pub width: i32,
	pub height: i32,
}

pub type VTDecompressionOutputCallback = unsafe extern "C" fn(
	refcon: *mut c_void,
	source_frame_refcon: *mut c_void,
	status: OSStatus,
	info_flags: u32,
	image: CVImageBufferRef,
	pts: CMTime,
	duration: CMTime,
);

#[repr(C)]
pub struct VTDecompressionOutputCallbackRecord {
	pub callback: Option<VTDecompressionOutputCallback>,
	pub refcon: *mut c_void,
}

#[repr(C)]
pub struct CVTimeStamp {
	pub version: u32,
	pub videoTimeScale: i32,
	pub videoTime: i64,
	pub hostTime: u64,
	pub rateScalar: f64,
	pub videoRefreshPeriod: i64,
	pub smpteTime: [u8; 24],
	pub flags: u64,
	pub reserved: u64,
}

pub type CVDisplayLinkOutputCallback = unsafe extern "C" fn(
	link: CVDisplayLinkRef,
	now: *const CVTimeStamp,
	output_time: *const CVTimeStamp,
	flags_in: u64,
	flags_out: *mut u64,
	ctx: *mut c_void,
) -> CVReturn;

/// Opaque CFDictionary callback tables (only their addresses are used).
#[repr(C)]
pub struct CFDictionaryKeyCallBacks {
	_p: [usize; 6],
}
#[repr(C)]
pub struct CFDictionaryValueCallBacks {
	_p: [usize; 5],
}
#[repr(C)]
pub struct CFArrayCallBacks {
	_p: [usize; 5],
}

pub const kCFNumberSInt32Type: CFIndex = 3;
pub const kCFNumberSInt64Type: CFIndex = 4;
pub const kCFStringEncodingUTF8: u32 = 0x0800_0100;

pub const fn fourcc(s: &[u8; 4]) -> u32 {
	((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | s[3] as u32
}
pub const kCMVideoCodecType_H264: u32 = fourcc(b"avc1");
pub const kCMVideoCodecType_HEVC: u32 = fourcc(b"hvc1");
pub const kCMVideoCodecType_AV1: u32 = fourcc(b"av01");
/// NV12 video range / full range, P010 (10-bit, MSB-aligned) video range / full range.
pub const PIX_420V: u32 = fourcc(b"420v");
pub const PIX_420F: u32 = fourcc(b"420f");
pub const PIX_X420: u32 = fourcc(b"x420");
pub const PIX_XF20: u32 = fourcc(b"xf20");

pub const kCMBlockBufferAssureMemoryNowFlag: u32 = 1;
pub const kVTInvalidSessionErr: OSStatus = -12903;
pub const kVTVideoDecoderBadDataErr: OSStatus = -12909;
pub const kVTVideoDecoderMalfunctionErr: OSStatus = -12911;
pub const kVTDecodeInfo_FrameDropped: u32 = 1 << 1;

pub const kCGWindowListOptionOnScreenAboveWindow: u32 = 1 << 1;
pub const kCGWindowListOptionIncludingWindow: u32 = 1 << 3;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
	pub static kCFAllocatorDefault: CFAllocatorRef;
	pub static kCFTypeDictionaryKeyCallBacks: CFDictionaryKeyCallBacks;
	pub static kCFTypeDictionaryValueCallBacks: CFDictionaryValueCallBacks;
	pub static kCFTypeArrayCallBacks: CFArrayCallBacks;
	pub static kCFBooleanTrue: CFBooleanRef;
	pub static kCFBooleanFalse: CFBooleanRef;
	pub fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
	pub fn CFRelease(cf: CFTypeRef);
	pub fn CFEqual(a: CFTypeRef, b: CFTypeRef) -> Boolean;
	pub fn CFGetTypeID(cf: CFTypeRef) -> usize;
	pub fn CFBooleanGetTypeID() -> usize;
	pub fn CFBooleanGetValue(b: CFBooleanRef) -> Boolean;
	pub fn CFNumberCreate(alloc: CFAllocatorRef, kind: CFIndex, value: *const c_void) -> CFNumberRef;
	pub fn CFNumberGetValue(n: CFNumberRef, kind: CFIndex, out: *mut c_void) -> Boolean;
	pub fn CFDataCreate(alloc: CFAllocatorRef, bytes: *const u8, len: CFIndex) -> CFDataRef;
	pub fn CFStringCreateWithCString(alloc: CFAllocatorRef, s: *const u8, enc: u32) -> CFStringRef;
	pub fn CFDictionaryCreateMutable(
		alloc: CFAllocatorRef,
		cap: CFIndex,
		kcb: *const CFDictionaryKeyCallBacks,
		vcb: *const CFDictionaryValueCallBacks,
	) -> CFMutableDictionaryRef;
	pub fn CFDictionarySetValue(d: CFMutableDictionaryRef, k: *const c_void, v: *const c_void);
	pub fn CFDictionaryGetValue(d: CFDictionaryRef, k: *const c_void) -> *const c_void;
	pub fn CFArrayCreate(
		alloc: CFAllocatorRef,
		values: *const *const c_void,
		n: CFIndex,
		cb: *const CFArrayCallBacks,
	) -> CFArrayRef;
	pub fn CFArrayGetCount(a: CFArrayRef) -> CFIndex;
	pub fn CFArrayGetValueAtIndex(a: CFArrayRef, i: CFIndex) -> *const c_void;
}

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
	pub static kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms: CFStringRef;
	pub fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
		alloc: CFAllocatorRef,
		count: usize,
		ptrs: *const *const u8,
		sizes: *const usize,
		nal_header_len: i32,
		out: *mut CMFormatDescriptionRef,
	) -> OSStatus;
	pub fn CMVideoFormatDescriptionCreateFromHEVCParameterSets(
		alloc: CFAllocatorRef,
		count: usize,
		ptrs: *const *const u8,
		sizes: *const usize,
		nal_header_len: i32,
		extensions: CFDictionaryRef,
		out: *mut CMFormatDescriptionRef,
	) -> OSStatus;
	pub fn CMVideoFormatDescriptionCreate(
		alloc: CFAllocatorRef,
		codec: u32,
		width: i32,
		height: i32,
		extensions: CFDictionaryRef,
		out: *mut CMFormatDescriptionRef,
	) -> OSStatus;
	pub fn CMVideoFormatDescriptionGetDimensions(d: CMFormatDescriptionRef) -> CMVideoDimensions;
	pub fn CMBlockBufferCreateWithMemoryBlock(
		alloc: CFAllocatorRef,
		memory: *mut c_void,
		block_len: usize,
		block_alloc: CFAllocatorRef,
		custom_source: *const c_void,
		offset: usize,
		data_len: usize,
		flags: u32,
		out: *mut CMBlockBufferRef,
	) -> OSStatus;
	pub fn CMBlockBufferReplaceDataBytes(
		src: *const c_void,
		dest: CMBlockBufferRef,
		offset: usize,
		len: usize,
	) -> OSStatus;
	pub fn CMSampleBufferCreateReady(
		alloc: CFAllocatorRef,
		data: CMBlockBufferRef,
		format: CMFormatDescriptionRef,
		num_samples: CFIndex,
		num_timing: CFIndex,
		timing: *const CMSampleTimingInfo,
		num_sizes: CFIndex,
		sizes: *const usize,
		out: *mut CMSampleBufferRef,
	) -> OSStatus;
}

#[link(name = "VideoToolbox", kind = "framework")]
extern "C" {
	pub static kVTDecompressionPropertyKey_RealTime: CFStringRef;
	pub static kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder: CFStringRef;
	pub static kVTDecompressionPropertyKey_MaximizePowerEfficiency: CFStringRef;
	pub static kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder: CFStringRef;
	pub fn VTDecompressionSessionCreate(
		alloc: CFAllocatorRef,
		format: CMFormatDescriptionRef,
		decoder_spec: CFDictionaryRef,
		dest_attrs: CFDictionaryRef,
		callback: *const VTDecompressionOutputCallbackRecord,
		out: *mut VTDecompressionSessionRef,
	) -> OSStatus;
	pub fn VTDecompressionSessionDecodeFrame(
		s: VTDecompressionSessionRef,
		sample: CMSampleBufferRef,
		flags: u32,
		frame_refcon: *mut c_void,
		info_out: *mut u32,
	) -> OSStatus;
	pub fn VTDecompressionSessionWaitForAsynchronousFrames(s: VTDecompressionSessionRef) -> OSStatus;
	pub fn VTDecompressionSessionCanAcceptFormatDescription(
		s: VTDecompressionSessionRef,
		format: CMFormatDescriptionRef,
	) -> Boolean;
	pub fn VTDecompressionSessionInvalidate(s: VTDecompressionSessionRef);
	pub fn VTSessionSetProperty(s: CFTypeRef, key: CFStringRef, value: CFTypeRef) -> OSStatus;
	pub fn VTSessionCopyProperty(s: CFTypeRef, key: CFStringRef, alloc: CFAllocatorRef, out: *mut c_void) -> OSStatus;
	pub fn VTIsHardwareDecodeSupported(codec: u32) -> Boolean;
}

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
	pub static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
	pub static kCVPixelBufferMetalCompatibilityKey: CFStringRef;
	pub static kCVPixelBufferIOSurfacePropertiesKey: CFStringRef;
	pub static kCVImageBufferYCbCrMatrixKey: CFStringRef;
	pub static kCVImageBufferYCbCrMatrix_ITU_R_601_4: CFStringRef;
	pub static kCVImageBufferYCbCrMatrix_ITU_R_709_2: CFStringRef;
	pub static kCVImageBufferYCbCrMatrix_ITU_R_2020: CFStringRef;
	pub fn CVPixelBufferGetWidth(pb: CVPixelBufferRef) -> usize;
	pub fn CVPixelBufferGetHeight(pb: CVPixelBufferRef) -> usize;
	pub fn CVPixelBufferGetWidthOfPlane(pb: CVPixelBufferRef, plane: usize) -> usize;
	pub fn CVPixelBufferGetHeightOfPlane(pb: CVPixelBufferRef, plane: usize) -> usize;
	pub fn CVPixelBufferGetPixelFormatType(pb: CVPixelBufferRef) -> u32;
	pub fn CVBufferGetAttachment(b: CVImageBufferRef, key: CFStringRef, mode: *mut u32) -> CFTypeRef;
	pub fn CVMetalTextureCacheCreate(
		alloc: CFAllocatorRef,
		cache_attrs: CFDictionaryRef,
		device: *mut c_void,
		tex_attrs: CFDictionaryRef,
		out: *mut CVMetalTextureCacheRef,
	) -> CVReturn;
	pub fn CVMetalTextureCacheCreateTextureFromImage(
		alloc: CFAllocatorRef,
		cache: CVMetalTextureCacheRef,
		image: CVImageBufferRef,
		tex_attrs: CFDictionaryRef,
		pixel_format: usize,
		width: usize,
		height: usize,
		plane: usize,
		out: *mut CVMetalTextureRef,
	) -> CVReturn;
	pub fn CVMetalTextureGetTexture(t: CVMetalTextureRef) -> *mut c_void;
	pub fn CVMetalTextureCacheFlush(cache: CVMetalTextureCacheRef, options: u64);
	pub fn CVDisplayLinkCreateWithActiveCGDisplays(out: *mut CVDisplayLinkRef) -> CVReturn;
	pub fn CVDisplayLinkSetOutputCallback(
		link: CVDisplayLinkRef,
		cb: Option<CVDisplayLinkOutputCallback>,
		ctx: *mut c_void,
	) -> CVReturn;
	pub fn CVDisplayLinkSetCurrentCGDisplay(link: CVDisplayLinkRef, display: CGDirectDisplayID) -> CVReturn;
	pub fn CVDisplayLinkStart(link: CVDisplayLinkRef) -> CVReturn;
	pub fn CVDisplayLinkStop(link: CVDisplayLinkRef) -> CVReturn;
	pub fn CVDisplayLinkRelease(link: CVDisplayLinkRef);
	pub fn CVDisplayLinkGetActualOutputVideoRefreshPeriod(link: CVDisplayLinkRef) -> f64;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
	pub static kCGWindowBounds: CFStringRef;
	pub static kCGWindowIsOnscreen: CFStringRef;
	pub fn CGWindowListCopyWindowInfo(option: u32, relative_to: CGWindowID) -> CFArrayRef;
	pub fn CGWindowListCreate(option: u32, relative_to: CGWindowID) -> CFArrayRef;
	pub fn CGRectMakeWithDictionaryRepresentation(d: CFDictionaryRef, out: *mut CGRect) -> bool;
	pub fn CGMainDisplayID() -> CGDirectDisplayID;
	pub fn CGDisplayBounds(d: CGDirectDisplayID) -> CGRect;
	pub fn CGGetDisplaysWithPoint(
		p: CGPoint,
		max: u32,
		displays: *mut CGDirectDisplayID,
		count: *mut u32,
	) -> i32;
	/// Retained `id<MTLDevice>` driving `display` (macOS 10.11+).
	pub fn CGDirectDisplayCopyCurrentMetalDevice(display: CGDirectDisplayID) -> *mut c_void;
}

#[link(name = "Metal", kind = "framework")]
extern "C" {}
#[link(name = "QuartzCore", kind = "framework")]
extern "C" {}

/// An owned (+1) CoreFoundation reference, released on drop.
pub struct Cf(pub CFTypeRef);

impl Cf {
	/// Take ownership of a +1 reference (a `Create` / `Copy` result). None for null.
	pub fn owned(p: CFTypeRef) -> Option<Self> {
		(!p.is_null()).then_some(Self(p))
	}
	/// Retain a +0 reference.
	pub unsafe fn retain(p: CFTypeRef) -> Option<Self> {
		(!p.is_null()).then(|| Self(CFRetain(p)))
	}
	pub fn ptr(&self) -> CFTypeRef {
		self.0
	}
}

impl Clone for Cf {
	fn clone(&self) -> Self {
		// SAFETY: `self.0` is a live CF object we hold a reference to.
		Self(unsafe { CFRetain(self.0) })
	}
}

impl Drop for Cf {
	fn drop(&mut self) {
		// SAFETY: we own one reference.
		unsafe { CFRelease(self.0) }
	}
}

// CF objects are thread-safe to retain/release; the pixel buffers we hand across threads are
// only read.
unsafe impl Send for Cf {}

pub fn cf_i32(v: i32) -> Cf {
	// SAFETY: a valid pointer to an i32 of the stated type.
	Cf(unsafe { CFNumberCreate(kCFAllocatorDefault, kCFNumberSInt32Type, &v as *const i32 as *const c_void) })
}

pub fn cf_str(s: &str) -> Cf {
	let mut b = s.as_bytes().to_vec();
	b.push(0);
	// SAFETY: NUL-terminated UTF-8.
	Cf(unsafe { CFStringCreateWithCString(kCFAllocatorDefault, b.as_ptr(), kCFStringEncodingUTF8) })
}

pub fn cf_data(b: &[u8]) -> Cf {
	// SAFETY: the slice is valid for its length; CFData copies it.
	Cf(unsafe { CFDataCreate(kCFAllocatorDefault, b.as_ptr(), b.len() as CFIndex) })
}

pub fn cf_array(items: &[&Cf]) -> Cf {
	let ptrs: Vec<*const c_void> = items.iter().map(|c| c.0).collect();
	// SAFETY: every element is a live CF object; the array retains them.
	Cf(unsafe { CFArrayCreate(kCFAllocatorDefault, ptrs.as_ptr(), ptrs.len() as CFIndex, &kCFTypeArrayCallBacks) })
}

/// A CFDictionary of CF keys → CF values (both retained by the dictionary).
pub fn cf_dict(pairs: &[(CFTypeRef, CFTypeRef)]) -> Cf {
	// SAFETY: standard CF type callbacks; keys/values are live CF objects.
	unsafe {
		let d = CFDictionaryCreateMutable(
			kCFAllocatorDefault,
			pairs.len() as CFIndex,
			&kCFTypeDictionaryKeyCallBacks,
			&kCFTypeDictionaryValueCallBacks,
		);
		for (k, v) in pairs {
			CFDictionarySetValue(d, *k, *v);
		}
		Cf(d as CFTypeRef)
	}
}

/// A CFBoolean's value (false for anything that is not a CFBoolean).
pub unsafe fn cf_bool(v: CFTypeRef) -> Option<bool> {
	if v.is_null() || CFGetTypeID(v) != CFBooleanGetTypeID() {
		return None;
	}
	Some(CFBooleanGetValue(v) != 0)
}
