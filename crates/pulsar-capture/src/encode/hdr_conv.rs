//! FP16 scRGB → P010 (BT.2020, PQ, limited range) on the GPU — the HDR replacement for the
//! SDR path's BGRA → NV12 `VideoProcessorBlt`.
//!
//! Why shaders and not the VideoProcessor: the VP's colour-space handling for an FP16 scRGB
//! input → HDR10 output (`VideoProcessorSetStreamColorSpace1` / `…OutputColorSpace1`) is
//! optional per driver and not reported reliably; a driver that ignores it would encode SDR
//! maths as PQ — exactly the fake-HDR bug this path exists to avoid. Two pixel-shader passes
//! do the same maths on every GPU, and match the CPU reference `crate::hdr::scrgb_to_ycbcr10`
//! (tested below):
//!
//! 1. luma: full-size render into the P010 luma plane (R16_UNORM view),
//! 2. chroma: half-size render into the CbCr plane (R16G16_UNORM view); the bilinear sample at
//!    a chroma texel's centre averages the 2×2 block (in linear light).
//!
//! Both passes scale native → encode size and apply the host rotation (the encode surface is
//! already W↔H swapped for 90/270, like the VideoProcessor path). Downscaling is a box filter
//! over each output pixel's footprint (bilinear taps on ~2×2-texel cells, linear light), so a
//! 4K HDR monitor at a 1080p encode keeps thin text as clean as the SDR path's filtered Blt.
//! Per pixel: scRGB (linear BT.709, 1.0 = 80 nits) → BT.2020 primaries → nits / 10 000 → PQ
//! → Y'CbCr (BT.2020 NCL) → 10-bit codes 64–940 / 64–960 → `code << 6` (P010 is MSB-aligned).

