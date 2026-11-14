import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';

const screenPermission = vi.fn();
const openScreenPermissionSettings = vi.fn();

vi.mock('#lib/api.js', () => ({
	api: {
		screenPermission: (request: boolean) => screenPermission(request),
		openScreenPermissionSettings: () => openScreenPermissionSettings()
	},
	onHostScreenPermission: () => Promise.resolve(() => {})
}));

import ScreenPermissionNotice from './ScreenPermissionNotice.svelte';

describe('ScreenPermissionNotice', () => {
	beforeEach(() => {
		screenPermission.mockReset();
		openScreenPermissionSettings.mockReset().mockResolvedValue(undefined);
	});

	it('stays hidden where the permission does not exist or is granted', async () => {
		screenPermission.mockResolvedValue('na');
		const { unmount } = render(ScreenPermissionNotice);
		await waitFor(() => expect(screenPermission).toHaveBeenCalledWith(false));
		expect(screen.queryByRole('alert')).toBeNull();
		unmount();
		screenPermission.mockResolvedValue('granted');
		render(ScreenPermissionNotice);
		await waitFor(() => expect(screenPermission).toHaveBeenCalledTimes(2));
		expect(screen.queryByRole('alert')).toBeNull();
	});

	it('explains a missing permission, asks for it and then asks for a restart', async () => {
		screenPermission.mockResolvedValueOnce('denied').mockResolvedValueOnce('denied');
		render(ScreenPermissionNotice);
		expect(await screen.findByText('Ekran kaydı izni yok')).toBeInTheDocument();
		await fireEvent.click(screen.getByRole('button', { name: 'İzin iste' }));
		expect(screenPermission).toHaveBeenLastCalledWith(true);
		expect(await screen.findByText(/yeniden başlatın/)).toBeInTheDocument();
		expect(screen.queryByRole('button', { name: 'İzin iste' })).toBeNull();
		await fireEvent.click(screen.getByRole('button', { name: 'Sistem Ayarlarını aç' }));
		expect(openScreenPermissionSettings).toHaveBeenCalledOnce();
	});
});
