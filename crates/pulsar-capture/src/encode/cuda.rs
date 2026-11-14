//! The few CUDA driver API calls the Linux NVENC encoder needs, runtime-loaded from
//! `libcuda.so.1` (the NVIDIA driver's own library — no CUDA toolkit, nothing linked at build
//! time). A machine without the NVIDIA driver gets an `Err` and the host keeps libavcodec.
//!
//! NVENC on Linux opens its session on a CUDA context (`NV_ENC_DEVICE_TYPE_CUDA`) and reads
//! its input from CUDA device memory (`NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR`): the encoder
//! allocates one pitched NV12 buffer here and copies each captured frame into it
//! (`cuMemcpy2D`, host → device).

#![allow(non_camel_case_types, non_snake_case)]

use std::ffi::{c_char, c_int, c_uint, c_void, CStr};
use std::sync::{Arc, OnceLock};

pub(crate) type CUresult = c_int;
pub(crate) type CUdevice = c_int;
pub(crate) type CUcontext = *mut c_void;
/// `CUdeviceptr` is `unsigned long long` on every 64-bit target.
pub(crate) type CUdeviceptr = u64;

pub(crate) const CUDA_SUCCESS: CUresult = 0;
/// `CUmemorytype`.
pub(crate) const CU_MEMORYTYPE_HOST: c_uint = 1;
pub(crate) const CU_MEMORYTYPE_DEVICE: c_uint = 2;

/// `CUDA_MEMCPY2D` (cuda.h). `CUmemorytype` is a C enum (4 bytes); `repr(C)` pads it to the
/// pointer that follows, as the C compiler does.
#[repr(C)]
pub(crate) struct CUDA_MEMCPY2D {
	pub srcXInBytes: usize,
	pub srcY: usize,
	pub srcMemoryType: c_uint,
	pub srcHost: *const c_void,
	pub srcDevice: CUdeviceptr,
	pub srcArray: *mut c_void,
	pub srcPitch: usize,
	pub dstXInBytes: usize,
	pub dstY: usize,
	pub dstMemoryType: c_uint,
	pub dstHost: *mut c_void,
	pub dstDevice: CUdeviceptr,
	pub dstArray: *mut c_void,
	pub dstPitch: usize,
	pub WidthInBytes: usize,
	pub Height: usize,
}
#[cfg(target_pointer_width = "64")]
const _: () = assert!(core::mem::size_of::<CUDA_MEMCPY2D>() == 128);
#[cfg(target_pointer_width = "64")]
const _: () = assert!(core::mem::offset_of!(CUDA_MEMCPY2D, srcHost) == 24);
#[cfg(target_pointer_width = "64")]
const _: () = assert!(core::mem::offset_of!(CUDA_MEMCPY2D, dstHost) == 80);

type FnInit = unsafe extern "C" fn(c_uint) -> CUresult;
type FnDeviceGetCount = unsafe extern "C" fn(*mut c_int) -> CUresult;
type FnDeviceGet = unsafe extern "C" fn(*mut CUdevice, c_int) -> CUresult;
type FnDeviceGetName = unsafe extern "C" fn(*mut c_char, c_int, CUdevice) -> CUresult;
type FnCtxCreate = unsafe extern "C" fn(*mut CUcontext, c_uint, CUdevice) -> CUresult;
type FnCtxDestroy = unsafe extern "C" fn(CUcontext) -> CUresult;
type FnCtxPush = unsafe extern "C" fn(CUcontext) -> CUresult;
type FnCtxPop = unsafe extern "C" fn(*mut CUcontext) -> CUresult;
type FnMemAllocPitch = unsafe extern "C" fn(*mut CUdeviceptr, *mut usize, usize, usize, c_uint) -> CUresult;
type FnMemFree = unsafe extern "C" fn(CUdeviceptr) -> CUresult;
type FnMemcpy2D = unsafe extern "C" fn(*const CUDA_MEMCPY2D) -> CUresult;
type FnGetErrorString = unsafe extern "C" fn(CUresult, *mut *const c_char) -> CUresult;

