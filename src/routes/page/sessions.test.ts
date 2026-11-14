// @vitest-environment node
// (SessionManager is pure logic — no DOM needed; avoids jsdom ESM issues with
// Svelte runes and $state outside a component context.)
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';

// Stub the event registrations (onPlayReady / onPlayEnded) and the api surface
// before importing SessionManager so the constructor doesn't blow up in Node.
vi.mock('#lib/api.js', async () => {
	const { vi: _vi } = await import('vitest');
	// 3-monitor host (mirrors the real PC test host) so the same-host free-display search
	// has monitors to assign. Each call returns a distinct play id so concurrent same-host
	// sessions don't share one.
	let nextId = 1;
	const mockStartRemotePlay = _vi.fn().mockImplementation(() =>
		Promise.resolve({
			id: nextId++,
			transport: 'direct',
			ws_port: 0,
			local: false,
			native: false,
			embedded: false,
			host_codecs: [],
			host_encoders: [],
			host_displays: [
				{ idx: 0, name: 'DISPLAY1', width: 2560, height: 1440, primary: true },
				{ idx: 1, name: 'DISPLAY2', width: 1920, height: 1080, primary: false },
				{ idx: 2, name: 'DISPLAY3', width: 1920, height: 1080, primary: false }
			]
		})
	);
	return {
		isTauri: false,
		api: {
			startRemotePlay: mockStartRemotePlay,
			stopStream: _vi.fn().mockResolvedValue(undefined),
			runCommand: _vi.fn().mockResolvedValue(undefined),
			setPaneCount: _vi.fn().mockResolvedValue(undefined),
			setPlayResolution: _vi.fn().mockResolvedValue(undefined),
			setActiveSession: _vi.fn().mockResolvedValue(undefined)
		},
		onPlayReady: _vi.fn(),
		onPlayOptimizing: _vi.fn(),
		onPlayEnded: _vi.fn(),
		onSystemSleep: _vi.fn(),
		onSystemWake: _vi.fn(),
		onPlayError: _vi.fn(),
		onPlayStats: _vi.fn(),
		onPeerAvatar: _vi.fn(),
		onPeerName: _vi.fn(),
		onHostDisplays: _vi.fn(),
		setFullscreen: _vi.fn().mockResolvedValue(undefined),
		recordConnection: _vi.fn()
	};
});

// Stub recordConnection imported inside sessions.svelte.ts via #lib/peers.svelte.js
vi.mock('#lib/peers.svelte.js', () => ({ recordConnection: vi.fn(), setPeerIdentity: vi.fn() }));

// Stub i18n (t) to avoid loading JSON files in Node
vi.mock('#lib/i18n.svelte.js', () => ({ t: (k: string) => k }));

import { api } from '#lib/api.js';
import { SessionManager, RESUME_DELAYS_MS } from './sessions.svelte';
import { ui } from '#lib/settings.svelte.js';
import { PORTAL_PICK } from '#lib/api.types.js';

function makeManager(mode: 'remote' | 'game' = 'remote') {
	return new SessionManager({ getMode: () => mode, onAuthDone: () => {} });
}

const target = { name: 'PC', id: '111 222 333' };

describe('SessionManager.startConnect — forwardControllers / gameMode decoupling', () => {
	beforeEach(() => {
		vi.clearAllMocks();
		// Ensure the default setting values are in place
		ui.forwardControllers = true;
		ui.touchpadAsMouse = true;
	});

	it('remote mode: 6th arg = ui.forwardControllers (true), 7th arg = false', async () => {
		const mgr = makeManager('remote');
		await mgr.startConnect(target, 'remote');
		expect(api.startRemotePlay).toHaveBeenCalledOnce();
		const args: unknown[] = (api.startRemotePlay as ReturnType<typeof vi.fn>).mock.calls[0];
		// 6th arg (index 5) = gamepad = ui.forwardControllers
		expect(args[5]).toBe(true);
		// 7th arg (index 6) = gameMode
		expect(args[6]).toBe(false);
	});

	it('game mode: 6th arg = ui.forwardControllers (true), 7th arg = true', async () => {
		const mgr = makeManager('game');
		await mgr.startConnect(target, 'game');
		expect(api.startRemotePlay).toHaveBeenCalledOnce();
		const args: unknown[] = (api.startRemotePlay as ReturnType<typeof vi.fn>).mock.calls[0];
		expect(args[5]).toBe(true);
		expect(args[6]).toBe(true);
	});

	it('remote mode with forwardControllers=false: 6th arg = false, 7th arg = false', async () => {
		ui.forwardControllers = false;
		const mgr = makeManager('remote');
		await mgr.startConnect(target, 'remote');
		const args: unknown[] = (api.startRemotePlay as ReturnType<typeof vi.fn>).mock.calls[0];
		expect(args[5]).toBe(false);
		expect(args[6]).toBe(false);
	});

	it('game mode with forwardControllers=false: gamepad off but gameMode still true', async () => {
		ui.forwardControllers = false;
		const mgr = makeManager('game');
		await mgr.startConnect(target, 'game');
		const args: unknown[] = (api.startRemotePlay as ReturnType<typeof vi.fn>).mock.calls[0];
		// gamepad (arg 5) reflects the setting, NOT the mode
		expect(args[5]).toBe(false);
		// gameMode (arg 6) is still true — mode and gamepad are decoupled
		expect(args[6]).toBe(true);
	});
});

