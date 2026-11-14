import { describe, it, expect, vi } from 'vitest';

vi.mock('#lib/api.js', () => ({
	onPlayDecoder: vi.fn(),
	onPlayRtt: vi.fn(),
	onPlayStats: vi.fn(),
	onPlayVStats: vi.fn()
}));

import { statsVideoError } from './media.svelte.js';

describe('statsVideoError', () => {
	const tr = (k: string) => `T(${k})`;

	it('passes a plain stats line through as no error', () => {
		expect(statsVideoError('h264 · nvenc · 1080p · 60fps', tr)).toBeNull();
	});

	it('returns a host stream error verbatim', () => {
		expect(statsVideoError('!every encoder failed', tr)).toBe('every encoder failed');
	});

	it('translates an app-raised error key', () => {
		expect(statsVideoError('!@session.rendererFailed', tr)).toBe('T(session.rendererFailed)');
	});
});
