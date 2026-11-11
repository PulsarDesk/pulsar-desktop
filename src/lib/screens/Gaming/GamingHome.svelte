<script lang="ts">
	import { onMount, tick } from 'svelte';
	import Icon from '#lib/Icon.svelte';
	import Modal from '#lib/Modal.svelte';
	import DeviceFormModal from '#lib/DeviceFormModal.svelte';
	import ConnectModal from './ConnectModal.svelte';
	import { t } from '#lib/i18n.svelte.js';
	import { api, type LanDevice } from '#lib/api.js';
	import {
		gameHistoryPeers,
		removeFromGameHistory,
		fmtPeerId,
		normalizeId,
		avatarFor,
		savedPeers,
		isSaved,
		addPeer,
		unsavePeer
	} from '#lib/peers.svelte.js';
	import { isLive } from '#lib/presence.svelte.js';
	import type { GamepadNav } from '#lib/gamepadNav.svelte.js';

	type Props = {
		/** The shared GamepadNav (our items + the connect modal register against it). */
		nav: GamepadNav;
		/** Pick a host by id → the shell opens the connecting/games screen (no popup for games). */
		onPickHost: (id: string, name?: string) => void;
	};
	let { nav, onPickHost }: Props = $props();
	const navItem = nav.item;

	// Game-only recents (separate timeline from remote recents / Devices).
	const recents = $derived(gameHistoryPeers(12));
	// The Devices address book (shared with remote mode). The star on a LAN host / recent
	// saves it here (or takes it out); "+" adds one by ID.
	const devices = $derived(savedPeers());
	// "+" opens the same device form as edit (empty); its ID field opens the keypad.
	let addOpen = $state(false);
	let idPick = $state<{ current: string; set: (id: string) => void } | null>(null);
	// The keypad calls onClose BEFORE onPick, so keep the setter outside `idPick`.
	let pickSet: ((id: string) => void) | null = null;
	function openAdd() {
		addOpen = true;
		nav.pushBack(closeEditDev);
		tick().then(() => nav.focusFirst());
	}
	// Removing a saved device asks first (one stray pad press must not delete it) — both the
	// Devices card's × and un-starring a LAN / recent card.
	let removeDev = $state<{ id: string; name: string } | null>(null);
	// The button that opened the confirm: focus returns to it on cancel (so a pad user lands
	// back inside the same card), else to the first item.
	let removeFrom: HTMLElement | null = null;
	function askRemove(p: { id: string; name: string }) {
		removeFrom = typeof document !== 'undefined' ? (document.activeElement as HTMLElement | null) : null;
		removeDev = { id: p.id, name: p.name };
		nav.pushBack(closeRemove); // B / Escape cancels
		tick().then(() => nav.focusFirst());
	}
	function closeRemove() {
		removeDev = null;
		nav.popBack(closeRemove);
		const from = removeFrom;
		removeFrom = null;
		tick().then(() => {
			if (from?.isConnected) nav.focus(from);
			else nav.focusFirst();
		});
	}
	// The star adds at once; taking a device out of Devices goes through the confirm.
	function toggleStar(id: string, name: string) {
		if (isSaved(id)) askRemove({ id, name: savedPeers().find((p) => p.id === normalizeId(id))?.name ?? name });
		else addPeer(name, id);
	}
	function confirmRemove() {
		if (removeDev) unsavePeer(removeDev.id);
		closeRemove();
	}

	// LAN auto-discovery: Pulsar hosts announcing on this network (multicast beacon).
	let lan = $state<LanDevice[]>([]);
	const lanHosts = $derived(lan.filter((d) => d.has_id && d.id));
	// LAN-reachable keys (normalized ids + addresses) for the recents online/offline dot.
	const lanKeys = $derived(
		new Set(
			lan.flatMap((d) => [d.id ? normalizeId(d.id) : '', d.addr, d.addr?.split(':')[0]]).filter(Boolean)
		)
	);
	const isOnline = (id: string) => lanKeys.has(normalizeId(id)) || lanKeys.has(id) || isLive(id);
	onMount(() => {
		const refresh = () => api.lanDevices().then((d) => (lan = d)).catch(() => {});
		refresh();
		const timer = setInterval(refresh, 2000);
		return () => clearInterval(timer);
	});

	// The connect pop-up collects ONLY the host id (ID box + numpad); on submit the shell
	// shows the games screen. A recent/LAN card already has the id, so it skips the popup.
	let modalOpen = $state(false);

	function initials(name: string) {
		return name.split(' ').map((w) => w[0]).slice(0, 2).join('').toUpperCase();
	}

	// Device edit: the SAME dialog as the remote Devices screen (name, ID, picture).
	let editDev = $state<{ id: string; name: string; image?: string } | null>(null);
	function openEdit(p: { id: string; name: string; image?: string }) {
		editDev = { id: p.id, name: p.name, image: p.image };
		nav.pushBack(closeEditDev); // B / Escape closes the dialog
		tick().then(() => nav.focusFirst());
	}
	function closeEditDev() {
		editDev = null;
		addOpen = false;
		nav.popBack(closeEditDev);
		tick().then(() => nav.focusFirst());
	}

	// Display image for a recent: the user-chosen `image` (icon: or data URL) wins,
	// then the pushed identity avatar, else initials.
	const recentImg = (r: { image?: string; avatar?: string }) =>
		r.image && !r.image.startsWith('icon:') ? r.image : r.avatar;
	const recentIcon = (r: { image?: string }) =>
		r.image?.startsWith('icon:') ? r.image.slice(5) : '';
