//! Synthetic encoder bench for `examples/mfbench.rs`: a moving noise block over a static
//! picture, encoded by the first non-NVIDIA hardware GPU's Media Foundation encoder at a steady
//! fps and sent as RTP — no screen, no window, so it runs headless. Scripted events (live
//! bitrate step, keyframe request, a client-reported loss) exercise the live controls.
#![cfg(windows)]

use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{
	D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
	D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
	D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE};

/// A scripted event, applied before encoding frame `at`.
#[derive(Clone, Copy, Debug)]
pub enum SynthEvent {
	/// Live bitrate change (kbps).
	Bitrate(u32),
	/// Keyframe on request.
	Keyframe,
	/// The client reports frame `at - back` lost (its RTP sequence range → `invalidate_lost`).
	Loss { back: u64 },
}

#[derive(Clone, Debug)]
pub struct SynthConfig {
	pub width: u32,
	pub height: u32,
	pub fps: u32,
	pub frames: u64,
	pub bitrate_kbps: u32,
	pub codec: crate::Codec,
	pub rfi: bool,
	pub low_latency: bool,
	pub dest: String,
	pub events: Vec<(u64, SynthEvent)>,
}

#[derive(Clone, Debug, Default)]
pub struct SynthReport {
	pub encoder: String,
	/// Per frame: (submit start since the run began µs, submit duration µs, RTP timestamp).
	pub frames: Vec<(u64, u32, u32)>,
	/// Per `Loss` event: (the lost frame's RTP timestamp, `invalidate_lost` result).
	pub losses: Vec<(u32, Result<usize, &'static str>)>,
	pub submit_errors: u32,
}

/// Encode `cfg.frames` synthetic frames through `MfEncoder` to `cfg.dest` (RTP).
pub fn run_synthetic_mf(cfg: &SynthConfig) -> Result<SynthReport, String> {
	unsafe {
		let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(|e| e.to_string())?;
		let mut adapter = None;
		let mut i = 0;
		while let Ok(a) = factory.EnumAdapters1(i) {
			i += 1;
			let d = a.GetDesc1().map_err(|e| e.to_string())?;
			if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 && d.VendorId != 0x1414 && d.VendorId != 0x10DE {
				adapter = Some(a);
				break;
			}
		}
		let adapter = adapter.ok_or("no non-NVIDIA hardware adapter")?;
		let mut device: Option<ID3D11Device> = None;
		let mut ctx: Option<ID3D11DeviceContext> = None;
		D3D11CreateDevice(
			&adapter,
			D3D_DRIVER_TYPE_UNKNOWN,
			windows::Win32::Foundation::HMODULE::default(),
			D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
			Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
			D3D11_SDK_VERSION,
			Some(&mut device),
			None,
			Some(&mut ctx),
		)
		.map_err(|e| format!("D3D11CreateDevice: {e}"))?;
		let (device, ctx) = (device.ok_or("no device")?, ctx.ok_or("no context")?);
		let (w, h) = (cfg.width & !1, cfg.height & !1);

		// The static picture: a gradient with coarse texture (like a desktop with text).
		let mut rng = 0x1234_5678u32;
		let mut next = move || {
			rng ^= rng << 13;
			rng ^= rng >> 17;
			rng ^= rng << 5;
			rng
		};
		let mut base = vec![0u8; (w * h * 4) as usize];
		for y in 0..h {
			for x in 0..w {
				let o = ((y * w + x) * 4) as usize;
				let t = if (x / 8 + y / 16) % 7 == 0 { (next() & 0x3F) as u8 } else { 0 };
				base[o] = (x * 255 / w) as u8 ^ t;
				base[o + 1] = (y * 255 / h) as u8;
				base[o + 2] = 0x60 ^ t;
				base[o + 3] = 0xFF;
			}
		}
		let desc = D3D11_TEXTURE2D_DESC {
			Width: w,
			Height: h,
			MipLevels: 1,
			ArraySize: 1,
			Format: DXGI_FORMAT_B8G8R8A8_UNORM,
			SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
			Usage: D3D11_USAGE_DEFAULT,
			BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
			CPUAccessFlags: 0,
			MiscFlags: 0,
		};
		let init = D3D11_SUBRESOURCE_DATA { pSysMem: base.as_ptr() as *const _, SysMemPitch: w * 4, SysMemSlicePitch: 0 };
		let mut tex: Option<ID3D11Texture2D> = None;
		device.CreateTexture2D(&desc, Some(&init), Some(&mut tex)).map_err(|e| format!("texture: {e}"))?;
		let tex = tex.ok_or("no texture")?;

		let params = crate::encode::EncParams {
			width: w,
			height: h,
			capture_width: w,
			capture_height: h,
			fps: cfg.fps,
			bitrate_kbps: cfg.bitrate_kbps,
			dest: cfg.dest.clone(),
			codec: cfg.codec,
			low_latency: cfg.low_latency,
			intra_refresh: false,
			rotation: 0,
			rfi: cfg.rfi,
			// Media Foundation is SDR 4:2:0 only.
			hdr: None,
			yuv444: false,
		};
		let mut enc = crate::encode::mf::MfEncoder::new(&device, &ctx, &params)?;
		// Like the real capture thread.
		{
			use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL};
			let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
		}
		let mut rep = SynthReport { encoder: enc.name().to_string(), ..Default::default() };

		// A moving block of fresh noise (a quarter of the width, a third of the height) whose
		// position sweeps the picture: the encoder sees real motion every frame.
		let (bw, bh) = ((w / 4) & !1, (h / 3) & !1);
		let mut block = vec![0u8; (bw * bh * 4) as usize];
		let t0 = Instant::now();
		let period = Duration::from_nanos(1_000_000_000 / cfg.fps.max(1) as u64);
		let mut prev: Option<(u32, u32)> = None;
		for n in 0..cfg.frames {
			// Steady cadence (sleep, then spin for the last part).
			let due = t0 + period * n as u32;
			while Instant::now() + Duration::from_millis(2) < due {
				std::thread::sleep(Duration::from_millis(1));
			}
			while Instant::now() < due {
				std::hint::spin_loop();
			}
			// Restore where the block was, draw it at its new place.
			if let Some((px, py)) = prev {
				let bx = D3D11_BOX { left: px, top: py, front: 0, right: px + bw, bottom: py + bh, back: 1 };
				let off = ((py * w + px) * 4) as usize;
				ctx.UpdateSubresource(&tex, 0, Some(&bx), base[off..].as_ptr() as *const _, w * 4, 0);
			}
			let px = ((n * 7) % (w - bw) as u64) as u32 & !1;
			let py = ((n * 3) % (h - bh) as u64) as u32 & !1;
			for c in block.chunks_exact_mut(4) {
				let r = next();
				c.copy_from_slice(&[r as u8, (r >> 8) as u8, (r >> 16) as u8, 0xFF]);
			}
			let bx = D3D11_BOX { left: px, top: py, front: 0, right: px + bw, bottom: py + bh, back: 1 };
			ctx.UpdateSubresource(&tex, 0, Some(&bx), block.as_ptr() as *const _, bw * 4, 0);
			prev = Some((px, py));

			for (_, ev) in cfg.events.iter().filter(|(at, _)| *at == n) {
				match *ev {
					SynthEvent::Bitrate(k) => enc.reconfigure_bitrate(k)?,
					SynthEvent::Keyframe => enc.request_idr(),
					SynthEvent::Loss { back } => {
						let lost_ts = rep.frames.get(n.saturating_sub(back) as usize).map(|f| f.2).unwrap_or(0);
						let r = match enc.rtp_seq_range_of(lost_ts) {
							Some((a, b)) => enc.invalidate_lost(a, b),
							None => Err("lost frame not sent yet"),
						};
						rep.losses.push((lost_ts, r));
					}
				}
			}
			let frame = crate::Frame {
				texture: &tex,
				format: DXGI_FORMAT_B8G8R8A8_UNORM,
				width: w,
				height: h,
				is_new: true,
				force_idr: false,
				present_ns: 0,
			};
			let start = t0.elapsed();
			let rtp_ts = (start.as_micros() as u64 * 9 / 100) as u32;
			if enc.submit(&frame, n as i64, rtp_ts).is_err() {
				rep.submit_errors += 1;
			}
			rep.frames.push((start.as_micros() as u64, (t0.elapsed() - start).as_micros() as u32, rtp_ts));
		}
		// Let the last access units leave the RTP sender.
		std::thread::sleep(Duration::from_millis(100));
		enc.flush_and_close();
		Ok(rep)
	}
}
