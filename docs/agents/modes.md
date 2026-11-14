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

## Host-only and headless host (CLI)

| Flag | What | Where |
| --- | --- | --- |
| `--host-only` | Normal GUI instance (window, tray, single-instance) whose UI only offers hosting: Home shows the SelfCard (ID, password, connected clients) only; the nav is Home + Settings; no connect form, recents, LAN devices, Devices, Games, game mode or split. `--connect` is ignored. Combines with `--background`. | `lib.rs` `HOST_ONLY` + `host_only` command; `+page.svelte` `hostOnly`, `Home.svelte` `hostOnly` |
| `--headless-host` | Host with no window, no tray, no webview (a Tauri app that creates none; `ui_start::set_headless` makes every popup/toast/cursor window a no-op). This user's config, identity, pairings (or shared mode with its lease). `headless::start` loop (retry 3 s → 12 s, stops on `needs_user`). Logs: the normal daily file + stdout (attaches to the parent console). Stops cleanly on `pulsar --headless-stop` (Windows, `Local\PulsarHeadlessHostStop` event), SIGINT/SIGTERM/SIGHUP on Linux/macOS (self-pipe handler; children keep default signal handling), and — **only when started from a console** (the release exe is GUI-subsystem and attaches to the parent console if there is one) — Ctrl+C / Ctrl+Break / console close / logoff / shutdown. Started by a supervisor, Task Scheduler or Explorer there is no console: logoff/shutdown then kill it without teardown, so a supervisor should call `--headless-stop` first. While attached, a Ctrl+C typed later in that shell (prompt already back) also stops it. Linux: still a (window-less) Tauri/GTK app, so it needs a display (`DISPLAY`/`WAYLAND_DISPLAY`); without one (plain systemd unit, ssh) it fails at start. Accepts no other flag. | `headless_host.rs` |
| `--headless-stop` | Signal this session's headless host to stop (exit 0; 1 = none running; 7 = one runs but this process may not signal it). The stop event grants this user `EVENT_MODIFY_STATE` with a Medium label, so a normal shell can stop a host started elevated by the same account. | `headless_host.rs` |
| `--split-seat <id> --data-dir <abs> --log <abs>` | One Pulsar Split seat host (Windows only), started by the Pulsar Split service as SYSTEM. Contract: pulsar-split `docs/agents/pulsar-host-backend.md`. | `headless_host.rs`, `seat/` |

All three headless flags are handled in `main.rs` before the elevation relaunch (no UAC
prompt — start `--headless-host` from an elevated shell to inject into elevated windows)
and before `run()`'s single-instance guard. Approval: with no window the Allow/Deny popup is
never shown — `--headless-host` admits only the password / one-time password, paired devices
and unattended access; a seat host asks the Pulsar Split operator over the pipe.

**Coexistence:** `--headless-host` takes `Local\PulsarSingletonMutex` like the GUI, so it
never runs next to a GUI instance of the same user/session (both would register the same
identity and fight over the relay ID). It exits with code 3 when a GUI runs — also when that
GUI is elevated (the default, `request_admin`) and the headless host is not: the mutex then
exists with a High label and opening it fails with access denied, which the headless path
(`acquire_single_instance_strict`) treats as "running" (the GUI's own guard still fails open
on errors). A GUI started while the headless host runs finds the mutex, signals the show
event nobody listens to, and exits — stop the headless host first. Gap: a non-elevated GUI
with `request_admin` off, started next to an **elevated** headless host, gets access denied
and fails open, so both run; keep them in the same elevation or stop the host first. A seat host takes neither that mutex nor shared mode,
so it coexists with the seat user's own Pulsar.

Exit codes (`headless_host.rs`): 0 ok, 2 bad argv, 3 already running (session / seat),
4 seat pipe could not be opened or its server is not the service, 5 seat pipe lost,
6 seat mutex could not be created (not SYSTEM), 7 `--headless-stop` refused (access denied).

### Split seat host (`--split-seat`)

- Argv is strict: exactly the three flags (any order), seat id = lowercase GUID, absolute
  paths; anything else (e.g. `--connect-pw`) → exit 2 without echoing values. All `PULSAR_*`
  env switches are removed from the process (`seat::strip_dev_env`).
- Takes `Global\PulsarSplit_Seat_<id>_PulsarHost` (DACL `D:P(A;;GA;;;SY)`, exit 3 if it
  exists); never `Local\PulsarSingletonMutex` / `Local\PulsarShowEvent`; shared mode bypassed
  (`lease::may_host` Ok, `util::data_dir` = `--data-dir`, never `%ProgramData%\Pulsar`).
