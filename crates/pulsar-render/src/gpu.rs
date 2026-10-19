//! Which GPU the renderer's EGL/GL actually runs on (Linux).
//!
//! Zero-copy presentation needs the decoder and the GL context on the SAME device: a
//! dmabuf from one GPU can't be sampled by another's GL (NVIDIA ↔ AMD/Intel tiled layouts
//! don't import), and CUDA↔GL interop only exists inside an NVIDIA GL context. So the
//! decoder walk (decode.rs) asks "what is the display GPU?" and puts the decoder on that
//! device first. Works for every machine shape without naming any:
//!
//! - one GPU (AMD/Intel iGPU, AMD/NVIDIA dGPU, nouveau, virtio): that GPU, trivially;
//! - hybrid laptops (AMD/Intel iGPU + NVIDIA dGPU): whichever one EGL picked — the iGPU
//!   normally, the dGPU under PRIME render offload;
//! - unknown (no `EGL_EXT_device_query`, no X display): `None` → the old conservative
//!   order with copy-back presentation.
//!
//! Detection opens its OWN X connection + EGL display (same process, same env → the same
//! vendor EGL the window gets) so it can answer before the window's context exists — the
//! decode thread starts first — and from the headless `--probe`.

use std::ffi::{c_void, CStr};
use std::sync::OnceLock;

pub const VENDOR_NVIDIA: u16 = 0x10de;

#[derive(Debug, Default)]
pub struct DisplayGpu {
	/// `/dev/dri/renderD*` of the GPU the EGL display runs on.
	pub render_node: Option<String>,
	/// PCI vendor id of that GPU (`0x10de` NVIDIA, `0x1002` AMD, `0x8086` Intel).
	pub vendor: Option<u16>,
}

impl DisplayGpu {
	pub fn is_nvidia(&self) -> bool {
		self.vendor == Some(VENDOR_NVIDIA)
	}
}

// EGL_EXT_device_query / EGL_EXT_device_drm / EGL_EXT_device_drm_render_node.
const EGL_DEVICE_EXT: i32 = 0x322C;
const EGL_DRM_DEVICE_FILE_EXT: i32 = 0x3233;
const EGL_DRM_RENDER_NODE_FILE_EXT: i32 = 0x3377;

type QueryDisplayAttrib = unsafe extern "C" fn(*mut c_void, i32, *mut isize) -> u32;
type QueryDeviceString = unsafe extern "C" fn(*mut c_void, i32) -> *const libc::c_char;

static GPU: OnceLock<DisplayGpu> = OnceLock::new();

/// The display GPU if detection already ran (never triggers it).
pub fn known() -> Option<&'static DisplayGpu> {
	GPU.get()
}

/// The display GPU, detected once per process.
pub fn display_gpu() -> &'static DisplayGpu {
	GPU.get_or_init(|| {
		let g = unsafe { detect() }.unwrap_or_default();
		eprintln!(
			"pulsar-render: display GPU {} (vendor {})",
			g.render_node.as_deref().unwrap_or("unknown"),
			g.vendor.map_or("unknown".to_string(), |v| format!("{v:#06x}"))
		);
		g
	})
}

unsafe fn detect() -> Option<DisplayGpu> {
	use khronos_egl as egl;
	let xd = x11::xlib::XOpenDisplay(std::ptr::null());
	if xd.is_null() {
		return None;
	}
	// Kept open for the process lifetime (one X connection + EGL display): terminating
	// an EGL display while the window thread brings its own up is not worth the risk.
	let inst = egl::Instance::new(egl::Static);
	let dpy = inst.get_display(xd as egl::NativeDisplayType)?;
	inst.initialize(dpy).ok()?;
	let qa: QueryDisplayAttrib = std::mem::transmute(inst.get_proc_address("eglQueryDisplayAttribEXT")?);
	let qs: QueryDeviceString = std::mem::transmute(inst.get_proc_address("eglQueryDeviceStringEXT")?);
	let mut dev: isize = 0;
	if qa(dpy.as_ptr(), EGL_DEVICE_EXT, &mut dev) == 0 || dev == 0 {
		return None;
	}
	let get = |name: i32| {
		let s = qs(dev as *mut c_void, name);
		(!s.is_null()).then(|| CStr::from_ptr(s).to_string_lossy().into_owned())
	};
	// Prefer the render node string; older drivers only report the primary (card) node.
	let render_node = get(EGL_DRM_RENDER_NODE_FILE_EXT)
		.or_else(|| get(EGL_DRM_DEVICE_FILE_EXT).and_then(|card| render_node_of(&card)))?;
	let vendor = pci_vendor(&render_node);
	Some(DisplayGpu {
		render_node: Some(render_node),
		vendor,
	})
}

/// `/dev/dri/cardN` → the render node of the same device (`/sys/class/drm/cardN/device/drm/`).
fn render_node_of(card: &str) -> Option<String> {
	let name = std::path::Path::new(card).file_name()?.to_str()?;
	let dir = std::fs::read_dir(format!("/sys/class/drm/{name}/device/drm")).ok()?;
	dir.filter_map(|e| e.ok()?.file_name().into_string().ok())
		.find(|n| n.starts_with("renderD"))
		.map(|n| format!("/dev/dri/{n}"))
}

/// PCI vendor of a `/dev/dri/renderD*` node (`None` for non-PCI devices, e.g. SoCs).
pub fn pci_vendor(node: &str) -> Option<u16> {
	let name = std::path::Path::new(node).file_name()?.to_str()?;
	let v = std::fs::read_to_string(format!("/sys/class/drm/{name}/device/vendor")).ok()?;
	u16::from_str_radix(v.trim().trim_start_matches("0x"), 16).ok()
}
