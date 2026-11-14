# Status — complete vs scaffolded

Part of the pulsar-desktop agent docs — index: [`CLAUDE.md`](../../CLAUDE.md).

## What's complete vs. scaffolded

**Complete + tested:** relay protocol + heartbeats (10 s; relay evicts at 30 s),
register→ID→P2P→relay-fallback + relay-down survival, E2E crypto, one-time-password
auth (`unattended_access` skips), Approve/view-only consent flow, controller
detection/normalization + virtual pads (uinput + ViGEmBus), capture/encode paths
above, native render + overlay, **side channels — clipboard, file transfer (+
file browser `fs_browse.rs` + Files window), chat, mic, voice call**
(`Session/sidechannels.svelte.ts`), multi-session **tabs + split view**
(`sessions.svelte.ts`), 4-language i18n, LAN discovery UI (`Home/LanDevices`),
Games library + folder scan, gamepad-driven UI nav (`gamepadNav.svelte.ts`),
auto-updater, relay mode, VNC-mode client path (`io_cmds.rs`), the SvelteKit UI
and the Tauri bridge.

**Sleep / display-off ends the viewed sessions, wake reconnects (2026-11-14):** Windows,
Linux (logind + screen saver + X11 DPMS) and macOS (NSWorkspace) — all untested on real
hardware; Linux/macOS details and the one gap (display-off on bare wlroots Wayland) in
[modes.md](modes.md) "Sleep ends the viewed sessions".

**Host monitor picker parity (2026-11-14, branch `parity/host-displays`, untested on macOS and
Wayland hardware):** macOS and Linux Wayland now advertise the host's monitors and watch for
display changes like Windows does — see host-encode.md "Host monitor picker". On Wayland the
portal dialog still picks the monitor; the client's picker follows what it picked
(`DataMsg::StreamedDisplay`). The macOS host-pointer marker is positioned on the streamed display
and sent since the `parity/host-takeover` merge (macOS takeover detection).

