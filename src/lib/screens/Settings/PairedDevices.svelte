<script lang="ts">
	// Settings → Güvenlik → Eşleşmiş cihazlar: the clients this HOST trusts by key (they
	// connect without a password or the Allow/Deny popup; 2FA still applies). Removing one
	// makes it ask for a password again.
	import { onMount } from 'svelte';
	import Icon from '#lib/Icon.svelte';
	import { api, onPairedDevicesChanged, type PairedDevice } from '#lib/api.js';
	import { t, i18n } from '#lib/i18n.svelte.js';

	let list = $state<PairedDevice[]>([]);
	let loaded = $state(false);

	async function refresh() {
		try {
			list = await api.listPairedDevices();
		} catch {
			/* keep the last list */
		}
		loaded = true;
	}

	onMount(() => {
		refresh();
		let off: (() => void) | null = null;
		let dead = false;
		onPairedDevicesChanged(refresh).then((u) => (dead ? u() : (off = u)));
		return () => {
			dead = true;
			off?.();
		};
	});

	async function revoke(d: PairedDevice) {
		await api.revokePairedDevice(d.key).catch(() => false);
		await refresh();
	}

	const fmt = (ms: number) => (ms ? new Date(ms).toLocaleString(i18n.lang) : '—');
</script>

<div class="srow col">
	<div class="st"><b>{t('settings.paired')}</b><span>{t('settings.pairedDesc')}</span></div>
	{#if loaded && list.length === 0}
		<p class="empty">{t('settings.pairedEmpty')}</p>
	{:else}
		<ul class="plist">
			{#each list as d (d.key)}
				<li>
					<div class="pic"><Icon name="link" size={16} /></div>
					<div class="who">
						<b>{d.name || d.id}</b>
						<span class="mono selectable">{d.id} · {d.fingerprint}</span>
						<span>
							{t('settings.pairedSince', { d: fmt(d.pairedAt) })}
							{#if d.lastSeen}· {t('settings.pairedLastSeen', { d: fmt(d.lastSeen) })}{/if}
						</span>
					</div>
					<button class="btn btn-ghost" aria-label={`${t('settings.pairedRevoke')}: ${d.name || d.id}`} onclick={() => revoke(d)}>
						{t('settings.pairedRevoke')}
					</button>
				</li>
			{/each}
		</ul>
	{/if}
</div>

<style>
	.srow.col {
		display: flex;
		flex-direction: column;
		align-items: stretch;
		gap: 10px;
		padding: 16px 0;
		border-bottom: 1px solid var(--border);
	}
	.st b {
		font-size: 14px;
		font-weight: 600;
		display: block;
	}
	.st span {
		font-size: 12.5px;
		color: var(--text-faint);
		margin-top: 3px;
		line-height: 1.45;
		display: block;
		max-width: 60ch;
	}
	.empty {
		margin: 0;
		font-size: 12.5px;
		color: var(--text-faint);
	}
	.plist {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 8px;
	}
	.plist li {
		display: flex;
		align-items: center;
		gap: 12px;
		padding: 10px 12px;
		border: 1px solid var(--border);
		border-radius: 10px;
		background: var(--surface);
	}
	.pic {
		width: 32px;
		height: 32px;
		border-radius: 8px;
		display: grid;
		place-items: center;
		background: var(--accent-soft);
		color: var(--accent-fg);
		flex: none;
	}
	.who {
		flex: 1;
		min-width: 0;
		display: flex;
		flex-direction: column;
		gap: 2px;
	}
	.who b {
		font-size: 13.5px;
		font-weight: 600;
	}
	.who span {
		font-size: 11.5px;
		color: var(--text-faint);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.mono {
		font-family: var(--font-mono);
	}
</style>
