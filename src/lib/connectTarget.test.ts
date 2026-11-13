import { describe, it, expect } from 'vitest';
import {
	isAddr,
	fmtTarget,
	canConnectTarget,
	guardIdSpaces,
	reformatInput,
	reverseConnectBack
} from './connectTarget';

describe('connectTarget', () => {
	it('isAddr detects IP/IP:port vs relay id', () => {
		expect(isAddr('192.168.1.42')).toBe(true);
		expect(isAddr('192.168.1.42:9000')).toBe(true);
		expect(isAddr('641 724 395')).toBe(false);
		expect(isAddr('641724395')).toBe(false);
		expect(isAddr('access.panomc.com')).toBe(true);
		expect(isAddr('a')).toBe(true);
	});

	it('fmtTarget groups a relay id in threes and caps at 9 digits', () => {
		expect(fmtTarget('641724395')).toBe('641 724 395');
		expect(fmtTarget('64172439599999')).toBe('641 724 395');
		expect(fmtTarget('64-1 72')).toBe('641 72'); // non-digits stripped
	});

	it('fmtTarget keeps address chars (digits, dots, colons) and caps length', () => {
		expect(fmtTarget('192.168.1.42:9000')).toBe('192.168.1.42:9000');
		expect(fmtTarget('192.168.1.42 !')).toBe('192.168.1.42'); // junk stripped → still an addr
		expect(fmtTarget('Access.PanoMC.com:3005')).toBe('Access.PanoMC.com:3005'); // hostname kept
	});

	it('canConnectTarget: full IPv4 (optional port), or ≥6 id digits', () => {
		expect(canConnectTarget('192.168.1.42')).toBe(true);
		expect(canConnectTarget('192.168.1.42:9000')).toBe(true);
		expect(canConnectTarget('192.168.1')).toBe(false); // incomplete IPv4
		expect(canConnectTarget('641 724')).toBe(true); // 6 digits
		expect(canConnectTarget('641 7')).toBe(false); // 5 digits
		expect(canConnectTarget('access.panomc.com:3005')).toBe(true);
		expect(canConnectTarget('access.panomc.com')).toBe(true);
		expect(canConnectTarget('localhost:3005')).toBe(true);
		expect(canConnectTarget('access')).toBe(false); // no dot
		expect(canConnectTarget('access.')).toBe(false); // half-typed
	});
});

describe('id input space guard', () => {
	const input = (v: string, caret: number) => {
		const el = document.createElement('input');
		el.value = v;
		el.setSelectionRange(caret, caret);
		return el;
	};
	const key = (el: HTMLInputElement, k: string) => {
		const e = new KeyboardEvent('keydown', { key: k, cancelable: true });
		Object.defineProperty(e, 'currentTarget', { value: el });
		guardIdSpaces(e as KeyboardEvent & { currentTarget: HTMLInputElement });
		return e.defaultPrevented;
	};

	it('backspace after a group space only moves the caret', () => {
		const el = input('517 072 042', 4);
		expect(key(el, 'Backspace')).toBe(true);
		expect(el.selectionStart).toBe(3);
		expect(el.value).toBe('517 072 042');
	});

	it('delete before a group space only moves the caret', () => {
		const el = input('517 072 042', 3);
		expect(key(el, 'Delete')).toBe(true);
		expect(el.selectionStart).toBe(4);
	});

	it('backspace on a digit is left alone', () => {
		expect(key(input('517 072 042', 5), 'Backspace')).toBe(false);
	});

	it('reformatInput restores spaces in the DOM and keeps the caret', () => {
		const el = input('517072 042', 6);
		expect(reformatInput(el)).toBe('517 072 042');
		expect(el.value).toBe('517 072 042');
		expect(el.selectionStart).toBe(7);
	});

	it('reverseConnectBack never connects out from a host-only instance', () => {
		expect(reverseConnectBack(false, '641724395')).toBe('641724395');
		expect(reverseConnectBack(true, '641724395')).toBeNull();
		expect(reverseConnectBack(false, '')).toBeNull();
		expect(reverseConnectBack(false, undefined)).toBeNull();
	});
});
