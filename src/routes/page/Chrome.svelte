<script lang="ts">
	import { onMount } from 'svelte';
	import Icon from '#lib/Icon.svelte';
	import { isTauri, isWindowMaximized } from '#lib/api.js';
	import { t } from '#lib/i18n.svelte.js';
	import { gamingNav } from '#lib/gamepadNav.svelte.js';
	import { update } from '#lib/update.svelte.js';

	type Props = {
		title: string;
		dark: boolean;
		onToggleTheme: () => void;
		/** Whether the app is in gaming mode (drives the toggle's active styling). */
		gaming: boolean;
		/** Flip the app-level personality (remote ↔ gaming). */
		onToggleMode: () => void;
		/** A connection is open (a client session or one this machine hosts): the mode can't
		 *  change, since the open session belongs to the current personality. */
		modeLocked?: boolean;
		/** Current split layout ('off' = normal single-tab flow) — lights up the button. */
		splitMode?: 'off' | 'h2' | 'v2' | 'grid4';
		/** Open the split-layout chooser (SplitPicker). */
		onSplit?: () => void;
		/** Window fullscreen state — lights up the fullscreen toggle. */
		fullscreen?: boolean;
		/** Toggle window fullscreen (plain — keeps this bar on the home; immersive is F11).
		 *  `dragFrom` is passed only by a title-bar drag-out (see `onBarDown`). */
		onToggleFullscreen?: (dragFrom?: [number, number]) => void;
		/** Update badge click (the shell moves to Home first, then opens the modal). */
		onUpdate?: () => void;
		/** Split mode only (the home with its settings / controllers is hidden there): open
		 *  Settings / the controllers popup from the bar. */
		onOpenSettings?: () => void;
		onOpenControllers?: () => void;
	};
	let {
		title,
		dark,
		onToggleTheme,
		gaming,
		onToggleMode,
		modeLocked = false,
		splitMode = 'off',
		onSplit,
		fullscreen = false,
		onToggleFullscreen,
		onUpdate,
		onOpenSettings,
		onOpenControllers
	}: Props = $props();

	// Real app version (from tauri.conf) instead of a hardcoded string. Empty outside Tauri.
	let ver = $state('');
	// The window is FRAMELESS (decorations:false) — this bar IS the title bar, so it draws its
	// own minimize / maximize-restore / close controls. `maximized` swaps the max↔restore glyph.
	let maximized = $state(false);
	let unlistenResize: (() => void) | undefined;
	// Sync onMount returning a sync cleanup; the async setup runs in an IIFE (an async onMount's
	// return value is NOT used as a cleanup by Svelte, which is what tripped the type check).
	onMount(() => {
		if (!isTauri) return;
		(async () => {
			try {
				const { getVersion } = await import('@tauri-apps/api/app');
				ver = await getVersion();
			} catch {
				/* keep empty → shows plain "Pulsar" */
			}
			try {
				const { getCurrentWindow } = await import('@tauri-apps/api/window');
				const w = getCurrentWindow();
				maximized = await isWindowMaximized();
				unlistenResize = await w.onResized(async () => {
					maximized = await isWindowMaximized();
				});
			} catch {
				/* not in Tauri / API unavailable */
			}
		})();
		return () => unlistenResize?.();
	});

	// Frameless window controls — minimize / maximize-restore / close via the Tauri window API.
	async function winCtl(action: 'min' | 'max' | 'close') {
		if (!isTauri) return;
		try {
			const { getCurrentWindow } = await import('@tauri-apps/api/window');
			const w = getCurrentWindow();
			if (action === 'min') await w.minimize();
			else if (action === 'max') {
				// In fullscreen this is the way back out — through the SAME toggle as the
				// fullscreen button / F11, so the app's fullscreen state stays in sync.
				if (fullscreen && onToggleFullscreen) onToggleFullscreen();
				else if (await isWindowMaximized()) await w.unmaximize();
				else await w.maximize();
			} else await w.close();
		} catch (e) {
			console.warn(`[winctl] ${action} failed`, e);
		}
	}

	// Title-bar grab while FULLSCREEN: Tauri's drag region would move the monitor-sized window
	// as-is. Take the press over instead (this listener runs before Tauri's document-level one):
	// once the pointer actually moves, leave fullscreen and hand the drag to the OS with the
	// window re-centred under the grab point — like dragging a maximized window. A double-click
	// leaves fullscreen too (Tauri's would toggle maximize underneath it).
	const DRAG_SLOP = 4;
	function onBarDown(e: MouseEvent) {
		if (!fullscreen || !onToggleFullscreen || e.button !== 0) return;
		const el = e.target as HTMLElement | null;
		if (!el?.hasAttribute('data-tauri-drag-region')) return;
		e.stopPropagation();
		e.preventDefault();
		const toggle = onToggleFullscreen;
		if (e.detail === 2) {
			toggle();
			return;
		}
		const x0 = e.clientX;
		const y0 = e.clientY;
		const done = () => {
			window.removeEventListener('mousemove', move, true);
			window.removeEventListener('mouseup', done, true);
		};
		const move = (m: MouseEvent) => {
			if (Math.abs(m.clientX - x0) < DRAG_SLOP && Math.abs(m.clientY - y0) < DRAG_SLOP) return;
			done();
			if ((m.buttons & 1) === 0) return;
			toggle([x0 / Math.max(1, window.innerWidth), y0]);
		};
		window.addEventListener('mousemove', move, true);
		window.addEventListener('mouseup', done, true);
	}

	// Register a top-bar button with the gaming nav ONLY while in gaming mode, so the
	// controller can roam up to it (gaming/theme). Reactive: re-applies when the
	// mode toggles. In remote mode the nav isn't running, so it stays a plain button.
	function navItem(node: HTMLElement, on: boolean) {
		let off: (() => void) | undefined;
		const apply = (g: boolean) => {
			off?.();
			off = undefined;
			if (g) off = gamingNav.item(node).destroy;
		};
		apply(on);
		return { update: apply, destroy: () => off?.() };
	}
