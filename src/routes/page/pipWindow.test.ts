// @vitest-environment node
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';

vi.mock('#lib/api.js', async () => {
	const { vi: _vi } = await import('vitest');
	return {
		isTauri: false,
		api: { pipResize: _vi.fn().mockResolvedValue(undefined), pipSnap: _vi.fn().mockResolvedValue(null), pipSize: _vi.fn().mockResolvedValue(null) },
		onPipRestore: _vi.fn(),
		onPipHover: _vi.fn(),
		onPipPress: _vi.fn(),
		onChatMsg: _vi.fn()
	};
});

import { api } from '#lib/api.js';
import { ui } from '#lib/settings.svelte.js';
import { PipWindow, PIP_BAR_H, growFactor } from './pipWindow.svelte';

function make() {
	const deps = { restore: vi.fn(), takeControl: vi.fn(), playId: () => 7, ownsInput: () => true, engaged: () => false };
	return { pw: new PipWindow(deps), deps };
}
const press = (x = 10, y = 10) =>
	({ button: 0, buttons: 1, screenX: x, screenY: y, preventDefault: vi.fn(), stopPropagation: vi.fn() }) as unknown as PointerEvent;

describe('picture-in-picture window (issue #18)', () => {
	beforeEach(() => {
		vi.useFakeTimers();
		vi.mocked(api.pipResize).mockClear();
		ui.pipHoverGrow = true;
		ui.pipHoverScale = 150;
	});
	afterEach(() => vi.useRealTimers());

	it('grows after the pointer rests on it, by the chosen ratio plus the control bar', () => {
		const { pw } = make();
		pw.enter();
		vi.advanceTimersByTime(399);
		expect(pw.grown).toBe(false);
		vi.advanceTimersByTime(1);
		expect(pw.grown).toBe(true);
		expect(api.pipResize).toHaveBeenCalledWith(720, 405 + PIP_BAR_H, false);
	});

	it('a quick pass does not grow it; leaving shrinks it back', () => {
		const { pw } = make();
		pw.enter();
		vi.advanceTimersByTime(200);
		pw.leave();
		vi.advanceTimersByTime(1000);
		expect(api.pipResize).not.toHaveBeenCalled();
		pw.enter();
		vi.advanceTimersByTime(400);
		pw.leave();
		vi.advanceTimersByTime(250);
		expect(pw.grown).toBe(false);
		expect(api.pipResize).toHaveBeenLastCalledWith(480, 270, false);
	});

	it('growth off: it only makes room for the controls', () => {
		ui.pipHoverGrow = false;
		const { pw } = make();
		pw.enter();
		vi.advanceTimersByTime(400);
		expect(api.pipResize).toHaveBeenCalledWith(480, 270 + PIP_BAR_H, false);
	});

	it('a click activates (the click itself never reaches the session) and keeps it grown', async () => {
		const { pw, deps } = make();
		const e = press();
		pw.down(e);
		expect(e.stopPropagation).toHaveBeenCalled();
		pw.up(press());
		expect(pw.active).toBe(false);
		await vi.advanceTimersByTimeAsync(300);
		expect(pw.active).toBe(true);
		expect(pw.grown).toBe(true);
		expect(deps.takeControl).toHaveBeenCalledOnce();
		pw.leave();
		vi.advanceTimersByTime(1000);
		expect(pw.grown).toBe(true);
	});

	it('another Pulsar window taking the input ends it', async () => {
		let owns = true;
		const deps = { restore: vi.fn(), takeControl: vi.fn(), playId: () => 7, ownsInput: () => owns, engaged: () => true };
		const pw = new PipWindow(deps);
		pw.down(press());
		pw.up(press());
		await vi.advanceTimersByTimeAsync(300);
		expect(pw.active).toBe(true);
		owns = false;
		await vi.advanceTimersByTimeAsync(600);
		expect(pw.active).toBe(false);
	});

	it('a double click brings the session back instead', async () => {
		const { pw, deps } = make();
		pw.down(press());
		pw.up(press());
		vi.advanceTimersByTime(120);
		pw.down(press());
		pw.up(press());
		await vi.advanceTimersByTimeAsync(600);
		expect(deps.restore).toHaveBeenCalledOnce();
		expect(deps.takeControl).not.toHaveBeenCalled();
	});

	it('a grown PiP shrinks around the pointer as the drag starts, and does not regrow meanwhile', async () => {
		const { pw } = make();
		pw.enter();
		vi.advanceTimersByTime(400);
		expect(pw.grown).toBe(true);
		vi.mocked(api.pipResize).mockClear();
		pw.down(press(10, 10));
		pw.move(press(40, 10));
		expect(pw.grown).toBe(false);
		expect(api.pipResize).toHaveBeenCalledWith(480, 270, true);
		pw.enter();
		vi.advanceTimersByTime(1000);
		expect(pw.grown).toBe(false);
	});

	it('grows only while the grown PiP stays within half the screen', () => {
		const screen = { w: 1920, h: 1040 };
		expect(growFactor({ w: 480, h: 270 }, 1.5, screen)).toBe(1.5);
		// 2x would pass half the height (520): capped there.
		expect(growFactor({ w: 480, h: 270 }, 2, screen)).toBeCloseTo(1.926, 2);
		expect(growFactor({ w: 600, h: 338 }, 2, screen)).toBeCloseTo(1.538, 2);
		// Already big: no growth, only room for the control bar.
		expect(growFactor({ w: 1000, h: 562 }, 1.5, screen)).toBe(1);
		expect(growFactor({ w: 480, h: 270 }, 1.5, null)).toBe(1.5);
	});

	it('a press that moves is a drag, not a click', async () => {
		const { pw, deps } = make();
		pw.down(press(10, 10));
		pw.move(press(30, 10));
		pw.up(press(30, 10));
		await vi.advanceTimersByTimeAsync(600);
		expect(deps.takeControl).not.toHaveBeenCalled();
		expect(pw.active).toBe(false);
	});
});