describe('SessionManager — same-host split: per-pane host-display selection (Phase 2a)', () => {
	beforeEach(() => {
		vi.clearAllMocks();
		ui.forwardControllers = true;
		ui.touchpadAsMouse = true;
	});

	// The displayIdx is the 10th positional arg (index 9) of startRemotePlay.
	const displayArg = (call: number) =>
		(api.startRemotePlay as ReturnType<typeof vi.fn>).mock.calls[call][9];

	it('a lone pane keeps single-session behavior: displayIdx undefined (host default 0)', async () => {
		const mgr = makeManager('remote');
		mgr.enterSplit('h2');
		await mgr.connectIntoPane(0, target, 'remote');
		expect(api.startRemotePlay).toHaveBeenCalledOnce();
		expect(displayArg(0)).toBeUndefined();
		expect(mgr.sessions).toHaveLength(1);
	});

	it('second same-host pane auto-assigns the first FREE monitor (idx 1)', async () => {
		const mgr = makeManager('remote');
		mgr.enterSplit('h2');
		await mgr.connectIntoPane(0, target, 'remote');
		await mgr.connectIntoPane(1, target, 'remote');
		// Two real sessions to the same host (not collapsed).
		expect(mgr.sessions).toHaveLength(2);
		// First pane → host default (undefined → 0); second → first free advertised idx (1).
		expect(displayArg(0)).toBeUndefined();
		expect(displayArg(1)).toBe(1);
	});

	it('two "Masaüstü" (gameId="") game panes to one host do NOT collapse into one session', async () => {
		const mgr = makeManager('game');
		mgr.enterSplit('h2', 'game');
		await mgr.connectIntoPane(0, target, 'game', '');
		await mgr.connectIntoPane(1, target, 'game', '');
		expect(mgr.sessions).toHaveLength(2);
		expect(mgr.sessions.every((s) => s.mode === 'game')).toBe(true);
		expect(displayArg(1)).toBe(1);
	});

	it('a third same-host pane skips both used monitors → idx 2', async () => {
		const mgr = makeManager('remote');
		mgr.enterSplit('grid4');
		await mgr.connectIntoPane(0, target, 'remote');
		await mgr.connectIntoPane(1, target, 'remote');
		await mgr.connectIntoPane(2, target, 'remote');
		expect(mgr.sessions).toHaveLength(3);
		expect(displayArg(2)).toBe(2);
	});

	it('a pane with NO free monitor left still opens its own session (same screen twice)', async () => {
		const mgr = makeManager('game');
		mgr.enterSplit('grid4', 'game');
		for (let i = 0; i < 4; i++) await mgr.connectIntoPane(i, target, 'game', '');
		// The host advertises 3 monitors: the 4th pane mirrors the default one instead of
		// collapsing onto pane 1's session (which left its cell gray).
		expect(mgr.sessions).toHaveLength(4);
		expect(displayArg(3)).toBeUndefined();
		expect(new Set(mgr.panes).size).toBe(4);
	});

	it('paneUsageFor tags the window / screen each pane streams (1-based)', async () => {
		const mgr = makeManager('game');
		mgr.enterSplit('grid4', 'game');
		await mgr.connectIntoPane(0, target, 'game', '');
		await mgr.connectIntoPane(1, target, 'game', '', undefined, 4242);
		await mgr.connectIntoPane(2, target, 'game', 'steam');
		const u = mgr.paneUsageFor(target.id);
		expect(u.displays).toEqual({ 0: 1 });
		expect(u.windows).toEqual({ 4242: 2 });
	});

	it('paneUsageFor never marks the Wayland portal entry as used', async () => {
		const mgr = makeManager('game');
		mgr.enterSplit('grid4', 'game');
		await mgr.connectIntoPane(0, target, 'game', '', undefined, PORTAL_PICK);
		await mgr.connectIntoPane(1, target, 'game', '', undefined, 4242);
		const u = mgr.paneUsageFor(target.id);
		expect(u.windows).toEqual({ 4242: 2 });
		expect(u.windows[PORTAL_PICK]).toBeUndefined();
		// A portal pick is a window session, never a screen one.
		expect(u.displays).toEqual({});
	});

	it('an explicit picker choice (displayIdx) is passed through and bypasses de-dup', async () => {
		const mgr = makeManager('remote');
		mgr.enterSplit('h2');
		await mgr.connectIntoPane(0, target, 'remote');
		// User picks monitor 2 explicitly for the second pane.
		await mgr.connectIntoPane(1, target, 'remote', '', 2);
		expect(mgr.sessions).toHaveLength(2);
		expect(displayArg(1)).toBe(2);
	});

	it('different hosts are independent: each first pane uses the host default', async () => {
		const mgr = makeManager('remote');
		mgr.enterSplit('h2');
		await mgr.connectIntoPane(0, target, 'remote');
		await mgr.connectIntoPane(1, { name: 'PC2', id: '444 555 666' }, 'remote');
		expect(mgr.sessions).toHaveLength(2);
		expect(displayArg(0)).toBeUndefined();
		expect(displayArg(1)).toBeUndefined();
	});

	it('connectIntoPane FORCES the split mode — a stray per-pane mode arg is ignored', async () => {
		const mgr = makeManager('remote');
		// Split entered as GAME — every pane must be a game session even if a caller passes
		// 'remote' (no mixed gaming+remote split).
		mgr.enterSplit('h2', 'game');
		await mgr.connectIntoPane(0, target, 'remote');
		await mgr.connectIntoPane(1, target, 'remote');
		expect(mgr.splitSessionMode).toBe('game');
		expect(mgr.sessions.every((s) => s.mode === 'game')).toBe(true);
	});

	it('setLayout preserves the established split mode across a reshape (2→4)', async () => {
		const mgr = makeManager('remote');
		mgr.enterSplit('h2', 'game');
		mgr.setLayout('grid4'); // omit mode — must keep 'game', not fall back to ui.appMode
		expect(mgr.splitSessionMode).toBe('game');
	});

	it('helpers report a host\'s known displays + claimed indices', async () => {
		const mgr = makeManager('remote');
		mgr.enterSplit('h2');
		await mgr.connectIntoPane(0, target, 'remote');
		await mgr.connectIntoPane(1, target, 'remote');
		expect(mgr.hostDisplaysFor(target.id)).toHaveLength(3);
		// pane 0 claimed 0 (default), pane 1 claimed 1.
		expect([...mgr.usedDisplaysFor(target.id)].sort()).toEqual([0, 1]);
		// Next free is 2.
		expect(mgr.freeDisplayFor(target.id)).toBe(2);
	});
});

