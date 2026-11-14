//! Metal presentation for the macOS renderer — the analogue of `win/present.rs` +
//! `win/fsr.rs` + `win/egui_paint.rs` in one place:
//!   - the decoded `CVPixelBuffer` (NV12 / P010, IOSurface-backed) is wrapped as two Metal
//!     textures through a `CVMetalTextureCache` (no copy) and converted YCbCr → RGB by a
//!     fragment shader straight onto the `CAMetalLayer` drawable, at the fit / letterbox rect,
//!     rotated upright;
//!   - picture upscaling = AMD FSR 1 (EASU + RCAS), the HLSL of `win/fsr.rs` ported to MSL:
//!     the picture is converted into a source-sized texture, EASU scales it to the on-screen
//!     size, RCAS sharpens it onto the drawable. A shader failure falls back to plain scaling;
//!   - the egui overlay is painted over it in the same pass by a minimal egui painter with the
//!     convention of the Windows one (premultiplied sRGBA treated as linear, non-sRGB target).
//!
//! The textures a frame samples stay referenced until its command buffer has completed
//! (`inflight`), so neither the pixel buffer nor its Metal views are recycled under the GPU.

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::*;
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};

use super::decode::Frame;
use super::ffi::*;
use super::view;

type Device = ProtocolObject<dyn MTLDevice>;
type Tex = Retained<ProtocolObject<dyn MTLTexture>>;
type Pso = Retained<ProtocolObject<dyn MTLRenderPipelineState>>;

const SHADERS: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct VOut { float4 pos [[position]]; float2 uv; };

struct Quad { float4 pos[4]; float4 uv[4]; };
vertex VOut video_vs(uint vid [[vertex_id]], constant Quad& q [[buffer(0)]]) {
	VOut o;
	o.pos = float4(q.pos[vid].xy, 0.0, 1.0);
	o.uv = q.uv[vid].xy;
	return o;
}

struct Yuv { float4 range; float4 mat; };
fragment float4 video_fs(VOut i [[stage_in]], texture2d<float> ty [[texture(0)]], texture2d<float> tc [[texture(1)]],
		sampler s [[sampler(0)]], constant Yuv& p [[buffer(0)]]) {
	float y = ty.sample(s, i.uv).r * p.range.x + p.range.y;
	float2 c = tc.sample(s, i.uv).rg * p.range.z + p.range.w;
	float3 rgb = float3(y + p.mat.x * c.y, y - p.mat.y * c.x - p.mat.z * c.y, y + p.mat.w * c.x);
	return float4(saturate(rgb), 1.0);
}

// ---- egui ----
struct EguiV { packed_float2 pos; packed_float2 uv; uchar4 color; };
struct EguiOut { float4 pos [[position]]; float2 uv; float4 color; };
vertex EguiOut egui_vs(uint vid [[vertex_id]], const device EguiV* v [[buffer(0)]], constant float2& screen [[buffer(1)]]) {
	EguiV e = v[vid];
	EguiOut o;
	o.pos = float4(2.0 * e.pos[0] / screen.x - 1.0, 1.0 - 2.0 * e.pos[1] / screen.y, 0.0, 1.0);
	o.uv = float2(e.uv[0], e.uv[1]);
	o.color = float4(e.color) / 255.0;
	return o;
}
fragment float4 egui_fs(EguiOut i [[stage_in]], texture2d<float> t [[texture(0)]], sampler s [[sampler(0)]]) {
	return t.sample(s, i.uv) * i.color;
}
"#;

const FSR_SHADERS: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct VOut { float4 pos [[position]]; float2 uv; };

vertex VOut full_vs(uint vid [[vertex_id]]) {
	VOut o;
	float2 uv = float2(float((vid << 1) & 2), float(vid & 2));
	o.uv = uv;
	o.pos = float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
	return o;
}

// ---- AMD FidelityFX Super Resolution 1.0.2 (MIT, Copyright (c) 2021 Advanced Micro Devices,
// Inc.), after agyild's mpv port, RGB like the original EASU: the same math as win/fsr.rs.
struct Fsr { float4 con0; float4 con1; };
float rcpLo(float a) { return as_type<float>(0x7ef07ebbu - as_type<uint>(a)); }
float rsqLo(float a) { return as_type<float>(0x5f347d74u - (as_type<uint>(a) >> 1)); }
float rcpMed(float a) { float b = as_type<float>(0x7ef19fffu - as_type<uint>(a)); return b * (-b * a + 2.0); }
float luma(float3 c) { return c.g + 0.5 * (c.r + c.b); }