use windows::core::{Interface, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST};
use windows::Win32::Graphics::Direct3D11::{
	ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView,
	ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
	D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_RENDER_TARGET, D3D11_BUFFER_DESC, D3D11_COMPARISON_NEVER, D3D11_FILTER_MIN_MAG_MIP_LINEAR,
	D3D11_FORMAT_SUPPORT_RENDER_TARGET, D3D11_RENDER_TARGET_VIEW_DESC, D3D11_RENDER_TARGET_VIEW_DESC_0,
	D3D11_RTV_DIMENSION_TEXTURE2D, D3D11_SAMPLER_DESC, D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_RTV,
	D3D11_TEXTURE2D_DESC, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DEFAULT, D3D11_USAGE_IMMUTABLE,
	D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::{
	DXGI_FORMAT, DXGI_FORMAT_P010, DXGI_FORMAT_R16G16_UNORM, DXGI_FORMAT_R16_UNORM, DXGI_SAMPLE_DESC,
};

const HLSL: &str = r#"
cbuffer C : register(b0) { float4 rot; };
Texture2D<float4> src : register(t0);
SamplerState lin : register(s0);
struct VOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
VOut vs_main(uint id : SV_VertexID) {
	VOut o;
	float2 uv = float2((id << 1) & 2, id & 2);
	o.pos = float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
	float2 c = uv - 0.5;
	o.uv = float2(dot(rot.xy, c), dot(rot.zw, c)) + 0.5;
	return o;
}
static const float3x3 BT709_TO_2020 = {
	0.6274039, 0.3292830, 0.0433131,
	0.0690973, 0.9195404, 0.0113623,
	0.0163914, 0.0880133, 0.8955953 };
float3 pq(float3 scrgb) {
	float3 l = saturate(mul(BT709_TO_2020, max(scrgb, 0.0)) * (80.0 / 10000.0));
	float3 p = pow(l, 0.1593017578125);
	return pow((0.8359375 + 18.8515625 * p) / (1.0 + 18.6875 * p), 78.84375);
}
float luma(float3 e) { return dot(e, float3(0.2627, 0.6780, 0.0593)); }
float p010(float code) { return round(clamp(code, 0.0, 1023.0)) * (64.0 / 65535.0); }
// The source (linear scRGB) averaged over this output pixel's footprint. A downscale (4K
// monitor -> 1080p encode, a lower ABR rung, and the chroma pass always) covers more than the
// 2x2 texels one bilinear tap reads; thin text / 1 px UI lines would alias and shimmer. So
// the footprint (the uv step per output pixel along each screen axis — exact for this
// full-screen triangle, rotation included) is split into n x m cells of about 2x2 texels and
// each cell is one bilinear tap at its centre: a box filter in linear light. 1:1 and up to
// 2:1 stay one tap (bilinear), the work grows only with the ratio (at most 4 x 4 taps).
float3 footprint(float2 uv) {
	float2 sz;
	src.GetDimensions(sz.x, sz.y);
	float2 dx = ddx(uv), dy = ddy(uv);
	int nx = (int)clamp(ceil(length(dx * sz) * 0.5 - 1e-3), 1.0, 4.0);
	int ny = (int)clamp(ceil(length(dy * sz) * 0.5 - 1e-3), 1.0, 4.0);
	float3 acc = 0.0;
	[loop] for (int j = 0; j < ny; j++) {
		[loop] for (int i = 0; i < nx; i++) {
			float2 o = dx * ((i + 0.5) / nx - 0.5) + dy * ((j + 0.5) / ny - 0.5);
			acc += src.SampleLevel(lin, uv + o, 0).rgb;
		}
	}
	return acc / (nx * ny);
}
float ps_y(VOut i) : SV_Target {
	float3 e = pq(footprint(i.uv));
	return p010(64.0 + 876.0 * luma(e));
}
float2 ps_uv(VOut i) : SV_Target {
	float3 e = pq(footprint(i.uv));
	float y = luma(e);
	return float2(p010(512.0 + 896.0 * (e.b - y) / 1.8814), p010(512.0 + 896.0 * (e.r - y) / 1.4746));
}
"#;

fn compile(entry: &str, target: &str) -> Result<ID3DBlob, String> {
	let entry_c = format!("{entry}\0");
	let target_c = format!("{target}\0");
	let mut code: Option<ID3DBlob> = None;
	let mut errs: Option<ID3DBlob> = None;
	let r = unsafe {
		D3DCompile(
			HLSL.as_ptr() as *const _,
			HLSL.len(),
			PCSTR(b"pulsar-hdr-conv\0".as_ptr()),
			None,
			None,
			PCSTR(entry_c.as_ptr()),
			PCSTR(target_c.as_ptr()),
			D3DCOMPILE_OPTIMIZATION_LEVEL3,
			0,
			&mut code,
			Some(&mut errs),
		)
	};
	if let Err(e) = r {
		let msg = errs
			.map(|b| unsafe {
				String::from_utf8_lossy(std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()))
					.into_owned()
			})
			.unwrap_or_default();
		return Err(format!("hdr shader {entry}: {e} {msg}"));
	}
	code.ok_or_else(|| format!("hdr shader {entry}: no code"))
}

fn bytes(b: &ID3DBlob) -> &[u8] {
	unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()) }
}

/// The uv rotation matrix (row-major 2×2, applied to `uv − 0.5`) that samples the unrotated
/// source for an output rotated `deg` clockwise (the VideoProcessor ROTATION_n convention).
pub(crate) fn rotation_matrix(deg: u32) -> [f32; 4] {
	match deg {
		90 => [0.0, 1.0, -1.0, 0.0],   // out (u,v) ← src (v, 1−u)
		180 => [-1.0, 0.0, 0.0, -1.0], // src (1−u, 1−v)
		270 => [0.0, -1.0, 1.0, 0.0],  // src (1−v, u)
		_ => [1.0, 0.0, 0.0, 1.0],
	}
}

