import { describe, it, expect } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import GeneralTab from './GeneralTab.svelte';
import { ui } from '#lib/settings.svelte.js';

describe('Settings → Genel → touchpad as mouse', () => {
	it('toggles the controller touchpad-as-mouse setting', async () => {
		ui.touchpadAsMouse = true;
		render(GeneralTab);
		const btn = await screen.findByRole('button', { name: 'Dokunmatik yüzey fare olarak' });
		expect(btn).toHaveAttribute('aria-pressed', 'true');
		await fireEvent.click(btn);
		expect(ui.touchpadAsMouse).toBe(false);
		expect(btn).toHaveAttribute('aria-pressed', 'false');
		await fireEvent.click(btn);
		expect(ui.touchpadAsMouse).toBe(true);
	});
});
