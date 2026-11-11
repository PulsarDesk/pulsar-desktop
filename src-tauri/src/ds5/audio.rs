//! Audio helpers shared by the local bridge and the streaming host: the pad-speaker Opus
//! encoder (the pad's BT format) and OS volume/mute merged into state reports.

use std::collections::VecDeque;
use std::io;

use pulsar_core::input::ds5::bt_audio::SPEAKER_OPUS_LEN;
use pulsar_core::input::ds5::report::{flag, SetState};
use pulsar_core::input::ds5::speaker::{SpeakerFramer, OPUS_BITRATE, OUT_FRAMES};

/// Speaker blocks quieter than this (peak) are not sent: haptics-only reports cost less
/// BT airtime, and the pad's speaker stays silent anyway.
pub const SILENCE: f32 = 1.0 / 2048.0;
/// Keep sending speaker audio this many blocks after it went quiet (no clipped tails).
pub const HANGOVER: u32 = 20;

/// Pad speaker Opus encoder in the format the pad decodes: 48 kHz stereo CELT, 10 ms,
/// CBR 160 kbit/s = 200 bytes per frame.
pub struct SpeakerEncoder {
	framer: SpeakerFramer,
	enc: opus::Encoder,
	quiet: u32,
}

impl SpeakerEncoder {
	pub fn new() -> io::Result<Self> {
		let map = |e: opus::Error| io::Error::other(format!("opus: {e}"));
		let mut enc =
			opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::LowDelay)
				.map_err(map)?;
		enc.set_bitrate(opus::Bitrate::Bits(OPUS_BITRATE))
			.map_err(map)?;
		enc.set_vbr(false).map_err(map)?;
		Ok(Self {
			framer: SpeakerFramer::default(),
			enc,
			quiet: HANGOVER,
		})
	}

	/// Feed 48 kHz stereo PCM; returns one Opus frame (or `None` for silence) per
	/// completed 10.667 ms block.
	pub fn push(&mut self, pcm: &[f32], out: &mut VecDeque<Option<Vec<u8>>>) {
		let mut blocks = Vec::new();
		self.framer.push(pcm, &mut blocks);
		for b in blocks {
			debug_assert_eq!(b.len(), OUT_FRAMES * 2);
			let peak = b.iter().fold(0f32, |m, v| m.max(v.abs()));
			self.quiet = if peak < SILENCE {
				self.quiet.saturating_add(1)
			} else {
				0
			};
			if self.quiet > HANGOVER {
				out.push_back(None);
				continue;
			}
			let mut buf = [0u8; SPEAKER_OPUS_LEN];
			match self.enc.encode_float(&b, &mut buf) {
				Ok(n) => out.push_back(Some(buf[..n].to_vec())),
				Err(e) => {
					tracing::debug!("ds5 bridge: opus encode failed: {e}");
					out.push_back(None);
				}
			}
		}
	}
}

/// Volume and mute from the host OS (UAC feature unit), merged into every state report.
#[derive(Clone, Copy, Default)]
pub struct Volume {
	pub known: bool,
	pub speaker: u8,
	pub mic: u8,
	pub speaker_mute: bool,
	pub mic_mute: bool,
}

impl Volume {
	/// Apply onto a state report the game wrote, unless the game set these fields itself.
	pub fn merge(&self, s: &mut SetState) {
		if !self.known {
			return;
		}
		let b = &mut s.0;
		if b[0] & flag::B0_SPEAKER_VOLUME == 0 {
			b[0] |= flag::B0_SPEAKER_VOLUME | flag::B0_HEADPHONE_VOLUME;
			b[4] = self.speaker; // headphones
			b[5] = self.speaker; // speaker
		}
		if b[0] & flag::B0_MIC_VOLUME == 0 {
			b[0] |= flag::B0_MIC_VOLUME;
			b[6] = self.mic;
		}
		if b[1] & flag::B1_AUDIO_MUTE == 0 {
			b[1] |= flag::B1_AUDIO_MUTE;
			// byte 9: bit4 mic mute, bit5 speaker mute, bit6 headphone mute
			b[9] = (b[9] & !0x70)
				| if self.mic_mute { 0x10 } else { 0 }
				| if self.speaker_mute { 0x60 } else { 0 };
		}
	}
}

impl Volume {
	/// From the virtual pad's UAC state (whole dB).
	pub fn from_db(speaker_db: i16, mic_db: i16, speaker_mute: bool, mic_mute: bool) -> Self {
		Self {
			known: true,
			speaker: (100 + speaker_db).clamp(0, 100) as u8,
			mic: mic_db.clamp(0, 64) as u8,
			speaker_mute,
			mic_mute,
		}
	}
}
