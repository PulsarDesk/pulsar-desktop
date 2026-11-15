//! HDR10 present: a decoded P010 picture (BT.2020, PQ, limited range — what a Pulsar host's
//! HDR stream decodes to, `decode.rs`) drawn by our own pixel shader, in one of two ways:
//!
//! - **`Out::Pq`** — the client monitor is in HDR mode: the swapchain is R10G10B10A2 with the
//!   HDR10 colour space (`DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020`, set in `mod.rs`), and the
//!   shader only turns Y'CbCr into R'G'B' — the PQ signal passes through untouched, so the
//!   monitor shows the host's HDR as it was.
//! - **`Out::Sdr`** — an SDR monitor: PQ → nits → BT.709 primaries → a highlight roll-off
//!   (identity up to 80 % of SDR reference white, then compressed toward 1.0 — no clipping of
//!   bright highlights, no dimming of the rest; 1000 nits — the HDR10 mastering peak a host
//!   reports by default — lands on 1.0) → sRGB, into the usual BGRA8 swapchain.
//!   Reference white is 203 nits (ITU-R BT.2408), which is where a host's SDR desktop sits in
//!   HDR mode, so the desktop looks like it does on an SDR stream.
//!
//! The VideoProcessor is not used for P010: its HDR colour-space conversion is optional per
//! driver and unreported; a driver that ignored it would show PQ code values as gamma ones
//! (washed out). Both shaders are exact maths on every GPU; `tests` check them against the CPU
//! reference below. Scaling is bilinear (the upscalers need 8-bit SDR input — in `Out::Sdr` the
//! tone-mapped picture can go through FSR / SGSR, `present.rs`).

use windows::core::{Interface, Result, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D11_SRV_DIMENSION_TEXTURE2D, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST};
use windows::Win32::Graphics::Direct3D11::{
	ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView,
	ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader, D3D11_BIND_CONSTANT_BUFFER,
	D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_WRITE, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_MAPPED_SUBRESOURCE,
	D3D11_MAP_WRITE_DISCARD, D3D11_SAMPLER_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0,
	D3D11_TEX2D_SRV, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DYNAMIC, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_R16G16_UNORM, DXGI_FORMAT_R16_UNORM};

/// How the HDR picture leaves the shader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Out {
	/// PQ R'G'B' (BT.2020) for an HDR10 swapchain.
	Pq,
	/// Tone-mapped sRGB (BT.709) for an SDR swapchain / the upscalers.
	Sdr,
}

/// SDR reference white in nits (ITU-R BT.2408): `Out::Sdr` maps it to 1.0 before the roll-off.
pub const SDR_WHITE_NITS: f32 = 203.0;
/// Below this share of reference white the tone curve is the identity.
pub const KNEE: f32 = 0.8;
/// The highlight that maps to 1.0 (nits); brighter clips.
pub const PEAK_NITS: f32 = 1000.0;

/// The roll-off above the knee (`m`, `ks`, `w` in units of reference white): extended
/// Reinhard on the excess over the knee, scaled so the peak `w` lands exactly on 1.0 and the
/// slope is 1 at the knee (no visible kink).
pub fn roll_off(m: f32, ks: f32, w: f32) -> f32 {
	if m <= ks {
		return m;
	}
	let x = (m - ks) / (1.0 - ks);
	let xw = ((w - ks) / (1.0 - ks)).max(1.0);
	(ks + (1.0 - ks) * x * (1.0 + x / (xw * xw)) / (1.0 + x)).min(1.0)
}

