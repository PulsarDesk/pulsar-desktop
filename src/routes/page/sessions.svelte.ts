// Session/tab orchestration for the app shell (+page.svelte). Owns the list of
// concurrent host connections (each a tab), the active tab, fullscreen state, and
// the connect/disconnect lifecycle. Kept out of the route component so the shell
// markup stays small; behavior is identical to the inline version it replaced.

import { savedStreamPicks } from '#lib/streamPicks.js';
import { api, isTauri, onHostDisplays, onPeerAvatar, onPeerName, onPlayEnded, onPlayError, onPlayStats, onPlayOptimizing, onPlayReady, onSystemSleep, onSystemWake, setFullscreen } from '#lib/api.js';
import { recordConnection, setPeerIdentity } from '#lib/peers.svelte.js';
import { PORTAL_PICK } from '#lib/api.types.js';
import { ui, saveUi } from '#lib/settings.svelte.js';
import { t } from '#lib/i18n.svelte.js';
import type { Game } from '#lib/games.svelte.js';

export type Target = { name: string; id: string };
export type Session = {
	tabId: number;
	playId: number; // Rust play id (-1 until active / local host session)
	phase: 'connecting' | 'active';
	/** First decoded frames have landed (`play-ready`) — the native child window is now
	 * actually painting over the webview. Drives the occlusion repaint-suspend. */
	ready?: boolean;
	/** First frames are in, held back while the stream is optimized (connecting screen step). */
	optimizing?: boolean;
	target: Target;
	/** User-renamed tab title (session-scoped, via the tab's edit button); falls
	 * back to `target.name` when unset. */
	label?: string;
	mode: 'remote' | 'game';
	/** For game-mode sessions: the game id passed to startConnect (empty = none). */
	gameId?: string;
	conn: 'direct' | 'relay';
	wsPort: number;
	native?: boolean;
	embedded?: boolean;
	/** Host's validated stream caps (empty = unknown) — gates the session-menu options. */
	hostCodecs?: string[];
	hostEncoders?: string[];
	/** Host's streamable monitors (primary first); the session menu's screen picker. */
	hostDisplays?: import('#lib/api.types.js').HostDisplay[];
	/** The host monitor this session STARTED on (0 = primary). Read by a sibling same-host
	 * pane so two panes to the same host claim DIFFERENT monitors (the host's DXGI
	 * Desktop-Duplication is single-owner per monitor). Undefined = host default (0). The
	 * session menu's live monitor switch doesn't update this — it's the connect-time claim
	 * used only for the free-display search; a live switch is the user's explicit choice. */
	displayIdx?: number;
	/** The host WINDOW this session captures (Phase 2b co-op "Pencere" mode): a raw Win32
	 * HWND the host WGC-captures instead of a whole monitor, so two same-host panes can
	 * share ONE monitor. Undefined = whole-monitor capture (display/game default). Set at
	 * connect time from the PaneConnect window picker (or a host-resolved game window). */
	windowHwnd?: number;
	// For a local host-game tab: the game's stop command, run when this tab closes.
	// Per-session (not a shared global) so closing one tab never stops another game.
	stopCmd?: string;
	/** Moved here from another window (a dragged tab) — already running there. */
	adopted?: boolean;
};

/** A whole split group handed between windows (tab_detach / tab_move payload). */
export type GroupMove = {
	group: {
		layout: 'h2' | 'v2' | 'grid4';
		sessionMode: 'game' | 'remote';
		focusedPane: number;
		/** Pane order, by PLAY id (tab ids are per window); null = empty pane. */
		panes: (number | null)[];
	};
	sessions: Session[];
};

const STREAM_PORT = 9000;

// A connect attempt that neither resolves nor rejects (e.g. the host accepted the
// transport but never answers the auth/caps step, or the link dropped mid-handshake
// after the await began) would otherwise leave the Connecting screen spinning forever
// — the core's auth recv loop has no timeout. Fail the attempt after this deadline so
// the user lands back on Home with a friendly error instead of an endless spinner.
const CONNECT_TIMEOUT_MS = 45_000;

/** Marker thrown when the connect attempt blows past `CONNECT_TIMEOUT_MS`. */
const CONNECT_TIMEOUT = 'connect-timed-out';

// How long to wait after auth resumes (password submitted / prompt closed) before
// timing out the post-auth handshake. Separate from CONNECT_TIMEOUT_MS because the
// full 45 s is generous for a network handshake but auth itself is unbounded.
const POST_AUTH_TIMEOUT_MS = 30_000;

// Known core connect errors arrive as raw English Rust strings (ConnError) — map them
// to friendly copy IN THE ACTIVE UI LANGUAGE for the connect flash (substring match
// so wrapped/formatted variants still hit). Unknown messages fall through verbatim.
function friendlyConnectError(raw: string): string {
	const m = raw.toLowerCase();
	if (m.includes(CONNECT_TIMEOUT)) return t('connErr.timeout');
	if (m.includes('relay did not respond')) return t('connErr.relayDown');
	if (m.includes('could not be reached via the relay')) return t('connErr.peerUnreachable');
	if (m.includes('not registered with a relay yet')) return t('connErr.notOnline');
	if (m.includes('p2p connection failed')) return t('connErr.p2pFailed');
	if (m.includes('network is unreachable') || m.includes('connection refused') || m.includes('timed out'))
		return t('connErr.unreachable');
	return raw;
}

type Deps = {
	/** Current default connect mode (from the shell's mode toggle). */
	getMode: () => 'remote' | 'game';
	/** Called when a connection attempt finishes (success or fail) so the shell can
	 * dismiss THAT target's password prompt (others may still be pending). */
	onAuthDone: (targetId: string) => void;
};

/** A session the machine's sleep ended: what it takes to connect it again. */
type Resume = {
	target: Target;
	mode: 'remote' | 'game';
	gameId: string;
	displayIdx?: number;
	windowHwnd?: number;
	label?: string;
};
/** After wake-up the network is rarely back at once: wait this long, then try again a few
 * times (ms after the previous try). */
export const RESUME_DELAYS_MS = [2500, 5000, 10_000, 20_000];

