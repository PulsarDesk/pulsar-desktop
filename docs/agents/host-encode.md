# Host capture/encode + capability probe

Part of the pulsar-desktop agent docs — index: [`CLAUDE.md`](../../CLAUDE.md).

## Host capture/encode paths

- **Windows, NVENC present** — `crates/pulsar-capture`: DXGI Desktop Duplication
  or WGC → NVENC SDK directly → hand-rolled RTP packetizer. No ffmpeg in the hot
  path; falls back to ffmpeg if init fails.
  - **Which source** (`src/path.rs`): a monitor on the NVIDIA GPU uses DXGI (same device,
    earliest hand-over). A monitor on another GPU (hybrid laptop panel on the iGPU) is
    measured once per adapter pair + driver + mode + fps: DXGI needs the encoder's CPU
    staging hop, WGC on the NVIDIA device gets the frame GPU-side but ~2 ms later. The
    verdict is cached in `%LOCALAPPDATA%\dev.pulsar.app\capture-paths.txt`.
  - A third candidate on a hybrid host is the monitor GPU's own encoder through Media
    Foundation (`src/encode/mf.rs`, DXGI capture → VideoProcessor NV12 → async hardware MFT,
    e.g. AMD VCN). It has no CPU hop, but needs a 1 ms lead to beat NVENC's cleaner picture.
    Measured on Vega it lost: WGC 4.57, DXGI 6.48, DXGI-MF 8.40.
    The host label then names "Media Foundation".
  - The native path is still entered only when NVENC exists. An AMD/Intel-only host keeps the
    ffmpeg chain; `MfEncoder` could serve it, but that is untested.
  - A monitor this process already duplicates (a second session on the same screen: DXGI
    `E_INVALIDARG`) falls back to WGC, so every session keeps its own NVENC encoder + ABR.
  - Both sources encode a fresh frame as soon as it arrives (one frame per fps slot).
    Holding it to the slot deadline cost ~4 ms at 120 fps.
  - `examples/capbench.rs` measures paint → encoded latency per source (see its doc);
    `PULSAR_CAPTURE=dxgi|wgc|dxgi-mf`, `PULSAR_CAPTURE_REPROBE=1` and `PULSAR_EMIT_ON_ARRIVAL=0`
    are the knobs (`--444` for a 4:4:4 stream).
  - **4:4:4 stays native** (2026-11-15) — see "4:4:4 chroma" below.