const HLSL: &str = r#"
cbuffer C : register(b0) {
	float4 src;  // uv offset (xy) and size (zw) of the source crop, normalised to the texture
	float4 rot;  // 2x2 rotation applied to (uv - 0.5): output -> unrotated source
	float4 tm;   // x = SDR white (nits), y = knee, z = peak (in units of SDR white)
};
Texture2D<float> ty : register(t0);
Texture2D<float2> tc : register(t1);
SamplerState lin : register(s0);
struct VOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
VOut vs_main(uint id : SV_VertexID) {
	VOut o;
	float2 uv = float2((id << 1) & 2, id & 2);
	o.pos = float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
	float2 c = uv - 0.5;
	float2 r = float2(dot(rot.xy, c), dot(rot.zw, c)) + 0.5;
	o.uv = src.xy + r * src.zw;
	return o;
}
// P010 texel (code << 6 in 16 bits) -> limited-range BT.2020 Y'CbCr -> PQ R'G'B'.
float3 rgb_pq(float2 uv) {
	float y = (ty.Sample(lin, uv) * (65535.0 / 64.0) - 64.0) / 876.0;
	float2 c = (tc.Sample(lin, uv) * (65535.0 / 64.0) - 512.0) / 896.0;
	float r = y + 1.4746 * c.y;
	float b = y + 1.8814 * c.x;
	float g = (y - 0.2627 * r - 0.0593 * b) / 0.6780;
	return saturate(float3(r, g, b));
}
float4 ps_pq(VOut i) : SV_Target { return float4(rgb_pq(i.uv), 1.0); }
float3 eotf(float3 e) {
	float3 p = pow(e, 1.0 / 78.84375);
	return 10000.0 * pow(max(p - 0.8359375, 0.0) / (18.8515625 - 18.6875 * p), 1.0 / 0.1593017578125);
}
static const float3x3 BT2020_TO_709 = {
	 1.6604910, -0.5876411, -0.0728499,
	-0.1245505,  1.1328999, -0.0083494,
	-0.0181508, -0.1005789,  1.1187297 };
float3 srgb(float3 l) { return l <= 0.0031308 ? l * 12.92 : 1.055 * pow(l, 1.0 / 2.4) - 0.055; }
float4 ps_sdr(VOut i) : SV_Target {
	float3 l = max(mul(BT2020_TO_709, eotf(rgb_pq(i.uv))), 0.0) / tm.x;
	float m = max(max(l.r, l.g), l.b);
	float ks = tm.y;
	if (m > ks) {
		float x = (m - ks) / (1.0 - ks);
		float xw = max((tm.z - ks) / (1.0 - ks), 1.0);
		float mt = min(ks + (1.0 - ks) * x * (1.0 + x / (xw * xw)) / (1.0 + x), 1.0);
		l *= mt / m;
	}
	return float4(srgb(saturate(l)), 1.0);
}
"#;

/// CPU reference of `ps_sdr` for one neutral (grey) PQ code: its 0–1 sRGB output.
pub fn sdr_reference_grey(y_code: u16) -> f32 {
	let e = ((y_code as f32 - 64.0) / 876.0).clamp(0.0, 1.0);
	let (m1, m2, c1, c2, c3) = (0.159_301_76_f32, 78.843_75_f32, 0.835_937_5_f32, 18.851_562_f32, 18.6875_f32);
	let p = e.powf(1.0 / m2);
	let nits = 10_000.0 * ((p - c1).max(0.0) / (c2 - c3 * p)).powf(1.0 / m1);
	let l = roll_off(nits / SDR_WHITE_NITS, KNEE, PEAK_NITS / SDR_WHITE_NITS).clamp(0.0, 1.0);
	if l <= 0.003_130_8 {
		l * 12.92
	} else {
		1.055 * l.powf(1.0 / 2.4) - 0.055
	}
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Consts {
	src: [f32; 4],
	rot: [f32; 4],
	tm: [f32; 4],
}

fn compile(entry: &str, target: &str) -> Result<ID3DBlob> {
	let e = format!("{entry}\0");
	let t = format!("{target}\0");
	let mut code = None;
	let mut errs: Option<ID3DBlob> = None;
	unsafe {
		let r = D3DCompile(
			HLSL.as_ptr() as *const _,
			HLSL.len(),
			PCSTR(b"pulsar-hdr-present\0".as_ptr()),
			None,
			None,
			PCSTR(e.as_ptr()),
			PCSTR(t.as_ptr()),
			D3DCOMPILE_OPTIMIZATION_LEVEL3,
			0,
			&mut code,
			Some(&mut errs),
		);
		if let Err(err) = r {
			if let Some(b) = errs {
				let msg = std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize());
				eprintln!("pulsar-render(win): HDR shader {entry}: {}", String::from_utf8_lossy(msg));
			}
			return Err(err);
		}
	}
	Ok(code.unwrap())
}

