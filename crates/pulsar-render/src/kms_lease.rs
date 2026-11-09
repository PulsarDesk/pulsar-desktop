//! DRM-lease fullscreen presentation (Linux / X11; built for the RK3588 Orange Pi 5B).
//!
//! While the session is FULLSCREEN, the renderer can skip X entirely: it leases the output's
//! CRTC + connector (+ the CRTC's planes, which the kernel adds implicitly) from the X server
//! (RandR 1.6 `CreateLease`) and drives KMS itself:
//!
//! - **Video plane** — each decoded DRM_PRIME frame (rkmpp NV12/NV15 dmabuf) is wrapped in a
//!   KMS framebuffer and put straight on an overlay-capable plane by an atomic commit. No GPU
//!   composite, no compositor copy, no X Present round-trip (the site of the multi-minute
//!   freezes under mutter). Commits are page-flip-event driven, so the loop is vblank-paced.
//! - **Overlay plane** — the egui chrome (HUD, open button, hints, host cursor) is painted by the
//!   SAME GL context into a staging texture, then copied upside-down (GL renders bottom-up,
//!   scanout reads top-down) into one of two ARGB8888 dumb buffers (imported as EGLImage →
//!   texture → FBO) shown on the primary plane above the video. The plane's own `reflect-y`
//!   can't do the flip: VOP2 Cluster windows reject rotation on linear buffers ("Unsupported
//!   linear rotation"). It is only re-rendered when egui's output changed, and the plane is
//!   disabled when nothing is drawn.
//!
//! Any failure releases the lease and the caller falls back to the normal EGL/X path. Only
//! `linux.rs` drives this module (see the `KMS-LEASE` markers there). `PULSAR_KMS_LEASE=0`
//! turns the path off.
//!
//! No build-time dependencies: libxcb / libxcb-randr are dlopen'ed and the DRM uAPI is used
//! through raw ioctls, so a system without them simply never leases.

use ffmpeg_sys_next as ff;
use glow::HasContext;
use std::ffi::c_void;
use std::os::raw::{c_char, c_int, c_ulong};
use std::ptr;
use x11::xlib;

// ---------------------------------------------------------------------------------------------
// DRM uAPI (include/uapi/drm/drm.h, drm_mode.h) — only what this module needs.
// ---------------------------------------------------------------------------------------------

const fn fourcc(c: &[u8; 4]) -> u32 {
	(c[0] as u32) | (c[1] as u32) << 8 | (c[2] as u32) << 16 | (c[3] as u32) << 24
}
const DRM_FORMAT_ARGB8888: u32 = fourcc(b"AR24");
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

const DRM_CLIENT_CAP_UNIVERSAL_PLANES: u64 = 2;
const DRM_CLIENT_CAP_ATOMIC: u64 = 3;
const DRM_MODE_OBJECT_PLANE: u32 = 0xeeee_eeee;
const DRM_MODE_FB_MODIFIERS: u32 = 2;
const DRM_MODE_PAGE_FLIP_EVENT: u32 = 0x01;
const DRM_MODE_ATOMIC_TEST_ONLY: u32 = 0x0100;
const DRM_MODE_ATOMIC_NONBLOCK: u32 = 0x0200;
const DRM_EVENT_FLIP_COMPLETE: u32 = 0x02;
const DRM_PLANE_TYPE_PRIMARY: u64 = 1;
const ROTATE_0: u64 = 0x1;

