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

## Platform parity: updates, pad wake, firewall, decoder caps (2026-11-14, none of it run on hardware)

| Feature | Windows | Linux | macOS |
| --- | --- | --- | --- |
| In-app update | NSIS quiet install (plugin) | **AppImage** (FUSE, `$APPIMAGE`): plugin in place. **.deb / .rpm**: `pkg_update.rs`: signed package from the same channel (`linux-<arch>-deb|rpm` keys, `gen-update-manifest.mjs`) → `pkexec apt-get install ./…deb` (`dnf`/`zypper`/`rpm -U` for rpm; `dpkg -i` without apt). No pkexec or polkit agent → `xdg-open` the verified file (software installer). Flatpak / AppImage without FUSE / raw binary: badge + manual hint | `.app.tar.gz` (plugin) |
| Auto-update setting / kiosk silent update | yes | AppImage only — a package install needs the admin password, so it is offered (badge + modal) but never installed unasked | yes |
| Pad wake / bring forward (`pad_wake.rs`) | AttachThreadInput + SetForegroundWindow | X11: `present_with_time(server time)` + EWMH `_NET_ACTIVE_WINDOW` source=pager (what `wmctrl -a` sends) + move to the current workspace. **Wayland: not possible** — a background client cannot take focus (xdg-activation needs a token from a focused surface or a launcher); `present()` only makes the compositor flag it ("Pulsar is ready" / task-bar flash) | `mac_activate.rs`: unhide + `activateIgnoringOtherApps` + `NSRunningApplication activateWithOptions` + `-[NSApp activate]` (14+) + deminiaturize / `orderFrontRegardless`. macOS 14+ cooperative activation may refuse to make Pulsar active; the window is still ordered above the others |
| Firewall | installer + elevated app add program rules (`firewall.rs`) | packages ship a ufw app profile (`/etc/ufw/applications.d/pulsar`) and a firewalld service (`pulsar.xml`) for **21117/udp (LAN discovery)**; postinst opens it only on the first install and only where ufw/firewalld is ACTIVE (an update never re-adds a rule the user removed), prerm removes it on a real removal (before the profile/service files are deleted). Sessions need no rule: they are UDP hole-punched (outbound first), which a stateful firewall admits. Not covered: a direct `ip:port` connect from another LAN machine to a host behind an active ufw/firewalld (the client sends first; the node port is ephemeral/sticky, so no static rule fits) — open the port by hand or set a fixed node port and allow it. AppImage: no install step, nothing is changed | `firewall_mac.rs`: when the Application Firewall is ON (off by default) and one of pulsar / pulsar-render / ffmpeg / ffplay is not permitted, ONE admin prompt per version (`osascript … with administrator privileges` → `socketfilterfw --add` + `--unblockapp`); cancelled = not asked again for that version (the marker dir is created first; if the marker cannot be written the prompt is skipped) |
| Decoder caps (`caps.rs`) | `pulsar-render --probe` (MFT enumeration; AV1 needs the AV1 Video Extension MFT) | `pulsar-render --probe` (real canned-frame decodes incl. AV1: dav1d/libaom/hwaccel) | hard-coded `h264` + `h265` software (mpv era) — **AV1 is never advertised** to a macOS client even where the decoder could do it (VideoToolbox AV1 is M3+ only; software dav1d depends on the bundled ffmpeg). Needs a `pulsar-render --probe` on macOS — owned by the macOS renderer work |

