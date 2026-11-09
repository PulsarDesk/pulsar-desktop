import { describe, it, expect, beforeEach } from 'vitest';
import { resToWh, savedStreamPicks } from './streamPicks.js';
import { ui } from '#lib/settings.svelte.js';

describe('saved stream picks', () => {
	beforeEach(() => {
		ui.streamRes = 'auto';
		ui.streamFps = 0;
		ui.bitrate = 0;
	});

	it('maps presets and explicit modes to pixels, auto to 0×0', () => {
		expect(resToWh('1080p')).toEqual([1920, 1080]);
		expect(resToWh('4K')).toEqual([3840, 2160]);
		expect(resToWh('2880x1800')).toEqual([2880, 1800]);
		expect(resToWh('auto')).toEqual([0, 0]);
	});

	it('is all-automatic by default', () => {
		expect(savedStreamPicks()).toEqual({ width: 0, height: 0, fps: 0, bitrateKbps: 0 });
	});

	it('carries a 1080p · 120 fps · 15 Mbit pick as connect arguments', () => {
		ui.streamRes = '1080p';
		ui.streamFps = 120;
		ui.bitrate = 15;
		expect(savedStreamPicks()).toEqual({ width: 1920, height: 1080, fps: 120, bitrateKbps: 15000 });
	});
});
