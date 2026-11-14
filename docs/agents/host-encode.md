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
    are the knobs.
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
  instead). Entered when the resolved encoder is VideoToolbox, codec ≠ AV1, no HDR/4:4:4;
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
  x11grab → libavfilter → libavcodec x264/x265/SVT-AV1/NVENC → libavformat RTP; live
  bitrate, forced IDR on request, short-GOP recovery; `PULSAR_LIBAV_HOST=0` disables).
  Falls back to the ffmpeg CLI below when it cannot start or for VA-API/Vulkan/HDR/4:4:4.
  - **NVENC there goes through the NVENC SDK** (2026-11-14, untested on Linux hardware):
    `pulsar_capture::NvencCuda` (`crates/pulsar-capture/src/encode/cuda_enc.rs`) instead of
    libavcodec's `h264_nvenc`. See "Linux NVENC SDK" below.
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
| macOS | `CGGetActiveDisplayList` (main first) + pixel mode + `CGDisplayCopyAllDisplayModes`; names from `NSScreen.localizedName` (cached, refreshed on the main queue) | avfoundation `Capture screen N` (`process::avfoundation_screen`; ffmpeg numbers screens in the same list order); absolute pointer goes to that display's `CGDirectDisplayID` bounds (`DesktopInput::set_display`; the id is re-resolved on an index change, a display-change generation change and at least every second). The host-pointer marker maps the pointer into the same display (`displays::pointer_in`), but macOS has no host-takeover detection yet, so it is not sent | `CGDisplayRegisterReconfigurationCallback` (registered on the main queue) |

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
| Windows | `pulsar_capture::list_capture_windows` (HWND) | WGC → NVENC (native path); a non-NVENC/HDR/4:4:4 stream falls back to the monitor | shipped |
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

- `pulsar-capture` builds on Linux too (only the encoder part; capture stays Windows-only).
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
- Which clients use it: the Windows client asks for RFI (`play.rs` `host_rfi` is
  `cfg!(windows)`). The Linux renderer already reports `vidsink-rfi` and resumes at the tag, but
  the Linux client does not ask yet.
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
order. Per-window fit (`fit_window`, resizing one host app window to the pane) is still
Windows-only.

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