describe('SessionManager — window fullscreen after the last session ends', () => {
	beforeEach(() => vi.clearAllMocks());

	it('fullscreen before the session → stays fullscreen after it ends', async () => {
		const mgr = makeManager('game');
		mgr.toggleFullscreen();
		await mgr.startConnect(target, 'game');
		mgr.endSession(mgr.sessions[0].tabId);
		expect(mgr.fullscreen).toBe(true);
	});

	it('windowed before, auto-fullscreen during → back to windowed', async () => {
		const mgr = makeManager('remote');
		await mgr.startConnect(target, 'remote');
		mgr.enterFullscreenAuto();
		mgr.endSession(mgr.sessions[0].tabId);
		expect(mgr.fullscreen).toBe(false);
	});

	it('user toggled during the session → their choice stays', async () => {
		const mgr = makeManager('remote');
		await mgr.startConnect(target, 'remote');
		mgr.toggleFullscreen();
		mgr.endSession(mgr.sessions[0].tabId);
		expect(mgr.fullscreen).toBe(true);
	});
});

describe('sleep ends the viewed sessions, wake-up connects them again', () => {
	// These tests replace the connect mock's behaviour; the suites after them need it back.
	const connect = vi.mocked(api.startRemotePlay).getMockImplementation()!;
	beforeEach(() => {
		vi.clearAllMocks();
		vi.useFakeTimers();
		ui.duplicateSessions = false;
	});
	afterEach(() => {
		vi.mocked(api.startRemotePlay).mockReset();
		vi.mocked(api.startRemotePlay).mockImplementation(connect);
		vi.useRealTimers();
	});
	const settle = async (ms: number) => {
		await vi.advanceTimersByTimeAsync(ms);
	};

	it('ends every remote session at sleep and brings it back with its mode and name', async () => {
		const sm = makeManager();
		await sm.startConnect(target);
		await sm.startConnect({ name: 'Console', id: '444 555 666' }, 'game', 'steam');
		sm.renameTab(sm.sessions[0].tabId, 'Work PC');
		sm.startHostSession({ title: 'Local game', cmdStop: 'stop' } as never);
		expect(sm.sessions).toHaveLength(3);

		sm.systemSleep();
		expect(vi.mocked(api.stopStream)).toHaveBeenCalledTimes(2);
		// The local host-game tab is not a viewed session: it stays, and its game keeps running.
		expect(sm.sessions.map((s) => s.target.name)).toEqual(['Local game']);
		expect(vi.mocked(api.runCommand)).not.toHaveBeenCalled();

		vi.mocked(api.startRemotePlay).mockClear();
		const wake = sm.systemWake();
		await settle(RESUME_DELAYS_MS[0] - 1);
		expect(vi.mocked(api.startRemotePlay)).not.toHaveBeenCalled(); // the network needs a moment
		await settle(1);
		await wake;
		expect(vi.mocked(api.startRemotePlay)).toHaveBeenCalledTimes(2);
		const back = sm.sessions.filter((s) => s.target.name !== 'Local game');
		expect(back.map((s) => [s.target.id, s.mode, s.gameId, s.label])).toEqual([
			['111 222 333', 'remote', undefined, 'Work PC'],
			['444 555 666', 'game', 'steam', undefined]
		]);
	});

	it('tries again while the network is not back, and gives up after the last round', async () => {
		const sm = makeManager();
		await sm.startConnect(target);
		sm.systemSleep();
		vi.mocked(api.startRemotePlay).mockClear();
		vi.mocked(api.startRemotePlay).mockRejectedValueOnce(new Error('peer offline'));
		const wake = sm.systemWake();
		await settle(RESUME_DELAYS_MS[0]);
		expect(sm.sessions).toHaveLength(0); // first try failed
		await settle(RESUME_DELAYS_MS[1]);
		await wake;
		expect(vi.mocked(api.startRemotePlay)).toHaveBeenCalledTimes(2);
		expect(sm.sessions).toHaveLength(1);

		// Never reachable again: every round tries once, then it stops asking.
		sm.systemSleep();
		vi.mocked(api.startRemotePlay).mockClear();
		vi.mocked(api.startRemotePlay).mockRejectedValue(new Error('peer offline'));
		const wake2 = sm.systemWake();
		await settle(RESUME_DELAYS_MS.reduce((a, b) => a + b, 0));
		await wake2;
		expect(vi.mocked(api.startRemotePlay)).toHaveBeenCalledTimes(RESUME_DELAYS_MS.length);
		await sm.systemWake(); // nothing remembered any more
		expect(vi.mocked(api.startRemotePlay)).toHaveBeenCalledTimes(RESUME_DELAYS_MS.length);
	});

	it('a lid that closes again before the reconnect keeps the session remembered', async () => {
		const sm = makeManager();
		vi.mocked(api.startRemotePlay).mockImplementation(() =>
			Promise.resolve({ id: 7, transport: 'direct', ws_port: 0, local: false, native: false, embedded: false, host_codecs: [], host_encoders: [], host_displays: [] } as never)
		);
		await sm.startConnect(target);
		sm.systemSleep();
		vi.mocked(api.startRemotePlay).mockClear();
		const first = sm.systemWake(); // lid opened …
		await settle(1000);
		sm.systemSleep(); // … and closed again a second later
		await settle(RESUME_DELAYS_MS[0]);
		await first;
		expect(vi.mocked(api.startRemotePlay)).not.toHaveBeenCalled();
		const second = sm.systemWake();
		await settle(RESUME_DELAYS_MS[0]);
		await second;
		expect(vi.mocked(api.startRemotePlay)).toHaveBeenCalledTimes(1);
		expect(sm.sessions).toHaveLength(1);
	});
});

