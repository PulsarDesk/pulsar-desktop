import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import Ds5Settings from './Ds5Settings.svelte';
import { api } from '#lib/api.js';
import { ui, DS5_DEFAULTS, normalizeDs5 } from '#lib/settings.svelte.js';

describe('DualSense settings model', () => {
	it('defaults: on, every feature Otomatik, auto-degrade on, 64 ms', () => {
		expect(DS5_DEFAULTS).toEqual({
			enabled: true,
			triggers: 'auto',
			haptics: 'auto',
			speaker: 'auto',
			mic: 'auto',
			leds: 'auto',
			rumble: 'auto',
			headset: 'auto',
			autoDegrade: true,
			jitterMs: 64
		});
		expect(ui.ds5.enabled).toBe(true);
	});

	it('normalizes old or broken saved blocks', () => {
		expect(normalizeDs5(undefined)).toEqual(DS5_DEFAULTS);
		const n = normalizeDs5({ speaker: 'off', haptics: 'bogus', autoDegrade: false, jitterMs: 9999 });
		expect(n.speaker).toBe('off');
		expect(n.haptics).toBe('auto');
		expect(n.autoDegrade).toBe(false);
		expect(n.jitterMs).toBe(500);
		expect(normalizeDs5({ jitterMs: 'x' }).jitterMs).toBe(64);
	});
});

describe('Ds5Settings', () => {
	beforeEach(() => {
		Object.assign(ui.ds5, DS5_DEFAULTS);
	});

	it('shows the Turkish labels and the shedding help', () => {
		render(Ds5Settings);
		expect(screen.getByText('DualSense özelliklerini kullan')).toBeTruthy();
		for (const l of ['Adaptif tetik', 'HD titreşim', 'Kol hoparlörü', 'Kol mikrofonu', 'Işık çubuğu / LED', 'Klasik titreşim', 'Kolun kulaklık girişi'])
			expect(screen.getByRole('group', { name: l })).toBeTruthy();
		expect(screen.getByText('Bağlantı zayıflayınca özellikleri otomatik kıs')).toBeTruthy();
		expect(screen.getByText(/önce hoparlör, sonra mikrofon, sonra HD titreşim kapanır/)).toBeTruthy();
		expect(screen.getByText(/en son kulaklık girişine giden ses durur/)).toBeTruthy();
		expect(screen.getByText(/oturumun sesi yalnız kulaklıktan çalar/)).toBeTruthy();
	});

	it('a feature mode change is saved and pushed to Rust', async () => {
		const spy = vi.spyOn(api, 'setDs5Settings');
		render(Ds5Settings);
		const group = screen.getByRole('group', { name: 'Kol hoparlörü' });
		const off = Array.from(group.querySelectorAll('button')).find((b) => b.textContent === 'Kapalı')!;
		await fireEvent.click(off);
		expect(ui.ds5.speaker).toBe('off');
		expect(off.getAttribute('aria-pressed')).toBe('true');
		expect(spy).toHaveBeenLastCalledWith(expect.objectContaining({ speaker: 'off', haptics: 'auto' }));
		spy.mockRestore();
	});

	it('the headset jack mode is saved and pushed to Rust', async () => {
		const spy = vi.spyOn(api, 'setDs5Settings');
		render(Ds5Settings);
		const group = screen.getByRole('group', { name: 'Kolun kulaklık girişi' });
		const on = Array.from(group.querySelectorAll('button')).find((b) => b.textContent === 'Otomatik')!;
		expect(on.getAttribute('aria-pressed')).toBe('true');
		const off = Array.from(group.querySelectorAll('button')).find((b) => b.textContent === 'Kapalı')!;
		await fireEvent.click(off);
		expect(ui.ds5.headset).toBe('off');
		expect(spy).toHaveBeenLastCalledWith(expect.objectContaining({ headset: 'off' }));
		expect(normalizeDs5({ headset: 'on' }).headset).toBe('on');
		expect(normalizeDs5({}).headset).toBe('auto');
		spy.mockRestore();
	});

	it('the auto-degrade toggle has its own setting', async () => {
		const spy = vi.spyOn(api, 'setDs5Settings');
		render(Ds5Settings);
		const tg = screen.getByRole('button', { name: 'Bağlantı zayıflayınca özellikleri otomatik kıs' });
		expect(tg.getAttribute('aria-pressed')).toBe('true');
		await fireEvent.click(tg);
		expect(ui.ds5.autoDegrade).toBe(false);
		expect(ui.ds5.enabled).toBe(true);
		expect(spy).toHaveBeenLastCalledWith(expect.objectContaining({ autoDegrade: false }));
		spy.mockRestore();
	});

	it('the master switch off disables the feature choices', async () => {
		render(Ds5Settings);
		await fireEvent.click(screen.getByRole('button', { name: 'DualSense özelliklerini kullan' }));
		expect(ui.ds5.enabled).toBe(false);
		const group = screen.getByRole('group', { name: 'HD titreşim' });
		for (const b of group.querySelectorAll('button')) expect((b as HTMLButtonElement).disabled).toBe(true);
	});

	it('the advanced jitter buffer is clamped', async () => {
		render(Ds5Settings);
		await fireEvent.click(screen.getByRole('button', { name: /Gelişmiş/ }));
		const input = screen.getByLabelText('Titreşim tamponu (ms)') as HTMLInputElement;
		expect(input.value).toBe('64');
		input.value = '2';
		await fireEvent.change(input);
		expect(ui.ds5.jitterMs).toBe(11);
	});
});
