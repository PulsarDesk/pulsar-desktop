<script lang="ts">
	import { onMount } from 'svelte';
	import Icon from '#lib/Icon.svelte';
	import { api } from '#lib/api.js';
	import type { Config, NetworkMode } from '#lib/types.js';
	import { t } from '#lib/i18n.svelte.js';
	import { saveTick, configTick } from '#lib/settings.svelte.js';
	import { navContainer } from '#lib/gamepadNav.svelte.js';
	import { closeOnEscape } from '#lib/escapeStack.js';
	import { applySettingsFilter, clearSettingsHighlight } from '#lib/settingsSearch.js';
	import DisplayTab from './Settings/DisplayTab.svelte';
	import NetworkTab from './Settings/NetworkTab.svelte';
	import SecurityTab from './Settings/SecurityTab.svelte';
	import GeneralTab from './Settings/GeneralTab.svelte';

	// `onReconnect` re-registers after relay/network changes. `padNav` = make every control
	// here controller-navigable (set when Settings is shown inside the gaming-mode shell).
	let { onReconnect, padNav = false }: { onReconnect?: () => void; padNav?: boolean } = $props();

	let tab = $state<'display' | 'network' | 'security' | 'general'>('display');

	// Search across every tab (issue #6): while a query is set, all tabs render stacked and
	// settingsSearch.ts hides what doesn't match. Ctrl/Cmd+F focuses the box, Esc clears it.
	let query = $state('');
	let searchEl = $state<HTMLInputElement | null>(null);
	let cardEl = $state<HTMLDivElement | null>(null);
	let found = $state(-1);
	const searching = $derived(query.trim().length > 0);
	$effect(() => {
		const q = query;
		const el = cardEl;
		if (!el) return;
		const run = () => (found = applySettingsFilter(el, q));
		run();
		if (!q.trim()) return;
		// Rows that appear while searching (a toggle revealing sub-rows, config loading) get
		// filtered too.
		const mo = new MutationObserver(() => {
			mo.disconnect();
			run();
			mo.observe(el, { childList: true, subtree: true });
		});
		mo.observe(el, { childList: true, subtree: true });
		return () => mo.disconnect();
	});
	$effect(() => clearSettingsHighlight);
	$effect(() => {
		if (searching) return closeOnEscape(() => (query = ''));
	});
	$effect(() => {
		const onKey = (e: KeyboardEvent) => {
			if ((e.ctrlKey || e.metaKey) && !e.altKey && e.key.toLowerCase() === 'f') {
				e.preventDefault();
				searchEl?.focus();
				searchEl?.select();
			}
		};
		window.addEventListener('keydown', onKey);
		return () => window.removeEventListener('keydown', onKey);
	});
	function pickTab(id: typeof tab) {
		query = '';
		tab = id;
	}
	let config = $state<Config | null>(null);
	let saveErr = $state('');
	let detected = $state<string[]>([]);

	// Centered "saved" toast, shared by both save paths. Rapid saves restart the
	// fade timer instead of stacking toasts (debounce), so a burst of changes
	// shows one steady toast that fades 3s after the LAST save.
	let toast = $state(false);
	let toastTimer: ReturnType<typeof setTimeout> | undefined;
	function notifySaved() {
		toast = true;
		clearTimeout(toastTimer);
		toastTimer = setTimeout(() => (toast = false), 3000);
	}
	// The tabs call saveUi() directly (it bumps saveTick) — react here so UI-only
	// saves get the same toast. Skip the initial mount value.
	let lastTick = saveTick.n;
	$effect(() => {
		if (saveTick.n !== lastTick) {
			lastTick = saveTick.n;
			notifySaved();
		}
	});

	const allTabs = [
		{ id: 'display', icon: 'monitor' },
		{ id: 'network', icon: 'wifi' },
		{ id: 'security', icon: 'shield' },
		{ id: 'general', icon: 'settings' }
	] as const;
	// Gaming mode is a pure client (it never hosts), so its Settings show only client
	// settings by default; "all settings" reveals the host ones (shared with remote mode).
	let showAll = $state(false);
	const clientOnly = $derived(padNav && !showAll);
	// Security holds host-only settings (unattended access, password, remote cursor, UAC).
	const tabs = $derived(clientOnly ? allTabs.filter((x) => x.id !== 'security') : allTabs);
	$effect(() => {
		if (clientOnly && tab === 'security') tab = 'display';
	});

	// Only these fields affect the relay registration — anything else (audio,
	// unattended, avatar…) saves without tearing the node down and re-registering.
	const reconnectKey = (c: Config) =>
		`${c.relay}|${c.network_mode}|${c.node_port}|${c.device_name}|${c.port_mapping ?? true}`;
	let lastReconnectKey = '';

	onMount(async () => {
		config = await api.getConfig();
		lastReconnectKey = reconnectKey(config);
		detected = await api.availableEncoders().catch(() => []);
	});

	// Core config (relay / network mode) — persisted via the Rust core, then
	// re-register so the change takes effect.
	async function saveConfig() {
		if (!config) return;
		// A cleared number input binds null (and >65535 overflows the core's u16) —
		// clamp before the snapshot crosses to Rust, or set_config rejects EVERY
		// subsequent save (they all send the whole snapshot).
		config.node_port = Math.min(65535, Math.max(0, Math.round(Number(config.node_port)) || 0));
		saveErr = '';
		try {
			await api.setConfig($state.snapshot(config));
		} catch (e) {
			saveErr = e instanceof Error ? e.message : String(e);
			return;
		}
		notifySaved();
		// Bump the shared tick for CORE saves too: the shell (+page) re-fetches its
		// own config copy off this, so screens reading shell config (the Home
		// unattended-access warning / blanked one-time password) reflect a Settings
		// change immediately instead of only after the next go-online.
		saveTick.n += 1;
		lastTick = saveTick.n; // our own bump — don't double-toast via the effect
		configTick.n += 1; // shell re-fetches its config copy off this (core saves only)
		const key = reconnectKey(config);
		if (key !== lastReconnectKey) {
			lastReconnectKey = key;
			onReconnect?.();
		}
	}
	function setMode(m: NetworkMode) {
		if (config) {
			config.network_mode = m;
			saveConfig();
		}
	}
	// Audio toggles live on the core Config (the host applies them per session).
	function toggleTransmit() {
		if (config) {
			config.transmit_audio = !config.transmit_audio;
			saveConfig();
		}
	}
	function toggleMute() {
		if (config) {
			config.mute_host_audio = !config.mute_host_audio;
			saveConfig();
		}
	}
	// How this device presents itself to peers (shown in their connect prompt).
	function setAvatar(mode: string) {
		if (config) {
			config.avatar_mode = mode;
			saveConfig();
		}
	}
	// Unattended access lives on the core Config (the host skips the one-time-password
	// auth when it's on).
	function toggleUnattended() {
		if (config) {
			config.unattended_access = !config.unattended_access;
			saveConfig();
		}
	}