describe('detachable tabs (release / adopt / reorder)', () => {
	beforeEach(() => {
		vi.mocked(api.stopStream).mockClear();
	});

	it('release hands the record over without stopping the stream', async () => {
		const mgr = makeManager('remote');
		await mgr.startConnect(target, 'remote');
		const s = mgr.sessions[0];
		const rec = mgr.release(s.tabId);
		expect(rec?.playId).toBe(s.playId);
		expect(mgr.sessions).toHaveLength(0);
		expect(mgr.activeTab).toBe('home');
		expect(api.stopStream).not.toHaveBeenCalled();
	});

	it('adopt adds the tabs as adopted, shows the last, skips duplicates', async () => {
		const src = makeManager('remote');
		await src.startConnect(target, 'remote');
		await src.startConnect({ name: 'B', id: '222 222 222' }, 'remote');
		const a = src.release(src.sessions[0].tabId)!;
		const b = src.release(src.sessions[0].tabId)!;
		const dst = makeManager('remote');
		dst.adopt([a, b, a]);
		expect(dst.sessions.map((x) => x.playId)).toEqual([a.playId, b.playId]);
		expect(dst.sessions.every((x) => x.adopted)).toBe(true);
		expect(dst.activeTab).toBe(dst.sessions[1].tabId);
	});

	it('only a live session can move', async () => {
		const mgr = makeManager('remote');
		await mgr.startConnect(target, 'remote');
		const s = mgr.sessions[0];
		// Mock api activates at once (not Tauri) — live.
		expect(mgr.canMove(s.tabId)).toBe(s.phase === 'active');
		// A split member can leave too (it is taken out of the group first).
		mgr.enterSplit('h2');
		expect(mgr.canMove(s.tabId)).toBe(true);
	});

	it('moveTab reorders the strip', async () => {
		const mgr = makeManager('remote');
		await mgr.startConnect(target, 'remote');
		await mgr.startConnect({ name: 'B', id: '222 222 222' }, 'remote');
		await mgr.startConnect({ name: 'C', id: '333 333 333' }, 'remote');
		const [a, b, c] = mgr.sessions.map((x) => x.tabId);
		mgr.moveTab(a, null);
		expect(mgr.sessions.map((x) => x.tabId)).toEqual([b, c, a]);
		mgr.moveTab(a, b);
		expect(mgr.sessions.map((x) => x.tabId)).toEqual([a, b, c]);
	});
});

