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

## Linux desktop GPUs — zero-copy needs the decoder on the DISPLAY GPU

`crates/pulsar-render/src/gpu.rs` asks EGL (`EGL_EXT_device_query`) which render node
the renderer's GL runs on; `decode.rs` then ranks the hwaccels display-GPU-first:
VAAPI on that node → CUDA → VAAPI on any other node → Vulkan → DRM (each still
validated on the canned keyframe). How frames reach GL (`present path …` in the log):

| Decoder | Display GPU | Path |
| --- | --- | --- |
| VAAPI (Mesa AMD/Intel, nouveau, nvidia-vaapi-driver) | same GPU | `DmabufLayers` — two-layer R8+GR88 dmabuf, two TEXTURE_2D EGLImages |
| CUDA | NVIDIA | `CudaInterop` — GPU-side copy into GL textures (libcuda dlopen'ed) |
| any | another GPU / unknown | `Nv12Upload` — readback (the old path) |

- Hybrid laptops (iGPU + NVIDIA) decode on the iGPU now; under PRIME offload the GL is on
  NVIDIA and CUDA interop wins. A dmabuf can't cross GPUs (tiled layouts), and interop
  only exists inside an NVIDIA GL — so "use the stronger NVDEC" means a copy there.
- A failed import/interop sets `ZC_FAILED` → readback for that decoder (no black screen).
- `PULSAR_ZEROCOPY=0` keeps the order but forces readback (A/B, driver bugs).
- Tier-0 SoC decoders (rkmpp) skip the display-GPU detection entirely (Mali EGL is fragile).
- Measured 2026-10-19 (debug build, local 1080p60 NVENC→RTP, Ryzen/Vega + RTX 3050 Ti laptop),
  renderer CPU: AMD zero-copy 13 % vs readback 31 %; NVIDIA (PRIME offload) interop 15 % vs
  readback 22 %; all 60 fps, colours checked. Not run: Intel, nvidia-vaapi-driver, 10-bit.

## Gaming overlay (in-session, game mode)

Hidden by default, opened with a key combo. Drawn **natively by pulsar-render
(egui)** on the video — controller-navigable; rich game-only menu
(encoder/decoder, codec, fps, quality/perf, bitrate), **NOT**
file/mic/clipboard. Video may pause while the rich menu screen is open —
acceptable; priority is Moonlight-class latency during play.
