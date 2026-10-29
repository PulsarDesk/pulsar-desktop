import { describe, it, expect } from 'vitest';
import { isLive, setLivePeers } from './presence.svelte.js';

describe('presence', () => {
	it('marks a host with a live session as live, by normalized id', () => {
		setLivePeers(['264 310 513']);
		expect(isLive('264310513')).toBe(true);
		expect(isLive('264 310 513')).toBe(true);
		expect(isLive('111 222 333')).toBe(false);
	});

	it('clears when the session ends', () => {
		setLivePeers(['264310513']);
		setLivePeers([]);
		expect(isLive('264310513')).toBe(false);
	});
});