- **macOS (2026-11-14, LOCAL, untested on macOS hardware)** — `crates/pulsar-capture/src/mac/`:
  ScreenCaptureKit (pulsar-core `macos_capture::ScStream`: one display, NV12 video range, the
  client's fps as `minimumFrameInterval`, cursor composited, queue depth 5) → VideoToolbox
  `VTCompressionSession` (`mac/vt.rs`: H.264 High / HEVC Main, hardware preferred, RealTime,
  no frame reordering, `MaxFrameDelayCount` 0, `EnableLowLatencyRateControl` when the OS takes
  it for the codec — the session is re-created without it otherwise, `PrioritizeEncodingSpeed…`
  for latency picks, 10 s safety GOP) → Annex B (`mac/annexb.rs`, parameter sets before every
  keyframe) → the same `RtpEgress`. Same `CaptureHandle` contract as Windows: live bitrate
  (`AverageBitRate` + `DataRateLimits` 1.5× / 1 s), keyframe on request, live display switch
  (rebuild in the thread, revert on failure), a stream SCK stops on its own is rebuilt (≤ 5 in a
  row), a still screen re-encodes the last frame once per fps slot, `LiveStats.source = "sck"`,
  display kept awake (`IOPMAssertion`). Not possible with VideoToolbox: reference-frame
  invalidation (declined, counted as `rfi_declined`) and periodic intra refresh (keyframes
  instead). Entered when the resolved encoder is VideoToolbox, codec ≠ AV1 (an HDR request streams SDR — see "HDR10" below; a 4:4:4
  request streams 4:2:0 here — see "4:4:4 chroma");
  `PULSAR_FFMPEG_CAPTURE=1` forces ffmpeg. Falls back to the ffmpeg avfoundation CLI on macOS <
  12.3 (ScreenCaptureKit is dlopen'ed, never linked — the app still starts on older macOS), a
  missing **Screen Recording permission** (the host's home screen shows a notice with "İzin
  iste" / "Sistem Ayarlarını aç" — `screen_permission.rs`, `ScreenPermissionNotice.svelte` —
  and the client gets a `!` stats line `host.screenPermission`), or a session VideoToolbox
  refuses (a low-latency-rate-control session that refuses its setup is retried without it
  first). `host_displays()` on macOS = the CoreGraphics list, main first (the index the native
  capture takes). The frame is **stretched** to the requested size (`preservesAspectRatio = NO`
  on macOS 14+, like ffmpeg's scale), never letterboxed. **Multi-display pointer:** the capture
  publishes the display it streams (pulsar-core `macos_capture::set_streamed_display`; the
  native thread on every build, the ffmpeg fallback at start) and the macOS injector maps the
  absolute pointer onto that display's bounds (main display when none / gone). The ffmpeg
  fallback captures the picked display too (`"Capture screen N"`, N = place in CoreGraphics'
  active list — a bare index would count the cameras first). A 7 s no-frame notice on macOS uses
  `host.captureNoFramesMac` (no GPU wording). Open (possible, not done): per-window capture
  (`window_hwnd` → `SCContentFilter initWithDesktopIndependentWindow:` + a macOS window list for
  the client's picker + window-relative pointer mapping); the client's labelled ghost pointer
  region (`ghost_region`) is still None on macOS. The streamed display is process-wide: two
  sessions streaming different displays map the pointer onto the one published last.
- **Linux X11** — the ffmpeg **libraries in-process** (`src-tauri/src/host/libav.rs`:
  x11grab → libavfilter → libavcodec x264/x265/SVT-AV1/NVENC/VA-API/QSV → libavformat RTP;
  live bitrate, forced IDR on request, short-GOP recovery; `PULSAR_LIBAV_HOST=0` disables).
  Falls back to the ffmpeg CLI below when it cannot start or for Vulkan (an HDR request is SDR
  on x11grab — 8-bit capture — and every ffmpeg plan is 4:2:0, see "4:4:4 chroma", so neither
  leaves it).
  - **AMD / Intel (VA-API, QSV) in-process** (2026-11-15, untested on Linux hardware):
    VA-API = `scale(BT.709 limited),format=nv12,hwupload` (`PULSAR_VAAPI_VPP=1` =
    `hwupload,scale_vaapi` GPU conversion) → `h264/hevc/av1_vaapi` (`async_depth=1`, no
    B-frames); QSV = system-memory NV12 (BT.709) on a QSV device whose VA-API child is an Intel
    GPU's node (`drm_node::qsv_node`) (`veryfast` + `low_power` in game mode — opened without it
    on GPUs that lack VDENC, remembered for re-opens — `look_ahead=0`, `async_depth=1`,
    `forced_idr`). Both tag the VUI BT.709 (what the client decodes). Rate (`rate_limits`):
    game = CBR with the ~0.5 s VBV (`PULSAR_LIBAV_SHORT_VBV=1`: ~125 ms, unmeasured); desktop
    = the CLI's burst headroom (2x peak, 1 s buffer → VBR on both, never CBR).
    `*_vaapi` (and QSV before FFmpeg 6.0) read the rate only at open, so a bitrate step
    re-opens the ENCODER alone on the same capture/graph/surfaces/RTP muxer at the next IDR
    the stream has anyway (≤ 0.25 s in game mode, ≤ 2 s desktop; a ≥ 25 % cut at once) —
    one IDR, no ffmpeg restart. QSV on FFmpeg ≥ 6.0 takes it live. Rules + tests:
    `host/libav_policy.rs`. `PULSAR_LIBAV_HW=0` sends VA-API / QSV back to the CLI.
    **Runtime guard** (`HwGuard`): 30 rejected frames in a row, or 3 s of frames in with no
    packet out, or a re-open failing twice ends the engine and turns the in-process hardware
    path off for the process (`libav::hw_in_process_failed`); host.rs's dead-engine watcher
    re-issues the request at once, and it lands on the CLI chain.
  - **Render node** (`src-tauri/src/drm_node.rs`): `StreamCfg::vaapi_device` (CLI
    `-vaapi_device`, the VA-API encoder probes / caps, the in-process VA-API engine) is the
    render node of the first non-NVIDIA GPU with an enabled, connected connector (sysfs), then
    the other non-NVIDIA GPUs, else `renderD128`; `PULSAR_VAAPI_DEVICE` overrides. Picked once
    per process (memoised); with more than one candidate the startup probe
    (`process::settle_vaapi_node`, from the caps probe / the warm-up) takes the first node a
    VA-API encode probe passes on, so a display-only screen GPU never drops the host to x264.
    **Not following it** (Linux sub-platform gaps): the CLI QSV encode and its caps probe
    (`-init_hw_device qsv=qs` in pulsar-core `pipeline/command.rs` — libva's default node; the
    in-process QSV picks an Intel node itself), and **Wayland** hosts, which encode VA-API
    through GStreamer (`vah26Xenc` / `vaapih26Xenc` in pulsar-core `pipeline/gst.rs`): those
    elements open GStreamer's default/first VA device, so a hybrid Wayland box may encode on
    the other GPU. Mapping the node there needs the per-device element names
    (`varenderD129h264enc`) or a device env in pulsar-core — not done.
  - **CLI fallback absorbs ABR steps on Linux too** (as Windows/macOS, same rule —
    `cli_bitrate_absorbs`): a bitrate-only step inside a 10 % band, or an up-step within 6 s of
    the last restart, keeps the running ffmpeg; a real down-step always restarts — only while
    the stream is actually on the CLI (a libav / Wayland / X11 gst stream clears it).
  - **Host-ordered restarts are never absorbed** (every OS): the display-mode watcher, the
    per-window watcher (window resized / closed), the CLI keyframe restart (`on_nack`, desktop)
    and the dead-engine watcher re-send the stored request unchanged; they set the session's
    `host_restart` flag first, so neither the CLI absorb nor the in-process libav fast path
    (x11grab keeps the size it opened with) swallows them. Wayland's in-process fast path
    still takes them (a portal restart there leaves the cast black). The keyframe request on a
    live in-process engine is answered with `request_keyframe` and sends no restart.
  - **NVENC there goes through the NVENC SDK** (2026-11-14, untested on Linux hardware):
    `pulsar_capture::NvencCuda` (`crates/pulsar-capture/src/encode/cuda_enc.rs`) instead of
    libavcodec's `h264_nvenc`. See "Linux NVENC SDK" below.
- **Linux X11 with a GStreamer-only HW encoder** (RK3588 / Orange Pi 5: `mpph26Xenc`; also
  gst VA-API/NVENC when ffmpeg has none) — `handlers.rs` "x11 gst" branch, runners in
  `host/x11_gst.rs`. Two runners (2026-11-15, untested on the Pi):
  - **In-process** (`pulsar_core::capture::start_pipeline`, core `capture/live.rs`) for every
    `ximagesrc` pipeline (whole screen / monitor region / one window): the description runs
    inside the app, held in the session's `cap_slot`. ABR steps set `bps` on the running
    encoder, `MediaNack([0])` forces a key unit, a recovery flip toggles the 500 ms forced
    key-unit timer — all live, no restart (`x11_gst::try_live`). Its description is built with
    the NORMAL GOP (`gop=fps`) and recovery runs only through that timer, so a flip back to
    Normal really returns to the long GOP (a baked `gop=fps/2` used to stay for the whole
    stream). The encode meter is pad probes on the encoder: frames out and the real per-frame
    time inside it (`pipeline::gst::EncodeTimer`), not the frame gap the `-v` meter reads.
    - **Start off the session loop** (`X11InProcStart`, own thread keyed to `cap_gen`): a live
      `ximagesrc` pipeline never prerolls, so `start_pipeline` waits for the encoder's FIRST
      frame and says why it failed (`StartError::Failed` bus error / `NoFirstFrame` /
      `Unavailable`). BGRx-direct that fails (the RGA >4G case) retries in-process with the
      CPU-convert I420 variant at once (2.6 s, not 2.6 + 3 + 2.6) and BGRx-direct is then off
      for the process; anything else falls back to the child (I420 if BGRx just failed). The
      old pipeline's teardown also runs off the loop (taken out of `cap_slot` in its own
      statement — the input path locks it per event) and the start joins it first.
    - **Watchdogs:** a pipeline whose `frames_encoded()` stayed flat 2 s (a driver call hung
      with no bus error) or that died is not absorbed: the next request restarts it.
      `LATCH_AFTER` (2) failed starts in a row turn the in-process runner off for the process;
      a frame resets the count. `PULSAR_GST_INPROC=0` forces the child.
    - **Trade-off (unverified on the Pi):** the vendor stack (`mpph26xenc` + librga/libmpp)
      now runs INSIDE the app — a crash or abort there kills the whole host app (UI, every
      session, audio), where it used to kill only the child. A marker
      (`$TMPDIR/pulsar-gst-inproc-start.marker`) is written before each in-process start and
      removed 10 s after it is up; a launch that finds it left behind runs the child for that
      whole run (and removes it, so the launch after tries in-process again).
  - **`gst-launch` child** for **KMS** (`kmssrc` zero-copy, game mode / `cursor_external`)
    and the fallback above. KMS must stay a child: reading the scanout FB needs
    `CAP_SYS_ADMIN`, carried only by the user-made `~/pulsar-gst-launch` copy, and the app
    itself never gets that capability. gst-launch has no control channel, so a bitrate-only
    step goes through `cli_bitrate_absorbs` (the same rule as the ffmpeg CLI on Windows /
    macOS): the same rate is kept, a **down-step beyond 10 % always restarts** (holding a rate
    above what the link carries means sustained queueing — latency first), an up-step beyond
    10 % restarts at most once per 6 s since the start; the pacer keeps the child's real rate.
    A recovery flip / dead child / host-ordered restart restarts it. A real live control for
    KMS would need the privileged child to only capture and hand DMABufs to an in-process
    encoder (`unixfdsink`/`unixfdsrc`, gst ≥ 1.24) — the Pi's gst 1.20 has no such element.
    KMS also stays primary-CRTC only (a non-primary monitor takes `ximagesrc`).
  - **Host-ordered restarts** (every OS): the display-mode watcher, the window watcher and the
    keyframe-by-restart in `on_nack` re-issue the stored request through
    `host::send_host_restart`, which raises `host_restart`; `make_on_stream` then skips every
    "nothing changed" fast path and bitrate absorb for it (the Wayland portal capture excepted:
    PipeWire follows the size itself). Before this an identical re-issue was swallowed by the
    absorbs (Linux child / in-process, and the ffmpeg CLI on Windows/macOS). `on_nack` skips
    the restart when a live encoder took the keyframe request (native NVENC / VideoToolbox,
    in-process gst, libav). Every fast path keeps `last_req_store` on the latest request
    (`refresh_last_req`) so a re-issue never puts a stale bitrate back over the client's ABR.
    The gst branch now stores its request too (it returned before the store, so the watchers
    never fired on an X11 gst host).
  - Windows / macOS: no GStreamer host path there (native NVENC / MF / VideoToolbox capture
    with their own live controls, ffmpeg CLI fallback with the same absorb rule).
    Wayland's in-process portal pipeline still bakes the recovery GOP into its description
    (x264's `intra-refresh` can't be driven by a timer, and a portal restart per flip is worse
    than the leftover short GOP) — a known gap.
- **Windows (non-NVENC) / macOS fallback** (and the Linux fallback) — ffmpeg CLI (`ddagrab`/
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

## 4:4:4 chroma (2026-11-15, LOCAL, not tested in a real session)

Opt-in (`PULSAR_YUV444=1` on the client); 4:2:0 stays the default. Three gates, so a 4:4:4
stream only ever reaches a client that shows it:

1. **Client probe.** `pulsar-render --probe-444` — a SEPARATE invocation from `--probe`, run
   by `caps.rs::with_yuv444` only when `PULSAR_YUV444` is set, under its own 6 s timeout, and
   merged additively (a hang/failure leaves every codec at 4:2:0; the `--probe` caps are never
   touched — the Windows check makes a hardware D3D11 device, which blocks while the NVIDIA
   driver is wedged). It decodes a canned 4:4:4 keyframe per codec
   (`crates/pulsar-render/testdata/test-444.{h264,h265}`) and lists the codecs that worked →
   `DecoderCap::yuv444` (+ `yuv444_max_px_s`, Linux). Per OS: **Windows** — the Media Foundation decoder on the
   renderer's own D3D11 device must output AYUV and the present VideoProcessor must Blt it
   (`win/decode.rs::probe_yuv444`). Measured: HEVC yes on an NVIDIA device (RTX 3080 Laptop),
   no on AMD Vega and Snapdragon X (Adreno); H.264 4:4:4 nowhere (the Microsoft H.264 decoder
   has no 4:4:4: `CopyDecodedFrame failed`). A hybrid laptop's renderer uses the default
   (iGPU) adapter, so it reports no 4:4:4. **macOS** — VideoToolbox must decode the fixture
   into `444f`/`444v` (`mac/decode.rs::decodes_444`; untested on a Mac). **Linux** — libavcodec
   in software decodes it to yuv444p for the planar GL upload (`decode.rs::select_yuv444`), on
   ONE thread (slice threads only, and NVENC writes one slice), so the probe times it and
   reports `max_px_s` = measured px/s / 2; under 720p30 no entry (in practice the RK3588
   boards). AV1 4:4:4 is never claimed (no fixture; no host encoder we drive makes AV1 High).
2. **Client request.** `play.rs`: with 4:4:4 wanted an "auto" codec is picked among the 4:4:4
   codecs first (`pick_codec`: HEVC 4:4:4 over AV1 4:2:0); the 4:4:4 list is cut to the codecs
   whose `yuv444_max_px_s` holds the session's top point (`yuv444_fits`, w x h x fps); then
   `yuv444_request`: asked for, and the codec in that list. The list travels as
   `StreamReq::decode_yuv444` (pulsar-core) on every (re-)request.
3. **Host.** `StreamReq::wants_yuv444(codec)` with the codec it finally streams (after its own
   fallbacks) → `CaptureConfig::yuv444` (Windows native NVENC only). `StreamPlan::yuv444` (the
   ffmpeg chain) is always false. An old client (no list) gets 4:2:0.

Encode per OS:

- **Windows NVENC (native path, kept):** H.264 High 4:4:4 Predictive / HEVC RExt
  (`nvsession.rs`) when `NV_ENC_CAPS_SUPPORT_YUV444_ENCODE` says so; otherwise (or AV1) 4:2:0 on
  the same path. The input is the scaled/rotated BGRA itself (`d3d::InputFormat::Argb`, NVENC
  converts): the NVIDIA VideoProcessor cannot write AYUV. NVENC's RGB→YUV follows the VUI, so a
  4:4:4 stream carries full-range BT.709 (`vui_444`) — the same YUV values the 4:2:0
  VideoProcessor path writes (measured on colour bars; without the VUI NVENC writes limited
  range). The hybrid Media Foundation candidate is skipped and nothing is measured/cached
  (`path::Plan::nvenc_only`). The host's stats label then reads `H.265 4:4:4 · …`
  (`CaptureStats::yuv444`). Guards (`lib.rs::start_capture_encode`, `encode/policy.rs`):
  - **Pixel-rate ceiling** `policy::YUV444_MAX_PIXEL_RATE` = 1080p120 (1440p60 under it): above
    it the build is 4:2:0 (re-judged on every rebuild, e.g. a resolution change).
  - **Preset <= P2** for 4:4:4 (`preset_444`; a forced `PULSAR_NVENC_PRESET` wins) and **no
    two-pass** (`two_pass_444`, also on a live bitrate change; `PULSAR_NVENC_MULTIPASS=1` wins).
  - **4:2:0 retry**: a 4:4:4 `Encoder::new` that fails (an untested RExt x RFI / intra refresh /
    preset combination) is rebuilt once in 4:2:0 on the same native path — never the ffmpeg
    fallback or a dying capture thread.
  - **No 4:4:4 here** (caps say no, or that retry): 4:2:0 for the rest of the capture,
    remembered per process and codec (`NO_444`), and a first build plans again as a 4:2:0 stream
    when that picks another path (a hybrid host's cached `DxgiMf` verdict) — so a GPU without
    4:4:4 never trades the measured-best path for nothing.
  - Not gated: the build-time low-budget class. The session starts cautiously (~4 Mbit, always
    low-budget at 1080p), so gating on it would mean 4:4:4 never; 4:4:4 is an explicit opt-in
    for sharp text on a LAN-class link. At WAN rates the 2x chroma data takes bits from luma.
  - Cost, RTX 3080 Laptop (`encode/new.rs::nvenc_444_encodes_on_the_native_path`, ignored;
    `PULSAR_TEST_SIZE` / `PULSAR_TEST_FPS`; variants game P1, RFI, intra refresh, remote+RFI):
    1280x720 submit ~0.95 ms 4:2:0 → ~1.7–1.9 ms 4:4:4 (~0.35 ms NVENC's RGB conversion, the
    rest the 4:4:4 encode); a shader writing AYUV would win the conversion back.
    Submit p50 per frame, 2026-11-15, HEVC / H.264 (game P1, RFI, intra refresh and
    remote+RFI all within ~0.1 ms; static test pattern):

    | Size | 4:2:0 | 4:4:4 |
    | --- | --- | --- |
    | 1920x1080 | 1.6 / 1.7 ms | 3.0 / 3.5 ms |
    | 2560x1440 | 2.2 / 2.4 ms | 4.2 / 4.9 ms |
    | 3840x2160 | 4.7 / 5.1 ms | 8.8 / 10.3 ms |

    Remote+RFI at 1080p60: 4:2:0 runs P4 (3.0 / 3.6 ms), 4:4:4 the capped P2 (3.1 / 3.0 ms).
    Every variant built and encoded (no 4:2:0 retry needed on this GPU).
- **Windows ffmpeg fallback / AMF / QSV / software:** 4:2:0 (`StreamPlan::yuv444` = false).
  AMF / VAAPI refuse `-pix_fmt yuv444p` + `rext`, and the fallback chain then ended on a CPU
  libx264/libx265 4:4:4 encode — a large latency regression for a stream that asked for quality.
- **macOS host:** the native ScreenCaptureKit + VideoToolbox path stays and streams 4:2:0 (no
  4:4:4 VideoToolbox encoder yet; leaving the native path for the ffmpeg CLI would cost latency
  for an unverified 4:4:4).
- **Linux host:** 4:2:0 on every encoder, so an NVENC/X11 host keeps the in-process libav path
  (live bitrate, keyframe on request, RFI). Before 2026-11-15 a 4:4:4 request moved the stream
  to the ffmpeg CLI (x11grab, `-pix_fmt yuv444p`) and lost all three — the same latency trade
  the macOS host refuses. The in-process path and the CUDA NVENC SDK encoder take NV12 only;
  a Linux 4:4:4 encode would need them to take RGB/yuv444p first (and a measurement).

Known, not changed here: the 4:2:0 native NVENC stream is full-range BT.709 data **without** a
VUI (decoders read it as limited range); the Linux presenter assumes limited BT.709 for every
stream.

## Host monitor picker + live display-change watch (per OS, 2026-11-14, untested on macOS / Wayland hardware)

`process::host_displays()` fills `StreamCaps::displays`; the index travels back in
`StreamReq::display_idx`. `host/handlers.rs::spawn_display_list_watch` re-reads the list after a
display-change event (`display_watch.rs`) and sends `DataMsg::Displays` when it differs; while no
event source is live it polls every 3 s (`display_watch::event_driven()`).

| OS | List | Index → capture | Change events |
| --- | --- | --- | --- |
| Windows | DXGI outputs (`pulsar_capture::list_displays`) | DXGI output / ddagrab `output_idx` | `WM_DISPLAYCHANGE` (hidden window) |
| Linux X11 | `xrandr` (`process::linux_displays`) | x11grab `DISPLAY+x,y` / ximagesrc region | RandR notify (`pulsar_core::displays`, x11rb) |
| Linux Wayland | `wl_output` + `zxdg_output_v1` over a short-lived Wayland connection (`pulsar_core::displays::list`), origin output first | the **ScreenCast portal dialog decides** — no portal lets an app pre-select a monitor. Per session (beside the restore token) the host remembers the request the picker was last answered for and the monitor that pick streams (`process::WaylandPick`, from `WaylandCapture::stream_rect` → `displays::index_of_rect`). The token is dropped (picker opens again) only for a NEW request — one that differs from both (`process::wayland_wants_repick`); reconnects / codec / ABR restreams of the same request restore silently. A pick that differs from the request is sent to the client as `DataMsg::StreamedDisplay(idx)`: the client (`play/hold.rs`) moves `cur_display`, the renderer overlay (`display N` on stdin), the webview menu (`host-display-streamed` event) and the pane's `displayIdx` there | long-lived Wayland connection: `wl_output` global added / removed, `wl_output.done` |
| macOS | `CGGetActiveDisplayList` (main first) + pixel mode + `CGDisplayCopyAllDisplayModes`; names from `NSScreen.localizedName` (cached, refreshed on the main queue) | avfoundation `Capture screen N` (`process::avfoundation_screen`; ffmpeg numbers screens in the same list order); absolute pointer goes to that display's `CGDirectDisplayID` bounds (`DesktopInput::set_display`; the id is re-resolved on an index change, a display-change generation change and at least every second). The host-pointer marker maps the pointer into the same display (`displays::pointer_in`) and is sent while the host user takes the mouse (`host::takeover`, macOS listen-only tap — `parity/host-takeover`) | `CGDisplayRegisterReconfigurationCallback` (registered on the main queue) |

Watcher notes: `pulsar_core::displays::watch` never blocks (macOS registers on the main queue,
Linux connects on its own thread); `display_watch::event_driven()` restarts a Linux watcher whose
connection dropped, at most every 30 s. The Wayland watcher releases `wl_output` proxies on
`GlobalRemove`. Nameless monitors (macOS before the AppKit name cache fills, a Wayland output with
no name / description / make) come from core with an empty name + `builtin` flag and are labelled
in the host's UI language (`process::display_label`, keys `display.builtin` / `display.numbered`
in `src-tauri/lang/{tr,en}.json`).

Not possible: on Wayland the host cannot choose the captured monitor itself — the portal shows
its own dialog to the person at the host (the restore token can only bring back an earlier pick).

## Per-window capture ("Pencere", Phase 2b co-op) — per OS

One application window instead of a monitor: the client's window pick
(`StreamReq::window_hwnd`, from `QueryWindows` / `list_remote_sources`) or, in game mode, the
window of the game the session launched (`process::resolve_launched_window` → `launched_hwnd`).
The id on the wire is an opaque `i64` whose meaning is the HOST's (table in pulsar-core
`service/wire.rs` `WindowInfo`).

| Host | Picker | Capture | Status |
| --- | --- | --- | --- |
| Windows | `pulsar_capture::list_capture_windows` (HWND) | WGC → NVENC (native path; HDR10 when the window's monitor is in HDR mode; 4:4:4 too); a non-NVENC stream falls back to the monitor | shipped |
| Linux X11 | `_NET_CLIENT_LIST` via pure-Rust `x11rb` (X window id) | libav `x11grab window_id` (`host/libav.rs` `Source::X11Window`), ffmpeg CLI `-window_id` + `scale`, or gst `ximagesrc xid=` (`gst::x11_window_pipeline`, MPP hosts; never KMS) | untested on hardware |
| Linux Wayland | one entry `WindowInfo::PORTAL_PICK` ("Pencereyi bu bilgisayarda seç") | ScreenCast portal with source type WINDOW only (`capture::start_with(.., Source::Window)`): the person AT THE HOST picks the window in the compositor's dialog; frames scaled into the requested box (`wayland_pipeline_sized`) | untested on hardware |
| macOS | ScreenCaptureKit `SCShareableContent` (`CGWindowID`) | `SCContentFilter(desktopIndependentWindow:)` → NV12 → loopback TCP → ffmpeg `-f rawvideo` → VideoToolbox (`pulsar_core::window_capture::serve_for`) | untested on hardware |

- Desktop glue: `src-tauri/src/host/window_src.rs` (picker entries, target per backend, encode
  size = the window's aspect fitted into the requested box, never upscaled, and the window
  each session streams now) and the `host_window` block in `host/handlers.rs::make_on_stream`
  (run under `block_in_place`: the macOS ScreenCaptureKit query + start can wait ~10 s behind
  the Screen Recording prompt). A window that is gone (or a ScreenCaptureKit start that fails)
  streams the screen, as Windows does.
- **Input (X11 / macOS):** while a session streams a window, the client's 0..1 absolute pointer
  maps onto THAT window, refreshed every 250 ms while it moves (`window_src::input_area`): X11 =
  the window's rect in the root (`translate_coordinates` + `get_geometry`) as the uinput
  pointer region (the absolute device spans the whole root); macOS = the window's
  `CGWindowListCopyWindowInfo` bounds in global points (`DesktopInput::set_window_rect`). The
  window is raised + focused once per pick (X11 `_NET_ACTIVE_WINDOW`; macOS
  `NSRunningApplication activateWithOptions:` + `AXRaise` with the Accessibility grant). Keys
  go to the focused window (no PostMessage-style per-window routing off Windows: X11 / macOS
  inject at the desktop, so two co-op panes on two windows of ONE host share one focus).
- **Size changes:** a host watcher (`host.rs`, 1 s, debounced over two looks) re-issues the
  last StreamReq when the streamed window settles at a new size or is closed / its
  ScreenCaptureKit stream stopped (x11grab keeps the size it opened with; a closed window then
  streams the screen).
- **Fit to the pane** (`StreamReq::adapt` with a window pick): `display_mode::fit_window` ->
  `pulsar_core::window_capture::fit` on X11 (unmaximize, `_NET_MOVERESIZE_WINDOW` on the
  window's RandR monitor clipped to `_NET_WORKAREA`, restored on teardown incl. the maximized
  state) and macOS (`AXPosition` / `AXSize`, needs the Accessibility grant; the menu bar is
  kept free with a fixed 38 pt inset, the Dock is not accounted for). Full-screen windows are
  left alone.
- **X11 limits:** without a compositing manager the parts covered by other windows are
  undefined in the grab.
- **Wayland:** no program can list, choose, move, resize or locate other programs' windows, so
  the pick is always the host person's, there is no fit to the pane, and the uinput pointer
  still spans the whole desktop for a window cast (mapping it into the window would need the
  RemoteDesktop portal's stream-relative `NotifyPointerMotionAbsolute` instead of uinput — not
  built). The window's restore token is kept per SESSION (`wl_window_token`), apart from the
  screen token, which is per session too (`host.rs` creates it empty for every connection): a
  re-stream re-opens the same window without the dialog; streaming the screen again forgets it.
  Every pane's pick shares `hwnd = PORTAL_PICK` (-1), so the client never marks that entry as
  "in pane N" (`sessions.svelte.ts` `paneUsageFor`).
- **macOS:** needs macOS 12.3+ (checked at runtime by class lookup) and the Screen Recording
  permission. ScreenCaptureKit is **weak-linked** (`src-tauri/build.rs`:
  `-Wl,-weak_framework,ScreenCaptureKit`, since `objc2-screen-capture-kit` links it strongly),
  so the app still launches on older macOS and just offers no window source there. The frame
  server repeats the last frame at the stream fps (ScreenCaptureKit only delivers on change;
  rawvideo timestamps count frames) and serves one ffmpeg at a time, so the encoder fallback
  chain can reconnect. `serve_for` returns the frame size from its own `SCShareableContent`
  query (one query per stream start).

## Linux NVENC SDK — reference-frame invalidation on Linux hosts (2026-11-14, LOCAL, unpushed)

libavcodec's and GStreamer's NVENC encoders cannot invalidate a reference frame, so a Linux
host could only heal a client-reported loss with a keyframe. Now the Linux host on an NVIDIA
GPU runs the **same NVENC SDK session as Windows**:

- `pulsar-capture` builds on Linux too (only the encoder part; its capture stays Windows +
  macOS — the macOS ScreenCaptureKit → VideoToolbox path is `src/mac/`).
  `encode/cuda.rs` = the CUDA driver API from `libcuda.so.1` (dlopen: `cuInit`, context,
  `cuMemAllocPitch`, `cuMemcpy2D`); `encode/cuda_enc.rs` = `NvencCuda`: `libnvidia-encode.so.1`
  (dlopen), session on a CUDA context (`NV_ENC_DEVICE_TYPE_CUDA`), one pitched NV12 device
  buffer registered once (`NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR`), two host→device copies
  per frame, sync encode, `RtpEgress` (our RTP packetizer + send log). `encode/nvsession.rs`
  holds the session config shared with Windows (`configure`: presets, CBR, one-frame VBV, RFI
  DPB, intra refresh, low-budget / two-pass classes), so both OSes stream with the same
  settings. `invalidate_lost` = the Windows `RefTracker` + `nvEncInvalidateRefFrames` +
  recovery SEI. Nothing is linked at build time.
- **X11** (`src-tauri/src/host/libav.rs`): an NVENC plan keeps x11grab → libavfilter
  (scale → NV12), then `NvencCuda` encodes instead of libavcodec (no libavformat RTP muxer).
  `LibavHost::request_rfi` → `Cmd::Rfi`. `NvencCuda::new` failing (no driver / CUDA, NVENC
  API < 12.2 = driver < 550, no free session) logs a warning and opens libavcodec as before.
- **Wayland** (pulsar-core `capture::start_with_frames` / `start_full`, `capture/frames.rs`; screen casts
  only — a Wayland WINDOW cast keeps the gst pipeline): the portal's
  PipeWire stream → `videoconvert` → NV12 → `fakesink` handoff → the app's `FrameEncoder`
  (`src-tauri/src/host/nvenc_frames.rs`, the SDK encoder). Built on the first frame at the
  cast's size, rebuilt on a size change. If it cannot be built or no frame arrives in 4 s,
  the gst-encoder pipeline runs on the SAME portal session (no second share dialog). Chosen
  when the gst pick is `nvh26xenc`, or software x264 while the client left the encoder open /
  asked for NVENC (an AppImage whose bundled `nvh264enc` cannot open a 5xx driver then still
  gets NVENC), and the codec is one NVENC encodes. `WaylandCapture::request_rfi` forwards
  the loss; bitrate / keyframe / short GOP reach the SDK encoder through the same calls.
- `host.rs` advertises `FEAT_RFI` on Linux once a background probe (`linux_nvenc_sdk`,
  started by `go_online`) found NVENC on CUDA; `on_rfi` feeds both the libav engine and the
  Wayland capture.
- Knobs: `PULSAR_NVENC_SDK=0` (libavcodec / gst encoders as before), `PULSAR_CUDA_DEVICE=N`
  (default 0), plus the Windows ones (`PULSAR_NVENC_PRESET`, `PULSAR_NVENC_MULTIPASS`,
  `PULSAR_IDR_SEC`). Encoder log: `$TMPDIR/pulsar-capture-dbg.txt`.
- **Untested on hardware**: no Linux machine with an NVIDIA GPU was available; only type
  checks, unit tests (the no-driver path of `NvencCuda::new`, plane-size rules, the gst handoff
  plumbing with `videotestsrc`) and the libav end-to-end x264 test ran.
- Which clients use it: every desktop client on pulsar-render (Windows, macOS, Linux on its
  Rust RTP path; `play.rs` `rfi_client`), see client-video.md.
- **Recovery flip absorbed** (2026-11-15): the client's loss flip no longer forces an IDR
  every 0.5 s on top of an SDK encoder that heals that mode itself, decided from what the BUILT
  encoder does (`libav::EncoderHeals`: NVENC turns RFI off without the GPU caps, for AV1 and
  under intra refresh), per mode (`EncoderHeals::absorbs`): the short GOP is absorbed with RFI
  or the intra wave, an intra-refresh flip only with the wave (the client sends `rfi 0` under
  intra refresh, so an RFI-only encoder needs the cadence). X11: `libav::recovery_cadence`;
  Wayland: `nvenc_frames::short_gop`, fed by the raw-frame factory (`BuiltHeals`). The Windows
  native path always absorbed it (`live_change`).
- A stream whose encoder has no RFI (libavcodec fallback, a Wayland window cast, a non-NVENC
  pick) ignores `MediaRfi` while the host still advertises `rfi`: no keyframe answer, since a
  hole the client's NACK repair fills must cost nothing; the client renderer asks for the
  keyframe early until the stream has shown a recovery frame (client-video.md).
- The FEAT_RFI probe (`host.rs` `linux_nvenc_sdk`) honours the encoders' own
  `PULSAR_NVENC_SDK` switch (`0` / `false` / `off`, `libav::sdk_enabled`): `off` used to keep
  advertising RFI with the SDK encoder disabled.
- Reviewed against the Windows encoder (2026-11-15): `NvencCuda` matches `Encoder` /
  `submit.rs` in the RFI DPB / `numRefL0` config (shared `nvsession::configure`), the
  `RefTracker` record / plan / `invalidate_failed` calls, the IDR detection, the keyframe
  policy and the recovery-SEI prefix. Difference kept: the Wayland path stamps the RTP time at
  encode, not at capture (`RawFrame` carries no capture time) — a few ms of the GStreamer
  convert, constant per frame.
- **VA-API / Intel / AMD**: no equivalent through the encoders we use. libavcodec's
  `*_vaapi`/`*_qsv` and GStreamer's `va*`/`vaapi*` encoders expose no reference invalidation.
  Possible only with an encoder of our own: raw VA-API (the app owns the DPB and the
  reference lists, but must also write SPS/PPS/slice headers itself), Intel oneVPL
  (`mfxExtAVCRefListCtrl.RejectedRefList`) or AMD AMF long-term references. Not done; those
  hosts keep the keyframe recovery.

## Poor-link host path (2026-11-09, LOCAL, unpushed, untested on a GPU / the field link)

- **Wire pacer** (`src-tauri/src/host/handlers/pacer.rs`, in the media-over-session video
  forwarder, every encoder): one token bucket for video, FEC parity and NACK retransmits
  (retransmits jump queued video; parity right after its frame; a NACK for a still-queued seq
  is not duplicated). Rate = 2 × live target × (1 + FEC ratio), ≥ 2 Mbit, ≥ 1.25 × intake;
  16 ms burst; 50 ms queue-delay deadline; 250 ms stale drop. Target atomic refreshed at the
  top of every `StreamReq`. `PULSAR_MOS_PACE=0` = pass-through. 2 s log line "mos video
  forwarder throughput" (per-class pkts/bytes, nack_*, pace_kbps, max_qdelay_ms, drops).
- `pulsar-capture/src/rtp.rs` `RtpSender`: packet-granular bucket (same model, AU out within
  1.5 frame intervals); **off by default toward loopback** (the forwarder paces), on toward
  remote; `PULSAR_RTP_PACE=0/1` forces. Windows timers are ~15.6 ms (no `timeBeginPeriod`).
- **FEC v2** (`host.rs` + core `FecPolicy`): pre-repair loss sizing, 10–50 %, WAN starts at
  15 % for 30 s; counters from `MediaSendStats` on the "client stats" line.
- **NVENC** (`encode/policy.rs`, `encoder.rs`, `nvenc.rs`): RFI DPB per level 5.1 (16/12/8),
  `numRefL0 = 1`, RFI off if caps 28/47 report 0; `RefTracker` decides invalidate vs decline
  (never forces an IDR; the client's keyframe request heals). Low-budget (< 0.1 bpp/frame):
  `lowDelayKeyFrameScale = 1`, one IDR per fresh encoder. Quarter-res two-pass only on a THIN
  stream (< 0.04 bpp/frame, ≤ 1920×1200×120 px/s; `TWO_PASS_BPP`): it used to follow the
  low-budget line, and the remote-desktop ceiling (~0.06 bpp) kept every LAN remote session on
  it — the first pass held the host on 84 % of the GPU's 3D engine at 1200p120 (0.7 % without;
  measured 2026-11-13, LOCAL, unpushed). Both classes re-judged live on bitrate changes
  (×/÷1.25 hysteresis); `PULSAR_NVENC_MULTIPASS=0/1`. Preset (`policy::preset_level`,
  `PULSAR_NVENC_PRESET=1..4`): game P1 + ULTRA_LOW_LATENCY; remote LOW_LATENCY with P4 up to
  1920×1200×60 px/s and P1 above (encoder utilisation at 1200p120: P4 55 %, P3 47 %, P2 39 %,
  P1 20 %; P4 is 0.03 dB PSNR ahead of P1 there).
  Fixed binding layout bugs vs SDK 12.2 (VUI pad, AV1 flag bits,
  `NV_ENC_CONFIG` = 3584 B, offset asserts). `mf.rs` answers RFI with Err. Rate/IDR log:
  `C:\Users\Public\pulsar-capture-dbg.txt`.

## Windows: an NVIDIA driver that stops answering (2026-11-13, LOCAL, unpushed)

Seen on a hybrid laptop host: `D3D11CreateDevice` on the NVIDIA adapter blocked inside
`D3DKMTDestroyHwQueue` for 75 min and for 5 h 50 min — uninterruptibly (the process cannot be
killed), each time exactly while another program's process hung in the same driver. A stream
that was already running kept running; a NEW native capture built and never delivered a frame
(or never finished building), and the bundled ffmpeg's QSV probes hung the same way. Nothing
may wait on that driver:

- `process.rs` `nv`: NVENC's state. The codec list is asked in a child (`pulsar
  --probe-nvenc`) with an 8 s deadline; a late answer still counts. A stream reports a stall
  (`nvenc_report_stall`) → NVENC is off (`nvenc_codecs_now()` empty, so every encoder pick
  degrades) until the probe child sent then answers. An answer that took > 3 s (a healthy
  probe: ~0.5 s) means the driver was stuck and is back → `on_nvenc_back` (and
  `nvenc_back_epoch()` moves on). An answer at once means the driver was not the cause: NVENC
  is on again, and only the session that stalled keeps off it (`native_stalled_at`, until the
  epoch moves on or the session ends) — no global lock-out, no bounce back into the capture
  that just failed.
- `host/failover.rs` + `make_on_stream`: a stall is a fresh native capture without one frame
  after 5 s, a running one that encodes nothing for 8 s (the 2 Hz stats reporter watches the
  count; 8 s because a DXGI re-init after a mode change retries ~5 s without encoding), a
  capture thread that cannot be stopped within 3 s, or a build that misses its 3 s init. The
  stream then restarts on the next encoder (AMF / Media Foundation / software) through the
  session's restream channel with its `force` flag (a forced request is never absorbed as a
  bitrate step). When the driver is back, the sessions on a fallback encoder are restarted
  the same way and return to the native path.
- A client is told the encoder families before the stream and drops the codecs its decoder
  cannot take from them (`incompatible_with`), so a session is never moved onto NVENC unless
  its caps listed it (`nvenc_told`). NVENC therefore stays LISTED while it is merely not
  answering — also at startup, from the last answer of an earlier run
  (`%LOCALAPPDATA%\dev.pulsar.app\nvenc-codecs.txt`).
- `pulsar-capture`: `start_capture_encode` no longer joins a thread that missed its init
  (stuck in the driver, the join held the host's stream handler); a build that returns after
  its stop was asked leaves without sending a packet.
- QSV is probed only when an Intel GPU exists (`pulsar_capture::gpu_vendors()`).
- ffmpeg CLI path: the remote GOP follows the bitrate (0.5 s from 8 Mbit/s, 2 s at 2 Mbit/s
  and below — pulsar-core `remote_idr_gop`): the CLI encoder cannot be asked for a keyframe,
  so the GOP is what bounds "recovering" after an unrepaired loss.
- Tested on the home PC by suspending the host's capture thread(s) and the probe child
  (a stand-in for the driver; a real wedge has not been caught since the change).

## Screen adaptation — host display mode per platform (2026-11-14, LOCAL branch `parity/display-mode`)

A client split pane sends `StreamReq::adapt = Some((w, h))`; the host switches the captured
monitor to the best-fit mode and restores it on teardown / `adapt: None`
(`src-tauri/src/display_mode.rs`, driven from `host/handlers.rs::apply_screen_adaptation` and
`SessionCleanupGuard::drop`). The best-fit choice is shared:
`crates/pulsar-capture/src/display_mode/mod.rs::plan` (aspect first, then area, ≥1080 lines,
never past the current size; keeps the refresh rate and the HiDPI scale).

| Platform | How | Crash safety | Hardware |
| --- | --- | --- | --- |
| Windows | `EnumDisplaySettingsW` / `ChangeDisplaySettingsExW` | none (mode stays after a crash) | tested earlier |
| Linux X11 | `xrandr --current` parsed (`display_mode/xrandr.rs`), `xrandr --output --mode --rate --pos` | journal `$XDG_RUNTIME_DIR/pulsar/display-mode.json`, `recover_stale()` at start | **untested** |
| Linux Wayland, wlroots (Sway, Hyprland, labwc, river, Wayfire) | `wlr-randr --json` / `--output --mode --pos` (wlr-output-management) | same journal | **untested** |
| Linux Wayland, KDE | `kscreen-doctor -j` / `output.<n>.mode.<id>` | same journal | **untested** |
| Linux Wayland, GNOME | Mutter `org.gnome.Mutter.DisplayConfig` D-Bus (zbus): `GetCurrentState` + `ApplyMonitorsConfig` method 1 (temporary, not saved to monitors.xml); scale kept when the new mode supports it (`display_mode/linux/gnome.rs`) | same journal | **untested** |
| Linux Wayland, other compositors (weston, gamescope, cosmic without wlr-output-management…) | **not possible**: Wayland has no standard protocol for a client to change output modes; the compositor offers no API | — | — |
| macOS | CoreGraphics `CGDisplayCopyAllDisplayModes` (+ duplicate low-res modes) → `CGConfigureDisplayWithDisplayMode`, completed `kCGConfigureForAppOnly` (`pulsar-capture/src/display_mode/macos.rs`) | the window server restores the mode itself when Pulsar exits, even on a crash | **untested** (no Mac) |

Wayland limit: the ScreenCast portal does not say which output it shares, so on Wayland the
switch is only made when exactly ONE output is enabled; with several nothing changes.
Mirrored GNOME logical monitors are refused. macOS display index = `CGGetActiveDisplayList`
order. Per-window fit (`fit_window`, resizing one host app window to the pane) works on
Windows, Linux X11 and macOS since the `parity/window-capture` merge (see "Per-window
capture"); not on Wayland.

### Client display-scale watch

* Windows: `dpi_watch.rs` (WebView2 RasterizationScale vs. the window DPI).
* Linux: `scale_watch.rs` — tao never emits `ScaleFactorChanged` on GTK, so the main window's
  GTK `scale-factor` notify is hooked after every main-window build (`ui_start.rs`); a change
  updates `process::set_ui_scale` and sends `uiscale <f>` to each live `pulsar-render`, whose
  egui overlay re-scales (`crates/pulsar-render/src/linux.rs`). WebKitGTK follows GTK itself.
  GTK3's scale is an integer (fractional desktop scales show as the rounded-up integer).
* macOS: tao's `ScaleFactorChanged` (NSWindowDidChangeBackingProperties) plus a
  `CGDisplayRegisterReconfigurationCallback` re-read of the window scale; WKWebView and the
  eframe renderer follow by themselves.

Untested on Linux and macOS hardware.

## HDR10 end to end (2026-11-15, LOCAL branch `feat/hdr`, untested in a real session)

The only HDR source is a host monitor in HDR mode (Windows "Use HDR": output colour space
BT.2020 / PQ). No 8-bit picture is ever labelled PQ any more (pulsar-core
`StreamPlan::effective_hdr_yuv444`: every ffmpeg capture is 8-bit SDR, so an HDR request there
streams plain BT.709 — before, AMF / software / VA-API re-quantised 8-bit BGRA to P010 and wrote
BT.2020 + PQ into the VUI: wrong colours on every client).

Negotiation: the host advertises `FEAT_HDR` (Windows with an NVENC that has HEVC or AV1) and
`DisplayInfo::hdr` per monitor; the client sets **`StreamReq::hdr10`** only when the Settings
switch is on, the host has `FEAT_HDR`, the codec is HEVC / AV1 and its renderer probe says
`ten_bit` for THAT codec (`play.rs::HdrNeg::want`, re-decided in `hold.rs` on every codec
change; `PULSAR_HDR=1` forces). The host reads only `hdr10` for real HDR: the legacy
`StreamReq::hdr` bit is what old desktop clients and the mobile app send from a setting alone,
with no capability check, and an old client given Main10 PQ shows black (Linux) or washed-out
(Windows) video. New clients send `hdr = false`. The host still decides per build: an SDR
monitor, H.264, the hybrid cross-adapter bridge or the Media Foundation encoder stream SDR.
`DisplayInfo::hdr` is informational (logged by the client), not a gate: it is a caps-time
snapshot, and the host falls back to SDR on its own, so asking costs nothing.

While the native capture streams HDR10 the host sends `DataMsg::HdrMeta { sdr_white_nits,
peak_nits }` every 2 s (`LiveStats::hdr_reference`: Windows "SDR content brightness" re-read by
GDI device name, and the monitor's peak). Clients tone-map around that SDR white instead of a
fixed 203 nits (see client-video.md "HDR10").

Release order (the desktop branch builds only against the pulsar-core branch): push core
(`StreamReq::hdr10`, `DataMsg::HdrMeta`, `FEAT_HDR`, `DisplayInfo::hdr`) first, then
`cargo update -p pulsar-core` here and commit that `Cargo.lock` bump with the desktop push.
pulsar-mobile builds `StreamReq` as a struct literal: its core bump must add `hdr10: false`
(mobile has no 10-bit decode check yet, so it must not ask for HDR10).

| Host | HDR capture + encode | Why / status |
| --- | --- | --- |
| Windows, NVENC, monitor on the NVIDIA GPU (DXGI) or WGC on the NVIDIA device | `DuplicateOutput1(R16G16B16A16_FLOAT)` / WGC `R16G16B16A16Float` (scRGB) → `encode/hdr_conv.rs` (2 pixel-shader passes: BT.709→BT.2020, ×80/10000, PQ, BT.2020 NCL Y'CbCr, limited range → P010 planes; scale + rotation) → NVENC HEVC Main10 / AV1 10-bit (`set_hdr10`: bit depth 10, VUI 9/16/9) → mastering display + content light level on every keyframe (`hdr.rs`: HEVC SEI 137/144, AV1 metadata OBUs, from `DXGI_OUTPUT_DESC1`). Cursor composited linear × SDR white level / 80. Native → encode downscale box-filters each output pixel's footprint in linear light (bilinear taps on ~2×2-texel cells, ≤ 4×4; thin lines do not alias at a 4K → 1080p encode). HDR on/off mid-stream rebuilds capture + encoder: DXGI on the ACCESS_LOST reinit (Reinit and Error arms compare the duplication format), WGC by a ~1 s re-check of the item's monitor (`dxgi/hdr.rs::HdrWatch`: cached factory, re-walked only when `IsCurrent()` is false or the window changed monitor), and the capture thread rebuilds on any frame whose format does not match the encoder (FP16 ⇄ BGRA). Failed HDR build → SDR on the same path. Stats label says `HDR10`. | Verified headless on the RTX 3080 laptop host (ignored test `hdr_nvenc_encodes_main10_with_metadata`): ffmpeg reads `hevc (Main 10), yuv420p10le(tv, bt2020nc/bt2020/smpte2084)` + MDCV/CLL side data. GPU conversion checked against the CPU reference on Adreno. **No real HDR monitor captured yet** (the host's monitor state could not be read from the SSH session). |
| Windows, hybrid (monitor on the iGPU, DXGI picked) | SDR | the cross-adapter bridge stages 8-bit BGRA through system memory; `path.rs` may pick WGC (then HDR works). Not forced, to stay out of the path measurement. |
| Windows, AMD / Intel / Snapdragon only (Media Foundation / AMF / QSV via ffmpeg) | SDR | `ddagrab` runs with `output_fmt=bgra`; a 10-bit ddagrab (`x2bgr10`) + a matching swscale BT.2020 conversion + AMF/QSV Main10 would be the way — not built, no hardware to verify. |
| macOS (ScreenCaptureKit → VideoToolbox) | SDR | SCK is configured for 8-bit NV12; HDR needs macOS 15 `SCStreamConfiguration.captureDynamicRange` + a 10-bit pixel format + VT HEVC Main10 with PQ attachments. Not built (no Mac). |
| Linux (x11grab / PipeWire) | SDR | X11 has no HDR; the ScreenCast portal / PipeWire has no HDR format negotiation yet (KDE's HDR screencast is experimental). Not possible today. |

Knobs: `PULSAR_HOST_HDR=0` treats every monitor as SDR.

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