#[repr(C)]
struct SetClientCap {
	capability: u64,
	value: u64,
}
#[repr(C)]
struct GemClose {
	handle: u32,
	pad: u32,
}
#[repr(C)]
struct PrimeHandle {
	handle: u32,
	flags: u32,
	fd: i32,
}
#[repr(C)]
struct CardRes {
	fb_id_ptr: u64,
	crtc_id_ptr: u64,
	connector_id_ptr: u64,
	encoder_id_ptr: u64,
	count_fbs: u32,
	count_crtcs: u32,
	count_connectors: u32,
	count_encoders: u32,
	min_width: u32,
	max_width: u32,
	min_height: u32,
	max_height: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct ModeInfo {
	clock: u32,
	hdisplay: u16,
	hsync_start: u16,
	hsync_end: u16,
	htotal: u16,
	hskew: u16,
	vdisplay: u16,
	vsync_start: u16,
	vsync_end: u16,
	vtotal: u16,
	vscan: u16,
	vrefresh: u32,
	flags: u32,
	type_: u32,
	name: [u8; 32],
}
#[repr(C)]
struct ModeCrtc {
	set_connectors_ptr: u64,
	count_connectors: u32,
	crtc_id: u32,
	fb_id: u32,
	x: u32,
	y: u32,
	gamma_size: u32,
	mode_valid: u32,
	mode: ModeInfo,
}
#[repr(C)]
struct GetPlaneRes {
	plane_id_ptr: u64,
	count_planes: u32,
}
#[repr(C)]
struct GetPlane {
	plane_id: u32,
	crtc_id: u32,
	fb_id: u32,
	possible_crtcs: u32,
	gamma_size: u32,
	count_format_types: u32,
	format_type_ptr: u64,
}
#[repr(C)]
struct FbCmd2 {
	fb_id: u32,
	width: u32,
	height: u32,
	pixel_format: u32,
	flags: u32,
	handles: [u32; 4],
	pitches: [u32; 4],
	offsets: [u32; 4],
	modifier: [u64; 4],
}
#[repr(C)]
struct ObjGetProps {
	props_ptr: u64,
	prop_values_ptr: u64,
	count_props: u32,
	obj_id: u32,
	obj_type: u32,
}
#[repr(C)]
struct GetProperty {
	values_ptr: u64,
	enum_blob_ptr: u64,
	prop_id: u32,
	flags: u32,
	name: [u8; 32],
	count_values: u32,
	count_enum_blobs: u32,
}
#[repr(C)]
struct CreateDumb {
	height: u32,
	width: u32,
	bpp: u32,
	flags: u32,
	handle: u32,
	pitch: u32,
	size: u64,
}
#[repr(C)]
struct DestroyDumb {
	handle: u32,
}
#[repr(C)]
struct AtomicReq {
	flags: u32,
	count_objs: u32,
	objs_ptr: u64,
	count_props_ptr: u64,
	props_ptr: u64,
	prop_values_ptr: u64,
	reserved: u64,
	user_data: u64,
}

const fn ioc(dir: u32, nr: u32, size: usize) -> c_ulong {
	((dir << 30) | ((size as u32) << 16) | (0x64 << 8) | nr) as c_ulong
}
const fn iowr<T>(nr: u32) -> c_ulong {
	ioc(3, nr, std::mem::size_of::<T>())
}
const fn iow<T>(nr: u32) -> c_ulong {
	ioc(1, nr, std::mem::size_of::<T>())
}
const IOCTL_GEM_CLOSE: c_ulong = iow::<GemClose>(0x09);
const IOCTL_SET_CLIENT_CAP: c_ulong = iow::<SetClientCap>(0x0d);
const IOCTL_PRIME_HANDLE_TO_FD: c_ulong = iowr::<PrimeHandle>(0x2d);
const IOCTL_PRIME_FD_TO_HANDLE: c_ulong = iowr::<PrimeHandle>(0x2e);
const IOCTL_MODE_GETRESOURCES: c_ulong = iowr::<CardRes>(0xA0);
const IOCTL_MODE_GETCRTC: c_ulong = iowr::<ModeCrtc>(0xA1);
const IOCTL_MODE_GETPROPERTY: c_ulong = iowr::<GetProperty>(0xAA);
const IOCTL_MODE_RMFB: c_ulong = iowr::<u32>(0xAF);
const IOCTL_MODE_CREATE_DUMB: c_ulong = iowr::<CreateDumb>(0xB2);
const IOCTL_MODE_DESTROY_DUMB: c_ulong = iowr::<DestroyDumb>(0xB4);
const IOCTL_MODE_GETPLANERESOURCES: c_ulong = iowr::<GetPlaneRes>(0xB5);
const IOCTL_MODE_GETPLANE: c_ulong = iowr::<GetPlane>(0xB6);
const IOCTL_MODE_ADDFB2: c_ulong = iowr::<FbCmd2>(0xB8);
const IOCTL_MODE_OBJ_GETPROPERTIES: c_ulong = iowr::<ObjGetProps>(0xB9);
const IOCTL_MODE_ATOMIC: c_ulong = iowr::<AtomicReq>(0xBC);

/// ioctl with EINTR retry; Err = errno.
unsafe fn io<T>(fd: c_int, req: c_ulong, arg: &mut T) -> Result<(), i32> {
	loop {
		let r = libc::ioctl(fd, req as _, arg as *mut T as *mut c_void);
		if r == 0 {
			return Ok(());
		}
		let e = *libc::__errno_location();
		if e != libc::EINTR {
			return Err(e);
		}
	}
}

fn errstr(e: i32) -> String {
	std::io::Error::from_raw_os_error(e).to_string()
}

/// (name, prop id, current value) of every property of a KMS object.
unsafe fn obj_props(fd: c_int, obj: u32, ty: u32) -> Result<Vec<(String, u32, u64)>, String> {
	let mut g: ObjGetProps = std::mem::zeroed();
	g.obj_id = obj;
	g.obj_type = ty;
	io(fd, IOCTL_MODE_OBJ_GETPROPERTIES, &mut g).map_err(|e| format!("getproperties: {}", errstr(e)))?;
	let n = g.count_props as usize;
	let mut ids = vec![0u32; n];
	let mut vals = vec![0u64; n];
	g.props_ptr = ids.as_mut_ptr() as u64;
	g.prop_values_ptr = vals.as_mut_ptr() as u64;
	io(fd, IOCTL_MODE_OBJ_GETPROPERTIES, &mut g).map_err(|e| format!("getproperties: {}", errstr(e)))?;
	let mut out = Vec::with_capacity(n);
	for i in 0..n.min(g.count_props as usize) {
		let mut p: GetProperty = std::mem::zeroed();
		p.prop_id = ids[i];
		if io(fd, IOCTL_MODE_GETPROPERTY, &mut p).is_err() {
			continue;
		}
		let len = p.name.iter().position(|&c| c == 0).unwrap_or(p.name.len());
		out.push((String::from_utf8_lossy(&p.name[..len]).into_owned(), ids[i], vals[i]));
	}
	Ok(out)
}

struct Plane {
	id: u32,
	fb_id: u32,
	formats: Vec<u32>,
	props: Vec<(String, u32, u64)>,
}

impl Plane {
	fn prop(&self, name: &str) -> Option<u32> {
		self.props.iter().find(|p| p.0 == name).map(|p| p.1)
	}
	fn val(&self, name: &str) -> Option<u64> {
		self.props.iter().find(|p| p.0 == name).map(|p| p.2)
	}
}

/// Property writes of one atomic commit, (object, property, value).
#[derive(Default)]
struct Req(Vec<(u32, u32, u64)>);

impl Req {
	fn set(&mut self, p: &Plane, name: &str, v: u64) -> Result<(), String> {
		let id = p.prop(name).ok_or_else(|| format!("plane {} has no '{name}' property", p.id))?;
		self.0.push((p.id, id, v));
		Ok(())
	}
	/// Like `set`, but a missing property is skipped (zpos / rotation are optional).
	fn opt(&mut self, p: &Plane, name: &str, v: u64) {
		if let Some(id) = p.prop(name) {
			self.0.push((p.id, id, v));
		}
	}
	/// Put `fb` on `p`: source rect in pixels → 16.16, destination rect on the CRTC.
	fn place(
		&mut self,
		p: &Plane,
		crtc: u32,
		fb: u32,
		src: (u32, u32, u32, u32),
		dst: (i32, i32, u32, u32),
	) -> Result<(), String> {
		self.set(p, "FB_ID", fb as u64)?;
		self.set(p, "CRTC_ID", crtc as u64)?;
		self.set(p, "SRC_X", (src.0 as u64) << 16)?;
		self.set(p, "SRC_Y", (src.1 as u64) << 16)?;
		self.set(p, "SRC_W", (src.2 as u64) << 16)?;
		self.set(p, "SRC_H", (src.3 as u64) << 16)?;
		self.set(p, "CRTC_X", dst.0 as i64 as u64)?;
		self.set(p, "CRTC_Y", dst.1 as i64 as u64)?;
		self.set(p, "CRTC_W", dst.2 as u64)?;
		self.set(p, "CRTC_H", dst.3 as u64)
	}
	fn disable(&mut self, p: &Plane) -> Result<(), String> {
		self.set(p, "FB_ID", 0)?;
		self.set(p, "CRTC_ID", 0)
	}
	unsafe fn commit(&self, fd: c_int, flags: u32) -> Result<(), i32> {
		let mut objs: Vec<u32> = Vec::new();
		for it in &self.0 {
			if !objs.contains(&it.0) {
				objs.push(it.0);
			}
		}
		let (mut counts, mut props, mut vals) = (Vec::new(), Vec::new(), Vec::new());
		for o in &objs {
			let mut n = 0u32;
			for it in self.0.iter().filter(|it| it.0 == *o) {
				props.push(it.1);
				vals.push(it.2);
				n += 1;
			}
			counts.push(n);
		}
		let mut a: AtomicReq = std::mem::zeroed();
		a.flags = flags;
		a.count_objs = objs.len() as u32;
		a.objs_ptr = objs.as_ptr() as u64;
		a.count_props_ptr = counts.as_ptr() as u64;
		a.props_ptr = props.as_ptr() as u64;
		a.prop_values_ptr = vals.as_ptr() as u64;
		io(fd, IOCTL_MODE_ATOMIC, &mut a)
	}
}

unsafe fn rmfb(fd: c_int, fb: u32) {
	if fb != 0 {
		let mut id = fb;
		let _ = io(fd, IOCTL_MODE_RMFB, &mut id);
	}
}

unsafe fn gem_close(fd: c_int, handle: u32) {
	if handle != 0 {
		let mut g = GemClose { handle, pad: 0 };
		let _ = io(fd, IOCTL_GEM_CLOSE, &mut g);
	}
}

// ---------------------------------------------------------------------------------------------
// libxcb + libxcb-randr (dlopen'ed) — RandR 1.6 CreateLease.
// ---------------------------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
struct Ck {
	seq: u32,
}
#[repr(C)]
struct ResCurrent {
	response_type: u8,
	pad0: u8,
	sequence: u16,
	length: u32,
	timestamp: u32,
	config_timestamp: u32,
	num_crtcs: u16,
	num_outputs: u16,
	num_modes: u16,
	names_len: u16,
	pad1: [u8; 8],
}
#[repr(C)]
struct OutputInfo {
	response_type: u8,
	status: u8,
	sequence: u16,
	length: u32,
	timestamp: u32,
	crtc: u32,
	mm_width: u32,
	mm_height: u32,
	connection: u8,
	subpixel_order: u8,
	num_crtcs: u16,
	num_modes: u16,
	num_preferred: u16,
	num_clones: u16,
	name_len: u16,
}
#[repr(C)]
struct CrtcInfo {
	response_type: u8,
	status: u8,
	sequence: u16,
	length: u32,
	timestamp: u32,
	x: i16,
	y: i16,
	width: u16,
	height: u16,
	mode: u32,
	rotation: u16,
	rotations: u16,
	num_outputs: u16,
	num_possible_outputs: u16,
}
#[repr(C)]
struct XcbError {
	response_type: u8,
	error_code: u8,
}

type Conn = c_void;
type ReplyFn<T> = unsafe extern "C" fn(*mut Conn, Ck, *mut *mut XcbError) -> *mut T;

struct Xcb {
	conn: *mut Conn,
	disconnect: unsafe extern "C" fn(*mut Conn),
	flush: unsafe extern "C" fn(*mut Conn) -> c_int,
	free_lease: unsafe extern "C" fn(*mut Conn, u32, u8) -> Ck,
}

unsafe fn dlopen(name: &str) -> Result<*mut c_void, String> {
	let c = std::ffi::CString::new(name).unwrap();
	let h = libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
	if h.is_null() {
		Err(format!("{name} not available"))
	} else {
		Ok(h)
	}
}

unsafe fn sym<T: Copy>(h: *mut c_void, name: &str) -> Result<T, String> {
	let c = std::ffi::CString::new(name).unwrap();
	let p = libc::dlsym(h, c.as_ptr());
	if p.is_null() {
		return Err(format!("symbol {name} missing"));
	}
	Ok(std::mem::transmute_copy(&p))
}

/// Lease the CRTC + connector whose RandR CRTC exactly covers `rect` (root coordinates).
/// Returns the xcb connection that owns the lease, the lease id, and the DRM lease fd.
unsafe fn create_lease(
	xd: *mut xlib::Display,
	root: u64,
	rect: (i32, i32, u32, u32),
) -> Result<(Xcb, u32, c_int, String), String> {
	let lx = dlopen("libxcb.so.1")?;
	let lr = dlopen("libxcb-randr.so.0")?;
	let connect: unsafe extern "C" fn(*const c_char, *mut c_int) -> *mut Conn = sym(lx, "xcb_connect")?;
	let has_error: unsafe extern "C" fn(*mut Conn) -> c_int = sym(lx, "xcb_connection_has_error")?;
	let gen_id: unsafe extern "C" fn(*mut Conn) -> u32 = sym(lx, "xcb_generate_id")?;
	let res_req: unsafe extern "C" fn(*mut Conn, u32) -> Ck =
		sym(lr, "xcb_randr_get_screen_resources_current")?;
	let res_reply: ReplyFn<ResCurrent> = sym(lr, "xcb_randr_get_screen_resources_current_reply")?;
	let res_outputs: unsafe extern "C" fn(*const ResCurrent) -> *const u32 =
		sym(lr, "xcb_randr_get_screen_resources_current_outputs")?;
	let res_outputs_len: unsafe extern "C" fn(*const ResCurrent) -> c_int =
		sym(lr, "xcb_randr_get_screen_resources_current_outputs_length")?;
	let out_req: unsafe extern "C" fn(*mut Conn, u32, u32) -> Ck = sym(lr, "xcb_randr_get_output_info")?;
	let out_reply: ReplyFn<OutputInfo> = sym(lr, "xcb_randr_get_output_info_reply")?;
	let out_name: unsafe extern "C" fn(*const OutputInfo) -> *const u8 =
		sym(lr, "xcb_randr_get_output_info_name")?;
	let crtc_req: unsafe extern "C" fn(*mut Conn, u32, u32) -> Ck = sym(lr, "xcb_randr_get_crtc_info")?;
	let crtc_reply: ReplyFn<CrtcInfo> = sym(lr, "xcb_randr_get_crtc_info_reply")?;
	let lease_req: unsafe extern "C" fn(*mut Conn, u32, u32, u16, u16, *const u32, *const u32) -> Ck =
		sym(lr, "xcb_randr_create_lease")?;
	let lease_reply: ReplyFn<c_void> = sym(lr, "xcb_randr_create_lease_reply")?;
	let lease_fds: unsafe extern "C" fn(*mut Conn, *mut c_void) -> *mut c_int =
		sym(lr, "xcb_randr_create_lease_reply_fds")?;
	let xcb = Xcb {
		conn: ptr::null_mut(),
		disconnect: sym(lx, "xcb_disconnect")?,
		flush: sym(lx, "xcb_flush")?,
		free_lease: sym(lr, "xcb_randr_free_lease")?,
	};

	let conn = connect(xlib::XDisplayString(xd), ptr::null_mut());
	if conn.is_null() || has_error(conn) != 0 {
		if !conn.is_null() {
			(xcb.disconnect)(conn);
		}
		return Err("xcb_connect failed".into());
	}
	let xcb = Xcb { conn, ..xcb };
	let fail = |x: &Xcb, m: String| -> Result<(Xcb, u32, c_int, String), String> {
		(x.disconnect)(x.conn);
		Err(m)
	};

	let res = res_reply(conn, res_req(conn, root as u32), ptr::null_mut());
	if res.is_null() {
		return fail(&xcb, "RandR screen resources unavailable".into());
	}
	let cfg_ts = (*res).config_timestamp;
	let outs = std::slice::from_raw_parts(res_outputs(res), res_outputs_len(res).max(0) as usize).to_vec();
	libc::free(res as *mut c_void);

	let mut pick: Option<(u32, u32, String)> = None;
	for o in outs {
		let oi = out_reply(conn, out_req(conn, o, cfg_ts), ptr::null_mut());
		if oi.is_null() {
			continue;
		}
		let (crtc, connected) = ((*oi).crtc, (*oi).connection == 0);
		let name = String::from_utf8_lossy(std::slice::from_raw_parts(out_name(oi), (*oi).name_len as usize))
			.into_owned();
		libc::free(oi as *mut c_void);
		if !connected || crtc == 0 {
			continue;
		}
		let ci = crtc_reply(conn, crtc_req(conn, crtc, cfg_ts), ptr::null_mut());
		if ci.is_null() {
			continue;
		}
		let r = ((*ci).x as i32, (*ci).y as i32, (*ci).width as u32, (*ci).height as u32);
		libc::free(ci as *mut c_void);
		if r == rect {
			pick = Some((crtc, o, name));
			break;
		}
	}
	let Some((crtc, output, name)) = pick else {
		return fail(&xcb, format!("no output exactly covers the window {rect:?}"));
	};

	let lid = gen_id(conn);
	let mut err: *mut XcbError = ptr::null_mut();
	let rep = lease_reply(conn, lease_req(conn, root as u32, lid, 1, 1, &crtc, &output), &mut err);
	if rep.is_null() {
		let code = if err.is_null() { -1 } else { (*err).error_code as i32 };
		if !err.is_null() {
			libc::free(err as *mut c_void);
		}
		return fail(&xcb, format!("RandR CreateLease on {name} refused (X error {code})"));
	}
	let fd = *lease_fds(conn, rep);
	libc::free(rep);
	if fd < 0 {
		return fail(&xcb, "CreateLease returned no fd".into());
	}
	Ok((xcb, lid, fd, name))
}

// ---------------------------------------------------------------------------------------------
// The lease presenter.
// ---------------------------------------------------------------------------------------------

type EglCreateImage = unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *mut c_void, *const i32) -> *mut c_void;
type EglDestroyImage = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;
type ImageTargetTex = unsafe extern "C" fn(u32, *mut c_void);

