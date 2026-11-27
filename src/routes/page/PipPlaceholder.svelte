<script lang="ts">
	// Picture-in-picture (issue #18): what a tab shows while its session runs in a small window
	// of its own. The tab keeps its place; "Bring back" returns the session here.
	import Icon from '#lib/Icon.svelte';
	import { t } from '#lib/i18n.svelte.js';

	let {
		name,
		pending = false,
		onRestore,
		onShow
	}: {
		name: string;
		/** The small window is still opening. */
		pending?: boolean;
		onRestore: () => void;
		onShow: () => void;
	} = $props();
</script>

<div class="pip-ph" role="status">
	<div class="card">
		<Icon name="pip" size={28} />
		<b>{t('pip.placeholder')}</b>
		<span class="selectable">{name}</span>
		<div class="row">
			<button class="btn btn-primary" disabled={pending} onclick={onRestore}>{t('pip.restore')}</button>
			<button class="btn btn-ghost" disabled={pending} onclick={onShow}>{t('pip.show')}</button>
		</div>
	</div>
</div>

<style>
	.pip-ph {
		position: absolute;
		inset: 0;
		display: grid;
		place-items: center;
		background: var(--bg);
	}
	.card {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 8px;
		padding: 24px 28px;
		border: 1px solid var(--border);
		border-radius: 12px;
		background: var(--surface);
		color: var(--text-muted);
		text-align: center;
		max-width: 80%;
	}
	b {
		color: var(--text);
		font-size: 14px;
	}
	span {
		font-size: 12.5px;
	}
	.row {
		display: flex;
		gap: 8px;
		margin-top: 6px;
	}
</style>