**Same-host co-op per-window input (2026-11-14, branch parity/coop-window-input):**
`WindowInput` now exists on Windows (PostMessage), Linux X11/XWayland (SendEvent/XTest) and
macOS (CGEventPostToPid); routing in `host.rs` is cross-platform. Since the `parity/all`
merge the per-window capture SOURCE off Windows is `parity/window-capture`'s (`window_src`:
X11 `x11grab -window_id` / libav / gst `ximagesrc xid=`, macOS ScreenCaptureKit window →
rawvideo); `make_on_stream` publishes the streamed window's id (X11 XID / `CGWindowID`) to
`win_capture_hwnd` + `coop::set_capture`, so the per-window injector runs end to end on X11
AND macOS. The co-op branch's own X11 capture plumbing (`StreamPlan.window_id`, its
`x11_window` block) was folded into that path; `pulsar_core::x11win` stays for its pid-tree
helpers. Native Wayland is impossible. Untested on Linux and macOS hardware — see
[modes.md](modes.md#same-host-co-op-per-window-input-all-desktop-oses-2026-11-14).

**Adaptive streaming (shipped 2026-09-03 in v0.11.0, refined in v0.12.0 — still awaiting the
maintainer's real-session test):** the shared
controller is `pulsar_core::adapt` (design + status in `../pulsar-core/docs/adaptive-streaming.md`).
Here: `src-tauri/src/play/hold.rs` measures (loss, 500 ms pings, per-frame arrivals, FEC
parity) and actuates (`StreamReq` bitrate / resolution / fps / `loss_recovery` / `fec`);
`adapt_memory.rs` remembers the last good rate per peer; the host (`host.rs`, `host/handlers.rs`)
sizes FEC parity from the client's `Stats` and encodes with intra-refresh / short GOP on request;
`pulsar-render` holds the last good frame on an unrepaired loss (`hold`), follows the RTT-derived
reorder wait (`maxdelay`), and on Windows/macOS has a reorder buffer. Test with
`scripts/netem.sh` + `scripts/validate-log.mjs`. Do not push behaviour changes untested.

**macOS client video (2026-11-14, never run on a Mac):** `pulsar-render` has a real macOS
backend now (VideoToolbox + Metal in a panel kept over the session tab, MetalFX spatial /
FSR upscaling, overlay, pacing, device-loss rebuild — see [client-video.md](client-video.md) "macOS renderer"); mpv is only the
fallback. Type-checked for `aarch64-apple-darwin` only; `src-tauri`'s macOS code could not be
type-checked here.

**macOS client input capture (2026-11-14, untested on macOS hardware):** CGEventTap in
`crates/pulsar-macinput` + `kbdhook/mac.rs`: system keys and combos to the host, relative
mouse (game mode / host pointer lock), absolute mouse confined to the video, free cursor,
click-to-engage, focus-loss release, held-key flush; Accessibility prompt + toast, webview
fallback without it. Details and the open hardware checks: modes.md "macOS input capture".
`pulsar-tauri` itself can't be type-checked for macOS from Windows (objc2-exception-helper
needs a mac C toolchain); the crate can.

**Linux host RFI (2026-11-14, LOCAL, unpushed, untested on Linux hardware):** an NVIDIA
Linux host (X11 and Wayland) encodes with the NVENC SDK on CUDA and heals a client-reported
loss with reference-frame invalidation like the Windows host — see
[host-encode.md](host-encode.md) "Linux NVENC SDK". Open: the Linux client does not ask for
RFI yet (`play.rs` `cfg!(windows)`); VA-API / QSV / AMF hosts have no equivalent (keyframe).

**macOS host virtual gamepad (2026-11-14, untested on macOS hardware):** pulsar-core
`input/mac_hid.rs` creates an `IOHIDUserDevice` virtual HID pad (Xbox target = Xbox Wireless
over Bluetooth 045E:0B13, DS4 target = DualShock 4 USB 054C:09CC) with rumble back to the
client, like ViGEm/uinput. It needs the restricted entitlement
`com.apple.developer.hid.virtual.device` (Developer ID + provisioning profile;
`src-tauri/entitlements/virtual-hid.plist`, used by release.yml only when the `APPLE_*`
secrets exist — none are set today, so released Mac builds fall back). Any host that cannot
create a pad (Mac without the entitlement, Windows without ViGEmBus, Linux without
`/dev/uinput`) now sends `DataMsg::PadUnavailable`: the client shows "Oyuncu N kolu karşı
tarafta çalışmıyor: …" in-session (`pad_notice.rs` → `play_notice`), the host a `pad-off`
toast. Whether GameController / SDL accept a virtual (non-USB/BT) device with these VID/PIDs
is unverified.

**Scaffolded / known gaps:** HW encode on
the Wayland/gst path (software x264 only); media-over-the-session for
symmetric-NAT (media is a direct UDP RTP flow today — fine for LAN/cone-NAT/
relay-direct). The Windows↔Windows black-screen work (probe-validated caps, encoder
fallback chain, host verdict → client banner, Linux decoder fallback, daily log file) is
merged (2026-09-02); AMD-on-Windows, macOS and Windows ARM64 are CI-built but **never run
on hardware** — read [host-encode.md](host-encode.md) before touching encoder/decoder
selection.

**Picture upscaling parity (2026-11-14, untested on hardware):** Windows = NVIDIA/Intel VSR,
FSR, SGSR; Linux pulsar-render = FSR, SGSR (`gl_fsr.rs` + `sgsr1.glsl`; not on the KMS lease
path, which bypasses GL); macOS pulsar-render = MetalFX spatial / FSR; mpv (macOS / Linux
fallback) = FSR. SGSR is hidden in the settings on
every OS (`PULSAR_UPSCALE=sgsr` reaches it) until the 2025-math fix is A/B'd — see
[client-video.md](client-video.md#picture-upscaling-settings--görüntü--görüntü-iyileştirme).

**Per-window host capture on Linux + macOS (2026-11-14, branch `parity/window-capture`,
untested on Linux X11 / Wayland / macOS hardware):** the "Pencere" pick and game-mode
launched-window capture, until now Windows-only (WGC), on X11 (`x11grab -window_id` /
`ximagesrc xid=`), Wayland (portal WINDOW source — the host person picks) and macOS
(ScreenCaptureKit window → rawvideo → ffmpeg). Per-OS table + limits:
[host-encode.md](host-encode.md#per-window-capture-pencere-phase-2b-co-op--per-os). Needs the
matching pulsar-core branch. X11 + macOS also map the pointer into the window, raise it,
restart the stream when it is resized / closed, and fit it to a split pane (macOS: needs the
Accessibility grant). Wayland can do none of those (no window positions for programs).

**macOS native host (2026-11-14, LOCAL branch `parity/mac-host-capture-audio`, never run on a
Mac — only `cargo check --target aarch64-apple-darwin` of pulsar-capture and pulsar-core; the
src-tauri wiring is reviewed by hand, it cannot be checked off a Mac):** ScreenCaptureKit →
VideoToolbox → RTP with live bitrate / keyframe / display switch (see host-encode.md), and the
system mix without Pulsar via ScreenCaptureKit audio (see audio.md); Screen Recording
permission notice on the home screen; the absolute pointer and the ffmpeg fallback follow the
picked display. Not possible on macOS: RFI and intra refresh (VideoToolbox has no API),
surround system audio (SCK gives at most stereo). Possible but not done: the ghost-pointer
region. Since the `parity/all` merge, per-window capture comes from `parity/window-capture`
(SCK window → rawvideo → ffmpeg, not the in-process VideoToolbox path), and the absolute
pointer follows each session's own display (`DesktopInput::set_display`, `parity/host-displays`);
the process-wide published display is only the fallback when a session set none.

**Pen + multi-touch (2026-11-14, branch `parity/pen-touch`, untested on pen / touch
hardware on every platform):** `InputEvent::Pen` / `InputEvent::Touch` (state messages, see
`pulsar-core` `input/pen_touch/`), gated by the host features `pen` / `touch`; an older host
gets the same input as mouse (tap = click, drag, two-finger scroll; pen tip = left, tip with
the barrel button = right) from `ClientPenTouch` in `play/hold.rs`, which also re-sends the
held state every 250 ms (the host lifts after 1 s without one) and lifts after 30 s without
new input from the source (2.5 s for a pen that only hovers, so a lost `pointerleave` does not
pin the host cursor). A Windows / Linux / macOS host in takeover (the host user on their own
mouse) holds a hovering pen back like `PointerMotion`; only real lifts (contact end of a pen
that was down, leaving range, the last finger up) pass (`host.rs`, per-session `pen_down`).

| | Windows | Linux | macOS |
| --- | --- | --- | --- |
| Client, remote mode | renderer `WM_POINTER` → `pen` / `touch` stdout lines (`pulsar-render/src/win/pointer.rs`, only while engaged + overlay closed + host takes them; `render_stats.rs` parses) | webview canvas PointerEvents (`Session/input.svelte.ts`) + native tablet attributes from an evdev reader of every `BTN_TOOL_PEN` node (not grabbed; needs `/dev/input` read access) | webview canvas PointerEvents + native tablet attributes from a listen-only CGEventTap (tablet events aimed at the app's pid) — WKWebView reports a Wacom pen as `mouse` |
| Client, game mode | same renderer path | canvas, forwarded without "controlling" (the evdev grab takes keyboard + mouse only) | canvas, same as Linux |
| Host, pen | synthetic `PT_PEN` pointer device (pressure, tilt, rotation, eraser, barrel, hover); through the SYSTEM helper when it runs (UAC / lock screen reachable) | uinput tablet "Pulsar Virtual Pen" | CGEvent tablet proximity + tablet-point mouse events, on the streamed display |
| Host, touch | synthetic `PT_TOUCH` device, 10 contacts; through the SYSTEM helper when it runs | uinput multitouch type B "Pulsar Virtual Touchscreen", 10 slots | **mapped to mouse + pixel scroll** — macOS has no public touch-injection API (no touchscreen input path; trackpad gestures are private IOHIDEvents), so a touch frame's pinch / rotate cannot be injected as touch (the touchpad pinch, `InputEvent::Pinch`, goes through the private magnify gesture instead — see "Touchpad pinch") |

Notes per platform:

- **Windows SYSTEM helper**: `pulsar-sysagent` protocol v2 adds `Request::Pointer` (one
  synthetic pen / touch frame in physical pixels). The host keeps running the pen / touch
  driver (`WinSink<SysBackend>` in `host/sysinput.rs`) and the helper injects the frames on its
  own `PT_PEN` / `PT_TOUCH` devices on the input desktop, recreated after a desktop switch,
  lifted on `ReleaseAll`, on disconnect and after 1 s of silence. A v2 helper serves v1 hosts;
  a v2 host that a v1 helper rejects reconnects as v1 and keeps pen / touch in-process.
  Whether a SYSTEM process may create synthetic pointer devices on Winlogon is unverified.
- **Windows client**: the mouse hook leaves pen/touch-synthesized mouse (`MI_WP_SIGNATURE` in
  `dwExtraInfo`) alone while the FOCUSED pane's host takes pen/touch; each play keeps its own
  host features (`PlaySession::host_pen_touch`, the renderer's `pentouch` line at spawn and
  respawn). The renderer needs no inverse rotation: the presented rect is upright and the host
  injects in logical coordinates, the same as the mouse path. Frames with more than 16
  contacts are re-read with a larger buffer.
- **macOS / Linux client**: `pulsar-core` `pen_touch::tablet::TabletAugment` (in `hold.rs`, only
  for a `pen` host) turns the webview's absolute mouse + left button into `InputEvent::Pen`
  while a native sample at most 500 ms old says a pen is in range, carrying its pressure,
  tilt, twist, eraser and barrel; a webview that reports real pens passes through untouched.
  The eraser end stays sticky while a pen hovers (`#penEraser`). Unverified: that macOS lets a
  listen-only tap see tablet events without a TCC prompt (if refused, pens stay a mouse), and
  which pointerType WebKitGTK gives.
- **macOS host**: absolute pointer, pen and touch land on the display behind the avfoundation
  device the host captures (`process::macos_streamed_display` → `avf_screens`), not on the
  main display.
- **Linux host (Wayland, multi-monitor)**: assumption, unverified — Xorg and wlroots map the
  direct uinput devices over the whole layout (the streamed-monitor region applies); GNOME and
  KDE Wayland map them to one output, so the region is skipped there. Explicit association
  (KWin `outputName`, Mutter `output` gsettings for `5055:0001` / `5055:0002`) is not done.

Tests: core `input/pen_touch` (driver, slot tracker, mouse mapping, client gate incl. the
hover stale limit, Windows pen flag steps + contact fields, Linux evdev reports + region policy,
macOS button choice, tablet augment + evdev fold), core `input/avf_screens`, `pulsar-sysagent`
`proto` (v2 pointer frames, validation) + `inject` (lift tracking), `Session/input.svelte.test.ts`,
`render_stats.rs` `pen_touch_tests`,
`kbdhook/imp.rs` `pen_and_touch_synthesized_mouse_is_recognized` (Windows; the app's lib-test
binary does not start in this Windows dev setup — STATUS_ENTRYPOINT_NOT_FOUND — so it ran
only as a type-check), `pulsar-render` `win/pointer.rs`.

**Shared (machine-wide) mode on Linux + macOS (2026-11-14, untested on Linux and macOS
hardware):** `crates/pulsar-machine` + the Unix branches of `shared_mode.rs` — see
[architecture.md](architecture.md) "Linux / macOS". Unit tests for every rule (dir checks,
group pick, script text, exit-code mapping, per-user paths); `flock` lease + `harden` tested on
Linux (WSL); the setup script ran end-to-end as root in WSL on a scratch dir. The deb/rpm
post-install default-ON block is unit-tested for its skip cases (decided / existing data) and
parses in `sh`; **never run** inside a real `dpkg -i` / `rpm -i`. **Never run:**
the real `pkexec` / macOS administrator prompt, the toggle + restart in the app, two users'
Pulsar handing the lease over (logind `Active`, CGSession on-console), macOS at all (only
`cargo check --target aarch64-apple-darwin -p pulsar-machine`; `pulsar-tauri` can't be
mac-checked from Windows).

**Host-only / headless host / Pulsar Split seat host (2026-11-13, not run on hardware):**
`--host-only`, `--headless-host`, `--headless-stop`, `--split-seat` — see
[modes.md](modes.md) "Host-only and headless host". Unit-tested (argv parsing, pipe protocol
1 over an in-memory pipe, `config.apply` mapping); smoke-run for argv errors, the
coexistence exit and the missing-pipe exit. **Never run** against the real Pulsar Split
service or as SYSTEM in a seat session. The seat host is the smallest seam (a window-less
Tauri app keeps the `AppHandle`), not the `HostEnv` extraction of the contract's §6 item 1.
Still open from that §6 list: a separate `pulsar-host.exe` artefact + its SHA-256 for the
Split manifest (19; today it is `pulsar.exe --split-seat`, so firewall/installer lists are
unchanged), in-process SYSTEM input injection following the input desktop (10), app launch
via the service (9, `apps.set`/`app.launch`), display modes via the service (12), pinned
audio endpoint (13), split-pads channel instead of ViGEm/USB-IP pads (14 — pads are still
created by pulsar-core in seat mode), per-seat capture/encoder in `status`,
`clients.*`, live `log.level`, `trust.revoke` ending that device's live sessions, `access:
approve`, Zeroize of parsed secrets (frames are wiped; parsed `String`s are not).
Review fixes (same day): the headless host refuses to start next to an **elevated** GUI
(access-denied on the singleton mutex = running), the seat pipe reader is no longer
cancelled by events (split frames stayed in sync only by luck), `--host-only` ignores
`reverse-request`, the caps cache goes to the seat `--data-dir`, Unix stop signals use a
self-pipe instead of an inherited blocked mask, and the stop event grants the same user
`EVENT_MODIFY_STATE` with a Medium label (`--headless-stop` exit 7 = access denied).
Unverified: the elevated-GUI / elevated-host cases (needs an elevated run), the Unix signal
path (not compiled on Linux here), and `--headless-host` on Linux needs a display.

**Touchpad pinch (2026-11-14 parity work; Linux + macOS untested on hardware):** one
shared client module, `src-tauri/src/pinch.rs` — the host-pinch flag (`FEAT_PINCH`, set by
play.rs), `PinchForwarder` (gesture → `InputEvent::Pinch` for hosts that inject it, else
Ctrl + wheel steps at 1.12× per notch) and `TouchTracker` (two-finger pinch recognition from
raw multitouch). Per client platform:
- **Windows** — the LL mouse hook turns Windows' synthesized Ctrl + wheel into the pinch
  (`kbdhook/imp.rs`, unchanged; it reads the shared flag now).
- **Linux** — `kbdhook/linux_pinch.rs`: one reader thread per capture generation opens
  every touchpad READ-ONLY (no grab — the compositor keeps it) and forwards a recognised
  pinch while the session is controlled (`input_active`). Needs read access to
  `/dev/input` (the same `input` group the keyboard/mouse grab needs). Touchpad pointer
  motion itself is still not forwarded by the evdev capture (only keyboards and REL mice are
  grabbed) — a separate gap.
- **macOS / any webview session** — `Session/input.svelte.ts`: WKWebView `gesturestart /
  gesturechange / gestureend` on the video canvas, and a Ctrl + wheel with no Ctrl key held
  (Chromium / WebKit report a pinch that way; idle 200 ms = end) → `input_pinch` command
  (`io_cmds.rs`). If a macOS native renderer ever covers the webview, the renderer must read
  `NSEventTypeMagnify` itself and relay it (not done — no such renderer path yet).
Hosts: Windows, Linux and macOS all advertise `FEAT_PINCH` now (`host.rs media_features`);
the injection is pulsar-core `DesktopInput::pinch` (see `../pulsar-core/docs/agents/modules.md`).

**Screen adaptation + display-scale watch on Linux/macOS (2026-11-14, LOCAL branch
`parity/display-mode`, untested on Linux and macOS hardware).** The host display-mode switch for
a client split pane now exists on Linux (X11 xrandr; Wayland via wlr-randr, kscreen-doctor or
GNOME Mutter's DisplayConfig D-Bus; crash journal restored at the next start) and macOS
(CoreGraphics, app-scoped, auto-restored on exit/crash). Other Wayland compositors cannot do it
(no standard protocol). The client's display-scale change is watched on Linux (GTK
scale-factor → live `uiscale` to the renderer) and macOS (CG reconfiguration callback). Details:
[host-encode.md](host-encode.md#screen-adaptation--host-display-mode-per-platform-2026-11-14-local-branch-paritydisplay-mode).

**Renderer GPU-loss recovery (2026-11-14):** Windows rebuilds a lost D3D device in place
(screen off, driver reset); Linux rebuilds a lost GL context in place (GPU reset, NVIDIA
suspend/resume purge; reset-notification EGL context + `glGetGraphicsResetStatus`, off by
default on the Mali blob driver) and reopens the HW
decoder. Both rate-limit AU-overflow / present-stall logs and take
`PULSAR_TEST_DEVICE_LOSS`. **Linux path untested on Linux hardware** (WSL type-check + unit
tests only). The macOS renderer (`src/mac/`) rebuilds its Metal device too (see the macOS
renderer section of client-video.md).
Details: [client-video.md](client-video.md) "GL context loss on the Linux renderer".

**Host takeover + host pointer marker parity (2026-11-14, not run on hardware):** the
"host user takes the mouse back" detection and the client's "Host" pointer marker now exist on
Windows, Linux (evdev + X11 XInput2 raw events; X11 pointer position) and macOS (listen-only
CGEventTap, Pulsar's injected events tagged via `kCGEventSourceUserData`; main-display pointer
position), and the macOS remote-cursor ghost is a system-wide overlay. Wayland without
`/dev/input` access does not detect it yet (possible via the portal's cursor-metadata mode;
not built). See [modes.md](modes.md) "Host pointer marker".
Linux X11 and macOS paths are **untested on hardware**.
