import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import Chrome from './Chrome.svelte';

const base = { title: '', dark: false, onToggleTheme: () => {}, gaming: false };

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