/// Can this device render into P010 planes (the converter's precondition)?
pub(crate) fn p010_render_supported(device: &ID3D11Device) -> bool {
	unsafe { device.CheckFormatSupport(DXGI_FORMAT_P010) }
		.map(|s| s & D3D11_FORMAT_SUPPORT_RENDER_TARGET.0 as u32 != 0)
		.unwrap_or(false)
}

/// The P010 encode surface (DEFAULT, render target — NVENC registers it as
/// `NV_ENC_BUFFER_FORMAT_YUV420_10BIT`).
pub(crate) fn create_p010_texture(device: &ID3D11Device, w: u32, h: u32) -> Result<ID3D11Texture2D, String> {
	let desc = D3D11_TEXTURE2D_DESC {
		Width: w,
		Height: h,
		MipLevels: 1,
		ArraySize: 1,
		Format: DXGI_FORMAT_P010,
		SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
		Usage: D3D11_USAGE_DEFAULT,
		BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
		CPUAccessFlags: 0,
		MiscFlags: 0,
	};
	let mut tex = None;
	unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }.map_err(|e| format!("P010 texture: {e}"))?;
	tex.ok_or_else(|| "P010 texture: null".into())
}

fn plane_rtv(device: &ID3D11Device, tex: &ID3D11Texture2D, fmt: DXGI_FORMAT) -> Result<ID3D11RenderTargetView, String> {
	let desc = D3D11_RENDER_TARGET_VIEW_DESC {
		Format: fmt,
		ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
		Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 } },
	};
	let mut rtv = None;
	unsafe { device.CreateRenderTargetView(tex, Some(&desc), Some(&mut rtv)) }
		.map_err(|e| format!("P010 plane view {fmt:?}: {e}"))?;
	rtv.ok_or_else(|| "P010 plane view: null".into())
}

/// Converts one FP16 scRGB texture per frame into a fixed P010 surface.
pub(crate) struct HdrConverter {
	device: ID3D11Device,
	vs: ID3D11VertexShader,
	ps_y: ID3D11PixelShader,
	ps_uv: ID3D11PixelShader,
	samp: ID3D11SamplerState,
	cb: ID3D11Buffer,
	rtv_y: ID3D11RenderTargetView,
	rtv_uv: ID3D11RenderTargetView,
	w: u32,
	h: u32,
	/// Cached SRV over the current source texture (keyed by its COM pointer, like the
	/// VideoProcessor input-view cache): the DXGI path alternates pool / present.
	srv: Option<(*mut core::ffi::c_void, ID3D11ShaderResourceView)>,
}

impl HdrConverter {
	/// Build the passes for the `w`×`h` P010 `target` (the encode size, after rotation).
	pub(crate) fn new(device: &ID3D11Device, target: &ID3D11Texture2D, w: u32, h: u32, rotation: u32) -> Result<Self, String> {
		unsafe {
			let vs_b = compile("vs_main", "vs_5_0")?;
			let y_b = compile("ps_y", "ps_5_0")?;
			let uv_b = compile("ps_uv", "ps_5_0")?;
			let (mut vs, mut ps_y, mut ps_uv) = (None, None, None);
			device.CreateVertexShader(bytes(&vs_b), None, Some(&mut vs)).map_err(|e| format!("hdr vs: {e}"))?;
			device.CreatePixelShader(bytes(&y_b), None, Some(&mut ps_y)).map_err(|e| format!("hdr ps_y: {e}"))?;
			device.CreatePixelShader(bytes(&uv_b), None, Some(&mut ps_uv)).map_err(|e| format!("hdr ps_uv: {e}"))?;
			let sd = D3D11_SAMPLER_DESC {
				Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
				AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
				AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
				AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
				ComparisonFunc: D3D11_COMPARISON_NEVER,
				MaxLOD: f32::MAX,
				..Default::default()
			};
			let mut samp = None;
			device.CreateSamplerState(&sd, Some(&mut samp)).map_err(|e| format!("hdr sampler: {e}"))?;
			let rot = rotation_matrix(rotation);
			let bd = D3D11_BUFFER_DESC {
				ByteWidth: 16,
				Usage: D3D11_USAGE_IMMUTABLE,
				BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
				..Default::default()
			};
			let init = D3D11_SUBRESOURCE_DATA { pSysMem: rot.as_ptr() as *const _, ..Default::default() };
			let mut cb = None;
			device.CreateBuffer(&bd, Some(&init), Some(&mut cb)).map_err(|e| format!("hdr cbuffer: {e}"))?;
			Ok(Self {
				device: device.clone(),
				vs: vs.unwrap(),
				ps_y: ps_y.unwrap(),
				ps_uv: ps_uv.unwrap(),
				samp: samp.unwrap(),
				cb: cb.unwrap(),
				rtv_y: plane_rtv(device, target, DXGI_FORMAT_R16_UNORM)?,
				rtv_uv: plane_rtv(device, target, DXGI_FORMAT_R16G16_UNORM)?,
				w,
				h,
				srv: None,
			})
		}
	}

