# Client video — native renderer, Linux/RK3588, gaming overlay

Part of the pulsar-desktop agent docs — index: [`CLAUDE.md`](../../CLAUDE.md).

## Client video — native renderer everywhere (webview video is GONE)

The old in-webview WebCodecs video path (`h264.ts`) was **removed — too slow**.
Video renders **natively on every platform** via the separate `pulsar-render`
process: Linux rkmpp/EGL zero-copy, Windows MF+D3D11 zero-copy (DXVA), macOS
mpv/VideoToolbox. The egui overlay draws on the video natively. Audio is native
too (`src-tauri/src/native_audio.rs`, libopus + SDL3; see audio.md), so no media
reaches the webview. Don't resurrect a webview video path.

## Linux / RK3588 (Orange Pi 5) renderer — the reality (IMPORTANT)

- **VSync (Linux renderer, `linux.rs`):** the swap interval follows the compositor.
  - With a compositing manager running (owner of `_NET_WM_CM_S<n>`, e.g. GNOME/mutter), the
    interval is 0: the compositor syncs to vblank, so nothing tears.
  - Without one (bare X, e.g. the Orange Pi image's openbox), the interval is 1.
  - Why: on the OPi 5B under mutter, interval 1 left `eglSwapBuffers` waiting forever in
    `xcb_wait_for_special_event` for a Present event that never came. The picture froze while
    decode and audio carried on. The rkvdec2 resets seen in the kernel log were most likely a
    consequence (frames never returned → MPP starved).
  - A swap that blocks for 250 ms or more logs `present stalled N ms`. `PULSAR_VSYNC=0|1`
    overrides the choice.
- **Orange Pi 5B appliance image:** `scripts/opi5-image/` (+ `.github/workflows/opi5-image.yml`,
  manual). It builds ubuntu-rockchip 22.04 server + a Pulsar `.deb` linked against the
  rockchip-multimedia PPA's rkmpp libavcodec, set up as a kiosk. The generic linux-arm64 release
  links Ubuntu's libavcodec, which has no rkmpp, so it decodes in software on the board. The
  image is prepared but not yet built or booted; see the verify checklist in its README.

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
- **Video intake on Linux (Rust RTP path, default):**
  - `play/hold.rs` sends each video RTP datagram to an abstract Unix datagram socket,
    `@pulsar-v-<port>` (`native_view::VideoSink`). It does not use loopback UDP, so the
    packets skip the IP stack and netfilter.
  - `write_sdp` names that socket in the SDP as `a=x-pulsar-unix:`. The renderer binds it.
  - `video.rs` runs `stream::rtp` (the Windows reorder buffer and depacketizer) on that
    socket. It feeds the access units straight into `avcodec_send_packet`, with no
    libavformat.
  - The renderer prints `vidsink-needidr` and `vidsink-rfi` like on Windows, and takes
    `ir` and `rfi` on stdin.
  - If nobody has bound the socket (an mpv fallback consumer), the app falls back to UDP
    per packet.
  - `PULSAR_LINUX_DEMUX=ffmpeg` (app env, inherited by the renderer) restores the old
    path: loopback UDP and libavformat's SDP/RTP demux with its jitter buffer.
- **Control on Linux** = `kbdhook/linux.rs`: grabs local keyboard+mouse via
  **evdev (EVIOCGRAB)**, hotplug-aware. **Leave combo: Ctrl+Shift+Q**; fullscreen **Ctrl+Shift+F11**.
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

### Menu layout and navigation (`overlay.rs`, 2026-11-13, LOCAL, not yet approved by the maintainer)

- **Hub** (`View::Root`): header as tall as a section's strip (`HEADER_H`: brand, mode, the
  transport in a well), stat tiles, the category boxes (`categories(mode)`), end session.
  One visual rule for hub and sections: what is CLICKED is a plate (`ROW_BG` / `ROW_HOVER`,
  cursor = `ROW_SEL` + cyan ring), what is only READ is a well (`SEG_BG`: stat tiles, the
  transport, the well behind side-by-side options). End session is a red plate, dim at rest.
  Two colours, the same on the hub and in a section: **where you are** is teal / cyan
  (`ROW_SEL` under the cursor and on the current category of the strip, the `CYAN` ring = the
  cursor itself); **the value in force** is indigo (`ACCENT_DIM` / `ACCENT`: lit option,
  switch, current tile). The strip's current category used to be indigo — the hub and the
  strip marked "this category" in two colours.
  Stat tiles and category boxes take a compact height on a short surface (`short_surface`).
