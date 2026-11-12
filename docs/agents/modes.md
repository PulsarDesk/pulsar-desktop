# Usage modes + CLI/headless start

Part of the pulsar-desktop agent docs — index: [`CLAUDE.md`](../../CLAUDE.md).

## Two usage modes (product direction — keep consistent everywhere)

Pulsar is ONE app with **two mode-aware personalities**, chosen at connect time
(`startConnect(target, mode: 'remote' | 'game')`). Mode drives **menu content,
overlay content, the look, and the encode profile**:

| | **Remote Desktop** (AnyDesk/RustDesk) | **Game Streaming** (Moonlight/Parsec) |
| - | - | - |
| Focus | general remote control + management | lowest latency, gaming |
| Menu | **full**: resolution/quality · codec/encoder · **file transfer · clipboard · multi-monitor** · chat · mic · reverse-direction · settings | **slim, game-only**: codec · bitrate (Mbit) · **fps** · resolution · quality/perf · encoder/decoder · controllers · end. **NO file/clipboard/mic/multi-monitor** |
| Overlay | thin info strip (connection/transport) | perf HUD (latency/fps/bitrate) + controller status + leave-combo hint |
| Look | neutral/general | gaming (cyan accent, immersive — `data-gaming`) |
| Encode | quality-focused | low-latency (mode-aware on the host) |

Established with the maintainer: **entering game streaming makes the whole app
gaming-focused; remote desktop makes it general remote-control-focused.**

## CLI / headless start (kiosk / appliance — esp. Orange Pi)

`pulsar --connect <id|ip> [--connect-pw <pw>] [--mode game]` auto-connects on
launch (splash, no home screen; `AppState.auto_connect`). Game mode's target app
defaults to **"Desktop"** — *always present, NOT deletable* (every host exposes a
"Desktop" entry). `pulsar --relay --session-rate 10mbit …` runs the app as a
relay (`relay_mode.rs`, same `pulsar_relay::Limits` flags as the relay repo).

## Input only while Pulsar is focused

While another app is focused, Pulsar acts on NO input (maintainer rule, 2026-11-11):

- **Controllers** (`src-tauri/src/focus_gate.rs`): the host gets a released pad — `GamepadSlot`
  (`play.rs` pad reader) sends the neutral state, the DualSense passthrough
  (`ds5/client.rs`) sends `Ds5Input` with every control let go, both repeated by the keepalive
  until focus returns. The in-app pad UI (gaming menu nav bridge, session overlay nav, the
  overlay/quit combos, split "press A to join") ignores the pad, and after refocus a button
  still held from the other app doesn't fire. Host → pad output (rumble, triggers, haptics,
  speaker) keeps playing. "Focused" = our window tree: on Windows the foreground window's root
  is ours (the `pulsar-render` video is a cross-process `WS_CHILD`), on Linux the evdev
  capture's focus (Tauri map or standalone render window), always true for a kiosk
  `--connect` launch (Orange Pi). Settings → Genel → "Arka planda kol girdisi" (default off)
  lets the pad keep driving the host (not the in-app UI) in the background.
