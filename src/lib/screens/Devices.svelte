<script lang="ts">
	import { onMount } from 'svelte';
	import Icon from '#lib/Icon.svelte';
	import DeviceFormModal from '#lib/DeviceFormModal.svelte';
	import { savedPeers, removePeer, toggleFav, fmtPeerId, normalizeId } from '#lib/peers.svelte.js';
	import { isLive } from '#lib/presence.svelte.js';
	import { api, type LanDevice } from '#lib/api.js';
	import { t } from '#lib/i18n.svelte.js';

	type Target = { name: string; id: string };
	type Props = { onConnect: (t: Target, m?: 'remote' | 'game') => void };
	let { onConnect }: Props = $props();

	let q = $state('');

	// add/edit-device dialog (shared with the gaming home): `editDev` is the device being
	// edited, null when adding a new one.
	let adding = $state(false);
	let editDev = $state<{ id: string; name: string; image?: string } | null>(null);
	function openEdit(d: { id: string; name: string; image?: string }) {
		editDev = { id: d.id, name: d.name, image: d.image };
		adding = true;
	}
	function closeForm() {
		adding = false;
		editDev = null;
	}

	const peers = $derived(savedPeers());
	// Ids are stored canonical (despaced) — despace the query too so typing the
	// grouped form ("641 724…") still matches.
	const filtered = $derived.by(() => {
		const ql = q.trim().toLowerCase();
		const qd = ql.replace(/\s/g, '');
		return peers.filter(
			(d) => !ql || d.name.toLowerCase().includes(ql) || (!!qd && d.id.includes(qd))
		);
	});

	// Online presence: a saved device is "online" when its id (or address) is seen
	// in the LAN discovery beacon list. Poll like the Home LAN section does.
	let lan = $state<LanDevice[]>([]);
	async function refreshLan() {
		try {
			lan = await api.lanDevices();
		} catch {
			/* core not bound yet — keep the last list */
		}
	}
	onMount(() => {
		refreshLan();
		const timer = setInterval(refreshLan, 3000);
		return () => clearInterval(timer);
	});
	// Address-host equality (NOT startsWith — "192.168.1.4" prefix-matches ".40"/".42"):
	// strip the :port and compare the host exactly, or compare the full addr verbatim.
	const addrHost = (a: string) => {
		const i = a.lastIndexOf(':');
		return i > a.indexOf(']') ? a.slice(0, i) : a; // keep IPv6 (":"-rich) intact unless :port follows
	};
	const isOnline = (id: string) =>
		isLive(id) ||
		lan.some((d) => (d.id && normalizeId(d.id) === id) || d.addr === id || addrHost(d.addr) === addrHost(id));

	function relTime(ms: number | null): string {
		if (ms == null) return t('devices.never');
		const m = Math.floor((Date.now() - ms) / 60000);
		if (m < 1) return t('devices.justNow');
		if (m < 60) return t('devices.minAgo', { n: m });
		const h = Math.floor(m / 60);
		if (h < 24) return t('devices.hourAgo', { n: h });
		return t('devices.dayAgo', { n: Math.floor(h / 24) });
	}

	function initials(name: string) {
		return name
			.split(' ')
			.map((w) => w[0])
			.slice(0, 2)
			.join('')
			.toUpperCase();
	}
</script>

<div class="head">
	<div><h1>{t('devices.title')}</h1><p class="sub">{t('devices.sub')}</p></div>
	<button class="btn btn-primary" onclick={() => (adding = true)}>
		<Icon name="plus" size={17} />{t('devices.add')}
	</button>
</div>