- **Section**: header = a 44 pt back button + the category strip (`draw_section_header`; the
  current section shows its name, the others an icon; Files is hub-only, it opens its own
  window). The panel's top edge is fixed (`panel_top`) so the header never moves under the
  pointer when the page height changes.
- **Rows** (`Rows`, one call per row): 44 pt high (38 on a surface under 620 pt), the shape
  follows the options — on/off → a switch (a click anywhere on the row); 2-4 options that fit
  → side by side, one click each; anything else → the row opens an **options page** of large
  tiles (`UiState::picker`). Rows taller than the surface scroll (`ScrollArea`, fades at the
  cut edges). The old rows changed their value by clicking the left / right half to step
  through the options — do not bring that back.
- **Keys and pad** (same `k up|down|left|right|go|escape` stdin channel): up/down move the
  cursor; up from the first row reaches the strip (left/right walk the sections, down
  returns); left of the first section is the **back button** (`UiState::back_focus`; A goes
  back one level), and up from the first line of an options page goes straight to it;
  left/right flip a switch at once and STAGE a side-by-side option (amber) until
  A / Enter confirms; A / Enter opens an options page and picks in it; B / Escape goes back
  one level (page → rows → hub → closed). A mouse click hides the cursor ring until the next
  key. Stream-restarting choices still wait for the menu to close (`UiState::deferred`).
- **A click beside the menu** (the scrim) closes it AND takes control: `OverlayCmd::Close` +
  `TakeControl` → `ov close`, `ov control`. The app engages on Windows right in
  `render_stats.rs` (as for `ov engage`) and emits `overlay-control`; the session screen
  takes control there the way its platform's video click does. Escape, the combo and the
  pad close without taking control (the click-to-control hint follows).
- Covered by `overlay::rows_tests` (key-driven, headless). To LOOK at it without a session:
  run the renderer in a test window fed by `ffmpeg testsrc2` and drive it over stdin /
  posted mouse messages (the renderer draws no overlay before its first decoded frame).

## Loss recovery on Windows — reference-frame invalidation (RFI)

When the host advertises `rfi` (native NVENC), the Windows client asks for `StreamReq::rfi`.
The host then keeps a level-5.1-sized DPB (`encode/policy.rs` `rfi_dpb`: 16 at ≤ 1080p,
12 at 1440p, 8 at 4K; one reference for prediction). The flow:

1. The renderer's reorder buffer reports a hole after 3 ms, while NACK repair still runs
   (`vidsink-rfi a b`). hold.rs wakes on it at once and sends `DataMsg::MediaRfi`.
2. The host maps the sequence range to its frames (`RtpEgress` send log) and calls
   `nvEncInvalidateRefFrames`. It tags the next frame with a Pulsar SEI.
3. The reorder buffer stops waiting as soon as the tagged packet arrives. The depacketizer
   drops the frames in between and resumes at the tag.

Waiting out the whole repair window releases a burst of about 24 frames. That burst
overflows the renderer's 8-AU queue and drops the recovery frame.

- **Reach is the host's call** (2026-11-09): the client no longer gates on RTT × fps ≤ 4
  (a 120 fps WAN stream never met it). The host's `RefTracker` declines a loss out of reach,
  a lost IDR, or a failed invalidation (no IDR forced) and marks it unhealed, so a later loss
  is only invalidated together with it; a loss before the newest IDR needs nothing.
- **Fallbacks:** The client asks for a keyframe if no recovery arrives within 150 ms of its
  repair window running out; each new hole while waiting restarts that wait, capped at 1 s
  (`stream/rtp.rs` `RFI_GIVE_UP`).
- **Measured 2026-11-08** (renderer drop 1/600 packets, 1200p120 HEVC, LAN): 0 keyframe
  requests in 45 s with RFI, against 52 in 40 s without it.
- **Knobs:**
  - `PULSAR_RFI=0` (client) turns RFI off.
  - `PULSAR_TEST_DROP=N` (renderer) drops 1 of every N packets.
  - `PULSAR_TEST_DEVICE_LOSS=<after_s>,<failed_attempts>` (Windows renderer) drops the renderer
    once as if its D3D device was removed and fails the first rebuilds (see "Device loss").

## Frame pacing on the Windows renderer ("Kare eşitleme")

