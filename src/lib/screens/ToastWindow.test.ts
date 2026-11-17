import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import type { PulsarToast } from '#lib/types.js';

const list: PulsarToast[] = [
	{ id: 1, kind: 'file-in', name: 'rapor.pdf', peer: 'Ali', path: 'C:/x/rapor.pdf' },
	{
		id: 2,
		kind: 'pad-off',
		name: 'Bu Pulsar sürümü sanal kol iznine (imza) sahip değil; kol girişi oyuna gitmiyor',
		peer: '',
		path: null
	},
	{
		id: 3,
		kind: 'input-off',
		name: 'Erişilebilirlik izni yok; uzaktan fare ve klavye çalışmaz. Açmak için tıkla',
		peer: '',
		path: null
	},
	{
		id: 4,
		kind: 'input-on',
		name: 'uinput kuralı kuruldu; uzaktan fare ve klavye artık çalışıyor',
		peer: '',
		path: null
	},
	{ id: 5, kind: 'call-in', name: '', peer: 'Salon PC', path: null, since: 0 },
	{ id: 6, kind: 'call-on', name: '', peer: 'Ofis', path: null, since: Date.now() - 65_000 }
];

const toastCall = vi.fn(() => Promise.resolve());

vi.mock('#lib/api.js', () => ({
	api: {
		toastList: () => Promise.resolve(list),
		toastOpen: () => Promise.resolve(),
		toastClose: () => Promise.resolve(),
		toastCall: (...a: unknown[]) => (toastCall as (...a: unknown[]) => Promise<void>)(...a)
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

	it('shows a remote input notice with its cause as a wrapped line and tooltip', async () => {
		render(ToastWindow);
		expect(await screen.findByText('Uzaktan kontrol çalışmıyor')).toBeInTheDocument();
		const cause = screen.getByText(/Erişilebilirlik izni yok/);
		expect(cause).toHaveClass('wrap');
		expect(cause.closest('.card')).toHaveAttribute('title', list[2].name);
	});

	it('shows a fixed remote input notice without the error colour', async () => {
		render(ToastWindow);
		expect(await screen.findByText('Uzaktan kontrol çalışıyor')).toBeInTheDocument();
		const line = screen.getByText(/kuralı kuruldu/);
		expect(line).toHaveClass('wrap');
		expect(line.closest('.card')?.querySelector('.ic')).not.toHaveClass('del');
	});

	it('rings with Answer / Decline on an incoming call card', async () => {
		render(ToastWindow);
		expect(await screen.findByText('Gelen arama')).toBeInTheDocument();
		expect(screen.getByText('Salon PC')).toBeInTheDocument();
		expect(screen.getByText('Salon PC').closest('.card')).toHaveClass('ringing');
		await fireEvent.click(screen.getByRole('button', { name: 'Yanıtla' }));
		expect(toastCall).toHaveBeenLastCalledWith(5, 'accept');
		await fireEvent.click(screen.getByRole('button', { name: 'Reddet' }));
		expect(toastCall).toHaveBeenLastCalledWith(5, 'decline');
	});

	it('shows the talk time and a hang-up button on an active call', async () => {
		render(ToastWindow);
		expect(await screen.findByText(/^Görüşmede · 1:0[5-9]$/)).toBeInTheDocument();
		await fireEvent.click(screen.getByRole('button', { name: 'Görüşmeyi bitir' }));
		expect(toastCall).toHaveBeenLastCalledWith(6, 'hangup');
	});
});
