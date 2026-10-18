# UI conventions (SvelteKit frontend)

Part of the pulsar-desktop agent docs — index: [`CLAUDE.md`](../../CLAUDE.md).

Rules the code relies on that are easy to break. Each has a regression test.

## Modals

- **Use `$lib/Modal.svelte`** for a generic dialog (title, ×, body). It portals to `<body>`,
  sits above its backdrop (`z-index` 71 over 70) and registers with the two shared stacks below.
- **`overlayModals.svelte.ts`** counts open modals so the split-mode shell can hide every pane's
  native render window (it composites OVER the webview on Linux). `openModal`/`closeModal` run
  inside `$effect`s, so their read of the counter is `untrack`ed — otherwise the effect depends on
  what it writes and loops (`effect_update_depth_exceeded`, issue #1; `Devices.test.ts`).
- **Escape closes the topmost modal — every modal must register with `$lib/escapeStack`**
  (`$effect(() => closeOnEscape(() => onClose()))`, or `onMount`/`onDestroy` for the gaming
  modals). One capture-phase listener runs only the newest entry and stops the event, so a
  stacked dialog never closes the one beneath it and the gaming nav's Escape→back doesn't fire a
  second close (`escapeStack.test.ts`).
- The entrance animation (`pulsar-modal-in`, `app.css`) animates the individual `translate` /
  `scale` properties, **never `transform`** — that would override a dialog's own
  `translate(-50%, -50%)` centring (`Modal.test.ts`).

## Text selection

The app is not drag-selectable: `body` has `user-select: none`; inputs, textareas and
contenteditable stay selectable. **Opt a displayed value in with `class="selectable"`** —
IDs, addresses, OTP, chat lines, error/diagnostic text, paths, release notes. Never inside a
`<button>` (the drag would also click); give such text a copy button (`copyText` from
`$lib/api`) instead.

## Frameless window (Linux)

`decorations: false`, and the app runs under XWayland (`GDK_BACKEND=x11`, for the native video
child windows). Two consequences:

- **GTK's maximized flag goes stale** (KWin maximizes, GTK never sees it) → use
  `isWindowMaximized()` from `$lib/api`, never `toggleMaximize()`. The title bar's max button
  exits fullscreen first (the same `sm.toggleFullscreen` as the fullscreen button / F11).
- **No native resize border** → `routes/page/ResizeEdges.svelte` draws edge/corner grips that
  call `startResizeDragging` (permission `core:window:allow-start-resize-dragging`); the WM
  enforces `minWidth`/`minHeight` (1200×780 = the default size). Linux-only; Windows/macOS are
  assumed to resize undecorated windows natively (not verified on hardware).

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
