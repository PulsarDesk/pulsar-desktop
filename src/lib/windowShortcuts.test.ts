import { describe, it, expect, vi } from 'vitest';
import { shortcutOf, runShortcut, SHORTCUT_KEYS } from './windowShortcuts';
import { ui } from '#lib/settings.svelte.js';

const key = (
	code: string,
	mods: Partial<{ ctrlKey: boolean; shiftKey: boolean; altKey: boolean; metaKey: boolean }> = {}
) => ({ ctrlKey: true, shiftKey: true, altKey: false, metaKey: false, code, ...mods });

describe('window shortcuts (issue #18)', () => {
	it('Ctrl+Shift+<picked key> only', () => {
		ui.shortcutPin = 'KeyT';
		ui.shortcutPip = 'KeyP';
		expect(shortcutOf(key('KeyT'))).toBe('pin');
		expect(shortcutOf(key('KeyP'))).toBe('pip');
		expect(shortcutOf(key('KeyP', { altKey: true }))).toBeNull();
		expect(shortcutOf(key('KeyP', { shiftKey: false }))).toBeNull();
		ui.shortcutPip = '';
		expect(shortcutOf(key('KeyP'))).toBeNull();
		ui.shortcutPip = 'KeyP';
	});
	it('the built-in combo keys are not offered', () => {
		for (const k of ['KeyM', 'KeyQ', 'KeyZ']) expect(SHORTCUT_KEYS).not.toContain(k);
	});
	it('one press seen twice (hook + webview) acts once', () => {
		const act = vi.fn();
		runShortcut('pip', act, 10_000);
		runShortcut('pip', act, 10_100);
		runShortcut('pip', act, 10_600);
		expect(act).toHaveBeenCalledTimes(2);
	});
});
