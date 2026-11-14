<script lang="ts">
	// Pulsar's own notice stack (toast.rs opens this as a borderless, always-on-top window in
	// the bottom-right corner, sized to the list). Not the OS notification centre. A file
	// notice opens its folder with the file selected when clicked; × just dismisses it. Rust
	// owns the list and the timers — this only renders what it is sent.
	import { onMount } from 'svelte';
	import Icon from '#lib/Icon.svelte';
	import { api, onToasts } from '#lib/api.js';
	import { t } from '#lib/i18n.svelte.js';
	import type { PulsarToast } from '#lib/types.js';

	const injected = (window as unknown as { __TOAST__?: PulsarToast[] }).__TOAST__;
	let list = $state<PulsarToast[]>(Array.isArray(injected) ? injected : []);

	onMount(() => {
		// The window itself is transparent: only the cards are drawn.
		document.documentElement.style.background = 'transparent';
		document.body.style.background = 'transparent';
		let off: (() => void) | undefined;
		let dead = false;
		onToasts((l) => (list = l)).then((o) => {
			if (dead) o();
			else off = o;
		});
		api.toastList()
			.then((l) => (list = l))
			.catch(() => {});
		return () => {
			dead = true;
			off?.();
		};
	});

	const icon = (k: PulsarToast['kind']) =>
		k === 'file-in' ? 'download' : k === 'file-del' ? 'x' : k === 'kbd-perm' ? 'keyboard' : 'edit';
</script>

<div class="stack">
	{#each list as n (n.id)}
		<div
			class="card"
			role="button"
			tabindex="0"
			title={n.path ? t('toast.reveal') : n.kind === 'kbd-perm' ? t('toast.kbd-perm-open') : undefined}
			onclick={() => api.toastOpen(n.id).catch(() => {})}
			onkeydown={(e) => {
				if (e.key === 'Enter' || e.key === ' ') api.toastOpen(n.id).catch(() => {});
			}}
		>
			<span class="ic" class:del={n.kind === 'file-del'}><Icon name={icon(n.kind)} size={16} /></span>
			<div class="txt">
				<div class="ttl">{t(`toast.${n.kind}`)}</div>
				<div class="nm">{n.kind === 'kbd-perm' ? t('toast.kbd-perm-body') : n.name}</div>
				{#if n.peer}<div class="sub">{t('toast.from', { peer: n.peer })}</div>{/if}
			</div>
			<button
				class="x"
				aria-label={t('toast.close')}
				onclick={(e) => {
					e.stopPropagation();
					api.toastClose(n.id).catch(() => {});
				}}><Icon name="x" size={13} /></button
			>
		</div>
	{/each}
</div>

<style>
	/* Geometry mirrors toast.rs (64 px cards, 8 px gaps, 10 px padding, 340 px wide) so the
	   window Rust sizes fits the stack exactly. Newest at the bottom, nearest the corner. */
	.stack {
		position: fixed;
		inset: 0;
		padding: 10px;
		display: flex;
		flex-direction: column;
		justify-content: flex-end;
		gap: 8px;
		background: transparent;
		font-family: var(--font-body);
		user-select: none;
	}
	.card {
		height: 64px;
		box-sizing: border-box;
		display: flex;
		align-items: center;
		gap: 11px;
		padding: 0 10px 0 12px;
		border-radius: 11px;
		background: oklch(0.21 0.014 265 / 0.97);
		border: 1px solid oklch(0.62 0.17 280 / 0.45);
		color: oklch(0.93 0.008 265);
		cursor: pointer;
		animation: in 0.18s ease-out;
	}
	.card:hover {
		border-color: oklch(0.7 0.17 280 / 0.8);
		background: oklch(0.24 0.016 265 / 0.98);
	}
	.ic {
		display: grid;
		place-items: center;
		width: 32px;
		height: 32px;
		border-radius: 9px;
		background: oklch(0.36 0.09 280 / 0.6);
		color: oklch(0.86 0.09 280);
		flex: none;
	}
	.ic.del {
		background: oklch(0.36 0.09 25 / 0.55);
		color: oklch(0.85 0.09 25);
	}
	.txt {
		min-width: 0;
		flex: 1;
		line-height: 1.25;
	}
	.ttl {
		font-size: 11px;
		font-weight: 700;
		letter-spacing: 0.02em;
		color: oklch(0.78 0.1 280);
	}
	.nm {
		font-size: 13px;
		font-weight: 600;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.sub {
		font-size: 11px;
		color: oklch(0.68 0.02 265);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.x {
		display: grid;
		place-items: center;
		width: 24px;
		height: 24px;
		border: 0;
		border-radius: 6px;
		background: transparent;
		color: oklch(0.7 0.02 265);
		cursor: pointer;
		flex: none;
	}
	.x:hover {
		background: oklch(0.32 0.02 265);
		color: oklch(0.95 0.01 265);
	}
	@keyframes in {
		from {
			opacity: 0;
			transform: translateY(8px);
		}
		to {
			opacity: 1;
			transform: none;
		}
	}
</style>
