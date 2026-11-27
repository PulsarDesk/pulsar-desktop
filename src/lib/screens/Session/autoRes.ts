// "Resolution follows the window" (issue #18 follow-up, Settings → Display, default on): a
// session shown smaller than the host's screen asks the host for a smaller stream — the
// encoder scales, the host's display mode never changes. Snapped to a few heights so a window
// being dragged bigger/smaller does not restart the host's encoder for every pixel.

/** Stream heights the request snaps to. */
const STEPS = [360, 480, 540, 720, 900, 1080, 1440, 2160];

/** Close enough to the host's size: stream it as it is (0×0 = the host's own size). */
const FULL = 0.9;

/**
 * The stream size to ask for, given the video area in device px and the host screen's size.
 * `[0, 0]` = the host's own size (the view is about as big as the screen, or nothing known).
 */
export function autoResFor(viewW: number, viewH: number, hostW: number, hostH: number): [number, number] {
	if (viewW <= 0 || viewH <= 0 || hostW <= 0 || hostH <= 0) return [0, 0];
	// The picture is fitted into the view: the axis that limits it sets the scale.
	const scale = Math.min(viewW / hostW, viewH / hostH);
	if (scale >= FULL) return [0, 0];
	const want = hostH * scale;
	const h = STEPS.find((s) => s >= want);
	if (!h || h >= hostH * FULL) return [0, 0];
	// Even width at the host's aspect (encoders want even sizes).
	const w = Math.round((h * hostW) / hostH / 2) * 2;
	return [w, h];
}