/// The loaded driver API (one per process; `cuInit` done).
pub(crate) struct Cuda {
	_lib: libloading::Library,
	device_get_count: FnDeviceGetCount,
	device_get: FnDeviceGet,
	device_get_name: FnDeviceGetName,
	ctx_create: FnCtxCreate,
	ctx_destroy: FnCtxDestroy,
	ctx_push: FnCtxPush,
	ctx_pop: FnCtxPop,
	mem_alloc_pitch: FnMemAllocPitch,
	mem_free: FnMemFree,
	memcpy_2d: FnMemcpy2D,
	get_error_string: Option<FnGetErrorString>,
}

// The fn pointers and the library handle are process-global; CUDA's driver API is thread-safe.
unsafe impl Send for Cuda {}
unsafe impl Sync for Cuda {}

impl Cuda {
	/// `libcuda.so.1`, loaded and initialised once per process.
	pub(crate) fn get() -> Result<Arc<Cuda>, String> {
		static CUDA: OnceLock<Result<Arc<Cuda>, String>> = OnceLock::new();
		CUDA.get_or_init(|| unsafe { Self::load() }.map(Arc::new)).clone()
	}

	unsafe fn load() -> Result<Cuda, String> {
		let lib = libloading::Library::new("libcuda.so.1").map_err(|e| format!("libcuda.so.1: {e}"))?;
		macro_rules! sym {
			($t:ty, $name:literal) => {
				*lib.get::<$t>(concat!($name, "\0").as_bytes()).map_err(|e| format!("libcuda {}: {e}", $name))?
			};
		}
		let init: FnInit = sym!(FnInit, "cuInit");
		let get_error_string: Option<FnGetErrorString> =
			lib.get::<FnGetErrorString>(b"cuGetErrorString\0").ok().map(|s| *s);
		let cuda = Cuda {
			device_get_count: sym!(FnDeviceGetCount, "cuDeviceGetCount"),
			device_get: sym!(FnDeviceGet, "cuDeviceGet"),
			device_get_name: sym!(FnDeviceGetName, "cuDeviceGetName"),
			ctx_create: sym!(FnCtxCreate, "cuCtxCreate_v2"),
			ctx_destroy: sym!(FnCtxDestroy, "cuCtxDestroy_v2"),
			ctx_push: sym!(FnCtxPush, "cuCtxPushCurrent_v2"),
			ctx_pop: sym!(FnCtxPop, "cuCtxPopCurrent_v2"),
			mem_alloc_pitch: sym!(FnMemAllocPitch, "cuMemAllocPitch_v2"),
			mem_free: sym!(FnMemFree, "cuMemFree_v2"),
			memcpy_2d: sym!(FnMemcpy2D, "cuMemcpy2D_v2"),
			get_error_string,
			_lib: lib,
		};
		cuda.check(init(0), "cuInit")?;
		Ok(cuda)
	}

	/// `Ok` on `CUDA_SUCCESS`, else the call name with the driver's message.
	pub(crate) fn check(&self, r: CUresult, what: &str) -> Result<(), String> {
		if r == CUDA_SUCCESS {
			return Ok(());
		}
		let mut msg: *const c_char = std::ptr::null();
		let text = match self.get_error_string {
			Some(f) if unsafe { f(r, &mut msg) } == CUDA_SUCCESS && !msg.is_null() => {
				unsafe { CStr::from_ptr(msg) }.to_string_lossy().into_owned()
			}
			_ => String::new(),
		};
		Err(format!("{what}: CUDA error {r} {text}").trim_end().to_string())
	}

	/// Number of CUDA devices (NVIDIA GPUs the driver exposes).
	pub(crate) fn device_count(&self) -> Result<i32, String> {
		let mut n: c_int = 0;
		self.check(unsafe { (self.device_get_count)(&mut n) }, "cuDeviceGetCount")?;
		Ok(n)
	}
}

/// A CUDA context on one device. Not current on any thread between calls: every user makes it
/// current for the span of its work with [`Context::push`].
pub(crate) struct Context {
	cuda: Arc<Cuda>,
	ctx: CUcontext,
	pub(crate) name: String,
}

// The context handle may move between threads; it is made current where it is used.
unsafe impl Send for Context {}