fn bytes(b: &ID3DBlob) -> &[u8] {
	unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()) }
}

/// The uv rotation (row-major 2×2 on `uv − 0.5`) that samples the unrotated source for an
/// output rotated `deg` clockwise (the VideoProcessor ROTATION_n convention the NV12 path uses).
fn rotation_matrix(deg: u32) -> [f32; 4] {
	match deg {
		90 => [0.0, 1.0, -1.0, 0.0],
		180 => [-1.0, 0.0, 0.0, -1.0],
		270 => [0.0, -1.0, 1.0, 0.0],
		_ => [1.0, 0.0, 0.0, 1.0],
	}
}

/// The two shader passes + their state, built once per renderer.
pub struct HdrPresent {
	device: ID3D11Device,
	ctx: ID3D11DeviceContext,
	vs: ID3D11VertexShader,
	ps_pq: ID3D11PixelShader,
	ps_sdr: ID3D11PixelShader,
	lin: ID3D11SamplerState,
	cb: ID3D11Buffer,
	/// Plane views of the last source texture (keyed by its COM pointer).
	views: Option<(*mut core::ffi::c_void, ID3D11ShaderResourceView, ID3D11ShaderResourceView)>,
}

fn plane_view(device: &ID3D11Device, tex: &ID3D11Texture2D, fmt: DXGI_FORMAT) -> Result<ID3D11ShaderResourceView> {
	let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
		Format: fmt,
		ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
		Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_SRV { MostDetailedMip: 0, MipLevels: 1 } },
	};
	let mut v = None;
	unsafe { device.CreateShaderResourceView(tex, Some(&desc), Some(&mut v))? };
	Ok(v.unwrap())
}

impl HdrPresent {
	pub fn new(device: &ID3D11Device) -> Result<Self> {
		unsafe {
			let (vs_b, pq_b, sdr_b) = (compile("vs_main", "vs_5_0")?, compile("ps_pq", "ps_5_0")?, compile("ps_sdr", "ps_5_0")?);
			let (mut vs, mut ps_pq, mut ps_sdr) = (None, None, None);
			device.CreateVertexShader(bytes(&vs_b), None, Some(&mut vs))?;
			device.CreatePixelShader(bytes(&pq_b), None, Some(&mut ps_pq))?;
			device.CreatePixelShader(bytes(&sdr_b), None, Some(&mut ps_sdr))?;
			let sd = D3D11_SAMPLER_DESC {
				Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
				AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
				AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
				AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
				MaxLOD: f32::MAX,
				..Default::default()
			};
			let mut lin = None;
			device.CreateSamplerState(&sd, Some(&mut lin))?;
			let bd = D3D11_BUFFER_DESC {
				ByteWidth: std::mem::size_of::<Consts>() as u32,
				Usage: D3D11_USAGE_DYNAMIC,
				BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
				CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
				..Default::default()
			};
			let mut cb = None;
			device.CreateBuffer(&bd, None, Some(&mut cb))?;
			Ok(Self {
				device: device.clone(),
				ctx: device.GetImmediateContext()?,
				vs: vs.unwrap(),
				ps_pq: ps_pq.unwrap(),
				ps_sdr: ps_sdr.unwrap(),
				lin: lin.unwrap(),
				cb: cb.unwrap(),
				views: None,
			})
		}
	}

