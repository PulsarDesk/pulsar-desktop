<script lang="ts" module>
	/** Fields the session status bar can show (Settings → Görüntü picks which, in this order). */
	export const STATUS_FIELDS = ['conn', 'res', 'fps', 'codec', 'encoder', 'decoder', 'latency', 'decode', 'bitrate', 'loss'] as const;
	export type StatusField = (typeof STATUS_FIELDS)[number];
	/** Quick audio toggles the bar can carry (issue #9): host audio here, my mic to the host,
	 * the host's own speakers. Same state as the session menu's Ses toggles. */
	export const STATUS_BUTTONS = ['speaker', 'mic', 'hostmute'] as const;
	export type StatusButton = (typeof STATUS_BUTTONS)[number];
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
		onMenu,
		buttons = [],
		speakerOn = true,
		micOn = false,
		micAvailable = false,
		hostMuted = false,
		onSpeaker,
		onMic,
		onHostMute
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
		buttons?: readonly string[];
		speakerOn?: boolean;
		micOn?: boolean;
		/** The mic side channel exists here (remote mode). */
		micAvailable?: boolean;
		hostMuted?: boolean;
		onSpeaker?: () => void;
		onMic?: () => void;
		onHostMute?: () => void;
	} = $props();

	type Btn = { key: string; icon: string; on: boolean; label: string; run?: () => void };
	const quick = $derived.by(() => {
		const out: Btn[] = [];
		for (const b of STATUS_BUTTONS) {
			if (!buttons.includes(b)) continue;
			if (b === 'speaker')
				out.push({ key: b, icon: 'speaker', on: speakerOn, label: t(speakerOn ? 'status.btn.speakerOn' : 'status.btn.speakerOff'), run: onSpeaker });
			else if (b === 'mic' && micAvailable)
				out.push({ key: b, icon: 'mic', on: micOn, label: t(micOn ? 'status.btn.micOn' : 'status.btn.micOff'), run: onMic });
			// "on" = the host's speakers play (not muted).
			else if (b === 'hostmute')
				out.push({ key: b, icon: 'monitor', on: !hostMuted, label: t(hostMuted ? 'status.btn.hostMuted' : 'status.btn.hostPlays'), run: onHostMute });
		}
		return out;
	});

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
	{#each quick as b (b.key)}
		<button class="qbtn" class:off={!b.on} aria-pressed={b.on} aria-label={b.label} title={b.label} onclick={() => b.run?.()}>
			<Icon name={b.icon} size={14} />
			{#if !b.on}<span class="slash" aria-hidden="true"></span>{/if}
		</button>
	{/each}
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
	.qbtn {
		position: relative;
		flex-shrink: 0;
		display: grid;
		place-items: center;
		width: 24px;
		height: 20px;
		padding: 0;
		border: none;
		border-radius: 5px;
		background: transparent;
		color: inherit;
		cursor: pointer;
	}
	.qbtn:hover {
		background: oklch(1 0 0 / 0.1);
	}
	/* Off: red-tinted and crossed out. */
	.qbtn.off {
		color: oklch(0.68 0.16 25);
	}
	.slash {
		position: absolute;
		left: 4px;
		right: 4px;
		top: 50%;
		height: 1.6px;
		background: currentColor;
		transform: rotate(-40deg);
		border-radius: 1px;
	}
</style>