describe('drag-to-split', () => {
	it('dropping a tab on the right edge splits left/right with the shown session', async () => {
		const mgr = makeManager('remote');
		await mgr.startConnect(target, 'remote');
		await mgr.startConnect({ name: 'B', id: '222 222 222' }, 'remote');
		const [a, b] = mgr.sessions.map((x) => x.tabId);
		mgr.activeTab = a;
		mgr.splitWithTab(b, 'right');
		expect(mgr.splitMode).toBe('h2');
		expect(mgr.panes).toEqual([a, b]);
		expect(mgr.focusedPane).toBe(1);
	});

	it('top edge splits top/bottom; the other pane is empty when Home was shown', async () => {
		const mgr = makeManager('remote');
		await mgr.startConnect(target, 'remote');
		const [a] = mgr.sessions.map((x) => x.tabId);
		mgr.activeTab = 'home';
		mgr.splitWithTab(a, 'top');
		expect(mgr.splitMode).toBe('v2');
		expect(mgr.panes).toEqual([a, null]);
	});

	it('never mixes gaming and remote sessions in one split', async () => {
		const mgr = makeManager('remote');
		await mgr.startConnect(target, 'remote');
		await mgr.startConnect(target, 'game');
		const [r, g] = mgr.sessions.map((x) => x.tabId);
		mgr.activeTab = r;
		mgr.splitWithTab(g, 'left');
		expect(mgr.splitSessionMode).toBe('game');
		expect(mgr.panes).toEqual([g, null]);
	});
});

