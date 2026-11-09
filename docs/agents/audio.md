# Audio streaming (host → client)

Part of the pulsar-desktop agent docs — index: [`CLAUDE.md`](../../CLAUDE.md).

## Audio streaming (host → client)

A second Opus/RTP stream parallel to video. Implementation lives in
**pulsar-core `audio/`**; this repo spawns it from `src-tauri/src/host/handlers.rs`
(`spawn_loopback_audio`, `run_loopback_capture_tracking`, `opus_rtp_output_layout`,
`audio_command_layout`, `set_host_muted`).

- **Windows default: process loopback, Exclude mode** — every process except
  Pulsar's own tree (`spawn_process_loopback_audio` with
  `ProcessLoopbackMode::Exclude` and our PID). It taps the app streams *before*
  the endpoint volume and mute, so a host muted or at 0 % still streams full-level
  audio. Measured on the Realtek host muted at 0 %: endpoint loopback = pure
  silence, process loopback = a test tone at −14 dBFS. Stereo only (fixed 48 kHz
  f32 format); Win10 20H1+. A launched game / picked window uses Include mode on
  its PID instead.
- **Fallback: WASAPI loopback** of the default render endpoint (the
  OBS/Sunshine approach — no virtual-audio-capturer/Stereo Mix needed). Used for
  a surround request, while a host-silent redirect to the virtual sink is armed,
  or when process loopback fails to start. Rust thread pipes PCM into ffmpeg
  `pipe:0`; ffmpeg does Opus/RTP. Silence-filled so the timeline tracks
  wall-clock.
- **dshow / Pulse `.monitor` / AVFoundation** on Linux/macOS, or when the user
  names a device (`Config::audio_input` non-empty → `audio_loopback()` false).

## Client playback (every desktop platform)

Native and in-process, Moonlight's model: `src-tauri/src/native_audio.rs`.

- `viewer.rs` takes the audio RTP on a UDP port and broadcasts it in-process. No
  WebSocket; the webview never sees audio.
- The player decodes with **libopus** (`opus` crate → `opusic-sys`, built from
  source with CMake like SDL3). It conceals up to 5 lost packets (PLC) and
  restarts the decoder on a longer gap or a new stream.
- Output is an **SDL3** audio stream. The queue is held near
  `PULSAR_AUDIO_TARGETMS` (default 30 ms) by nudging the playback rate ±3 %, and
  flushed once above 120 ms. The device opens on the first packet.
- Stereo only: `opus` has no multistream decoder, and the client always asks for
  stereo.
- This replaced the webview path (WebCodecs + WebAudio over a loopback
  WebSocket, Windows/macOS) and Linux's ffmpeg → SDL3 subprocess.
- Live check: `cargo test -p pulsar-tauri native_audio -- --ignored` plays a
  quiet tone through the player and captures it back with process loopback.

## Host policy

Config toggles: `transmit_audio`, `mute_host_audio`. **Game mode forces
`transmit` on but NOT `mute_host`** (`AudioSettings::policy`, unit-tested). This
caveat applies to the endpoint loopback fallback only: on common codecs the WASAPI loopback tap is post-mute/post-volume, so a capture
opened *into* a muted endpoint latches **pure silence** (verified live, −91 dBFS,
fixed 2026-06-13). Never force-mute the captured endpoint; user-set `mute_host`
is still honored (mid-session muting is safe). Windows mute = endpoint **mute
flag**, NOT volume-0. Un-muted on teardown.
Mic/voice-call side-channel PCM lives in `src-tauri/src/audio_io.rs`.
