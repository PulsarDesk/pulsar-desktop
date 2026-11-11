// Controller + keyboard navigation for the gaming-mode shell. A small, framework-light
// roving-focus controller: gaming-shell widgets register themselves with the `item`
// Svelte action, and this drives a single focus that moves SPATIALLY (geometry-based, so
// no manual row/col wiring) with the D-pad / left stick, activates with A, goes back with
// B, and jumps between sections with the bumpers. A keyboard fallback (arrows / Enter /
// Esc) mirrors it so the shell is fully usable without a pad — and on platforms where the
// webview Gamepad API is unavailable (e.g. WebKitGTK on RK3588).
//
// Menu navigation uses the browser Gamepad API (not the gilrs reader, which is for
// in-session forwarding): it only runs while the app shell — not a live session — is
// focused, so it never competes with controller input being sent to a host.
//
// Modal scoping: when a `[data-navmodal]` element is in the DOM (the connect pop-up),
// focus is confined to items inside it, and B/Esc run the modal's back handler (pushed
// via pushBack) instead of the shell's — so the pop-up is fully pad-navigable and the
// background can't be reached behind it.

import type { NavInput } from '#lib/api.js';

/** Standard-mapping button indices (Gamepad API "standard" layout). X = West (□ / X). */
const BTN = { A: 0, B: 1, X: 2, LB: 4, RB: 5, UP: 12, DOWN: 13, LEFT: 14, RIGHT: 15 } as const;
type Dir = 'up' | 'down' | 'left' | 'right';
const AXIS_DEADZONE = 0.55;
/** Delay (ms) before a held direction auto-repeats, and the repeat interval after. */
const REPEAT_DELAY = 360;
const REPEAT_RATE = 130;
/** Pad poll interval (ms). We poll with setTimeout (not requestAnimationFrame) — ~60 Hz is
 * plenty for menu nav and a plain timer keeps us off the compositor's frame clock. */
const POLL_MS = 16;
/** Linux client = WebKitGTK. This build ships libmanette, so touching the webview Gamepad
 * API (`navigator.getGamepads()` / a `gamepadconnected` listener) starts libmanette's
 * gamepad monitor — which, on RK3588 with a DualSense, busy-loops the GTK main thread at
 * ~100% the moment a pad connects (froze the whole gaming home; verified the process loads
 * libmanette-0.2 and the spin is in the GDK main loop with the X queue empty). We never need
 * it on Linux — the gilrs→SDL bridge (api.onGamepadNav) is the pad-nav source there — so the
 * browser-Gamepad-API fallback is hard-disabled on Linux. */
const IS_LINUX = typeof navigator !== 'undefined' && /Linux|X11/i.test(navigator.userAgent);

type Opts = {
	/** Called on B / Escape when no modal back-handler is active (e.g. leave a sub-view). */
	onBack?: () => void;
	/** Called on LB(-1) / RB(+1) — section/tab jump (e.g. cycle the bottom dock). */
	onBumper?: (dir: -1 | 1) => void;
};

export class GamepadNav {
	#items = new Set<HTMLElement>();
	#current: HTMLElement | null = null;
	#opts: Opts;
	/** Stack of back handlers (e.g. an open modal pushes its close fn). The top wins. */
	#backStack: (() => void)[] = [];
	#timer: ReturnType<typeof setTimeout> | 0 = 0;
	#running = false;
	#prevInp: NavInput | null = null;
	#dirHeld: Dir | null = null;
	#nextRepeat = 0;
	/** Latest input from the gilrs→webview bridge (api.onGamepadNav) + when it arrived.
	 * This is the ONLY pad-nav source on Linux (no webview Gamepad API) and the preferred
	 * one everywhere; the browser Gamepad API is a fallback used only when no bridge event
	 * has arrived recently. */
	#bridge: NavInput | null = null;
	#bridgeAt = 0;
	/** Resting axis values for the browser-Gamepad-API fallback, snapshotted when a pad
	 * first appears, so a D-pad reported as an axis is read as DEVIATION from rest (a
	 * trigger that rests off-center can't masquerade as a held direction). */
	#axisRest: number[] | null = null;
	/** Whether the user has STARTED keyboard/controller navigation. The focus ring
	 * (`data-navfocus`) stays HIDDEN until the FIRST directional/activate input, so a mouse/touch
	 * user never sees a stray selection border. The first input "wakes" nav at the default action
	 * (`data-navdefault`, e.g. "Connect to a host") — not whatever registered first. Reset on stop(). */
	#navActive = false;
	/** A <select> opened with A/Enter. While open, a list of its options is shown under it
	 * (#pop) and the d-pad/arrows move the highlight (#pending) instead of focus; A/Enter
	 * applies the highlighted option, B/Escape closes without changing. Closed selects are
	 * never changed by navigation — passing over a setting must not edit it. (The native
	 * popup can't be driven by a pad, so this list is drawn here.) */
	#editing: HTMLSelectElement | null = null;
	#pending = -1;
	#pop: HTMLDivElement | null = null;
	#now = () => (typeof performance !== 'undefined' ? performance.now() : 0);

