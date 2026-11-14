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

**Scaffolded / known gaps:** macOS virtual gamepad (no-op stub); HW encode on
the Wayland/gst path (software x264 only); media-over-the-session for
symmetric-NAT (media is a direct UDP RTP flow today — fine for LAN/cone-NAT/
relay-direct). The Windows↔Windows black-screen work (probe-validated caps, encoder
fallback chain, host verdict → client banner, Linux decoder fallback, daily log file) is
merged (2026-09-02); AMD-on-Windows, macOS and Windows ARM64 are CI-built but **never run
on hardware** — read [host-encode.md](host-encode.md) before touching encoder/decoder
selection.

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
| Host, touch | synthetic `PT_TOUCH` device, 10 contacts; through the SYSTEM helper when it runs | uinput multitouch type B "Pulsar Virtual Touchscreen", 10 slots | **mapped to mouse + pixel scroll** — macOS has no public touch-injection API (no touchscreen input path; trackpad gestures are private IOHIDEvents), so pinch / rotate cannot be injected |

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
