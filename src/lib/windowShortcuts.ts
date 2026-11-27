// User shortcuts for the window features (issue #18): Ctrl+Shift+<key>, the key picked in
// Settings → General. The input hook catches them while in control (and while Pulsar is merely
// focused, on Windows/macOS); the webview catches them otherwise. Both can see one press, so an
// action within `DEDUPE_MS` of the same one is dropped.

import { api, isTauri } from '#lib/api.js';
import { evdevCode } from '#lib/keymap.js';
import { ui } from '#lib/settings.svelte.js';

/** Keys a shortcut may use: letters, minus the built-in combos (Ctrl+Shift+M menu, Q leave)
 * and Z (Ctrl+Alt+Z let go). */
export const SHORTCUT_KEYS: string[] = 'ABCDEFGHIJKLNOPRSTUVWXY'.split('').map((c) => `Key${c}`);

const DEDUPE_MS = 400;
const last: Record<string, number> = {};

/** Push the picked keys to the input hook. */
export function pushShortcuts() {
	if (!isTauri) return;
	api.setWindowShortcuts(evdevCode(ui.shortcutPin), evdevCode(ui.shortcutPip)).catch(() => {});
}

/** Which shortcut a keydown is (Ctrl+Shift+key, no Alt/Meta), if any. */
export function shortcutOf(
	e: Pick<KeyboardEvent, 'ctrlKey' | 'shiftKey' | 'altKey' | 'metaKey' | 'code'>
): 'pin' | 'pip' | null {
	if (!e.ctrlKey || !e.shiftKey || e.altKey || e.metaKey) return null;
	if (ui.shortcutPin && e.code === ui.shortcutPin) return 'pin';
	if (ui.shortcutPip && e.code === ui.shortcutPip) return 'pip';
	return null;
}

/** Run `act` for shortcut `name` unless the same one just ran (hook + webview both saw it). */
export function runShortcut(name: 'pin' | 'pip', act: () => void, now = Date.now()) {
	if (now - (last[name] ?? 0) < DEDUPE_MS) return;
	last[name] = now;
	act();
}
