<script lang="ts">
	// Gaming-mode host games — a full IN-STAGE screen (NOT a popup). Shown after picking a
	// host on the home: a connecting/loading state while the host's library is fetched, then
	// the games as big cards (Desktop pinned first). Back (button / B / Esc) returns home.
	import { tick } from 'svelte';
	import Icon from '#lib/Icon.svelte';
	import { type GameInfo } from '#lib/api.js';
	import type { HostDisplay, WindowInfo } from '#lib/api.types.js';
	import { t } from '#lib/i18n.svelte.js';
	import { allPeers, fmtPeerId, normalizeId } from '#lib/peers.svelte.js';
	import type { GamepadNav } from '#lib/gamepadNav.svelte.js';

	type Props = {
		nav: GamepadNav;
		/** Host id being connected to (for the header). */
		target: string;
		/** Host display name from the picked card (LAN / recents); falls back to the peer store. */
		targetName?: string;
		/** Fetched games, or null while loading / on error. */
		games: GameInfo[] | null;
		/** Live host-desktop thumbnail (data URL) for the pinned Desktop card, if any. */
		desktopImg?: string;
		loading: boolean;
		err: string;
		/** Launch: '' = whole Desktop, else the game id. */
		onPlay: (gameId: string) => void;
		onBack: () => void;
		/** Split pane: the host's open app windows. Picking one streams ONLY that window. */
		windows?: WindowInfo[];
		onPlayWindow?: (w: WindowInfo) => void;
		/** Split pane: the host's monitors. When given, the pane layout is used — Games, Open
		 * windows, Screens — instead of the single pinned Desktop card. */
		displays?: HostDisplay[];
		onPlayDisplay?: (idx: number) => void;
		/** Sources another pane already streams: hwnd / monitor idx → 1-based pane number. */
		usedWindows?: Record<number, number>;
		usedDisplays?: Record<number, number>;
	};
	let {
		nav,
		target,
		targetName = '',
		games,
		desktopImg = '',
		loading,
		err,
		onPlay,
		onBack,
		windows = [],
		onPlayWindow,
		displays,
		onPlayDisplay,
		usedWindows = {},
		usedDisplays = {}
	}: Props = $props();
	// Header identity: name + icon of the host, like its home card. Name: the picked card, else
	// the stored peer (game/remote recents, devices, pushed identity) — never just the bare id.
	const peer = $derived(allPeers().find((p) => p.id === normalizeId(target)));
	const hostName = $derived.by(() => {
		const n = (targetName || peer?.name || '').trim();
		return n && normalizeId(n) !== normalizeId(target) ? n : '';
	});
	const hostImg = $derived(peer?.image && !peer.image.startsWith('icon:') ? peer.image : peer?.avatar);
	const hostIcon = $derived(peer?.image?.startsWith('icon:') ? peer.image.slice(5) : "");
	const paneMode = $derived(displays !== undefined);
	const navItem = nav.item;

	// Cover image for a fetched game: the host-sent cover, else the bundled Steam logo for
	// the Steam default, else none (the card shows initials).
	function coverFor(g: GameInfo): string {
		if (g.image) return g.image;
		if (g.id === 'steam' || g.title.toLowerCase() === 'steam') return '/steam.svg';
		return '';
	}

	// The first game card (the always-present "Desktop" card). Once the list has loaded
	// we select it by default so controller nav starts ON a game — not parked on the Back
	// button (which registers first) or the bottom dock. Without this the user had to
	// d-pad up from the bottom to reach the first game.
	let firstCard = $state<HTMLButtonElement>();
	let didAutofocus = false;
	$effect(() => {
		// Track loading/err/games so this re-runs as the fetch resolves.
		const ready = !loading && !err;
		void games;
		if (loading) {
			didAutofocus = false; // re-arm for the next fetch
			return;
		}
		if (!ready || didAutofocus) return;
		didAutofocus = true;
		// Wait for the cards to mount + lay out, then focus the first one. nav.focus()
		// validates registration + visibility, so a not-yet-painted node is a no-op.
		tick().then(() => {
			if (firstCard) nav.focus(firstCard);
		});
	});

	function initials(name: string) {
		return name.split(' ').map((w) => w[0]).slice(0, 2).join('').toUpperCase();
	}
</script>