Until 2026-11-14 the switch only changed the menu on Windows (and still does on macOS): the
renderer presented every picture the moment it was decoded. Now (`win/mod.rs` `deliver` /
`pace_tick`), with the switch on, decoded pictures wait in `pace_q` and leave ONE per display
refresh, oldest first — the Linux pacer's rule (video.rs `take_frame`, Moonlight's per-vblank
metering): trim toward one picture, keep up to 2 (game) / 3 (remote) while the queue keeps
running dry, hard cap 4, an empty refresh keeps the last picture up, a burst flushes the
queue. The refresh clock is DWM's own counter (`DwmGetCompositionTimingInfo().cRefresh`),
polled every loop pass; without it a picture goes out as soon as it is there. Default off
(`ui.framePacing`), sent at session start (`pace 0|1`); the menu switch also acts at once.

## Device loss on the Windows renderer (screen off, driver reset)

`win/mod.rs` `event_loop` checks `GetDeviceRemovedReason` every pass and rebuilds the whole
D3D stack in place (the app does not respawn the renderer). What went wrong on 2026-11-14 — a
laptop whose screen turned off at night with a session connected — and the rules since:

- **Release the swapchain for real before the new one** (`Renderer::retire`): drop every view
  of the back buffer, the swapchain, then `ClearState` + `Flush`. D3D11 destroys a released
  flip-model swapchain only at a flush; until then `CreateSwapChainForHwnd` on the same
  window returns "Access is denied" — the rebuild failed every time.
- **Retry until it works** (every 500 ms, logged once and then once a minute). The first
  version tried once; a failure left `renderer` at `None`, which the removed-device check
  never looked at again — the renderer stayed dead until the app was killed.
- **Keep pumping window messages without a device.** The renderer's window is a child in the
  app's window tree (shared input queue): an unanswered message hangs the APP's UI thread
  (Windows: "pulsar.exe stopped interacting", hang on `pulsar-render.exe`).
- **Drop arriving AUs without a device**, and report backlog overflows at most once per 2 s
  with counts (`OverflowLog`): one line per frame made 2.6 million lines / 415 MB in a night.
- After a rebuild the picture restarts from a keyframe (`NEED_IDR`).

Try it: `PULSAR_TEST_DEVICE_LOSS=6,8` (see Knobs) in a test window — the window keeps
answering, the log shows "renderer rebuilt after 8 failed attempt(s)", the picture returns.

Still open (not a renderer matter): a connected session keeps the client's audio stream
running, so Windows never enters Modern Standby while one is up (sleep study: "Audio Active"
100 %), and the stream goes on all night with the screen off.

## Poor-link client behaviour (2026-11-09, LOCAL, unpushed, untested on the field link)

All in `src-tauri/src/play/hold.rs` unless noted. Why: a tethered ~10 Mbit / 78 ms link
showed ~600 ms head-of-line holds, "stream stopped" over a live session and a locked RTT.