void easuTap(thread float3& aC, thread float& aW, float2 off, float2 dir, float2 len, float lob, float clp, float3 c) {
	float2 v;
	v.x = off.x * dir.x + off.y * dir.y;
	v.y = off.x * (-dir.y) + off.y * dir.x;
	v *= len;
	float d2 = min(v.x * v.x + v.y * v.y, clp);
	float wB = (2.0 / 5.0) * d2 - 1.0;
	float wA = lob * d2 - 1.0;
	wB *= wB;
	wA *= wA;
	wB = (25.0 / 16.0) * wB - (25.0 / 16.0 - 1.0);
	float w = wB * wA;
	aC += c * w;
	aW += w;
}

void easuSet(thread float2& dir, thread float& len, float w, float lA, float lB, float lC, float lD, float lE) {
	float dc = lD - lC;
	float cb = lC - lB;
	float lenX = rcpLo(max(abs(dc), abs(cb)));
	float dirX = lD - lB;
	lenX = saturate(abs(dirX) * lenX);
	lenX *= lenX;
	float ec = lE - lC;
	float ca = lC - lA;
	float lenY = rcpLo(max(abs(ec), abs(ca)));
	float dirY = lE - lA;
	lenY = saturate(abs(dirY) * lenY);
	lenY *= lenY;
	dir += float2(dirX, dirY) * w;
	len += dot(float2(w, w), float2(lenX, lenY));
}

float3 tapAt(texture2d<float> tex, sampler pt, float4 con0, float2 p) {
	return tex.sample(pt, (p + 0.5) * con0.zw, level(0)).rgb;
}

fragment float4 easu_fs(VOut i [[stage_in]], texture2d<float> tex [[texture(0)]], sampler pt [[sampler(0)]],
		constant Fsr& k [[buffer(0)]]) {
	float2 pp = i.uv * k.con0.xy - 0.5;
	float2 fp = floor(pp);
	pp -= fp;
	float3 b = tapAt(tex, pt, k.con0, fp + float2(0, -1)), c = tapAt(tex, pt, k.con0, fp + float2(1, -1));
	float3 e = tapAt(tex, pt, k.con0, fp + float2(-1, 0)), f = tapAt(tex, pt, k.con0, fp), g = tapAt(tex, pt, k.con0, fp + float2(1, 0)), h = tapAt(tex, pt, k.con0, fp + float2(2, 0));
	float3 ii = tapAt(tex, pt, k.con0, fp + float2(-1, 1)), j = tapAt(tex, pt, k.con0, fp + float2(0, 1)), kk = tapAt(tex, pt, k.con0, fp + float2(1, 1)), l = tapAt(tex, pt, k.con0, fp + float2(2, 1));
	float3 n = tapAt(tex, pt, k.con0, fp + float2(0, 2)), o = tapAt(tex, pt, k.con0, fp + float2(1, 2));
	float bL = luma(b), cL = luma(c), eL = luma(e), fL = luma(f), gL = luma(g), hL = luma(h);
	float iL = luma(ii), jL = luma(j), kL = luma(kk), lL = luma(l), nL = luma(n), oL = luma(o);
	float2 dir = float2(0.0);
	float len = 0.0;
	easuSet(dir, len, (1.0 - pp.x) * (1.0 - pp.y), bL, eL, fL, gL, jL);
	easuSet(dir, len, pp.x * (1.0 - pp.y), cL, fL, gL, hL, kL);
	easuSet(dir, len, (1.0 - pp.x) * pp.y, fL, iL, jL, kL, nL);
	easuSet(dir, len, pp.x * pp.y, gL, jL, kL, lL, oL);
	float2 dir2 = dir * dir;
	float dirR = dir2.x + dir2.y;
	bool zro = dirR < (1.0 / 32768.0);
	dirR = rsqLo(dirR);
	dirR = zro ? 1.0 : dirR;
	dir.x = zro ? 1.0 : dir.x;
	dir *= dirR;
	len = len * 0.5;
	len *= len;
	float stretch = (dir.x * dir.x + dir.y * dir.y) * rcpLo(max(abs(dir.x), abs(dir.y)));
	float2 len2 = float2(1.0 + (stretch - 1.0) * len, 1.0 - 0.5 * len);
	float lob = 0.5 + ((1.0 / 4.0 - 0.04) - 0.5) * len;
	float clp = rcpLo(lob);
	float3 aC = float3(0.0);
	float aW = 0.0;
	easuTap(aC, aW, float2(0, -1) - pp, dir, len2, lob, clp, b);
	easuTap(aC, aW, float2(1, -1) - pp, dir, len2, lob, clp, c);
	easuTap(aC, aW, float2(-1, 1) - pp, dir, len2, lob, clp, ii);
	easuTap(aC, aW, float2(0, 1) - pp, dir, len2, lob, clp, j);
	easuTap(aC, aW, float2(0, 0) - pp, dir, len2, lob, clp, f);
	easuTap(aC, aW, float2(-1, 0) - pp, dir, len2, lob, clp, e);
	easuTap(aC, aW, float2(1, 1) - pp, dir, len2, lob, clp, kk);
	easuTap(aC, aW, float2(2, 1) - pp, dir, len2, lob, clp, l);
	easuTap(aC, aW, float2(2, 0) - pp, dir, len2, lob, clp, h);
	easuTap(aC, aW, float2(1, 0) - pp, dir, len2, lob, clp, g);
	easuTap(aC, aW, float2(1, 2) - pp, dir, len2, lob, clp, o);
	easuTap(aC, aW, float2(0, 2) - pp, dir, len2, lob, clp, n);
	float3 pix = aC / aW;
	float3 mn = min(min(f, g), min(j, kk));
	float3 mx = max(max(f, g), max(j, kk));
	return float4(saturate(clamp(pix, mn, mx)), 1.0);
}