- **Keyboard/mouse capture**: automatic engages (CLI `--connect` kiosk start, game mode's
  connect-time engage — `kbd_engage { auto: true }`) only happen while Pulsar is focused;
  otherwise they wait and engage when Pulsar gets the focus (Windows `PENDING_ENGAGE` in
  `kbdhook/imp.rs`; Linux's evdev grab is held only while focused anyway). On Windows a kiosk
  session now releases on focus loss too and re-engages on its own when focus returns.

## Free cursor ("Serbest imleç", remote mode, all desktop platforms)

Parsec-style handover, issue #3. Settings → Görüntü + the session menu's Görüntü view
(`ui.pointerFree`, default OFF; `set_pointer_free` command; renderer `ptrfree 0|1` /
`ov set ptrfree on|off`). Only remote-mode sessions (`kbdhook::set_session_remote` from
`kbd_capture_start`); game mode always keeps the cursor clipped to the video.

- `kbdhook/imp.rs` `free_mode()`: setting on + remote session + no host pointer lock + the
  WH_MOUSE_LL mechanism (the Interception path sends relative deltas and freezes the local
  cursor, so it keeps the clip). `CONFINE` still holds the video rect (pointer
  normalization), but no `ClipCursor` is applied.
- Engaged + the pointer leaves the video (no button held) → the move passes through, nothing
  is forwarded, and `handle_hover` on hook_thread releases (`HOVER_LEAVE`). A drag that
  started inside keeps driving the remote, pinned to the edge, until the button is up.
- Disengaged + the pointer is over the video + Pulsar is the foreground app + no button
  down → engage, no click. Not while the session menu is open (`overlay_suspend`), not while
  the stream is stopped, and not after an explicit release (Ctrl+Alt+Z, 3×RightCtrl, the
  overlay combo): `HOVER_ARMED` waits until the pointer has left the video once.
- The renderer reports its overlay-open button (`vidsink-ovbtn`); over it the pointer counts
  as outside the video, so the button stays clickable.
- Handover edges skip the renderer tooltip (`kbdhook::hover_edge` in `render_hint`), except
  the first engage of a session.
- **Linux / macOS (and phone hosts): the canvas path**, `Session/input.svelte.ts`
  (`pointerFree` input, off on Windows where the hook does it). No `confine_pointer`; a
  `pointerenter`/move over the canvas with no button, Pulsar focused (`document.hasFocus`) and
  nothing blocking (session menu open, stream stopped) → `startControl`; `pointerleave` with
  no button → `stopControl` (keyboard grab released too). A drag holds pointer capture, so
  the leave comes after the release. Explicit releases (Ctrl+Alt+Z, Ctrl+Shift+F12, the
  native `kbd-detach`) wait for a leave. The in-control hint shows only on the first hover
  engage (`quietHint`). Tests: `Session/input.svelte.test.ts`.
  - Linux: works where the video container is input pass-through (the default embedded
    renderer). The standalone render window (`--connect` kiosk) and the KMS-lease presenter
    take the pointer themselves, so there it stays click-to-engage.
  - macOS: the video is still mpv's own window, so the handover covers the Pulsar window's
    canvas only (the same limit as remote control there today).

## Host pointer marker (remote mode, issue #7)

While the person AT the host uses their own mouse (`host::takeover`, which holds remote
pointer input back for 1.2 s), the client marks the host's pointer and shows its own.

- **Host** (`host/handlers.rs` `spawn_host_pointer_watch`, remote sessions only): ~30 Hz
  `DataMsg::HostPointer { active: true, x, y }` while `takeover::active()`, one `active: false`
  when idle. Position = the pointer within the streamed monitor (Windows: GetCursorPos vs
  `display_rect`); `-1` = unknown (Linux host — takeover reads evdev deltas only —, a window
  capture, the pointer on another monitor). macOS hosts don't detect takeover → nothing.
- **Desktop client**: `play/hold.rs` → renderer `hostptr 1 x y` / `hostptr 0`, `hostname <name>`
  (from `PeerName`), `hostptrshow 0|1` (setting). `overlay.rs` `draw_host_pointer`: amber ring +
  "Host · name" label at the mapped point (Windows `video_point`, Linux `VIDEO_RECT`, macOS the
  whole viewport — its video is mpv's own window) and a top notice; fades 1.5 s after idle. The
  local cursor is un-hidden meanwhile (Windows/Linux renderer `hide_local_cursor` / `want_hidden`;
  the webview canvas path via the `host-pointer` event → `Session.svelte` `hostBusy`).
- **Setting**: `ui.hostCursor` (default ON) → `set_host_cursor`; session menu Görüntü row
  `ov set hostptr on|off`. Never in game mode (renderer `Mode::Remote` gate + no host send).
- **Mobile client** (pulsar-mobile): `datachan.rs` emits `host-pointer`; `ui/js/app.js` draws the
  same marker over the video rect (slot 0, not split, not game mode); Settings toggle
  `hostCursor` (prefs, default on).
