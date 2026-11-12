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
