import { describe, it, expect } from 'vitest';
import { render, screen, fireEvent, within } from '@testing-library/svelte';
import PairedDevices from './PairedDevices.svelte';

describe('Settings → Güvenlik → paired devices', () => {
	// The browser mock's pairing store is shared across tests: cancel first, then remove.
	it('keeps the device when the confirm is cancelled', async () => {
		render(PairedDevices);
		await fireEvent.click(await screen.findByRole('button', { name: 'Kaldır: Salon Laptopu' }));
		const dialog = await screen.findByRole('dialog', { name: 'Eşleşmeyi kaldır' });
		await fireEvent.click(within(dialog).getByRole('button', { name: 'İptal' }));
		expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
		expect(screen.getByText('Salon Laptopu')).toBeInTheDocument();
	});

	it('lists the paired clients and revokes one after confirming', async () => {
		render(PairedDevices);
		expect(await screen.findByText('Salon Laptopu')).toBeInTheDocument();
		expect(screen.getByText(/641 724 395 · abab abab abab abab/)).toBeInTheDocument();
		await fireEvent.click(screen.getByRole('button', { name: 'Kaldır: Salon Laptopu' }));
		// Nothing is removed until the dialog is confirmed.
		const dialog = await screen.findByRole('dialog', { name: 'Eşleşmeyi kaldır' });
		expect(screen.getByText('Salon Laptopu')).toBeInTheDocument();
		await fireEvent.click(within(dialog).getByRole('button', { name: 'Kaldır' }));
		expect(await screen.findByText('Eşleşmiş cihaz yok.')).toBeInTheDocument();
		expect(screen.queryByText('Salon Laptopu')).not.toBeInTheDocument();
	});
});
