//! In-process audio encoder for the host's stereo capture: Opus/RTP, or uncompressed L16/RTP.
//!
//! The capture used to pipe raw PCM into an ffmpeg (`-f f32le -i pipe:0 … -c:a libopus -f rtp`).
//! ffmpeg's raw-PCM demuxer reads 4096-sample packets (~85 ms at 48 kHz, its
//! `PCM_DEMUX_TARGET_FPS`), so the encoder only saw audio in 85 ms lumps: every packet
//! waited up to 85 ms on the host and they left in bursts of eight or nine. The client then had
//! to buffer ~100 ms to ride the bursts. Measured: max packet gap 86–92 ms on every session;
//! `-avioflags direct`, `-blocksize` and a WAV header (same demuxer packet size) change nothing.
//! With this encoder: 14–23 ms, and the client queue went from 34–134 ms to 12–34 ms.
//!
//! [`RtpAudioSink`] takes the capture's PCM as a [`Write`] sink and sends each packet the moment
//! its samples are in:
//! - [`Codec::Opus`]: libopus with the settings ffmpeg used (restricted-lowdelay, VBR,
//!   complexity 10, 10 ms frames), RFC 7587 RTP on payload type 97 — every receiver unchanged.
//! - [`Codec::L16`]: 16-bit big-endian PCM (RFC 3551 L16) on payload type 98, 5 ms packets
//!   (960 B: under the 1200 B the video packets use) — `AudioQuality::Raw`.
//!
//! Lifetime: the sink belongs to one stream generation (`ProcEpoch`). A re-stream or teardown
//! drains the session's processes and bumps the epoch; the next write then fails, which ends
//! the capture thread exactly as ffmpeg's broken pipe used to.

use pulsar_core::audio::{AudioQuality, ChannelLayout, L16_PAYLOAD_TYPE, OPUS_PAYLOAD_TYPE};
use std::io::{self, Write};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::Ordering;

/// Sample rate (`pulsar_core::audio::SAMPLE_RATE`).
const RATE: u32 = 48_000;
const CHANNELS: usize = 2;
const RTP_HEADER: usize = 12;
/// Largest payload we build (an L16 packet is 960 B; Opus at 320 kb/s × 10 ms ≈ 400 B).
const MAX_PAYLOAD: usize = 1500;

/// What the sink sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Codec {
	/// Opus at this bitrate (bits/s), 10 ms frames.
	Opus { bitrate: u32 },
	/// Uncompressed 16-bit PCM, 5 ms packets.
	L16,
}

impl Codec {
	/// The stereo codec for a requested quality.
	pub(super) fn for_quality(q: AudioQuality) -> Self {
		match q {
			AudioQuality::Raw => Codec::L16,
			q => Codec::Opus { bitrate: pulsar_core::audio::opus_bitrate_for(ChannelLayout::Stereo, q) },
		}
	}

	/// Frames (per channel) in one packet.
	fn frame(self) -> usize {
		match self {
			Codec::Opus { .. } => RATE as usize / 100,
			Codec::L16 => RATE as usize / 200,
		}
	}
}

/// Ten unpredictable bytes from std (`RandomState` is seeded per process + instance).
fn random_bytes() -> [u8; 10] {
	use std::hash::{BuildHasher, Hasher};
	let word = || {
		let mut h = std::collections::hash_map::RandomState::new().build_hasher();
		h.write_u128(
			std::time::SystemTime::now()
				.duration_since(std::time::UNIX_EPOCH)
				.map(|d| d.as_nanos())
				.unwrap_or(0),
		);
		h.finish().to_le_bytes()
	};
	let (a, b) = (word(), word());
	let mut out = [0u8; 10];
	out[..8].copy_from_slice(&a);
	out[8..].copy_from_slice(&b[..2]);
	out
}

/// Where an `rtp://host:port` destination (the form the ffmpeg path takes) points.
pub(super) fn rtp_dest(dest: &str) -> Option<SocketAddr> {
	dest.strip_prefix("rtp://")?.split('?').next()?.parse().ok()
}

/// 48 kHz stereo f32le PCM in, one RTP datagram per packet out.
pub(super) struct RtpAudioSink {
	/// `None` = L16.
	enc: Option<opus::Encoder>,
	payload_type: u8,
	/// Frames per packet.
	frame: usize,
	sock: UdpSocket,
	dest: SocketAddr,
	/// PCM bytes not yet making up a whole packet.
	pending: Vec<u8>,
	samples: Vec<f32>,
	packet: Vec<u8>,
	seq: u16,
	ts: u32,
	ssrc: u32,
	marker: bool,
	epoch: crate::process::ProcEpoch,
	born: u64,
	pub(super) packets: u64,
}

