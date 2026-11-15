import { describe, it, expect, beforeAll, beforeEach } from 'vitest';
import { i18nReady, loadLang, setLang, LANGS } from './i18n.svelte';
import { lanName, withLanNames } from './lanName';

describe('lanName', () => {
	beforeAll(async () => {
		await i18nReady;
		await Promise.all(LANGS.map((l) => loadLang(l.value)));
	});
	beforeEach(() => setLang('tr'));

	it('keeps an announced name', () => {
		expect(lanName({ name: ' Salon PC ', platform: 'windows' })).toBe('Salon PC');
	});

	it('labels an empty name with the platform', () => {
		expect(lanName({ name: '', platform: 'windows' })).toBe('Pulsar cihazı (Windows)');
		setLang('en');
		expect(lanName({ name: '  ', platform: 'macos' })).toBe('Pulsar device (macOS)');
	});

	it('labels an empty name without a known platform', () => {
		setLang('en');
		expect(lanName({ name: '', platform: '' })).toBe('Pulsar device');
	});

	it('has the labels in every language', () => {
		for (const l of LANGS) {
			setLang(l.value);
			const label = lanName({ name: '', platform: 'linux' });
			expect(label).not.toBe('devices.lanUnnamedOn');
			expect(label).toContain('Linux');
			expect(lanName({ name: '', platform: 'x' })).not.toBe('devices.lanUnnamed');
		}
	});

	it('fills only the empty names of a list', () => {
		setLang('en');
		const out = withLanNames([
			{ id: '1', has_id: true, name: 'A', addr: 'a', platform: 'linux' },
			{ id: '', has_id: false, name: '', addr: 'b', platform: 'linux' }
		]);
		expect(out.map((d) => d.name)).toEqual(['A', 'Pulsar device (Linux)']);
	});
});