	constructor(opts: Opts = {}) {
		this.#opts = opts;
		this.onKeydown = this.onKeydown.bind(this);
	}

	/** Update the back/bumper handlers after construction — used when the nav is a shared
	 * singleton (so the gaming shell can wire its own view-specific handlers into it). */
	setOpts(opts: Opts) {
		this.#opts = opts;
	}

	/** Svelte action: `use:nav.item`. Registers the node as focusable; unregisters on
	 * destroy. The first registered visible item becomes the initial focus (unless a
	 * modal is open — then focus is driven by enterModal). */
	item = (node: HTMLElement) => {
		this.#items.add(node);
		node.setAttribute('data-navitem', '');
		if (!node.hasAttribute('tabindex') && !/^(a|button|input|select|textarea)$/i.test(node.tagName))
			node.tabIndex = -1;
		// NO auto-focus on registration: the ring must not appear (and nothing should grab focus)
		// until the user actually navigates with a key/pad. #wake() picks the start element on the
		// first input. (Previously this focused the first-registered item — the stray border bug.)
		return {
			destroy: () => {
				this.#items.delete(node);
				node.removeAttribute('data-navitem');
				node.removeAttribute('data-navfocus');
				if (this.#current === node) this.#current = null;
			}
		};
	};

	start() {
		if (this.#running || typeof window === 'undefined') return;
		this.#running = true;
		window.addEventListener('keydown', this.onKeydown);
		this.#loop();
	}

	stop() {
		this.#running = false;
		if (typeof window !== 'undefined') window.removeEventListener('keydown', this.onKeydown);
		if (this.#timer) clearTimeout(this.#timer);
		this.#timer = 0;
		if (this.#current) this.#current.removeAttribute('data-navfocus');
		this.#stopEditing();
		// Re-entering gaming mode should again start ring-less until the user navigates.
		this.#navActive = false;
	}

	/** Feed an input snapshot from the gilrs→webview bridge (api.onGamepadNav). */
	ingestBridge(s: NavInput) {
		this.#bridge = s;
		this.#bridgeAt = this.#now();
	}

	/** Push a back handler (a modal's close fn). B / Esc will call this until popped. */
	pushBack(fn: () => void) {
		this.#backStack.push(fn);
	}
	/** Remove a previously-pushed back handler (call on modal destroy). */
	popBack(fn?: () => void) {
		if (fn) this.#backStack = this.#backStack.filter((f) => f !== fn);
		else this.#backStack.pop();
	}

	/** Force focus to a specific registered element (e.g. the modal's ID box on open). */
	focus(node: HTMLElement | null) {
		if (node && this.#items.has(node) && this.#focusable(node)) this.#setCurrent(node);
	}

	/** Focus the first focusable item in the current scope (used when a modal opens, and
	 * when it closes to return focus to the shell). */
	focusFirst() {
		const c = this.#candidates();
		if (c.length) this.#setCurrent(c[0]);
		else if (this.#current) {
			this.#current.removeAttribute('data-navfocus');
			this.#current = null;
		}
	}

	// ---- focus management -------------------------------------------------

	#modalEl(): HTMLElement | null {
		if (typeof document === 'undefined') return null;
		// The TOPMOST dialog (the last one opened is the last in the DOM — modals portal to
		// <body>): e.g. the ID keypad opened over the gaming "add device" form.
		const all = document.querySelectorAll<HTMLElement>('[data-navmodal]');
		return all.length ? all[all.length - 1] : null;
	}

	#focusable(el: HTMLElement): boolean {
		if (!el.isConnected) return false;
		const r = el.getBoundingClientRect();
		// NOTE: deliberately NOT reading `el.offsetParent` here — it's a second forced
		// synchronous layout per element, and under `content-visibility` it forces WebKitGTK
		// to resolve geometry it was skipping (brutal on opi5's software-painted webview when
		// move() runs this over every registered control). rect width/height + isConnected
		// already gate real visibility.
		if (r.width <= 0 || r.height <= 0) return false;
		if (el.closest('[inert]')) return false;
		return true;
	}

	/** Focusable items in the active scope: confined to the open modal if one exists. */
	#candidates(): HTMLElement[] {
		const modal = this.#modalEl();
		const all = [...this.#items].filter((el) => this.#focusable(el));
		return modal ? all.filter((el) => modal.contains(el)) : all;
	}

	#setCurrent(el: HTMLElement) {
		if (this.#current && this.#current !== el) this.#current.removeAttribute('data-navfocus');
		this.#current = el;
		// The VISIBLE selection ring only shows once the user is navigating (mouse/touch users
		// never see it). el.focus() still runs so a woken pad/keyboard user can activate it, and
		// so a modal's first field is focused for typing — but no ring until #navActive.
		if (this.#navActive) el.setAttribute('data-navfocus', '');
		try {
			el.focus({ preventScroll: true });
		} catch {
			el.focus();
		}
		// A disabled control can't take DOM focus, so the previous one kept it — and its own
		// focus style (Delete above a disabled Connect) looked like a second selection. Drop it.
		if (typeof document !== 'undefined' && document.activeElement !== el) {
			(document.activeElement as HTMLElement | null)?.blur?.();
		}
		// Scroll the item's whole ROW into view, not just the control: a settings toggle sits
		// at the right of a taller row, so scrolling to the toggle alone left the row's label
		// half off-screen. The margin keeps a little gap from the scroller's edge.
		const row = el.closest<HTMLElement>('[data-navrow], .srow') ?? el;
		row.style.scrollMargin = '12px';
		row.scrollIntoView({ block: 'nearest', inline: 'nearest' });
		// The first / last item of a scrolling list scrolls it all the way: non-focusable
		// content after the last control (notes, a version line) or before the first one
		// otherwise stays hidden — the list looked like it "never reached the bottom".
		const box = scrollBox(row);
		if (box) {
			const r = row.getBoundingClientRect();
			let above = false;
			let below = false;
			for (const it of this.#items) {
				if (it === el || !it.isConnected || !box.contains(it) || row.contains(it)) continue;
				const ir = it.getBoundingClientRect();
				if (ir.width <= 0 || ir.height <= 0) continue;
				if (ir.top >= r.bottom - 1) below = true;
				if (ir.bottom <= r.top + 1) above = true;
			}
			if (!below) box.scrollTop = box.scrollHeight;
			else if (!above) box.scrollTop = 0;
		}
	}

	/** The element marked as the default focus (the primary action — e.g. "Connect to a host",
	 * tagged `data-navdefault`). Null → #wake falls back to the first candidate. */
	#defaultItem(): HTMLElement | null {
		for (const el of this.#items)
			if (el.hasAttribute('data-navdefault') && this.#focusable(el)) return el;
		return null;
	}

	/** The first keyboard/controller input "wakes" navigation: reveal the focus ring, STARTING at
	 * the default primary action (or, in a modal, its first item; else the first candidate).
	 * Returns true when it just woke — so a directional input reveals-without-moving the first time. */
	#wake(): boolean {
		if (this.#navActive) return false;
		this.#navActive = true;
		const start = this.#modalEl()
			? this.#candidates()[0]
			: (this.#defaultItem() ?? this.#current ?? this.#candidates()[0]);
		if (start) this.#setCurrent(start);
		return true;
	}

	/** Move focus to the nearest item in `dir` within the active scope (geometric scoring).
	 * Reads each candidate's rect EXACTLY ONCE (center cached) — the old path called
	 * getBoundingClientRect twice per item (#candidates→#focusable, then #center) plus an
	 * offsetParent read, i.e. ~3 forced layouts × every registered control per nav step. On
	 * opi5/WebKitGTK (software paint, no batching) that pegged the main thread; one rect each
	 * roughly thirds the layout work. */
	move(dir: Dir) {
		if (this.#wake()) return; // first nav input only REVEALS the ring at the default action
		// An OPENED <select> (A/Enter on it) takes the d-pad: step its value (prev/next
		// non-disabled option, native `change` so Svelte's onchange runs). A closed one is
		// just passed over like any other control.
		if (this.#editing) {
			if (this.#editing.isConnected && this.#editing === this.#current) {
				this.#stepPending(dir === 'down' || dir === 'right' ? 1 : -1);
				return;
			}
			this.#stopEditing();
		}
		const modal = this.#modalEl();
		const cands: Cand[] = [];
		for (const el of this.#items) {
			if (!el.isConnected) continue;
			if (modal && !modal.contains(el)) continue;
			if (el.closest('[inert]')) continue;
			const r = el.getBoundingClientRect();
			if (r.width <= 0 || r.height <= 0) continue;
			cands.push({
				el,
				cx: r.left + r.width / 2,
				cy: r.top + r.height / 2,
				top: r.top,
				bottom: r.bottom,
				left: r.left,
				right: r.right
			});
		}
		if (cands.length === 0) return;
		// No current, or current left the scope (e.g. a modal just opened) → snap to first.
		const from = this.#current ? cands.find((c) => c.el === this.#current) : undefined;
		if (!from) {
			this.#setCurrent(cands[0].el);
			return;
		}
		// Up/down inside a scrolling box (the Settings card) stays in that box until it runs
		// out: its next row may be scrolled off-screen BELOW something outside the box (the
		// gaming dock's controller pill), which is geometrically closer — so the dock used to
		// steal focus before the list's last rows were reached.
		if (dir === 'up' || dir === 'down') {
			const box = scrollBox(from.el);
			if (box) {
				const inBox = this.#pickVertical(cands.filter((c) => box.contains(c.el)), from, dir);
				if (inBox) {
					this.#setCurrent(inBox);
					return;
				}
			}
		}
		if (dir === 'left' || dir === 'right') {
			const side = this.#pickSideways(cands, from, dir);
			if (side) this.#setCurrent(side);
			return;
		}
		const best = this.#pickVertical(cands, from, dir);
		if (best) this.#setCurrent(best);
	}

	/** Up/down goes to the NEXT ROW, then the item in it closest horizontally. The old
	 * distance + 2×sideways score skipped a row whose items sat off to the side (gaming home:
	 * "Connect to a host" → down jumped over "Hosts on your network" to the recents' × buttons,
	 * which are nearer the centre). A row = the candidates whose near edge is within
	 * ROW_SLACK px of the nearest one in that direction. */
	#pickVertical(cands: Cand[], from: Cand, dir: Dir): HTMLElement | null {
		const ROW_SLACK = 24;
		const gap = (c: Cand) => (dir === 'down' ? c.top - from.bottom : from.top - c.bottom);
		const ahead = cands.filter(
			(c) => c.el !== from.el && (dir === 'down' ? c.cy - from.cy : from.cy - c.cy) > 1 && gap(c) > -4
		);
		if (!ahead.length) return null;
		const near = Math.min(...ahead.map((c) => Math.max(0, gap(c))));
		const row = ahead.filter((c) => Math.max(0, gap(c)) <= near + ROW_SLACK);
		const fx0 = from.left;
		const fx1 = from.right;
		let best: Cand | null = null;
		let bestKey = Infinity;
		for (const c of row) {
			// Horizontal gap between the two boxes (0 when they overlap), then centre offset.
			const hgap = Math.max(0, c.left - fx1, fx0 - c.right);
			const key = hgap * 4 + Math.abs(c.cx - from.cx) * 0.25 + Math.max(0, gap(c));
			if (key < bestKey) {
				bestKey = key;
				best = c;
			}
		}
		return best?.el ?? null;
	}

	/** Left/right never drifts up or down to a far-away row (it used to: from the dock's
	 * controller pill "left" went up into the page; from a settings toggle it went to the
	 * row above instead of the tab rail). Order:
	 *  1. an item on the SAME ROW (vertical overlap) that way;
	 *  2. out of the current scrolling box sideways (Settings card → its tab rail), landing on
	 *     the box's selected entry (`.on` / aria-selected / aria-current) when there is one;
	 *  3. anything within 45° of the direction;
	 * else stay put. */
	#pickSideways(cands: Cand[], from: Cand, dir: Dir): HTMLElement | null {
		const ahead = cands.filter((c) => {
			if (c.el === from.el) return false;
			return (dir === 'right' ? c.cx - from.cx : from.cx - c.cx) > 1;
		});
		const sameRow = ahead.filter((c) => c.bottom > from.top + 2 && c.top < from.bottom - 2);
		if (sameRow.length) return this.#pick(sameRow, from, dir);
		const box = scrollBox(from.el);
		if (box) {
			// Only what sits BESIDE the box (its vertical span), not a dock under it.
			const br = box.getBoundingClientRect();
			const out = ahead.filter((c) => !box.contains(c.el) && c.cy >= br.top && c.cy <= br.bottom);
			const sel = out.filter((c) =>
				c.el.matches('.on, [aria-selected="true"], [aria-current]:not([aria-current="false"])')
			);
			if (sel.length) return this.#pick(sel, from, dir);
			if (out.length) return this.#pick(out, from, dir);
		}
		const cone = ahead.filter((c) => Math.abs(c.cy - from.cy) <= Math.abs(c.cx - from.cx));
		return cone.length ? this.#pick(cone, from, dir) : null;
	}

	/** Nearest candidate in `dir` from `from` (geometric score: distance along the direction
	 * plus twice the sideways offset), or null when nothing lies that way. */
	#pick(
		cands: Cand[],
		from: Cand,
		dir: Dir
	): HTMLElement | null {
		let best: HTMLElement | null = null;
		let bestScore = Infinity;
		for (const c of cands) {
			if (c.el === from.el) continue;
			const dx = c.cx - from.cx;
			const dy = c.cy - from.cy;
			let primary: number, cross: number;
			if (dir === 'right') { primary = dx; cross = Math.abs(dy); }
			else if (dir === 'left') { primary = -dx; cross = Math.abs(dy); }
			else if (dir === 'down') { primary = dy; cross = Math.abs(dx); }
			else { primary = -dy; cross = Math.abs(dx); }
			if (primary <= 1) continue;
			const score = primary + cross * 2;
			if (score < bestScore) { bestScore = score; best = c.el; }
		}
		return best;
	}

	activate() {
		// First input wakes nav onto the default action; then we click whatever is current — so a
		// cold A/Enter activates "Connect to a host" directly (no stray ring beforehand).
		this.#wake();
		const el = this.#current;
		if (!el || !this.#focusable(el)) return;
		if (el.tagName === 'SELECT') {
			// A/Enter opens / closes the select for d-pad editing (never the native popup,
			// which a pad can't drive).
			if (this.#editing === el) {
				this.#applyPending();
				this.#stopEditing();
			} else this.#startEditing(el as HTMLSelectElement);
			return;
		}
		el.click();
	}

	#startEditing(sel: HTMLSelectElement) {
		this.#stopEditing();
		this.#editing = sel;
		this.#pending = sel.selectedIndex;
		sel.setAttribute('data-navedit', '');
		if (typeof document === 'undefined') return;
		const pop = document.createElement('div');
		pop.className = 'navpop';
		pop.setAttribute('role', 'listbox');
		[...sel.options].forEach((o, i) => {
			const row = document.createElement('div');
			row.className = 'navpop-opt';
			row.textContent = o.textContent;
			if (o.disabled) row.classList.add('off');
			// Mouse still works on the list: a click picks that option.
			row.addEventListener('mousedown', (e) => {
				e.preventDefault();
				if (o.disabled) return;
				this.#pending = i;
				this.#applyPending();
				this.#stopEditing();
			});
			pop.appendChild(row);
		});
		document.body.appendChild(pop);
		this.#pop = pop;
		// Fixed under the select (above it when the space below is too small), at least as
		// wide as the select.
		const r = sel.getBoundingClientRect();
		pop.style.minWidth = `${Math.round(r.width)}px`;
		pop.style.left = `${Math.round(Math.min(r.left, window.innerWidth - pop.offsetWidth - 8))}px`;
		const below = window.innerHeight - r.bottom - 8;
		if (pop.offsetHeight > below && r.top > below) {
			pop.style.bottom = `${Math.round(window.innerHeight - r.top + 6)}px`;
			pop.style.maxHeight = `${Math.round(r.top - 14)}px`;
		} else {
			pop.style.top = `${Math.round(r.bottom + 6)}px`;
			pop.style.maxHeight = `${Math.round(below - 6)}px`;
		}
		this.#paintPending();
	}

	/** Move the highlight to the prev/next NON-disabled option (clamped at the ends). */
	#stepPending(step: 1 | -1) {
		const sel = this.#editing;
		if (!sel) return;
		let i = this.#pending + step;
		while (i >= 0 && i < sel.options.length && sel.options[i].disabled) i += step;
		if (i < 0 || i >= sel.options.length) return;
		this.#pending = i;
		this.#paintPending();
	}

	#paintPending() {
		const pop = this.#pop;
		if (!pop) return;
		[...pop.children].forEach((c, i) => {
			c.classList.toggle('on', i === this.#pending);
			c.classList.toggle('cur', i === this.#editing?.selectedIndex);
		});
		(pop.children[this.#pending] as HTMLElement | undefined)?.scrollIntoView({ block: 'nearest' });
	}

	/** Commit the highlighted option (native `change` so Svelte's onchange runs). */
	#applyPending() {
		const sel = this.#editing;
		if (!sel || this.#pending < 0 || this.#pending === sel.selectedIndex) return;
		if (sel.options[this.#pending]?.disabled) return;
		sel.selectedIndex = this.#pending;
		sel.dispatchEvent(new Event('change', { bubbles: true }));
	}

	#stopEditing() {
		this.#editing?.removeAttribute('data-navedit');
		this.#editing = null;
		this.#pending = -1;
		this.#pop?.remove();
		this.#pop = null;
	}

	/** Click the first element marked with `sel` in the active scope (e.g. the numpad
	 * delete key via West/Square). Used for context shortcuts that aren't the focused item. */
	#clickMarked(sel: string) {
		const root: ParentNode = this.#modalEl() ?? (typeof document !== 'undefined' ? document : null)!;
		if (!root) return;
		const el = root.querySelector<HTMLElement>(sel);
		if (el && this.#focusable(el)) el.click();
	}

	#back() {
		if (this.#editing) {
			this.#stopEditing();
			return;
		}
		const fn = this.#backStack.at(-1) ?? this.#opts.onBack;
		fn?.();
	}

	// ---- keyboard fallback ------------------------------------------------

	onKeydown(e: KeyboardEvent) {
		const tgt = e.target as HTMLElement | null;
		// A focused <select> is NOT a typing field here: its native arrow-key handling changed
		// the value while the user only meant to pass over it. Arrows go through move(), which
		// edits a select only after A/Enter opened it.
		const typing = !!tgt && (tgt.isContentEditable || /^(input|textarea)$/i.test(tgt.tagName));
		if (e.key === 'Escape') {
			e.preventDefault();
			this.#back();
			return;
		}
		// A single-line <input> has no use for Up/Down (the caret only moves left/right), so
		// those still navigate — from the connect modal's ID field down to its keypad and back.
		if (typing && tgt?.tagName === 'INPUT' && (e.key === 'ArrowUp' || e.key === 'ArrowDown')) {
			e.preventDefault();
			this.move(e.key === 'ArrowUp' ? 'up' : 'down');
			return;
		}
		if (typing) return; // let fields own arrows/Enter (caret/submit)
		switch (e.key) {
			case 'ArrowUp': e.preventDefault(); this.move('up'); break;
			case 'ArrowDown': e.preventDefault(); this.move('down'); break;
			case 'ArrowLeft': e.preventDefault(); this.move('left'); break;
			case 'ArrowRight': e.preventDefault(); this.move('right'); break;
			case 'Enter':
			case ' ': e.preventDefault(); this.activate(); break;
		}
	}

	// ---- gamepad polling --------------------------------------------------

	#loop() {
		if (!this.#running) return;
		this.#tick();
		// setTimeout, NOT requestAnimationFrame — see POLL_MS: rAF drives the GDK frame
		// clock on WebKitGTK and pins the main thread when a pad's touchpad device is present.
		this.#timer = setTimeout(() => this.#loop(), POLL_MS);
	}

	#pads(): (Gamepad | null)[] {
		// NEVER call getGamepads() on Linux: it starts WebKitGTK's libmanette monitor, which
		// freezes the GTK main thread on a pad connect (see IS_LINUX). The bridge is the only
		// pad-nav source there anyway.
		if (IS_LINUX || typeof navigator === 'undefined' || !navigator.getGamepads) return [];
		try {
			return navigator.getGamepads();
		} catch {
			return [];
		}
	}

	/** Read the browser Gamepad API into the unified NavInput shape (fallback path —
	 * Windows/macOS where the webview exposes it; absent on Linux WebKitGTK). */
	#readBrowserPad(): NavInput | null {
		const pad = this.#pads().find((p): p is Gamepad => !!p && p.connected);
		if (!pad) {
			this.#axisRest = null; // re-calibrate on reconnect
			return null;
		}
		const pressed = (i: number) => !!pad.buttons[i]?.pressed;
		if (!this.#axisRest) this.#axisRest = [...pad.axes];
		const ax = pad.axes[0] ?? 0;
		const ay = pad.axes[1] ?? 0;
		// D-pad-as-axes (axes 6/7) read as deviation from rest, ignored if the axis rests
		// off-center (a trigger, not a D-pad — that once froze nav).
		const dpadAxis = (i: number) => {
			const rest = this.#axisRest?.[i] ?? 0;
			if (Math.abs(rest) >= 0.4) return 0;
			return (pad.axes[i] ?? 0) - rest;
		};
		const dpx = dpadAxis(6);
		const dpy = dpadAxis(7);
		return {
			up: pressed(BTN.UP) || ay < -AXIS_DEADZONE || dpy < -AXIS_DEADZONE,
			down: pressed(BTN.DOWN) || ay > AXIS_DEADZONE || dpy > AXIS_DEADZONE,
			left: pressed(BTN.LEFT) || ax < -AXIS_DEADZONE || dpx < -AXIS_DEADZONE,
			right: pressed(BTN.RIGHT) || ax > AXIS_DEADZONE || dpx > AXIS_DEADZONE,
			a: pressed(BTN.A),
			b: pressed(BTN.B),
			x: pressed(BTN.X),
			lb: pressed(BTN.LB),
			rb: pressed(BTN.RB)
		};
	}

	#tick() {
		const now = this.#now();
		// Prefer the gilrs bridge (works on every platform; the only path on Linux). Fall
		// back to the browser Gamepad API only if no bridge event has arrived recently.
		const inp =
			this.#bridge && now - this.#bridgeAt < 600 ? this.#bridge : this.#readBrowserPad();
		if (!inp) {
			this.#dirHeld = null;
			this.#prevInp = null;
			return;
		}
		const dir: Dir | null = inp.up ? 'up' : inp.down ? 'down' : inp.left ? 'left' : inp.right ? 'right' : null;
		if (dir) {
			if (dir !== this.#dirHeld) {
				this.move(dir);
				this.#dirHeld = dir;
				this.#nextRepeat = now + REPEAT_DELAY;
			} else if (now >= this.#nextRepeat) {
				this.move(dir);
				this.#nextRepeat = now + REPEAT_RATE;
			}
		} else {
			this.#dirHeld = null;
		}

		const p = this.#prevInp;
		const edge = (k: keyof NavInput) => inp[k] && !p?.[k];
		if (edge('a')) this.activate();
		if (edge('b')) this.#back();
		// West / Square = delete: click the scope's delete key (numpad backspace) if any.
		if (edge('x')) this.#clickMarked('[data-navdelete]');
		if (edge('lb')) this.#opts.onBumper?.(-1);
		if (edge('rb')) this.#opts.onBumper?.(1);
		this.#prevInp = inp;
	}
}

/** The single gaming-mode nav instance, shared across the gaming shell AND the top-bar
 * chrome (so the controller can also reach the gaming/language/theme buttons). The shell
 * wires its view-specific handlers via `setOpts` and drives start/stop + bridge input. */
export const gamingNav = new GamepadNav();

/** Svelte action: auto-register EVERY focusable control inside `node` with the gaming nav,
 * kept in sync as the DOM changes (MutationObserver). Lets a whole panel (e.g. Settings, with
 * its many native controls across tab components) become controller-navigable WITHOUT tagging
 * each control by hand. `enabled=false` is a no-op (e.g. when the panel is shown in remote
 * mode). Native <select>/number <input> can be focused + activated but not value-edited by a
 * pad — buttons/toggles/seg-buttons are fully operable. */
const NAV_SEL =
	'button, select, input:not([type="hidden"]):not([type="range"]), a[href], [role="button"], [tabindex]:not([tabindex="-1"])';
export function navContainer(node: HTMLElement, enabled: boolean) {
	// DIFF-based registration: keep a map of el→unregister so a re-scan only ADDS newly
	// appeared controls and REMOVES vanished ones — existing items (and the current focus)
	// are left untouched. The old code destroyed+re-registered EVERY control on each scan,
	// which reset gamingNav's focus to the first item and yanked the scroll position; with a
	// MutationObserver firing on every Svelte childList change that made the gaming-mode
	// Settings panel stutter (focus/scroll fighting the user). Scans are also DEBOUNCED into
	// one trailing pass per mutation burst (a tab switch fires many childList mutations).
	const reg = new Map<HTMLElement, () => void>();
	let mo: MutationObserver | undefined;
	let on = false;
	let pending: ReturnType<typeof setTimeout> | 0 = 0;
	const doScan = () => {
		pending = 0;
		if (!on) return;
		const present = new Set<HTMLElement>();
		node.querySelectorAll<HTMLElement>(NAV_SEL).forEach((el) => {
			if (el.hasAttribute('disabled')) return;
			present.add(el);
			if (!reg.has(el)) {
				const r = gamingNav.item(el);
				reg.set(el, r && typeof r.destroy === 'function' ? r.destroy : () => {});
			}
		});
		for (const [el, off] of reg) {
			if (!present.has(el)) {
				off();
				reg.delete(el);
			}
		}
	};
	// Coalesce a burst of mutations into a single trailing scan (setTimeout, not rAF — rAF
	// pins the GDK frame clock on WebKitGTK, see POLL_MS).
	const scan = () => {
		if (pending || !on) return;
		pending = setTimeout(doScan, 50);
	};
	const clearAll = () => {
		for (const off of reg.values()) off();
		reg.clear();
	};
	const apply = (en: boolean) => {
		on = en;
		mo?.disconnect();
		mo = undefined;
		if (pending) {
			clearTimeout(pending);
			pending = 0;
		}
		if (en) {
			mo = new MutationObserver(scan);
			mo.observe(node, { childList: true, subtree: true });
			doScan(); // initial pass synchronously so first focus lands immediately
		} else {
			clearAll();
		}
	};
	apply(enabled);
	return {
		update: apply,
		destroy: () => {
			mo?.disconnect();
			if (pending) clearTimeout(pending);
			clearAll();
		}
	};
}

type Cand = {
	el: HTMLElement;
	cx: number;
	cy: number;
	top: number;
	bottom: number;
	left: number;
	right: number;
};

/** Nearest ancestor of `el` that scrolls vertically (overflow-y auto/scroll), or null. */
function scrollBox(el: HTMLElement): HTMLElement | null {
	for (let p = el.parentElement; p; p = p.parentElement) {
		const oy = getComputedStyle(p).overflowY;
		if ((oy === 'auto' || oy === 'scroll') && p.scrollHeight > p.clientHeight) return p;
	}
	return null;
}