impl RtpAudioSink {
	/// A sink for the stream generation `epoch` is at right now.
	pub(super) fn new(dest: SocketAddr, codec: Codec, epoch: crate::process::ProcEpoch) -> Result<Self, String> {
		let enc = match codec {
			Codec::Opus { bitrate } => {
				let mut enc = opus::Encoder::new(RATE, opus::Channels::Stereo, opus::Application::LowDelay)
					.map_err(|e| format!("opus encoder: {e}"))?;
				enc.set_bitrate(opus::Bitrate::Bits(bitrate as i32))
					.map_err(|e| format!("opus bitrate: {e}"))?;
				let _ = enc.set_vbr(true);
				let _ = enc.set_complexity(10);
				Some(enc)
			}
			Codec::L16 => None,
		};
		let bind: SocketAddr = if dest.is_ipv6() {
			"[::]:0".parse().unwrap()
		} else {
			"0.0.0.0:0".parse().unwrap()
		};
		let sock = UdpSocket::bind(bind).map_err(|e| format!("audio rtp socket: {e}"))?;
		// Random starting sequence, timestamp and SSRC, as RFC 3550 asks (and ffmpeg did).
		let r = random_bytes();
		let born = epoch.load(Ordering::SeqCst);
		let frame = codec.frame();
		Ok(Self {
			payload_type: if enc.is_some() { OPUS_PAYLOAD_TYPE } else { L16_PAYLOAD_TYPE },
			enc,
			frame,
			sock,
			dest,
			pending: Vec::with_capacity(frame * CHANNELS * 4 * 2),
			samples: vec![0.0; frame * CHANNELS],
			packet: vec![0; RTP_HEADER + MAX_PAYLOAD],
			seq: u16::from_be_bytes([r[0], r[1]]),
			ts: u32::from_be_bytes([r[2], r[3], r[4], r[5]]),
			ssrc: u32::from_be_bytes([r[6], r[7], r[8], r[9]]),
			marker: true,
			epoch,
			born,
			packets: 0,
		})
	}

	fn send_packet(&mut self) -> io::Result<()> {
		let n = match self.enc.as_mut() {
			Some(enc) => enc
				.encode_float(&self.samples, &mut self.packet[RTP_HEADER..])
				.map_err(|e| io::Error::other(format!("opus encode: {e}")))?,
			None => {
				for (out, &x) in self.packet[RTP_HEADER..].chunks_exact_mut(2).zip(&self.samples) {
					let v = (x.clamp(-1.0, 1.0) * 32767.0).round() as i16;
					out.copy_from_slice(&v.to_be_bytes());
				}
				self.samples.len() * 2
			}
		};
		let h = &mut self.packet[..RTP_HEADER];
		h[0] = 0x80; // V=2, no padding / extension / CSRCs
		h[1] = self.payload_type | if self.marker { 0x80 } else { 0 };
		h[2..4].copy_from_slice(&self.seq.to_be_bytes());
		h[4..8].copy_from_slice(&self.ts.to_be_bytes());
		h[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
		// A send error (nobody listening yet, a transient ICMP) costs one packet; the stream
		// goes on, as it did with ffmpeg.
		let _ = self.sock.send_to(&self.packet[..RTP_HEADER + n], self.dest);
		self.marker = false;
		self.seq = self.seq.wrapping_add(1);
		self.ts = self.ts.wrapping_add(self.frame as u32);
		self.packets += 1;
		Ok(())
	}
}

impl Write for RtpAudioSink {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		if self.epoch.load(Ordering::SeqCst) != self.born {
			// The session re-streamed or ended: stop the capture (its next write fails).
			return Err(io::Error::new(io::ErrorKind::BrokenPipe, "audio stream retired"));
		}
		self.pending.extend_from_slice(buf);
		let packet_bytes = self.frame * CHANNELS * 4;
		let mut off = 0;
		while self.pending.len() - off >= packet_bytes {
			for (dst, b) in self
				.samples
				.iter_mut()
				.zip(self.pending[off..off + packet_bytes].chunks_exact(4))
			{
				*dst = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
			}
			off += packet_bytes;
			self.send_packet()?;
		}
		self.pending.drain(..off);
		Ok(buf.len())
	}

