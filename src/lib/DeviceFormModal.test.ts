import { describe, it, expect, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import DeviceFormModal from './DeviceFormModal.svelte';
import { GamepadNav } from './gamepadNav.svelte';
import { _reset, addPeer } from './peers.svelte.js';
import { api } from './api.js';

function box<T extends HTMLElement>(el: T, left: number, top: number, width: number, height: number) {
	el.getBoundingClientRect = () =>
		({ left, top, width, height, right: left + width, bottom: top + height, x: left, y: top }) as DOMRect;
	return el;
}

const pwInput = () => screen.getByLabelText('Şifre') as HTMLInputElement;
const toggle = () => screen.getByRole('button', { name: /Şifreyi (göster|gizle)/ });

describe('Device dialog — saved password', () => {
	beforeEach(() => _reset());

	it('masks the password with a show/hide toggle', async () => {
		render(DeviceFormModal, { props: { device: null, onClose: () => {} } });
		expect(pwInput().type).toBe('password');
		await fireEvent.click(toggle());
		expect(pwInput().type).toBe('text');
		expect(toggle()).toHaveAttribute('aria-pressed', 'true');
		expect(toggle()).toHaveAccessibleName('Şifreyi gizle');
		await fireEvent.click(toggle());
		expect(pwInput().type).toBe('password');
	});

	it('saves the password with a new device (Rust store, not localStorage)', async () => {
		render(DeviceFormModal, { props: { device: null, onClose: () => {} } });
		await fireEvent.input(screen.getByLabelText('ID veya IP'), { target: { value: '111222333' } });
		await fireEvent.input(pwInput(), { target: { value: 'gizli-1' } });
		await fireEvent.click(screen.getByRole('button', { name: 'Ekle' }));
		await waitFor(async () => expect(await api.getDevicePassword('111222333')).toBe('gizli-1'));
		expect(JSON.stringify({ ...localStorage })).not.toContain('gizli-1');
	});

	it('loads the saved password when editing and moves it with a changed id', async () => {
		addPeer('Ofis', '222333444');
		await api.setDevicePassword('222333444', 'eski');
		render(DeviceFormModal, { props: { device: { id: '222333444', name: 'Ofis' }, onClose: () => {} } });
		await waitFor(() => expect(pwInput().value).toBe('eski'));
		await fireEvent.input(screen.getByLabelText('ID veya IP'), { target: { value: '222333445' } });
		await fireEvent.click(screen.getByRole('button', { name: 'Kaydet' }));
		await waitFor(async () => expect(await api.getDevicePassword('222333445')).toBe('eski'));
		expect(await api.getDevicePassword('222333444')).toBe('');
	});

	it('has no pairing row while adding (no target yet)', () => {
		render(DeviceFormModal, { props: { device: null, onClose: () => {} } });
		expect(screen.queryByRole('button', { name: /Eşleştir/ })).not.toBeInTheDocument();
	});
});

describe('Device dialog — pairing', () => {
	beforeEach(() => _reset());

	it('pairs, shows the badge, and unpairs again', async () => {
		addPeer('Salon', '333444555');
		render(DeviceFormModal, { props: { device: { id: '333444555', name: 'Salon' }, onClose: () => {} } });
		await fireEvent.click(screen.getByRole('button', { name: 'Eşleştir' }));
		await screen.findByText('Eşleştirildi.');
		expect(screen.getByText('Eşleşti')).toBeInTheDocument();
		expect((await api.deviceCreds(['333444555']))['333444555'].paired).toBe(true);

		await fireEvent.click(screen.getByRole('button', { name: 'Eşleşmeyi kaldır' }));
		await screen.findByText('Eşleşme kaldırıldı.');
		expect(screen.queryByText('Eşleşti')).not.toBeInTheDocument();
		expect(screen.getByRole('button', { name: 'Eşleştir' })).toBeInTheDocument();
		expect((await api.deviceCreds(['333444555']))['333444555'].paired).toBe(false);
	});

	it('shows a paired device as paired when the dialog opens', async () => {
		addPeer('Salon', '444555666');
		await api.pairDevice('444555666');
		render(DeviceFormModal, { props: { device: { id: '444555666', name: 'Salon' }, onClose: () => {} } });
		expect(await screen.findByText('Eşleşti')).toBeInTheDocument();
		expect(screen.getByRole('button', { name: 'Eşleşmeyi kaldır' })).toBeInTheDocument();
	});
});

describe('Device dialog — gamepad nav', () => {
	beforeEach(() => {
		_reset();
		Element.prototype.scrollIntoView ??= () => {};
	});

	it('registers the password field, its toggle and Pair with the pad nav; A toggles visibility', async () => {
		addPeer('Salon', '555666777');
		const nav = new GamepadNav();
		render(DeviceFormModal, {
			props: { device: { id: '555666777', name: 'Salon' }, onClose: () => {}, navItem: nav.item }
		});
		const pw = pwInput();
		const tg = toggle();
		const pair = screen.getByRole('button', { name: 'Eşleştir' });
		for (const el of [pw, tg, pair]) expect(el).toHaveAttribute('data-navitem');
		// Lay the row out: field, then its eye button to the right, Pair below.
		box(screen.getByLabelText('ID veya IP'), 200, 100, 260, 36);
		box(pw, 200, 160, 220, 36);
		box(tg, 430, 160, 30, 36);
		box(pair, 380, 240, 100, 36);
		nav.move('down'); // the first input only reveals the ring
		nav.focus(pw);
		nav.move('right');
		expect(tg).toHaveAttribute('data-navfocus');
		nav.activate();
		await waitFor(() => expect(pw.type).toBe('text'));
		nav.move('down');
		expect(pair).toHaveAttribute('data-navfocus');
	});
});
