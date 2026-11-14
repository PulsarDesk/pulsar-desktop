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

**Scaffolded / known gaps:** macOS virtual gamepad (no-op stub); HW encode on
the Wayland/gst path (software x264 only); media-over-the-session for
symmetric-NAT (media is a direct UDP RTP flow today — fine for LAN/cone-NAT/
relay-direct). The Windows↔Windows black-screen work (probe-validated caps, encoder
fallback chain, host verdict → client banner, Linux decoder fallback, daily log file) is
merged (2026-09-02); AMD-on-Windows, macOS and Windows ARM64 are CI-built but **never run
on hardware** — read [host-encode.md](host-encode.md) before touching encoder/decoder
selection.

**Picture upscaling parity (2026-11-14, untested on hardware):** Windows = NVIDIA/Intel VSR,
FSR, SGSR; Linux pulsar-render = FSR, SGSR (`gl_fsr.rs` + `sgsr1.glsl`; not on the KMS lease
path, which bypasses GL); mpv (macOS / Linux fallback) = FSR. SGSR is hidden in the settings on
every OS (`PULSAR_UPSCALE=sgsr` reaches it) until the 2025-math fix is A/B'd — see
[client-video.md](client-video.md#picture-upscaling-settings--görüntü--görüntü-iyileştirme).

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
