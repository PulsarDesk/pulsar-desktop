import { describe, it, expect, vi } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import type { PulsarToast } from '#lib/types.js';

const list: PulsarToast[] = [
	{ id: 1, kind: 'file-in', name: 'rapor.pdf', peer: 'Ali', path: 'C:/x/rapor.pdf' },
	{
		id: 2,
		kind: 'pad-off',
		name: 'Bu Pulsar sürümü sanal kol iznine (imza) sahip değil; kol girişi oyuna gitmiyor',
		peer: '',
		path: null
	}
];

vi.mock('#lib/api.js', () => ({
	api: {
		toastList: () => Promise.resolve(list),
		toastOpen: () => Promise.resolve(),
		toastClose: () => Promise.resolve()
	},
	onToasts: () => Promise.resolve(() => {})
}));

import ToastWindow from './ToastWindow.svelte';

describe('Toast window', () => {
	it('shows a missing virtual controller notice with its cause', async () => {
		render(ToastWindow);
		expect(await screen.findByText('Sanal kol oluşturulamadı')).toBeInTheDocument();
		const cause = screen.getByText(/sanal kol iznine/);
		expect(cause).toHaveClass('wrap');
		// The full sentence is the card's tooltip (the text may be clamped to two lines).
		expect(cause.closest('.card')).toHaveAttribute('title', list[1].name);
		// No "X gönderdi" line for a notice nobody sent.
		expect(screen.getAllByText(/gönderdi/)).toHaveLength(1);
	});
});
