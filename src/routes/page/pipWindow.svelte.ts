// Picture-in-picture window behaviour (issue #18), for the `pip-<n>` window only. The PiP
// shows ONE session, frameless, and has three states:
//
//   passive — small, no input reaches the host; a drag moves the window.
//   hover   — the pointer rests on it ~400 ms: it grows (Settings: on, 125/150/200 %) towards
//             the screen's middle, keeping its corner (pip.rs `pip_resize`), and shows its
//             control bar under the video. Still no input.
//   active  — a click (not a drag): the window takes the focus and the session takes control,
//             the click itself never reaching the host. It stays grown until the focus leaves.
//
// A double click brings the session back to its tab.
//
// Where the events come from: hover is polled Rust-side for every OS (`pip-hover`; on Windows
// the native video child takes the mouse, so the webview under it never hears the pointer).
// Presses: on Windows the renderer reports them (`pip-press`: click or drag); on Linux and
// macOS the video passes the pointer through to the webview (capture-phase DOM handlers).

import { api, isTauri, onChatMsg, onPipHover, onPipPress, onPipRestore } from '#lib/api.js';
import { ui, saveUi } from '#lib/settings.svelte.js';
import { myLabel } from './tabWindows.svelte';

/** Height of the control bar shown while grown (CSS px). */
export const PIP_BAR_H = 34;
const HOVER_MS = 400;
const LEAVE_MS = 250;
/** A click within this of the previous one = double click (bring back). */
const DOUBLE_MS = 300;
/** Pointer travel that turns a press into a window drag. */
const DRAG_PX = 5;

type Deps = {
	/** Give the session back to its tab (TabWindows.pipReturn). */
	restore: () => void;
	/** The session takes control (Session.pipTakeControl). */
	takeControl: () => void;
	/** The play id this window shows (-1 = none yet). */
	playId: () => number;
	/** This window owns keyboard/mouse capture (its focus reached TabWindows). */
	ownsInput: () => boolean;
	/** The session's input is captured for the host right now. */
	engaged: () => boolean;
};

export class PipWindow {
	hover = $state(false);
	grown = $state(false);
	/** Clicked into: input goes to the host. Cleared when the focus leaves the window. */
	active = $state(false);
	/** Chat lines from the host since the menu was last opened here (the bar's badge). */
	unread = $state(0);

	#deps: Deps;
	/** The window's size while not grown (logical px). */
	#base = { w: 480, h: 270 };
	#hoverTimer: ReturnType<typeof setTimeout> | undefined;
	#leaveTimer: ReturnType<typeof setTimeout> | undefined;
	#clickTimer: ReturnType<typeof setTimeout> | undefined;
	#moveTimer: ReturnType<typeof setTimeout> | undefined;
	/** Our own resize is in flight: its resize events are not the user's. */
	#selfResize = 0;
	#lastUp = 0;
	#press: { x: number; y: number; dragged: boolean } | null = null;

	constructor(deps: Deps) {
		this.#deps = deps;
	}