float3 ld(texture2d<float> tex, int2 lim, int2 p) { return tex.read(uint2(clamp(p, int2(0, 0), lim))).rgb; }
constant float RCAS_LIMIT = 0.25 - 1.0 / 16.0;
fragment float4 rcas_fs(VOut i [[stage_in]], texture2d<float> tex [[texture(0)]], constant Fsr& k [[buffer(0)]]) {
	int2 lim = int2(k.con1.xy) - 1;
	int2 p = int2(i.uv * k.con1.xy);
	float3 b = ld(tex, lim, p + int2(0, -1)), d = ld(tex, lim, p + int2(-1, 0)), e = ld(tex, lim, p), f = ld(tex, lim, p + int2(1, 0)), h = ld(tex, lim, p + int2(0, 1));
	float3 mn = min(min(b, d), min(f, h));
	float3 mx = max(max(b, d), max(f, h));
	float3 hitMin = min(mn, e) / max(4.0 * mx, float3(1e-5));
	float3 hitMax = (1.0 - max(mx, e)) / min(4.0 * mn - 4.0, float3(-1e-5));
	float3 lobeC = max(-hitMin, hitMax);
	float lobe = max(-RCAS_LIMIT, min(max(lobeC.r, max(lobeC.g, lobeC.b)), 0.0)) * exp2(-k.con1.z);
	float bL = luma(b), dL = luma(d), eL = luma(e), fL = luma(f), hL = luma(h);
	float nz = 0.25 * (bL + dL + fL + hL) - eL;
	float rng = max(max(max(bL, dL), max(eL, fL)), hL) - min(min(min(bL, dL), min(eL, fL)), hL);
	nz = saturate(abs(nz) * rcpMed(max(rng, 1e-5)));
	nz = -0.5 * nz + 1.0;
	lobe *= nz;
	float rcpL = rcpMed(4.0 * lobe + 1.0);
	return float4(saturate((lobe * (b + d + h + f) + e) * rcpL), 1.0);
}
"#;

/// FSR sharpening in stops (0 = strongest), as on Windows.
const SHARPNESS: f32 = 0.2;

#[repr(C)]
struct Quad {
	pos: [[f32; 4]; 4],
	uv: [[f32; 4]; 4],
}

#[repr(C)]
struct YuvParams {
	range: [f32; 4],
	mat: [f32; 4],
}

#[repr(C)]
struct FsrParams {
	con0: [f32; 4],
	con1: [f32; 4],
}

/// The background the renderer shows before the first picture (as on Windows).
pub const IDLE_CLEAR: [f64; 4] = [0.02, 0.02, 0.03, 1.0];

/// What one frame draws: the picture (and where), the clear colour, the overlay.
pub struct VideoDraw<'a> {
	pub frame: &'a Frame,
	/// Source crop in DISPLAYED (upright) pixels, and the dest rect on the drawable (pixels).
	pub src: (f32, f32, f32, f32),
	pub dst: (f32, f32, f32, f32),
}

