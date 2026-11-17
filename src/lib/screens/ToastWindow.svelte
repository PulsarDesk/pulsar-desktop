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

	// Call cards (`call-in` / `call-out` / `call-on`) carry their own buttons and stay until the
	// call moves on; the talk timer ticks while one is up.
	const isCall = (k: PulsarToast['kind']) => k === 'call-in' || k === 'call-out' || k === 'call-on';
	let now = $state(Date.now());
	$effect(() => {
		if (!list.some((n) => n.kind === 'call-on')) return;
		const timer = setInterval(() => (now = Date.now()), 1000);
		return () => clearInterval(timer);
	});
	const talk = (since = 0) => {
		const s = Math.max(0, Math.floor((now - since) / 1000));
		return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
	};
	const callAct = (id: number, a: 'accept' | 'decline' | 'hangup') => api.toastCall(id, a).catch(() => {});

	const icon = (k: PulsarToast['kind']) =>
		k.startsWith('call-')
			? k === 'call-missed' || k === 'call-declined' || k === 'call-noanswer' || k === 'call-ended'
				? 'phoneOff'
				: 'phone'
			: k === 'file-in'
			? 'download'
			: k === 'file-del'
				? 'x'
				: k === 'kbd-perm'
					? 'keyboard'
					: k === 'pad-off'
						? 'gaming'
						: k === 'input-off' || k === 'input-on'
							? 'keyboard'
							: k === 'chat-in'
								? 'chat'
								: 'edit';
</script>

<div class="stack">
	{#each list as n (n.id)}
		{#if isCall(n.kind)}
			<div class="card call" class:ringing={n.kind === 'call-in'} role="group" aria-label={t(`toast.${n.kind}`)}>
				<span class="ic callic" class:ring={n.kind === 'call-in'}><Icon name="phone" size={16} /></span>
				<div class="txt">
					<div class="ttl">{t(`toast.${n.kind}`)}{n.kind === 'call-on' ? ` · ${talk(n.since)}` : ''}</div>
					<div class="nm">{n.peer || t('home.remoteDevice')}</div>
				</div>
				{#if n.kind === 'call-in'}
					<button class="cb ok" aria-label={t('call.accept')} title={t('call.accept')} onclick={() => callAct(n.id, 'accept')}
						><Icon name="phone" size={16} /></button
					>
					<button class="cb no" aria-label={t('call.decline')} title={t('call.decline')} onclick={() => callAct(n.id, 'decline')}
						><Icon name="phoneOff" size={16} /></button
					>
				{:else}
					<button
						class="cb no"
						aria-label={t(n.kind === 'call-out' ? 'call.cancel' : 'call.hangup')}
						title={t(n.kind === 'call-out' ? 'call.cancel' : 'call.hangup')}
						onclick={() => callAct(n.id, 'hangup')}><Icon name="phoneOff" size={16} /></button
					>
				{/if}
			</div>
		{:else}
		<div
			class="card"
			role="button"
			tabindex="0"
			title={n.path
				? t('toast.reveal')
				: n.kind === 'kbd-perm'
					? t('toast.kbd-perm-open')
					: n.kind === 'pad-off' || n.kind === 'input-off' || n.kind === 'input-on'
						? n.name
						: n.kind === 'chat-in'
							? t('toast.chat-open')
							: undefined}
			onclick={() => api.toastOpen(n.id).catch(() => {})}
			onkeydown={(e) => {
				if (e.key === 'Enter' || e.key === ' ') api.toastOpen(n.id).catch(() => {});
			}}
		>
			<span class="ic" class:del={n.kind === 'file-del' || n.kind === 'pad-off' || n.kind === 'input-off' || n.kind === 'call-missed' || n.kind === 'call-declined' || n.kind === 'call-noanswer'}><Icon name={icon(n.kind)} size={16} /></span>
			<div class="txt">
				{#if n.kind === 'chat-in'}
					<div class="ttl">{n.peer || t('toast.chat-in')}</div>
					<div class="nm wrap">{n.name}</div>
				{:else if n.kind.startsWith('call-')}
					<div class="ttl">{t(`toast.${n.kind}`)}</div>
					<div class="nm">{n.peer || t('home.remoteDevice')}</div>
				{:else}
					<div class="ttl">{t(`toast.${n.kind}`)}</div>
					<div class="nm" class:wrap={n.kind === 'pad-off' || n.kind === 'input-off' || n.kind === 'input-on'}>{n.kind === 'kbd-perm' ? t('toast.kbd-perm-body') : n.name}</div>
					{#if n.peer}<div class="sub">{t('toast.from', { peer: n.peer })}</div>{/if}
				{/if}
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
		{/if}
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
	/* A sentence (the pad-off cause), not a file name: two lines, then an ellipsis. */
	.nm.wrap {
		font-size: 12px;
		font-weight: 500;
		white-space: normal;
		display: -webkit-box;
		-webkit-line-clamp: 2;
		line-clamp: 2;
		-webkit-box-orient: vertical;
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
	/* Call cards: an answer (green) / hang-up (red) round button, a pulsing icon while it rings. */
	.card.call {
		cursor: default;
		gap: 9px;
	}
	.card.call.ringing {
		border-color: oklch(0.72 0.15 150 / 0.75);
	}
	.ic.callic {
		background: oklch(0.38 0.09 150 / 0.55);
		color: oklch(0.86 0.12 150);
	}
	.ic.ring {
		animation: ring 1.2s ease-in-out infinite;
	}
	.cb {
		display: grid;
		place-items: center;
		width: 34px;
		height: 34px;
		border: 0;
		border-radius: 50%;
		cursor: pointer;
		flex: none;
		color: white;
	}
	.cb.ok {
		background: oklch(0.62 0.16 150);
	}
	.cb.ok:hover {
		background: oklch(0.68 0.17 150);
	}
	.cb.no {
		background: oklch(0.58 0.19 25);
	}
	.cb.no:hover {
		background: oklch(0.64 0.2 25);
	}
	@keyframes ring {
		0%,
		100% {
			transform: rotate(0);
		}
		10%,
		30% {
			transform: rotate(-14deg);
		}
		20%,
		40% {
			transform: rotate(14deg);
		}
		50% {
			transform: rotate(0);
		}
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
