//! Glass-to-encode measurement for the `capbench` example.
//!
//! The per-source timestamps (`Frame::present_ns`) come from different clocks' points of
//! view — DXGI's `LastPresentTime` and WGC's `SystemRelativeTime` need not mark the same
//! moment — so they cannot settle DXGI vs WGC on their own. capbench paints a counter into a
//! window (colour R = n & 0xFF, G = n >> 8 & 0xFF, B = 0x80) and records each paint's QPC time
//! here; with `PULSAR_BENCH_PIXEL=1` the capture thread reads the centre pixel of every fresh
//! frame after encoding it and measures from that paint instead. The centre pixel stays put
//! under any display rotation, so the unrotated DXGI surface and the upright WGC one agree.
#![cfg(windows)]

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::OnceLock;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
	ID3D11Device, ID3D11Texture2D, D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE,
	D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

static MARKS: OnceLock<Vec<AtomicI64>> = OnceLock::new();

fn marks() -> &'static [AtomicI64] {
	MARKS.get_or_init(|| (0..65536).map(|_| AtomicI64::new(0)).collect())
}

/// The capture thread's clock (QPC, ns).
pub fn now_ns() -> i64 {
	unsafe { crate::dxgi::platform::Qpc::new().now_ns() }
}

/// capbench painted counter `n` at `qpc_ns` (see the module doc for the colour encoding).
pub fn mark_paint(n: u32, qpc_ns: i64) {
	marks()[(n & 0xFFFF) as usize].store(qpc_ns, Ordering::Relaxed);
}

/// Per-build reader of a frame's centre pixel.
pub(crate) struct PixelProbe {
	staging: Option<(usize, ID3D11Texture2D)>,
}

impl PixelProbe {
	pub(crate) fn enabled() -> bool {
		std::env::var("PULSAR_BENCH_PIXEL").map(|v| v == "1").unwrap_or(false)
	}

	pub(crate) fn new() -> Self {
		PixelProbe { staging: None }
	}

	/// The paint time of the mark in `tex`'s centre pixel, 0 when it carries none.
	pub(crate) unsafe fn paint_ns(&mut self, tex: &ID3D11Texture2D) -> i64 {
		let Ok(dev) = tex.GetDevice() else { return 0 };
		let Ok(ctx) = dev.GetImmediateContext() else { return 0 };
		let key = dev.as_raw() as usize;
		if self.staging.as_ref().map(|(k, _)| *k) != Some(key) {
			self.staging = make_staging(&dev).map(|t| (key, t));
		}
		let Some((_, staging)) = self.staging.as_ref() else { return 0 };
		let mut d = D3D11_TEXTURE2D_DESC::default();
		tex.GetDesc(&mut d);
		let (cx, cy) = (d.Width / 2, d.Height / 2);
		let bx = D3D11_BOX { left: cx, top: cy, front: 0, right: cx + 1, bottom: cy + 1, back: 1 };
		ctx.CopySubresourceRegion(staging, 0, 0, 0, 0, tex, 0, Some(&bx));
		let mut m = D3D11_MAPPED_SUBRESOURCE::default();
		if ctx.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut m)).is_err() {
			return 0;
		}
		let px = std::slice::from_raw_parts(m.pData as *const u8, 4);
		let (b, g, r) = (px[0], px[1], px[2]);
		ctx.Unmap(staging, 0);
		if b != 0x80 {
			return 0;
		}
		marks()[(r as usize) | ((g as usize) << 8)].load(Ordering::Relaxed)
	}
}

unsafe fn make_staging(dev: &ID3D11Device) -> Option<ID3D11Texture2D> {
	let desc = D3D11_TEXTURE2D_DESC {
		Width: 1,
		Height: 1,
		MipLevels: 1,
		ArraySize: 1,
		Format: DXGI_FORMAT_B8G8R8A8_UNORM,
		SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
		Usage: D3D11_USAGE_STAGING,
		BindFlags: 0,
		CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
		MiscFlags: 0,
	};
	let mut t: Option<ID3D11Texture2D> = None;
	dev.CreateTexture2D(&desc, None, Some(&mut t)).ok()?;
	t
}
