import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import Chrome from './Chrome.svelte';

const base = { title: '', gaming: false };

describe('Chrome top bar mode toggle', () => {
	it('switches the mode when no connection is open', async () => {
		const onToggleMode = vi.fn();
		render(Chrome, { props: { ...base, onToggleMode } });
		const btn = screen.getByRole('button', { name: 'Oyun moduna geç / çık' });
		expect(btn).not.toBeDisabled();
		await fireEvent.click(btn);
		expect(onToggleMode).toHaveBeenCalledOnce();
	});

	it('is locked while a connection is open, and says why', async () => {
		const onToggleMode = vi.fn();
		render(Chrome, { props: { ...base, onToggleMode, modeLocked: true } });
		const btn = screen.getByRole('button', { name: 'Oyun moduna geç / çık' });
		expect(btn).toBeDisabled();
		expect(btn).toHaveAttribute('title', 'Bağlantı açıkken mod değiştirilemez');
		await fireEvent.click(btn);
		expect(onToggleMode).not.toHaveBeenCalled();
	});
});

describe('Chrome update badge', () => {
	it('hands the click to the shell (which shows the modal on Home)', async () => {
		const { update } = await import('#lib/update.svelte.js');
		update.available = true;
		update.open = false;
		const onUpdate = vi.fn();
		render(Chrome, { props: { ...base, onToggleMode: () => {}, onUpdate } });
		await fireEvent.click(screen.getByRole('button', { name: 'Güncelleme mevcut' }));
		expect(onUpdate).toHaveBeenCalledOnce();
		update.available = false;
	});
});

describe('Chrome keep-on-top pin (issue #18)', () => {
	it('toggles the window pin and shows its state', async () => {
		const onTogglePin = vi.fn();
		render(Chrome, { props: { ...base, onToggleMode: () => {}, onTogglePin, pinned: true } });
		const btn = screen.getByRole('button', { name: 'Üstte tut' });
		expect(btn).toHaveAttribute('aria-pressed', 'true');
		expect(btn).toHaveAttribute('title', 'Üstte tutmayı bırak');
		await fireEvent.click(btn);
		expect(onTogglePin).toHaveBeenCalledOnce();
	});

	it('is absent when the feature is off, and hidden in fullscreen', () => {
		const { unmount } = render(Chrome, { props: { ...base, onToggleMode: () => {} } });
		expect(screen.queryByRole('button', { name: 'Üstte tut' })).toBeNull();
		unmount();
		render(Chrome, {
			props: { ...base, onToggleMode: () => {}, onTogglePin: () => {}, onToggleFullscreen: () => {}, fullscreen: true }
		});
		expect(screen.queryByRole('button', { name: 'Üstte tut' })).toBeNull();
	});

	it('warns on Wayland that the desktop may ignore it', () => {
		render(Chrome, { props: { ...base, onToggleMode: () => {}, onTogglePin: () => {}, pinMaybeIgnored: true } });
		expect(screen.getByRole('button', { name: 'Üstte tut' }).getAttribute('title')).toContain('Wayland');
	});
});
