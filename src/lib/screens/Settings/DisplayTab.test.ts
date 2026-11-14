import { describe, it, expect } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import DisplayTab from './DisplayTab.svelte';
import { ui } from '#lib/settings.svelte.js';

describe('Settings → Görüntü → touchpad gestures', () => {
	it('toggles sending 3/4-finger touchpad gestures to the host (default on)', async () => {
		ui.touchpadGestures = true;
		render(DisplayTab, {
			props: { config: null, detected: [], saveConfig: () => {}, toggleTransmit: () => {}, toggleMute: () => {} }
		});
		const btn = await screen.findByRole('button', { name: 'Touchpad hareketlerini gönder' });
		expect(btn).toHaveAttribute('aria-pressed', 'true');
		await fireEvent.click(btn);
		expect(ui.touchpadGestures).toBe(false);
		expect(btn).toHaveAttribute('aria-pressed', 'false');
		await fireEvent.click(btn);
		expect(ui.touchpadGestures).toBe(true);
	});
});
