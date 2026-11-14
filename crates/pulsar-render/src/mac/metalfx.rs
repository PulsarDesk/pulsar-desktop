//! MetalFX spatial upscaling (`MTLFXSpatialScaler`, macOS 13+) — the macOS analogue of the
//! driver video super resolution the Windows renderer hands `upscale ai|auto` to.
//!
//! MetalFX is NOT linked: the framework is opened with `dlopen` and its classes are looked up
//! and messaged through the Objective-C runtime, so the binary still starts on macOS 12 and
//! older (no MetalFX there → FSR). `+[MTLFXSpatialScalerDescriptor supportsDevice:]` rules out
//! GPUs without support (e.g. older Intel Macs), and a scaler that cannot be created makes the
//! caller fall back to FSR for the rest of the session.

use std::ffi::{c_char, c_void};
use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, ProtocolObject};
use objc2::{msg_send, sel};
use objc2_metal::{MTLCommandBuffer, MTLDevice, MTLPixelFormat, MTLTexture, MTLTextureUsage};

/// `MTLFXSpatialScalerColorProcessingModePerceptual`: the input holds gamma-encoded (sRGB-like)
/// values in a non-sRGB texture, as the video pass writes them.
const COLOR_PROCESSING_PERCEPTUAL: isize = 0;

/// The descriptor class, once MetalFX is loaded (None = no MetalFX on this system).
fn descriptor_class() -> Option<&'static AnyClass> {
	static CLASS: OnceLock<Option<&'static AnyClass>> = OnceLock::new();
	*CLASS.get_or_init(|| {
		extern "C" {
			fn dlopen(path: *const c_char, mode: i32) -> *mut c_void;
		}
		const RTLD_LAZY: i32 = 0x1;
		// SAFETY: a system framework path; the handle stays open for the process lifetime.
		let h = unsafe { dlopen(c"/System/Library/Frameworks/MetalFX.framework/MetalFX".as_ptr(), RTLD_LAZY) };
		if h.is_null() {
			return None;
		}
		AnyClass::get(c"MTLFXSpatialScalerDescriptor")
	})
}

/// Whether MetalFX spatial scaling can run on `device`.
pub fn supported(device: &ProtocolObject<dyn MTLDevice>) -> bool {
	let Some(cls) = descriptor_class() else { return false };
	if cls.class_method(sel!(supportsDevice:)).is_none() {
		// macOS 13.0 has the class but not yet the query: try to create a scaler instead.
		return true;
	}
	// SAFETY: `+supportsDevice:(id<MTLDevice>) -> BOOL`.
	unsafe { msg_send![cls, supportsDevice: device] }
}

/// One spatial scaler for a fixed input → output size.
pub struct SpatialScaler {
	obj: Retained<AnyObject>,
	/// (input w, input h, output w, output h) in pixels.
	pub dims: (u32, u32, u32, u32),
	/// The usages the input / output textures must have.
	pub color_usage: MTLTextureUsage,
	pub output_usage: MTLTextureUsage,
}

impl SpatialScaler {
	/// A scaler from `dims.0 × dims.1` to `dims.2 × dims.3`, both textures in `format`.
	pub fn new(device: &ProtocolObject<dyn MTLDevice>, dims: (u32, u32, u32, u32), format: MTLPixelFormat) -> Option<Self> {
		let cls = descriptor_class()?;
		// SAFETY: the MTLFXSpatialScalerDescriptor / MTLFXSpatialScaler API (MetalFX.h): property
		// setters taking MTLPixelFormat / NSUInteger / NSInteger, `-newSpatialScalerWithDevice:`
		// returning a +1 object or nil, the usage getters returning MTLTextureUsage.
		unsafe {
			let desc: Option<Retained<AnyObject>> = msg_send![cls, new];
			let desc = desc?;
			let _: () = msg_send![&*desc, setColorTextureFormat: format];
			let _: () = msg_send![&*desc, setOutputTextureFormat: format];
			let _: () = msg_send![&*desc, setInputWidth: dims.0 as usize];
			let _: () = msg_send![&*desc, setInputHeight: dims.1 as usize];
			let _: () = msg_send![&*desc, setOutputWidth: dims.2 as usize];
			let _: () = msg_send![&*desc, setOutputHeight: dims.3 as usize];
			let _: () = msg_send![&*desc, setColorProcessingMode: COLOR_PROCESSING_PERCEPTUAL];
			let obj: Option<Retained<AnyObject>> = msg_send![&*desc, newSpatialScalerWithDevice: device];
			let obj = obj?;
			let color_usage: MTLTextureUsage = msg_send![&*obj, colorTextureUsage];
			let output_usage: MTLTextureUsage = msg_send![&*obj, outputTextureUsage];
			Some(Self { obj, dims, color_usage, output_usage })
		}
	}

	/// Scale `color` (input size) into `out` (output size, private storage) on `cb`.
	pub fn encode(
		&self,
		cb: &ProtocolObject<dyn MTLCommandBuffer>,
		color: &ProtocolObject<dyn MTLTexture>,
		out: &ProtocolObject<dyn MTLTexture>,
	) {
		// SAFETY: textures created with the scaler's required usages and sizes (see metal.rs);
		// `-encodeToCommandBuffer:` retains what it needs until the buffer completes.
		unsafe {
			let _: () = msg_send![&*self.obj, setColorTexture: color];
			let _: () = msg_send![&*self.obj, setOutputTexture: out];
			let _: () = msg_send![&*self.obj, setInputContentWidth: self.dims.0 as usize];
			let _: () = msg_send![&*self.obj, setInputContentHeight: self.dims.1 as usize];
			let _: () = msg_send![&*self.obj, encodeToCommandBuffer: cb];
		}
	}
}
