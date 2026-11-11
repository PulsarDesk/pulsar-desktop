import { describe, it, expect, beforeEach } from 'vitest';
import { render, screen, fireEvent, within } from '@testing-library/svelte';
import GamingHome from './GamingHome.svelte';
import { GamepadNav } from '#lib/gamepadNav.svelte.js';
import { _reset, recordConnection, addPeer, isSaved } from '#lib/peers.svelte.js';

const props = () => ({ nav: new GamepadNav(), onPickHost: () => {} });
/** The recent card's star (the Devices card's × has the same label, but no aria-pressed). */
const star = (name: RegExp) =>
	screen.getAllByRole('button', { name }).find((b) => b.hasAttribute('aria-pressed'))!;

describe('Gaming home — devices star', () => {
	beforeEach(() => {
		_reset();
		recordConnection('641724395', 'Salon', 'pc', 'game');
	});

	it('starring a recent saves it at once', async () => {
		render(GamingHome, { props: props() });
		await fireEvent.click(star(/Cihazlara ekle/));
		expect(isSaved('641724395')).toBe(true);
		expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
	});

	it('un-starring asks first; cancel keeps the device, confirm removes it', async () => {
		addPeer('Salon', '641724395');
		render(GamingHome, { props: props() });

		await fireEvent.click(star(/Cihazlardan kaldır/));
		let dlg = screen.getByRole('dialog', { name: 'Cihazlardan kaldır' });
		expect(dlg).toHaveAttribute('data-navmodal'); // pad-navigable like the other gaming modals
		expect(within(dlg).getByText(/"Salon" cihazlardan kaldırılsın mı/)).toBeInTheDocument();
		await fireEvent.click(within(dlg).getByRole('button', { name: 'Vazgeç' }));
		expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
		expect(isSaved('641724395')).toBe(true);

		await fireEvent.click(star(/Cihazlardan kaldır/));
		dlg = screen.getByRole('dialog', { name: 'Cihazlardan kaldır' });
		await fireEvent.click(within(dlg).getByRole('button', { name: 'Kaldır' }));
		expect(isSaved('641724395')).toBe(false);
	});

	it('un-star confirm closes on Escape without removing', async () => {
		addPeer('Salon', '641724395');
		render(GamingHome, { props: props() });
		await fireEvent.click(star(/Cihazlardan kaldır/));
		await fireEvent.keyDown(window, { key: 'Escape' });
		expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
		expect(isSaved('641724395')).toBe(true);
	});

	it('marks every card as a nav group with the card as its item', () => {
		addPeer('Salon', '641724395');
		const { container } = render(GamingHome, { props: props() });
		const rows = container.querySelectorAll('[data-navgroup]');
		expect(rows.length).toBe(2); // the recent + the Devices entry
		for (const r of rows) {
			expect(r.querySelectorAll('[data-navcard]').length).toBe(1);
			// The card's own action is its first inner button (A, A = connect).
			expect(r.querySelector('.ractions button')).toHaveAccessibleName('Bağlan');
		}
	});
});
