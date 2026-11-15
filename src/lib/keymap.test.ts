import { describe, it, expect } from 'vitest';
import { EVDEV, evdevCode } from './keymap.js';

// Mirrors pulsar-core `input::keytables::CLIENT_KEYS`: every key a host table types. The core
// tests check each host (Windows, macOS; Linux types raw evdev) against that list, so the
// webview must reach all of it.
const CLIENT_KEYS = [
	1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
	27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50,
	51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71, 72, 73, 74,
	75, 76, 77, 78, 79, 80, 81, 82, 83, 86, 87, 88, 89, 92, 93, 94, 95, 96, 97, 98, 99, 100, 102,
	103, 104, 105, 106, 107, 108, 109, 110, 111, 117, 119, 121, 122, 123, 124, 125, 126, 127, 183,
	184, 185, 186, 187, 188, 189, 190, 191, 192, 193, 194
];

describe('webview keymap', () => {
	it('reaches every key the hosts type, each code once', () => {
		const codes = Object.values(EVDEV);
		expect(new Set(codes).size).toBe(codes.length);
		expect([...codes].sort((a, b) => a - b)).toEqual(CLIENT_KEYS);
	});

	it('maps the ISO / JIS / PC extras', () => {
		expect(evdevCode('IntlBackslash')).toBe(86);
		expect(evdevCode('IntlRo')).toBe(89);
		expect(evdevCode('IntlYen')).toBe(124);
		expect(evdevCode('PrintScreen')).toBe(99);
		expect(evdevCode('Pause')).toBe(119);
		expect(evdevCode('F13')).toBe(183);
		expect(evdevCode('F24')).toBe(194);
		expect(evdevCode('Lang1')).toBe(122);
		expect(evdevCode('NoSuchKey')).toBe(0);
	});
});
