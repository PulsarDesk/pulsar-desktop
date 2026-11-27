// "Keep on top" (window pin, issue #18) for THIS window — the main window or a torn-off tab
// window. Window-level: every tab of a pinned window stays on top with it. The Rust side
// (`src-tauri/src/winpin.rs`) owns the truth; the title bar's button, the Pulsar menu's row and
// Settings all go through it and it echoes every change as `window-pin` (label, on).
//
// Instantiate at component init: the subscription $effect is scoped to the component.

import { api, isTauri, onWindowPin } from '#lib/api.js';
import { ui } from '#lib/settings.svelte.js';
import { myLabel } from './tabWindows.svelte';

export class WindowPin {
	pinned = $state(false);
	/** Linux Wayland: the desktop may ignore keep-on-top (the tooltip warns). */
	maybeIgnored = $state(false);

	constructor() {
		$effect(() => {
			if (!isTauri) return;
			let dead = false;
			let off: (() => void) | undefined;
			// A torn-off tab window starts pinned when its source window was.
			api.windowPinGet()
				.then((v) => {
					if (!dead) this.pinned = v;
				})
				.catch(() => {});
			api.windowPinMaybeIgnored()
				.then((v) => {
					if (!dead) this.maybeIgnored = v;
				})
				.catch(() => {});
			onWindowPin((label, on) => {
				if (label === myLabel) this.pinned = on;
			}).then((u) => {
				if (dead) u();
				else off = u;
			});
			return () => {
				dead = true;
				off?.();
			};
		});
	}

	/** Settings → General → "Keep on top" (default on). */
	get available(): boolean {
		return ui.windowPin;
	}

	async toggle() {
		if (!this.available) return;
		const want = !this.pinned;
		this.pinned = want; // optimistic; the echo / the result settles it
		try {
			this.pinned = await api.windowPinSet(want);
		} catch {
			this.pinned = !want;
		}
	}
}