describe('split: put back / swap', () => {
	it('dragging a paned tab back onto the strip ends a 2-pane split', async () => {
		const mgr = makeManager('remote');
		await mgr.startConnect(target, 'remote');
		await mgr.startConnect({ name: 'B', id: '222 222 222' }, 'remote');
		const [a, b] = mgr.sessions.map((x) => x.tabId);
		mgr.activeTab = a;
		mgr.splitWithTab(b, 'right');
		mgr.unpane(b);
		expect(mgr.splitMode).toBe('off');
		expect(mgr.activeTab).toBe(a);
		expect(mgr.sessions).toHaveLength(2);
	});

	it('putInPane swaps two paned tabs', async () => {
		const mgr = makeManager('remote');
		await mgr.startConnect(target, 'remote');
		await mgr.startConnect({ name: 'B', id: '222 222 222' }, 'remote');
		const [a, b] = mgr.sessions.map((x) => x.tabId);
		mgr.activeTab = a;
		mgr.splitWithTab(b, 'right');
		mgr.putInPane(b, 0);
		expect(mgr.panes).toEqual([b, a]);
	});
});

describe('split group tab', () => {
	async function twoInSplit() {
		const mgr = makeManager('remote');
		await mgr.startConnect(target, 'remote');
		await mgr.startConnect({ name: 'B', id: '222 222 222' }, 'remote');
		await mgr.startConnect({ name: 'C', id: '333 333 333' }, 'remote');
		const [a, b, c] = mgr.sessions.map((x) => x.tabId);
		mgr.activeTab = a;
		mgr.splitWithTab(b, 'right');
		return { mgr, a, b, c };
	}

	it('clicking another tab parks the group; clicking a member brings it back', async () => {
		const { mgr, a, b, c } = await twoInSplit();
		mgr.select(c);
		expect(mgr.splitMode).toBe('off');
		expect(mgr.activeTab).toBe(c);
		expect(mgr.group?.shown).toBe(false);
		expect(mgr.group?.members).toEqual([a, b]);
		mgr.select(b);
		expect(mgr.splitMode).toBe('h2');
		expect(mgr.panes).toEqual([a, b]);
		expect(mgr.focusedPane).toBe(1);
	});

	it('a parked group with one member left dissolves', async () => {
		const { mgr, a, c } = await twoInSplit();
		mgr.select(c);
		mgr.ungroup(a);
		expect(mgr.group).toBe(null);
	});

	it('export + adopt moves the group with its layout', async () => {
		const { mgr } = await twoInSplit();
		const m = mgr.exportGroup()!;
		expect(m.group.layout).toBe('h2');
		expect(m.sessions).toHaveLength(2);
		expect(mgr.splitMode).toBe('off');
		expect(mgr.sessions).toHaveLength(1);
		const dst = makeManager('remote');
		dst.adoptGroup(m);
		expect(dst.splitMode).toBe('h2');
		expect(dst.panes.map((t) => dst.sessions.find((s) => s.tabId === t)?.playId)).toEqual(m.group.panes);
	});
});

describe('drag-to-split picks the other side', () => {
	it('dragging the shown tab itself still pairs it with another session', async () => {
		const mgr = makeManager('remote');
		await mgr.startConnect(target, 'remote');
		await mgr.startConnect({ name: 'B', id: '222 222 222' }, 'remote');
		const [a, b] = mgr.sessions.map((x) => x.tabId);
		mgr.activeTab = b;
		mgr.splitWithTab(b, 'left');
		expect(mgr.panes).toEqual([b, a]);
	});
});

describe('several sessions to one device (setting)', () => {
	it('off (default): a repeat connect switches to the open session', async () => {
		ui.duplicateSessions = false;
		const mgr = makeManager('remote');
		await mgr.startConnect(target, 'remote');
		await mgr.startConnect(target, 'remote');
		expect(mgr.sessions).toHaveLength(1);
	});
	it('on: a repeat connect opens a second session', async () => {
		ui.duplicateSessions = true;
		const mgr = makeManager('remote');
		await mgr.startConnect(target, 'remote');
		await mgr.startConnect(target, 'remote');
		expect(mgr.sessions).toHaveLength(2);
		ui.duplicateSessions = false;
	});
});

describe('entering split uses the open tabs', () => {
	it('fills the other panes with the open sessions of that mode', async () => {
		const mgr = makeManager('remote');
		await mgr.startConnect(target, 'remote');
		await mgr.startConnect({ name: 'B', id: '222 222 222' }, 'remote');
		await mgr.startConnect({ name: 'C', id: '333 333 333' }, 'remote');
		const [a, b, c] = mgr.sessions.map((x) => x.tabId);
		mgr.activeTab = a;
		mgr.enterSplit('grid4', 'remote');
		expect(mgr.panes).toEqual([a, c, b, null]);
	});
});
