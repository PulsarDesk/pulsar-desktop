// Detachable session tabs (Chrome-style), the shell side. A tab dragged out of the strip
// opens in its own window; dropped on another Pulsar window's strip it moves there; the only
// tab of a detached window drags the whole window (and merges when dropped on a strip).
// Sessions never restart: the record moves, the Rust side moves the native video
// (src-tauri/src/tabwin.rs).

import { api, isTauri, onInputWindow, onTabAdopt, onTabDropHover, onTabMergeInto } from '#lib/api.js';
import type { SessionManager, Session, GroupMove } from './sessions.svelte';

/** Set when this window is a detached tab window (`tabwin-<n>`, injected by tabwin.rs). */
export const tabWinNo: number | null = (() => {
	if (typeof window === 'undefined') return null;
	const n = (window as unknown as { __TABWIN__?: number }).__TABWIN__;
	return typeof n === 'number' ? n : null;
})();
export const isTabWin = tabWinNo !== null;
export const myLabel = isTabWin ? `tabwin-${tabWinNo}` : 'main';

/** Lets an old window's session screen unmount (its 0×0 video report) before the video
 * moves — so that report can't land after the new window's. */
const settle = () => new Promise((r) => setTimeout(r, 60));
const log = (msg: string) => {
	if (isTauri) api.uiLog(`tabwin: ${msg}`).catch(() => {});
};

export class TabWindows {
	/** The session window that owns keyboard/mouse capture (focused last). */
	inputWin = $state('main');
	/** A tab from another window is over this window's tab area: show "drop here". */
	dropHover = $state(false);
	get ownsInput(): boolean {
		return this.inputWin === myLabel;
	}
	#sm: SessionManager;
	/** A tab window had sessions (so becoming empty means: close). */
	#hadSessions = false;
	/** Tabs released here but not yet handed to their new window: closing now would end their
	 * sessions (closing a tab window stops the plays still mapped to it). */
	#moving = 0;
	#count = 0;

	constructor(sm: SessionManager) {
		this.#sm = sm;
	}

	/** Wire up: input owner, adoption events, and (tab window) the handed-off sessions. */
	async init() {
		if (!isTauri) return;
		this.inputWin = await api.tabInputWindow().catch(() => 'main');
		await onInputWindow((l) => (this.inputWin = l));
		// Every window hears these (global listeners): each payload names the window it is for.
		await onTabAdopt(([to, payload]) => {
			if (to !== myLabel) return;
			this.dropHover = false;
			const before = this.#sm.sessions.length;
			try {
				this.#take(payload);
			} catch (e) {
				log(`adopt threw: ${e}`);
			}
			log(`adopted: ${before} → ${this.#sm.sessions.length} tab(s) here`);
		});
		await onTabDropHover(([to, on]) => {
			if (to === myLabel) this.dropHover = on;
		});
		if (isTabWin) {
			// Dropped (title bar or tab drag) on another window's tab area: move every tab there.
			await onTabMergeInto(([from, to]) => {
				if (from === myLabel) void this.#mergeAll(to);
			});
			const payload = await api.tabTakeHandoff().catch(() => []);
			this.#take(payload);
			log(`new window: took ${this.#sm.sessions.length} tab(s)`);
			// Opened focused: this window owns the input from the start.
			this.inputWin = myLabel;
		}
	}

