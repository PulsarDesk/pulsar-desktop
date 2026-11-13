//! AMD FidelityFX Super Resolution 1 (EASU upscale + RCAS sharpen) for the Linux GL presenter
//! (`video.rs`) — the "picture upscaling" setting (Settings → Görüntü → Görüntü iyileştirme;
//! stdin `upscale off|auto|fsr|ai`; Linux has no vendor AI upscaler, so every "on" mode is FSR).
//!
//! The presenter's own draw (whichever decode path: dmabuf / CUDA / NV12 / YUV upload) is
//! redirected into `a` (an RGBA texture at the stream's size) instead of the screen; EASU scales
//! `a` into `b` at the letterboxed on-screen size; RCAS sharpens `b` onto the screen. Any GL
//! failure (shader, incomplete framebuffer) turns FSR off for the session: plain scaling, never
//! a black picture.
//!
//! Shader math: FSR 1.0.2 (Copyright (c) 2021 Advanced Micro Devices, Inc., MIT license) after
//! agyild's mpv port, in GLSL ES 1.00 like the presenter's shaders (no bit-cast reciprocal
//! approximations there — plain guarded divisions), RGB with luma-driven edge analysis.

use glow::HasContext;
use std::sync::atomic::{AtomicBool, Ordering};

static ON: AtomicBool = AtomicBool::new(false);

/// `upscale <mode>` from the app: anything but "off" enables FSR.
pub fn set_upscale(mode: &str) {
	ON.store(!matches!(mode.trim(), "" | "off"), Ordering::Relaxed);
}

pub fn enabled() -> bool {
	ON.load(Ordering::Relaxed)
}

const VERT: &str = "attribute vec2 pos;\nattribute vec2 uvin;\nvarying vec2 uv;\nvoid main(){ uv=uvin; gl_Position=vec4(pos,0.0,1.0); }\n";

const PRE: &str = "#ifdef GL_FRAGMENT_PRECISION_HIGH\nprecision highp float;\n#else\nprecision mediump float;\n#endif\nvarying vec2 uv;\nuniform sampler2D tex;\nuniform vec4 con0;\nuniform vec4 con1;\nfloat luma(vec3 c){ return c.g + 0.5*(c.r + c.b); }\nfloat rcpS(float a){ return 1.0 / max(a, 1e-5); }\n";

