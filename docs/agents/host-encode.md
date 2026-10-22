# Host capture/encode + capability probe

Part of the pulsar-desktop agent docs — index: [`CLAUDE.md`](../../CLAUDE.md).

## Host capture/encode paths

- **Windows, NVENC present** — `crates/pulsar-capture`: DXGI Desktop Duplication
  (or WGC) → NVENC SDK directly → hand-rolled RTP packetizer. No ffmpeg in the hot
  path; falls back to ffmpeg if init fails.
- **Linux X11** — the ffmpeg **libraries in-process** (`src-tauri/src/host/libav.rs`:
  x11grab → libavfilter → libavcodec x264/x265/SVT-AV1/NVENC → libavformat RTP; live
  bitrate, forced IDR on request, short-GOP recovery; `PULSAR_LIBAV_HOST=0` disables).
  Falls back to the ffmpeg CLI below when it cannot start or for VA-API/Vulkan/HDR/4:4:4.
- **Windows (non-NVENC) / macOS** (and the Linux fallback) — ffmpeg CLI (`ddagrab`/
  `gdigrab`/`avfoundation`/`x11grab` → HW encode or libx264 → RTP). Arg builders are pure
  functions in pulsar-core `pipeline/` (unit-tested); spawning is here (`process.rs`, `host/`).
- **Wayland (KDE/GNOME)** — pulsar-core `capture` (Linux-only module): XDG
  ScreenCast portal (`ashpd`) → PipeWire → **in-process** GStreamer
  (`pipewiresrc ! queue leaky=downstream ! x264enc(zerolatency) name=venc ! rtph264pay`;
  bitrate/keyframe/short-GOP applied live, no restart).
  `x11grab` of rootless Xwayland is **always black**, so this is required. Restore
  token skips the share dialog after first connect. Software x264 (no gst HW
  plugins); leaky queue bounds latency. **Input injection is uinput**
  (`DesktopInput`), NOT the RemoteDesktop portal (its `Start` hangs on this KDE);
  needs the user in the `input` group.
  **AppImage:** the binary links libgstreamer, so linuxdeploy bundles Ubuntu 22.04's
  1.20 core — a distro's newer plugins can't load into it (no elements → black screen on
  every non-Debian Wayland host). `release.yml` therefore bundles the pipeline's plugins
  (`bundleMediaFramework` + curated `GSTREAMER_PLUGINS_DIR`) and `gst-launch`/
  `gst-inspect`; `pipewiresrc` uses the SYSTEM libpipewire (linuxdeploy excludes it — a
  bundled 0.3.48 client hangs without its Debian-path SPA plugins). `main.rs::appimage_env`
  gives the bundled gst its own registry, and `process.rs::bundled_gst_tool`
  probes the Wayland encoders with that same stack. System gst tools get the bundled env
  stripped (`pulsar_core::capture::use_system_gst_env`). **libva comes from the HOST**
  when it has one: 22.04's 2.14 loader can't open a newer Mesa driver
  (`__vaDriverInit_1_24`), so `main.rs::appimage_host_libva` re-execs once with the host's
  libva in `LD_PRELOAD` (bundled copy stays as the no-libva fallback) — that is what makes
  the bundled `vaapih264enc` and pulsar-render's VA-API decode work. libjack is bundled by
  hand (on linuxdeploy's excludelist, but libavdevice/libmpv need it to load at all).
  An encoder the bundled stack can't open is re-probed on the SYSTEM gst
  (`process.rs::gst_probe`); if it works there the stream runs as a system `gst-launch-1.0`
  child (`gst_on_system` → `capture::start(.., system_gst)`, no live bitrate) — that's how
  an AppImage gets `vah264enc` (gst ≥1.22) or an `nvh264enc` new enough for 5xx+ NVIDIA
  drivers (the bundled 1.20 one fails: "Could not configure supporting library").
  pulsar-render gets `$APPDIR/usr/lib` via `apply_render_lib_env` (linuxdeploy gives it a
  bare `$ORIGIN` RUNPATH → its libav*.58 resolved from the host). A capture that fails to
  start or dies mid-stream is sent to the client as a `!` stats line (its video error).
  **Check:** `Pulsar.AppImage --selftest` (`src-tauri/src/selftest.rs`); CI runs it in
  clean distro containers (`scripts/appimage/smoke.sh`) before publishing.

All paths emit **RTP/H.264(+HEVC/AV1) over UDP**.

## Capability probe (`src-tauri/src/caps.rs`) — read before touching codecs

Moonlight-model startup probe, re-run every launch, no persistence:

- Windows NVENC: `pulsar_capture::nvenc_codecs()` opens a real NVENC session and
  enumerates codec GUIDs — **instead of** the ffmpeg one-frame probe, which targets
  the display GPU (wrong on hybrid laptops) and lists encoders on GPUs with no
  NVENC silicon (GP108/MX). Advertising HEVC there → client SDP says HEVC, host
  degrades wire to H.264 → **permanent black screen**. Families with zero
  validated codecs are dropped.
- Decoders: `pulsar-render --probe` decodes real keyframes headlessly (replaced
  the old hardcoded "MF decodes X" assumption).
- `DecoderCap.incompatible_with` blacklists bad host-encoder × client-decoder
  combos (e.g. rkmpp-HEVC × nvenc on RK3588).
- `process.rs` keeps a Sunshine-style one-frame `probe_encoder_codec` +
  `resolve_codec_validated` with an unreliable-probe guard for hybrid laptops.