<div class="gscreen">
	<div class="ghead">
		<button class="back" use:navItem onclick={onBack} aria-label={t('gaming.back')}>
			<Icon name="arrowRight" size={16} class="flip" />
		</button>
		<span class="havatar">
			{#if hostImg}<img src={hostImg} alt="" />{:else if hostIcon}<Icon name={hostIcon} size={18} />{:else if hostName}{initials(hostName)}{:else}<Icon name="monitor" size={18} />{/if}
		</span>
		<span class="hmeta">
			{#if hostName}
				<span class="htitle selectable">{hostName}</span>
				<span class="hid mono selectable">{fmtPeerId(target)}</span>
			{:else}
				<span class="htitle mono selectable">{fmtPeerId(target)}</span>
			{/if}
		</span>
	</div>

	{#if loading}
		<div class="state">
			<span class="spinner"></span>
			<span>{t('home.fetching')}</span>
		</div>
	{:else if err}
		<div class="state err selectable">{err}</div>
		<button class="btn" use:navItem onclick={onBack}>{t('gaming.back')}</button>
	{:else if paneMode}
		<!-- Split pane: one picker, three kinds of source. A source another pane already shows
		     carries an "in pane N" tag (still pickable: the same screen twice is allowed). -->
		<div class="games">
			<div class="section mono">{t('gaming.sectionGames')}</div>
			{#if games && games.length}
				{#each games as g, gi (g.id)}
					{@const cover = coverFor(g)}
					{#if gi === 0}
						<button class="gcard" use:navItem bind:this={firstCard} onclick={() => onPlay(g.id)}>
							<span class="gicon" class:img={!!cover}>
								{#if cover}<img src={cover} alt="" />{:else}{initials(g.title)}{/if}
							</span>
							<span class="gmeta"><span class="gname">{g.title}</span><span class="gkind mono">{t('gaming.gameKind')}</span></span>
							<Icon name="gaming" size={16} class="push" />
						</button>
					{:else}
						<button class="gcard" use:navItem onclick={() => onPlay(g.id)}>
							<span class="gicon" class:img={!!cover}>
								{#if cover}<img src={cover} alt="" />{:else}{initials(g.title)}{/if}
							</span>
							<span class="gmeta"><span class="gname">{g.title}</span><span class="gkind mono">{t('gaming.gameKind')}</span></span>
							<Icon name="gaming" size={16} class="push" />
						</button>
					{/if}
				{/each}
			{:else}
				<div class="hint">{t('home.noHostGames')}</div>
			{/if}
			{#if windows.length && onPlayWindow}
				<div class="section mono">{t('gaming.windows')}</div>
				{#each windows as w (w.hwnd)}
					<button class="gcard" use:navItem onclick={() => onPlayWindow(w)}>
						<span class="gicon"><Icon name="monitor" size={18} /></span>
						<span class="gmeta">
							<span class="gname">{w.title}</span>
							<span class="gkind mono">{usedWindows[w.hwnd] ? t('gaming.inPane', { n: usedWindows[w.hwnd] }) : t('gaming.windowKind')}</span>
						</span>
						{#if usedWindows[w.hwnd]}<span class="used mono">{usedWindows[w.hwnd]}</span>{/if}
						<Icon name="gaming" size={16} class="push" />
					</button>
				{/each}
			{/if}
			<div class="section mono">{t('gaming.screens')}</div>
			{#each displays?.length ? displays : [{ idx: 0, name: '', width: 0, height: 0, primary: true }] as d (d.idx)}
				<button
					class="gcard"
					use:navItem
					bind:this={() => undefined, (el) => { if (!games?.length && d.idx === (displays?.[0]?.idx ?? 0)) firstCard = el; }}
					onclick={() => onPlayDisplay?.(d.idx)}
				>
					<span class="gicon" class:img={!!desktopImg && d.primary}>
						{#if desktopImg && d.primary}<img src={desktopImg} alt="" />{:else}<Icon name="monitor" size={20} />{/if}
					</span>
					<span class="gmeta">
						<span class="gname">{t('gaming.screenN', { n: d.idx + 1 })}</span>
						<span class="gkind mono">
							{#if usedDisplays[d.idx]}{t('gaming.inPane', { n: usedDisplays[d.idx] })}{:else}{d.width ? `${d.width}×${d.height}` : t('gaming.wholeScreen')}{d.primary && d.width ? ` · ${t('gaming.primary')}` : ''}{/if}
						</span>
					</span>
					{#if usedDisplays[d.idx]}<span class="used mono">{usedDisplays[d.idx]}</span>{/if}
					<Icon name="gaming" size={16} class="push" />
				</button>
			{/each}
		</div>
	{:else}
		<div class="games">
			<button class="gcard" use:navItem bind:this={firstCard} onclick={() => onPlay('')}>
				<span class="gicon" class:img={!!desktopImg}>
					{#if desktopImg}<img src={desktopImg} alt="" />{:else}<Icon name="monitor" size={20} />{/if}
				</span>
				<span class="gmeta"><span class="gname">{t('gaming.desktop')}</span><span class="gkind mono">{t('gaming.wholeScreen')}</span></span>
				<Icon name="gaming" size={16} class="push" />
			</button>
			{#if games && games.length}
				{#each games as g (g.id)}
					{@const cover = coverFor(g)}
					<button class="gcard" use:navItem onclick={() => onPlay(g.id)}>
						<span class="gicon" class:img={!!cover}>
							{#if cover}<img src={cover} alt="" />{:else}{initials(g.title)}{/if}
						</span>
						<span class="gmeta"><span class="gname">{g.title}</span><span class="gkind mono">{g.kind}</span></span>
						<Icon name="gaming" size={16} class="push" />
					</button>
				{/each}
			{:else}
				<div class="hint">{t('home.noHostGames')}</div>
			{/if}
		</div>
	{/if}
</div>

<style>
	.used {
		min-width: 20px;
		height: 20px;
		padding: 0 6px;
		display: grid;
		place-items: center;
		border-radius: 10px;
		background: var(--accent-soft, var(--surface-3));
		color: var(--accent, var(--text));
		font-size: 11px;
		flex: none;
	}
	.section {
		margin-top: 10px;
		font-size: 11px;
		letter-spacing: 0.08em;
		text-transform: uppercase;
		color: var(--text-faint, var(--text-muted));
	}
	.gscreen {
		flex: 1;
		min-height: 0;
		overflow-y: auto;
		display: flex;
		flex-direction: column;
		align-items: center;
		padding: 32px 24px 24px;
		gap: 20px;
	}
	.ghead {
		width: 100%;
		max-width: 480px;
		display: flex;
		align-items: center;
		gap: 12px;
	}
	.back {
		width: 36px;
		height: 36px;
		display: grid;
		place-items: center;
		border: 1px solid var(--border);
		border-radius: 9px;
		background: var(--surface-2);
		color: var(--text-muted);
		cursor: pointer;
		flex: none;
	}
	.back:hover {
		background: var(--surface-3);
		color: var(--text);
	}
	.back :global(.flip) {
		transform: rotate(180deg);
	}
	.havatar {
		width: 40px;
		height: 40px;
		border-radius: 10px;
		background: var(--accent-soft);
		color: var(--accent-fg);
		display: grid;
		place-items: center;
		font-weight: 700;
		font-size: 13px;
		font-family: var(--font-display);
		flex: none;
		overflow: hidden;
	}
	.havatar img {
		width: 100%;
		height: 100%;
		object-fit: cover;
		display: block;
	}
	.hmeta {
		display: flex;
		flex-direction: column;
		line-height: 1.25;
		min-width: 0;
	}
	.htitle {
		font-size: 17px;
		font-weight: 600;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.htitle.mono {
		letter-spacing: 0.04em;
	}
	.hid {
		font-size: 12px;
		color: var(--text-faint);
		letter-spacing: 0.04em;
	}
	.state {
		display: flex;
		align-items: center;
		gap: 10px;
		font-size: 14px;
		color: var(--text-muted);
		padding: 40px 0;
	}
	.state.err {
		color: var(--danger-text);
		word-break: break-word;
		text-align: center;
		max-width: 480px;
	}
	.spinner {
		width: 18px;
		height: 18px;
		border: 2px solid var(--border-strong);
		border-top-color: var(--accent);
		border-radius: 50%;
		animation: spin 0.8s linear infinite;
	}
	@keyframes spin {
		to {
			transform: rotate(360deg);
		}
	}
	.games {
		width: 100%;
		max-width: 480px;
		display: flex;
		flex-direction: column;
		gap: 8px;
	}
	.hint {
		font-size: 12.5px;
		color: var(--text-faint);
		text-align: center;
		padding: 12px;
	}
	.gcard {
		display: flex;
		align-items: center;
		gap: 12px;
		width: 100%;
		padding: 13px 15px;
		background: var(--surface-2);
		border: 1px solid var(--border);
		border-radius: var(--r);
		cursor: pointer;
		text-align: left;
		color: var(--text);
	}
	.gcard:hover {
		background: var(--surface-3);
	}
	.gicon {
		width: 40px;
		height: 40px;
		border-radius: 10px;
		background: var(--accent-soft);
		color: var(--accent-fg);
		display: grid;
		place-items: center;
		font-weight: 700;
		font-size: 13px;
		font-family: var(--font-display);
		flex: none;
		overflow: hidden;
	}
	/* Cover-image variant: a 16:9 thumbnail (host desktop screenshot / Steam logo / cover). */
	.gicon.img {
		width: 64px;
		background: var(--surface-3);
	}
	.gicon img {
		width: 100%;
		height: 100%;
		object-fit: cover;
		display: block;
	}
	.gmeta {
		display: flex;
		flex-direction: column;
		line-height: 1.3;
		min-width: 0;
	}
	.gname {
		font-size: 15px;
		font-weight: 600;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.gkind {
		font-size: 11px;
		color: var(--text-faint);
	}
	.gcard :global(.push) {
		margin-left: auto;
		color: var(--text-faint);
	}
</style>
