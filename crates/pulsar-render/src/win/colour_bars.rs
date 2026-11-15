//! Colour-bar harness for the Windows client: decode a real H.264 stream of red / green /
//! blue / white bars with the Media Foundation decoder, convert it with the present path's
//! VideoProcessor (`Present::set_colour` + `blt`) into a BGRA texture, read it back and check
//! the bars come out as the RGB the host captured. Fixtures (`testdata/colour-*.h264`, made by
//! the bundled ffmpeg the way a Pulsar host encodes — see testdata/README.md):
//!
//! * `colour-bt709.h264` — BT.709 limited, tagged (what every Pulsar host sends);
//! * `colour-untagged.h264` — BT.709 limited, no colour description (an encoder that writes
//!   none): must decode as BT.709, Pulsar's default;
//! * `colour-bt601.h264` — BT.601 limited, tagged: must follow the tag.
//!
//! Needs a D3D11 device with video support (any GPU, or skips with a note).

use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
	D3D11CreateDevice, ID3D11Device, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ,
	D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAP_READ,
	D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

use super::decode::Decoder;
use super::present::Present;
use crate::colour::{Colour, Matrix};
use crate::stream::{AccessUnit, Codec};

const W: u32 = 256;
const H: u32 = 128;

fn device() -> Option<ID3D11Device> {
	let mut device = None;
	// SAFETY: plain device creation with out-params owned here.
	unsafe {
		D3D11CreateDevice(
			None,
			D3D_DRIVER_TYPE_HARDWARE,
			Default::default(),
			D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
			Some(&[D3D_FEATURE_LEVEL_11_0]),
			D3D11_SDK_VERSION,
			Some(&mut device),
			None,
			None,
		)
		.ok()?;
	}
	device
}

unsafe fn texture(dev: &ID3D11Device, staging: bool) -> ID3D11Texture2D {
	let desc = D3D11_TEXTURE2D_DESC {
		Width: W,
		Height: H,
		MipLevels: 1,
		ArraySize: 1,
		Format: DXGI_FORMAT_B8G8R8A8_UNORM,
		SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
		Usage: if staging { D3D11_USAGE_STAGING } else { D3D11_USAGE_DEFAULT },
		BindFlags: if staging { 0 } else { D3D11_BIND_RENDER_TARGET.0 as u32 },
		CPUAccessFlags: if staging { D3D11_CPU_ACCESS_READ.0 as u32 } else { 0 },
		MiscFlags: 0,
	};
	let mut t = None;
	dev.CreateTexture2D(&desc, None, Some(&mut t)).unwrap();
	t.unwrap()
}

/// Decode `stream`, present it, and return the decoder's colour + the RGB at each bar's centre.
/// `apply` = false leaves the processor's default colour spaces (the old behaviour).
unsafe fn run(dev: &ID3D11Device, stream: &[u8], apply: bool) -> Option<(Colour, Vec<[u8; 3]>)> {
	let mut dec = Decoder::new(dev, Codec::H264, W, H, 30).ok()?;
	let au = AccessUnit { data: stream.to_vec(), pts_90k: 0, key: true, conceal: false };
	let mut frames = Vec::new();
	for i in 0..8u32 {
		frames.extend(dec.decode(&AccessUnit { pts_90k: i * 3000, ..au.clone() }).ok()?);
		if !frames.is_empty() {
			break;
		}
	}
	let nv12 = frames.into_iter().next()?;
	let (sw, sh) = dec.visible().unwrap_or((W, H));
	let present = Present::new(dev, sw, sh, W, H).ok()?;
	let colour = dec.colour();
	if apply {
		present.set_colour(colour);
	}
	let out = texture(dev, false);
	present.blt(&nv12, &out).ok()?;
	let staging = texture(dev, true);
	let ctx = dev.GetImmediateContext().ok()?;
	ctx.CopyResource(&staging, &out);
	let mut m = D3D11_MAPPED_SUBRESOURCE::default();
	ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m)).ok()?;
	let px = std::slice::from_raw_parts(m.pData as *const u8, (m.RowPitch * H) as usize);
	let bars = (0..4)
		.map(|b| {
			let (x, y) = (b * (W / 4) + W / 8, H / 2);
			let o = (y * m.RowPitch + x * 4) as usize;
			[px[o + 2], px[o + 1], px[o]] // BGRA → RGB
		})
		.collect();
	ctx.Unmap(&staging, 0);
	Some((colour, bars))
}

const BARS: [[u8; 3]; 4] = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255]];

fn near(got: &[[u8; 3]]) -> bool {
	got.iter().zip(BARS).all(|(g, w)| g.iter().zip(w).all(|(a, b)| a.abs_diff(b) <= 10))
}

#[test]
fn bars_decode_to_the_captured_rgb() {
	let Some(dev) = device() else {
		eprintln!("no D3D11 video device: colour-bar harness skipped");
		return;
	};
	for (name, stream, want) in [
		("bt709", &include_bytes!("../../testdata/colour-bt709.h264")[..], Some(Matrix::Bt709)),
		("untagged", &include_bytes!("../../testdata/colour-untagged.h264")[..], None),
		("bt601", &include_bytes!("../../testdata/colour-bt601.h264")[..], Some(Matrix::Bt601)),
	] {
		// SAFETY: D3D11 / MF calls on a device owned by this test.
		let Some((colour, bars)) = (unsafe { run(&dev, stream, true) }) else {
			eprintln!("{name}: no Media Foundation H.264 decode here: skipped");
			continue;
		};
		let old = unsafe { run(&dev, stream, false) }.map(|r| r.1);
		eprintln!("{name}: decoder colour {colour:?}, bars {bars:?} (processor defaults: {old:?})");
		if let Some(m) = want {
			assert_eq!(colour.matrix, m, "{name}");
		}
		assert!(!colour.full_range, "{name}");
		assert!(near(&bars), "{name}: bars {bars:?}, want {BARS:?}");
	}
}