	/** Tabs handed to this window: a list of tab records, or a whole split group. */
	#take(payload: unknown) {
		if (Array.isArray(payload)) this.#sm.adopt(payload);
		else if (payload && typeof payload === 'object' && 'group' in payload) this.#sm.adoptGroup(payload as GroupMove);
	}

	/** The split group tab dragged out of the window: the whole split moves (onto another
	 * window, or into a new one), layout and all. */
	dragGroupOut = async (grab: [number, number], screen: [number, number]) => {
		const target = await api.tabDropTarget(screen[0], screen[1]).catch(() => null);
		// Already the only thing in a tab window and dropped on no window: it stays.
		if (!target && isTabWin && this.#sm.sessions.every((s) => this.#sm.inGroup(s.tabId))) return;
		this.#moving++;
		const m = this.#sm.exportGroup();
		if (!m || !m.sessions.length) {
			this.#moving--;
			return;
		}
		const plays = m.sessions.map((s) => s.playId);
		log(`group drag out (${m.group.layout}, panes ${JSON.stringify(m.group.panes)}): plays ${plays.join(',')} → ${target ?? 'new window'}`);
		await settle();
		try {
			if (target) await api.tabMove(target, m, plays);
			else await api.tabDetach(m, plays, grab, screen[0], screen[1]);
		} catch (e) {
			log(`group move failed (${e}) — kept here`);
			this.#sm.adoptGroup(m);
		} finally {
			this.#moving--;
			this.watchEmpty(this.#count);
		}
	};

	/** A tab window whose last session left (moved away or ended) closes. */
	watchEmpty(count: number) {
		this.#count = count;
		if (!isTabWin) return;
		if (count > 0) {
			this.#hadSessions = true;
			return;
		}
		if (!this.#hadSessions || this.#moving > 0) return;
		log('emptied → closing this tab window');
		void (async () => {
			const { getCurrentWindow } = await import('@tauri-apps/api/window');
			await getCurrentWindow().close();
		})();
	}

	/** Report where this window's tab area is (the top chrome + tab strip) so another
	 * window's dragged tab can be dropped onto it. `on=false` (fullscreen, split): no target. */
	reportStrip(on: boolean) {
		if (!isTauri) return;
		const els = on
			? [document.querySelector<HTMLElement>('.chrome'), document.querySelector<HTMLElement>('.tabs')]
			: [];
		const rects = els.filter((e): e is HTMLElement => !!e).map((e) => e.getBoundingClientRect());
		if (!rects.length) {
			api.tabStripRect(0, 0, 0, 0).catch(() => {});
			return;
		}
		const l = Math.min(...rects.map((r) => r.left));
		const t = Math.min(...rects.map((r) => r.top));
		const r = Math.max(...rects.map((r) => r.right));
		const b = Math.max(...rects.map((r) => r.bottom));
		api.tabStripRect(l, t, r - l, b - t).catch(() => {});
	}

	/** Tab `tabId` dropped outside the strip: onto another window's strip → move it there;
	 * anywhere else → its own new window under the pointer. */
	dragOut = async (tabId: number, grab: [number, number], screen: [number, number]) => {
		if (!this.#sm.canMove(tabId)) return;
		const target = await api.tabDropTarget(screen[0], screen[1]).catch(() => null);
		// The last tab of a tab window dropped on no strip: it already has its own window.
		if (!target && isTabWin && this.#sm.sessions.length <= 1) return;
		this.#moving++;
		// A split member leaves its group first (an ordinary tab moves).
		if (this.#sm.inGroup(tabId)) this.#sm.ungroup(tabId);
		const rec = this.#sm.release(tabId);
		if (!rec) {
			this.#moving--;
			return;
		}
		log(`drag out: play ${rec.playId} → ${target ?? 'new window'}`);
		await settle();
		try {
			if (target) await api.tabMove(target, [rec], [rec.playId]);
			else await api.tabDetach([rec], [rec.playId], grab, screen[0], screen[1]);
		} catch (e) {
			// The window couldn't be made / found: keep the tab here.
			console.warn('[tabwin] move failed', e);
			log(`move failed (${e}) — tab kept here`);
			this.#sm.adopt([rec]);
		} finally {
			this.#moving--;
			// Handed over: now an emptied tab window may close.
			this.watchEmpty(this.#count);
		}
	};

	/** Move every movable tab of this window into window `to` (it then closes, empty). */
	async #mergeAll(to: string) {
		const recs: Session[] = [];
		this.#moving++;
		for (const s of [...this.#sm.sessions]) {
			if (!this.#sm.canMove(s.tabId)) continue;
			const rec = this.#sm.release(s.tabId);
			if (rec) recs.push(rec);
		}
		if (!recs.length) {
			this.#moving--;
			return;
		}
		await settle();
		await api
			.tabMove(
				to,
				recs,
				recs.map((r) => r.playId)
			)
			.catch(() => this.#sm.adopt(recs));
		this.#moving--;
		this.watchEmpty(this.#count);
	}
}