/// One ARGB8888 overlay buffer: dumb BO → KMS fb, and the same memory as a GL render target.
struct OvBuf {
	handle: u32,
	dmabuf: c_int,
	fb: u32,
	img: *mut c_void,
	tex: Option<glow::Texture>,
	fbo: Option<glow::Framebuffer>,
}

/// A decoded frame on (or on its way to) the video plane. The AVFrame reference keeps MPP from
/// reusing the buffer while it is scanned out.
struct VidOnScreen {
	frame: *mut ff::AVFrame,
	fb: u32,
}

/// What the caller drew into the overlay this frame.
pub enum Overlay {
	/// egui output identical to what is on screen: leave the overlay plane alone.
	Keep,
	/// Nothing to draw: hide the overlay plane.
	Empty,
	/// Painted into the buffer bound by `begin_overlay`.
	Drawn,
}

pub struct KmsLease {
	xcb: Xcb,
	lid: u32,
	fd: c_int,
	crtc: u32,
	pub w: u32,
	pub h: u32,
	vplane: Plane,
	oplane: Plane,
	/// X's framebuffer on the overlay (primary) plane when we took over — restored on release.
	x_fb: u32,
	can_hide_primary: bool,
	// GL side of the overlay buffers.
	egl_dpy: *mut c_void,
	create_image: EglCreateImage,
	destroy_image: EglDestroyImage,
	image_target: ImageTargetTex,
	ov: Vec<OvBuf>,
	/// egui renders here first; `flip_to_back` copies it upside-down into the overlay buffer.
	stage_tex: Option<glow::Texture>,
	stage_fbo: Option<glow::Framebuffer>,
	flip_prog: Option<glow::Program>,
	flip_vbo: Option<glow::Buffer>,
	/// Overlay buffer committed last (on screen once the pending flip lands); None = plane off.
	ov_front: Option<usize>,
	/// Hash of the egui output in `ov_front` (see `prims_hash`).
	pub ov_hash: u64,
	/// The overlay plane still shows X's buffer (nothing committed yet).
	ov_untouched: bool,
	vid_cur: Option<VidOnScreen>,
	vid_old: Option<VidOnScreen>,
	/// KMS fb per decoder buffer, keyed by the dmabuf's (st_dev, st_ino): MPP cycles a small
	/// pool, so after warm-up no frame needs a PRIME import / ADDFB2 (both map the buffer into
	/// the VOP IOMMU). (dev, ino, w, h, fourcc, fb, last-use tick)
	fb_cache: Vec<(u64, u64, i32, i32, u32, u32, u64)>,
	tick: u64,
	flip_pending: Option<std::time::Instant>,
	released: bool,
	/// Commits made (the first one is logged).
	frames: u64,
}

const FB_CACHE_MAX: usize = 32;

impl KmsLease {
	/// Lease the output the fullscreen window `rect` covers and prepare both planes.
	pub unsafe fn acquire(
		xd: *mut xlib::Display,
		root: u64,
		rect: (i32, i32, u32, u32),
		gl: &glow::Context,
		egl_dpy: *mut c_void,
		get_proc: &dyn Fn(&str) -> *const c_void,
	) -> Result<KmsLease, String> {
		let need = |s: &str| -> Result<*const c_void, String> {
			let p = get_proc(s);
			if p.is_null() {
				Err(format!("{s} missing"))
			} else {
				Ok(p)
			}
		};
		let create_image: EglCreateImage = std::mem::transmute(need("eglCreateImageKHR")?);
		let destroy_image: EglDestroyImage = std::mem::transmute(need("eglDestroyImageKHR")?);
		let image_target: ImageTargetTex = std::mem::transmute(need("glEGLImageTargetTexture2DOES")?);

		let (xcb, lid, fd, out_name) = create_lease(xd, root, rect)?;
		// From here on every error path must drop the lease: build the struct early with
		// placeholder planes and let `release_kms` clean up.
		let mut k = KmsLease {
			xcb,
			lid,
			fd,
			crtc: 0,
			w: 0,
			h: 0,
			vplane: Plane { id: 0, fb_id: 0, formats: Vec::new(), props: Vec::new() },
			oplane: Plane { id: 0, fb_id: 0, formats: Vec::new(), props: Vec::new() },
			x_fb: 0,
			can_hide_primary: false,
			egl_dpy,
			create_image,
			destroy_image,
			image_target,
			ov: Vec::new(),
			stage_tex: None,
			stage_fbo: None,
			flip_prog: None,
			flip_vbo: None,
			ov_front: None,
			ov_hash: 0,
			ov_untouched: true,
			vid_cur: None,
			vid_old: None,
			fb_cache: Vec::new(),
			tick: 0,
			flip_pending: None,
			released: false,
			frames: 0,
		};
		match k.init(gl) {
			Ok(()) => {
				eprintln!(
					"pulsar-render: kms lease acquired on {out_name}: crtc {} {}x{}, video plane {}, overlay plane {} (primary hide {})",
					k.crtc, k.w, k.h, k.vplane.id, k.oplane.id, k.can_hide_primary
				);
				Ok(k)
			}
			Err(e) => {
				k.release(gl);
				Err(e)
			}
		}
	}

	unsafe fn init(&mut self, gl: &glow::Context) -> Result<(), String> {
		let fd = self.fd;
		for cap in [DRM_CLIENT_CAP_UNIVERSAL_PLANES, DRM_CLIENT_CAP_ATOMIC] {
			let mut c = SetClientCap { capability: cap, value: 1 };
			io(fd, IOCTL_SET_CLIENT_CAP, &mut c).map_err(|e| format!("client cap {cap}: {}", errstr(e)))?;
		}
		// The lease fd only sees leased objects: exactly our CRTC.
		let mut r: CardRes = std::mem::zeroed();
		io(fd, IOCTL_MODE_GETRESOURCES, &mut r).map_err(|e| format!("getresources: {}", errstr(e)))?;
		let mut crtcs = vec![0u32; r.count_crtcs as usize];
		let mut r2: CardRes = std::mem::zeroed();
		r2.count_crtcs = r.count_crtcs;
		r2.crtc_id_ptr = crtcs.as_mut_ptr() as u64;
		io(fd, IOCTL_MODE_GETRESOURCES, &mut r2).map_err(|e| format!("getresources: {}", errstr(e)))?;
		self.crtc = *crtcs.first().ok_or("lease has no CRTC")?;
		let mut c: ModeCrtc = std::mem::zeroed();
		c.crtc_id = self.crtc;
		io(fd, IOCTL_MODE_GETCRTC, &mut c).map_err(|e| format!("getcrtc: {}", errstr(e)))?;
		if c.mode_valid == 0 {
			return Err("leased CRTC has no active mode".into());
		}
		self.w = c.mode.hdisplay as u32;
		self.h = c.mode.vdisplay as u32;

		// Planes of the lease (the kernel adds the CRTC's primary + cursor planes implicitly
		// when X leases without them).
		let mut pr: GetPlaneRes = std::mem::zeroed();
		io(fd, IOCTL_MODE_GETPLANERESOURCES, &mut pr).map_err(|e| format!("getplaneresources: {}", errstr(e)))?;
		let mut ids = vec![0u32; pr.count_planes as usize];
		pr.plane_id_ptr = ids.as_mut_ptr() as u64;
		io(fd, IOCTL_MODE_GETPLANERESOURCES, &mut pr).map_err(|e| format!("getplaneresources: {}", errstr(e)))?;
		let mut planes = Vec::new();
		for &id in ids.iter().take(pr.count_planes as usize) {
			let mut p: GetPlane = std::mem::zeroed();
			p.plane_id = id;
			if io(fd, IOCTL_MODE_GETPLANE, &mut p).is_err() {
				continue;
			}
			let mut fmts = vec![0u32; p.count_format_types as usize];
			p.format_type_ptr = fmts.as_mut_ptr() as u64;
			if io(fd, IOCTL_MODE_GETPLANE, &mut p).is_err() || p.possible_crtcs & 1 == 0 {
				continue;
			}
			fmts.truncate(p.count_format_types as usize);
			planes.push(Plane { id, fb_id: p.fb_id, formats: fmts, props: obj_props(fd, id, DRM_MODE_OBJECT_PLANE)? });
		}
		let nv12 = fourcc(b"NV12");
		let oi = planes
			.iter()
			.position(|p| {
				p.val("type") == Some(DRM_PLANE_TYPE_PRIMARY)
					&& p.formats.contains(&DRM_FORMAT_ARGB8888)
					&& p.prop("rotation").is_some()
			})
			.ok_or("no primary ARGB8888 plane with rotation in the lease")?;
		let oplane = planes.remove(oi);
		let vi = planes
			.iter()
			.position(|p| p.formats.contains(&nv12))
			.ok_or("no NV12-capable second plane in the lease")?;
		self.vplane = planes.remove(vi);
		self.x_fb = oplane.fb_id;
		self.oplane = oplane;

		// Overlay buffers: dumb BO → dmabuf → KMS fb + EGLImage-backed GL render target.
		for _ in 0..2 {
			let b = self.make_ov_buf(gl)?;
			self.ov.push(b);
		}
		self.make_stage(gl)?;
		// Start both cleared (transparent).
		for i in 0..2 {
			gl.bind_framebuffer(glow::FRAMEBUFFER, self.ov[i].fbo);
			gl.viewport(0, 0, self.w as i32, self.h as i32);
			gl.clear_color(0.0, 0.0, 0.0, 0.0);
			gl.clear(glow::COLOR_BUFFER_BIT);
		}
		gl.bind_framebuffer(glow::FRAMEBUFFER, None);
		gl.finish();

		// Probe the configurations we will use (TEST_ONLY changes nothing on screen).
		let mut t = Req::default();
		self.ov_req(&mut t, Some(0))?;
		t.disable(&self.vplane)?;
		t.commit(fd, DRM_MODE_ATOMIC_TEST_ONLY)
			.map_err(|e| format!("overlay plane test failed: {}", errstr(e)))?;
		let mut t = Req::default();
		t.disable(&self.oplane)?;
		t.disable(&self.vplane)?;
		self.can_hide_primary = t.commit(fd, DRM_MODE_ATOMIC_TEST_ONLY).is_ok();
		Ok(())
	}

