import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import Tabs from './Tabs.svelte';

const session = (tabId: number, pip?: string) => ({
	tabId,
	phase: 'active' as const,
	target: { name: `PC ${tabId}`, id: `${tabId}${tabId}${tabId} 000 000` },
	mode: 'remote' as const,
	pip
});
const base = { activeTab: 1, onSelect: () => {}, onEnd: () => {}, onRename: () => {} };

describe('tab right-click menu (picture-in-picture, issue #18)', () => {
	it('opens a live session in a small window', async () => {
		const onPip = vi.fn();
		render(Tabs, { props: { ...base, sessions: [session(1)], canPip: () => true, onPip } });
		await fireEvent.contextMenu(screen.getByText('PC 1'));
		await fireEvent.click(screen.getByRole('menuitem'));
		expect(onPip).toHaveBeenCalledWith(1);
		expect(screen.queryByRole('menuitem')).toBeNull();
	});

	it('a placeholder offers "bring back" and shows its badge', async () => {
		const onPipRestore = vi.fn();
		render(Tabs, { props: { ...base, sessions: [session(2, 'pip-1')], onPip: () => {}, onPipRestore } });
		expect(document.querySelector('.tpip')).not.toBeNull();
		await fireEvent.contextMenu(screen.getByText('PC 2'));
		await fireEvent.click(screen.getByRole('menuitem'));
		expect(onPipRestore).toHaveBeenCalledWith(2);
	});

	it('no menu when the tab cannot go to a small window', async () => {
		render(Tabs, { props: { ...base, sessions: [session(3)], canPip: () => false, onPip: () => {} } });
		await fireEvent.contextMenu(screen.getByText('PC 3'));
		expect(screen.queryByRole('menuitem')).toBeNull();
	});
});
