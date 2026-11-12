//! AMD FidelityFX Super Resolution 1 (EASU upscale + RCAS sharpen) as two D3D11 pixel-shader
//! passes — the "FSR" picture-upscaling option (Settings → Görüntü → Görüntü iyileştirme).
//!
//! The VideoProcessor first converts (and crops / rotates) the decoded NV12 frame into `src`
//! at its own resolution; EASU scales `src` to the on-screen size into `mid`; RCAS sharpens
//! `mid` into the swapchain back buffer at the letterboxed destination rect.
//!
//! The shader math is FSR 1.0.2 (Copyright (c) 2021 Advanced Micro Devices, Inc., MIT license),
//! after agyild's single-file mpv port, extended from luma-only to RGB the way the original
//! EASU works: the edge direction and length come from luma, the 12 taps accumulate RGB.

use windows::core::{Interface, Result, PCSTR};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST};
use windows::Win32::Graphics::Direct3D11::{
	ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView,
	ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
	D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BUFFER_DESC,
	D3D11_COMPARISON_ALWAYS, D3D11_CPU_ACCESS_WRITE, D3D11_FILTER_MIN_MAG_MIP_POINT,
	D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_WRITE_DISCARD, D3D11_SAMPLER_DESC, D3D11_TEXTURE2D_DESC,
	D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DEFAULT, D3D11_USAGE_DYNAMIC, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

const HLSL: &str = r#"
cbuffer C : register(b0) {
	float4 con0; // input size (xy), 1/input size (zw)
	float4 con1; // output size (xy), sharpness stops (z)
};
Texture2D<float4> tex : register(t0);
SamplerState pt : register(s0);

struct VOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
VOut vs_main(uint id : SV_VertexID) {
	VOut o;
	float2 uv = float2((id << 1) & 2, id & 2);
	o.uv = uv;
	o.pos = float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
	return o;
}

float rcpLo(float a) { return asfloat(0x7ef07ebbu - asuint(a)); }
float rsqLo(float a) { return asfloat(0x5f347d74u - (asuint(a) >> 1)); }
float rcpMed(float a) { float b = asfloat(0x7ef19fffu - asuint(a)); return b * (-b * a + 2.0); }
float luma(float3 c) { return c.g + 0.5 * (c.r + c.b); }
float3 tapAt(float2 p) { return tex.SampleLevel(pt, (p + 0.5) * con0.zw, 0).rgb; }

void easuTap(inout float3 aC, inout float aW, float2 off, float2 dir, float2 len, float lob, float clp, float3 c) {
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

void easuSet(inout float2 dir, inout float len, float w, float lA, float lB, float lC, float lD, float lE) {
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

float4 ps_easu(VOut i) : SV_Target {
	float2 pp = i.uv * con0.xy - 0.5;
	float2 fp = floor(pp);
	pp -= fp;
	float3 b = tapAt(fp + float2(0, -1)), c = tapAt(fp + float2(1, -1));
	float3 e = tapAt(fp + float2(-1, 0)), f = tapAt(fp), g = tapAt(fp + float2(1, 0)), h = tapAt(fp + float2(2, 0));
	float3 ii = tapAt(fp + float2(-1, 1)), j = tapAt(fp + float2(0, 1)), k = tapAt(fp + float2(1, 1)), l = tapAt(fp + float2(2, 1));
	float3 n = tapAt(fp + float2(0, 2)), o = tapAt(fp + float2(1, 2));
	float bL = luma(b), cL = luma(c), eL = luma(e), fL = luma(f), gL = luma(g), hL = luma(h);
	float iL = luma(ii), jL = luma(j), kL = luma(k), lL = luma(l), nL = luma(n), oL = luma(o);
	float2 dir = 0.0;
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
	float3 aC = 0.0;
	float aW = 0.0;
	easuTap(aC, aW, float2(0, -1) - pp, dir, len2, lob, clp, b);
	easuTap(aC, aW, float2(1, -1) - pp, dir, len2, lob, clp, c);
	easuTap(aC, aW, float2(-1, 1) - pp, dir, len2, lob, clp, ii);
	easuTap(aC, aW, float2(0, 1) - pp, dir, len2, lob, clp, j);
	easuTap(aC, aW, float2(0, 0) - pp, dir, len2, lob, clp, f);
	easuTap(aC, aW, float2(-1, 0) - pp, dir, len2, lob, clp, e);
	easuTap(aC, aW, float2(1, 1) - pp, dir, len2, lob, clp, k);
	easuTap(aC, aW, float2(2, 1) - pp, dir, len2, lob, clp, l);
	easuTap(aC, aW, float2(2, 0) - pp, dir, len2, lob, clp, h);
	easuTap(aC, aW, float2(1, 0) - pp, dir, len2, lob, clp, g);
	easuTap(aC, aW, float2(1, 2) - pp, dir, len2, lob, clp, o);
	easuTap(aC, aW, float2(0, 2) - pp, dir, len2, lob, clp, n);
	float3 pix = aC / aW;
	// Dering: stay within the 2x2 neighbourhood.
	float3 mn = min(min(f, g), min(j, k));
	float3 mx = max(max(f, g), max(j, k));
	return float4(saturate(clamp(pix, mn, mx)), 1.0);
}

static const float RCAS_LIMIT = 0.25 - 1.0 / 16.0;
float3 ld(int2 p) { return tex.Load(int3(clamp(p, int2(0, 0), int2(con1.xy) - 1), 0)).rgb; }
float4 ps_rcas(VOut i) : SV_Target {
	int2 p = int2(i.uv * con1.xy);
	float3 b = ld(p + int2(0, -1)), d = ld(p + int2(-1, 0)), e = ld(p), f = ld(p + int2(1, 0)), h = ld(p + int2(0, 1));
	float3 mn = min(min(b, d), min(f, h));
	float3 mx = max(max(b, d), max(f, h));
	float3 hitMin = min(mn, e) / max(4.0 * mx, 1e-5);
	float3 hitMax = (1.0 - max(mx, e)) / min(4.0 * mn - 4.0, -1e-5);
	float3 lobeC = max(-hitMin, hitMax);
	float lobe = max(-RCAS_LIMIT, min(max(lobeC.r, max(lobeC.g, lobeC.b)), 0.0)) * exp2(-con1.z);
	// Denoise: less sharpening on noise.
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

/// Sharpening in stops (0 = strongest, 2 = mildest) — FSR's default range; 0.2 as in the port.
const SHARPNESS: f32 = 0.2;

#[repr(C)]
struct Consts {
	con0: [f32; 4],
	con1: [f32; 4],
}

struct Target {
	tex: ID3D11Texture2D,
	rtv: ID3D11RenderTargetView,
	srv: ID3D11ShaderResourceView,
	w: u32,
	h: u32,
}

pub struct Fsr {
	device: ID3D11Device,
	ctx: ID3D11DeviceContext,
	vs: ID3D11VertexShader,
	easu: ID3D11PixelShader,
	rcas: ID3D11PixelShader,
	samp: ID3D11SamplerState,
	cb: ID3D11Buffer,
	/// The converted source at its own resolution (the VideoProcessor writes it).
	src: Option<Target>,
	/// EASU's output at the on-screen size (RCAS reads it).
	mid: Option<Target>,
}

fn compile(entry: &str, target: &str) -> Result<ID3DBlob> {
	let entry_c = format!("{entry}\0");
	let target_c = format!("{target}\0");
	let mut blob: Option<ID3DBlob> = None;
	let mut errors: Option<ID3DBlob> = None;
	let hr = unsafe {
		D3DCompile(
			HLSL.as_ptr() as *const _,
			HLSL.len(),
			None,
			None,
			None,
			PCSTR(entry_c.as_ptr()),
			PCSTR(target_c.as_ptr()),
			D3DCOMPILE_OPTIMIZATION_LEVEL3,
			0,
			&mut blob,
			Some(&mut errors),
		)
	};
	if let Err(e) = hr {
		if let Some(err) = errors {
			let msg = unsafe {
				std::slice::from_raw_parts(err.GetBufferPointer() as *const u8, err.GetBufferSize())
			};
			eprintln!("pulsar-render(win): FSR shader {entry}: {}", String::from_utf8_lossy(msg));
		}
		return Err(e);
	}
	blob.ok_or_else(windows::core::Error::from_thread)
}

fn bytes(b: &ID3DBlob) -> &[u8] {
	unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()) }
}

impl Fsr {
	pub fn new(device: &ID3D11Device) -> Result<Self> {
		let vs_b = compile("vs_main", "vs_5_0")?;
		let easu_b = compile("ps_easu", "ps_5_0")?;
		let rcas_b = compile("ps_rcas", "ps_5_0")?;
		let (mut vs, mut easu, mut rcas, mut samp, mut cb) = (None, None, None, None, None);
		unsafe {
			device.CreateVertexShader(bytes(&vs_b), None, Some(&mut vs))?;
			device.CreatePixelShader(bytes(&easu_b), None, Some(&mut easu))?;
			device.CreatePixelShader(bytes(&rcas_b), None, Some(&mut rcas))?;
			let sd = D3D11_SAMPLER_DESC {
				Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
				AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
				AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
				AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
				MipLODBias: 0.0,
				MaxAnisotropy: 1,
				ComparisonFunc: D3D11_COMPARISON_ALWAYS,
				BorderColor: [0.0; 4],
				MinLOD: 0.0,
				MaxLOD: f32::MAX,
			};
			device.CreateSamplerState(&sd, Some(&mut samp))?;
			let bd = D3D11_BUFFER_DESC {
				ByteWidth: std::mem::size_of::<Consts>() as u32,
				Usage: D3D11_USAGE_DYNAMIC,
				BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
				CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
				MiscFlags: 0,
				StructureByteStride: 0,
			};
			device.CreateBuffer(&bd, None, Some(&mut cb))?;
		}
		Ok(Self {
			device: device.clone(),
			ctx: unsafe { device.GetImmediateContext()? },
			vs: vs.unwrap(),
			easu: easu.unwrap(),
			rcas: rcas.unwrap(),
			samp: samp.unwrap(),
			cb: cb.unwrap(),
			src: None,
			mid: None,
		})
	}

	fn target(&self, w: u32, h: u32) -> Result<Target> {
		let desc = D3D11_TEXTURE2D_DESC {
			Width: w.max(1),
			Height: h.max(1),
			MipLevels: 1,
			ArraySize: 1,
			Format: DXGI_FORMAT_B8G8R8A8_UNORM,
			SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
			Usage: D3D11_USAGE_DEFAULT,
			BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
			CPUAccessFlags: 0,
			MiscFlags: 0,
		};
		let (mut tex, mut rtv, mut srv) = (None, None, None);
		unsafe {
			self.device.CreateTexture2D(&desc, None, Some(&mut tex))?;
			let tex: ID3D11Texture2D = tex.unwrap();
			self.device.CreateRenderTargetView(&tex, None, Some(&mut rtv))?;
			self.device.CreateShaderResourceView(&tex, None, Some(&mut srv))?;
			Ok(Target { tex, rtv: rtv.unwrap(), srv: srv.unwrap(), w: w.max(1), h: h.max(1) })
		}
	}

	/// The source-sized texture the VideoProcessor converts into (rebuilt on a size change).
	pub fn source(&mut self, w: u32, h: u32) -> Result<ID3D11Texture2D> {
		if self.src.as_ref().is_none_or(|t| t.w != w || t.h != h) {
			self.src = Some(self.target(w, h)?);
		}
		Ok(self.src.as_ref().unwrap().tex.clone())
	}

	unsafe fn pass(
		&self,
		ps: &ID3D11PixelShader,
		input: &ID3D11ShaderResourceView,
		rtv: &ID3D11RenderTargetView,
		vp: (f32, f32, f32, f32),
		consts: Consts,
	) -> Result<()> {
		let ctx = &self.ctx;
		let mut m = D3D11_MAPPED_SUBRESOURCE::default();
		ctx.Map(&self.cb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut m))?;
		std::ptr::copy_nonoverlapping(&consts as *const Consts as *const u8, m.pData as *mut u8, std::mem::size_of::<Consts>());
		ctx.Unmap(&self.cb, 0);
		ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
		ctx.OMSetBlendState(None, None, 0xffff_ffff);
		ctx.RSSetState(None);
		ctx.RSSetViewports(Some(&[D3D11_VIEWPORT {
			TopLeftX: vp.0,
			TopLeftY: vp.1,
			Width: vp.2,
			Height: vp.3,
			MinDepth: 0.0,
			MaxDepth: 1.0,
		}]));
		ctx.IASetInputLayout(None);
		ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
		ctx.VSSetShader(&self.vs, None);
		ctx.PSSetShader(ps, None);
		ctx.PSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
		ctx.PSSetSamplers(0, Some(&[Some(self.samp.clone())]));
		ctx.PSSetShaderResources(0, Some(&[Some(input.clone())]));
		ctx.Draw(3, 0);
		// Unbind the input: next frame it is a render target again.
		ctx.PSSetShaderResources(0, Some(&[None]));
		Ok(())
	}

	/// EASU `src` → `mid` (dest size), then RCAS `mid` → `back` at `dst` (bars cleared black).
	pub unsafe fn run(&mut self, back: &ID3D11Texture2D, dst: RECT) -> Result<()> {
		let dw = (dst.right - dst.left).max(1) as u32;
		let dh = (dst.bottom - dst.top).max(1) as u32;
		if self.mid.as_ref().is_none_or(|t| t.w != dw || t.h != dh) {
			self.mid = Some(self.target(dw, dh)?);
		}
		let (Some(src), Some(mid)) = (self.src.as_ref(), self.mid.as_ref()) else {
			return Ok(());
		};
		let (sw, sh) = (src.w as f32, src.h as f32);
		self.pass(
			&self.easu,
			&src.srv,
			&mid.rtv,
			(0.0, 0.0, dw as f32, dh as f32),
			Consts { con0: [sw, sh, 1.0 / sw, 1.0 / sh], con1: [dw as f32, dh as f32, SHARPNESS, 0.0] },
		)?;
		let mut back_rtv = None;
		self.device.CreateRenderTargetView(back, None, Some(&mut back_rtv))?;
		let back_rtv: ID3D11RenderTargetView = back_rtv.unwrap();
		self.ctx.ClearRenderTargetView(&back_rtv, &[0.0, 0.0, 0.0, 1.0]);
		self.pass(
			&self.rcas,
			&mid.srv,
			&back_rtv,
			(dst.left as f32, dst.top as f32, dw as f32, dh as f32),
			Consts { con0: [dw as f32, dh as f32, 1.0 / dw as f32, 1.0 / dh as f32], con1: [dw as f32, dh as f32, SHARPNESS, 0.0] },
		)
	}
}

/// GPU vendor of a D3D11 device (PCI vendor id): picks the "AI" upscaler (NVIDIA RTX VSR /
/// Intel VSR) or falls back to FSR.
pub fn vendor(device: &ID3D11Device) -> u32 {
	use windows::Win32::Graphics::Dxgi::IDXGIDevice;
	unsafe {
		device
			.cast::<IDXGIDevice>()
			.and_then(|d| d.GetAdapter())
			.and_then(|a| a.GetDesc())
			.map(|d| d.VendorId)
			.unwrap_or(0)
	}
}

#[cfg(test)]
mod tests {
	#[test]
	fn the_fsr_shaders_compile() {
		for (entry, target) in [("vs_main", "vs_5_0"), ("ps_easu", "ps_5_0"), ("ps_rcas", "ps_5_0")] {
			assert!(super::compile(entry, target).is_ok(), "{entry} failed to compile");
		}
	}
}