export class SessionManager {
	// Multiple concurrent host connections, each a tab. `activeTab` is 'home' or a
	// session's tabId. Fullscreen hides all chrome/tabs (only the active host).
	sessions = $state<Session[]>([]);
	#activeTab = $state<'home' | number>('home');
	get activeTab(): 'home' | number {
		return this.#activeTab;
	}
	/** Switching to a session tab makes it the input target: with several tabs open, pads
	 * (incl. a DualSense driven directly) go to the focused tab only (play.rs route_here). */
	set activeTab(v: 'home' | number) {
		this.#activeTab = v;
		if (typeof v === 'number' && this.splitMode === 'off') this.#focusTab(v);
	}
	#focusTab(tabId: number) {
		const s = this.sessions.find((x) => x.tabId === tabId);
		if (s && s.playId >= 0) api.setActiveSession(s.playId).catch(() => {});
	}
	fullscreen = $state(false);
	// Window fullscreen state when the first session of a run opened (null = no session open), and
	// whether the USER toggled fullscreen while sessions were open. When the last session ends the
	// window goes back to how it was before (`#fsBefore`) — unless the user changed it meanwhile,
	// then their choice stays.
	#fsBefore: boolean | null = null;
	#fsTouched = false;
	#noteSessionStart() {
		if (this.sessions.length > 0) return;
		this.#fsBefore = this.fullscreen;
		this.#fsTouched = false;
	}
	connectErr = $state('');

	// ── Split mode ──────────────────────────────────────────────────────────────
	// The window can be divided into 2 (left/right `h2`, top/bottom `v2`) or 4 (2×2
	// `grid4`) panes, each an INDEPENDENT session (same or different host). `'off'` is
	// the normal AnyDesk-style single-tab flow (unchanged). Each `panes[]` entry is the
	// tabId of an existing session OR `null` (empty pane → PaneConnect). `focusedPane`
	// drives default keyboard/mouse routing (the focused pane's session is the active
	// one). All sessions stay mounted in the background even when not in a pane (dropping
	// to a smaller layout keeps extra sessions as ordinary tabs, never disconnects them).
	splitMode = $state<'off' | 'h2' | 'v2' | 'grid4'>('off');
	panes = $state<(number | null)[]>([]);
	focusedPane = $state(0);
	// In split mode EVERY pane is the SAME personality — the mode the app was in when split
	// was entered (gaming OR remote), never a gaming+remote mix. Captured by enterSplit and
	// FORCED onto every connectIntoPane (per-pane mode is ignored). Drives the fixed outer
	// chrome look (`data-gaming`) and which connect UI PaneConnect renders. Defaults to the
	// app's current personality so a caller that omits it inherits ui.appMode.
	splitSessionMode = $state<'game' | 'remote'>('remote');

	/** How many panes a layout has. */
	#layoutSize(layout: 'h2' | 'v2' | 'grid4'): number {
		return layout === 'grid4' ? 4 : 2;
	}

	/** Number of panes currently holding a live session (drives the backend pane cap +
	 * the per-cell resolution reduction). */
	get activePaneCount(): number {
		if (this.splitMode === 'off') return 1;
		return Math.max(1, this.panes.filter((p) => p !== null).length);
	}

	/** Per-cell reduced resolution requested from the host while split: each cell shows
	 * a fraction of the screen, so a full-res stream per pane is wasted bandwidth/decode
	 * (critical on the Pi). 2-up → 720p per cell, 4-up → 540p per cell; single pane keeps
	 * the host default (0×0). Applied live via setPlayResolution once the session is up. */
	#paneResolution(): { width: number; height: number } {
		const n = this.activePaneCount;
		if (n <= 1) return { width: 0, height: 0 };
		if (n === 2) return { width: 1280, height: 720 };
		return { width: 960, height: 540 };
	}

	/** Push the active-pane count to the backend (resident-pool reap cap) after any
	 * pane-count change. */
	#syncPaneCount() {
		api.setPaneCount(this.activePaneCount).catch(() => {});
	}

	/** Re-apply the per-cell reduced resolution to every paned session (called whenever
	 * the pane count changes, so a 4→2 reshape bumps the remaining cells back up). */
	#applyPaneResolutions() {
		if (this.splitMode === 'off') return;
		const { width, height } = this.#paneResolution();
		for (const tabId of this.panes) {
			if (tabId === null) continue;
			const s = this.sessions.find((x) => x.tabId === tabId);
			if (s && s.playId >= 0) api.setPlayResolution(s.playId, width, height).catch(() => {});
		}
	}

	/** Enter split mode with `layout`: seed pane 0 with the current session (if a
	 * session tab is active), the rest empty. Focuses pane 0. `mode` fixes the personality
	 * of EVERY pane (the mode the app was in when split was entered); omit it to inherit the
	 * app's current `ui.appMode`. */
	enterSplit(layout: 'h2' | 'v2' | 'grid4', mode: 'game' | 'remote' = ui.appMode) {
		// A parked group becomes ordinary tabs (one split group per window).
		this.parked = null;
		const size = this.#layoutSize(layout);
		const seed = typeof this.activeTab === 'number' ? this.activeTab : null;
		const next: (number | null)[] = new Array(size).fill(null);
		next[0] = seed;
		// The other panes take the tabs already open (that mode, live, most recent first) —
		// an empty connect pane only when there are not enough of them.
		const seedMode = seed !== null ? this.sessions.find((s) => s.tabId === seed)?.mode : undefined;
		if (seedMode !== undefined) mode = seedMode;
		const rest = this.sessions
			.filter((s) => s.tabId !== seed && s.mode === mode && s.phase === 'active' && s.playId >= 0)
			.reverse();
		for (let i = 0; i < size; i++) {
			if (next[i] === null) next[i] = rest.shift()?.tabId ?? null;
		}
		this.splitSessionMode = mode;
		this.panes = next;
		this.splitMode = layout;
		this.focusedPane = 0;
		this.#syncPaneCount();
		this.#applyPaneResolutions();
		if (seed !== null) this.focusPane(0);
	}

	/** Leave split mode → back to the normal single-tab flow. The session that was in the
	 * focused pane becomes the active tab; every other paned/background session stays
	 * mounted as a tab (nothing is disconnected). Restores host-default resolution. */
	exitSplit() {
		const focused = this.panes[this.focusedPane];
		const restore =
			typeof focused === 'number'
				? focused
				: (this.panes.find((p) => p !== null) ?? null);
		this.splitMode = 'off';
		this.panes = [];
		this.focusedPane = 0;
		this.activeTab = restore ?? (this.sessions.length ? this.sessions[this.sessions.length - 1].tabId : 'home');
		this.#syncPaneCount();
		// Restore each (still-mounted) session to the host-default resolution.
		for (const s of this.sessions) {
			if (s.playId >= 0) api.setPlayResolution(s.playId, 0, 0).catch(() => {});
		}
		if (typeof this.activeTab === 'number') {
			const s = this.sessions.find((x) => x.tabId === this.activeTab);
			if (s && s.playId >= 0) api.setActiveSession(s.playId).catch(() => {});
		}
	}

	/** Reshape the grid without disconnecting any session. Growing 2→4 keeps the existing
	 * panes and adds empty cells; shrinking 4→2 keeps the first panes and leaves the
	 * dropped panes' sessions running as background tabs. */
	setLayout(layout: 'h2' | 'v2' | 'grid4', mode: 'game' | 'remote' = ui.appMode) {
		if (this.splitMode === 'off') {
			this.enterSplit(layout, mode);
			return;
		}
		// Already split: a reshape PRESERVES the established split personality — every pane
		// stays the mode split was entered in (splitSessionMode is left untouched).
		const size = this.#layoutSize(layout);
		const next: (number | null)[] = new Array(size).fill(null);
		for (let i = 0; i < size; i++) next[i] = this.panes[i] ?? null;
		this.panes = next;
		this.splitMode = layout;
		if (this.focusedPane >= size) this.focusedPane = 0;
		this.#syncPaneCount();
		this.#applyPaneResolutions();
		this.focusPane(this.focusedPane);
	}

	/** Focus a pane: routes default keyboard/mouse + unlocked controllers to its session
	 * (api.setActiveSession). A focused empty pane just clears the keyboard/mouse target. */
	focusPane(i: number) {
		if (i < 0 || i >= this.panes.length) return;
		this.focusedPane = i;
		const tabId = this.panes[i];
		if (typeof tabId === 'number') {
			const s = this.sessions.find((x) => x.tabId === tabId);
			if (s && s.playId >= 0) api.setActiveSession(s.playId).catch(() => {});
		}
	}

	/** Despaced id, the form sessions are matched on. */
	#despace(id: string): string {
		return id.replace(/\s/g, '');
	}

	/** Live (connecting/active) sessions that target host `id`. */
	#liveSessionsForHost(id: string): Session[] {
		const want = this.#despace(id);
		if (!want) return [];
		return this.sessions.filter((s) => this.#despace(s.target.id) === want);
	}

	/** The host monitors known for host `id` — read from ANY live session to that host
	 * (they all share the same host caps). Empty until the first pane to this host is up. */
	hostDisplaysFor(id: string): import('#lib/api.types.js').HostDisplay[] {
		for (const s of this.#liveSessionsForHost(id)) {
			if (s.hostDisplays && s.hostDisplays.length) return s.hostDisplays;
		}
		return [];
	}

	/** The Rust play id of ANY active session to host `id` — used to route a `hostWindowList`
	 * query (the command needs an existing session's control channel). Returns undefined when
	 * no live session to that host exists yet (the first pane can't list windows, only display/
	 * game; the picker for "Pencere" appears once a sibling same-host session is up). */
	livePlayIdForHost(id: string): number | undefined {
		for (const s of this.#liveSessionsForHost(id)) {
			if (s.playId >= 0) return s.playId;
		}
		return undefined;
	}

	/** What each pane streams from host `id` — for the pane source picker's "in pane N" tags.
	 * A window session maps its hwnd; a plain screen session (no game, no window) its monitor. */
	paneUsageFor(id: string): { windows: Record<number, number>; displays: Record<number, number> } {
		const windows: Record<number, number> = {};
		const displays: Record<number, number> = {};
		for (const s of this.#liveSessionsForHost(id)) {
			const pane = this.panes.indexOf(s.tabId);
			if (pane < 0) continue;
			// A Wayland portal pick is no window id: each pane gets its own window in the
			// host's dialog, so it never marks the single picker entry as taken.
			if (s.windowHwnd === PORTAL_PICK) continue;
			if (s.windowHwnd !== undefined) windows[s.windowHwnd] = pane + 1;
			else if (!s.gameId) displays[s.displayIdx ?? 0] = pane + 1;
		}
		return { windows, displays };
	}

	/** Host monitor indices already CLAIMED by a live session to host `id` (its connect-time
	 * `displayIdx`, default 0). Used to keep two same-host panes off the same monitor. */
	usedDisplaysFor(id: string, exceptTab?: number): Set<number> {
		const used = new Set<number>();
		for (const s of this.#liveSessionsForHost(id)) {
			if (s.tabId === exceptTab) continue;
			used.add(s.displayIdx ?? 0);
		}
		return used;
	}

	/** The FIRST free host monitor for a NEW pane to host `id`: the lowest `hostDisplays`
	 * index not already claimed by a sibling same-host session. Returns `undefined` (→ host
	 * default 0, single-session behavior preserved) when no sibling exists yet OR every
	 * known monitor is taken (the host then clamps + Phase 2b's same-monitor capture covers
	 * the overflow). */
	freeDisplayFor(id: string): number | undefined {
		const siblings = this.#liveSessionsForHost(id);
		if (!siblings.length) return undefined; // first pane → host default (0)
		const used = this.usedDisplaysFor(id);
		const displays = this.hostDisplaysFor(id);
		// Displays known: pick the lowest free advertised idx. Unknown (sibling caps not in
		// yet): fall back to the lowest non-negative idx not in `used`.
		const candidates = displays.length ? displays.map((d) => d.idx) : [0, 1, 2, 3];
		for (const idx of candidates) {
			if (!used.has(idx)) return idx;
		}
		return undefined; // all taken → host default / Phase 2b
	}

	/** Connect into an empty pane: runs the EXISTING startConnect (which de-dups but
	 * permits same-host distinct sessions), then assigns the resulting session's tabId
	 * into panes[i]. The reduced per-cell resolution is applied once the session goes
	 * active. A second pane to a host that already has a live session is auto-assigned a
	 * FREE host monitor (so both panes capture different screens), unless the caller passes
	 * an explicit `displayIdx` (the PaneConnect monitor picker). The pane's session mode is
	 * always FORCED to `splitSessionMode` — every pane is the split's fixed personality, so
	 * the caller's `_m` (if any) is ignored. */
	connectIntoPane = async (
		i: number,
		target: Target,
		_m?: 'remote' | 'game',
		gameId = '',
		displayIdx?: number,
		windowHwnd?: number
	) => {
		if (i < 0 || i >= this.panes.length) return;
		// Force the split's fixed personality onto this pane — no mixed gaming+remote split.
		const m = this.splitSessionMode;
		const before = new Set(this.sessions.map((s) => s.tabId));
		// "Pencere" mode (window capture) shares a monitor, so display-exclusivity does NOT
		// apply — skip the free-monitor search and let the host capture the chosen window.
		// Otherwise (display/game) pick a free monitor for a same-host second pane unless the
		// picker already chose one.
		const wantDisplay =
			windowHwnd !== undefined ? undefined : (displayIdx ?? this.freeDisplayFor(target.id));
		await this.startConnect(target, m, gameId, { displayIdx: wantDisplay, windowHwnd, newSession: true });
		// The new session is the tab that wasn't there before; if startConnect de-duped to
		// an existing session it focused it (activeTab), so fall back to that.
		const added = this.sessions.find((s) => !before.has(s.tabId));
		const tabId =
			added?.tabId ?? (typeof this.activeTab === 'number' ? this.activeTab : null);
		if (tabId === null) return;
		this.panes[i] = tabId;
		this.focusPane(i);
		this.#syncPaneCount();
		this.#applyPaneResolutions();
	};

	#nextTab = 0;
	#deps: Deps;
	// Sessions the machine's sleep ended, to connect again when it is back (systemSleep /
	// systemWake), and a counter that every sleep and wake bumps so a reconnect round that was
	// overtaken stops.
	#resume: Resume[] = [];
	#powerGen = 0;
	// Targets (despaced peer id) for which interactive auth (password prompt or
	// host Allow/Deny) is currently in progress. The connect timeout is paused
	// while the target is in this set — auth time must not count against the
	// handshake deadline, since auth is a human-interaction step.
	#authInProgress = new Set<string>();
	// When the timeout fires while auth is in progress it parks {reject, arm} here
	// (keyed by despaced peer id) rather than immediately rejecting. Once the user
	// submits a password (notifyAuthSubmit) we re-arm for POST_AUTH_TIMEOUT_MS.
	// Once the connect finishes (finally block) or the tab closes, the entry is
	// deleted — any parked arm that lost its race just becomes a no-op.
	#pendingTimeoutReject = new Map<string, { reject: (e: Error) => void; arm: (ms: number) => void }>();

	constructor(deps: Deps) {
		this.#deps = deps;
		// The Connecting screen holds until the stream is REALLY up (first decoded
		// frames — `play-ready`), so the user never lands on a black session.
		onPlayReady((playId) => this.#activateByPlayId(playId));
		onPlayOptimizing((playId) => {
			this.sessions = this.sessions.map((s) => (s.playId === playId ? { ...s, optimizing: true } : s));
		});
		// The HOST's pushed identity (image + name) lands at stream start, keyed by play id —
		// usually BEFORE Session.svelte mounts (it waits for play-ready), so its listener missed
		// it and recents / LAN never got the host's face. Cache it here (always alive) against
		// the session's target id; a push that beats the play id is parked until #patch sets it.
		onPeerAvatar((e) => this.#identity(e.peer, { avatar: e.dataUrl }));
		onPeerName((e) => this.#identity(e.peer, { name: e.name }));
		// The host's monitors changed mid-session → the session menu's monitor picker follows.
		onHostDisplays((e) => {
			this.sessions = this.sessions.map((s) => (s.playId === e.id ? { ...s, hostDisplays: e.displays } : s));
		});
		// A session can die during the connecting-hold window (after start_remote_play
		// resolves but before play-ready) — Session.svelte isn't mounted yet to see
		// `play-ended` then, so close the tab here regardless of phase. (Local host-game
		// tabs carry playId -1 and can never match.)
		onPlayEnded((playId) => {
			const s = this.sessions.find((x) => x.playId === playId);
			if (s) this.endSession(s.tabId);
		});
		onSystemSleep(() => this.systemSleep());
		onSystemWake(() => void this.systemWake());
		// Why a session ended / can't start, shown on Home instead of the tab just vanishing:
		// the host stopped answering, or (still connecting) the host reports a stream error
		// ("!…" stats, e.g. it can't capture its screen).
		onPlayError(([playId, reason]) => {
			if (!this.sessions.some((x) => x.playId === playId)) return;
			if (reason === 'hostSilent') this.connectErr = t('connErr.hostSilent');
		});
		onPlayStats((e) => {
			if (!e.label.startsWith('!')) return;
			const s = this.sessions.find((x) => x.playId === e.id);
			if (!s || s.phase !== 'connecting') return;
			this.connectErr = e.label.slice(1);
			this.endSession(s.tabId);
		});
	}

	/** The machine goes to sleep (suspend, display off): every session it is viewing ends —
	 * a connected session keeps the machine awake (its audio stream) and the host encoding —
	 * and is remembered for `systemWake`. Rust ends the plays too (the webview may be throttled
	 * with the display off); ending them here as well is harmless. A local host-game tab (no
	 * peer) stays. */
	systemSleep = () => {
		this.#powerGen++;
		for (const s of [...this.sessions]) {
			const remote = s.playId >= 0 || s.phase === 'connecting';
			if (!remote) continue;
			this.#remember({
				target: s.target,
				mode: s.mode,
				gameId: s.gameId ?? '',
				displayIdx: s.displayIdx,
				windowHwnd: s.windowHwnd,
				label: s.label
			});
			this.endSession(s.tabId);
		}
	};

	#remember(r: Resume) {
		const same = (x: Resume) =>
			x.target.id === r.target.id && x.mode === r.mode && x.gameId === r.gameId && x.displayIdx === r.displayIdx && x.windowHwnd === r.windowHwnd;
		if (!this.#resume.some(same)) this.#resume.push(r);
	}

	/** The machine is back: connect the sessions sleep ended again, in a few rounds
	 * (`RESUME_DELAYS_MS`) because the network usually returns after the screen. A round stops
	 * when the machine sleeps again (the sessions already back are remembered again by
	 * `systemSleep`, the rest stay remembered). What still fails after the last round is given
	 * up; its error stays on Home like any failed connect. */
	systemWake = async () => {
		const gen = ++this.#powerGen;
		for (const delay of RESUME_DELAYS_MS) {
			if (this.#resume.length === 0) return;
			await new Promise((r) => setTimeout(r, delay));
			if (gen !== this.#powerGen) return;
			const todo = this.#resume;
			this.#resume = [];
			for (let i = 0; i < todo.length; i++) {
				const r = todo[i];
				const before = new Set(this.sessions.map((s) => s.tabId));
				await this.startConnect(r.target, r.mode, r.gameId, {
					...(r.displayIdx !== undefined ? { displayIdx: r.displayIdx } : {}),
					...(r.windowHwnd !== undefined ? { windowHwnd: r.windowHwnd } : {})
				});
				const tab = this.sessions.find((s) => !before.has(s.tabId));
				if (tab && r.label) this.renameTab(tab.tabId, r.label);
				if (gen !== this.#powerGen) {
					// Asleep again mid-round: what was not tried yet stays remembered (a session
					// that did come back is in `sessions`, so `systemSleep` took it).
					for (const rest of todo.slice(tab ? i + 1 : i)) this.#remember(rest);
					return;
				}
				if (!tab) this.#remember(r);
			}
		}
		this.#resume = [];
	};

	#activateByPlayId(playId: number) {
		this.sessions = this.sessions.map((s) =>
			s.playId === playId ? { ...s, phase: 'active', ready: true } : s
		);
		// Tabs: the active tab's session became live now that its play id exists.
		if (this.splitMode === 'off' && typeof this.activeTab === 'number') {
			const s = this.sessions.find((x) => x.playId === playId);
			if (s && s.tabId === this.activeTab) api.setActiveSession(playId).catch(() => {});
		}
		// In split mode, a freshly-active paned session must get its reduced per-cell
		// resolution (it could only be applied once the play id existed) and, if it's the
		// focused pane, become the input target.
		if (this.splitMode !== 'off') {
			const s = this.sessions.find((x) => x.playId === playId);
			if (s && this.panes.includes(s.tabId)) {
				const { width, height } = this.#paneResolution();
				api.setPlayResolution(playId, width, height).catch(() => {});
				if (this.panes[this.focusedPane] === s.tabId)
					api.setActiveSession(playId).catch(() => {});
			}
		}
	}

	#pendingIdentity = new Map<string, { name?: string; avatar?: string }>();

	#identity(peer: string, patch: { name?: string; avatar?: string }) {
		if (!isTauri) return;
		const s = this.sessions.find((x) => x.playId != null && x.playId >= 0 && String(x.playId) === peer);
		if (s) setPeerIdentity(s.target.id, patch);
		else if (/^\d+$/.test(peer) && peer.length < 9) {
			// A play id we don't know YET (start_remote_play hasn't resolved). Keep it small.
			if (this.#pendingIdentity.size > 16) this.#pendingIdentity.clear();
			this.#pendingIdentity.set(peer, { ...this.#pendingIdentity.get(peer), ...patch });
		}
	}

	#patch(tabId: number, patch: Partial<Session>) {
		this.sessions = this.sessions.map((s) => (s.tabId === tabId ? { ...s, ...patch } : s));
	}

	startConnect = async (
		target: Target,
		m?: 'remote' | 'game',
		gameId = '',
		opts: {
			holdConnecting?: Promise<unknown>;
			/** Initial host monitor to capture (0 = primary). Passed by a same-host second
			 * pane (split mode) so it lands on a FREE screen. Its presence ALSO opts the
			 * connect out of the same-(host,mode,game) de-dup — a deliberate second pane to a
			 * host that already has an identical session must become a SECOND real session,
			 * not focus the first. */
			displayIdx?: number;
			/** Initial per-WINDOW capture target (Phase 2b co-op "Pencere"): a raw Win32 HWND
			 * the host WGC-captures instead of a whole monitor. Like `displayIdx`, its presence
			 * opts the connect out of the same-(host,mode,game) de-dup (a deliberate second pane
			 * capturing a specific window is its own session). Wins over the monitor on the host. */
			windowHwnd?: number;
			/** A split-pane connect: always a NEW session, never de-duped onto an open one
			 * (a single-monitor host has no free display to pass, and two panes of the same
			 * screen are a deliberate choice — de-duping put one session in both cells and
			 * left the second one gray). */
			newSession?: boolean;
		} = {}
	) => {
		const useMode = m ?? this.#deps.getMode();
		this.connectErr = '';
		// Dedupe: a repeated connect to the same peer (e.g. a re-fired reverse-direction
		// `reverse-request`, or a double-click) must not open a second tab to that device —
		// that would spin up duplicate viewer ports + a second native renderer and confuse the
		// tab strip. If an identical session (same id + same mode + same gameId) is already
		// connecting/active, focus it instead. Matched on despaced id; local host-game tabs
		// (no real peer id) never collide. A blank target id is treated as unknown and never
		// deduped. NOTE: different mode (remote vs game) or different gameId are NOT deduped —
		// those are legitimately distinct sessions to the same host. A split-pane connect that
		// carries an explicit `displayIdx` is ALSO never deduped: it's a deliberate second pane
		// to the same (host,mode,game) — e.g. two "Masaüstü" panes (gameId '') — that must
		// become two real sessions on different monitors, not collapse onto the first. An
		// explicit `windowHwnd` (the "Pencere" picker) opts out of de-dup the same way — a
		// pane capturing a specific host window is its own session even on a shared monitor.
		const want = target.id.replace(/\s/g, '');
		// Settings → "Aynı cihaza birden fazla oturum": every connect is a new session.
		if (want && !ui.duplicateSessions && !opts.newSession && opts.displayIdx === undefined && opts.windowHwnd === undefined) {
			const existing = this.sessions.find(
				(s) =>
					s.target.id.replace(/\s/g, '') === want &&
					s.mode === useMode &&
					(useMode !== 'game' || (s.gameId ?? '') === (gameId ?? ''))
			);
			if (existing) {
				this.activeTab = existing.tabId;
				return;
			}
		}
		this.#noteSessionStart();
		const tabId = this.#nextTab++;
		this.sessions = [
			...this.sessions,
			{ tabId, playId: -1, phase: 'connecting', target, mode: useMode, gameId: gameId || undefined, conn: 'direct', wsPort: 0, displayIdx: opts.displayIdx, windowHwnd: opts.windowHwnd }
		];
		this.activeTab = tabId;
		// Headless --connect: hold the actual network connect until the launch splash is gone,
		// so the Connecting screen (and its real P2P/relay status) is on-screen for the whole
		// connection instead of being hidden behind the splash. No-op for manual connects.
		if (opts.holdConnecting) {
			await opts.holdConnecting;
			// The user may have cancelled (closed the connecting tab) during the splash hold.
			if (!this.sessions.some((s) => s.tabId === tabId)) return;
		}
		// Auth (password prompt + host Allow/Deny) happens during this call, driven
		// by events — no password is passed up front.
		// Hoisted so the finally block can clean up auth-pause state regardless of
		// whether the try body threw before or after peerKey was used.
		const peerKey = target.id.replace(/\s/g, '');
		try {
			// Guard the connect with a deadline: if it neither resolves nor rejects in
			// time, abort with a timeout error (routed through friendlyConnectError) so
			// the Connecting screen can't spin forever. Keep the underlying promise so a
			// late resolution (after the timeout fired) can still be torn down rather than
			// leaking its viewer ports + native renderer + the held host session.
			const play = api.startRemotePlay(
				target.id,
				gameId,
				STREAM_PORT,
				ui.codec,
				ui.encoder,
				ui.forwardControllers,
				useMode === 'game',
				ui.quality,
				ui.touchpadAsMouse,
				opts.displayIdx,
				opts.windowHwnd,
				savedStreamPicks(),
				ui.audioQuality
			);
			let timer: ReturnType<typeof setTimeout> | undefined;
			let timedOut = false;
			// Capture `this` for use in closures below (class private fields can't be
			// accessed via a free `this` alias in all TS targets, so we assign early).
			const self = this;
			const timeout = new Promise<never>((_, reject) => {
				// Reschedule when auth is in progress so human interaction time (typing a
				// password, waiting for Allow/Deny) does not count against the deadline.
				// Each reschedule grants POST_AUTH_TIMEOUT_MS for the post-auth handshake.
				function arm(ms: number) {
					timer = setTimeout(() => {
						if (peerKey && self.#authInProgress.has(peerKey)) {
							// Auth is active — park the reject+arm until notifyAuthSubmit
							// re-arms or notifyAuthEnd clears the pause.
							self.#pendingTimeoutReject.set(peerKey, { reject, arm });
							return;
						}
						timedOut = true;
						reject(new Error(CONNECT_TIMEOUT));
					}, ms);
				}
				arm(CONNECT_TIMEOUT_MS);
			});
			// If the connect wins the race, stop the timeout from later rejecting (and
			// leaking the timer). If the timeout wins, a late-resolving connect must still
			// stop its stream so we don't strand a half-open host session.
			play.then(
				(info) => {
					if (timedOut) api.stopStream(info.id).catch(() => {});
				},
				() => {}
			);
			const info = await Promise.race([play, timeout]).finally(() => clearTimeout(timer));
			// The user may have cancelled (closed the connecting tab) while the auth/connect
			// was in flight. If so the tab is gone, so #patch would be a no-op and the UI would
			// never learn the play id → tear down the session we just created instead of leaking
			// its viewer ports + native renderer + the held host session.
			if (!this.sessions.some((s) => s.tabId === tabId)) {
				api.stopStream(info.id).catch(() => {});
				return;
			}
			// Connected, but HOLD the Connecting screen until first frames arrive
			// (`play-ready`, video+audio pipelines start together host-side). A
			// fallback timer activates anyway so a stats-less edge case can't hang.
			// Browser dev (mock api): Tauri events never fire (no play-ready, no
			// play-ended) and the mock always returns id 0 — substitute the tab id so
			// concurrent mock tabs don't share a play id, and activate immediately
			// instead of sitting on Connecting for the full fallback window.
			const pid = isTauri ? info.id : tabId;
			this.#patch(tabId, {
				playId: pid,
				phase: isTauri ? 'connecting' : 'active',
				conn: info.transport === 'relay' ? 'relay' : 'direct',
				wsPort: info.ws_port,
				native: info.native,
				embedded: info.embedded,
				hostCodecs: info.host_codecs ?? [],
				hostEncoders: info.host_encoders ?? [],
				hostDisplays: info.host_displays ?? []
			});
			// Identity the host pushed before we knew this play id → cache it now.
			const early = this.#pendingIdentity.get(String(pid));
			if (early && isTauri) {
				this.#pendingIdentity.delete(String(pid));
				setPeerIdentity(target.id, early);
			}
			// Fallback only: native Windows sessions reveal on their own once the stream has
			// settled (≤ ~8 s after the first frame), so this must not cut that short.
			setTimeout(() => this.#activateByPlayId(pid), 14_000);
			recordConnection(
				target.id,
				target.name,
				useMode === 'game' ? 'console' : 'pc',
				useMode === 'game' ? 'game' : 'remote'
			);
		} catch (e) {
			this.connectErr = friendlyConnectError(e instanceof Error ? e.message : String(e));
			this.removeTab(tabId);
		} finally {
			// Clean up auth-pause state so a stale entry can't affect future connects.
			if (peerKey) {
				this.#authInProgress.delete(peerKey);
				this.#pendingTimeoutReject.delete(peerKey);
			}
			this.#deps.onAuthDone(target.id); // this connection's prompt (if any) is done
		}
	};

	/** Called when an auth prompt (password / Allow-Deny) becomes active for a peer.
	 * Pauses the connect timeout so human-interaction time doesn't count. */
	notifyAuthStart = (peerId: string) => {
		const key = peerId.replace(/\s/g, '');
		if (key) this.#authInProgress.add(key);
	};

	/** Called when the user submits a password (or the prompt is dismissed) for a peer.
	 * Resumes the timeout — granting POST_AUTH_TIMEOUT_MS for the post-auth handshake. */
	notifyAuthSubmit = (peerId: string) => {
		const key = peerId.replace(/\s/g, '');
		if (!key) return;
		this.#authInProgress.delete(key);
		// If the timeout fired while auth was paused, it parked itself here — re-arm
		// it now so the post-auth handshake is still bounded.
		const parked = this.#pendingTimeoutReject.get(key);
		if (parked) {
			this.#pendingTimeoutReject.delete(key);
			parked.arm(POST_AUTH_TIMEOUT_MS);
		}
	};

	removeTab = (tabId: number) => {
		this.sessions = this.sessions.filter((s) => s.tabId !== tabId);
		if (this.activeTab === tabId)
			this.activeTab = this.sessions.length ? this.sessions[this.sessions.length - 1].tabId : 'home';
		// Split mode: a removed session vacates its pane (back to an empty PaneConnect cell)
		// without changing the layout. The pane count change re-bumps the surviving cells'
		// resolution back up.
		if (this.splitMode !== 'off' && this.panes.includes(tabId)) {
			this.panes = this.panes.map((p) => (p === tabId ? null : p));
			this.#syncPaneCount();
			this.#applyPaneResolutions();
		}
		// A session of the PARKED split group left: the group loses it; one member left = no
		// group any more (that one is an ordinary tab).
		if (this.parked && this.parked.panes.includes(tabId)) {
			const panes = this.parked.panes.map((p) => (p === tabId ? null : p));
			this.parked = panes.filter((p) => p !== null).length >= 2 ? { ...this.parked, panes } : null;
		}
	};

	/** A tab that can move to another window (Chrome-style tear-off / merge): a live remote
	 * session. Not a connecting one (its connect is in flight here), not a local host-game tab
	 * (no stream), not the Linux single-surface mode (its video is part of this window). */
	canMove = (tabId: number): boolean => {
		const s = this.sessions.find((x) => x.tabId === tabId);
		// Connecting tabs move once the host answered (play id known, e.g. "preparing
		// stream"): what's left is the stream coming up, which every window hears.
		return !!s && s.playId >= 0 && !s.embedded;
	};

	// ── Split group (the split's sessions shown as ONE tab, like a browser tab group) ──────
	// The live split (`splitMode` / `panes`) can be PARKED while another tab is shown alone —
	// the group keeps its layout and comes back with one click (`showGroup`).
	parked = $state<{
		layout: 'h2' | 'v2' | 'grid4';
		panes: (number | null)[];
		focusedPane: number;
		sessionMode: 'game' | 'remote';
	} | null>(null);

	/** The split group, shown or parked: its layout, panes (tab ids) and whether it's on screen. */
	get group(): { layout: 'h2' | 'v2' | 'grid4'; panes: (number | null)[]; members: number[]; shown: boolean } | null {
		if (this.splitMode !== 'off') {
			const members = this.panes.filter((p): p is number => p !== null);
			return { layout: this.splitMode, panes: this.panes, members, shown: true };
		}
		if (this.parked) {
			const members = this.parked.panes.filter((p): p is number => p !== null);
			return { layout: this.parked.layout, panes: this.parked.panes, members, shown: false };
		}
		return null;
	}

	/** Is tab `tabId` a member of the split group (shown or parked)? */
	inGroup = (tabId: number): boolean => !!this.group?.members.includes(tabId);

	/** Put the shown split aside (another tab is shown alone); `showGroup` brings it back. */
	#park() {
		if (this.splitMode === 'off') return;
		this.parked = {
			layout: this.splitMode,
			panes: [...this.panes],
			focusedPane: this.focusedPane,
			sessionMode: this.splitSessionMode
		};
		this.splitMode = 'off';
		this.panes = [];
		this.focusedPane = 0;
		this.#syncPaneCount();
	}

	/** Show the split group again (its tab was clicked). */
	showGroup = () => {
		const p = this.parked;
		if (!p) return;
		this.parked = null;
		const panes = p.panes.map((t) => (t !== null && this.sessions.some((s) => s.tabId === t) ? t : null));
		this.splitSessionMode = p.sessionMode;
		this.panes = panes;
		this.splitMode = p.layout;
		this.#syncPaneCount();
		this.#applyPaneResolutions();
		this.focusPane(Math.min(p.focusedPane, panes.length - 1));
	};

	/** A tab of the strip was clicked: a group member focuses its pane (bringing the group
	 * back if parked); any other tab (or Home) is shown alone, the group parked. */
	select = (tab: 'home' | number) => {
		if (typeof tab === 'number' && this.inGroup(tab)) {
			if (this.splitMode === 'off') this.showGroup();
			const i = this.panes.indexOf(tab);
			if (i >= 0) this.focusPane(i);
			return;
		}
		this.#park();
		this.activeTab = tab;
	};

	/** Take tab `tabId` out of the split group (it becomes an ordinary tab). */
	ungroup = (tabId: number) => {
		if (this.panes.includes(tabId)) this.unpane(tabId);
		else if (this.parked?.panes.includes(tabId)) {
			const panes = this.parked.panes.map((p) => (p === tabId ? null : p));
			this.parked = panes.filter((p) => p !== null).length >= 2 ? { ...this.parked, panes } : null;
		}
	};

	/** Hand the whole split group to another window: its layout + sessions (pane order, by play
	 * id), dropped HERE without stopping anything. */
	exportGroup = (): GroupMove | null => {
		const g = this.group;
		if (!g) return null;
		const focusedPane = g.shown ? this.focusedPane : (this.parked?.focusedPane ?? 0);
		const sessionMode = g.shown ? this.splitSessionMode : (this.parked?.sessionMode ?? 'remote');
		const panes = g.panes.map((t) => (t === null ? null : (this.sessions.find((s) => s.tabId === t)?.playId ?? null)));
		this.splitMode = 'off';
		this.panes = [];
		this.parked = null;
		this.focusedPane = 0;
		this.#syncPaneCount();
		const sessions: Session[] = [];
		for (const t of g.members) {
			const rec = this.release(t);
			if (rec) sessions.push(rec);
		}
		return { group: { layout: g.layout, sessionMode, focusedPane, panes }, sessions };
	};

	/** A split group moved in from another window: its sessions become this window's split
	 * (or plain tabs when this window already has a group — one group per window). */
	adoptGroup = (m: GroupMove) => {
		const plays = m.sessions.map((s) => s.playId);
		this.adopt(m.sessions);
		if (this.group) return; // already has a split group: they stay ordinary tabs
		const panes = m.group.panes.map((pid) =>
			pid === null ? null : (this.sessions.find((s) => s.playId === pid && plays.includes(pid))?.tabId ?? null)
		);
		if (panes.filter((p) => p !== null).length < 2) return;
		this.splitSessionMode = m.group.sessionMode;
		this.panes = panes;
		this.splitMode = m.group.layout;
		this.#syncPaneCount();
		this.#applyPaneResolutions();
		this.focusPane(Math.min(m.group.focusedPane, panes.length - 1));
	};

	/** Hand a tab over to another window: its record (as that window adopts it) — and drop it
	 * HERE without stopping anything (the stream keeps running for the new window). */
	release = (tabId: number): Session | null => {
		const s = this.sessions.find((x) => x.tabId === tabId);
		if (!s) return null;
		this.removeTab(tabId);
		return $state.snapshot(s) as Session;
	};

	/** Tabs moved in from another window: add them (fresh tab ids here) and show the last. */
	adopt = (list: unknown[]) => {
		let last: number | null = null;
		for (const raw of list) {
			const s = raw as Session;
			if (!s || typeof s.playId !== 'number' || this.sessions.some((x) => x.playId === s.playId)) continue;
			this.#noteSessionStart();
			const tabId = this.#nextTab++;
			this.sessions = [...this.sessions, { ...s, tabId, adopted: true }];
			last = tabId;
			// Moved while still preparing: the source window's fallback reveal timer stayed
			// behind, so arm one here (a no-op once play-ready activated it).
			if (s.phase === 'connecting') {
				const pid = s.playId;
				// Its play-ready may already have gone out while this window was loading.
				if (isTauri)
					api.tabPlayReady(pid)
						.then((up) => up && this.#activateByPlayId(pid))
						.catch(() => {});
				setTimeout(() => this.#activateByPlayId(pid), 14_000);
			}
		}
		// Shown at once — over a shown split too (the group parks; its tab brings it back).
		if (last !== null) this.select(last);
	};

	/** A tab dragged onto the session area can split it there (live sessions only). */
	canSplitWith = (tabId: number): boolean => {
		const s = this.sessions.find((x) => x.tabId === tabId);
		if (!s || s.phase !== 'active' || s.playId < 0) return false;
		// One split group per window: with one parked, a new split would make a second.
		if (this.parked) return false;
		// Already split: it can go into a pane of the split's mode.
		return this.splitMode === 'off' || s.mode === this.splitSessionMode;
	};

	/** Split mode: show tab `tabId` in pane `i` (that pane's session goes back to being a
	 * background tab; if the tab sat in another pane, the two swap). */
	putInPane = (tabId: number, i: number) => {
		if (this.splitMode === 'off' || i < 0 || i >= this.panes.length || !this.canSplitWith(tabId)) return;
		const from = this.panes.indexOf(tabId);
		const next = [...this.panes];
		if (from >= 0) next[from] = next[i];
		next[i] = tabId;
		this.panes = next;
		this.#syncPaneCount();
		this.#applyPaneResolutions();
		this.focusPane(i);
	};

	/** Split mode: tab `tabId` was dragged back onto the tab strip — it leaves its pane. With
	 * one session (or none) left on screen the split ends (back to the single view). */
	unpane = (tabId: number) => {
		if (this.splitMode === 'off') return;
		const i = this.panes.indexOf(tabId);
		if (i < 0) return;
		this.panes = this.panes.map((p) => (p === tabId ? null : p));
		if (this.panes.filter((p) => p !== null).length <= 1) {
			const rest = this.panes.find((p) => p !== null);
			if (rest != null) this.focusedPane = this.panes.indexOf(rest);
			this.exitSplit();
			return;
		}
		this.#syncPaneCount();
		this.#applyPaneResolutions();
	};

	/** Drag-to-split (IntelliJ-style): tab `tabId` dropped on edge `zone` of the session area.
	 * The window splits in two — the dragged session on that side, the session that was on
	 * screen on the other (an empty connect pane when that was Home, the same tab, or a
	 * session of the other mode: a split is all gaming or all remote). */
	splitWithTab = (tabId: number, zone: 'left' | 'right' | 'top' | 'bottom') => {
		const s = this.sessions.find((x) => x.tabId === tabId);
		if (!s || !this.canSplitWith(tabId)) return;
		const cur = typeof this.activeTab === 'number' ? this.sessions.find((x) => x.tabId === this.activeTab) : undefined;
		// The other side: the session that was on screen — or, when that IS the dragged one (or
		// Home was showing), the most recent other live session of the same mode, so a split
		// made from two tabs always holds both.
		const fallback = this.sessions.filter((x) => x.tabId !== tabId && x.mode === s.mode && x.phase === 'active').at(-1);
		const other = cur && cur.tabId !== tabId && cur.mode === s.mode ? cur.tabId : (fallback?.tabId ?? null);
		const layout = zone === 'left' || zone === 'right' ? 'h2' : 'v2';
		const first = zone === 'left' || zone === 'top';
		this.splitSessionMode = s.mode;
		this.panes = first ? [tabId, other] : [other, tabId];
		this.splitMode = layout;
		this.#syncPaneCount();
		this.#applyPaneResolutions();
		this.focusPane(first ? 0 : 1);
	};

	/** Reorder: move tab `tabId` right before tab `before` (null = to the end). */
	moveTab = (tabId: number, before: number | null) => {
		const from = this.sessions.findIndex((x) => x.tabId === tabId);
		if (from < 0 || before === tabId) return;
		const next = [...this.sessions];
		const [s] = next.splice(from, 1);
		const at = before === null ? -1 : next.findIndex((x) => x.tabId === before);
		next.splice(at < 0 ? next.length : at, 0, s);
		this.sessions = next;
	};

	// Close a tab: stop its stream (host sees a disconnect) and drop it.
	/** Rename a session tab for this session only (empty name restores the default). */
	renameTab = (tabId: number, name: string) => {
		const label = name.trim();
		this.#patch(tabId, { label: label || undefined });
	};

	endSession = (tabId: number) => {
		const s = this.sessions.find((x) => x.tabId === tabId);
		if (s) {
			if (s.playId >= 0) api.stopStream(s.playId).catch(() => {});
			// Run only THIS tab's own stop command (set for local host-game tabs); remote
			// game tabs carry no stopCmd, so closing them never touches a local game.
			if (s.stopCmd) api.runCommand(s.stopCmd).catch(() => {});
		}
		this.removeTab(tabId);
		// LAST session closed: put the window back the way it was when the sessions started
		// (windowed or fullscreen), unless the user toggled fullscreen during them — then their
		// choice stays. Programmatic, so it never touches the saved game-mode preference. Closing
		// one pane/tab while other sessions are live (split couch co-op) changes nothing.
		if (this.sessions.length === 0 && this.#fsBefore !== null) {
			const want = this.#fsTouched ? this.fullscreen : this.#fsBefore;
			this.#fsBefore = null;
			if (want !== this.fullscreen) {
				this.fullscreen = want;
				setFullscreen(want).catch(() => {});
			}
		}
	};

	// ONE fullscreen toggle for the button, F11, and Ctrl+Shift+F11 — they must behave identically.
	// `dragFrom`: only from Chrome's title-bar drag-out. Guarded with Array.isArray because this
	// is also bound straight to `onclick` handlers, which pass a MouseEvent.
	toggleFullscreen = (dragFrom?: unknown) => {
		this.fullscreen = !this.fullscreen;
		if (this.sessions.length > 0) this.#fsTouched = true;
		const drag = !this.fullscreen && Array.isArray(dragFrom) ? (dragFrom as [number, number]) : undefined;
		setFullscreen(this.fullscreen, drag).catch(() => {});
		// Remember the choice for game mode so the app reopens fullscreen the way the user
		// left it (scoped to game mode — a fullscreen remote session never persists).
		if (this.#deps.getMode() === 'game') {
			ui.gamingFullscreen = this.fullscreen;
			saveUi();
		}
	};

	// Programmatic fullscreen (kiosk / CLI --connect start): NOT a user choice — not persisted and
	// not counted as "changed during the session", so the window still goes back afterwards.
	enterFullscreenAuto = () => {
		if (this.fullscreen) return;
		this.fullscreen = true;
		setFullscreen(true).catch(() => {});
	};

	// Host launches one of its own games — a local tab (no remote peer / video).
	startHostSession = (game: Game) => {
		this.#noteSessionStart();
		const tabId = this.#nextTab++;
		this.sessions = [
			...this.sessions,
			{
				tabId,
				playId: -1,
				phase: 'active',
				target: { name: game.title, id: t('host.local') },
				mode: 'game',
				conn: 'direct',
				wsPort: 0,
				stopCmd: game.cmdStop || undefined
			}
		];
		this.activeTab = tabId;
	};
}