pub struct Gpu {
	pub device: Retained<Device>,
	queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
	pub layer: Retained<CAMetalLayer>,
	video: Pso,
	/// Same shader, writing into the FSR source texture (same pixel format, own state for clarity).
	easu: Option<Pso>,
	rcas: Option<Pso>,
	egui: Pso,
	lin: Retained<ProtocolObject<dyn MTLSamplerState>>,
	pt: Retained<ProtocolObject<dyn MTLSamplerState>>,
	cache: Cf,
	fsr_src: Option<Tex>,
	fsr_mid: Option<Tex>,
	fsr_failed: bool,
	logged_fsr: Option<bool>,
	inflight: VecDeque<(Retained<ProtocolObject<dyn MTLCommandBuffer>>, Vec<Cf>)>,
	textures: HashMap<egui::TextureId, Tex>,
	/// A command buffer ended in error (GPU reset / removed): the caller rebuilds.
	pub lost: bool,
}

fn ns(s: &str) -> Retained<NSString> {
	NSString::from_str(s)
}

fn pso(
	device: &Device,
	lib: &ProtocolObject<dyn MTLLibrary>,
	vs: &str,
	fs: &str,
	blend: bool,
) -> Result<Pso, String> {
	let v = lib.newFunctionWithName(&ns(vs)).ok_or_else(|| format!("no {vs}"))?;
	let f = lib.newFunctionWithName(&ns(fs)).ok_or_else(|| format!("no {fs}"))?;
	let d = MTLRenderPipelineDescriptor::new();
	d.setVertexFunction(Some(&v));
	d.setFragmentFunction(Some(&f));
	// SAFETY: index 0 always exists.
	let ca = unsafe { d.colorAttachments().objectAtIndexedSubscript(0) };
	ca.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
	if blend {
		ca.setBlendingEnabled(true);
		ca.setSourceRGBBlendFactor(MTLBlendFactor::One);
		ca.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
		ca.setSourceAlphaBlendFactor(MTLBlendFactor::One);
		ca.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
	}
	device
		.newRenderPipelineStateWithDescriptor_error(&d)
		.map_err(|e| format!("pipeline {fs}: {}", e.localizedDescription()))
}

fn sampler(device: &Device, filter: MTLSamplerMinMagFilter) -> Result<Retained<ProtocolObject<dyn MTLSamplerState>>, String> {
	let d = MTLSamplerDescriptor::new();
	d.setMinFilter(filter);
	d.setMagFilter(filter);
	d.setSAddressMode(MTLSamplerAddressMode::ClampToEdge);
	d.setTAddressMode(MTLSamplerAddressMode::ClampToEdge);
	device.newSamplerStateWithDescriptor(&d).ok_or_else(|| "sampler".to_string())
}

fn ndc_quad(dst: (f32, f32, f32, f32), w: f32, h: f32) -> [[f32; 4]; 4] {
	let (x, y, dw, dh) = dst;
	let (x0, y0) = (2.0 * x / w - 1.0, 1.0 - 2.0 * y / h);
	let (x1, y1) = (2.0 * (x + dw) / w - 1.0, 1.0 - 2.0 * (y + dh) / h);
	[[x0, y0, 0.0, 0.0], [x1, y0, 0.0, 0.0], [x0, y1, 0.0, 0.0], [x1, y1, 0.0, 0.0]]
}

fn viewport(x: f64, y: f64, w: f64, h: f64) -> MTLViewport {
	MTLViewport { originX: x, originY: y, width: w, height: h, znear: 0.0, zfar: 1.0 }
}

unsafe fn bytes<T>(v: &T) -> (NonNull<c_void>, usize) {
	(NonNull::from(v).cast(), std::mem::size_of::<T>())
}

