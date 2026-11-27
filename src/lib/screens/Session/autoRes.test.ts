import { describe, it, expect } from 'vitest';
import { autoResFor } from './autoRes';

describe('resolution follows the window', () => {
	it('a window about as big as the host screen streams the host size', () => {
		expect(autoResFor(2560, 1440, 2560, 1440)).toEqual([0, 0]);
		expect(autoResFor(2400, 1350, 2560, 1440)).toEqual([0, 0]);
	});
	it('a smaller window asks for the next step up, at the host aspect', () => {
		// Half-size window on a 1440p host → 720p.
		expect(autoResFor(1280, 720, 2560, 1440)).toEqual([1280, 720]);
		// A PiP (480×270 at 1.5 dpr = 720×405) → 480p.
		expect(autoResFor(720, 405, 1920, 1080)).toEqual([854, 480]);
	});
	it('the limiting axis decides (a tall narrow window)', () => {
		expect(autoResFor(960, 1400, 1920, 1080)).toEqual([960, 540]);
	});
	it('nothing known: the host size', () => {
		expect(autoResFor(0, 0, 1920, 1080)).toEqual([0, 0]);
		expect(autoResFor(800, 600, 0, 0)).toEqual([0, 0]);
	});
	it('a step at or above the host size streams the host size', () => {
		expect(autoResFor(1100, 620, 1280, 720)).toEqual([0, 0]);
	});
});