	unsafe fn make_ov_buf(&self, gl: &glow::Context) -> Result<OvBuf, String> {
		let fd = self.fd;
		let mut d: CreateDumb = std::mem::zeroed();
		d.width = self.w;
		d.height = self.h;
		d.bpp = 32;
		io(fd, IOCTL_MODE_CREATE_DUMB, &mut d).map_err(|e| format!("create_dumb: {}", errstr(e)))?;
		let mut b = OvBuf { handle: d.handle, dmabuf: -1, fb: 0, img: ptr::null_mut(), tex: None, fbo: None };
		let mut f: FbCmd2 = std::mem::zeroed();
		f.width = self.w;
		f.height = self.h;
		f.pixel_format = DRM_FORMAT_ARGB8888;
		f.handles[0] = d.handle;
		f.pitches[0] = d.pitch;
		if let Err(e) = io(fd, IOCTL_MODE_ADDFB2, &mut f) {
			self.free_ov_buf(gl, &b);
			return Err(format!("addfb2 (overlay): {}", errstr(e)));
		}
		b.fb = f.fb_id;
		let mut ph = PrimeHandle { handle: d.handle, flags: (libc::O_CLOEXEC | libc::O_RDWR) as u32, fd: -1 };
		if let Err(e) = io(fd, IOCTL_PRIME_HANDLE_TO_FD, &mut ph) {
			self.free_ov_buf(gl, &b);
			return Err(format!("prime export (overlay): {}", errstr(e)));
		}
		b.dmabuf = ph.fd;
		let attrs = [
			0x3271, DRM_FORMAT_ARGB8888 as i32, // EGL_LINUX_DRM_FOURCC_EXT
			0x3057, self.w as i32, // EGL_WIDTH
			0x3056, self.h as i32, // EGL_HEIGHT
			0x3272, ph.fd,         // EGL_DMA_BUF_PLANE0_FD_EXT
			0x3273, 0,             // EGL_DMA_BUF_PLANE0_OFFSET_EXT
			0x3274, d.pitch as i32, // EGL_DMA_BUF_PLANE0_PITCH_EXT
			0x3038, // EGL_NONE
		];
		b.img = (self.create_image)(self.egl_dpy, ptr::null_mut(), 0x3270, ptr::null_mut(), attrs.as_ptr());
		if b.img.is_null() {
			self.free_ov_buf(gl, &b);
			return Err("EGLImage import of the overlay buffer failed".into());
		}
		let tex = gl.create_texture().map_err(|e| e.to_string())?;
		b.tex = Some(tex);
		gl.bind_texture(glow::TEXTURE_2D, Some(tex));
		gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32);
		gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32);
		(self.image_target)(glow::TEXTURE_2D, b.img);
		gl.bind_texture(glow::TEXTURE_2D, None);
		let fbo = gl.create_framebuffer().map_err(|e| e.to_string())?;
		b.fbo = Some(fbo);
		gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
		gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(tex), 0);
		let st = gl.check_framebuffer_status(glow::FRAMEBUFFER);
		gl.bind_framebuffer(glow::FRAMEBUFFER, None);
		if st != glow::FRAMEBUFFER_COMPLETE {
			self.free_ov_buf(gl, &b);
			return Err(format!("overlay FBO incomplete (0x{st:x})"));
		}
		Ok(b)
	}

	/// The staging render target + the flip shader (a full-screen quad sampling it with V
	/// inverted).
	unsafe fn make_stage(&mut self, gl: &glow::Context) -> Result<(), String> {
		let tex = gl.create_texture()?;
		self.stage_tex = Some(tex);
		gl.bind_texture(glow::TEXTURE_2D, Some(tex));
		gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32);
		gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32);
		gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
		gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
		gl.tex_image_2d(
			glow::TEXTURE_2D,
			0,
			glow::RGBA as i32,
			self.w as i32,
			self.h as i32,
			0,
			glow::RGBA,
			glow::UNSIGNED_BYTE,
			glow::PixelUnpackData::Slice(None),
		);
		gl.bind_texture(glow::TEXTURE_2D, None);
		let fbo = gl.create_framebuffer()?;
		self.stage_fbo = Some(fbo);
		gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
		gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(tex), 0);
		let st = gl.check_framebuffer_status(glow::FRAMEBUFFER);
		gl.bind_framebuffer(glow::FRAMEBUFFER, None);
		if st != glow::FRAMEBUFFER_COMPLETE {
			return Err(format!("overlay staging FBO incomplete (0x{st:x})"));
		}
		const VS: &str = "attribute vec2 pos;
varying vec2 uv;
void main(){ uv = vec2(pos.x * 0.5 + 0.5, 0.5 - pos.y * 0.5); gl_Position = vec4(pos, 0.0, 1.0); }
";
		const FS: &str = "precision mediump float;
