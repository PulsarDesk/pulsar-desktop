<script lang="ts">
	// The REAL gaming connect flow rendered INSIDE a split pane. It reuses the exact same
	// screens the full-screen gaming shell (GamingShell) uses — `GamingHome` (pick a host)
	// → `GamesScreen` (pick a game / Desktop) — just scoped to one pane, and emits the final
	// pick into THAT pane via `onConnect(index, target, 'game', gameId)` (→ connectIntoPane).
	//
	// Each pane gets its OWN GamepadNav instance (not the gaming shell's singleton) so two
	// panes' connect screens never fight over a single roving focus. The pane's nav is driven
	// by the shared gilrs→webview bridge (api.onGamepadNav) only while this pane is FOCUSED,
	// so controller nav targets the focused pane and stays inert in the others.
	import { onMount, onDestroy } from 'svelte';
	import GamingHome from './GamingHome.svelte';
	import GamesScreen from './GamesScreen.svelte';
	import { GamepadNav } from '#lib/gamepadNav.svelte.js';
	import { api, onGamepadNav, type GameInfo } from '#lib/api.js';
	import type { HostDisplay, WindowInfo } from '#lib/api.types.js';
	import { t } from '#lib/i18n.svelte.js';

	type Target = { name: string; id: string };
	type Props = {
		/** Which pane this connect fills. */
		index: number;
		/** Whether this pane is the focused one — gates the controller-nav bridge so only the
		 * focused pane's connect screen reacts to the pad. */
		focused?: boolean;
		/** Connect into THIS pane (runs startConnect + assigns the tabId). The mode arg is
		 * advisory — connectIntoPane FORCES the split's fixed personality (game). */
		onConnect: (
			i: number,
			target: Target,
			mode: 'remote' | 'game',
			gameId?: string,
			displayIdx?: number,
			windowHwnd?: number
		) => void;
		/** What the OTHER panes already stream from host `id` (hwnd / monitor idx → 1-based
		 * pane number), so the picker can tag those sources. */
		usageFor?: (id: string) => { windows: Record<number, number>; displays: Record<number, number> };
		/** The games-fetch authenticates against the host (the host may prompt) but is NOT a
		 * session, so the password modal otherwise stays up after the host accepts. Signal the
		 * parent when the fetch SUCCEEDS so it can dismiss the (now-stale) prompt for this host. */
		onFetched?: (id: string) => void;
	};
	let { index, focused = false, onConnect, onFetched, usageFor }: Props = $props();

	// This pane's own roving-focus controller (one per pane — never the shell singleton).
	const nav = new GamepadNav();
	const navItem = nav.item;

	// Two-step flow (identical to GamingShell.openGames/playPick/backHome): home → games.
	let step = $state<'home' | 'games'>('home');
	let gamesTarget = $state('');
	// Display name of the picked host (LAN / recents card), for the games-screen header.
	let gamesTargetName = $state('');
	let hostGames = $state<GameInfo[] | null>(null);
	let gamesLoading = $state(false);
	let gamesErr = $state('');
	let desktopImg = $state('');
	let hostWindows = $state<WindowInfo[]>([]);
	let hostDisplays = $state<HostDisplay[]>([]);
	const usage = $derived(
		gamesTarget && usageFor ? usageFor(gamesTarget) : { windows: {}, displays: {} }
	);

	async function openGames(id: string, name = '') {
		gamesTarget = id;
		gamesTargetName = name;
		hostGames = null;
		desktopImg = '';
		gamesErr = '';
		gamesLoading = true;
		step = 'games';
		hostWindows = [];
		hostDisplays = [];
		try {
			// Games + open windows + monitors in ONE authenticated round trip (no live session
			// to this host needed, so the first pane can pick a window too).
			const src = await api.listRemoteSources(id);
			if (gamesTarget !== id) return; // the user went back / picked another host meanwhile
			const fetched = src.games;
			hostWindows = src.windows;
			hostDisplays = src.displays;
			// Auth succeeded (host accepted / OTP matched) — dismiss any still-open password
			// prompt for this host (the fetch isn't a session, so onAuthDone never fired).
			onFetched?.(id);
			// The host always publishes a built-in "desktop" entry (with a live screenshot).
			desktopImg = fetched.find((g) => g.id === 'desktop')?.image ?? '';
			hostGames = fetched.filter((g) => g.id !== 'desktop');
		} catch (e) {
			gamesErr = e instanceof Error ? e.message : String(e);
		} finally {
			gamesLoading = false;
		}
	}
	function backHome() {
		step = 'home';
		hostGames = null;
		gamesErr = '';
		gamesTarget = '';
		hostWindows = [];
		hostDisplays = [];
	}
	function playDisplay(idx: number) {
		onConnect(index, { name: t('gaming.screenN', { n: idx + 1 }), id: gamesTarget }, 'game', '', idx);
		backHome();
	}
	function playWindow(w: WindowInfo) {
		onConnect(index, { name: w.title, id: gamesTarget }, 'game', '', undefined, w.hwnd);
		backHome();
	}
	function playPick(gameId: string) {
		const name = gameId
			? (hostGames?.find((g) => g.id === gameId)?.title ?? gameId)
			: t('gaming.desktop');
		onConnect(index, { name, id: gamesTarget }, 'game', gameId);
		backHome();
	}

	// B / Escape from the games step returns to the host pick (mirrors the shell).
	nav.setOpts({
		onBack: () => {
			if (step !== 'home') backHome();
		}
	});

	// Drive this pane's nav off the shared gilrs→webview bridge, but only while this pane is
	// focused — so the pad moves focus in the focused pane and leaves the others alone.
	let unlistenNav: (() => void) | null = null;
	let dead = false;
	onMount(() => {
		onGamepadNav((s) => {
			if (focused) nav.ingestBridge(s);
		}).then((off) => {
			if (dead) off();
			else unlistenNav = off;
		});
	});
	onDestroy(() => {
		dead = true;
		nav.stop();
		unlistenNav?.();
	});

	// Start/stop the pad nav with focus (keyboard arrows + bridge), so an unfocused pane's
	// connect screen never grabs focus.
	$effect(() => {
		if (focused) nav.start();
		else nav.stop();
	});
</script>

<div class="pane-game">
	{#if step === 'games'}
		<GamesScreen
			{nav}
			target={gamesTarget}
				targetName={gamesTargetName}
			games={hostGames}
			{desktopImg}
			loading={gamesLoading}
			err={gamesErr}
			onPlay={playPick}
			onBack={backHome}
			windows={hostWindows}
			onPlayWindow={playWindow}
			displays={hostDisplays}
			onPlayDisplay={playDisplay}
			usedWindows={usage.windows}
			usedDisplays={usage.displays}
		/>
	{:else}
		<GamingHome {nav} onPickHost={openGames} />
	{/if}
</div>

<style>
	.pane-game {
		flex: 1;
		min-width: 0;
		min-height: 0;
		display: flex;
		flex-direction: column;
	}
</style>