- Logs to `--log` only (append, no stdout, no own rotation, `RUST_LOG` ignored).
- Opens `\\.\pipe\PulsarSplit\seat\<id>\pulsar-host` (retries 5 s), checks the server is
  `pulsar-seat-service.exe` (or the pre-merge `pulsar-split-service.exe`) as SYSTEM in session 0, then speaks protocol 1 (`seat/proto.rs`):
  `hello` (capabilities `approve`, `connect_password`, `otp`, `trust_store`), `secrets.load`
  (identity via `Identity::from_secret_bytes`, never `identity.key`; connect password and
  relay key in memory only), `config.apply` (→ `Config`, never saved; `display_name` is the
  announced name verbatim; `use_local_relay`/`unattended_access` forced off; `capture`
  dxgi/wgc → `PULSAR_CAPTURE`; after start, rebind keys come back in `restart_needed`),
  `status`, `auth.decide`, `auth.otp.get` / `.rotate`, `trust.list` / `.revoke` /
  `.revoke_all`, `relay.auth`, `server.shutdown` (teardown within `grace_ms`, then exit 0).
  The host binds/registers only after both `secrets.load` and `config.apply`.
- Events: `auth.request` / `auth.ended` (replace the popup, same cap 3 and budgets),
  `otp.changed`, `trust.changed`, `relay.state`, `relay.auth_required`, `relay.key_issued`
  (the key is never written to `config.json`).
- `not_supported`: `clients.list`, `clients.kick`, `clients.set_view_only`, `apps.set`,
  `app.launched`, `display.mode.result`, `pad.attach` / `.denied` / `.detach`, `log.level`;
  `config.apply` with `access: approve` or `capture: split_idd` is refused `not_supported`;
  `audio_endpoint_id`, `render_adapter_luid`, `codecs`, `max_clients` are ignored (logged).
