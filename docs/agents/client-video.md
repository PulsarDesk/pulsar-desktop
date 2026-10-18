# Client video — native renderer, Linux/RK3588, gaming overlay

Part of the pulsar-desktop agent docs — index: [`CLAUDE.md`](../../CLAUDE.md).

## Client video — native renderer everywhere (webview video is GONE)

The old in-webview WebCodecs video path (`h264.ts`) was **removed — too slow**.
Video renders **natively on every platform** via the separate `pulsar-render`
process: Linux rkmpp/EGL zero-copy, Windows MF+D3D11 zero-copy (DXVA), macOS
mpv/VideoToolbox. The egui overlay draws on the video natively. **Only Opus
audio still decodes in the webview** (`src/lib/opus-audio.ts`, fed by
`viewer.rs`'s UDP→WebSocket relay). Don't resurrect a webview video path.

## Linux / RK3588 (Orange Pi 5) renderer — the reality (IMPORTANT)

- **WebKitGTK can't hardware-decode** the stream. Video MUST be native:
  historically `mpv --wid=<XID>` with `--hwdec` → `h264_rkmpp`/`hevc_rkmpp`
  (`--untimed --no-correct-pts --video-sync=desync` are load-bearing — RTP has no
  usable PTS), now the `pulsar-render` rkmpp/EGL path. `native_view/spawn.rs`.
- **The webview can NOT be composited transparently over native video** on this
  GTK3/WebKitGTK stack (wry webview renders opaque black over GtkGLArea; proven
  with a magenta-clear probe). **Do NOT build a "rich webview UI over live
  video" path on Linux.** Moonlight's model: one native renderer, overlay drawn
  natively; the rich menu is a separate screen you toggle to. (A libmpv-render →
  GtkGLArea single-surface exists behind `PULSAR_SINGLE_SURFACE=1`, still blocked
  by the transparency wall.)
- **Control on Linux** = `kbdhook/linux.rs`: grabs local keyboard+mouse via
  **evdev (EVIOCGRAB)**, hotplug-aware. **Leave combo: Ctrl+Shift+Q** (F12
  unreliable on media-mode keyboards).
- **Rendezvous gotcha:** a host serving LAN clients must register with its
  **LAN IP**, not `127.0.0.1`, or the relay hands clients a loopback addr.
- **Auto-update → the appliance runs the AppImage** (`$APPIMAGE` self-replace;
  FUSE required). A `--connect` kiosk checks updates on boot before connecting
  (8 s timeout). Raw `--no-bundle` binary can't self-update. Updater UX in-app:
  `@tauri-apps/plugin-updater` + minisign key in `tauri.conf.json`,
  `src/lib/updater.ts`, `routes/page/UpdateModal.svelte`,
  `scripts/gen-update-manifest.mjs` (consent-based: badge, changelog, progress).

## Gaming overlay (in-session, game mode)

Hidden by default, opened with a key combo. Drawn **natively by pulsar-render
(egui)** on the video — controller-navigable; rich game-only menu
(encoder/decoder, codec, fps, quality/perf, bitrate), **NOT**
file/mic/clipboard. Video may pause while the rich menu screen is open —
acceptable; priority is Moonlight-class latency during play.
