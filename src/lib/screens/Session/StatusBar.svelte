<script lang="ts" module>
	/** Fields the session status bar can show (Settings → Görüntü picks which, in this order). */
	export const STATUS_FIELDS = ['conn', 'res', 'fps', 'codec', 'encoder', 'decoder', 'latency', 'decode', 'bitrate', 'loss'] as const;
	export type StatusField = (typeof STATUS_FIELDS)[number];
	/** Height of the bar in CSS px — the video area shrinks by this much (nothing covers it). */
	export const STATUS_BAR_H = 26;
</script>

<script lang="ts">
	// Session status bar (issue #5): a thin strip UNDER the video with the live link facts —
	// P2P/relay, resolution, fps, codec, host encoder, client decoder, latency, decode time,
	// bitrate, loss. The video area is shortened by STATUS_BAR_H, so the bar never covers the
	// remote screen (the native renderer is positioned over `.screen` only). Optionally hosts
	// the Pulsar menu button (`menu`), which then replaces the one drawn on the video.
	import Icon from '#lib/Icon.svelte';
	import { t } from '#lib/i18n.svelte.js';

	let {
		fields,
		conn,
		res,
		fps,
		codec,
		encoder,
		decoder,
		latencyMs,
		decodeMs,
		mbps,
		lossPct,
		menu = false,
		onMenu
	}: {
		fields: readonly string[];
		conn: 'direct' | 'relay';
		res: string;
		fps: number;
		codec: string;
		encoder: string;
		decoder: string;
		latencyMs: number;
		decodeMs: number;
		mbps: number;
		lossPct: number;
		menu?: boolean;
		onMenu?: () => void;
	} = $props();

	const items = $derived.by(() => {
		const out: { key: string; label: string; text: string }[] = [];
		for (const f of STATUS_FIELDS) {
			if (!fields.includes(f)) continue;
			let text = '';
			switch (f) {
				case 'conn': text = conn === 'relay' ? 'Relay' : 'P2P'; break;
				case 'res': text = res; break;
				case 'fps': text = fps > 0 ? `${Math.round(fps)} fps` : ''; break;
				case 'codec': text = codec; break;
				case 'encoder': text = encoder; break;
				case 'decoder': text = decoder; break;
				case 'latency': text = latencyMs > 0 ? `${latencyMs} ms` : ''; break;
				case 'decode': text = decodeMs > 0 ? `${decodeMs.toFixed(1)} ms` : ''; break;
				case 'bitrate': text = mbps > 0 ? `${mbps.toFixed(1)} Mbps` : ''; break;
				case 'loss': text = `%${lossPct.toFixed(1)}`; break;
			}
			out.push({ key: f, label: t(`status.${f}`), text: text || '—' });
		}
		return out;
	});
</script>

<div class="sbar" style="height:{STATUS_BAR_H}px" role="status" aria-label={t('status.aria')}>
	<div class="items mono">
		{#each items as it (it.key)}
			<span class="item" title={it.label}><span class="k">{it.label}</span> {it.text}</span>
		{/each}
	</div>
	{#if menu}
		<button class="menu" aria-label={t('session.overlayBtn')} title={t('session.overlayBtn')} onclick={() => onMenu?.()}>
			<Icon name="menu" size={15} />
		</button>
	{/if}
</div>

<style>
	.sbar {
		position: absolute;
		left: 0;
		right: 0;
		bottom: 0;
		display: flex;
		align-items: center;
		gap: 10px;
		padding: 0 6px 0 10px;
		box-sizing: border-box;
		background: #101118;
		border-top: 1px solid oklch(1 0 0 / 0.08);
		color: oklch(0.86 0.02 260);
		font-size: 11.5px;
		user-select: none;
		overflow: hidden;
	}
	.items {
		flex: 1;
		min-width: 0;
		display: flex;
		align-items: center;
		gap: 14px;
		white-space: nowrap;
		overflow: hidden;
	}
	.item {
		flex-shrink: 0;
	}
	.k {
		color: oklch(0.62 0.02 260);
	}
	.menu {
		flex-shrink: 0;
		display: grid;
		place-items: center;
		width: 22px;
		height: 20px;
		padding: 0;
		border: none;
		border-radius: 5px;
		background: transparent;
		color: inherit;
		cursor: pointer;
	}
	.menu:hover {
		background: oklch(1 0 0 / 0.1);
	}
</style>