	fn flush(&mut self) -> io::Result<()> {
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::sync::atomic::AtomicU64;
	use std::sync::Arc;

	fn tone(frames: usize) -> Vec<u8> {
		let mut out = Vec::with_capacity(frames * CHANNELS * 4);
		for i in 0..frames {
			let v = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / RATE as f32).sin() * 0.5;
			for _ in 0..CHANNELS {
				out.extend_from_slice(&v.to_le_bytes());
			}
		}
		out
	}

	fn receiver() -> UdpSocket {
		let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
		rx.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
		rx
	}

	#[test]
	fn rtp_dest_parses_the_ffmpeg_form() {
		assert_eq!(rtp_dest("rtp://127.0.0.1:5004"), Some("127.0.0.1:5004".parse().unwrap()));
		assert_eq!(rtp_dest("rtp://10.0.0.2:6000?pkt_size=1200"), Some("10.0.0.2:6000".parse().unwrap()));
		assert_eq!(rtp_dest("udp://127.0.0.1:5004"), None);
	}

	#[test]
	fn quality_picks_the_codec() {
		assert_eq!(Codec::for_quality(AudioQuality::Standard), Codec::Opus { bitrate: 128_000 });
		assert_eq!(Codec::for_quality(AudioQuality::High), Codec::Opus { bitrate: 320_000 });
		assert_eq!(Codec::for_quality(AudioQuality::Raw), Codec::L16);
	}

	#[test]
	fn every_10ms_of_pcm_leaves_as_one_rtp_opus_packet() {
		let rx = receiver();
		let epoch: crate::process::ProcEpoch = Arc::new(AtomicU64::new(7));
		let mut sink =
			RtpAudioSink::new(rx.local_addr().unwrap(), Codec::Opus { bitrate: 128_000 }, epoch.clone()).unwrap();
		// Odd-sized writes, like WASAPI packets that don't line up with 10 ms.
		let frame = RATE as usize / 100;
		let pcm = tone(frame * 5);
		for chunk in pcm.chunks(1000) {
			sink.write_all(chunk).unwrap();
		}
		assert_eq!(sink.packets, 5);
		let mut dec = opus::Decoder::new(RATE, opus::Channels::Stereo).unwrap();
		let mut buf = [0u8; 2048];
		let mut pcm_out = vec![0f32; frame * CHANNELS];
		let mut last: Option<(u16, u32)> = None;
		for i in 0..5 {
			let n = rx.recv(&mut buf).unwrap();
			let (seq, payload) = crate::native_audio::rtp_opus(&buf[..n]).expect("valid RTP/Opus");
			assert_eq!(buf[1] & 0x7f, OPUS_PAYLOAD_TYPE);
			assert_eq!(buf[1] & 0x80 != 0, i == 0, "marker on the first packet only");
			let ts = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
			if let Some((s, t)) = last {
				assert_eq!(seq, s.wrapping_add(1));
				assert_eq!(ts, t.wrapping_add(frame as u32));
			}
			last = Some((seq, ts));
			assert_eq!(dec.decode_float(payload, &mut pcm_out, false).unwrap(), frame);
		}
		// A re-stream / teardown (epoch bump) stops the capture through its next write.
		epoch.fetch_add(1, Ordering::SeqCst);
		assert_eq!(sink.write(&pcm[..64]).unwrap_err().kind(), io::ErrorKind::BrokenPipe);
	}

	#[test]
	fn raw_sends_5ms_l16_packets_that_carry_the_samples() {
		let rx = receiver();
		let epoch: crate::process::ProcEpoch = Arc::new(AtomicU64::new(0));
		let mut sink = RtpAudioSink::new(rx.local_addr().unwrap(), Codec::L16, epoch).unwrap();
		let frame = RATE as usize / 200;
		let pcm = tone(frame * 3);
		sink.write_all(&pcm).unwrap();
		assert_eq!(sink.packets, 3);
		let mut buf = [0u8; 2048];
		let n = rx.recv(&mut buf).unwrap();
		assert_eq!(n, RTP_HEADER + frame * CHANNELS * 2, "960 B of PCM per packet");
		assert_eq!(buf[1] & 0x7f, L16_PAYLOAD_TYPE);
		// Sample 10 (left) comes back within one 16-bit step.
		let want = f32::from_le_bytes(pcm[80..84].try_into().unwrap());
		let off = RTP_HEADER + 10 * CHANNELS * 2;
		let got = i16::from_be_bytes([buf[off], buf[off + 1]]) as f32 / 32767.0;
		assert!((got - want).abs() < 1.0 / 32767.0 + 1e-6, "{got} vs {want}");
	}
}
