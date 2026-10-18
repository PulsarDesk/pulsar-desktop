import { describe, it, expect, beforeEach } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import Devices from './Devices.svelte';
import { _reset } from '$lib/peers.svelte';
import { modalCount } from '$lib/overlayModals.svelte';

describe('Devices screen', () => {
	beforeEach(() => _reset());

	// Regression for issue #1: opening the dialog looped its open/close effect
	// (effect_update_depth_exceeded) and left the page stuck behind the backdrop.
	const open = async () => {
		await fireEvent.click(screen.getByRole('button', { name: /Cihaz ekle/ }));
		expect(screen.getByRole('dialog', { name: 'Cihaz ekle' })).toBeInTheDocument();
		expect(modalCount.n).toBe(1);
	};
	const expectClosed = () => {
		expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
		expect(modalCount.n).toBe(0);
	};

	it.each([
		['Escape', () => fireEvent.keyDown(window, { key: 'Escape' })],
		['the × button', () => fireEvent.click(screen.getByRole('button', { name: 'Kapat' }))],
		['Cancel', () => fireEvent.click(screen.getByRole('button', { name: 'Vazgeç' }))]
	])('opens the add-device dialog and closes it with %s', async (_, close) => {
		render(Devices, { props: { onConnect: () => {} } });
		await open();
		await close();
		expectClosed();
		await open(); // and it opens again afterwards
	});

	it('caps a typed relay id at 9 digits, grouped; addresses pass through', async () => {
		render(Devices, { props: { onConnect: () => {} } });
		await fireEvent.click(screen.getByRole('button', { name: /Cihaz ekle/ }));
		const id = screen.getByLabelText('ID veya IP') as HTMLInputElement;

		await fireEvent.input(id, { target: { value: '64172439512345' } });
		expect(id.value).toBe('641 724 395');
		await fireEvent.input(id, { target: { value: '641 724 3951' } }); // one more digit typed
		expect(id.value).toBe('641 724 395');
		await fireEvent.input(id, { target: { value: '192.168.1.42:21118' } });
		expect(id.value).toBe('192.168.1.42:21118');
	});
});