- **Repair window** (`repair_window_ms`): median of the last 8 RTTs + 2 × their mean
  deviation + 20 ms host slack + the FEC hold, 60–200 ms; sweep every 50 ms. The renderer's reorder wait is
  that + one sweep, capped at 250 ms (`pulsar-render/src/stream/rtp.rs` `MAX_DELAY_CAP_US`;
  `PULSAR_MAXDELAY` pin uncapped). Under >= 10 % pre-repair loss a hole with a whole frame
  behind it is given up after max(2 × RTT, 60 ms) (`rtt <ms> [<jitter ms>]` stdin feeds it; the
  second number is only shown: the hub's latency tile and the HUD read "4 (±0.6)").
- **NACK waits for FEC** while parity flows (`fec_wait_ms`: 2 frame intervals + 8 ms), sent
  early once the block's parity can't cover it (`fec_block_gave_up`); one NACK re-send.
- **Ping/RTT**: Pings never wait for a Pong (lost after 2 s), go out at uneven 350–650 ms gaps
  (`ping_gap_ms`), and `PongMatcher` votes over the last 4 Pongs — with a fixed cadence and no
  ping id, RTT > 500 ms locked onto RTT − 500. Residual ~1–6 % wrong matches; the real fix is a
  ping id (wire change).
- **Liveness**: media counts (`host_silent`: 5 s of nothing, or 15 s without control); the
  renderer no longer declares "stream stopped" itself — hold.rs sends `stopped 1|0` after 5 s
  without video while control lives (`video_stopped`). The renderer shows a "recovering" pill
  while the depacketizer waits for a keyframe/RFI (`win/mod.rs`, `stream/rtp.rs` `WAITING`).
- **ABR inputs**: `Sample.recv_bytes`/`window_ms` = first-copy video payload per window
  (goodput). Soft pins: the first request asks for `ctl.point()`, not the pick; a shortfall
  toast ("the link can't carry your pick") at most once per 30 s (`shortfall_due`).
- **Path changes both ways** (relay to direct, and the core watchdog's direct-to-relay
  fallback): `ctl.path_changed` with `path_cap_kbps` (relay cap only while relayed).
  `adapt_memory.rs` keys the per-peer hint by path class `lan`/`wan`/`relay` (100.64/10 = wan;
  old peer-only entries dropped) — a LAN rate seeding the mobile session started the collapse.

## Picture upscaling (Settings → Görüntü → Görüntü iyileştirme)

`ui.upscale` = off (default) | auto | fsr | ai | sgsr → `set_upscale` → renderer stdin `upscale <mode>`
(seeded with the other renderer lines at spawn / respawn).

- **Windows** (`crates/pulsar-render/src/win/present.rs`, `fsr.rs`): only when the Blt scales
  UP. `ai` / `auto` on NVIDIA or Intel = the driver's own super resolution through the
  VideoProcessor stream extension (NVIDIA RTX VSR, Intel VSR — the Moonlight technique);
  `fsr` or any other GPU = AMD FSR 1: the VideoProcessor converts into a source-sized texture,
  EASU scales it, RCAS sharpens into the back buffer. Shader failure → plain scaling, never
  black. `cargo test -p pulsar-render fsr` compiles the HLSL. `sgsr` = Snapdragon GSR 1
  (`win/sgsr1.hlsl`, one pass) — NOT offered in the settings since the 2026-11-13 A/B (dark
  halos on desktop text, Adreno); reachable via `PULSAR_UPSCALE=sgsr` (diagnostic override of
  the mode, both Windows and Linux) or a saved `upscale=sgsr`.
- **mpv** (macOS, the Linux mpv fallback): `--glsl-shaders=<temp>/pulsar-fsr.glsl`
  (`native_view/FSR.glsl`, AMD FSR 1.0.2 MIT, agyild's port) for any mode but off.
- **Linux pulsar-render** (`crates/pulsar-render/src/gl_fsr.rs`): the presenter's draw (any
  decode path) is redirected into a stream-sized FBO texture, EASU scales it into a second one,
  RCAS sharpens onto the screen; GLSL ES 1.00 (guarded divisions instead of bit-cast
  reciprocals). `sgsr` runs Snapdragon GSR 1 in one pass instead (`crates/pulsar-render/src/
  sgsr1.glsl`, GLSL ES 1.00, bilinear source, `textureGather` emulated with 4 texel-centre
  fetches); if its shader does not build, FSR runs. `auto` / `fsr` / `ai` = FSR (no vendor AI
  upscaler on Linux). The menu's stream line names the one really running (`FSR` / `SGSR`).
  Not on the KMS zero-copy (game-mode lease) path, which bypasses GL: the decoded dmabuf goes
  straight to a plane and the display controller scales it — a shader pass there would need a
  GL composite into a scanout buffer, i.e. no longer zero-copy (by design, not done).
- **SGSR math** (both `sgsr1.glsl` and `win/sgsr1.hlsl`): the 2025 revision of upstream's GLSL
  mobile shader — std = 2.181818 / sum, distance weight 0.55, luma change limited to ±23/255.
  Upstream's HLSL still has the 2023 math (std = (10.14185 / sum)², weight 0.5, no limit), which
  over-sharpens edges; that was the likely cause of the halos. Untested on hardware with the new
  math (Windows and Linux): A/B it on desktop text before offering SGSR in the settings again
  (add `<option value="sgsr">` back in `DisplayTab.svelte`, the native menu row in
  `overlay.rs`, and accept `sgsr` in `Session.svelte`'s `upscale` case). macOS: the mac renderer
  can reuse `sgsr1.glsl` as is (GLSL ES 1.00 body; needs the prelude `gl_fsr.rs` PRE provides:
  `uv`, `tex`, `con0`) or translate it to Metal.
- ABR: with an upscaler on, game mode uses `min_kbps_scale = 1.25` (gives resolution up earlier).