varying vec2 uv;
uniform sampler2D t;
void main(){ gl_FragColor = texture2D(t, uv); }
";
		let prog = gl.create_program()?;
		self.flip_prog = Some(prog);
		for (ty, src) in [(glow::VERTEX_SHADER, VS), (glow::FRAGMENT_SHADER, FS)] {
			let sh = gl.create_shader(ty)?;
			gl.shader_source(sh, src);
			gl.compile_shader(sh);
			if !gl.get_shader_compile_status(sh) {
				let log = gl.get_shader_info_log(sh);
				gl.delete_shader(sh);
				return Err(format!("flip shader: {log}"));
			}
			gl.attach_shader(prog, sh);
			gl.delete_shader(sh);
		}
		gl.bind_attrib_location(prog, 0, "pos");
		gl.link_program(prog);
		if !gl.get_program_link_status(prog) {
			return Err(format!("flip program: {}", gl.get_program_info_log(prog)));
		}
		gl.use_program(Some(prog));
		gl.uniform_1_i32(gl.get_uniform_location(prog, "t").as_ref(), 0);
		gl.use_program(None);
		let vbo = gl.create_buffer()?;
		self.flip_vbo = Some(vbo);
		let quad: [f32; 8] = [-1.0, -1.0, 1.0, -1.0, -1.0, 1.0, 1.0, 1.0];
		let bytes: Vec<u8> = quad.iter().flat_map(|f| f.to_ne_bytes()).collect();
		gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
		gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, &bytes, glow::STATIC_DRAW);
		gl.bind_buffer(glow::ARRAY_BUFFER, None);
		Ok(())
	}

	/// Copy the staging texture upside-down into the overlay back buffer and wait for the GPU.
	unsafe fn flip_to_back(&self, gl: &glow::Context) {
		self.bind_back(gl);
		gl.disable(glow::SCISSOR_TEST);
		gl.disable(glow::BLEND);
		gl.use_program(self.flip_prog);
		gl.active_texture(glow::TEXTURE0);
		gl.bind_texture(glow::TEXTURE_2D, self.stage_tex);
		gl.bind_buffer(glow::ARRAY_BUFFER, self.flip_vbo);
		gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 8, 0);
		gl.enable_vertex_attrib_array(0);
		gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
		gl.disable_vertex_attrib_array(0);
		gl.bind_buffer(glow::ARRAY_BUFFER, None);
		gl.bind_texture(glow::TEXTURE_2D, None);
		gl.use_program(None);
		gl.bind_framebuffer(glow::FRAMEBUFFER, None);
		gl.finish();
	}

	unsafe fn free_stage(&mut self, gl: &glow::Context) {
		if let Some(f) = self.stage_fbo.take() {
			gl.delete_framebuffer(f);
		}
		if let Some(t) = self.stage_tex.take() {
			gl.delete_texture(t);
		}
		if let Some(p) = self.flip_prog.take() {
			gl.delete_program(p);
		}
		if let Some(b) = self.flip_vbo.take() {
			gl.delete_buffer(b);
		}
	}

	unsafe fn free_ov_buf(&self, gl: &glow::Context, b: &OvBuf) {
		if let Some(f) = b.fbo {
			gl.delete_framebuffer(f);
		}
		if let Some(t) = b.tex {
			gl.delete_texture(t);
		}
		if !b.img.is_null() {
			(self.destroy_image)(self.egl_dpy, b.img);
		}
		if b.dmabuf >= 0 {
			libc::close(b.dmabuf);
		}
		rmfb(self.fd, b.fb);
		if b.handle != 0 {
			let mut d = DestroyDumb { handle: b.handle };
			let _ = io(self.fd, IOCTL_MODE_DESTROY_DUMB, &mut d);
		}
	}

	/// Overlay plane state: buffer `i` full-screen above the video, or hidden (`None`).
	unsafe fn ov_req(&self, r: &mut Req, i: Option<usize>) -> Result<(), String> {
		match i {
			Some(i) => {
				r.place(&self.oplane, self.crtc, self.ov[i].fb, (0, 0, self.w, self.h), (0, 0, self.w, self.h))?;
				r.opt(&self.oplane, "rotation", ROTATE_0);
				r.opt(&self.oplane, "zpos", 1);
				r.opt(&self.oplane, "pixel blend mode", 0); // pre-multiplied (egui's output)
				r.opt(&self.vplane, "zpos", 0);
			}
			None => r.disable(&self.oplane)?,
		}
		Ok(())
	}

	/// The buffer egui may paint into now (never the one on screen).
	fn ov_back(&self) -> usize {
		if self.ov_front == Some(0) {
			1
		} else {
			0
		}
	}

	/// Bind the staging target, cleared transparent, then paint egui and
	/// `present(.., Overlay::Drawn)`. False (nothing bound) while a flip is still pending: the
	/// back buffer may then still be on screen.
	pub unsafe fn begin_overlay(&mut self, gl: &glow::Context) -> bool {
		if self.flip_pending.is_some() {
			return false;
		}
		gl.bind_framebuffer(glow::FRAMEBUFFER, self.stage_fbo);
		gl.viewport(0, 0, self.w as i32, self.h as i32);
		gl.clear_color(0.0, 0.0, 0.0, 0.0);
		gl.clear(glow::COLOR_BUFFER_BIT);
		true
	}

	unsafe fn bind_back(&self, gl: &glow::Context) {
		gl.bind_framebuffer(glow::FRAMEBUFFER, self.ov[self.ov_back()].fbo);
		gl.viewport(0, 0, self.w as i32, self.h as i32);
		gl.clear_color(0.0, 0.0, 0.0, 0.0);
		gl.clear(glow::COLOR_BUFFER_BIT);
	}

	/// KMS framebuffer for a DRM_PRIME frame (cached per decoder buffer).
	unsafe fn video_fb(&mut self, f: *mut ff::AVFrame) -> Result<u32, String> {
		if (*f).format != ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as c_int {
			return Err("frame is not DRM_PRIME".into());
		}
		let d = (*f).data[0] as *const ff::AVDRMFrameDescriptor;
		if d.is_null() || (*d).nb_layers != 1 || (*d).nb_objects < 1 || (*d).nb_objects > 4 {
			return Err("unsupported DRM frame layout".into());
		}
		let layer = &(*d).layers[0];
		let (w, h) = ((*f).width, (*f).height);
		if !self.vplane.formats.contains(&layer.format) {
			return Err(format!("video plane lacks format 0x{:08x}", layer.format));
		}
		let mut st: libc::stat = std::mem::zeroed();
		if libc::fstat((*d).objects[0].fd, &mut st) != 0 {
			return Err("fstat(dmabuf) failed".into());
		}
		let (dev, ino) = (st.st_dev as u64, st.st_ino as u64);
		self.tick += 1;
		let tick = self.tick;
		if let Some(e) = self
			.fb_cache
			.iter_mut()
			.find(|e| e.0 == dev && e.1 == ino && e.2 == w && e.3 == h && e.4 == layer.format)
		{
			e.6 = tick;
			return Ok(e.5);
		}
		let nobj = (*d).nb_objects as usize;
		let mut handles = [0u32; 4];
		for i in 0..nobj {
			let mut ph = PrimeHandle { handle: 0, flags: 0, fd: (*d).objects[i].fd };
			if let Err(e) = io(self.fd, IOCTL_PRIME_FD_TO_HANDLE, &mut ph) {
				for &hd in &handles[..i] {
					gem_close(self.fd, hd);
				}
				return Err(format!("prime import: {}", errstr(e)));
			}
			handles[i] = ph.handle;
		}
		let mut c: FbCmd2 = std::mem::zeroed();
		c.width = w as u32;
		c.height = h as u32;
		c.pixel_format = layer.format;
		for i in 0..(layer.nb_planes as usize).min(4) {
			let p = &layer.planes[i];
			let oi = (p.object_index as usize).min(nobj - 1);
			c.handles[i] = handles[oi];
			c.pitches[i] = p.pitch as u32;
			c.offsets[i] = p.offset as u32;
			let m = (*d).objects[oi].format_modifier;
			if m != DRM_FORMAT_MOD_INVALID && m != 0 {
				c.flags = DRM_MODE_FB_MODIFIERS;
				c.modifier[i] = m;
			}
		}
		let r = io(self.fd, IOCTL_MODE_ADDFB2, &mut c);
		// The fb holds its own reference on the BOs.
		for i in 0..nobj {
			if !handles[..i].contains(&handles[i]) {
				gem_close(self.fd, handles[i]);
			}
		}
		r.map_err(|e| format!("addfb2 (video {w}x{h} 0x{:08x}): {}", layer.format, errstr(e)))?;
		if self.fb_cache.len() >= FB_CACHE_MAX {
			// Evict the least recently used fb that is not on screen.
			let busy = [self.vid_cur.as_ref().map(|v| v.fb), self.vid_old.as_ref().map(|v| v.fb)];
			if let Some((i, _)) = self
				.fb_cache
				.iter()
				.enumerate()
				.filter(|(_, e)| !busy.contains(&Some(e.5)))
				.min_by_key(|(_, e)| e.6)
			{
				let e = self.fb_cache.remove(i);
				rmfb(self.fd, e.5);
			}
		}
		self.fb_cache.push((dev, ino, w, h, layer.format, c.fb_id, tick));
		Ok(c.fb_id)
	}

	/// Letterbox the `vw`×`vh` frame on the screen by the view-fit mode, clipped to the screen.
	/// Returns (src rect in frame pixels, dst rect on the CRTC).
	fn placement(&self, vw: i32, vh: i32) -> ((u32, u32, u32, u32), (i32, i32, u32, u32)) {
		let (sw, sh) = (self.w as i64, self.h as i64);
		let (fw, fh) = (vw as i64, vh as i64);
		let (rw, rh) = match crate::video::fit_label() {
			"stretch" => (sw, sh),
			"original" => (fw, fh),
			_ => {
				let r2 = sw * fh / fw;
				if r2 > sh {
					(sh * fw / fh, sh)
				} else {
					(sw, r2)
				}
			}
		};
		let (x, y) = ((sw - rw) / 2, (sh - rh) / 2);
		// Clip to the screen and crop the source by the same proportion (ORIGINAL on a larger
		// stream). Even values keep the 4:2:0 chroma sampling aligned.
		let (cx0, cy0) = (x.max(0), y.max(0));
		let (cx1, cy1) = ((x + rw).min(sw), (y + rh).min(sh));
		let sx0 = (cx0 - x) * fw / rw;
		let sy0 = (cy0 - y) * fh / rh;
		let sx1 = (cx1 - x) * fw / rw;
		let sy1 = (cy1 - y) * fh / rh;
		let ev = |v: i64| (v & !1).max(0) as u32;
		let src = (ev(sx0), ev(sy0), ev(sx1 - sx0).max(2), ev(sy1 - sy0).max(2));
		let dst = (cx0 as i32, cy0 as i32, (cx1 - cx0).max(1) as u32, (cy1 - cy0).max(1) as u32);
		(src, dst)
	}

	/// Commit this frame: the video plane when `fresh` (or nothing is shown yet), the overlay
	/// per `ov`. Non-blocking with a page-flip event; returns whether anything was committed.
	/// Call `wait_flip` before the next one.
	pub unsafe fn present(
		&mut self,
		gl: &glow::Context,
		frame: *mut ff::AVFrame,
		fresh: bool,
		ov: Overlay,
		ov_hash: u64,
	) -> Result<bool, String> {
		if self.flip_pending.is_some() {
			self.wait_flip(50)?;
			if self.flip_pending.is_some() {
				return Ok(false);
			}
		}
		let mut r = Req::default();
		let mut new_vid: Option<VidOnScreen> = None;
		if !frame.is_null() && (fresh || self.vid_cur.is_none()) {
			let fb = self.video_fb(frame)?;
			let (vw, vh) = ((*frame).width, (*frame).height);
			let (src, dst) = self.placement(vw, vh);
			r.place(&self.vplane, self.crtc, fb, src, dst)?;
			r.opt(&self.vplane, "zpos", 0);
			r.opt(&self.vplane, "rotation", ROTATE_0);
			// The cursor side-channel maps into this rect (GL convention: bottom-left origin).
			*crate::video::VIDEO_RECT.lock().unwrap() =
				[dst.0, self.h as i32 - (dst.1 + dst.3 as i32), dst.2 as i32, dst.3 as i32];
			*crate::video::VIDEO_SRC.lock().unwrap() = [vw, vh];
			let c = ff::av_frame_clone(frame);
			if c.is_null() {
				return Err("av_frame_clone failed".into());
			}
			new_vid = Some(VidOnScreen { frame: c, fb });
		}
		let ov = match ov {
			// First commit: X's buffer must leave the primary plane either way.
			Overlay::Keep if self.ov_untouched => Overlay::Empty,
			o => o,
		};
		let mut new_front = self.ov_front;
		match ov {
			Overlay::Keep => {}
			Overlay::Drawn => {
				self.flip_to_back(gl);
				new_front = Some(self.ov_back());
				self.ov_req(&mut r, new_front)?;
			}
			Overlay::Empty => {
				if self.can_hide_primary {
					if self.ov_front.is_some() || self.ov_untouched {
						new_front = None;
						self.ov_req(&mut r, None)?;
					}
				} else if self.ov_front.is_none() || self.ov_hash != ov_hash {
					// The primary must stay on: show a cleared buffer.
					self.bind_back(gl);
					gl.bind_framebuffer(glow::FRAMEBUFFER, None);
					gl.finish();
					new_front = Some(self.ov_back());
					self.ov_req(&mut r, new_front)?;
				}
			}
		}
		if r.0.is_empty() {
			return Ok(false);
		}
		if let Err(e) = r.commit(self.fd, DRM_MODE_PAGE_FLIP_EVENT | DRM_MODE_ATOMIC_NONBLOCK) {
			if let Some(v) = new_vid {
				let mut f = v.frame;
				ff::av_frame_free(&mut f);
			}
			return Err(format!("atomic commit failed: {}", errstr(e)));
		}
		if self.frames == 0 {
			eprintln!("pulsar-render: kms first commit ok (video plane {}, overlay plane {})", self.vplane.id, self.oplane.id);
		}
		self.frames += 1;
		self.flip_pending = Some(std::time::Instant::now());
		self.ov_untouched = false;
		self.ov_front = new_front;
		self.ov_hash = ov_hash;
		if let Some(v) = new_vid {
			// The previous frame stays scanned out until this flip lands.
			if let Some(old) = self.vid_old.take() {
				Self::drop_vid(old);
			}
			self.vid_old = self.vid_cur.replace(v);
		}
		Ok(true)
	}

	unsafe fn drop_vid(v: VidOnScreen) {
		let mut f = v.frame;
		ff::av_frame_free(&mut f);
	}

	/// Wait up to `timeout_ms` for the pending page flip. A flip that never lands within 500 ms
	/// is an error (the caller falls back to X).
	pub unsafe fn wait_flip(&mut self, timeout_ms: i32) -> Result<(), String> {
		let Some(since) = self.flip_pending else {
			return Ok(());
		};
		let mut pfd = libc::pollfd { fd: self.fd, events: libc::POLLIN, revents: 0 };
		let n = libc::poll(&mut pfd, 1, timeout_ms);
		if n > 0 {
			let mut buf = [0u8; 1024];
			let len = libc::read(self.fd, buf.as_mut_ptr() as *mut c_void, buf.len());
			let mut off = 0usize;
			while len > 0 && off + 8 <= len as usize {
				let ty = u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap());
				let l = u32::from_ne_bytes(buf[off + 4..off + 8].try_into().unwrap()) as usize;
				if ty == DRM_EVENT_FLIP_COMPLETE {
					self.flip_pending = None;
					if let Some(old) = self.vid_old.take() {
						Self::drop_vid(old);
					}
				}
				if l < 8 {
					break;
				}
				off += l;
			}
		}
		if self.flip_pending.is_some() && since.elapsed().as_millis() > 500 {
			return Err("page flip did not complete within 500 ms".into());
		}
		Ok(())
	}

	/// Hand the output back: put X's buffer back on the primary plane, clear ours, drop the lease.
	pub unsafe fn release(&mut self, gl: &glow::Context) {
		if self.released {
			return;
		}
		let _ = self.wait_flip(100);
		if self.vplane.id != 0 && self.oplane.id != 0 {
			let mut r = Req::default();
			let ok = r.disable(&self.vplane).is_ok() && {
				r.opt(&self.vplane, "zpos", self.vplane.val("zpos").unwrap_or(0));
				r.opt(&self.vplane, "rotation", ROTATE_0);
				if self.x_fb != 0 {
					r.place(&self.oplane, self.crtc, self.x_fb, (0, 0, self.w, self.h), (0, 0, self.w, self.h))
						.is_ok()
				} else {
					true
				}
			};
			r.opt(&self.oplane, "rotation", self.oplane.val("rotation").unwrap_or(ROTATE_0));
			r.opt(&self.oplane, "zpos", self.oplane.val("zpos").unwrap_or(0));
			if ok {
				if let Err(e) = r.commit(self.fd, 0) {
					eprintln!("pulsar-render: kms restore commit failed: {} — disabling our planes", errstr(e));
					let mut r = Req::default();
					let _ = r.disable(&self.vplane);
					let _ = r.commit(self.fd, 0);
				}
			}
		}
		for v in [self.vid_cur.take(), self.vid_old.take()].into_iter().flatten() {
			Self::drop_vid(v);
		}
		for e in self.fb_cache.drain(..) {
			rmfb(self.fd, e.5);
		}
		self.free_stage(gl);
		let bufs: Vec<OvBuf> = self.ov.drain(..).collect();
		for b in &bufs {
			self.free_ov_buf(gl, b);
		}
		self.release_kms();
		eprintln!("pulsar-render: kms lease released");
	}

	unsafe fn release_kms(&mut self) {
		if self.released {
			return;
		}
		self.released = true;
		libc::close(self.fd);
		(self.xcb.free_lease)(self.xcb.conn, self.lid, 1);
		(self.xcb.flush)(self.xcb.conn);
		(self.xcb.disconnect)(self.xcb.conn);
	}
}

