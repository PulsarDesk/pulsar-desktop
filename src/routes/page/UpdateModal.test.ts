import { describe, it, expect, afterEach } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import UpdateModal from './UpdateModal.svelte';
import { update } from '#lib/update.svelte.js';

describe('Update modal', () => {
	const show = (installable = true) => {
		update.from = '0.16.0';
		update.to = '0.17.0';
		update.installable = installable;
		update.phase = 'idle';
		update.available = true;
		update.open = true;
	};
	afterEach(() => {
		update.available = false;
		update.open = false;
		update.kind = 'inPlace';
		update.pkgNote = '';
	});

	it('warns that installing closes the open sessions', () => {
		show();
		render(UpdateModal, { props: { openSessions: 2 } });
		expect(screen.getByText('Kurulum Pulsar\'ı yeniden başlatır: açık 2 oturum kapanacak.')).toBeInTheDocument();
	});

	it('says nothing about sessions when none is open', () => {
		show();
		render(UpdateModal, { props: { openSessions: 0 } });
		expect(screen.queryByText(/oturum kapanacak/)).not.toBeInTheDocument();
	});

	it('says nothing about sessions when this install cannot update itself', () => {
		show(false);
		render(UpdateModal, { props: { openSessions: 1 } });
		expect(screen.queryByText(/oturum kapanacak/)).not.toBeInTheDocument();
	});

	it('a Linux .deb install says the package manager installs it after a password', () => {
		show();
		update.kind = 'deb';
		render(UpdateModal, { props: { openSessions: 0 } });
		expect(screen.getByText(/Bu kurulum bir \.deb paketi/)).toBeInTheDocument();
		expect(screen.getByRole('button', { name: 'Kur' })).toBeEnabled();
	});

	it('explains a hand-off to the software installer', () => {
		show();
		update.kind = 'rpm';
		update.pkgNote = 'opened';
		render(UpdateModal, { props: { openSessions: 0 } });
		expect(screen.getByText(/yazılım yükleyicisinde açıldı/)).toBeInTheDocument();
	});

	it('in-place installs show no package hint', () => {
		show();
		render(UpdateModal, { props: { openSessions: 0 } });
		expect(screen.queryByText(/paketi:/)).not.toBeInTheDocument();
	});
});