impl Gpu {
	/// Bring up Metal on `device` (the GPU driving the panel's display) for `layer`.
	pub fn new(device: Retained<Device>, layer: Retained<CAMetalLayer>) -> Result<Self, String> {
		let queue = device.newCommandQueue().ok_or("no command queue")?;
		let lib = device
			.newLibraryWithSource_options_error(&ns(SHADERS), None)
			.map_err(|e| format!("shader compile: {}", e.localizedDescription()))?;
		let video = pso(&device, &lib, "video_vs", "video_fs", false)?;
		let egui = pso(&device, &lib, "egui_vs", "egui_fs", true)?;
		// FSR is optional and in its own library: if it doesn't build on this GPU the picture
		// still shows, with plain scaling.
		let (easu, rcas) = match device.newLibraryWithSource_options_error(&ns(FSR_SHADERS), None) {
			Ok(fl) => (
				pso(&device, &fl, "full_vs", "easu_fs", false)
					.map_err(|e| eprintln!("pulsar-render(mac): FSR EASU unavailable: {e}"))
					.ok(),
				pso(&device, &fl, "full_vs", "rcas_fs", false)
					.map_err(|e| eprintln!("pulsar-render(mac): FSR RCAS unavailable: {e}"))
					.ok(),
			),
			Err(e) => {
				eprintln!("pulsar-render(mac): FSR shaders unavailable: {}", e.localizedDescription());
				(None, None)
			}
		};
		let lin = sampler(&device, MTLSamplerMinMagFilter::Linear)?;
		let pt = sampler(&device, MTLSamplerMinMagFilter::Nearest)?;
		let mut cache: CVMetalTextureCacheRef = std::ptr::null_mut();
		// SAFETY: a live MTLDevice pointer; CV retains it.
		let r = unsafe {
			CVMetalTextureCacheCreate(
				kCFAllocatorDefault,
				std::ptr::null(),
				Retained::as_ptr(&device) as *mut c_void,
				std::ptr::null(),
				&mut cache,
			)
		};
		if r != 0 || cache.is_null() {
			return Err(format!("CVMetalTextureCacheCreate failed ({r})"));
		}
		layer.setDevice(Some(&device));
		layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
		layer.setFramebufferOnly(true);
		layer.setOpaque(true);
		// Never let presentation pace the DECODE (see win/mod.rs show_frame): the pacer meters
		// pictures by the display link, the layer presents at once.
		layer.setDisplaySyncEnabled(false);
		layer.setMaximumDrawableCount(3);
		layer.setAllowsNextDrawableTimeout(true);
		Ok(Self {
			device,
			queue,
			layer,
			video,
			easu,
			rcas,
			egui,
			lin,
			pt,
			cache: Cf(cache as CFTypeRef),
			fsr_src: None,
			fsr_mid: None,
			fsr_failed: false,
			logged_fsr: None,
			inflight: VecDeque::new(),
			textures: HashMap::new(),
			lost: false,
		})
	}

	/// Drop the references of finished frames; wait for the oldest beyond a small ring (a GPU
	/// far behind) so the pool of pixel buffers is never starved.
	fn retire_inflight(&mut self) {
		while let Some((cb, _)) = self.inflight.front() {
			let st = cb.status();
			if st == MTLCommandBufferStatus::Error {
				let e = cb.error().map(|e| e.localizedDescription().to_string()).unwrap_or_default();
				eprintln!("pulsar-render(mac): GPU command buffer failed: {e}");
				self.lost = true;
			}
			if st == MTLCommandBufferStatus::Completed || st == MTLCommandBufferStatus::Error {
				self.inflight.pop_front();
			} else if self.inflight.len() > 4 {
				cb.waitUntilCompleted();
			} else {
				break;
			}
		}
	}

	fn plane(&self, frame: &Frame, plane: usize, fmt: MTLPixelFormat) -> Option<(Tex, Cf)> {
		// SAFETY: a live pixel buffer + cache; the CVMetalTexture is kept alive by the caller
		// for as long as the GPU may sample its Metal texture.
		unsafe {
			let w = CVPixelBufferGetWidthOfPlane(frame.ptr(), plane);
			let h = CVPixelBufferGetHeightOfPlane(frame.ptr(), plane);
			let mut t: CVMetalTextureRef = std::ptr::null_mut();
			let r = CVMetalTextureCacheCreateTextureFromImage(
				kCFAllocatorDefault,
				self.cache.ptr() as CVMetalTextureCacheRef,
				frame.ptr(),
				std::ptr::null(),
				fmt.0,
				w,
				h,
				plane,
				&mut t,
			);
			if r != 0 || t.is_null() {
				return None;
			}
			let keep = Cf(t as CFTypeRef);
			let mt = CVMetalTextureGetTexture(t) as *mut ProtocolObject<dyn MTLTexture>;
			let tex = Retained::retain(mt)?;
			Some((tex, keep))
		}
	}