	/// Draw the `crop` (x, y, w, h in texels, pre-rotation) of the P010 `src` (`tex_w`×`tex_h`
	/// surface) rotated `rot_deg` clockwise into `viewport` (x, y, w, h) of `rtv`.
	#[allow(clippy::too_many_arguments)]
	pub unsafe fn draw(
		&mut self,
		src: &ID3D11Texture2D,
		tex_w: u32,
		tex_h: u32,
		crop: (f32, f32, f32, f32),
		rot_deg: u32,
		rtv: &ID3D11RenderTargetView,
		viewport: (f32, f32, f32, f32),
		out: Out,
	) -> Result<()> {
		let key = src.as_raw();
		if self.views.as_ref().map(|v| v.0) != Some(key) {
			let y = plane_view(&self.device, src, DXGI_FORMAT_R16_UNORM)?;
			let c = plane_view(&self.device, src, DXGI_FORMAT_R16G16_UNORM)?;
			self.views = Some((key, y, c));
		}
		let (_, vy, vc) = self.views.as_ref().unwrap();
		let (tw, th) = (tex_w.max(1) as f32, tex_h.max(1) as f32);
		let consts = Consts {
			src: [crop.0 / tw, crop.1 / th, crop.2 / tw, crop.3 / th],
			rot: rotation_matrix(rot_deg),
			tm: [SDR_WHITE_NITS, KNEE, PEAK_NITS / SDR_WHITE_NITS, 0.0],
		};
		let ctx = &self.ctx;
		let mut m = D3D11_MAPPED_SUBRESOURCE::default();
		ctx.Map(&self.cb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut m))?;
		std::ptr::copy_nonoverlapping(&consts as *const Consts as *const u8, m.pData as *mut u8, std::mem::size_of::<Consts>());
		ctx.Unmap(&self.cb, 0);
		ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
		ctx.OMSetBlendState(None, None, 0xffff_ffff);
		ctx.RSSetState(None);
		ctx.RSSetViewports(Some(&[D3D11_VIEWPORT {
			TopLeftX: viewport.0,
			TopLeftY: viewport.1,
			Width: viewport.2,
			Height: viewport.3,
			MinDepth: 0.0,
			MaxDepth: 1.0,
		}]));
		ctx.IASetInputLayout(None);
		ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
		ctx.VSSetShader(&self.vs, None);
		ctx.VSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
		ctx.PSSetShader(if out == Out::Pq { &self.ps_pq } else { &self.ps_sdr }, None);
		ctx.PSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
		ctx.PSSetSamplers(0, Some(&[Some(self.lin.clone())]));
		ctx.PSSetShaderResources(0, Some(&[Some(vy.clone()), Some(vc.clone())]));
		ctx.Draw(3, 0);
		ctx.PSSetShaderResources(0, Some(&[None, None]));
		Ok(())
	}
}

/// Test helper shared with `decode.rs`: tone-map a P010 picture's visible `w`×`h` into a
/// BGRA8 target of the same size and read back its middle row.
#[cfg(test)]
pub(crate) mod tests_support {
	use super::*;
	use windows::Win32::Graphics::Direct3D11::{
		D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
		D3D11_USAGE_STAGING,
	};
	use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

	pub(crate) unsafe fn sdr_row(d: &ID3D11Device, src: &ID3D11Texture2D, tw: u32, th: u32, w: u32, h: u32) -> Vec<u32> {
		let mk = |usage, bind, cpu| D3D11_TEXTURE2D_DESC {
			Width: w,
			Height: h,
			MipLevels: 1,
			ArraySize: 1,
			Format: DXGI_FORMAT_B8G8R8A8_UNORM,
			SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
			Usage: usage,
			BindFlags: bind,
			CPUAccessFlags: cpu,
			MiscFlags: 0,
		};
		let (mut t, mut s, mut rtv) = (None, None, None);
		d.CreateTexture2D(&mk(D3D11_USAGE_DEFAULT, D3D11_BIND_RENDER_TARGET.0 as u32, 0), None, Some(&mut t)).unwrap();
		d.CreateTexture2D(&mk(D3D11_USAGE_STAGING, 0, D3D11_CPU_ACCESS_READ.0 as u32), None, Some(&mut s)).unwrap();
		let (t, s): (ID3D11Texture2D, ID3D11Texture2D) = (t.unwrap(), s.unwrap());
		d.CreateRenderTargetView(&t, None, Some(&mut rtv)).unwrap();
		let mut hp = HdrPresent::new(d).unwrap();
		hp.draw(src, tw, th, (0.0, 0.0, w as f32, h as f32), 0, &rtv.unwrap(), (0.0, 0.0, w as f32, h as f32), Out::Sdr)
			.unwrap();
		let ctx = d.GetImmediateContext().unwrap();
		ctx.CopyResource(&s, &t);
		let mut m = D3D11_MAPPED_SUBRESOURCE::default();
		ctx.Map(&s, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
		let base = (m.pData as *const u8).add(m.RowPitch as usize * (h as usize / 2)) as *const u32;
		let row = std::slice::from_raw_parts(base, w as usize).to_vec();
		ctx.Unmap(&s, 0);
		row
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
	use windows::Win32::Graphics::Direct3D11::{
		D3D11CreateDevice, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ,
		D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA,
		D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
	};
	use windows::Win32::Graphics::Dxgi::Common::{
		DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_P010, DXGI_FORMAT_R10G10B10A2_UNORM, DXGI_SAMPLE_DESC,
	};

	fn device() -> Option<ID3D11Device> {
		let mut d = None;
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
				None,
			)
			.ok()?;
		}
		d
	}