impl Drop for KmsLease {
	fn drop(&mut self) {
		// Normal teardown goes through `release` (GL objects need the context); this only
		// guarantees the output is handed back to X.
		unsafe {
			for v in [self.vid_cur.take(), self.vid_old.take()].into_iter().flatten() {
				Self::drop_vid(v);
			}
			self.release_kms();
		}
	}
}

/// Lease lifecycle for the render loop: when to take the output, when to hand it back, and the
/// failure backoff. `linux.rs` owns one and calls it at the `KMS-LEASE` markers.
pub struct Ctl {
	pub lease: Option<KmsLease>,
	enabled: bool,
	/// Consecutive loop iterations the lease conditions held (hysteresis before acquiring).
	ready: u32,
	fails: u32,
	retry_at: std::time::Instant,
	/// The current frame can go on a plane as-is (checked on the X path, before leasing).
	pub frame_ok: bool,
	/// Window size last seen under the lease, and a counter for the periodic WM-state check.
	leased_win: (u32, u32),
	fs_tick: u32,
	// Pointer while leased. X can't draw its cursor on a leased output, and its pointer
	// coordinates stop matching the screen once the WM shrinks the window, so the renderer
	// integrates XI2 raw motion into its own position (leased-mode pixels) and draws the
	// cursor itself. The X pointer is actively grabbed on the video window meanwhile, so
	// clicks can't land on (and focus) whatever X window happens to be under it.
	xd: *mut xlib::Display,
	xwin: u64,
	raw: Option<RawInput>,
	raw_tried: bool,
	ptr: (f64, f64),
	grabbed: bool,
	grab_tick: u32,
	/// After a release: re-assert the WM fullscreen geometry of the toplevel for a while
	/// (see `refit_tick`). (since, last attempt, attempts)
	refit: Option<(std::time::Instant, std::time::Instant, u32)>,
}

/// Loop iterations the conditions must hold before leasing (~0.3-0.5 s): a fullscreen toggle
/// or a fresh session settles first.
const READY_ITERS: u32 = 30;
/// After this many real failures the lease path stays off for the life of the process.
const MAX_FAILS: u32 = 3;

impl Ctl {
	pub fn new() -> Ctl {
		let enabled = enabled();
		if !enabled {
			eprintln!("pulsar-render: kms lease disabled (PULSAR_KMS_LEASE=0)");
		}
		Ctl {
			lease: None,
			enabled,
			ready: 0,
			fails: 0,
			retry_at: std::time::Instant::now(),
			frame_ok: false,
			leased_win: (0, 0),
			fs_tick: 0,
			xd: ptr::null_mut(),
			xwin: 0,
			raw: None,
			raw_tried: false,
			ptr: (0.0, 0.0),
			grabbed: false,
			grab_tick: 0,
			refit: None,
		}
	}

	pub fn active(&self) -> bool {
		self.lease.is_some()
	}

	/// Whether pointer input comes from XI2 raw events (vs. the X core events) right now.
	pub fn raw_input(&self) -> bool {
		self.lease.is_some() && self.raw.as_ref().is_some_and(|r| r.on)
	}

	/// The renderer-owned pointer position in leased-mode pixels (top-left origin), while
	/// leased with raw input.
	pub fn pointer_px(&self) -> Option<(f32, f32)> {
		self.raw_input().then_some((self.ptr.0 as f32, self.ptr.1 as f32))
	}

	/// Feed one X event; consumes XI2 raw events while leased. Motion moves the own pointer
	/// (clamped to the leased mode) and yields `RawEv::Motion`; buttons are passed through.
	pub unsafe fn raw_event(&mut self, ev: &mut xlib::XEvent) -> Option<RawEv> {
		let (w, h) = self.lease.as_ref().map(|k| (k.w as f64, k.h as f64))?;
		let r = self.raw.as_ref()?.decode(self.xd, ev)?;
		if let RawEv::Motion(dx, dy) = r {
			self.ptr.0 = (self.ptr.0 + dx).clamp(0.0, w - 1.0);
			self.ptr.1 = (self.ptr.1 + dy).clamp(0.0, h - 1.0);
		}
		Some(r)
	}

	/// Start the own-pointer input for a fresh lease: position from the X pointer (still
	/// valid — the window covered the output a moment ago), raw events on, pointer grabbed.
	unsafe fn input_begin(&mut self, xd: *mut xlib::Display, win: u64) {
		self.xd = xd;
		self.xwin = win;
		if !self.raw_tried {
			self.raw_tried = true;
			match RawInput::new(xd) {
				Ok(r) => self.raw = Some(r),
				Err(e) => eprintln!("pulsar-render: kms: no XI2 raw input ({e}) — X pointer coordinates are used"),
			}
		}
		let (w, h) = self.lease.as_ref().map(|k| (k.w as f64, k.h as f64)).unwrap_or((1.0, 1.0));
		let (mut rr, mut cr) = (0u64, 0u64);
		let (mut rx, mut ry, mut x, mut y, mut m) = (0i32, 0i32, 0i32, 0i32, 0u32);
		self.ptr = if xlib::XQueryPointer(xd, win, &mut rr, &mut cr, &mut rx, &mut ry, &mut x, &mut y, &mut m) != 0 {
			((x as f64).clamp(0.0, w - 1.0), (y as f64).clamp(0.0, h - 1.0))
		} else {
			(w / 2.0, h / 2.0)
		};
		if let Some(r) = self.raw.as_mut() {
			r.set(xd, true);
		}
		self.grab_tick = 0;
		self.try_grab();
	}

	unsafe fn try_grab(&mut self) {
		if self.grabbed || self.xd.is_null() {
			return;
		}
		let mask = (xlib::ButtonPressMask | xlib::ButtonReleaseMask | xlib::PointerMotionMask) as u32;
		let r = xlib::XGrabPointer(
			self.xd,
			self.xwin,
			xlib::False,
			mask,
			xlib::GrabModeAsync,
			xlib::GrabModeAsync,
			0,
			0,
			xlib::CurrentTime,
		);
		self.grabbed = r == xlib::GrabSuccess;
		if self.grab_tick == 0 || self.grabbed {
			eprintln!("pulsar-render: kms pointer grab {}", if self.grabbed { "ok".to_string() } else { format!("failed ({r})") });
		}
	}

	unsafe fn input_end(&mut self) {
		if self.xd.is_null() {
			return;
		}
		if let Some(r) = self.raw.as_mut() {
			r.set(self.xd, false);
		}
		if self.grabbed {
			xlib::XUngrabPointer(self.xd, xlib::CurrentTime);
			self.grabbed = false;
		}
		// Hand the X pointer back where the user last saw ours.
		if self.lease.is_some() && self.xwin != 0 {
			xlib::XWarpPointer(self.xd, 0, self.xwin, 0, 0, 0, 0, self.ptr.0 as i32, self.ptr.1 as i32);
		}
		xlib::XFlush(self.xd);
	}

	/// After a release X re-enables the output, and the WM re-fits the fullscreen toplevel it
	/// had shrunk while the output read as disconnected — by itself only after 1-6 s. Called
	/// every loop iteration (also while idle): for up to 10 s after a release, every 250 ms,
	/// re-assert `_NET_WM_STATE_FULLSCREEN` and the screen geometry on a
	/// toplevel that is still WM-fullscreen but smaller than the screen. Never changes a
	/// window the WM doesn't consider fullscreen. `PULSAR_KMS_REFIT=0` turns it off.
	pub unsafe fn refit_tick(&mut self) {
		let Some((since, last, n)) = self.refit else { return };
		if self.xd.is_null() || self.lease.is_some() || since.elapsed().as_secs() >= 10 || n >= 40 {
			if n > 0 || since.elapsed().as_secs() >= 10 {
				eprintln!("pulsar-render: kms refit done ({n} request(s), {} ms)", since.elapsed().as_millis());
			}
			self.refit = None;
			return;
		}
		if last.elapsed().as_millis() < 250 {
			return;
		}
		self.refit = Some((since, std::time::Instant::now(), n));
		let xd = self.xd;
		let Some(client) = wm_client(xd, self.xwin) else { return };
		if wm_fullscreen(xd, self.xwin) != Some(true) {
			if n == 0 {
				eprintln!("pulsar-render: kms refit: toplevel not WM-fullscreen — left alone");
			}
			self.refit = None;
			return;
		}
		let scr = xlib::XDefaultScreen(xd);
		let (sw, sh) = (xlib::XDisplayWidth(xd, scr) as u32, xlib::XDisplayHeight(xd, scr) as u32);
		let (mut root, mut x, mut y, mut cw, mut ch, mut bw, mut dp) = (0u64, 0i32, 0i32, 0u32, 0u32, 0u32, 0u32);
		if xlib::XGetGeometry(xd, client, &mut root, &mut x, &mut y, &mut cw, &mut ch, &mut bw, &mut dp) == 0 {
			return;
		}
		if (cw, ch) == (sw, sh) {
			eprintln!("pulsar-render: kms refit: toplevel back at {cw}x{ch} after {} ms", since.elapsed().as_millis());
			self.refit = None;
			return;
		}
		eprintln!(
			"pulsar-render: kms refit: toplevel {cw}x{ch} on a {sw}x{sh} screen ({} ms after release) — re-asserting fullscreen",
			since.elapsed().as_millis()
		);
		let state = xlib::XInternAtom(xd, b"_NET_WM_STATE\0".as_ptr() as *const c_char, xlib::False);
		let fs = xlib::XInternAtom(xd, b"_NET_WM_STATE_FULLSCREEN\0".as_ptr() as *const c_char, xlib::False);
		let mut ev: xlib::XEvent = std::mem::zeroed();
		ev.client_message.type_ = xlib::ClientMessage;
		ev.client_message.window = client;
		ev.client_message.message_type = state;
		ev.client_message.format = 32;
		ev.client_message.data.set_long(0, 1); // _NET_WM_STATE_ADD
		ev.client_message.data.set_long(1, fs as i64);
		ev.client_message.data.set_long(3, 1); // source: application
		xlib::XSendEvent(
			xd,
			xlib::XDefaultRootWindow(xd),
			xlib::False,
			xlib::SubstructureRedirectMask | xlib::SubstructureNotifyMask,
			&mut ev,
		);
		xlib::XMoveResizeWindow(xd, client, 0, 0, sw, sh);
		xlib::XFlush(xd);
		self.refit = Some((since, std::time::Instant::now(), n + 1));
	}