	/** Wire up (PiP window only): restore requests, focus, size and position tracking. */
	async init() {
		if (!isTauri) return;
		await onPipRestore((label) => {
			if (label === myLabel) this.#deps.restore();
		});
		await onPipHover((label, on) => {
			if (label !== myLabel) return;
			if (on) this.enter();
			else this.leave();
		});
		await onChatMsg((e) => {
			if (Number(e.peer) === this.#deps.playId()) this.unread++;
		});
		await onPipPress((play, drag) => {
			if (play !== this.#deps.playId()) return;
			if (drag) void this.#drag();
			else this.#click();
		});
		const { getCurrentWindow } = await import('@tauri-apps/api/window');
		const w = getCurrentWindow();
		try {
			const [sz, sf] = [await w.innerSize(), await w.scaleFactor()];
			this.#base = { w: sz.width / sf, h: sz.height / sf };
		} catch {
			/* keep the default */
		}
		// Focus left the window: no longer active — unless the input is captured, which on Windows
		// moves the focus into the renderer child (still ours). The watch below then ends it once
		// neither holds.
		await w.onFocusChanged(({ payload: focused }) => {
			if (!focused && !this.#deps.engaged()) this.deactivate();
		});
		await w.onResized(async ({ payload }) => {
			if (this.#selfResize > 0 || this.grown) return;
			const sf = await w.scaleFactor().catch(() => 1);
			this.#base = { w: payload.width / sf, h: payload.height / sf };
			this.#remember();
		});
		// Dragged and let go: snap into a near corner, and open the next PiP there.
		await w.onMoved(() => {
			if (this.#selfResize === 0) this.#remember();
		});
	}

	/** Once the window settles (a drag or a resize ended): snap into a near corner, and save
	 * where it is for the next PiP. Not while grown (that is not its size). */
	#remember() {
		clearTimeout(this.#moveTimer);
		this.#moveTimer = setTimeout(() => {
			if (this.grown) return;
			api.pipSnap()
				.then((r) => {
					if (r && r.length === 4) {
						ui.pipLast = r;
						saveUi();
					}
				})
				.catch(() => {});
		}, 450);
	}

	#resize(w: number, h: number) {
		this.#selfResize++;
		api.pipResize(w, h)
			.catch(() => {})
			.finally(() => setTimeout(() => this.#selfResize--, 300));
	}

	#grow() {
		if (this.grown) return;
		this.grown = true;
		const k = ui.pipHoverGrow ? Math.max(1, ui.pipHoverScale / 100) : 1;
		this.#resize(Math.round(this.#base.w * k), Math.round(this.#base.h * k) + PIP_BAR_H);
	}

	#shrink() {
		clearTimeout(this.#hoverTimer);
		if (!this.grown) return;
		this.grown = false;
		this.#resize(this.#base.w, this.#base.h);
		// A drag usually ends with the pointer still on the window (grown): it snaps and is
		// remembered once it is back to its own size.
		this.#remember();
	}

	/** The pointer entered the window. */
	enter = () => {
		this.hover = true;
		clearTimeout(this.#leaveTimer);
		clearTimeout(this.#hoverTimer);
		this.#hoverTimer = setTimeout(() => {
			if (this.hover) this.#grow();
		}, HOVER_MS);
	};

	/** The pointer left the window: shrink back unless it is in use. */
	leave = () => {
		this.hover = false;
		clearTimeout(this.#hoverTimer);
		clearTimeout(this.#leaveTimer);
		this.#leaveTimer = setTimeout(() => {
			if (!this.hover && !this.active) this.#shrink();
		}, LEAVE_MS);
	};

	/** Capture-phase press on the video. While passive it never reaches the session: a drag
	 * moves the window, a click activates, a double click brings the session back. */
	down = (e: PointerEvent) => {
		if (this.active || e.button !== 0) return;
		e.preventDefault();
		e.stopPropagation();
		this.#press = { x: e.screenX, y: e.screenY, dragged: false };
	};

	move = (e: PointerEvent) => {
		const p = this.#press;
		if (!p || p.dragged || (e.buttons & 1) === 0) return;
		if (Math.abs(e.screenX - p.x) < DRAG_PX && Math.abs(e.screenY - p.y) < DRAG_PX) return;
		p.dragged = true;
		e.stopPropagation();
		void this.#drag();
	};

	async #drag() {
		if (!isTauri) return;
		const { getCurrentWindow } = await import('@tauri-apps/api/window');
		await getCurrentWindow().startDragging().catch(() => {});
	}

	up = (e: PointerEvent) => {
		const p = this.#press;
		this.#press = null;
		if (!p || this.active) return;
		e.preventDefault();
		e.stopPropagation();
		if (p.dragged) return;
		this.#click();
	};

	/** A click on the video (no drag). Active: take control again (e.g. after the free cursor
	 * left the video). Passive: activate — unless a second click follows (bring back). */
	#click() {
		if (this.active) {
			this.#deps.takeControl();
			return;
		}
		const now = Date.now();
		if (now - this.#lastUp < DOUBLE_MS) {
			clearTimeout(this.#clickTimer);
			this.#lastUp = 0;
			this.#deps.restore();
			return;
		}
		this.#lastUp = now;
		clearTimeout(this.#clickTimer);
		this.#clickTimer = setTimeout(() => this.activate(), DOUBLE_MS);
	}

	#activeWatch: ReturnType<typeof setInterval> | undefined;

	/** Back to passive (the focus went elsewhere and nothing is captured). */
	deactivate() {
		clearInterval(this.#activeWatch);
		this.active = false;
		if (!this.hover) this.#shrink();
	}

	/** Take control: the window gets the focus, the session the input. */
	activate = () => {
		this.active = true;
		this.#grow();
		clearInterval(this.#activeWatch);
		if (typeof document !== 'undefined')
			this.#activeWatch = setInterval(() => {
				if (!document.hasFocus() && !this.#deps.engaged()) this.deactivate();
			}, 500);
		void (async () => {
			if (isTauri) {
				const { getCurrentWindow } = await import('@tauri-apps/api/window');
				await getCurrentWindow().setFocus().catch(() => {});
				// The session takes control only once this window owns the input (the focus
				// edge travels through Rust: `input-window`).
				for (let i = 0; i < 25 && !this.#deps.ownsInput(); i++) await new Promise((r) => setTimeout(r, 20));
			}
			this.#deps.takeControl();
		})();
	};
}
