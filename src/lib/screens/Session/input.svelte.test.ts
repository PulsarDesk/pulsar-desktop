import { describe, it, expect, vi, beforeEach } from 'vitest';
import { flushSync } from 'svelte';

vi.mock('#lib/api.js', () => {
	const ok = () => Promise.resolve();
	return {
		api: {
			inputPointer: vi.fn(ok),
			inputButton: vi.fn(ok),
			inputKey: vi.fn(ok),
			inputChar: vi.fn(ok),
			inputScroll: vi.fn(ok),
			kbdCaptureStart: vi.fn(ok),
			kbdCaptureStop: vi.fn(ok),
			kbdEngage: vi.fn(ok)
		}
	};
});

import { SessionInput } from './input.svelte.js';

// Free cursor ("Serbest imleç") on the canvas path (Linux / macOS): hovering the video takes
// control, leaving it hands control back, an explicit release waits for a leave.
function make(opts: { free?: boolean; blocked?: boolean } = {}) {
	const canvas = document.createElement('canvas');
	canvas.getBoundingClientRect = () => ({ left: 0, top: 0, width: 100, height: 100, x: 0, y: 0, right: 100, bottom: 100, toJSON() {} }) as DOMRect;
	let input!: SessionInput;
	const stop = $effect.root(() => {
		input = new SessionInput({
			playId: () => 1,
			wsPort: () => 9000,
			canvas: () => canvas,
			mode: () => 'remote',
			native: () => false,
			streamW: () => 0,
			streamH: () => 0,
			fitMode: () => 'fit',
			pointerFree: () => opts.free ?? true,
			hoverBlocked: () => opts.blocked ?? false
		});
	});
	return { input, stop };
}

const ev = (buttons = 0) => ({ clientX: 50, clientY: 50, buttons, pointerId: 1, button: 0 }) as unknown as PointerEvent;
const key = (code: string, mods: Partial<KeyboardEvent> = {}) =>
	({ code, key: '', ctrlKey: false, altKey: false, shiftKey: false, metaKey: false, preventDefault() {}, getModifierState: () => false, ...mods }) as unknown as KeyboardEvent;

describe('SessionInput free cursor (canvas path)', () => {
	beforeEach(() => {
		vi.spyOn(document, 'hasFocus').mockReturnValue(true);
	});

	it('hovering the video takes control, leaving gives it back', () => {
		const { input, stop } = make();
		input.onEnter(ev());
		flushSync();
		expect(input.controlling).toBe(true);
		input.clearMove(ev());
		expect(input.controlling).toBe(false);
		stop();
	});

	it('a drag keeps control past the edge; the leave after the release hands it back', () => {
		const { input, stop } = make();
		input.onEnter(ev());
		input.clearMove(ev(1)); // left with the button still held
		expect(input.controlling).toBe(true);
		input.clearMove(ev(0));
		expect(input.controlling).toBe(false);
		stop();
	});

	it('after Ctrl+Alt+Z hovering waits until the pointer has left once', () => {
		const { input, stop } = make();
		input.onEnter(ev());
		input.onKey(key('KeyZ', { ctrlKey: true, altKey: true }), true);
		expect(input.controlling).toBe(false);
		input.onMove(ev());
		expect(input.controlling).toBe(false);
		input.clearMove(ev());
		input.onEnter(ev());
		expect(input.controlling).toBe(true);
		stop();
	});

	it('no hover engage while blocked, unfocused, or with the setting off', () => {
		const blocked = make({ blocked: true });
		blocked.input.onEnter(ev());
		expect(blocked.input.controlling).toBe(false);
		blocked.stop();

		const off = make({ free: false });
		off.input.onEnter(ev());
		expect(off.input.controlling).toBe(false);
		off.input.onDown(ev()); // click still engages as before
		expect(off.input.controlling).toBe(true);
		off.input.clearMove(ev());
		expect(off.input.controlling).toBe(true); // leaving doesn't release without the setting
		off.stop();

		vi.spyOn(document, 'hasFocus').mockReturnValue(false);
		const unfocused = make();
		unfocused.input.onEnter(ev());
		expect(unfocused.input.controlling).toBe(false);
		unfocused.stop();
	});
});