	/// Convert `src` (FP16 scRGB, any size) into the P010 target on `ctx`.
	pub(crate) unsafe fn run(&mut self, ctx: &ID3D11DeviceContext, src: &ID3D11Texture2D) -> Result<(), String> {
		let key = src.as_raw();
		if self.srv.as_ref().map(|(k, _)| *k) != Some(key) {
			let mut srv = None;
			self.device
				.CreateShaderResourceView(src, None, Some(&mut srv))
				.map_err(|e| format!("hdr source view: {e}"))?;
			self.srv = Some((key, srv.ok_or("hdr source view: null")?));
		}
		let srv = self.srv.as_ref().unwrap().1.clone();
		ctx.OMSetBlendState(None, None, 0xffff_ffff);
		ctx.RSSetState(None);
		ctx.IASetInputLayout(None);
		ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
		ctx.VSSetShader(&self.vs, None);
		ctx.VSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
		ctx.PSSetSamplers(0, Some(&[Some(self.samp.clone())]));
		ctx.PSSetShaderResources(0, Some(&[Some(srv)]));
		for (rtv, ps, w, h) in [
			(&self.rtv_y, &self.ps_y, self.w, self.h),
			(&self.rtv_uv, &self.ps_uv, self.w / 2, self.h / 2),
		] {
			ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
			ctx.RSSetViewports(Some(&[D3D11_VIEWPORT {
				TopLeftX: 0.0,
				TopLeftY: 0.0,
				Width: w as f32,
				Height: h as f32,
				MinDepth: 0.0,
				MaxDepth: 1.0,
			}]));
			ctx.PSSetShader(ps, None);
			ctx.Draw(3, 0);
		}
		// Unbind: the P010 surface goes to NVENC next, the source back to the capture.
		ctx.OMSetRenderTargets(None, None);
		ctx.PSSetShaderResources(0, Some(&[None]));
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
	use windows::Win32::Graphics::Direct3D11::{
		D3D11CreateDevice, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
		D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_USAGE_STAGING,
	};
	use windows::Win32::Graphics::Direct3D11::D3D11_BIND_SHADER_RESOURCE;
	use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R16G16B16A16_FLOAT;

	fn device() -> Option<(ID3D11Device, ID3D11DeviceContext)> {
		let (mut d, mut c) = (None, None);
		unsafe {
			D3D11CreateDevice(
				None,
				D3D_DRIVER_TYPE_HARDWARE,
				Default::default(),
				D3D11_CREATE_DEVICE_BGRA_SUPPORT,
				Some(&[D3D_FEATURE_LEVEL_11_0]),
				D3D11_SDK_VERSION,
				Some(&mut d),
				None,
				Some(&mut c),
			)
			.ok()?;
		}
		Some((d?, c?))
	}

	fn f16(v: f32) -> u16 {
		// f32 → IEEE half (normal range only; enough for test colours).
		if v == 0.0 {
			return 0;
		}
		let b = v.to_bits();
		let sign = ((b >> 16) & 0x8000) as u16;
		let exp = ((b >> 23) & 0xFF) as i32 - 127 + 15;
		let man = ((b >> 13) & 0x3FF) as u16;
		sign | ((exp as u16) << 10) | man
	}

	/// `w`×`h` FP16 texture; `colour(x, y)` per pixel.
	fn fp16_tex(d: &ID3D11Device, w: u32, h: u32, colour: impl Fn(u32, u32) -> [f32; 3]) -> ID3D11Texture2D {
		let mut px = Vec::with_capacity((w * h * 4) as usize);
		for y in 0..h {
			for x in 0..w {
				let c = colour(x, y);
				px.extend_from_slice(&[f16(c[0]), f16(c[1]), f16(c[2]), f16(1.0)]);
			}
		}
		let desc = D3D11_TEXTURE2D_DESC {
			Width: w,
			Height: h,
			MipLevels: 1,
			ArraySize: 1,
			Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
			SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
			Usage: D3D11_USAGE_DEFAULT,
			BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
			CPUAccessFlags: 0,
			MiscFlags: 0,
		};
		let init = D3D11_SUBRESOURCE_DATA { pSysMem: px.as_ptr() as *const _, SysMemPitch: w * 8, SysMemSlicePitch: 0 };
		let mut t = None;
		unsafe { d.CreateTexture2D(&desc, Some(&init), Some(&mut t)) }.unwrap();
		t.unwrap()
	}

	/// Read a P010 texture back: (luma codes, chroma (cb, cr) codes), 10-bit.
	fn read_p010(d: &ID3D11Device, c: &ID3D11DeviceContext, t: &ID3D11Texture2D, w: u32, h: u32) -> (Vec<u16>, Vec<(u16, u16)>) {
		let desc = D3D11_TEXTURE2D_DESC {
			Width: w,
			Height: h,
			MipLevels: 1,
			ArraySize: 1,
			Format: DXGI_FORMAT_P010,
			SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
			Usage: D3D11_USAGE_STAGING,
			BindFlags: 0,
			CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
			MiscFlags: 0,
		};
		let mut s = None;
		unsafe {
			d.CreateTexture2D(&desc, None, Some(&mut s)).unwrap();
			let s: ID3D11Texture2D = s.unwrap();
			c.CopyResource(&s, t);
			let mut m = D3D11_MAPPED_SUBRESOURCE::default();
			c.Map(&s, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
			let base = m.pData as *const u8;
			let pitch = m.RowPitch as usize;
			let rd = |off: usize| u16::from_le_bytes([*base.add(off), *base.add(off + 1)]) >> 6;
			let mut y = vec![];
			for row in 0..h as usize {
				for col in 0..w as usize {
					y.push(rd(row * pitch + col * 2));
				}
			}
			let mut uv = vec![];
			let cbase = pitch * h as usize;
			for row in 0..(h / 2) as usize {
				for col in 0..(w / 2) as usize {
					let o = cbase + row * pitch + col * 4;
					uv.push((rd(o), rd(o + 2)));
				}
			}
			c.Unmap(&s, 0);
			(y, uv)
		}
	}

	fn setup() -> Option<(ID3D11Device, ID3D11DeviceContext)> {
		let (d, c) = device()?;
		if !p010_render_supported(&d) {
			eprintln!("skip: this GPU cannot render into P010");
			return None;
		}
		Some((d, c))
	}

	#[test]
	fn flat_colours_match_the_cpu_reference() {
		let Some((d, c)) = setup() else { return };
		for rgb in [[0.0f32, 0.0, 0.0], [203.0 / 80.0; 3], [1.0, 0.0, 0.0], [0.0, 0.5, 0.0], [0.2, 0.3, 4.0], [12.5, 12.5, 12.5]] {
			let src = fp16_tex(&d, 32, 16, |_, _| rgb);
			let dst = create_p010_texture(&d, 32, 16).unwrap();
			let mut conv = HdrConverter::new(&d, &dst, 32, 16, 0).unwrap();
			unsafe { conv.run(&c, &src).unwrap() };
			let (y, uv) = read_p010(&d, &c, &dst, 32, 16);
			let (ry, rcb, rcr) = crate::hdr::scrgb_to_ycbcr10(rgb);
			let near = |a: u16, b: u16| (a as i32 - b as i32).abs() <= 2;
			assert!(y.iter().all(|&v| near(v, ry)), "{rgb:?}: Y {} vs {ry}", y[0]);
			assert!(uv.iter().all(|&(cb, cr)| near(cb, rcb) && near(cr, rcr)), "{rgb:?}: {:?} vs {rcb},{rcr}", uv[0]);
		}
	}

	#[test]
	fn downscale_and_rotation_map_the_picture() {
		let Some((d, c)) = setup() else { return };
		// 64×32 source: left half black, right half white (SDR white at 203 nits).
		let w = 203.0 / 80.0;
		let src = fp16_tex(&d, 64, 32, |x, _| if x < 32 { [0.0; 3] } else { [w; 3] });
		let (black, _, _) = crate::hdr::scrgb_to_ycbcr10([0.0; 3]);
		let (white, _, _) = crate::hdr::scrgb_to_ycbcr10([w; 3]);
		// Plain 2:1 downscale into 32×16: left column black, right column white.
		let dst = create_p010_texture(&d, 32, 16).unwrap();
		let mut conv = HdrConverter::new(&d, &dst, 32, 16, 0).unwrap();
		unsafe { conv.run(&c, &src).unwrap() };
		let (y, _) = read_p010(&d, &c, &dst, 32, 16);
		assert!((y[0] as i32 - black as i32).abs() <= 2 && (y[31] as i32 - white as i32).abs() <= 2, "{} {}", y[0], y[31]);
		// 180°: mirrored — left column white.
		let mut conv = HdrConverter::new(&d, &dst, 32, 16, 180).unwrap();
		unsafe { conv.run(&c, &src).unwrap() };
		let (y, _) = read_p010(&d, &c, &dst, 32, 16);
		assert!((y[0] as i32 - white as i32).abs() <= 2 && (y[31] as i32 - black as i32).abs() <= 2, "{} {}", y[0], y[31]);
		// 90° CW into a portrait 16×32 surface: the source's right half (white) ends up at the
		// bottom; top row black.
		let dst = create_p010_texture(&d, 16, 32).unwrap();
		let mut conv = HdrConverter::new(&d, &dst, 16, 32, 90).unwrap();
		unsafe { conv.run(&c, &src).unwrap() };
		let (y, _) = read_p010(&d, &c, &dst, 16, 32);
		assert!((y[0] as i32 - black as i32).abs() <= 2, "top {}", y[0]);
		assert!((y[16 * 31] as i32 - white as i32).abs() <= 2, "bottom {}", y[16 * 31]);
	}

	/// A 4:1 downscale of 1 px lines (one white column in four): a single bilinear tap would land
	/// between two black columns and drop the line; the footprint filter keeps its share (a
	/// quarter of the light) in every output column, luma and chroma alike.
	#[test]
	fn downscale_filters_thin_lines_over_the_footprint() {
		let Some((d, c)) = setup() else { return };
		let w = 203.0 / 80.0;
		let src = fp16_tex(&d, 64, 32, |x, _| if x % 4 == 0 { [w; 3] } else { [0.0; 3] });
		let dst = create_p010_texture(&d, 16, 8).unwrap();
		let mut conv = HdrConverter::new(&d, &dst, 16, 8, 0).unwrap();
		unsafe { conv.run(&c, &src).unwrap() };
		let (y, uv) = read_p010(&d, &c, &dst, 16, 8);
		let (want, wcb, wcr) = crate::hdr::scrgb_to_ycbcr10([w * 0.25; 3]);
		assert!(y.iter().all(|&v| (v as i32 - want as i32).abs() <= 3), "luma {:?} vs {want}", &y[..16]);
		assert!(uv.iter().all(|&(cb, cr)| (cb as i32 - wcb as i32).abs() <= 3 && (cr as i32 - wcr as i32).abs() <= 3), "{:?}", &uv[..8]);
		// The same lines rotated 90°: the filter follows the rotated footprint.
		let dst = create_p010_texture(&d, 8, 16).unwrap();
		let mut conv = HdrConverter::new(&d, &dst, 8, 16, 90).unwrap();
		unsafe { conv.run(&c, &src).unwrap() };
		let (y, _) = read_p010(&d, &c, &dst, 8, 16);
		assert!(y.iter().all(|&v| (v as i32 - want as i32).abs() <= 3), "rotated luma {:?} vs {want}", &y[..8]);
	}

	/// The whole HDR encoder on an NVIDIA GPU (run by hand on a host with one:
	/// `cargo test -p pulsar-capture --lib hdr_nvenc -- --ignored --nocapture`): FP16 frames in,
	/// HEVC Main10 out with the HDR SEI on the keyframe. Writes the Annex B stream to
	/// `%TEMP%\pulsar-hdr-test.h265` for a look with ffprobe (colour tags, pix_fmt).
	#[test]
	#[ignore]
	fn hdr_nvenc_encodes_main10_with_metadata() {
		let (d, c) = unsafe { super::super::d3d::create_nvidia_device() }.expect("NVIDIA device");
		let out = std::env::temp_dir().join("pulsar-hdr-test.h265");
		let _ = std::fs::remove_file(&out);
		std::env::set_var("PULSAR_DUMP_BITSTREAM", &out);
		let (w, h) = (1280u32, 720u32);
		let p = crate::encode::EncParams {
			width: w,
			height: h,
			capture_width: w,
			capture_height: h,
			fps: 60,
			bitrate_kbps: 20_000,
			dest: "rtp://127.0.0.1:9".into(),
			codec: crate::Codec::H265,
			low_latency: true,
			intra_refresh: false,
			rotation: 0,
			rfi: false,
			hdr: Some(crate::hdr::HdrMeta::default()),
		};
		let mut enc = unsafe { crate::encode::Encoder::new(&d, &c, &p) }.expect("HDR encoder");
		for i in 0..30u32 {
			// A moving gradient up to 1000 nits (12.5 in scRGB).
			let tex = fp16_tex(&d, w, h, |x, y| {
				let v = ((x + i * 8) % w) as f32 / w as f32 * 12.5;
				[v, v * 0.6, (y as f32 / h as f32) * 4.0]
			});
			let frame = crate::Frame {
				texture: &tex,
				format: DXGI_FORMAT_R16G16B16A16_FLOAT,
				width: w,
				height: h,
				is_new: true,
				force_idr: false,
				present_ns: 0,
			};
			unsafe { enc.submit(&frame, i as i64, i * 1500) }.expect("submit");
		}
		unsafe { enc.flush_and_close() };
		std::env::remove_var("PULSAR_DUMP_BITSTREAM");
		let bs = std::fs::read(&out).expect("bitstream dump");
		// SPS (NAL 33): general_profile_idc in the low 5 bits of its 2nd payload byte = 2 (Main10).
		let sps = bs.windows(5).position(|w| w[..4] == [0, 0, 1, 0x42]).expect("SPS");
		assert_eq!(bs[sps + 6] & 0x1F, 2, "profile idc");
		// Our prefix SEI with mastering_display_colour_volume (137) is there.
		assert!(bs.windows(4).any(|w| w == [0x4E, 0x01, 137, 24]), "HDR SEI");
		eprintln!("HDR test stream: {} bytes at {}", bs.len(), out.display());
	}

	#[test]
	fn rotation_matrices_are_orthonormal() {
		for d in [0, 90, 180, 270] {
			let m = rotation_matrix(d);
			assert_eq!((m[0] * m[3] - m[1] * m[2]), 1.0, "{d}");
		}
	}
}
