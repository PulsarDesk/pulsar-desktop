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
			inputPen: vi.fn(ok),
			inputTouch: vi.fn(ok),
			kbdCaptureStart: vi.fn(ok),
			kbdCaptureStop: vi.fn(ok),
			kbdEngage: vi.fn(ok)
		}
	};
});

import { SessionInput } from './input.svelte.js';
import { api } from '#lib/api.js';

// Free cursor ("Serbest imleç") on the canvas path (Linux / macOS): hovering the video takes
// control, leaving it hands control back, an explicit release waits for a leave.
function make(opts: { free?: boolean; blocked?: boolean; game?: boolean } = {}) {
	const canvas = document.createElement('canvas');
	canvas.getBoundingClientRect = () => ({ left: 0, top: 0, width: 100, height: 100, x: 0, y: 0, right: 100, bottom: 100, toJSON() {} }) as DOMRect;
	let input!: SessionInput;
	const stop = $effect.root(() => {
		input = new SessionInput({
			playId: () => 1,
			wsPort: () => 9000,
			canvas: () => canvas,
			mode: () => (opts.game ? 'game' : 'remote'),
			native: () => opts.game ?? false,
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

const pt = (pointerType: string, o: Partial<PointerEvent> = {}) =>
	({ clientX: 50, clientY: 25, buttons: 0, button: 0, pointerId: 7, pressure: 0, tiltX: 0, tiltY: 0, twist: 0, pointerType, preventDefault() {}, ...o }) as unknown as PointerEvent;

describe('SessionInput pen and touch', () => {
	beforeEach(() => {
		vi.clearAllMocks();
		vi.spyOn(document, 'hasFocus').mockReturnValue(true);
	});

	function controlling() {
		const m = make({ free: false });
		m.input.onDown(pt('mouse')); // the engaging click (swallowed)
		m.input.onUp(pt('mouse'));
		vi.clearAllMocks();
		return m;
	}

	it('a pen stroke goes out as pen state, never as mouse buttons', () => {
		const { input, stop } = controlling();
		input.onDown(pt('pen', { buttons: 1, pressure: 0.8, tiltX: 30 }));
		expect(api.inputPen).toHaveBeenLastCalledWith(1, {
			x: 0.5,
			y: 0.25,
			pressure: 0.8,
			tiltX: 30,
			tiltY: 0,
			twist: 0,
			inRange: true,
			contact: true,
			eraser: false,
			barrel: false
		});
		input.onUp(pt('pen'));
		expect(vi.mocked(api.inputPen).mock.calls.at(-1)![1]).toMatchObject({ inRange: true, contact: false, pressure: 0 });
		input.clearMove(pt('pen'));
		expect(vi.mocked(api.inputPen).mock.calls.at(-1)![1]).toMatchObject({ inRange: false });
		expect(api.inputButton).not.toHaveBeenCalled();
		expect(input.controlling).toBe(true);
		stop();
	});

	it('the eraser button and the barrel button are reported', () => {
		const { input, stop } = controlling();
		input.onDown(pt('pen', { buttons: 32, button: 5, pressure: 0.3 }));
		expect(vi.mocked(api.inputPen).mock.calls.at(-1)![1]).toMatchObject({ eraser: true, contact: true });
		input.onDown(pt('pen', { buttons: 3, pressure: 0.3 }));
		expect(vi.mocked(api.inputPen).mock.calls.at(-1)![1]).toMatchObject({ barrel: true, eraser: false });
		stop();
	});

	it('fingers go out as one frame; a lift sends the rest at once', () => {
		const { input, stop } = controlling();
		input.onDown(pt('touch', { pointerId: 1 }));
		input.onDown(pt('touch', { pointerId: 2, clientX: 100 }));
		expect(api.inputTouch).toHaveBeenLastCalledWith(1, [
			{ id: 1, x: 0.5, y: 0.25, pressure: 0 },
			{ id: 2, x: 1, y: 0.25, pressure: 0 }
		]);
		input.onUp(pt('touch', { pointerId: 1 }));
		expect(api.inputTouch).toHaveBeenLastCalledWith(1, [{ id: 2, x: 1, y: 0.25, pressure: 0 }]);
		input.onCancel(pt('touch', { pointerId: 2 }));
		expect(api.inputTouch).toHaveBeenLastCalledWith(1, []);
		expect(api.inputButton).not.toHaveBeenCalled();
		stop();
	});

	it('releasing control lifts the pen and every finger on the host', () => {
		const { input, stop } = controlling();
		input.onDown(pt('pen', { buttons: 1, pressure: 0.5 }));
		input.onDown(pt('touch', { pointerId: 9 }));
		input.stopControl();
		expect(vi.mocked(api.inputPen).mock.calls.at(-1)![1]).toMatchObject({ inRange: false, contact: false });
		expect(api.inputTouch).toHaveBeenLastCalledWith(1, []);
		stop();
	});

	it('native game mode forwards pen and touch without taking control', () => {
		const { input, stop } = make({ game: true });
		input.onDown(pt('touch', { pointerId: 4 }));
		expect(api.inputTouch).toHaveBeenLastCalledWith(1, [{ id: 4, x: 0.5, y: 0.25, pressure: 0 }]);
		input.onUp(pt('touch', { pointerId: 4 }));
		expect(api.inputTouch).toHaveBeenLastCalledWith(1, []);
		input.onDown(pt('pen', { buttons: 1, pressure: 0.6 }));
		expect(vi.mocked(api.inputPen).mock.calls.at(-1)![1]).toMatchObject({ contact: true, pressure: 0.6 });
		expect(input.controlling).toBe(false);
		expect(api.inputButton).not.toHaveBeenCalled();
		stop();
	});
});