- Off in seat mode: pad cleanup (`pad_cleanup::startup`), clipboard, file transfer and the
  remote file browser (SYSTEM would act in the seat user's session), the in-app relay, the
  local-mouse takeover hook and the remote-cursor overlay (never started headless).
- Broken pipe → exit 5 at once (it must not run uncontrolled).

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
- **Controller touchpad as mouse** (Settings → Genel → Oyun kolu → "Dokunmatik yüzey fare olarak",
  `ui.touchpadAsMouse`, default on, read at connect): a DS4 / DualSense touchpad drives the
  host pointer with the shared gestures of `pulsar_core::input::touchpad_mouse` — one finger
  = relative pointer, two fingers = scroll (natural direction), pad press = left click,
  two-finger press = right click. Readers per OS: **Linux** the pad's evdev touchpad node
  (`pulsar_core::input::touchpad_linux`, both MT slots; gate = `kbdhook::input_active`);
  **Windows + macOS** `src-tauri/src/touchpad_mouse.rs`, fed by `controllers.rs`
  (`PadView::touch`: SDL3 `SDL_GetGamepadTouchpadFinger` + the TOUCHPAD button, or the
  DualSense passthrough's raw report while it owns the pad); gate = Pulsar focused and the
  session menu closed (background input does NOT extend to the mouse); a disabled pad is
  ignored. **Routing** = the gamepad forward gate's rule (`focus_gate::PadRouting` →
  `pulsar_core::input::pad_route::pad_routes_to`, unit-tested in core): several tabs → only
  the focused tab's host; split screen → the pad's lock owner, else the focused pane, and no
  pane while "press to join" waits. Linux applies the same rule with "unlocked" (its evdev
  node is not tied to an SDL pad uuid, so a split-pane lock is not seen there).
  **The pad click reaches the host twice, on every OS** (maintainer decision pending, not a
  regression): as the mouse click, and in the forwarded pad state (`button::TOUCHPAD`, the
  touchpad button of the host's virtual pad) / the DualSense
  passthrough report; the touch contacts also reach the host's virtual DualSense through
  the passthrough. A game that reacts to the touchpad button gets it while the pointer
  clicks. Linux readers mark a new MT tracking id as a new contact (re-seed, no jump when a
  slot swaps fingers inside one frame); SDL and the raw DualSense report carry no contact
  id here, so that rare swap can still jump once on Windows / macOS. SDL contacts are
  scaled back with SDL's own divisors (920 DS4 / 1070 DS5) so motion matches the raw
  paths. **Untested on hardware on all three OSes**
  (macOS: SDL may serve the pad through its GameController/MFi driver instead of HIDAPI —
  that driver also reports the touchpad, unverified).
- **Keyboard/mouse capture**: automatic engages (CLI `--connect` kiosk start, game mode's
  connect-time engage — `kbd_engage { auto: true }`) only happen while Pulsar is focused;
  otherwise they wait and engage when Pulsar gets the focus (Windows `PENDING_ENGAGE` in
  `kbdhook/imp.rs`; Linux's evdev grab is held only while focused anyway; macOS
  `PENDING_ENGAGE` in the `pulsar-macinput` engine). On Windows and macOS a kiosk session
  releases on focus loss too and re-engages on its own when focus returns.

## macOS input capture (CGEventTap) - 2026-11-14, untested on macOS hardware

`src-tauri/src/kbdhook/mac.rs` (Tauri glue) + `crates/pulsar-macinput` (everything else; its
own crate so it type-checks with `cargo check --target aarch64-apple-darwin -p
pulsar-macinput` from a non-Mac, and its engine is unit-tested on every OS with `cargo test -p
pulsar-macinput`). Same surface and rules as the Windows hook (`imp.rs`):

- **Tap**: `CGEventTapCreate(kCGSessionEventTap, kCGHeadInsertEventTap, ...)` on its own
  CFRunLoop thread, created at the first arm and kept (enabled/disabled with the arm). A tap
  macOS disables (callback timeout, secure input) is re-enabled from the callback. A 100 ms
  run-loop timer reads the window list (`CGWindowListCopyWindowInfo`: bounds, owner pid,
  layer, number; no screen-recording permission needed) and the frontmost app.
- **Engaged**: every key is swallowed and forwarded (Cmd+Tab, Cmd+Space, Cmd+Q, Ctrl+arrows
  reach the host). Key map: `keymap.rs` (`kVK_*` to evdev, positional like Windows; Cmd = Meta,
  Option = Alt, Right Option = AltGr on a Windows host). Modifiers come from `FlagsChanged`
  (device bits tell left/right apart). Caps Lock is forwarded as a tap per toggle (the local
  caps state toggles too: the HID system does that before any tap). Fn stays local. A key-up
  whose down acted locally (pressed before engaging) also reaches the local app.
- **Combos**, engaged or merely frontmost: Ctrl+Shift+M (menu, kept-chord rule), Ctrl+Shift+Q
  (end), Ctrl+Shift+F11 or Cmd+Shift+F11 (fullscreen, as on the webview path). Engaged only:
  Ctrl+Alt(Option)+Z and 3xRightCtrl let go. Never while another app is frontmost.
- **Mouse**: relative in game mode and while the host holds a pointer lock. The cursor is
  frozen (`CGAssociateMouseAndMouseCursorPosition(false)`, plus a warp back to the anchor if
  it moves anyway), `kCGMouseEventDeltaX/Y` times the sensitivity are sent, the move is dropped.
  Remote mode: absolute, normalized to the video rect, the move passes locally; an overshoot
  is re-located and `CGWarpMouseCursorPosition`ed to the edge (the confine), re-checked every
  100 ms (window moves). Free cursor ("Serbest imlec"): no confine, leaving the video
  releases, hovering it engages, same rules as Windows. Buttons 0-2 are forwarded;
  back/forward are swallowed (no host injector knows them). Wheel: point deltas (trackpad)
  or lines x 100 (notched wheel), browser-pixel units like every other client. macOS's
  "natural scrolling" setting is already applied to those deltas and is **kept on purpose**:
  the remote content moves the way the Mac's own content does for this user (the webview
  fallback forwards browser wheel deltas, which carry it too). A Windows/Linux client has no
  such setting, so its deltas are the device's. A click on another app's window releases and
  acts locally.
- **Keys**: positional (`kVK_*` -> evdev). ISO keyboards (`KBGetLayoutType(LMGetKbdType())`,
  read per arm): the § and ` key codes macOS swaps are swapped back, like Chromium/SDL do.
  The volume keys (`kVK_VolumeUp/Down/Mute`) and Fn stay local.
- **Video rect**: the renderer child's window (the one under the cursor, else the largest),
  narrowed by the renderer's `vidsink-vrect` fractions when it reports them. Without a
  renderer window of ours on screen: the session area of the Pulsar window
  (`native_view_rect` -> `set_view`, by Quartz window number so it follows moves).
- **Click-to-engage**: a left click inside the video rect on the video's own window (not a
  Pulsar dialog over it) engages from the tap. The click still acts locally (activates the
  window) and never reaches the host; the host pointer starts at the click. The webview
  `kbd_engage` paths land on the same engage.
- **Focus**: Tauri `Focused(false)` releases only if the frontmost app
  (`NSWorkspace.frontmostApplication`) is neither Pulsar nor a renderer child (a child =
  `proc_pidinfo` parent pid is ours). The 100 ms poll releases when another app becomes
  frontmost (600 ms grace after an engage: activation is asynchronous). `focus_gate`
  (controllers) uses the same answer on macOS.
- **Accessibility permission** (`AXIsProcessTrusted`): without it `CGEventTapCreate` fails, so
  `enable` arms nothing and the session keeps the webview canvas path exactly as before (OS
  shortcuts stay local). `kbd_os_capture` answers `tap_ready()` (trusted **and** the tap really
  created), not trust alone: a stale grant on a rebuilt unsigned binary reports trusted while
  the creation fails; `enable` then emits `kbd-released` and the canvas path stays in charge.
  The tap thread is started once (IDLE -> STARTING -> RUNNING, set by the thread itself); a
  start slower than the 3 s wait is waited for again on the next arm, never doubled, and a tap
  that comes up late stays inert (`TAP_OK`) until the next arm. The first arm per run calls `AXIsProcessTrustedWithOptions` (macOS's
  own prompt) and shows a Pulsar toast (`toast.kbd-perm`, click -> System Settings -> Privacy &
  Security -> Accessibility). `kbd_os_capture` tells the UI which path owns control;
  `Session.svelte` asks again on window focus and hands control from the canvas to the tap
  when the permission was just granted (`SessionInput` `osCapture` keeps the canvas path out
  of the tap's way). TCC binds the grant to the signed app identity: an unsigned dev build may
  have to be re-granted after a rebuild.
- **Not done on macOS**: WYSIWYG `Char` typing through the local layout (Linux has xkb; the
  Windows hook is positional too), touchpad pinch -> `InputEvent::Pinch` (separate task),
  media keys (`NX_SYSDEFINED`; no host injector maps media keys and the Windows client leaves
  them local as well).
- **Needs a Mac to verify**: that the session tap really suppresses Cmd+Tab / Cmd+Space /
  Mission Control on current macOS; association + warp with the mpv window in front; Caps
  Lock; the window-list geometry with Retina and several displays; that the `src-tauri` side
  compiles (`pulsar-tauri` cannot be type-checked for macOS from Windows).


## Ctrl+Shift+M toggles the Pulsar menu — the second press has to arrive too

Opening the menu releases control, so the press that closes it comes in on the *disengaged*
key path. Every path must read it (2026-11-14: it opened and never closed):

- **Linux** — the evdev thread checks the chord before its suspend gate, on the live kernel
  key state (`chord_mods`).
- **Windows** (`kbdhook/imp.rs` `handle_key`) — fed by the LL hook, the renderer's hook
  (`hkey`) and, where the LL hook is never called while Pulsar is focused, the webview relay
  in `Session.svelte` (`kbd_web_key`). The relay runs while the menu is open as well, and it
  always passes the modifiers (also from an input field), so their up-strokes are seen.
- **Kept chord** — the release flush clears the modifier booleans (a missed up-stroke must not
  let a bare key fire a combo), but after this combo Ctrl+Shift are still physically down and
  the OS doesn't know (their down-strokes were swallowed while engaged; the webview gets the
  next M as a bare "m"). `keep_chord` keeps them as held until their next real edge, a focus
  loss or `CHORD_KEEP` (5 s); the `overlay-toggle` event carries `true` so the relay treats
  the bare M / Q / F11 as the combo for the same time.
- **macOS** (`pulsar-macinput` engine `handle_key`, a port of the Windows one incl. the kept
  chord), fed by the event tap, which sees the real modifier edges while disengaged too.
- **Webview key path** (macOS without Accessibility, phone hosts, the Windows fallback) — `SessionInput.onKey` only
  reads combos while controlling, so `SessionUi` (the Escape handler) reads Ctrl+Shift+M
  itself while the overlay is open.

Two sources may report one press (Linux evdev + the webview): both end in
`SessionUi.toggleOverlay`, whose 250 ms debounce keeps one.

**Closing by the toggle takes control again** (2026-11-14): the combo opened the menu out of
the session and closes it back into it — `SessionUi` calls `takeControl` (Session.svelte: the
OS capture on Windows native / macOS with Accessibility / game mode, the canvas path otherwise) once the close is
applied. So does a click on the video beside the menu (`ov control`, client-video.md).
Escape, the menu's own close and B on the pad leave control released (click-to-control). Tests: `ui.svelte.test.ts`,
`kbdhook::imp::chord_tests` (the Tauri crate's test binary needs a Common-Controls v6
manifest next to it to load on Windows — `TaskDialogIndirect`).

## Sleep ends the viewed sessions, wake-up connects them again (2026-11-14)

A connected session keeps the CLIENT machine awake — its audio stream alone holds Windows
out of Modern Standby — and keeps the host encoding (a laptop left connected streamed into a
dark screen all night). So when the machine that VIEWS goes into a sleep-like state, its
sessions end, and they come back by themselves afterwards:

- **Sleep-like** = the system suspends, or its display turns off (on a Modern Standby machine
  that is its sleep; no suspend event comes while audio plays). An automatic resume with the
  display still off does not count as awake.
- `src-tauri/src/power_watch.rs` (Windows: `PowerRegisterSuspendResumeNotification` +
  `PowerSettingRegisterNotification(GUID_CONSOLE_DISPLAY_STATE)`, callbacks, no window) emits
  `system-sleep`, then stops every play itself — the webview may be throttled with the
  display off, so the UI is not trusted to do it — and emits `system-wake` on the way back.
- `SessionManager.systemSleep` remembers each remote session (target, mode, game, monitor,
  tab name) and ends it; `systemWake` connects them again in rounds (`RESUME_DELAYS_MS`:
  the network returns after the screen), stops if the machine sleeps again, and gives up after
  the last round (the error shows on Home like any failed connect). A local host-game tab is
  not a viewed session and stays. A reconnect goes through the host's normal access rules
  (password / approval prompt again where the host asks for one). Split panes come back as
  tabs.
- The HOST's own sleep is not handled here (its clients see it go silent).
- **Linux** (`power_watch/linux.rs`, plain threads with blocking zbus / Xlib connections;
  untested on Linux hardware): logind `PrepareForSleep` on the system bus, with a logind
  *delay* inhibitor held while awake so the suspend waits (≤ 3 s of our own, capped by
  `InhibitDelayMaxSec`) until the sessions are stopped; "display off" = the desktop screen
  saver / lock screen `ActiveChanged(true)` (`org.freedesktop.ScreenSaver`: KDE, Xfce,
  Cinnamon, MATE, LXQt; `org.gnome.ScreenSaver`: GNOME) or, in an X11 session, the X server's
  DPMS level ≠ On (polled every 2 s). Locking the screen therefore also ends the sessions.
  Because the evdev grab hides local input from the desktop's idle timer, every forwarded
  event (`kbdhook/linux.rs` `fwd` → `power_watch::note_input`) is reported as user activity
  at most every 20 s: `XResetScreenSaver` on X11, `org.freedesktop.ScreenSaver.SimulateUserActivity`,
  and on GNOME (no such call) an `org.gnome.SessionManager` idle inhibitor held until 60 s
  after the last input. **Not possible:** on Wayland compositors with neither screen-saver
  service (bare wlroots: sway, Hyprland …) display-off is not detectable — there is no
  portable protocol to read the output power state; only the suspend is seen there.
- **macOS** (`power_watch/macos.rs`; untested on macOS hardware): NSWorkspace
  `willSleep` / `didWake` (suspend) and `screensDidSleep` / `screensDidWake` (display off)
  observers on the workspace notification center, delivered on the main thread's run loop
  (the app's own NSApplication loop — no extra thread). macOS gives no way to delay a sleep
  from willSleep for long, so the stops are fired and not awaited. `macos.rs` only uses objc2
  crates and was type-checked for aarch64-apple-darwin in a scratch crate (`pulsar-tauri` as a
  whole cannot be checked for macOS from Windows).
- Same events (`system-sleep` / `system-wake`) and the same stop logic on all three.

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
- **macOS with the Accessibility permission**: the event tap does it (`pulsar-macinput`
  engine `free_mode` / `handle_hover`, same rules as Windows); the canvas path is off.
- **Linux / macOS without the permission (and phone hosts): the canvas path**, `Session/input.svelte.ts`
  (`pointerFree` input, off on Windows where the hook does it). No `confine_pointer`; a
  `pointerenter`/move over the canvas with no button, Pulsar focused (`document.hasFocus`) and
  nothing blocking (session menu open, stream stopped) → `startControl`; `pointerleave` with
  no button → `stopControl` (keyboard grab released too). A drag holds pointer capture, so
  the leave comes after the release. Explicit releases (Ctrl+Alt+Z, Ctrl+Shift+F11, the
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
  `display_rect`; Linux X11: XQueryPointer vs the captured monitor, `takeover::PointerPos`;
  macOS: the main display, `pulsar_core::input::local_pointer_macos::pointer_on_main_display`);
  `-1` = unknown (Wayland, a window capture, the pointer on another monitor).
- **Takeover detection per OS** (`host/takeover.rs`): Windows `WH_MOUSE_LL` minus
  `LLMHF_INJECTED`. Linux: evdev read of every `/dev/input` pointer except `Pulsar*` uinput
  devices (needs the `input` group) **plus**, on X11, an XInput2 raw-event listener (RawMotion /
  RawButton* on the root, no permission needed) that skips `Pulsar*` and `XTEST` slave devices
  (`is_synthetic_device`); its device-id cache is cleared on `XI_HierarchyChanged` (X reuses
  ids when our per-session uinput devices go away). Wayland without `/dev/input` access: **not
  done yet, but possible** — the ScreenCast portal offers `CursorMode::Metadata` (GNOME/KDE),
  whose PipeWire buffers carry the pointer position (`SPA_META_Cursor`); "pointer moved with no
  Pulsar injection in the last few ms" would give takeover detection and the marker x/y without
  the `input` group. It needs a capture path that reads PipeWire buffers itself (GStreamer's
  `pipewiresrc` drops the cursor meta) and the cursor drawn over the video another way (the
  client's cursor overlay), since Metadata mode removes it from the frames. Until then Wayland
  sends `-1/-1` and detects takeover only via evdev. macOS: listen-only `CGEventTap`
  (`pulsar_core::input::local_pointer_macos::watch_local_pointer`); every event Pulsar's
  injector posts carries `kCGEventSourceUserData = "PULS"` and is skipped (plus our own pid).
  Mouse-only listen taps need no TCC grant on 10.15+ (only keyboard taps need Input
  Monitoring); `request_listen_access` still asks once, and a failed tap is retried every 10 s
  so a later grant works without a restart; while it fails, remote input always applies. **Linux X11 and macOS untested on hardware.**
- **Host-side ghost** (`remote_cursor.rs`, the client's pointer while held back): Linux = GTK
  popup; Windows/macOS = transparent click-through webview. On macOS it is made a system-wide
  overlay (`pulsar_core::overlay_macos::make_overlay_window`: status-window level, joins every
  Space; showing over another app's full-screen Space is **unverified** — a plain NSWindow may
  not, and the fix would be an NSPanel with `NSWindowStyleMaskNonactivatingPanel`) and kept out of the avfoundation capture by
  `content_protected` (`NSWindowSharingNone`). Untested on macOS hardware; mixed-DPI monitor
  layouts may misplace it (Tauri physical coordinates).
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

## Start with the computer / in the background (Settings → Genel)

`src-tauri/src/autostart.rs`: Windows = a logon task "Pulsar autostart (<user>)" at highest
privileges when Pulsar is elevated (a Run-key start would ask UAC at every sign-in), else the
HKCU Run key; Linux = `~/.config/autostart/pulsar.desktop`; macOS =
`~/Library/LaunchAgents/com.pulsardesk.pulsar.plist`. "In the background" adds `--background`:
no splash, the window stays hidden (tray) — shown anyway when there is no tray (`splash.rs`
`stay_hidden` / `show_hidden_background`).

## Several sessions to one device (Settings → Genel, default off)

`ui.duplicateSessions`: a repeat connect to the same host/mode/game opens another session
instead of focusing the open one (`SessionManager.startConnect` de-dup).

## Same-host co-op: per-window input (all desktop OSes, 2026-11-14)

When a session's stream shows ONE window (a game this session launched, or a window the
client picked), its keyboard/mouse go to that window instead of the global desktop, so two
panes on one host do not fight over the single focus. `host.rs` `on_input` holds a
`pulsar_core::input::WindowInput` built from `win_capture_hwnd` — the window the CURRENT
stream confirmed capturing (`make_on_stream` publishes it). Same API on every OS; when the
window is in front, keys go through the global injector (`desk_keys` keeps each UP on the
path its DOWN took).

| OS | Window id | Backend (pulsar-core `input/`) | Reaches | Does not reach |
| --- | --- | --- | --- | --- |
| Windows | HWND | `window_win.rs`: PostMessage | Win32 message-pump apps | Raw Input / DirectInput / GetAsyncKeyState games (the ProtoInput co-op isolation in `coop.rs` covers those) |
| Linux X11 / XWayland | XID | `window_x11.rs`: background = core `SendEvent` to the window (deepest child under the pointer); front = XTest | apps reading core X events; front window = everything | background XI2 clients (GTK3/4, Qt5/6 pointer, SDL relative mouse, most games), Xt apps without `allowSendEvents`, `/dev/input` readers |
| Linux native Wayland | — | **impossible**: no protocol lets one client inject into another's surface; uinput / RemoteDesktop portal are seat-global | — | everything (global injector stays) |
| macOS | CGWindowID | `window_mac.rs`: `CGEventPostToPid` to the owning PID (+ window number fields); front = HID tap | AppKit/NSEvent apps, SDL keyboard, many windowed games | IOHID / GameController readers; keyboard of a second window of the SAME process goes to that process's key window |

**Per-window capture source (what publishes `win_capture_hwnd`), per OS:**

| OS | Window list (client picker, `QueryWindows`) | Launched game → window | Capture | State |
| --- | --- | --- | --- | --- |
| Windows | `pulsar_capture::list_capture_windows` (EnumWindows) | `find_window_for_launch` (PID + child PIDs) | native WGC + NVENC | wired |
| Linux X11 | `window_src::list` → `pulsar_core::window_capture` (`_NET_CLIENT_LIST`) | `window_capture::find_for_launch` (largest window of the pid tree), polled 10 s | `window_src` (`parity/window-capture`): libav `x11grab window_id`, ffmpeg `x11grab -window_id` (ffmpeg 5.1+) or gst `ximagesrc xid=`, the window's aspect fitted into the request | wired 2026-11-14 |
| Linux Wayland | one portal entry (`PORTAL_PICK`) | none | portal WINDOW cast (no window id) | impossible for input (see above); global injector |
| macOS | `window_src::list` → ScreenCaptureKit `SCShareableContent` | `window_capture::find_for_launch` | ScreenCaptureKit window → rawvideo → ffmpeg (VideoToolbox) | wired at the `parity/all` merge |

Since the `parity/all` merge, `make_on_stream` publishes the window `host_window` resolved
(X11 XID, or the `CGWindowID` once its ScreenCaptureKit stream started) as the input target
and in `coop::set_capture`; a screen stream clears it. The co-op branch's own X11 capture
block (`x11_window`, `StreamPlan.window_id`) was folded into that path. A resized window
re-streams through the window watcher (see host-encode.md "Per-window capture"); an unmapped
(minimized) X11 window captures black.

`WindowInput` caches its window-server answers (`is_alive`, `in_front`, size, X11 child
under the pointer) for 200 ms, because the host asks on every input event. X11 "front"
needs the X input focus inside the target AND (with EWMH) `_NET_ACTIVE_WINDOW` naming it
(XWayland keeps `_NET_ACTIVE_WINDOW` stale when a Wayland window takes focus). macOS
"front" = owner pid is `NSWorkspace.frontmostApplication` AND the window is that pid's
top on-screen window; relative mouse deltas are divided by the display's backing scale.

The ProtoInput instance isolation (`coop.rs`) is DLL injection into a Windows process
and has no Linux/macOS counterpart.

**Untested on Linux and macOS hardware** (type-checked + unit tests only).

## Reverse direction

The requesting client pre-authorizes the host's E2E key for its connect back (one use, 60 s:
`auth::preauth::expect_reverse` / `take_reverse`) — no password or Allow/Deny; it ends its own
session and hides Pulsar. The other side brings Pulsar forward (`pad_wake::bring_forward`,
AttachThreadInput — never a synthetic Alt: that opened the window menu loop and froze the UI;
Linux X11 `_NET_ACTIVE_WINDOW`, macOS app activation, Wayland only `present()` — see
[status.md](status.md) "Platform parity").