	/// A `w`×`h` P010 texture: luma code `y(x)` per column, neutral chroma.
	fn p010(d: &ID3D11Device, w: u32, h: u32, y: impl Fn(u32) -> u16) -> Option<ID3D11Texture2D> {
		let mut data: Vec<u16> = Vec::new();
		for _ in 0..h {
			for x in 0..w {
				data.push(y(x) << 6);
			}
		}
		for _ in 0..h / 2 {
			for _ in 0..w / 2 {
				data.extend_from_slice(&[512 << 6, 512 << 6]);
			}
		}
		let desc = D3D11_TEXTURE2D_DESC {
			Width: w,
			Height: h,
			MipLevels: 1,
			ArraySize: 1,
			Format: DXGI_FORMAT_P010,
			SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
			Usage: D3D11_USAGE_DEFAULT,
			BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
			CPUAccessFlags: 0,
			MiscFlags: 0,
		};
		let init = D3D11_SUBRESOURCE_DATA { pSysMem: data.as_ptr() as *const _, SysMemPitch: w * 2, SysMemSlicePitch: 0 };
		let mut t = None;
		unsafe { d.CreateTexture2D(&desc, Some(&init), Some(&mut t)) }.ok()?;
		t
	}

	/// Render `out` into a `w`×`h` target of `fmt`; returns the first row's raw u32 texels.
	fn render(d: &ID3D11Device, src: &ID3D11Texture2D, w: u32, h: u32, rot: u32, fmt: DXGI_FORMAT, out: Out) -> Vec<u32> {
		let mk = |usage, bind, cpu| D3D11_TEXTURE2D_DESC {
			Width: w,
			Height: h,
			MipLevels: 1,
			ArraySize: 1,
			Format: fmt,
			SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
			Usage: usage,
			BindFlags: bind,
			CPUAccessFlags: cpu,
			MiscFlags: 0,
		};
		unsafe {
			let (mut t, mut s, mut rtv) = (None, None, None);
			d.CreateTexture2D(&mk(D3D11_USAGE_DEFAULT, D3D11_BIND_RENDER_TARGET.0 as u32, 0), None, Some(&mut t)).unwrap();
			d.CreateTexture2D(&mk(D3D11_USAGE_STAGING, 0, D3D11_CPU_ACCESS_READ.0 as u32), None, Some(&mut s)).unwrap();
			let (t, s): (ID3D11Texture2D, ID3D11Texture2D) = (t.unwrap(), s.unwrap());
			d.CreateRenderTargetView(&t, None, Some(&mut rtv)).unwrap();
			let mut hp = HdrPresent::new(d).unwrap();
			let mut sd = D3D11_TEXTURE2D_DESC::default();
			src.GetDesc(&mut sd);
			hp.draw(src, sd.Width, sd.Height, (0.0, 0.0, sd.Width as f32, sd.Height as f32), rot, &rtv.unwrap(), (0.0, 0.0, w as f32, h as f32), out)
				.unwrap();
			let ctx = d.GetImmediateContext().unwrap();
			ctx.CopyResource(&s, &t);
			let mut m = D3D11_MAPPED_SUBRESOURCE::default();
			ctx.Map(&s, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
			let row = std::slice::from_raw_parts(m.pData as *const u32, w as usize).to_vec();
			ctx.Unmap(&s, 0);
			row
		}
	}

	#[test]
	fn pq_passes_through_and_sdr_tone_maps_like_the_reference() {
		let Some(d) = device() else { return };
		// Columns: black (64), SDR white (572 ≈ 203 nits), 1000 nits (~769), 10 000 nits (940).
		let codes = [64u16, 572, 769, 940];
		let Some(src) = p010(&d, 8, 4, |x| codes[(x / 2) as usize]) else {
			eprintln!("skip: no P010 textures on this GPU");
			return;
		};
		// PQ: the R'G'B' signal is Y' itself for neutral grey (10-bit R10G10B10A2 channel).
		let row = render(&d, &src, 8, 4, 0, DXGI_FORMAT_R10G10B10A2_UNORM, Out::Pq);
		for (i, &c) in codes.iter().enumerate() {
			let px = row[i * 2];
			let want = ((c as f32 - 64.0) / 876.0 * 1023.0).round() as i32;
			for ch in [px & 0x3FF, (px >> 10) & 0x3FF, (px >> 20) & 0x3FF] {
				assert!((ch as i32 - want).abs() <= 2, "PQ code {c}: {ch} vs {want}");
			}
		}
		// SDR: grey, monotonic, black stays black, SDR white near the top, 10 000 nits = 255.
		let row = render(&d, &src, 8, 4, 0, DXGI_FORMAT_B8G8R8A8_UNORM, Out::Sdr);
		let mut last = -1i32;
		for (i, &c) in codes.iter().enumerate() {
			let px = row[i * 2];
			let (b, g, r) = (px & 0xFF, (px >> 8) & 0xFF, (px >> 16) & 0xFF);
			assert!(b.abs_diff(g) <= 1 && g.abs_diff(r) <= 1, "grey stays grey: {r} {g} {b}");
			let want = (sdr_reference_grey(c) * 255.0).round() as i32;
			assert!((g as i32 - want).abs() <= 2, "SDR code {c}: {g} vs {want}");
			assert!(g as i32 > last || (i == 3 && g == 255), "monotonic: {g} after {last}");
			last = g as i32;
		}
		assert!(row[0] & 0xFF <= 1);
		assert!((row[2] >> 8) & 0xFF >= 230, "SDR white stays bright: {}", (row[2] >> 8) & 0xFF);
		// 180° rotation mirrors the columns.
		let row = render(&d, &src, 8, 4, 180, DXGI_FORMAT_B8G8R8A8_UNORM, Out::Sdr);
		assert!(row[0] & 0xFF >= 250 && row[7] & 0xFF <= 1, "{:x} {:x}", row[0], row[7]);
	}

	#[test]
	fn the_overlay_painter_builds_with_its_pq_shader() {
		let Some(d) = device() else { return };
		let p = crate::win::egui_paint::EguiPaint::new(&d).expect("egui painter (SDR + PQ shaders)");
		p.set_pq(true);
	}

	#[test]
	fn the_tone_curve_keeps_sdr_and_compresses_highlights() {
		// 100 nits (half of reference white): identity region.
		let l = sdr_reference_grey(((0.508 * 876.0) + 64.0) as u16);
		assert!(l > 0.70 && l < 0.76, "{l}");
		assert!(sdr_reference_grey(940) <= 1.0 && sdr_reference_grey(940) > 0.99);
		assert_eq!(sdr_reference_grey(64), 0.0);
		// The roll-off: identity below the knee, continuous, increasing, peak → 1.0.
		let w = PEAK_NITS / SDR_WHITE_NITS;
		assert_eq!(roll_off(0.5, KNEE, w), 0.5);
		assert!((roll_off(KNEE + 1e-4, KNEE, w) - KNEE).abs() < 1e-3);
		assert!((roll_off(w, KNEE, w) - 1.0).abs() < 1e-5);
		let mut last = 0.0;
		for i in 1..100 {
			let v = roll_off(i as f32 * 0.05, KNEE, w);
			assert!(v >= last);
			last = v;
		}
		// SDR white keeps ~90 % (not dimmed to half like a global Reinhard would).
		assert!(roll_off(1.0, KNEE, w) > 0.89);
	}
}