	/// Debug: release mid-session and don't re-lease for 15 s (measures the refit).
	pub unsafe fn debug_release(&mut self, gl: &glow::Context) {
		self.release(gl, "debug");
		self.ready = 0;
		self.retry_at = std::time::Instant::now() + std::time::Duration::from_secs(15);
	}

	/// Debug (PULSAR_KMS_DEBUG=1, see `debug_signals`): write the overlay buffer on screen to
	/// /tmp/ov.png (composited over grey) and a 256x256 crop around the own pointer to
	/// /tmp/ov-ptr.png.
	pub unsafe fn debug_dump(&self, gl: &glow::Context) {
		let Some(k) = self.lease.as_ref() else {
			eprintln!("pulsar-render: kms dump: not leased");
			return;
		};
		eprintln!(
			"pulsar-render: kms dump: pointer {:.0},{:.0} raw={} grabbed={} overlay front={:?} commits={}",
			self.ptr.0, self.ptr.1, self.raw_input(), self.grabbed, k.ov_front, k.frames
		);
		let Some(i) = k.ov_front else { return };
		let (w, h) = (k.w as usize, k.h as usize);
		let mut px = vec![0u8; w * h * 4];
		gl.bind_framebuffer(glow::FRAMEBUFFER, k.ov[i].fbo);
		gl.read_pixels(0, 0, w as i32, h as i32, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut px)));
		gl.bind_framebuffer(glow::FRAMEBUFFER, None);
		// Premultiplied over mid grey. Row 0 of the dumb buffer is the top of the screen.
		let rgb: Vec<u8> = px
			.chunks_exact(4)
			.flat_map(|p| {
				let a = p[3] as u32;
				[0, 1, 2].map(|c| (p[c] as u32 + 96 * (255 - a) / 255).min(255) as u8)
			})
			.collect();
		let save = |path: &str, data: &[u8], w: usize, h: usize| {
			let r = std::fs::File::create(path).map_err(|e| e.to_string()).and_then(|f| {
				let mut e = png::Encoder::new(std::io::BufWriter::new(f), w as u32, h as u32);
				e.set_color(png::ColorType::Rgb);
				e.set_depth(png::BitDepth::Eight);
				e.write_header().and_then(|mut wr| wr.write_image_data(data)).map_err(|e| e.to_string())
			});
			if let Err(e) = r {
				eprintln!("pulsar-render: kms dump {path}: {e}");
			}
		};
		save("/tmp/ov.png", &rgb, w, h);
		let c = 256usize;
		let x0 = (self.ptr.0 as usize).saturating_sub(c / 4).min(w.saturating_sub(c));
		let y0 = (self.ptr.1 as usize).saturating_sub(c / 4).min(h.saturating_sub(c));
		let mut crop = Vec::with_capacity(c * c * 3);
		for y in y0..(y0 + c).min(h) {
			crop.extend_from_slice(&rgb[(y * w + x0) * 3..(y * w + (x0 + c).min(w)) * 3]);
		}
		save("/tmp/ov-ptr.png", &crop, c.min(w), c.min(h));
	}

	/// Size to lay the egui chrome out at: the leased mode while leased (the X window may have
	/// been shrunk by the WM meanwhile — see `update`), else None (use the window size).
	pub fn draw_size(&self) -> Option<(u32, u32)> {
		self.lease.as_ref().map(|k| (k.w, k.h))
	}

	/// Hand the output back to X (normal end of a lease: session idle/end, new host, windowed,
	/// exit…).
	pub unsafe fn release(&mut self, gl: &glow::Context, why: &str) {
		if self.lease.is_none() {
			return;
		}
		self.input_end();
		if let Some(mut k) = self.lease.take() {
			eprintln!("pulsar-render: kms lease ending ({why})");
			k.release(gl);
		}
		if refit_enabled() {
			let now = std::time::Instant::now();
			self.refit = Some((now, now, 0));
		}
	}

	/// A lease operation failed: release, fall back to X, back off before retrying.
	pub unsafe fn fail(&mut self, gl: &glow::Context, why: &str) {
		self.fails += 1;
		eprintln!("pulsar-render: kms lease fallback to X: {why} (failure {}/{MAX_FAILS})", self.fails);
		self.release(gl, "failure");
		self.ready = 0;
		self.retry_at = std::time::Instant::now() + std::time::Duration::from_secs(10);
	}

	/// Once per loop iteration, before rendering. `want` = the caller's conditions (window
	/// mapped); fullscreen + frame suitability are checked here. The lease is deliberately kept
	/// across overlay open/close and input engage/release: every release re-enables the CRTC
	/// in X (a full modeset — the TV blanks for 1-3 s) and leaves the WM-shrunk toplevel small
	/// for seconds. While leased the renderer draws the local cursor itself (`pointer_px`).
	#[allow(clippy::too_many_arguments)]
	pub unsafe fn update(
		&mut self,
		want: bool,
		xd: *mut xlib::Display,
		win: u64,
		w: u32,
		h: u32,
		gl: &glow::Context,
		egl_dpy: *mut c_void,
		get_proc: &dyn Fn(&str) -> *const c_void,
	) {
		let scr = xlib::XDefaultScreen(xd);
		let (sw, sh) = (xlib::XDisplayWidth(xd, scr) as u32, xlib::XDisplayHeight(xd, scr) as u32);
		let fullscreen = w == sw && h == sh;
		if let Some(k) = self.lease.as_mut() {
			// Normally already done right after the commit; a slow flip is finished here.
			let flip = k.wait_flip(100);
			let (kw, kh) = (k.w, k.h);
			if let Err(e) = flip {
				self.fail(gl, &e);
				return;
			}
			if !want {
				self.release(gl, "window hidden");
				self.ready = 0;
				return;
			}
			if !self.grabbed {
				self.grab_tick += 1;
				if self.grab_tick % 60 == 0 {
					self.try_grab();
				}
			}
			// While we hold the output, X reports it disconnected and the WM (mutter) shrinks the
			// fullscreen window (seen: 1920x1080 → 1200x780 within ~250 ms). Releasing on that size
			// change made the lease flap about once a second. So under a lease the WM's
			// _NET_WM_STATE_FULLSCREEN decides; the window size only when there is no WM state.
			if (w, h) != self.leased_win {
				self.leased_win = (w, h);
				self.fs_tick = 0;
				eprintln!("pulsar-render: kms window is {w}x{h} under the lease");
			}
			self.fs_tick += 1;
			if self.fs_tick % 10 == 1 {
				let leave = match wm_fullscreen(xd, win) {
					Some(fs) => !fs,
					None => w != kw || h != kh,
				};
				if leave {
					self.release(gl, "window left fullscreen");
					self.ready = 0;
				}
			}
			return;
		}
		if !(self.enabled && want && fullscreen && self.frame_ok && self.fails < MAX_FAILS) {
			self.ready = 0;
			return;
		}
		self.ready += 1;
		if self.ready < READY_ITERS || std::time::Instant::now() < self.retry_at {
			return;
		}
		self.ready = 0;
		let root = xlib::XDefaultRootWindow(xd);
		let (mut x, mut y, mut child) = (0i32, 0i32, 0u64);
		xlib::XTranslateCoordinates(xd, win, root, 0, 0, &mut x, &mut y, &mut child);
		if wm_fullscreen(xd, win) == Some(false) {
			// Screen-sized but not WM-fullscreen (a maximized undecorated window): no lease.
			return;
		}
		match KmsLease::acquire(xd, root, (x, y, w, h), gl, egl_dpy, get_proc) {
			Ok(k) => {
				self.leased_win = (w, h);
				self.fs_tick = 0;
				self.lease = Some(k);
				self.refit = None;
				self.input_begin(xd, win);
			}
			Err(e) => self.fail(gl, &format!("acquire: {e}")),
		}
	}

	/// Present this frame through the lease and pace the loop: block on the page flip after a
	/// commit, otherwise sleep until the decoder publishes a frame (≤16 ms, like the X path).
	pub unsafe fn present(
		&mut self,
		gl: &glow::Context,
		frame: *mut ff::AVFrame,
		fresh: bool,
		ov: Overlay,
		ov_hash: u64,
		frame_seen: &mut u64,
	) {
		if self.lease.is_none() {
			return;
		}
		if !frame.is_null() && !frame_ok(frame) {
			// e.g. a codec switch to a software decoder: not a lease failure.
			self.frame_ok = false;
			self.release(gl, "frame no longer DRM_PRIME single-layer");
			return;
		}
		let Some(k) = self.lease.as_mut() else { return };
		match k.present(gl, frame, fresh, ov, ov_hash) {
			Ok(true) => {
				if let Err(e) = k.wait_flip(100) {
					self.fail(gl, &e);
				}
			}
			Ok(false) => {
				*frame_seen = crate::video::wait_frame(*frame_seen, std::time::Duration::from_millis(16));
			}
			Err(e) => self.fail(gl, &e),
		}
	}
}

