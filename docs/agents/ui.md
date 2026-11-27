# UI conventions (SvelteKit frontend)

Part of the pulsar-desktop agent docs — index: [`CLAUDE.md`](../../CLAUDE.md).

Rules the code relies on that are easy to break. Each has a regression test.

## Modals

- **Use `#lib/Modal.svelte`** for a generic dialog (title, ×, body). It portals to `<body>`,
  sits above its backdrop (`z-index` 71 over 70) and registers with the two shared stacks below.
- **`overlayModals.svelte.ts`** counts open modals so the split-mode shell can hide every pane's
  native render window (it composites OVER the webview on Linux). `openModal`/`closeModal` run
  inside `$effect`s, so their read of the counter is `untrack`ed — otherwise the effect depends on
  what it writes and loops (`effect_update_depth_exceeded`, issue #1; `Devices.test.ts`).
- **Escape closes the topmost modal — every modal must register with `#lib/escapeStack.js`**
  (`$effect(() => closeOnEscape(() => onClose()))`, or `onMount`/`onDestroy` for the gaming
  modals). One capture-phase listener runs only the newest entry and stops the event, so a
  stacked dialog never closes the one beneath it and the gaming nav's Escape→back doesn't fire a
  second close (`escapeStack.test.ts`).
- The entrance animation (`pulsar-modal-in`, `app.css`) animates the individual `translate` /
  `scale` properties, **never `transform`** — that would override a dialog's own
  `translate(-50%, -50%)` centring (`Modal.test.ts`).

## Gaming-mode cards (pad navigation)

- A card with its own buttons is a **nav group**: the row gets `data-navgroup`, the card
  `data-navcard` (both registered with `use:nav.item`). In `GamepadNav`, up/down and left move
  card to card (the card itself — never another card's buttons, the original "left landed on
  the other card's star" bug). Right from a card walks into ITS buttons, then on to the next
  card; left inside the buttons walks back to the card. A on a card clicks it (connects).
  B/Esc on a button returns to its card, on a card runs the shell's back; a modal's back
  handler wins over both (`gamepadNav.test.ts`).
- Taking a device out of gaming Devices (the × or un-starring a card) always goes through the
  remove confirm (`GamingHome.test.ts`).

## Text selection

The app is not drag-selectable: `body` has `user-select: none`; inputs, textareas and
contenteditable stay selectable. **Opt a displayed value in with `class="selectable"`** —
IDs, addresses, OTP, chat lines, error/diagnostic text, paths, release notes. Never inside a
`<button>` (the drag would also click); give such text a copy button (`copyText` from
`#lib/api.js`) instead.

## Frameless window (Linux)

`decorations: false`, and the app runs under XWayland (`GDK_BACKEND=x11`, for the native video
child windows). Two consequences:

- **GTK's maximized flag goes stale** (KWin maximizes, GTK never sees it) → use
  `isWindowMaximized()` from `#lib/api`, never `toggleMaximize()`. The title bar's max button
  exits fullscreen first (the same `sm.toggleFullscreen` as the fullscreen button / F11).
- **No native resize border** → `routes/page/ResizeEdges.svelte` draws edge/corner grips that
  call `startResizeDragging` (permission `core:window:allow-start-resize-dragging`); the WM
  enforces `minWidth`/`minHeight` (1200×780 = the default size). Linux-only; Windows/macOS are
  assumed to resize undecorated windows natively (not verified on hardware).

## Session tabs: tear-off and merge (Chrome-style)

A live session tab can be dragged along the strip (reorder), out of it (its own window,
`tabwin-<n>`) or onto another Pulsar window's strip (moves there). The only tab of a tab
window drags the whole window and merges when dropped on a strip. Closing a tab window ends
its sessions. The session never restarts:

- Frontend: `Tabs.svelte` (drag), `routes/page/tabWindows.svelte.ts` (detach / move / strip
  rect / input owner), `SessionManager.release` / `adopt` / `moveTab`. A tab window is the
  same `+page.svelte` with `window.__TABWIN__` set: no Home, no bootstrap (`isPopup`).
- Rust: `src-tauri/src/tabwin.rs`. Play → window map (`window_for_play` — use it instead of
  `get_webview_window("main")` for anything per-session: scale, confine, dialogs, respawn).
  Video moves with `reparent 0x<hwnd>` on the Windows renderer's stdin / a GDK container
  reparent on Linux; macOS plays in mpv's own window. Capture belongs to the session window
  focused last (`input-window` event); `kbd_capture_stop(id)` only stops that session's.
- Not movable: connecting tabs, local host-game tabs, split mode, Linux single-surface.

## Keep on top (window pin, issue #18)

Window-level: the main window or a tab window, with all its tabs. Settings → Genel → "Üstte tut"
(`ui.windowPin`, default on; pushed with `set_window_pin_enabled` at start and on change).

- Controls: the title-bar pin (`Chrome.svelte`, left of fullscreen; hidden in fullscreen, absent
  while the setting is off) and the Pulsar menu's Tools row (`overlay.rs` `pin_row`, both modes;
  `pin <available> <on>` in, `ov set pin on|off` out → `play_pin_set`). State per window lives in
  `src-tauri/src/winpin.rs`; every change is echoed as `window-pin` (label, on) and to the
  window's renderers. Frontend: `routes/page/windowPin.svelte.ts`.
- Leaving fullscreen re-applies the pin (`winpin::reapply` in `set_window_fullscreen`).
- A tab torn off a pinned window opens pinned (`tab_detach`). Not remembered across restarts.
- Setting off: every pinned window is released, `window_pin_set(true)` is refused.
- Linux Wayland (XWayland): the compositor may ignore keep-above; the tooltip says so.

## Picture-in-picture (small window, issue #18)

Session-level: a session goes to a frameless window of its own (`pip-<n>`), its tab STAYS where
it was as a placeholder (`Session.pip` = the PiP label; `PipPlaceholder.svelte`: "Bring back" /
"Show"). Settings → Genel: "Küçük pencere" (`ui.pip`, default on, `set_pip_enabled`; off brings
every PiP back), "Üzerine gelince büyüt" (`ui.pipHoverGrow`, on) and its ratio
(`ui.pipHoverScale` 125/150/200). Entries: tab right-click (`Tabs.svelte`), the status bar's ⧉,
the Pulsar menu's Tools row (`pipwin <available> <on>` in, `ov set pip on|off` out).

- Rust `src-tauri/src/pip.rs`: opens the window (bottom-right of the source monitor, stacked
  above other PiPs, or `ui.pipLast`), hands the record over like a tear-off
  (`tabwin::handoff_insert` + `move_video`), `pip_return` sends the video back to the
  placeholder's window (`pip-return`) and closes the PiP. A moved placeholder only re-homes
  (`rehome` in `tab_move`/`tab_detach`); a closing tab window hands its placeholders to main
  (`home_closed`). Alt+F4 on a live PiP = bring back; an ended session closes it (`pip_close`).
- Frontend `routes/page/pipWindow.svelte.ts`: passive → hover (400 ms, grows keeping its corner
  via `pip_resize`, shows `PipBar.svelte`) → active (a click: focus + take control, the click
  never reaches the host; stays grown until the focus leaves). Double click = bring back; a
  press that moves drags the window; a drag's end snaps into a near corner (`pip_snap`) and is
  remembered. The PiP window has no chrome/tabs/status bar and opens unpinned, unfocused, and
  without the input (`tabWindows` does not claim `inputWin` for it).
- Where the events come from: hover is POLLED Rust-side (`pip-hover`, 100 ms) on every OS — on
  Windows the renderer child takes the mouse. Presses: the Windows renderer reports them in PiP
  mode (`ov pipclick` / `ov pipdrag` → `pip-press`) instead of `ov engage`; Linux (embedded) and
  macOS pass the pointer to the webview (capture-phase handlers on `.stage`).
- **The Pulsar menu of a PiP** opens in a window of its own beside the PiP (side with room,
  inside the monitor), the video stays in the PiP: Windows `win/menu.rs` (WS_POPUP,
  NOACTIVATE, owned by the PiP, own flip-model swapchain on the renderer's device), Linux
  `PipMenuWin` in `linux.rs` (override-redirect X window, second EGL surface of the same GL
  context, made current only for its pass), macOS `Panel::new_menu` + `Gpu::render_overlay_on`
  (floating non-activating NSPanel, pointer polled — AppKit sends moves to the key window only).
  Keys keep arriving over stdin (`kin …`) in every case. macOS UNTESTED on hardware.
- Geometry: GTK reports/sets frameless windows inconsistently (builds ~37 px taller, drops a
  move racing a resize, `outer_position` above what `set_position` moves), so `pip.rs` keeps its
  own record of each PiP's rect (`GEOM`) and lays growth out from it; user moves/resizes (window
  Moved/Resized outside a short window after our own) update it. Margins are on the client area.
  A drag of a grown PiP shrinks it around the pointer first; no growth while it moves.
- Hovering a PiP never takes control, also with the free cursor: Windows `handle_hover` and the
  macOS engine (`Arm::click_only`) skip PiP plays, the canvas path's `hoverBlocked` is true.

### More window features (issue #18 follow-ups)

- **Transparency** (`src-tauri/src/winalpha.rs`): Settings → Genel "Saydamlık" (`ui.opacity`, on)
  + default opacity (`ui.opacityDefault`, 100); per window from the Pulsar menu's Tools row
  (`opacity <available> <pct>` / `ov set opacity <pct>` → `play_alpha_set`). Windows = layered
  top-level (`LWA_ALPHA`, dropped at 100 %), Linux = GTK toplevel opacity, macOS = NSWindow
  `alphaValue` + the renderer panel (`Panel::set_alpha`). Whether the Windows flip-model video
  child turns see-through with its parent is UNVERIFIED.
- **Small window when minimized** (`ui.pipAutoMinimize`, on): `+page.svelte` sends the session on
  screen (active tab / focused pane) to a PiP when its window is minimized.
- **PiP bar extras**: chat badge (`chat-msg` for its play since the menu was opened) and a host
  monitor picker (several host screens; `Session.pipSetMonitor`).
- **Shortcuts** (`src/lib/windowShortcuts.ts`): Ctrl+Shift+<key> for keep-on-top (`ui.shortcutPin`,
  T) and small window (`ui.shortcutPip`, P; in a PiP: bring back). M/Q/Z are not offered. The
  input hooks catch them like the built-in combos (`kbdhook::shortcut_of`: Windows `imp.rs`,
  Linux `linux.rs`, macOS engine `Signal::Shortcut`) and emit `shortcut`; the webview catches them
  when no hook does; `runShortcut` drops the second sighting of one press.
- **Resolution follows the window** (Settings → Görüntü, `ui.autoResWindow`, on;
  `Session/autoRes.ts`): with the resolution on Auto, not split, no screen adaptation, a view
  smaller than the host screen asks `set_play_resolution` for a snapped step (360…2160) after
  1.5 s settled; back to 0×0 (host size) when it no longer applies. A PiP grown on hover holds
  (`resHold`). Applies in fullscreen too (a 1080p client of a 4K host streams 1080p).

## Theming

Tokens live in `src/app.css` (ported from `../design/project/assets/tokens.css`, which has **no
dark theme** — the dark values exist only here). `[data-theme='dark']` and `[data-gaming]`
(cyan accent) override them. The logo (`PulsarMark`) uses `--mark` and text selection uses
`--selection-bg`/`--selection-text`, themed separately from `--accent` because the light-theme
accent sinks into the dark surfaces.

## Testing in a browser

`bun run dev` serves the UI at `localhost:5173` against a deterministic mock (`api.invoke.ts`).
Headless checks can drive it with Playwright; the Tauri-only parts (window controls, resize
grips) render only inside the real app.

## Latency and its jitter

The session shows the latency with how much it varies after it — "4 ms (±0.6 ms)" — in the
status bar (`StatusBar.svelte`, one-way = RTT / 2, so the jitter is halved too) and in the
native renderer (hub tile and HUD, full RTT). One source: `play/hold.rs` `rtt_jitter_ms` (mean
absolute deviation of the last 8 RTT samples around their median — the same number the repair
window is sized with), sent as `play-rtt { rtt, jitter }` and `rtt <ms> <jitter>`.

## Theme

Light / dark is a switch in Settings → Genel ("Koyu tema", `theme.svelte.ts` `setDark`). It
used to be a button in the window title bar; the title bar keeps window and mode controls only.

## Toasts (`toast.rs`, `ToastWindow.svelte`)

Pulsar's own notices, bottom-right of the primary monitor's work area, never taking focus:
`file-in` (a file arrived), `kbd-perm` (macOS Accessibility), `pad-off` (no virtual pad),
`input-off` (a client's input cannot reach this host; click = fix: `input_notice::fix`),
`input-on` (that fix worked),
`chat-in`. **Host chat** (2026-11-15): a client's chat line shows as a `chat-in` toast — the
client's name (its id when it sent none) as the title, the line under it — when the
connections window (the host chat UI) is not on screen (absent, hidden or minimized); the
window then only opens in the background (taskbar). It used to jump to the front for every
line of a remote session and stay silent in a game session. Clicking the toast opens the chat.
