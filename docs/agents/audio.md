# Audio streaming (host → client)

Part of the pulsar-desktop agent docs — index: [`CLAUDE.md`](../../CLAUDE.md).

## Audio streaming (host → client)

A second Opus/RTP stream parallel to video. Implementation lives in
**pulsar-core `audio/`**; this repo spawns it from `src-tauri/src/host/handlers.rs`
(`spawn_loopback_audio`, `run_loopback_capture_tracking`, `opus_rtp_output_layout`,
`audio_command_layout`, `set_host_muted`).

- **Windows default: WASAPI loopback** of the default render endpoint (the
  OBS/Sunshine approach — no virtual-audio-capturer/Stereo Mix needed). Rust
  thread pipes PCM into ffmpeg `pipe:0`; ffmpeg does Opus/RTP. Silence-filled so
  the timeline tracks wall-clock.
- **dshow / Pulse `.monitor` / AVFoundation** on Linux/macOS, or when the user
  names a device (`Config::audio_input` non-empty → `audio_loopback()` false).

Config toggles: `transmit_audio`, `mute_host_audio`. **Game mode forces
`transmit` on but NOT `mute_host`** (`AudioSettings::policy`, unit-tested):
on common codecs the WASAPI loopback tap is post-mute/post-volume, so a capture
opened *into* a muted endpoint latches **pure silence** (verified live, −91 dBFS,
fixed 2026-06-13). Never force-mute the captured endpoint; user-set `mute_host`
is still honored (mid-session muting is safe). Windows mute = endpoint **mute
flag**, NOT volume-0. Un-muted on teardown. Client-side Pulse buffer capped
(~80 ms `-buffer_duration` in `native_view/spawn.rs::spawn_native_audio`).
Mic/voice-call side-channel PCM lives in `src-tauri/src/audio_io.rs`.