/// `_NET_WM_STATE_FULLSCREEN` of the WM client window that contains `win` (the first ancestor
/// carrying `_NET_WM_STATE`). None = no such window / no EWMH WM.
unsafe fn wm_fullscreen(xd: *mut xlib::Display, win: u64) -> Option<bool> {
	let state = xlib::XInternAtom(xd, b"_NET_WM_STATE ".as_ptr() as *const c_char, xlib::True);
	let fs = xlib::XInternAtom(xd, b"_NET_WM_STATE_FULLSCREEN ".as_ptr() as *const c_char, xlib::True);
	if state == 0 {
		return None;
	}
	let mut w = win;
	for _ in 0..16 {
		let (mut ty, mut fmt, mut n, mut after) = (0u64, 0i32, 0u64, 0u64);
		let mut data: *mut u8 = ptr::null_mut();
		let ok = xlib::XGetWindowProperty(
			xd, w, state, 0, 64, xlib::False, xlib::XA_ATOM, &mut ty, &mut fmt, &mut n, &mut after, &mut data,
		) == 0;
		if ok && ty != 0 {
			let atoms = if data.is_null() || fmt != 32 {
				&[][..]
			} else {
				std::slice::from_raw_parts(data as *const u64, n as usize)
			};
			let r = fs != 0 && atoms.contains(&fs);
			if !data.is_null() {
				xlib::XFree(data as *mut c_void);
			}
			return Some(r);
		}
		if !data.is_null() {
			xlib::XFree(data as *mut c_void);
		}
		let (mut root, mut parent) = (0u64, 0u64);
		let mut kids: *mut u64 = ptr::null_mut();
		let mut nk = 0u32;
		if xlib::XQueryTree(xd, w, &mut root, &mut parent, &mut kids, &mut nk) == 0 {
			return None;
		}
		if !kids.is_null() {
			xlib::XFree(kids as *mut c_void);
		}
		if parent == 0 || parent == root {
			return None;
		}
		w = parent;
	}
	None
}

/// Hash of one egui frame's output, to skip re-rendering an unchanged overlay. Includes the
/// texture delta so an atlas update is never skipped.
pub fn prims_hash(prims: &[egui::ClippedPrimitive], delta: &egui::TexturesDelta) -> u64 {
	use std::hash::{Hash, Hasher};
	let mut h = std::collections::hash_map::DefaultHasher::new();
	if !delta.is_empty() {
		// Never equal to a previous frame's hash.
		static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
		N.fetch_add(1, std::sync::atomic::Ordering::Relaxed).hash(&mut h);
	}
	prims.len().hash(&mut h);
	for p in prims {
		for v in [p.clip_rect.min.x, p.clip_rect.min.y, p.clip_rect.max.x, p.clip_rect.max.y] {
			v.to_bits().hash(&mut h);
		}
		if let egui::epaint::Primitive::Mesh(m) = &p.primitive {
			m.texture_id.hash(&mut h);
			m.indices.hash(&mut h);
			for v in &m.vertices {
				v.pos.x.to_bits().hash(&mut h);
				v.pos.y.to_bits().hash(&mut h);
				v.uv.x.to_bits().hash(&mut h);
				v.uv.y.to_bits().hash(&mut h);
				v.color.to_array().hash(&mut h);
			}
		} else {
			// Paint callbacks can't be compared: always redraw.
			std::time::Instant::now().hash(&mut h);
		}
	}
	h.finish() | 1 // 0 is reserved for "empty"
}

/// Whether the lease path may be used at all (`PULSAR_KMS_LEASE=0` turns it off).
pub fn enabled() -> bool {
	!matches!(std::env::var("PULSAR_KMS_LEASE").as_deref(), Ok("0") | Ok("off") | Ok("false"))
}

/// Whether a frame can go on a KMS plane as-is (single-layer DRM_PRIME, as rkmpp produces).
pub unsafe fn frame_ok(f: *const ff::AVFrame) -> bool {
	if f.is_null() || (*f).format != ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as c_int {
		return false;
	}
	let d = (*f).data[0] as *const ff::AVDRMFrameDescriptor;
	!d.is_null() && (*d).nb_layers == 1
}

/// Whether to re-fit the toplevel after a release (`PULSAR_KMS_REFIT=0` turns it off).
fn refit_enabled() -> bool {
	!matches!(std::env::var("PULSAR_KMS_REFIT").as_deref(), Ok("0") | Ok("off") | Ok("false"))
}

/// The WM client window that contains `win` (the first ancestor carrying `_NET_WM_STATE`).
unsafe fn wm_client(xd: *mut xlib::Display, win: u64) -> Option<u64> {
	let state = xlib::XInternAtom(xd, b"_NET_WM_STATE\0".as_ptr() as *const c_char, xlib::True);
	if state == 0 {
		return None;
	}
	let mut w = win;
	for _ in 0..16 {
		let (mut ty, mut fmt, mut n, mut after) = (0u64, 0i32, 0u64, 0u64);
		let mut data: *mut u8 = ptr::null_mut();
		let ok = xlib::XGetWindowProperty(
			xd, w, state, 0, 0, xlib::False, xlib::XA_ATOM, &mut ty, &mut fmt, &mut n, &mut after, &mut data,
		) == 0;
		if !data.is_null() {
			xlib::XFree(data as *mut c_void);
		}
		if ok && ty != 0 {
			return Some(w);
		}
		let (mut root, mut parent) = (0u64, 0u64);
		let mut kids: *mut u64 = ptr::null_mut();
		let mut nk = 0u32;
		if xlib::XQueryTree(xd, w, &mut root, &mut parent, &mut kids, &mut nk) == 0 {
			return None;
		}
		if !kids.is_null() {
			xlib::XFree(kids as *mut c_void);
		}
		if parent == 0 || parent == root {
			return None;
		}
		w = parent;
	}
	None
}

// ---------------------------------------------------------------------------------------------
// XI2 raw input (libXi dlopen'ed) — the pointer source while leased.
// ---------------------------------------------------------------------------------------------

#[repr(C)]
struct XIEventMask {
	deviceid: c_int,
	mask_len: c_int,
	mask: *mut u8,
}
#[repr(C)]
struct XIValuatorState {
	mask_len: c_int,
	mask: *mut u8,
	values: *mut f64,
}
#[repr(C)]
struct XIRawEvent {
	type_: c_int,
	serial: c_ulong,
	send_event: c_int,
	display: *mut xlib::Display,
	extension: c_int,
	evtype: c_int,
	time: c_ulong,
	deviceid: c_int,
	sourceid: c_int,
	detail: c_int,
	flags: c_int,
	valuators: XIValuatorState,
	raw_values: *mut f64,
}

const XI_ALL_MASTER_DEVICES: c_int = 1;
const XI_RAW_BUTTON_PRESS: c_int = 15;
const XI_RAW_BUTTON_RELEASE: c_int = 16;
const XI_RAW_MOTION: c_int = 17;

/// One decoded raw event: relative motion in (accelerated) pixels, or a button edge.
pub enum RawEv {
	Motion(f64, f64),
	Button(u32, bool),
}

type XISelectEvents = unsafe extern "C" fn(*mut xlib::Display, u64, *mut XIEventMask, c_int) -> c_int;

struct RawInput {
	opcode: c_int,
	select: XISelectEvents,
	on: bool,
}

impl RawInput {
	unsafe fn new(xd: *mut xlib::Display) -> Result<RawInput, String> {
		let (mut op, mut ev, mut er) = (0, 0, 0);
		if xlib::XQueryExtension(xd, b"XInputExtension\0".as_ptr() as *const c_char, &mut op, &mut ev, &mut er) == 0 {
			return Err("no XInputExtension".into());
		}
		let h = dlopen("libXi.so.6")?;
		let query: unsafe extern "C" fn(*mut xlib::Display, *mut c_int, *mut c_int) -> c_int =
			sym(h, "XIQueryVersion")?;
		let select: XISelectEvents = sym(h, "XISelectEvents")?;
		// 2.1+: raw events reach root selections regardless of grabs (ours included).
		let (mut ma, mut mi) = (2, 2);
		if query(xd, &mut ma, &mut mi) != 0 || ma < 2 || (ma == 2 && mi < 1) {
			return Err(format!("XI {ma}.{mi} < 2.1"));
		}
		Ok(RawInput { opcode: op, select, on: false })
	}

	unsafe fn set(&mut self, xd: *mut xlib::Display, on: bool) {
		let mut m = [0u8; 3];
		if on {
			for e in [XI_RAW_BUTTON_PRESS, XI_RAW_BUTTON_RELEASE, XI_RAW_MOTION] {
				m[(e >> 3) as usize] |= 1 << (e & 7);
			}
		}
		let mut em = XIEventMask { deviceid: XI_ALL_MASTER_DEVICES, mask_len: m.len() as c_int, mask: m.as_mut_ptr() };
		(self.select)(xd, xlib::XDefaultRootWindow(xd), &mut em, 1);
		xlib::XFlush(xd);
		self.on = on;
	}

	unsafe fn decode(&self, xd: *mut xlib::Display, ev: &mut xlib::XEvent) -> Option<RawEv> {
		if !self.on || ev.get_type() != xlib::GenericEvent {
			return None;
		}
		let c = &mut *(ev as *mut xlib::XEvent as *mut xlib::XGenericEventCookie);
		if c.extension != self.opcode || xlib::XGetEventData(xd, c) == 0 {
			return None;
		}
		let r = &*(c.data as *const XIRawEvent);
		let out = match c.evtype {
			XI_RAW_MOTION => {
				// `values` holds one entry per SET mask bit, in bit order; bits 0/1 = x/y.
				let v = &r.valuators;
				let (mut d, mut k) = ([0.0f64; 2], 0usize);
				for bit in 0..2usize.min(v.mask_len.max(0) as usize * 8) {
					if *v.mask.add(bit >> 3) & (1 << (bit & 7)) != 0 {
						d[bit] = *v.values.add(k);
						k += 1;
					}
				}
				Some(RawEv::Motion(d[0], d[1]))
			}
			XI_RAW_BUTTON_PRESS | XI_RAW_BUTTON_RELEASE => {
				Some(RawEv::Button(r.detail as u32, c.evtype == XI_RAW_BUTTON_PRESS))
			}
			_ => None,
		};
		xlib::XFreeEventData(xd, c);
		out
	}
}

/// Debug hooks (only with PULSAR_KMS_DEBUG=1): SIGRTMIN+1 dumps the overlay buffer
/// (`Ctl::debug_dump`), SIGRTMIN+2 toggles a forced "disengaged" (local cursor shown),
/// SIGRTMIN+3 asks the app to end the session (`ov end`, as the overlay's end button does),
/// SIGRTMIN+4 releases the lease mid-session and holds off re-leasing for 15 s.
pub static DBG_DUMP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub static DBG_ENGAGE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub static DBG_END: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub static DBG_RELEASE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_dbg(sig: c_int) {
	use std::sync::atomic::Ordering::SeqCst;
	let base = libc::SIGRTMIN();
	if sig == base + 1 {
		DBG_DUMP.store(true, SeqCst);
	} else if sig == base + 2 {
		DBG_ENGAGE.store(true, SeqCst);
	} else if sig == base + 3 {
		DBG_END.store(true, SeqCst);
	} else if sig == base + 4 {
		DBG_RELEASE.store(true, SeqCst);
	}
}

pub fn debug_signals() {
	if std::env::var("PULSAR_KMS_DEBUG").as_deref() != Ok("1") {
		return;
	}
	unsafe {
		for i in 1..=4 {
			libc::signal(libc::SIGRTMIN() + i, on_dbg as *const () as usize);
		}
	}
	eprintln!("pulsar-render: kms debug signals on (RTMIN+1 dump, +2 disengage toggle, +3 end, +4 release)");
}
