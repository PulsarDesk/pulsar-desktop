// The user's saved stream picks (resolution / fps / bitrate limit from the session menu),
// turned into what `start_remote_play` takes, so every connect starts on them.

import { ui } from '#lib/settings.svelte.js';

/** A resolution pick ('auto', a preset, or an explicit "WxH") → pixels; [0, 0] = automatic. */
export function resToWh(v: string): [number, number] {
	const wh = /^(\d+)x(\d+)$/.exec(v);
	if (wh) return [Number(wh[1]), Number(wh[2])];
	switch (v) {
		case '4K':
			return [3840, 2160];
		case '1440p':
			return [2560, 1440];
		case '1080p':
			return [1920, 1080];
		case '720p':
			return [1280, 720];
		default:
			return [0, 0];
	}
}

/** The saved picks as connect arguments; 0 = automatic for each. */
export function savedStreamPicks(): { width: number; height: number; fps: number; bitrateKbps: number } {
	const [width, height] = resToWh(ui.streamRes ?? 'auto');
	return {
		width,
		height,
		fps: ui.streamFps > 0 ? ui.streamFps : 0,
		bitrateKbps: ui.bitrate > 0 ? ui.bitrate * 1000 : 0
	};
}