</script>

<!-- App toolbar (the OS now draws the title bar + window controls + drag/resize). -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="chrome" data-tauri-drag-region onmousedown={onBarDown}>
	<div class="ctitle">{title}</div>
	<div class="cright">
		{#if update.available}
			<!-- Update badge: sits LEFT of the split button; pulses until the user opens
			     the modal. Shown even when this install can't self-update (the modal then
			     explains how to update manually). -->
			<button
				class="game-btn upd"
				use:navItem={gaming}
				title={t('update.available')}
				aria-label={t('update.available')}
				onclick={() => (onUpdate ? onUpdate() : (update.open = true))}
			>
				<Icon name="download" size={16} />
				<span class="upd-dot"></span>
			</button>
		{/if}
		{#if onSplit}
			<button
				class="game-btn"
				class:on={splitMode !== 'off'}
				use:navItem={gaming}
				title={t('chrome.split')}
				aria-label={t('chrome.splitToggle')}
				aria-pressed={splitMode !== 'off'}
				onclick={onSplit}
			>
				<Icon name="split" size={16} />
			</button>
		{/if}
		{#if splitMode !== 'off' && onOpenControllers}
			<button
				class="game-btn"
				use:navItem={gaming}
				title={t('controllers.title')}
				aria-label={t('controllers.title')}
				onclick={onOpenControllers}
			>
				<Icon name="gaming" size={16} />
			</button>
		{/if}
		{#if splitMode !== 'off' && onOpenSettings}
			<button
				class="game-btn"
				use:navItem={gaming}
				title={t('nav.settings')}
				aria-label={t('nav.settings')}
				onclick={onOpenSettings}
			>
				<Icon name="settings" size={16} />
			</button>
		{/if}
		{#if onToggleFullscreen}
			<button
				class="game-btn"
				class:on={fullscreen}
				use:navItem={gaming}
				title={t('gaming.fullscreen')}
				aria-label={t('gaming.fullscreen')}
				aria-pressed={fullscreen}
				onclick={() => onToggleFullscreen()}
			>
				<Icon name="expand" size={16} />
			</button>
		{/if}
		<button
			class="game-btn"
			class:on={gaming}
			use:navItem={gaming}
			title={modeLocked ? t('chrome.gamingLocked') : t('chrome.gamingMode')}
			aria-label={t('chrome.gamingToggle')}
			aria-pressed={gaming}
			disabled={modeLocked}
			onclick={() => {
				if (!modeLocked) onToggleMode();
			}}
		>
			<Icon name="gaming" size={16} />
		</button>
		<button
			class="icon-btn"
			use:navItem={gaming}
			title={t('chrome.theme')}
			aria-label={t('chrome.themeToggle')}
			onclick={onToggleTheme}
		>
			<Icon name={dark ? 'sun' : 'moon'} size={16} />
		</button>
		<span class="mono ver">{ver ? `Pulsar v${ver}` : 'Pulsar'}</span>
		{#if isTauri}
			<div class="winctl">
				<button class="wc" onclick={() => winCtl('min')} title={t('chrome.minimize')} aria-label={t('chrome.minimize')}>
					<Icon name="winmin" size={15} />
				</button>
				<button class="wc" onclick={() => winCtl('max')} title={t('chrome.maximize')} aria-label={t('chrome.maximize')}>
					<Icon name={maximized || fullscreen ? 'winrestore' : 'winmax'} size={14} />
				</button>
				<button class="wc close" onclick={() => winCtl('close')} title={t('chrome.close')} aria-label={t('chrome.close')}>
					<Icon name="x" size={15} />
				</button>
			</div>
		{/if}
	</div>
</div>

<style>
	.chrome {
		height: 44px;
		flex: none;
		display: flex;
		align-items: center;
		padding: 0 14px;
		border-bottom: 1px solid var(--border);
		background: var(--surface);
		user-select: none;
		position: relative;
	}
	.ctitle {
		position: absolute;
		left: 0;
		right: 0;
		text-align: center;
		font-size: 13px;
		font-weight: 600;
		color: var(--text-muted);
		pointer-events: none;
	}
	.cright {
		margin-left: auto;
		display: flex;
		align-items: center;
		gap: 10px;
		z-index: 1;
	}
	.cright .ver {
		font-size: 11.5px;
		color: var(--text-faint);
	}
	.upd {
		position: relative;
		color: var(--accent-fg);
	}
	.upd-dot {
		position: absolute;
		top: 3px;
		right: 3px;
		width: 7px;
		height: 7px;
		border-radius: 50%;
		background: var(--accent);
		animation: upd-pulse 1.6s ease-in-out infinite;
	}
	@keyframes upd-pulse {
		0%,
		100% {
			opacity: 1;
			transform: scale(1);
		}
		50% {
			opacity: 0.45;
			transform: scale(0.8);
		}
	}
	.game-btn {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 30px;
		height: 28px;
		border: 1px solid var(--border);
		border-radius: var(--r-sm);
		background: var(--surface-2);
		color: var(--text-muted);
		cursor: pointer;
		transition:
			background var(--dur) var(--ease),
			color var(--dur) var(--ease),
			border-color var(--dur) var(--ease);
	}
	.game-btn:hover:not(:disabled) {
		background: var(--surface-3);
		color: var(--text);
	}
	.game-btn:disabled {
		opacity: 0.45;
		cursor: not-allowed;
	}
	/* Active = gaming mode on: cyan accent (the accent family is already cyan via
	   data-gaming, so this reads as the lit-up gaming control). */
	.game-btn.on {
		background: var(--accent-soft);
		border-color: var(--accent);
		color: var(--accent-ink);
	}
	/* Frameless window controls: flush to the top-right corner, full bar height, no border —
	   the standard custom-title-bar look. `margin-right` cancels the bar's right padding so they
	   reach the window edge; close gets the red hover. Not a drag region (they're buttons). */
	.winctl {
		display: flex;
		align-items: stretch;
		height: 44px;
		margin-left: 6px;
		margin-right: -14px;
	}
	.wc {
		width: 44px;
		border: none;
		background: transparent;
		color: var(--text-muted);
		cursor: pointer;
		display: grid;
		place-items: center;
		transition:
			background var(--dur) var(--ease),
			color var(--dur) var(--ease);
	}
	.wc:hover {
		background: var(--surface-3);
		color: var(--text);
	}
	.wc.close:hover {
		background: #e81123;
		color: #fff;
	}
</style>