{#if adding}
	<DeviceFormModal device={editDev} onClose={closeForm} />
{/if}

<div class="toolbar">
	<div class="field search">
		<Icon name="search" size={16} />
		<input bind:value={q} placeholder={t('devices.search')} aria-label={t('devices.searchAria')} />
	</div>
</div>

{#if peers.length === 0}
	<div class="empty card">
		<Icon name="devices" size={28} />
		<div class="et">{t('devices.empty')}</div>
		<!-- eslint-disable-next-line svelte/no-at-html-tags -->
		<p>{@html t('devices.emptyBody')}</p>
	</div>
{:else}
	<div class="grid">
		{#each filtered as d (d.id)}
			<div class="device-tile">
				<div class="tico">
					{#if d.image && !d.image.startsWith('icon:')}
						<img class="timg" src={d.image} alt="" />
					{:else if d.image?.startsWith('icon:')}
						<Icon name={d.image.slice(5)} size={22} />
					{:else if d.avatar}
						<img class="timg" src={d.avatar} alt="" />
					{:else}
						<span class="tinit">{initials(d.name)}</span>
					{/if}
				</div>
				<div class="meta">
					<div class="nrow">
						<button
							class="favbtn"
							class:on={d.fav}
							aria-label={t('devices.fav')}
							onclick={() => toggleFav(d.id)}><Icon name="star" size={13} /></button
						>
						<span class="dname">{d.name}</span>
					</div>
					<div class="did mono selectable">{fmtPeerId(d.id)}</div>
					<div class="dstatus">
						<span class="dot" class:on={isOnline(d.id)}></span>
						{isOnline(d.id) ? t('devices.online') : t('devices.unknown')} · {relTime(d.lastConnected)}
					</div>
				</div>
				<div class="actions">
					<button class="btn btn-ghost connect" onclick={() => onConnect({ name: d.name, id: d.id })}>
						{t('devices.connect')}
					</button>
					<button class="rm" aria-label={t('devices.edit')} title={t('devices.edit')} onclick={() => openEdit(d)}>
						<Icon name="edit" size={14} />
					</button>
					<button class="rm" aria-label={t('devices.remove')} onclick={() => removePeer(d.id)}>
						<Icon name="x" size={15} />
					</button>
				</div>
			</div>
		{/each}
	</div>
{/if}

<style>
	.head {
		display: flex;
		align-items: flex-end;
		justify-content: space-between;
		margin-bottom: 28px;
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
	.toolbar {
		display: flex;
		gap: 12px;
		margin-bottom: 18px;
		align-items: center;
	}
	.search {
		flex: 1;
		max-width: 340px;
	}
	.empty {
		text-align: center;
		color: var(--text-faint);
		padding: 40px 24px;
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 8px;
	}
	.empty .et {
		font-family: var(--font-display);
		font-size: 17px;
		color: var(--text);
	}
	.empty p {
		font-size: 13.5px;
		max-width: 40ch;
		line-height: 1.5;
		margin: 0;
	}
	.grid {
		display: grid;
		grid-template-columns: repeat(2, 1fr);
		gap: 14px;
	}
	.tico {
		width: 44px;
		height: 44px;
		border-radius: 11px;
		flex: none;
		display: grid;
		place-items: center;
		background: var(--accent-soft);
		color: var(--accent-fg);
		overflow: hidden;
	}
	.timg {
		width: 100%;
		height: 100%;
		object-fit: cover;
	}
	.tinit {
		font-weight: 700;
		font-size: 13px;
		font-family: var(--font-display);
	}
	.meta {
		flex: 1;
		min-width: 0;
	}
	.nrow {
		display: flex;
		align-items: center;
		gap: 6px;
	}
	.favbtn {
		border: none;
		background: transparent;
		color: var(--border-strong);
		cursor: pointer;
		padding: 0;
		display: grid;
		place-items: center;
	}
	.favbtn.on {
		color: var(--warn);
	}
	.dname {
		font-size: 15px;
		font-weight: 600;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.did {
		font-size: 11.5px;
		color: var(--text-faint);
		margin-top: 3px;
	}
	.dstatus {
		font-size: 11.5px;
		color: var(--text-faint);
		margin-top: 5px;
		display: flex;
		align-items: center;
		gap: 5px;
	}
	.dot {
		width: 7px;
		height: 7px;
		border-radius: 50%;
		background: var(--border-strong);
		flex: none;
	}
	.dot.on {
		background: var(--ok);
	}
	.actions {
		display: flex;
		align-items: center;
		gap: 6px;
		flex: none;
	}
	.connect {
		padding: 8px 14px;
		font-size: 13.5px;
	}
	.rm {
		width: 30px;
		height: 30px;
		border: 1px solid var(--border);
		background: var(--surface);
		border-radius: var(--r-sm);
		color: var(--text-faint);
		cursor: pointer;
		display: grid;
		place-items: center;
	}
	.rm:hover {
		color: var(--danger-text);
		border-color: var(--danger);
	}
</style>