	fn target(device: &Device, w: u32, h: u32) -> Option<Tex> {
		// SAFETY: a plain 2D render-target descriptor.
		unsafe {
			let d = MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
				MTLPixelFormat::BGRA8Unorm,
				w.max(1) as usize,
				h.max(1) as usize,
				false,
			);
			d.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
			d.setStorageMode(MTLStorageMode::Private);
			device.newTextureWithDescriptor(&d)
		}
	}

	fn sized(slot: &mut Option<Tex>, w: u32, h: u32, make: impl FnOnce() -> Option<Tex>) -> Option<Tex> {
		let ok = slot.as_ref().is_some_and(|t| t.width() == w as usize && t.height() == h as usize);
		if !ok {
			*slot = make();
		}
		slot.clone()
	}

	/// Apply an egui `TexturesDelta` (upload `set`, free `free`).
	pub fn update_textures(&mut self, delta: &mut egui::TexturesDelta) {
		let set = std::mem::take(&mut delta.set);
		let free = std::mem::take(&mut delta.free);
		for (id, d) in set.iter().flat_map(|(id, ds)| ds.iter().map(move |d| (id, d))) {
			let [w, h] = d.image.size();
			if w == 0 || h == 0 {
				continue;
			}
			let pixels: Vec<u8> = match &d.image {
				egui::ImageData::Color(img) => img.pixels.iter().flat_map(|c| c.to_array()).collect(),
			};
			let (x, y) = match d.pos {
				Some([x, y]) => (x, y),
				None => {
					// SAFETY: a plain RGBA8 sampled texture (default managed/shared storage, so
					// replaceRegion works).
					let t = unsafe {
						let desc = MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
							MTLPixelFormat::RGBA8Unorm,
							w,
							h,
							false,
						);
						desc.setUsage(MTLTextureUsage::ShaderRead);
						self.device.newTextureWithDescriptor(&desc)
					};
					let Some(t) = t else { continue };
					self.textures.insert(*id, t);
					(0, 0)
				}
			};
			let Some(t) = self.textures.get(id) else { continue };
			let region = MTLRegion {
				origin: MTLOrigin { x, y, z: 0 },
				size: MTLSize { width: w, height: h, depth: 1 },
			};
			// SAFETY: `pixels` holds w×h RGBA8 texels; the region lies inside the texture
			// (egui only patches inside the image it created).
			unsafe {
				t.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
					region,
					0,
					NonNull::new(pixels.as_ptr() as *mut c_void).unwrap(),
					w * 4,
				);
			}
		}
		for id in &free {
			self.textures.remove(id);
		}
	}

	fn paint_egui(
		&self,
		enc: &ProtocolObject<dyn MTLRenderCommandEncoder>,
		prims: &[egui::ClippedPrimitive],
		size: (u32, u32),
		ppp: f32,
	) {
		if prims.is_empty() || size.0 == 0 || size.1 == 0 {
			return;
		}
		let mut verts: Vec<egui::epaint::Vertex> = Vec::new();
		let mut idx: Vec<u32> = Vec::new();
		let mut draws = Vec::new();
		for p in prims {
			let egui::epaint::Primitive::Mesh(m) = &p.primitive else { continue };
			if m.vertices.is_empty() || m.indices.is_empty() {
				continue;
			}
			draws.push((p.clip_rect, m.texture_id, verts.len(), idx.len(), m.indices.len()));
			verts.extend_from_slice(&m.vertices);
			idx.extend_from_slice(&m.indices);
		}
		if draws.is_empty() {
			return;
		}
		let opts = MTLResourceOptions::StorageModeShared;
		// SAFETY: the slices are valid for their byte lengths; Metal copies them.
		let (vb, ib) = unsafe {
			(
				self.device.newBufferWithBytes_length_options(
					NonNull::new(verts.as_ptr() as *mut c_void).unwrap(),
					std::mem::size_of_val(verts.as_slice()),
					opts,
				),
				self.device.newBufferWithBytes_length_options(
					NonNull::new(idx.as_ptr() as *mut c_void).unwrap(),
					std::mem::size_of_val(idx.as_slice()),
					opts,
				),
			)
		};
		let (Some(vb), Some(ib)) = (vb, ib) else { return };
		let screen = [size.0 as f32 / ppp, size.1 as f32 / ppp];
		enc.setRenderPipelineState(&self.egui);
		enc.setViewport(viewport(0.0, 0.0, size.0 as f64, size.1 as f64));
		// SAFETY: buffer indices / bytes match the egui shaders' bindings.
		unsafe {
			let (p, n) = bytes(&screen);
			enc.setVertexBytes_length_atIndex(p, n, 1);
			enc.setFragmentSamplerState_atIndex(Some(&self.lin), 0);
		}
		let stride = std::mem::size_of::<egui::epaint::Vertex>();
		for (clip, tex, v0, i0, n) in draws {
			let Some(t) = self.textures.get(&tex) else { continue };
			let x0 = (clip.min.x * ppp).round().clamp(0.0, size.0 as f32) as usize;
			let y0 = (clip.min.y * ppp).round().clamp(0.0, size.1 as f32) as usize;
			let x1 = (clip.max.x * ppp).round().clamp(0.0, size.0 as f32) as usize;
			let y1 = (clip.max.y * ppp).round().clamp(0.0, size.1 as f32) as usize;
			if x1 <= x0 || y1 <= y0 {
				continue;
			}
			enc.setScissorRect(MTLScissorRect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 });
			// SAFETY: offsets inside the buffers built above; indices are relative to the mesh's
			// first vertex (the vertex buffer offset).
			unsafe {
				enc.setVertexBuffer_offset_atIndex(Some(&vb), v0 * stride, 0);
				enc.setFragmentTexture_atIndex(Some(t), 0);
				enc.drawIndexedPrimitives_indexCount_indexType_indexBuffer_indexBufferOffset(
					MTLPrimitiveType::Triangle,
					n,
					MTLIndexType::UInt32,
					&ib,
					i0 * 4,
				);
			}
		}
	}

	fn draw_video(
		&self,
		enc: &ProtocolObject<dyn MTLRenderCommandEncoder>,
		y: &Tex,
		c: &Tex,
		yuv: &YuvParams,
		quad: &Quad,
		vp: MTLViewport,
	) {
		enc.setRenderPipelineState(&self.video);
		enc.setViewport(vp);
		// SAFETY: bindings match video_vs / video_fs.
		unsafe {
			let (p, n) = bytes(quad);
			enc.setVertexBytes_length_atIndex(p, n, 0);
			let (p, n) = bytes(yuv);
			enc.setFragmentBytes_length_atIndex(p, n, 0);
			enc.setFragmentTexture_atIndex(Some(y), 0);
			enc.setFragmentTexture_atIndex(Some(c), 1);
			enc.setFragmentSamplerState_atIndex(Some(&self.lin), 0);
			enc.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::TriangleStrip, 0, 4);
		}
	}

	fn pass(tex: &ProtocolObject<dyn MTLTexture>, clear: Option<[f64; 4]>) -> Retained<MTLRenderPassDescriptor> {
		let d = MTLRenderPassDescriptor::renderPassDescriptor();
		// SAFETY: index 0 always exists.
		let ca = unsafe { d.colorAttachments().objectAtIndexedSubscript(0) };
		ca.setTexture(Some(tex));
		match clear {
			Some(c) => {
				ca.setLoadAction(MTLLoadAction::Clear);
				ca.setClearColor(MTLClearColor { red: c[0], green: c[1], blue: c[2], alpha: c[3] });
			}
			None => ca.setLoadAction(MTLLoadAction::DontCare),
		}
		ca.setStoreAction(MTLStoreAction::Store);
		d
	}

	/// Draw one frame on the next drawable and present it. `size` = the drawable size in
	/// pixels. Returns false when no drawable was available (the frame is skipped, like a busy
	/// swapchain on Windows) or nothing could be drawn.
	pub fn render(
		&mut self,
		size: (u32, u32),
		video: Option<VideoDraw<'_>>,
		upscale: bool,
		overlay: Option<(&[egui::ClippedPrimitive], f32)>,
	) -> bool {
		self.retire_inflight();
		if size.0 == 0 || size.1 == 0 {
			return false;
		}
		let Some(drawable) = self.layer.nextDrawable() else { return false };
		let Some(cb) = self.queue.commandBuffer() else { return false };
		let target = drawable.texture();
		let (tw, th) = (target.width() as f32, target.height() as f32);
		let mut keep: Vec<Cf> = Vec::new();

		// The picture's planes and colour constants.
		let pic = video.and_then(|v| {
			let fmt = v.frame.pixel_format();
			let p010 = fmt == PIX_X420 || fmt == PIX_XF20;
			let full = fmt == PIX_420F || fmt == PIX_XF20;
			let (fy, fc) = if p010 {
				(MTLPixelFormat::R16Unorm, MTLPixelFormat::RG16Unorm)
			} else {
				(MTLPixelFormat::R8Unorm, MTLPixelFormat::RG8Unorm)
			};
			let (y, ky) = self.plane(v.frame, 0, fy)?;
			let (c, kc) = self.plane(v.frame, 1, fc)?;
			keep.push(ky);
			keep.push(kc);
			let (range, mat) = view::yuv_consts(v.frame.matrix(), if p010 { 10 } else { 8 }, full, p010);
			let (iw, ih) = v.frame.size();
			let uvs = view::quad_uvs(view::rotation(), iw as f32, ih as f32, v.src);
			Some((v, y, c, YuvParams { range, mat }, uvs))
		});
		let drew_video = pic.is_some();

		let mut fsr_done = false;
		if let Some((v, y, c, yuv, uvs)) = pic.as_ref() {
			let want = upscale && view::scales_up(v.src, v.dst) && !self.fsr_failed && self.easu.is_some() && self.rcas.is_some();
			if self.logged_fsr != Some(want) {
				self.logged_fsr = Some(want);
				crate::overlay::set_upscaler_active(if want { "FSR" } else { "" });
				eprintln!(
					"pulsar-render(mac): upscaler {} ({:.0}x{:.0} -> {:.0}x{:.0})",
					if want { "FSR" } else { "none" },
					v.src.2,
					v.src.3,
					v.dst.2,
					v.dst.3
				);
			}
			if want {
				let (cw, ch) = (v.src.2.round().max(1.0) as u32, v.src.3.round().max(1.0) as u32);
				let (dw, dh) = (v.dst.2.round().max(1.0) as u32, v.dst.3.round().max(1.0) as u32);
				let dev = &self.device;
				let src = Self::sized(&mut self.fsr_src, cw, ch, || Self::target(dev, cw, ch));
				let mid = Self::sized(&mut self.fsr_mid, dw, dh, || Self::target(dev, dw, dh));
				match (src, mid) {
					(Some(src), Some(mid)) => {
						let quad = Quad {
							pos: [[-1.0, 1.0, 0.0, 0.0], [1.0, 1.0, 0.0, 0.0], [-1.0, -1.0, 0.0, 0.0], [1.0, -1.0, 0.0, 0.0]],
							uv: uvs.map(|u| [u[0], u[1], 0.0, 0.0]),
						};
						// 1: convert the crop into a source-sized RGB texture.
						if let Some(enc) = cb.renderCommandEncoderWithDescriptor(&Self::pass(&src, None)) {
							self.draw_video(&enc, y, c, yuv, &quad, viewport(0.0, 0.0, cw as f64, ch as f64));
							enc.endEncoding();
						}
						let k = FsrParams {
							con0: [cw as f32, ch as f32, 1.0 / cw as f32, 1.0 / ch as f32],
							con1: [dw as f32, dh as f32, SHARPNESS, 0.0],
						};
						// 2: EASU to the on-screen size.
						if let Some(enc) = cb.renderCommandEncoderWithDescriptor(&Self::pass(&mid, None)) {
							enc.setRenderPipelineState(self.easu.as_ref().unwrap());
							enc.setViewport(viewport(0.0, 0.0, dw as f64, dh as f64));
							// SAFETY: bindings match easu_fs.
							unsafe {
								let (p, n) = bytes(&k);
								enc.setFragmentBytes_length_atIndex(p, n, 0);
								enc.setFragmentTexture_atIndex(Some(&src), 0);
								enc.setFragmentSamplerState_atIndex(Some(&self.pt), 0);
								enc.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 3);
							}
							enc.endEncoding();
						}
						// 3: RCAS onto the drawable at the dest rect (+ the overlay).
						if let Some(enc) = cb.renderCommandEncoderWithDescriptor(&Self::pass(&target, Some([0.0, 0.0, 0.0, 1.0]))) {
							enc.setRenderPipelineState(self.rcas.as_ref().unwrap());
							enc.setViewport(viewport(v.dst.0 as f64, v.dst.1 as f64, dw as f64, dh as f64));
							// SAFETY: bindings match rcas_fs.
							unsafe {
								let (p, n) = bytes(&k);
								enc.setFragmentBytes_length_atIndex(p, n, 0);
								enc.setFragmentTexture_atIndex(Some(&mid), 0);
								enc.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 3);
							}
							if let Some((prims, ppp)) = overlay {
								self.paint_egui(&enc, prims, size, ppp);
							}
							enc.endEncoding();
							fsr_done = true;
						}
					}
					_ => {
						eprintln!("pulsar-render(mac): FSR textures unavailable — plain scaling");
						self.fsr_failed = true;
					}
				}
			}
		}
		if !fsr_done {
			let clear = if drew_video { [0.0, 0.0, 0.0, 1.0] } else { IDLE_CLEAR };
			let Some(enc) = cb.renderCommandEncoderWithDescriptor(&Self::pass(&target, Some(clear))) else {
				return false;
			};
			if let Some((v, y, c, yuv, uvs)) = pic.as_ref() {
				let quad = Quad { pos: ndc_quad(v.dst, tw, th), uv: uvs.map(|u| [u[0], u[1], 0.0, 0.0]) };
				self.draw_video(&enc, y, c, yuv, &quad, viewport(0.0, 0.0, tw as f64, th as f64));
			}
			if let Some((prims, ppp)) = overlay {
				self.paint_egui(&enc, prims, size, ppp);
			}
			enc.endEncoding();
		}
		cb.presentDrawable(ProtocolObject::from_ref(&*drawable));
		cb.commit();
		// SAFETY: flushing the cache only drops textures nobody references any more.
		unsafe { CVMetalTextureCacheFlush(self.cache.ptr() as CVMetalTextureCacheRef, 0) };
		self.inflight.push_back((cb, keep));
		true
	}
}
