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
			inputPinch: vi.fn(ok),
			inputPen: vi.fn(ok),
			inputTouch: vi.fn(ok),
			kbdCaptureStart: vi.fn(ok),
			kbdCaptureStop: vi.fn(ok),
			kbdEngage: vi.fn(ok)
		}
	};
});

import { SessionInput, pinchWheelFactor, PINCH_IDLE_MS } from './input.svelte.js';
import { api } from '#lib/api.js';

// Free cursor ("Serbest imleç") on the canvas path (Linux / macOS): hovering the video takes
// control, leaving it hands control back, an explicit release waits for a leave.
function make(opts: { free?: boolean; blocked?: boolean; os?: boolean; osEngaged?: boolean; game?: boolean } = {}) {
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
			hoverBlocked: () => opts.blocked ?? false,
			osCapture: () => opts.os ?? false,
			osEngaged: () => opts.osEngaged ?? false
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

// macOS with the Accessibility permission: the CGEventTap owns control (click-to-engage, keys,
// mouse) — the canvas path must not start as well, or every input would reach the host twice.
describe('SessionInput with the OS-level capture', () => {
	beforeEach(() => {
		vi.spyOn(document, 'hasFocus').mockReturnValue(true);
	});

	it('neither a click nor a hover starts canvas control', () => {
		const { input, stop } = make({ os: true });
		expect(input.controllable).toBe(false);
		input.startControl();
		input.onEnter(ev());
		flushSync();
		expect(input.controlling).toBe(false);
		stop();
	});

	it('without it the canvas path works as before', () => {
		const { input, stop } = make({ os: false, free: false });
		expect(input.controllable).toBe(true);
		input.startControl();
		expect(input.controlling).toBe(true);
		stop();
	});
});

describe('SessionInput touchpad pinch', () => {
	const wheel = (deltaY: number, ctrlKey = true) =>
		({ deltaX: 0, deltaY, deltaMode: 0, ctrlKey, preventDefault() {} }) as unknown as WheelEvent;
	const gesture = (type: string, scale: number) => {
		const e = new Event(type, { cancelable: true }) as Event & { scale: number };
		e.scale = scale;
		return e;
	};
	beforeEach(() => {
		vi.spyOn(document, 'hasFocus').mockReturnValue(true);
		vi.mocked(api.inputPinch).mockClear();
		vi.mocked(api.inputScroll).mockClear();
	});

	it('maps pinch wheel deltas to zoom factors', () => {
		expect(pinchWheelFactor(0)).toBe(1);
		expect(pinchWheelFactor(-100)).toBeCloseTo(Math.E);
		expect(pinchWheelFactor(100)).toBeCloseTo(1 / Math.E);
		expect(pinchWheelFactor(Number.NaN)).toBe(1);
		expect(pinchWheelFactor(-1e9)).toBeCloseTo(Math.exp(3));
	});

	it('forwards a synthesized Ctrl + wheel pinch and ends it when idle', () => {
		vi.useFakeTimers();
		const { input, stop } = make({ free: false });
		input.startControl();
		flushSync();
		input.onWheel(wheel(-10));
		input.onWheel(wheel(-10));
		const calls = vi.mocked(api.inputPinch).mock.calls;
		expect(calls[0]).toEqual([1, 0, 1]);
		expect(calls[1][1]).toBe(1);
		expect(calls[2][2]).toBeCloseTo(Math.exp(0.2));
		expect(api.inputScroll).not.toHaveBeenCalled();
		vi.advanceTimersByTime(PINCH_IDLE_MS + 1);
		expect(calls.at(-1)?.[1]).toBe(2);
		// A plain wheel still scrolls.
		input.onWheel(wheel(30, false));
		expect(api.inputScroll).toHaveBeenCalledTimes(1);
		stop();
		vi.useRealTimers();
	});

	it('forwards WebKit gesture events and ignores their duplicate wheel', () => {
		const { input, stop } = make({ free: false });
		input.startControl();
		flushSync();
		input.onGestureStart(gesture('gesturestart', 1));
		input.onGestureChange(gesture('gesturechange', 1.5));
		input.onWheel(wheel(-10));
		input.onGestureEnd(gesture('gestureend', 1.6));
		const calls = vi.mocked(api.inputPinch).mock.calls.map((c) => [c[1], c[2]]);
		expect(calls).toEqual([
			[0, 1],
			[1, 1.5],
			[1, 1.6],
			[2, 1.6]
		]);
		stop();
	});

	it('closes an open pinch when control stops', () => {
		const { input, stop } = make({ free: false });
		input.startControl();
		flushSync();
		input.onGestureStart(gesture('gesturestart', 1));
		input.stopControl();
		flushSync();
		expect(vi.mocked(api.inputPinch).mock.calls.at(-1)?.[1]).toBe(2);
		stop();
	});

	it('forwards gesture pinches while the macOS event tap is engaged', () => {
		const { input, stop } = make({ os: true, osEngaged: true });
		expect(input.controlling).toBe(false);
		input.onGestureStart(gesture('gesturestart', 1));
		input.onGestureChange(gesture('gesturechange', 1.4));
		input.onGestureEnd(gesture('gestureend', 1.4));
		expect(vi.mocked(api.inputPinch).mock.calls.map((c) => c[1])).toEqual([0, 1, 1, 2]);
		stop();
	});

	it('ignores gesture pinches while the macOS event tap is not engaged', () => {
		const { input, stop } = make({ os: true, osEngaged: false });
		input.onGestureStart(gesture('gesturestart', 1));
		expect(api.inputPinch).not.toHaveBeenCalled();
		stop();
	});

	it('ignores pinches while not controlling', () => {
		const { input, stop } = make({ free: false });
		input.onGestureStart(gesture('gesturestart', 1));
		input.onWheel(wheel(-10));
		expect(api.inputPinch).not.toHaveBeenCalled();
		stop();
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

	it('a hovering eraser stays an eraser until the pen leaves', () => {
		const { input, stop } = controlling();
		input.onDown(pt('pen', { buttons: 32, button: 5, pressure: 0.3 }));
		input.onUp(pt('pen', { buttons: 0, button: 5 }));
		expect(vi.mocked(api.inputPen).mock.calls.at(-1)![1]).toMatchObject({ eraser: true, contact: false, inRange: true });
		const sent = vi.mocked(api.inputPen).mock.calls.length;
		// Hover moves carry no buttons: no tool-switch edge goes out.
		input.onMove(pt('pen', { buttons: 0, button: -1, clientX: 60 }));
		expect(vi.mocked(api.inputPen).mock.calls.length).toBe(sent);
		input.clearMove(pt('pen', { button: -1 }));
		expect(vi.mocked(api.inputPen).mock.calls.at(-1)![1]).toMatchObject({ inRange: false, eraser: true });
		// Back in range with the tip end: a plain pen again.
		input.onMove(pt('pen', { buttons: 0, button: -1 }));
		expect(vi.mocked(api.inputPen).mock.calls.at(-1)![1]).toMatchObject({ inRange: true, eraser: false });
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
