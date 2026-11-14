//! The NVENC SDK encoder (`pulsar_capture::NvencCuda`) as the Wayland capture's raw-frame
//! encoder (`pulsar_core::capture::FrameEncoder`): the portal's frames, converted to NV12 by
//! GStreamer, are encoded here and sent as RTP to the session's video port — so a Wayland host
//! on an NVIDIA GPU heals a client-reported loss with reference-frame invalidation, like the
//! Windows host and the X11 (libav) host. When the encoder cannot be built, pulsar-core runs the
//! gst encoder pipeline on the same portal session instead. Untested on Linux hardware.

#![cfg(target_os = "linux")]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

use pulsar_capture::{Codec, Nv12Frame, NvencCuda, NvencCudaParams};
use pulsar_core::capture::{FrameEncoder, FrameEncoderFactory, RawFrame};
use pulsar_core::pipeline::gst::GstEncoder;

/// Use the SDK encoder for this Wayland stream? The gst pick is NVIDIA's own plugin, or software
/// x264 while the client left the encoder to us / asked for NVENC (an NVIDIA host whose gst
/// NVENC plugin cannot open the driver — the AppImage's 1.20 one on a 5xx driver — then still
/// gets hardware encoding). `sdk_ready` = the NVENC SDK probe found NVENC on CUDA; `codecs` =
/// what it encodes; `env` = `PULSAR_NVENC_SDK`.
pub(super) fn wanted(sdk_ready: bool, codecs: &[Codec], genc: GstEncoder, enc_pref: &str, codec: Codec, env: Option<&str>) -> bool {
	let enabled = !matches!(env.map(str::trim), Some("0") | Some("false") | Some("off"));
	let family = match genc {
		GstEncoder::Nv => true,
		GstEncoder::X264 => matches!(enc_pref, "" | "auto" | "nvenc"),
		_ => false,
	};
	enabled && sdk_ready && family && codecs.contains(&codec)
}

/// The factory pulsar-core calls on the first frame (and on a size change) with the cast's size.
/// `base` carries everything but the size; the live bitrate survives a rebuild; the RTP clock
/// runs from the factory's creation so it stays monotonic across rebuilds.
pub(super) fn factory(base: NvencCudaParams) -> FrameEncoderFactory {
	let kbps = Arc::new(AtomicU32::new(base.bitrate_kbps));
	let t0 = Instant::now();
	Box::new(move |w, h| {
		let p = NvencCudaParams {
			width: w & !1,
			height: h & !1,
			bitrate_kbps: kbps.load(Ordering::Relaxed),
			..base.clone()
		};
		let enc = NvencCuda::new(&p)?;
		Ok(Box::new(SdkFrames { enc, t0, kbps: kbps.clone() }) as Box<dyn FrameEncoder>)
	})
}

struct SdkFrames {
	enc: NvencCuda,
	t0: Instant,
	kbps: Arc<AtomicU32>,
}

impl FrameEncoder for SdkFrames {
	fn encode(&mut self, f: &RawFrame<'_>) -> Result<(), String> {
		let frame = Nv12Frame { y: f.y, y_pitch: f.y_stride, uv: f.uv, uv_pitch: f.uv_stride };
		self.enc.encode(&frame, super::libav::rtp_ts_90k(self.t0.elapsed())).map(|_| ())
	}

	fn set_bitrate(&mut self, kbps: u32) -> bool {
		match self.enc.set_bitrate(kbps) {
			Ok(()) => {
				self.kbps.store(kbps, Ordering::Relaxed);
				true
			}
			Err(e) => {
				tracing::warn!(kbps, "NVENC SDK (Wayland) bitrate change refused: {e}");
				false
			}
		}
	}

	fn request_keyframe(&mut self) {
		self.enc.request_idr();
	}

	fn request_rfi(&mut self, first: u16, last: u16) -> bool {
		match self.enc.invalidate_lost(first, last) {
			Ok(n) => {
				tracing::debug!(first, last, frames = n, "RFI (Wayland): references invalidated");
				true
			}
			Err(why) => {
				tracing::debug!(first, last, "RFI (Wayland) declined: {why}");
				false
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn sdk_only_for_nvidia_or_an_open_software_pick() {
		let all = [Codec::H264, Codec::H265];
		assert!(wanted(true, &all, GstEncoder::Nv, "nvenc", Codec::H265, None));
		assert!(wanted(true, &all, GstEncoder::X264, "", Codec::H264, None));
		assert!(wanted(true, &all, GstEncoder::X264, "auto", Codec::H264, None));
		// The user asked for software, or another hardware family: keep it.
		assert!(!wanted(true, &all, GstEncoder::X264, "software", Codec::H264, None));
		assert!(!wanted(true, &all, GstEncoder::Vaapi, "", Codec::H264, None));
		// No probe answer / the codec is not encodable / disabled.
		assert!(!wanted(false, &all, GstEncoder::Nv, "nvenc", Codec::H264, None));
		assert!(!wanted(true, &all, GstEncoder::Nv, "nvenc", Codec::Av1, None));
		assert!(!wanted(true, &all, GstEncoder::Nv, "nvenc", Codec::H264, Some("0")));
	}
}