const EASU: &str = r#"
vec3 tapAt(vec2 p){ return texture2D(tex, (p + 0.5) * con0.zw).rgb; }
void easuTap(inout vec3 aC, inout float aW, vec2 off, vec2 dir, vec2 len, float lob, float clp, vec3 c){
	vec2 v;
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
void easuSet(inout vec2 dir, inout float len, float w, float lA, float lB, float lC, float lD, float lE){
	float dc = lD - lC;
	float cb = lC - lB;
	float lenX = rcpS(max(abs(dc), abs(cb)));
	float dirX = lD - lB;
	lenX = clamp(abs(dirX) * lenX, 0.0, 1.0);
	lenX *= lenX;
	float ec = lE - lC;
	float ca = lC - lA;
	float lenY = rcpS(max(abs(ec), abs(ca)));
	float dirY = lE - lA;
	lenY = clamp(abs(dirY) * lenY, 0.0, 1.0);
	lenY *= lenY;
	dir += vec2(dirX, dirY) * w;
	len += w * lenX + w * lenY;
}
void main(){
	vec2 pp = uv * con0.xy - 0.5;
	vec2 fp = floor(pp);
	pp -= fp;
	vec3 b = tapAt(fp + vec2(0.0, -1.0)); vec3 c = tapAt(fp + vec2(1.0, -1.0));
	vec3 e = tapAt(fp + vec2(-1.0, 0.0)); vec3 f = tapAt(fp); vec3 g = tapAt(fp + vec2(1.0, 0.0)); vec3 h = tapAt(fp + vec2(2.0, 0.0));
	vec3 i = tapAt(fp + vec2(-1.0, 1.0)); vec3 j = tapAt(fp + vec2(0.0, 1.0)); vec3 k = tapAt(fp + vec2(1.0, 1.0)); vec3 l = tapAt(fp + vec2(2.0, 1.0));
	vec3 n = tapAt(fp + vec2(0.0, 2.0)); vec3 o = tapAt(fp + vec2(1.0, 2.0));
	float bL = luma(b); float cL = luma(c); float eL = luma(e); float fL = luma(f); float gL = luma(g); float hL = luma(h);
	float iL = luma(i); float jL = luma(j); float kL = luma(k); float lL = luma(l); float nL = luma(n); float oL = luma(o);
	vec2 dir = vec2(0.0);
	float len = 0.0;
	easuSet(dir, len, (1.0 - pp.x) * (1.0 - pp.y), bL, eL, fL, gL, jL);
	easuSet(dir, len, pp.x * (1.0 - pp.y), cL, fL, gL, hL, kL);
	easuSet(dir, len, (1.0 - pp.x) * pp.y, fL, iL, jL, kL, nL);
	easuSet(dir, len, pp.x * pp.y, gL, jL, kL, lL, oL);
	float dirR = dir.x * dir.x + dir.y * dir.y;
	bool zro = dirR < (1.0 / 32768.0);
	dirR = zro ? 1.0 : inversesqrt(dirR);
	dir.x = zro ? 1.0 : dir.x;
	dir *= dirR;
	len = len * 0.5;
	len *= len;
	float stretch = (dir.x * dir.x + dir.y * dir.y) * rcpS(max(abs(dir.x), abs(dir.y)));
	vec2 len2 = vec2(1.0 + (stretch - 1.0) * len, 1.0 - 0.5 * len);
	float lob = 0.5 + ((1.0 / 4.0 - 0.04) - 0.5) * len;
	float clp = rcpS(lob);
	vec3 aC = vec3(0.0);
	float aW = 0.0;
	easuTap(aC, aW, vec2(0.0, -1.0) - pp, dir, len2, lob, clp, b);
	easuTap(aC, aW, vec2(1.0, -1.0) - pp, dir, len2, lob, clp, c);
	easuTap(aC, aW, vec2(-1.0, 1.0) - pp, dir, len2, lob, clp, i);
	easuTap(aC, aW, vec2(0.0, 1.0) - pp, dir, len2, lob, clp, j);
	easuTap(aC, aW, vec2(0.0, 0.0) - pp, dir, len2, lob, clp, f);
	easuTap(aC, aW, vec2(-1.0, 0.0) - pp, dir, len2, lob, clp, e);
	easuTap(aC, aW, vec2(1.0, 1.0) - pp, dir, len2, lob, clp, k);
	easuTap(aC, aW, vec2(2.0, 1.0) - pp, dir, len2, lob, clp, l);
	easuTap(aC, aW, vec2(2.0, 0.0) - pp, dir, len2, lob, clp, h);
	easuTap(aC, aW, vec2(1.0, 0.0) - pp, dir, len2, lob, clp, g);
	easuTap(aC, aW, vec2(1.0, 2.0) - pp, dir, len2, lob, clp, o);
	easuTap(aC, aW, vec2(0.0, 2.0) - pp, dir, len2, lob, clp, n);
	vec3 pix = aC / aW;
	vec3 mn = min(min(f, g), min(j, k));
	vec3 mx = max(max(f, g), max(j, k));
	gl_FragColor = vec4(clamp(clamp(pix, mn, mx), 0.0, 1.0), 1.0);
}
"#;

const RCAS: &str = r#"
vec3 ld(vec2 p){ return texture2D(tex, (p + 0.5) * con0.zw).rgb; }
void main(){
	vec2 p = floor(uv * con0.xy);
	vec3 b = ld(p + vec2(0.0, -1.0)); vec3 d = ld(p + vec2(-1.0, 0.0)); vec3 e = ld(p);
	vec3 f = ld(p + vec2(1.0, 0.0)); vec3 h = ld(p + vec2(0.0, 1.0));
	vec3 mn = min(min(b, d), min(f, h));
	vec3 mx = max(max(b, d), max(f, h));
	vec3 hitMin = min(mn, e) / max(4.0 * mx, vec3(1e-5));
	vec3 hitMax = (1.0 - max(mx, e)) / min(4.0 * mn - 4.0, vec3(-1e-5));
	vec3 lobeC = max(-hitMin, hitMax);
	float lobe = max(-(0.25 - 1.0 / 16.0), min(max(lobeC.r, max(lobeC.g, lobeC.b)), 0.0)) * exp2(-con1.z);
	float bL = luma(b); float dL = luma(d); float eL = luma(e); float fL = luma(f); float hL = luma(h);
	float nz = 0.25 * (bL + dL + fL + hL) - eL;
	float rng = max(max(max(bL, dL), max(eL, fL)), hL) - min(min(min(bL, dL), min(eL, fL)), hL);
	nz = clamp(abs(nz) * rcpS(rng), 0.0, 1.0);
	nz = -0.5 * nz + 1.0;
	lobe *= nz;
	gl_FragColor = vec4(clamp((lobe * (b + d + h + f) + e) / (4.0 * lobe + 1.0), 0.0, 1.0), 1.0);
}
"#;

/// Sharpening in stops (0 = strongest, 2 = mildest), as in the mpv port.
const SHARPNESS: f32 = 0.2;
/// Texture unit used while allocating the FSR targets (keeps the presenter's units 0..2 intact
/// until its own draw has run).
const ALLOC_UNIT: u32 = glow::TEXTURE7;

struct Target {
	fbo: glow::Framebuffer,
	tex: glow::Texture,
	w: i32,
	h: i32,
}

/// Output pixels per second FSR may cost on a mobile GPU (Mali / Adreno / PowerVR). Measured on
/// the Orange Pi 5 (Mali-G610, 2026-11-13): 640x360 → 1280x720 @60 took ~22 % of the GPU at
/// its top clock, so 1080p60 output (~124 Mpx/s) is ~55 % — fine — while 1080p120 (~249 Mpx/s)
/// would need more than the whole GPU. Desktop GPUs have no cap.
const MOBILE_BUDGET_PX_S: f32 = 160e6;

pub struct GlFsr {
	/// A mobile GPU: FSR only within `MOBILE_BUDGET_PX_S`.
	mobile: bool,
	/// Last budget verdict (logged on change).
	over_budget: bool,
	easu: glow::Program,
	rcas: glow::Program,
	vbo: glow::Buffer,
	a: Option<Target>,
	b: Option<Target>,
}

unsafe fn program(gl: &glow::Context, frag: &str) -> Option<glow::Program> {
	let prog = gl.create_program().ok()?;
	for (ty, src) in [(glow::VERTEX_SHADER, VERT.to_string()), (glow::FRAGMENT_SHADER, format!("{PRE}{frag}"))] {
		let s = gl.create_shader(ty).ok()?;
		gl.shader_source(s, &src);
		gl.compile_shader(s);
		if !gl.get_shader_compile_status(s) {
			eprintln!("pulsar-render: FSR shader: {}", gl.get_shader_info_log(s));
			return None;
		}
		gl.attach_shader(prog, s);
	}
	gl.bind_attrib_location(prog, 0, "pos");
	gl.bind_attrib_location(prog, 1, "uvin");
	gl.link_program(prog);
	if !gl.get_program_link_status(prog) {
		eprintln!("pulsar-render: FSR link: {}", gl.get_program_info_log(prog));
		return None;
	}
	gl.use_program(Some(prog));
	if let Some(loc) = gl.get_uniform_location(prog, "tex") {
		gl.uniform_1_i32(Some(&loc), 0);
	}
	gl.use_program(None);
	Some(prog)
}

impl GlFsr {
	pub unsafe fn new(gl: &glow::Context) -> Option<Self> {
		let easu = program(gl, EASU)?;
		let rcas = program(gl, RCAS)?;
		let vbo = gl.create_buffer().ok()?;
		let renderer = gl.get_parameter_string(glow::RENDERER);
		let mobile = ["Mali", "Adreno", "PowerVR", "Immortalis"].iter().any(|m| renderer.contains(m));
		eprintln!("pulsar-render: FSR upscaler ready (gpu {renderer:?}, mobile budget {mobile})");
		Some(Self { mobile, over_budget: false, easu, rcas, vbo, a: None, b: None })
	}

	/// Can the GPU afford FSR for `px` output pixels at `fps`? Always on a desktop GPU; within
	/// `MOBILE_BUDGET_PX_S` on a mobile one (the stream keeps its frame rate over sharpness).
	pub fn affordable(&mut self, px: i64, fps: f32) -> bool {
		let over = self.mobile && px as f32 * fps.max(30.0) > MOBILE_BUDGET_PX_S;
		if over != self.over_budget {
			self.over_budget = over;
			eprintln!(
				"pulsar-render: FSR {} ({px} px x {fps:.0} fps vs {:.0} Mpx/s budget)",
				if over { "paused: over the GPU budget" } else { "back on" },
				MOBILE_BUDGET_PX_S / 1e6
			);
		}
		!over
	}

	/// A render-target texture of `w`x`h` (reused while the size holds). None if the
	/// framebuffer can't be completed on this GL.
	unsafe fn target(gl: &glow::Context, slot: &mut Option<Target>, w: i32, h: i32) -> Option<()> {
		if slot.as_ref().is_some_and(|t| t.w == w && t.h == h) {
			return Some(());
		}
		if let Some(old) = slot.take() {
			gl.delete_framebuffer(old.fbo);
			gl.delete_texture(old.tex);
		}
		let tex = gl.create_texture().ok()?;
		gl.active_texture(ALLOC_UNIT);
		gl.bind_texture(glow::TEXTURE_2D, Some(tex));
		gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32);
		gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32);
		gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
		gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
		gl.tex_image_2d(
			glow::TEXTURE_2D,
			0,
			glow::RGBA as i32,
			w,
			h,
			0,
			glow::RGBA,
			glow::UNSIGNED_BYTE,
			glow::PixelUnpackData::Slice(None),
		);
		gl.bind_texture(glow::TEXTURE_2D, None);
		let fbo = gl.create_framebuffer().ok()?;
		gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
		gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(tex), 0);
		let ok = gl.check_framebuffer_status(glow::FRAMEBUFFER) == glow::FRAMEBUFFER_COMPLETE;
		if !ok {
			eprintln!("pulsar-render: FSR framebuffer {w}x{h} incomplete");
			gl.delete_framebuffer(fbo);
			gl.delete_texture(tex);
			return None;
		}
		*slot = Some(Target { fbo, tex, w, h });
		Some(())
	}

	/// Redirect the presenter's next draw into the stream-sized texture. Returns the framebuffer
	/// to restore (`Some(prev)`), or None (FSR can't run: draw normally).
	pub unsafe fn begin(&mut self, gl: &glow::Context, vw: i32, vh: i32) -> Option<Option<glow::Framebuffer>> {
		let prev = gl.get_parameter_framebuffer(glow::FRAMEBUFFER_BINDING);
		if Self::target(gl, &mut self.a, vw, vh).is_none() {
			gl.bind_framebuffer(glow::FRAMEBUFFER, prev);
			return None;
		}
		gl.bind_framebuffer(glow::FRAMEBUFFER, self.a.as_ref().map(|t| t.fbo));
		gl.viewport(0, 0, vw, vh);
		Some(prev)
	}

	unsafe fn pass(&self, gl: &glow::Context, prog: glow::Program, src: &Target, out_w: i32, out_h: i32) {
		gl.use_program(Some(prog));
		gl.active_texture(glow::TEXTURE0);
		gl.bind_texture(glow::TEXTURE_2D, Some(src.tex));
		let (sw, sh) = (src.w as f32, src.h as f32);
		if let Some(l) = gl.get_uniform_location(prog, "con0") {
			gl.uniform_4_f32(Some(&l), sw, sh, 1.0 / sw, 1.0 / sh);
		}
		if let Some(l) = gl.get_uniform_location(prog, "con1") {
			gl.uniform_4_f32(Some(&l), out_w as f32, out_h as f32, SHARPNESS, 0.0);
		}
		// Upright full-viewport quad (the targets are already in GL orientation).
		let quad: [f32; 24] = [
			-1.0, -1.0, 0.0, 0.0, 1.0, -1.0, 1.0, 0.0, 1.0, 1.0, 1.0, 1.0, -1.0, -1.0, 0.0, 0.0, 1.0, 1.0, 1.0,
			1.0, -1.0, 1.0, 0.0, 1.0,
		];
		let bytes = std::slice::from_raw_parts(quad.as_ptr() as *const u8, std::mem::size_of_val(&quad));
		gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
		gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STREAM_DRAW);
		gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 16, 0);
		gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, 16, 8);
		gl.enable_vertex_attrib_array(0);
		gl.enable_vertex_attrib_array(1);
		gl.draw_arrays(glow::TRIANGLES, 0, 6);
	}

	/// After the presenter drew into `a`: EASU `a` → `b` (rw x rh), RCAS `b` → the restored
	/// framebuffer at (vx, vy, rw, rh). False when `b` can't be made (the frame is lost once;
	/// the caller turns FSR off).
	pub unsafe fn finish(
		&mut self,
		gl: &glow::Context,
		prev: Option<glow::Framebuffer>,
		vx: i32,
		vy: i32,
		rw: i32,
		rh: i32,
	) -> bool {
		if Self::target(gl, &mut self.b, rw, rh).is_none() {
			gl.bind_framebuffer(glow::FRAMEBUFFER, prev);
			return false;
		}
		let (Some(a), Some(b)) = (self.a.as_ref(), self.b.as_ref()) else {
			gl.bind_framebuffer(glow::FRAMEBUFFER, prev);
			return false;
		};
		gl.bind_framebuffer(glow::FRAMEBUFFER, Some(b.fbo));
		gl.viewport(0, 0, rw, rh);
		self.pass(gl, self.easu, a, rw, rh);
		gl.bind_framebuffer(glow::FRAMEBUFFER, prev);
		gl.viewport(vx, vy, rw, rh);
		self.pass(gl, self.rcas, b, rw, rh);
		gl.bind_texture(glow::TEXTURE_2D, None);
		gl.use_program(None);
		true
	}
}