</script>

<div class="ghome">
	<div class="hero">
		<span class="eyebrow mono">{t('gaming.eyebrow')}</span>
		<h1>{t('gaming.title')}</h1>
		<button class="btn btn-primary connect" data-navdefault use:navItem onclick={() => (modalOpen = true)}>
			<Icon name="connect" size={20} />
			{t('gaming.connectCta')}
		</button>
	</div>

	{#if lanHosts.length}
		<div class="recents">
			<div class="rlab"><span class="lpulse"></span>{t('gaming.lan')}</div>
			<div class="rgrid">
				{#each lanHosts as d (d.addr + '|' + d.id)}
					<div class="rrow" data-navgroup>
						<button class="rcard" data-navcard use:navItem onclick={() => onPickHost(d.id, d.name)}>
							<span class="ravatar">
								{#if avatarFor(d.id)}<img class="rimg" src={avatarFor(d.id)} alt="" />{:else}{initials(d.name)}{/if}
							</span>
							<span class="rmeta">
								<span class="rname">{d.name}{@render savedMark(d.id)}</span>
								<span class="rid mono">{fmtPeerId(d.id)}</span>
							</span>
						</button>
						<div class="ractions">{@render go(d.id, d.name)}{@render star(d.id, d.name)}</div>
					</div>
				{/each}
			</div>
		</div>
	{/if}

	<div class="recents">
		<div class="rlab">{t('gaming.recents')}</div>
		{#if recents.length === 0}
			<div class="empty">{t('gaming.noRecents')}</div>
		{:else}
			<div class="rgrid">
				{#each recents as r (r.id)}
					<div class="rrow" data-navgroup>
						<button class="rcard" data-navcard use:navItem onclick={() => onPickHost(r.id, r.name)}>
							<span class="ravatar">
								{#if recentImg(r)}
									<img class="rimg" src={recentImg(r)} alt="" />
								{:else if recentIcon(r)}
									<Icon name={recentIcon(r)} size={20} />
								{:else}{initials(r.name)}{/if}
							</span>
							<span class="rmeta">
								<span class="rname">
									<span class="rdot" class:on={isOnline(r.id)} title={isOnline(r.id) ? t('home.online') : t('home.unknown')}></span>
									{r.name}{@render savedMark(r.id)}
								</span>
								<span class="rid mono">{fmtPeerId(r.id)}</span>
							</span>
						</button>
						<div class="ractions">
							{@render go(r.id, r.name)}
							{@render star(r.id, r.name)}
							<button
								class="ract"
								use:navItem
								title={t('home.removeRecent')}
								aria-label={t('home.removeRecent')}
								onclick={() => removeFromGameHistory(r.id)}><Icon name="x" size={13} /></button
							>
						</div>
					</div>
				{/each}
			</div>
		{/if}
	</div>

	<div class="recents">
		<div class="rlab">
			{t('gaming.devices')}
			<button
				class="radd"
				use:navItem
				title={t('gaming.addDevice')}
				aria-label={t('gaming.addDevice')}
				onclick={openAdd}><Icon name="plus" size={14} /></button
			>
		</div>
		{#if devices.length === 0}
			<div class="empty">{t('gaming.noDevices')}</div>
		{/if}
		<div class="rgrid">
			{#each devices as p (p.id)}
				<div class="rrow" data-navgroup>
					<button class="rcard" data-navcard use:navItem onclick={() => onPickHost(p.id, p.name)}>
						<span class="ravatar">
							{#if recentImg(p)}
								<img class="rimg" src={recentImg(p)} alt="" />
							{:else if recentIcon(p)}
								<Icon name={recentIcon(p)} size={20} />
							{:else}{initials(p.name)}{/if}
						</span>
						<span class="rmeta">
							<span class="rname">
								<span class="rdot" class:on={isOnline(p.id)}></span>
								{p.name}
							</span>
							<span class="rid mono">{fmtPeerId(p.id)}</span>
						</span>
					</button>
					<div class="ractions">
						{@render go(p.id, p.name)}
						<button
							class="ract"
							use:navItem
							title={t('recent.edit')}
							aria-label={t('recent.edit')}
							onclick={() => openEdit(p)}><Icon name="edit" size={13} /></button
						>
						<button
							class="ract"
							use:navItem
							title={t('gaming.removeDevice')}
							aria-label={t('gaming.removeDevice')}
							onclick={() => askRemove(p)}><Icon name="x" size={13} /></button
						>
					</div>
				</div>
			{/each}
		</div>
	</div>
</div>

{#snippet savedMark(id: string)}
	{#if isSaved(id)}<span class="rsaved" title={t('gaming.saved')}><Icon name="star" size={12} /></span>{/if}
{/snippet}

{#snippet star(id: string, name: string)}
	<button
		class="ract rstar"
		class:on={isSaved(id)}
		use:navItem
		title={t(isSaved(id) ? 'gaming.unstar' : 'gaming.star')}
		aria-label={t(isSaved(id) ? 'gaming.unstar' : 'gaming.star')}
		aria-pressed={isSaved(id)}
		onclick={() => toggleStar(id, name)}><Icon name="star" size={15} /></button
	>
{/snippet}

{#snippet go(id: string, name: string)}
	<!-- The card's own action as its first inner button: A on a card enters it and lands here,
	     so A, A connects. Only shown while the card holds focus (mouse users click the card). -->
	<button
		class="ract rgo"
		use:navItem
		title={t('gaming.connect')}
		aria-label={t('gaming.connect')}
		onclick={() => onPickHost(id, name)}><Icon name="connect" size={14} /></button
	>
{/snippet}

{#if idPick}
	<ConnectModal
		{nav}
		mode="id"
		initial={idPick.current}
		onPick={(id) => pickSet?.(id)}
		onClose={() => (idPick = null)}
	/>
{/if}

{#if modalOpen}
	<ConnectModal
		{nav}
		onPick={(id) => onPickHost(id)}
		onClose={() => (modalOpen = false)}
	/>
{/if}

{#if removeDev}
	<Modal title={t('gaming.removeDevice')} onClose={closeRemove} navModal>
		<div class="editform">
			<p class="confirm">{t('gaming.removeConfirm', { name: removeDev.name })}</p>
			<div class="factions">
				<button class="btn btn-ghost" use:navItem onclick={closeRemove}>{t('recent.cancel')}</button>
				<button class="btn btn-primary danger" use:navItem onclick={confirmRemove}>{t('gaming.removeYes')}</button>
			</div>
		</div>
	</Modal>
{/if}

{#if editDev || addOpen}
	<DeviceFormModal
		device={editDev}
		onClose={closeEditDev}
		note={t('gaming.editNeedsKbm')}
		{navItem}
		pickId={(current, set) => {
			pickSet = set;
			idPick = { current, set };
		}}
	/>
{/if}

<style>
	.ghome {
		flex: 1;
		min-height: 0;
		overflow-y: auto;
		display: flex;
		flex-direction: column;
		align-items: center;
		padding: 48px 24px 24px;
		gap: 40px;
	}
	.hero {
		width: 100%;
		max-width: 440px;
		display: flex;
		flex-direction: column;
		align-items: center;
		text-align: center;
		gap: 18px;
	}
	.eyebrow {
		font-size: 11px;
		letter-spacing: 0.16em;
		text-transform: uppercase;
		color: var(--accent-fg);
	}
	h1 {
		font-size: 34px;
		letter-spacing: -0.03em;
	}
	.connect {
		justify-content: center;
		min-width: 240px;
		padding: 16px 28px;
		font-size: 17px;
		margin-top: 4px;
		/* Clip the glass-shine sweep to the button's rounded box. */
		position: relative;
		overflow: hidden;
	}
	/* Slow left→right "glass/lightning" highlight gliding across the hero connect
	 * button. Cheap: a single skewed gradient layer animated with `transform`
	 * (translate only — no blur/filter), so it stays light even on software paint. */
	.connect::before {
		content: '';
		position: absolute;
		inset: 0;
		background: linear-gradient(
			105deg,
			transparent 30%,
			rgba(255, 255, 255, 0.35) 50%,
			transparent 70%
		);
		transform: translateX(-150%);
		/* Finite on purpose: an endless animation makes WebKitGTK repaint this area every display
		   frame (software rendering on the Orange Pi: the idle home screen held a core at ~90 %). */
		animation: connect-shine 3.5s ease-in-out 2;
		pointer-events: none;
	}
	@keyframes connect-shine {
		0% {
			transform: translateX(-150%);
		}
		100% {
			transform: translateX(150%);
		}
	}
	@media (prefers-reduced-motion: reduce) {
		.connect::before {
			animation: none;
			opacity: 0;
		}
	}
	.recents {
		width: 100%;
		max-width: 760px;
	}
	.rlab {
		display: flex;
		align-items: center;
		justify-content: center;
		gap: 8px;
		font-size: 11.5px;
		color: var(--text-faint);
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: 0.08em;
		margin-bottom: 14px;
		text-align: center;
	}
	.lpulse {
		width: 7px;
		height: 7px;
		border-radius: 50%;
		background: var(--ok);
		flex: none;
		/* Finite on purpose: an endless animation makes WebKitGTK repaint this area every display
		   frame (software rendering on the Orange Pi: the idle home screen held a core at ~90 %). */
		animation: lpulse 1.8s ease-out 3;
	}
	@keyframes lpulse {
		0% {
			box-shadow: 0 0 0 0 color-mix(in oklch, var(--ok) 55%, transparent);
		}
		70% {
			box-shadow: 0 0 0 7px transparent;
		}
		100% {
			box-shadow: 0 0 0 0 transparent;
		}
	}
	.empty {
		font-size: 12.5px;
		color: var(--text-faint);
		text-align: center;
		padding: 16px;
		border: 1px dashed var(--border);
		border-radius: var(--r-sm);
	}
	.rgrid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(220px, 1fr));
		gap: 10px;
	}
	.rrow {
		display: flex;
		align-items: center;
		gap: 6px;
	}
	.rrow .rcard {
		flex: 1;
		min-width: 0;
	}
	.rcard {
		display: flex;
		align-items: center;
		gap: 12px;
		padding: 11px 13px;
		background: var(--surface-2);
		border: 1px solid var(--border);
		border-radius: var(--r);
		cursor: pointer;
		text-align: left;
		color: var(--text);
		transition: background var(--dur) var(--ease);
	}
	.rcard:hover {
		background: var(--surface-3);
	}
	.ravatar {
		width: 38px;
		height: 38px;
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
	.rimg {
		width: 100%;
		height: 100%;
		object-fit: cover;
	}
	.rmeta {
		display: flex;
		flex-direction: column;
		line-height: 1.3;
		min-width: 0;
	}
	.rname {
		font-size: 14.5px;
		font-weight: 600;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
		display: inline-flex;
		align-items: center;
		gap: 6px;
	}
	.rdot {
		width: 7px;
		height: 7px;
		border-radius: 50%;
		background: var(--text-faint);
		flex: none;
	}
	.rdot.on {
		background: var(--ok);
		box-shadow: 0 0 0 2px color-mix(in oklch, var(--ok) 22%, transparent);
	}
	.rid {
		font-size: 11px;
		color: var(--text-faint);
	}
	/* Card actions (star / edit / remove) float over the card's right edge instead of
	   taking width beside it: hidden until the card is hovered or holds the (pad) focus, so
	   the card keeps its full size. A saved star stays visible as a marker. */
	.rrow {
		position: relative;
	}
	.ractions {
		--fade: var(--surface-2);
		position: absolute;
		top: 1px;
		bottom: 1px;
		right: 1px;
		display: flex;
		align-items: center;
		gap: 4px;
		padding: 0 8px 0 36px;
		border-radius: 0 var(--r) var(--r) 0;
		pointer-events: none;
		/* Shown with the buttons: the card's own colour fades in behind them, so a long name
		   runs out under a gradient instead of being overdrawn. */
		background: linear-gradient(90deg, transparent, var(--fade) 30px);
		opacity: 0;
		transition: opacity var(--dur) var(--ease);
	}
	.rrow:hover .ractions {
		--fade: var(--surface-3);
	}
	.rrow:hover .ractions,
	.rrow:focus-within .ractions,
	.rrow:has(:global([data-navfocus])) .ractions {
		opacity: 1;
	}
	.ract {
		width: 28px;
		height: 28px;
		padding: 0;
		display: grid;
		place-items: center;
		border: 1px solid var(--border);
		border-radius: 8px;
		background: var(--surface);
		color: var(--text-muted);
		cursor: pointer;
		box-shadow: 0 2px 8px -2px rgb(0 0 0 / 0.25);
		opacity: 0;
		transform: scale(0.9);
		pointer-events: auto;
		transition:
			opacity var(--dur) var(--ease),
			transform var(--dur) var(--ease);
	}
	.rrow:hover .ract,
	.rrow:focus-within .ract,
	.rrow:has(:global([data-navfocus])) .ract {
		opacity: 1;
		transform: none;
	}
	.ract:hover {
		background: var(--accent-soft);
		color: var(--accent-fg);
		border-color: var(--accent);
	}
	.rstar.on {
		color: var(--accent);
	}
	/* Connect button: only while the card (or one of its buttons) holds focus — the pad needs
	   it inside an entered card; on hover the card itself is the click target. */
	.rgo {
		display: none;
	}
	.rrow:focus-within .rgo,
	.rrow:has(:global([data-navfocus])) .rgo {
		display: grid;
	}
	/* An entered card (focus on one of its buttons) keeps an accent edge, so it is clear which
	   card the buttons belong to. */
	.rrow:has(.ract:global([data-navfocus])) .rcard {
		border-color: var(--accent);
	}
	/* Saved marker next to the name (the star button itself only shows on hover/focus). */
	.rsaved {
		display: inline-grid;
		place-items: center;
		flex: none;
		color: var(--accent);
	}
	.rsaved :global(svg) {
		fill: currentColor;
	}
	.rstar.on :global(svg) {
		fill: currentColor;
	}
	/* Add (+) beside the Devices title. */
	.radd {
		flex: none;
		width: 22px;
		height: 22px;
		padding: 0;
		margin-left: 2px;
		display: grid;
		place-items: center;
		border: 1px solid var(--border);
		border-radius: 6px;
		background: var(--surface-2);
		color: var(--text-muted);
		cursor: pointer;
	}
	.radd:hover {
		background: var(--accent-soft);
		color: var(--accent-fg);
		border-color: var(--accent);
	}

	.confirm {
		margin: 0 0 18px;
		color: var(--text-muted);
		line-height: 1.5;
	}
	.danger {
		background: var(--danger);
		border-color: var(--danger);
	}
	/* Remove-device confirm dialog. */
	.editform {
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.editform .factions {
		display: flex;
		justify-content: flex-end;
		gap: 10px;
		margin-top: 18px;
	}
</style>
