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
  its PID instead. Also used while a host-silent redirect to the virtual sink is
  armed: the host's volume slider is then the sink's, and that sink's endpoint
  loopback neither ignored it (−12 dB at 0.41 → 0.10) nor ran in real time
  (46.8k–56.4k samples/s, measured) — game-mode crackle.
- **Virtual DualSense plugged → default-endpoint loopback**
  (`host/handlers/ds5_route.rs`, logic in pulsar-core `audio/endpoint_route.rs`).
  Process loopback is endpoint-agnostic: what a game renders to the pad's own
  endpoint ("Speakers (N- DualSense Wireless Controller)", ch 1/2 speaker, 3/4
  haptics) was played in the client's audio (verified 2026-11-11). While a usbip
  pad is up on this host (`ds5::host::audio_pad_plugged`), the stream comes from
  the default render endpoint's loopback instead, divided by its master
  attenuation (`GetMasterVolumeLevel` dB — not the slider scalar, which is an
  audio taper; 1.0 for an endpoint with hardware volume), polled every 20 ms,
  glided over 10 ms, clamped to ±1. Process loopback keeps running and stays
  the source when: the endpoint is muted or at 0 (nothing to recover; the pad
  audio leaks then, accepted), the default IS a DualSense endpoint, the endpoint
  capture fails (retried with 2 s…60 s backoff) or isn't 48 kHz, or an
  Include stream (launched game / picked window) runs next to another
  in-process stream on the host (same-host co-op keeps per-app isolation). A
  lone Include stream does switch: it then carries the whole system mix, not
  just the game. Both captures write one encoder through a mux — only the
  active source passes, switched once the endpoint capture delivers PCM, so a
  switch never doubles; the old source fades out over 5 ms and the new one in
  (`SwitchFader`), so it doesn't click. With a **host-silent redirect armed**
  (game mode's usual case) the default is the virtual sink, whose slider is the
  host's volume (compensated the same way) and whose loopback isn't real-time
  (above): it is paced — no silence fill, `DriftResampler` jitter buffer (30 ms
  target, +5 ms per underrun up to 60 ms, back down after 30 s calm; ratio =
  measured source rate ±3 % × a ±0.5 % fill trim; crossfaded drop past target +
  60 ms; faded silence on underrun) drained by a wall-clock pacer in 5 ms
  blocks. Stats every 10 s: `audio: virtual-sink jitter buffer`. A default-device change restarts the
  endpoint capture on the new id (process loopback meanwhile). The endpoint is
  never opened while muted (latched silence, below). Only the in-process
  encoder path does this; the ffmpeg fallback and surround keep their paths.
- **Encoded in-process** (`host/handlers/opus_rtp.rs`): libopus, 10 ms frames,
  128 kb/s, RTP PT 97 to the same destination; stops when the session's
  `ProcEpoch` moves (re-stream / teardown). ffmpeg reading the PCM from a pipe
  held it in 85 ms lumps (its raw-PCM demuxer reads 4096-sample packets; no
  flag changes that) — ~85 ms extra delay and bursty packets. ffmpeg remains
  the fallback if the encoder can't start.
- **Fallback: WASAPI loopback** of the default render endpoint (the
  OBS/Sunshine approach — no virtual-audio-capturer/Stereo Mix needed). Used for
  a surround request or when process loopback fails to start. Rust thread pipes
  PCM into ffmpeg `pipe:0`; ffmpeg does Opus/RTP (with the 85 ms lumps above). Silence-filled so the timeline tracks
  wall-clock.
- **Audio quality** (`StreamReq.audio_quality`, `AudioQuality`): Standard =
  Opus 128 kb/s, High = Opus 320 kb/s, Raw = uncompressed L16 (RTP PT 98, 5 ms
  / 960 B packets, ~1.6 Mb/s) from the in-process encoder; hosts that can send
  Raw advertise `audio_raw`, ffmpeg paths send High instead. The client's
  "Otomatik" (`play.rs` `auto_audio_quality`) picks Raw on a LAN the probe
  measured at ≥15 Mbit/s, High at ≥3 Mbit/s, else Standard; the pick is
  Settings → Görüntü / the session menu's Ses view (a re-stream). The host's
  audio forwarder passes PT 97 and 98 only.
- **dshow / Pulse `.monitor` / AVFoundation** on Linux/macOS, or when the user
  names a device (`Config::audio_input` non-empty → `audio_loopback()` false).

## Client playback (every desktop platform)

Native and in-process, Moonlight's model: `src-tauri/src/native_audio.rs`.

- `viewer.rs` takes the audio RTP on a UDP port and broadcasts it in-process. No
  WebSocket; the webview never sees audio.
- The player decodes with **libopus** (`opus` crate → `opusic-sys`, built from
  source with CMake like SDL3). It conceals up to 5 lost packets (PLC) and
  restarts the decoder on a longer gap or a new stream.
- Output is an **SDL3** audio stream (10 ms device buffer requested). The queue
  is a sawtooth; its LOW point is held at a margin (`PULSAR_AUDIO_TARGETMS`,
  default 15 ms; +10 ms per dry queue, eases back) by slowly nudging the rate
  (max ±2 %). Prebuffers before playing; flushes far past the target. The device
  opens on the first packet. `audio: player stats` every 5 s (in_hz, dry,
  flushes, gap_max_ms, q_ms, ratio).
- The old ±3 %-per-packet steering on the post-push level sat at +3 % (1030 Hz
  for a 1 kHz tone) with 114 dropouts / 478 clicks per 30 s — the crackle.
  Check audio with the tone test: host plays a 1 kHz WAV, the client's Pulsar
  output is recorded with process loopback (Include, its PID) and analysed.
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
