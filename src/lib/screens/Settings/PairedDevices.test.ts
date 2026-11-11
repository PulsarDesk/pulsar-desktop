import { describe, it, expect } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import PairedDevices from './PairedDevices.svelte';

describe('Settings → Güvenlik → paired devices', () => {
	it('lists the paired clients and revokes one', async () => {
		render(PairedDevices);
		expect(await screen.findByText('Salon Laptopu')).toBeInTheDocument();
		expect(screen.getByText(/641 724 395 · abab abab abab abab/)).toBeInTheDocument();
		await fireEvent.click(screen.getByRole('button', { name: 'Kaldır: Salon Laptopu' }));
		expect(await screen.findByText('Eşleşmiş cihaz yok.')).toBeInTheDocument();
		expect(screen.queryByText('Salon Laptopu')).not.toBeInTheDocument();
	});
});