</script>

<!-- Settings fills its box in both shells: the title and the tab rail stay put and only the
     content card scrolls (the parent must not scroll — HomeView pins `.content`). -->
<div class="settings fill" use:navContainer={padNav}>
{#if padNav}
	<div class="head gaming">
		<div>
			<h1>{t(showAll ? 'settings.title' : 'settings.gamingTitle')}</h1>
			<p class="sub">{t(showAll ? 'settings.gamingAllSub' : 'settings.gamingSub')}</p>
		</div>
		<button class="btn btn-ghost allbtn" onclick={() => (showAll = !showAll)}>
			<Icon name={showAll ? 'gaming' : 'settings'} size={16} />{t(showAll ? 'settings.gamingOnly' : 'settings.showAll')}
		</button>
	</div>
{:else}
	<div class="head"><h1>{t('settings.title')}</h1><p class="sub">{t('settings.sub')}</p></div>
{/if}
<div class="layout">
	<div class="rail">
		{#each tabs as tb (tb.id)}
			<button class="tab" class:on={!searching && tab === tb.id} onclick={() => pickTab(tb.id)}>
				<Icon name={tb.icon} size={17} />{t('settings.tab.' + tb.id)}
			</button>
		{/each}
	</div>

	<!-- Right column: the search box on top (level with the tab rail), the content card below. -->
	<div class="main">
		<div class="search">
			<Icon name="search" size={15} />
			<input
				bind:this={searchEl}
				bind:value={query}
				type="search"
				placeholder={t('settings.searchPlaceholder')}
				aria-label={t('settings.search')}
				spellcheck="false"
				autocomplete="off"
			/>
			{#if searching}
				<button class="clear" aria-label={t('settings.searchClear')} onclick={() => { query = ''; searchEl?.focus(); }}><Icon name="x" size={14} /></button>
			{/if}
		</div>
	<div class="card" bind:this={cardEl}>
		{#if searching}
			<!-- Every tab at once; settingsSearch.ts hides what doesn't match. -->
			{#each tabs as tb (tb.id)}
				<section class="tabpane" data-tab={tb.id}>
					<div class="tabhead"><Icon name={tb.icon} size={15} />{t('settings.tab.' + tb.id)}</div>
					{#if tb.id === 'display'}
						<DisplayTab bind:config {detected} {saveConfig} {toggleTransmit} {toggleMute} {clientOnly} />
					{:else if tb.id === 'network'}
						<NetworkTab bind:config {saveConfig} {setMode} {onReconnect} />
					{:else if tb.id === 'security'}
						<SecurityTab bind:config {toggleUnattended} {saveConfig} />
					{:else}
						<GeneralTab bind:config {saveConfig} {setAvatar} />
					{/if}
				</section>
			{/each}
			{#if found === 0}
				<p class="noresult">{t('settings.searchEmpty', { q: query.trim() })}</p>
			{/if}
		{:else if tab === 'display'}
			<DisplayTab
				bind:config
				{detected}
				{saveConfig}
				{toggleTransmit}
				{toggleMute}
				{clientOnly}
			/>
		{:else if tab === 'network'}
			<NetworkTab bind:config {saveConfig} {setMode} {onReconnect} />
		{:else if tab === 'security'}
			<SecurityTab bind:config {toggleUnattended} {saveConfig} />
		{:else}
			<GeneralTab bind:config {saveConfig} {setAvatar} />
		{/if}
		{#if saveErr}
			<div class="saveerr selectable" role="alert">{t('settings.saveFailed')} · {saveErr}</div>
		{/if}
	</div>
	</div>
</div>
</div>

{#if toast}
	<div class="toast" role="status" aria-live="polite">
		<span class="tick"><Icon name="check" size={13} /></span>{t('settings.savedToast')}
	</div>
{/if}

<style>
	/* The search filter hides nodes with the `hidden` attribute; rows set their own display. */
	.card :global([hidden]) {
		display: none !important;
	}
	.search {
		display: flex;
		align-items: center;
		gap: 8px;
		max-width: 420px;
		margin: 0 0 14px;
		padding: 8px 12px;
		border: 1px solid var(--border-strong);
		border-radius: var(--r-sm);
		background: var(--surface);
		color: var(--text-faint);
	}
	.main {
		display: flex;
		flex-direction: column;
		min-width: 0;
		min-height: 0;
	}
	.search {
		flex: none;
	}
	.search:focus-within {
		border-color: var(--accent);
	}
	.search input {
		flex: 1;
		min-width: 0;
		border: none;
		outline: none;
		background: transparent;
		color: var(--text);
		font-family: var(--font-sans);
		font-size: 13.5px;
	}
	.search input::-webkit-search-cancel-button {
		display: none;
	}
	.search .clear {
		display: grid;
		place-items: center;
		padding: 2px;
		border: none;
		background: transparent;
		color: inherit;
		cursor: pointer;
	}
	.tabpane + .tabpane {
		margin-top: 26px;
	}
	.tabhead {
		display: flex;
		align-items: center;
		gap: 8px;
		font-size: 15px;
		font-weight: 700;
		color: var(--text);
		margin-bottom: 4px;
	}
	.noresult {
		color: var(--text-muted);
		font-size: 13.5px;
		padding: 18px 0;
	}
	:global(::highlight(pulsar-settings-search)) {
		background-color: var(--accent-soft);
		color: var(--accent-ink);
	}
	.head {
		margin-bottom: 28px;
	}
	.head.gaming {
		display: flex;
		align-items: flex-start;
		justify-content: space-between;
		gap: 16px;
	}
	.allbtn {
		flex: none;
		display: inline-flex;
		align-items: center;
		gap: 8px;
	}
	h1 {
		font-size: 27px;
		letter-spacing: -0.03em;
	}
	.sub {
		color: var(--text-muted);
		font-size: 14.5px;
		margin: 7px 0 0;
	}
	.layout {
		display: grid;
		grid-template-columns: 186px 1fr;
		gap: 24px;
		align-items: start;
	}
	.rail {
		display: flex;
		flex-direction: column;
		gap: 3px;
		/* Remote shell: the page scrolls, the tab list stays in view. */
		position: sticky;
		top: 0;
	}
	/* Gaming mode: a full-height column — title, then rail | card — where only the card
	   scrolls (GamingShell's .settings-wrap no longer scrolls itself). */
	.settings.fill {
		display: flex;
		flex-direction: column;
		flex: 1;
		height: 100%;
		min-height: 0;
	}
	.settings.fill .head {
		flex: none;
	}
	.settings.fill .layout {
		flex: 1;
		min-height: 0;
		grid-template-rows: minmax(0, 1fr);
		align-items: stretch;
	}
	.settings.fill .rail {
		align-self: start;
		position: static;
	}
	.settings.fill .card {
		flex: 1;
		min-height: 0;
		overflow-y: auto;
	}
	.tab {
		display: flex;
		align-items: center;
		gap: 10px;
		padding: 10px 12px;
		border: none;
		border-radius: var(--r-sm);
		cursor: pointer;
		text-align: left;
		font-family: var(--font-sans);
		font-size: 14px;
		font-weight: 500;
		color: var(--text-muted);
		background: transparent;
	}
	.tab.on {
		font-weight: 600;
		color: var(--accent-ink);
		background: var(--accent-soft);
	}
	.saveerr {
		font-size: 12.5px;
		color: var(--danger-text);
		padding-top: 12px;
		word-break: break-word;
	}
	/* Bottom-center semi-transparent confirmation pill (green check + text);
	 * never intercepts clicks. The 3s lifetime is owned by the JS timer — this
	 * only plays the visual fade. */
	.toast {
		position: fixed;
		left: 0;
		right: 0;
		bottom: 36px;
		margin: 0 auto;
		width: max-content;
		z-index: 50;
		pointer-events: none;
		display: flex;
		align-items: center;
		gap: 9px;
		padding: 11px 20px;
		border-radius: 999px;
		background: color-mix(in oklch, var(--text) 80%, transparent);
		color: var(--surface);
		font-size: 14px;
		font-weight: 600;
		box-shadow: var(--shadow-lg);
		animation: toast-fade 3s ease forwards;
	}
	.tick {
		width: 19px;
		height: 19px;
		border-radius: 50%;
		background: var(--ok);
		color: #fff;
		display: grid;
		place-items: center;
		flex: none;
	}
	@keyframes toast-fade {
		0% {
			opacity: 0;
			transform: translateY(6px) scale(0.98);
		}
		8%,
		70% {
			opacity: 1;
			transform: translateY(0) scale(1);
		}
		100% {
			opacity: 0;
			transform: translateY(0) scale(1);
		}
	}
	@media (prefers-reduced-motion: reduce) {
		.toast {
			animation: none;
		}
	}
</style>