impl Context {
	/// A context on CUDA device `ordinal`.
	pub(crate) fn create(cuda: Arc<Cuda>, ordinal: i32) -> Result<Context, String> {
		unsafe {
			let mut dev: CUdevice = 0;
			cuda.check((cuda.device_get)(&mut dev, ordinal), "cuDeviceGet")?;
			let mut name = [0 as c_char; 128];
			let name = if (cuda.device_get_name)(name.as_mut_ptr(), name.len() as c_int, dev) == CUDA_SUCCESS {
				CStr::from_ptr(name.as_ptr()).to_string_lossy().into_owned()
			} else {
				format!("CUDA device {ordinal}")
			};
			let mut ctx: CUcontext = std::ptr::null_mut();
			cuda.check((cuda.ctx_create)(&mut ctx, 0, dev), "cuCtxCreate")?;
			// cuCtxCreate leaves the new context current on this thread: pop it, so the
			// thread's context stack is as it was.
			let mut popped: CUcontext = std::ptr::null_mut();
			let _ = (cuda.ctx_pop)(&mut popped);
			Ok(Context { cuda, ctx, name })
		}
	}

	/// The raw `CUcontext` (what `nvEncOpenEncodeSessionEx` takes as its device).
	pub(crate) fn raw(&self) -> CUcontext {
		self.ctx
	}

	/// Make the context current on this thread until the guard drops.
	pub(crate) fn push(&self) -> Result<Current, String> {
		self.cuda.check(unsafe { (self.cuda.ctx_push)(self.ctx) }, "cuCtxPushCurrent")?;
		Ok(Current(self.cuda.clone()))
	}

	/// A pitched device allocation of `rows` rows of `width_bytes` (the context must be
	/// current). Returns the pointer and the row pitch.
	pub(crate) fn alloc_pitch(&self, width_bytes: usize, rows: usize) -> Result<(CUdeviceptr, usize), String> {
		let mut ptr: CUdeviceptr = 0;
		let mut pitch: usize = 0;
		self.cuda.check(
			unsafe { (self.cuda.mem_alloc_pitch)(&mut ptr, &mut pitch, width_bytes, rows, 16) },
			"cuMemAllocPitch",
		)?;
		Ok((ptr, pitch))
	}

	/// Free an allocation of [`alloc_pitch`] (the context must be current).
	pub(crate) fn free(&self, ptr: CUdeviceptr) {
		if ptr != 0 {
			let _ = unsafe { (self.cuda.mem_free)(ptr) };
		}
	}

	/// Copy `rows` rows of `width_bytes` from host memory (`src`, row pitch `src_pitch`) to
	/// device memory at `dst` (row pitch `dst_pitch`). Synchronous. The context must be current.
	pub(crate) unsafe fn copy_host_to_device(
		&self,
		src: *const u8,
		src_pitch: usize,
		dst: CUdeviceptr,
		dst_pitch: usize,
		width_bytes: usize,
		rows: usize,
	) -> Result<(), String> {
		let c = CUDA_MEMCPY2D {
			srcXInBytes: 0,
			srcY: 0,
			srcMemoryType: CU_MEMORYTYPE_HOST,
			srcHost: src as *const c_void,
			srcDevice: 0,
			srcArray: std::ptr::null_mut(),
			srcPitch: src_pitch,
			dstXInBytes: 0,
			dstY: 0,
			dstMemoryType: CU_MEMORYTYPE_DEVICE,
			dstHost: std::ptr::null_mut(),
			dstDevice: dst,
			dstArray: std::ptr::null_mut(),
			dstPitch: dst_pitch,
			WidthInBytes: width_bytes,
			Height: rows,
		};
		self.cuda.check((self.cuda.memcpy_2d)(&c), "cuMemcpy2D")
	}
}

impl Drop for Context {
	fn drop(&mut self) {
		if !self.ctx.is_null() {
			let _ = unsafe { (self.cuda.ctx_destroy)(self.ctx) };
			self.ctx = std::ptr::null_mut();
		}
	}
}

/// The context is current on this thread while this lives (see [`Context::push`]).
pub(crate) struct Current(Arc<Cuda>);

impl Drop for Current {
	fn drop(&mut self) {
		let mut popped: CUcontext = std::ptr::null_mut();
		let _ = unsafe { (self.0.ctx_pop)(&mut popped) };
	}
}
