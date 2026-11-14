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

**Per-window host capture on Linux + macOS (2026-11-14, branch `parity/window-capture`,
untested on Linux X11 / Wayland / macOS hardware):** the "Pencere" pick and game-mode
launched-window capture, until now Windows-only (WGC), on X11 (`x11grab -window_id` /
`ximagesrc xid=`), Wayland (portal WINDOW source — the host person picks) and macOS
(ScreenCaptureKit window → rawvideo → ffmpeg). Per-OS table + limits:
[host-encode.md](host-encode.md#per-window-capture-pencere-phase-2b-co-op--per-os). Needs the
matching pulsar-core branch. Per-window input and window-to-pane fitting stay Windows-only.

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
