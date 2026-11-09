//! Shared streaming types + RTP depacketizer for the native-decode backends (Windows Media
//! Foundation, macOS VideoToolbox, Linux libavcodec). Pure Rust (no platform deps) so it
//! compiles + unit-tests anywhere and every backend consumes the IDENTICAL depacketization
//! (codec parity). Linux reads the RTP from a Unix datagram socket (`rtp::recv_loop_unix`);
//! `PULSAR_LINUX_DEMUX=ffmpeg` keeps the old libavformat RTP/SDP demux there instead.

#![allow(dead_code)]

pub mod recovery; // loss hold + AU backlog bound (Linux renderer policy, pure)
pub mod rtp; // RTP receive + depacketize (→ Annex-B / OBU access units)

/// Video codec of the stream (from the SDP rtpmap). Drives depacketization + the decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
	H264,
	H265,
	Av1,
}

/// One complete access unit ready for the decoder: an **Annex-B** elementary-stream chunk
/// (H.264/HEVC NALs with `00 00 00 01` start codes) or the raw **OBU stream** (AV1), the
/// presentation timestamp in 90 kHz RTP ticks, and whether it begins an IDR/keyframe.
#[derive(Clone, Debug)]
pub struct AccessUnit {
	pub data: Vec<u8>,
	pub pts_90k: u32,
	pub key: bool,
}

/// Parse the host SDP (`pulsar-<port>.sdp`, written by `spawn.rs::write_sdp`) for the UDP
/// media port + the codec. Falls back to H.264 on an unrecognized rtpmap.
pub fn parse_sdp(path: &str) -> std::io::Result<(u16, Codec)> {
	let text = std::fs::read_to_string(path)?;
	let mut port = 0u16;
	let mut codec = Codec::H264;
	for line in text.lines() {
		if let Some(rest) = line.strip_prefix("m=video ") {
			if let Some(tok) = rest.split_whitespace().next() {
				port = tok.parse().unwrap_or(0);
			}
		} else if let Some(rest) = line.strip_prefix("a=rtpmap:96 ") {
			let up = rest.to_ascii_uppercase();
			codec = if up.starts_with("H265") || up.starts_with("HEVC") {
				Codec::H265
			} else if up.starts_with("AV1") {
				Codec::Av1
			} else {
				Codec::H264
			};
		}
	}
	Ok((port, codec))
}

/// The abstract Unix datagram socket the app forwards video RTP to (`a=x-pulsar-unix:<name>`,
/// written by `spawn.rs::write_sdp` on Linux), if the SDP names one.
pub fn sdp_unix_name(path: &str) -> Option<String> {
	let text = std::fs::read_to_string(path).ok()?;
	text.lines()
		.find_map(|l| l.trim().strip_prefix("a=x-pulsar-unix:"))
		.map(|n| n.trim().to_string())
		.filter(|n| !n.is_empty())
}
