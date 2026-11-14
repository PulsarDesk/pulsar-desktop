import { describe, expect, it } from 'vitest';
import tr from './i18n/tr.json';
import en from './i18n/en.json';
import ru from './i18n/ru.json';
import kk from './i18n/kk.json';
import { sharedErrorKey, sharedNoAccessKey } from './sharedMode';

describe('shared mode error copy', () => {
	it('maps backend codes per platform', () => {
		expect(sharedErrorKey('SHARED_ELEVATION', false)).toBe('shared.needAdmin');
		expect(sharedErrorKey('SHARED_ELEVATION', true)).toBe('shared.needAdminUnix');
		expect(sharedErrorKey('SHARED_CANCELLED', true)).toBe('shared.cancelled');
		expect(sharedErrorKey('SHARED_DENIED', true)).toBe('shared.denied');
		expect(sharedErrorKey('SHARED_DIR:/var/lib/pulsar: owned by uid 1000', true)).toBe('shared.failed');
		expect(sharedNoAccessKey(false)).toBe('shared.noAccess');
		expect(sharedNoAccessKey(true)).toBe('shared.noAccessUnix');
	});

	it('every key exists in every language', () => {
		const keys = [
			'settings.sharedModeDescUnix',
			...['SHARED_ELEVATION', 'SHARED_CANCELLED', 'SHARED_DENIED', 'x'].flatMap((m) => [
				sharedErrorKey(m, true),
				sharedErrorKey(m, false)
			]),
			sharedNoAccessKey(true),
			sharedNoAccessKey(false)
		];
		for (const dict of [tr, en, ru, kk] as Record<string, string>[]) {
			for (const k of keys) expect(dict[k], k).toBeTruthy();
		}
	});
});
