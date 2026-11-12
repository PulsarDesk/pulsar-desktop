import { describe, it, expect } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import Approve from './Approve.svelte';

describe('Approve popup', () => {
	it('a connection request offers Allow, view-only and Deny', () => {
		render(Approve, { props: { id: 1, peer: '482913056', pw: 'wait' } });
		expect(screen.getByText('Bağlantı isteği')).toBeInTheDocument();
		expect(screen.getByRole('button', { name: 'İzin Ver' })).toBeInTheDocument();
		expect(screen.getByRole('button', { name: 'İzin ver, kontrol edemesin' })).toBeInTheDocument();
	});

	it('a pairing request says it is permanent and has no view-only', () => {
		render(Approve, { props: { id: 2, peer: '482913056', pw: 'pair' } });
		expect(screen.getByText('Eşleştirme isteği')).toBeInTheDocument();
		expect(screen.getByText(/kalıcı olarak güvenilir/)).toBeInTheDocument();
		expect(screen.getByText('482 913 056')).toBeInTheDocument();
		expect(screen.getByRole('button', { name: 'Eşleştir' })).toBeInTheDocument();
		expect(screen.getByRole('button', { name: /Reddet/ })).toBeInTheDocument();
		expect(screen.queryByRole('button', { name: 'İzin ver, kontrol edemesin' })).not.toBeInTheDocument();
	});
});
