<script lang="ts">
	// macOS host: the Screen Recording permission ScreenCaptureKit needs (video + system audio).
	// Shown when the OS reports it missing, or when a stream was refused for it
	// (`host-screen-permission`). Other systems answer "na" and never show it.
	import Icon from '#lib/Icon.svelte';
	import { api, onHostScreenPermission } from '#lib/api.js';
	import { t } from '#lib/i18n.svelte.js';

	/** 'asked' = the system dialog was shown; the grant needs a restart of Pulsar. */
	let perm = $state<'na' | 'granted' | 'denied' | 'asked'>('na');

	$effect(() => {
		let off: (() => void) | undefined;
		let gone = false;
		api
			.screenPermission(false)
			.then((s) => {
				if (!gone) perm = s;
			})
			.catch(() => {});
		onHostScreenPermission(() => {
			if (perm !== 'asked') perm = 'denied';
		}).then((o) => {
			if (gone) o();
			else off = o;
		});
		return () => {
			gone = true;
			off?.();
		};
	});

	async function ask() {
		try {
			perm = (await api.screenPermission(true)) === 'granted' ? 'granted' : 'asked';
		} catch {
			/* not macOS */
		}
	}

	function openSettings() {
		api.openScreenPermissionSettings().catch(() => {});
	}
</script>

{#if perm === 'denied' || perm === 'asked'}
	<div class="warn" role="alert">
		<Icon name="shield" size={15} />
		<div>
			<div class="warn-title">{t('home.screenPermTitle')}</div>
			<div class="warn-body">{perm === 'asked' ? t('home.screenPermRestart') : t('home.screenPermBody')}</div>
			<div class="actions">
				{#if perm === 'denied'}
					<button class="act" onclick={ask}>{t('home.screenPermAsk')}</button>
				{/if}
				<button class="act" onclick={openSettings}>{t('home.screenPermOpen')}</button>
			</div>
		</div>
	</div>
{/if}

<style>
	.warn {
		display: flex;
		align-items: flex-start;
		gap: 9px;
		margin-top: 12px;
		padding: 10px 12px;
		border-radius: var(--r-sm);
		background: color-mix(in oklch, var(--warn) 14%, var(--surface));
		border: 1px solid color-mix(in oklch, var(--warn) 45%, var(--border));
		color: color-mix(in oklch, var(--warn) 65%, var(--text));
	}
	.warn :global(svg) {
		flex: none;
		margin-top: 1px;
	}
	.warn-title {
		font-size: 12.5px;
		font-weight: 700;
	}
	.warn-body {
		font-size: 12px;
		line-height: 1.45;
		margin-top: 2px;
		color: var(--text-muted);
	}
	.actions {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		margin-top: 8px;
	}
	.act {
		font: inherit;
		font-size: 12px;
		font-weight: 600;
		padding: 4px 10px;
		border-radius: var(--r-sm);
		border: 1px solid color-mix(in oklch, var(--warn) 45%, var(--border));
		background: var(--surface);
		color: var(--text);
		cursor: pointer;
	}
	.act:hover {
		background: color-mix(in oklch, var(--warn) 10%, var(--surface));
	}
</style>
