import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import StatusBar from './StatusBar.svelte';

const base = {
	fields: ['fps'],
	conn: 'direct' as const,
	res: '1920x1080',
	fps: 60,
	codec: 'H.265',
	encoder: 'NVENC',
	decoder: 'D3D11',
	latencyMs: 12,
	decodeMs: 2,
	mbps: 20,
	lossPct: 0
};

describe('StatusBar latency', () => {
	it('shows the jitter after the latency once it is measured', () => {
		const { unmount } = render(StatusBar, { ...base, fields: ['latency'] });
		expect(screen.getByText('12 ms')).toBeTruthy();
		unmount();
		render(StatusBar, { ...base, fields: ['latency'], jitterMs: 0.6 });
		expect(screen.getByText('12 ms (±0.6 ms)')).toBeTruthy();
	});
});

describe('StatusBar audio buttons (issue #9)', () => {
	it('shows the picked buttons with their state and toggles them', async () => {
		const onSpeaker = vi.fn();
		const onMic = vi.fn();
		render(StatusBar, {
			...base,
			buttons: ['speaker', 'mic'],
			speakerOn: true,
			micOn: false,
			micAvailable: true,
			onSpeaker,
			onMic
		});
		const speaker = screen.getByRole('button', { name: /Karşı tarafın sesi açık/ });
		expect(speaker.getAttribute('aria-pressed')).toBe('true');
		const mic = screen.getByRole('button', { name: /Mikrofonun kapalı/ });
		expect(mic.getAttribute('aria-pressed')).toBe('false');
		await fireEvent.click(speaker);
		await fireEvent.click(mic);
		expect(onSpeaker).toHaveBeenCalledOnce();
		expect(onMic).toHaveBeenCalledOnce();
		// Not picked → not shown.
		expect(screen.queryByRole('button', { name: /Host hoparlörü/ })).toBeNull();
	});

	it('hides the mic where there is no mic channel (game mode)', () => {
		render(StatusBar, { ...base, buttons: ['mic', 'hostmute'], micAvailable: false, hostMuted: true });
		expect(screen.queryByRole('button', { name: /Mikrofon/ })).toBeNull();
		expect(screen.getByRole('button', { name: /Host hoparlörü sessiz/ }).getAttribute('aria-pressed')).toBe('false');
	});
});
