import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { flushSync } from 'svelte';

vi.mock('#lib/api.js', () => {
	const ok = () => Promise.resolve();
	return { api: { setOverlay: vi.fn(ok) } };
});

import { api } from '#lib/api.js';
import { SessionUi } from './ui.svelte.js';

function make() {
	let ui!: SessionUi;
	const stopControl = vi.fn();
	const takeControl = vi.fn();
	const stop = $effect.root(() => {
		ui = new SessionUi({
			playId: () => 1,
			stopControl,
			takeControl,
			onEnd: () => {},
			onToggleFullscreen: () => {},
			resetPanel: () => {}
		});
	});
	return { ui, stop, stopControl, takeControl };
}

const press = (init: KeyboardEventInit) => {
	const e = new KeyboardEvent('keydown', { bubbles: true, cancelable: true, ...init });
	window.dispatchEvent(e);
	return e;
};
const combo = (more: KeyboardEventInit = {}) => press({ code: 'KeyM', key: 'M', ctrlKey: true, shiftKey: true, ...more });
/** Let the overlay pump (one awaited `setOverlay` per step) settle. */
const settle = async () => {
	await vi.advanceTimersByTimeAsync(0);
	flushSync();
};

// The Pulsar menu (Ctrl+Shift+M). Opening it releases control, and the webview key path only
// reads its combos while controlling — so the press that should close it has to be read here.
describe('SessionUi overlay combo', () => {
	beforeEach(() => {
		vi.useFakeTimers();
		vi.setSystemTime(10_000);
		vi.mocked(api.setOverlay).mockClear();
	});
	afterEach(() => {
		vi.useRealTimers();
	});

	it('Ctrl+Shift+M closes the open overlay and takes control again', async () => {
		const { ui, stop, stopControl, takeControl } = make();
		ui.toggleOverlay();
		await settle();
		expect(ui.overlayOpen).toBe(true);
		expect(stopControl).toHaveBeenCalledTimes(1);
		expect(takeControl).not.toHaveBeenCalled();

		vi.advanceTimersByTime(1000);
		const e = combo();
		await settle();
		expect(ui.overlayOpen).toBe(false);
		expect(e.defaultPrevented).toBe(true);
		expect(takeControl).toHaveBeenCalledTimes(1);
		expect(vi.mocked(api.setOverlay).mock.calls).toEqual([
			[1, true],
			[1, false]
		]);
		stop();
	});

	it('the OS hook reporting the same press does not open it again', async () => {
		const { ui, stop } = make();
		ui.toggleOverlay();
		await settle();
		vi.advanceTimersByTime(1000);
		combo(); // the webview's key …
		ui.toggleOverlay(); // … and the hook's `overlay-toggle` for the same press
		await settle();
		expect(ui.overlayOpen).toBe(false);
		stop();
	});

	it('Escape and the overlay\'s own close leave control released', async () => {
		const { ui, stop, takeControl } = make();
		ui.toggleOverlay();
		await settle();
		press({ key: 'Escape', code: 'Escape' });
		await settle();
		expect(ui.overlayOpen).toBe(false);

		vi.advanceTimersByTime(1000);
		ui.toggleOverlay();
		await settle();
		ui.closeOverlay(); // the renderer's `ov close` (its close button, B on the pad)
		await settle();
		expect(ui.overlayOpen).toBe(false);
		expect(takeControl).not.toHaveBeenCalled();
		stop();
	});

	it('a toggle that reopens it before the close applied takes nothing', async () => {
		const { ui, stop, takeControl } = make();
		ui.toggleOverlay();
		await settle();
		vi.advanceTimersByTime(1000);
		ui.toggleOverlay(); // close …
		vi.advanceTimersByTime(300);
		ui.toggleOverlay(); // … and open again while the close is in flight
		await settle();
		expect(ui.overlayOpen).toBe(true);
		expect(takeControl).not.toHaveBeenCalled();
		stop();
	});

	it('leaves the key alone while the overlay is closed, and ignores autorepeat', async () => {
		const { ui, stop } = make();
		expect(combo().defaultPrevented).toBe(false);
		await settle();
		expect(ui.overlayOpen).toBe(false);

		ui.toggleOverlay();
		await settle();
		vi.advanceTimersByTime(1000);
		combo({ repeat: true }); // the key still held from the press that opened it
		press({ code: 'KeyM', key: 'm' }); // a bare "m" (typed into the chat composer)
		await settle();
		expect(ui.overlayOpen).toBe(true);
		stop();
	});
});
