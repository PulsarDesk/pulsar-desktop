import { describe, it, expect, vi } from 'vitest';
import { closeOnEscape } from './escapeStack';

const esc = () => window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', cancelable: true }));

describe('closeOnEscape', () => {
	it('closes only the topmost modal, then the one beneath once that is gone', () => {
		const below = vi.fn();
		const top = vi.fn();
		const unBelow = closeOnEscape(below);
		const unTop = closeOnEscape(top);

		esc();
		expect(top).toHaveBeenCalledOnce();
		expect(below).not.toHaveBeenCalled();

		unTop();
		esc();
		expect(below).toHaveBeenCalledOnce();
		unBelow();
	});

	it('stops the event so a later window listener (gaming nav back) does not also fire', () => {
		const later = vi.fn();
		window.addEventListener('keydown', later);
		const un = closeOnEscape(() => {});
		esc();
		expect(later).not.toHaveBeenCalled();
		un();
		esc();
		expect(later).toHaveBeenCalledOnce();
		window.removeEventListener('keydown', later);
	});

	it('ignores other keys and does nothing with no modal open', () => {
		const close = vi.fn();
		const un = closeOnEscape(close);
		window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter' }));
		expect(close).not.toHaveBeenCalled();
		un();
		expect(() => esc()).not.toThrow();
	});
});
