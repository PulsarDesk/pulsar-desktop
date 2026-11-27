<script lang="ts">
	// Picture-in-picture (issue #18): the small window's controls, under the video while it is
	// grown (hover / in use). Its empty part drags the window.
	import Icon from '#lib/Icon.svelte';
	import { t } from '#lib/i18n.svelte.js';
	import { PIP_BAR_H } from './pipWindow.svelte';

	let {
		active,
		pinned,
		speakerOn,
		unread = 0,
		monitors = [],
		monitor = 0,
		onMonitor,
		onMenu,
		onRestore,
		onTogglePin,
		onSpeaker,
		onEnd
	}: {
		/** Input goes to the host (clicked into). */
		active: boolean;
		pinned: boolean;
		speakerOn: boolean;
		/** Chat lines waiting (badge on the menu button). */
		unread?: number;
		/** The host's monitors (a picker shows when there are several) and the streamed one. */
		monitors?: import('#lib/api.types.js').HostDisplay[];
		monitor?: number;
		onMonitor?: (idx: number) => void;
		onMenu: () => void;
		onRestore: () => void;
		/** Absent = keep-on-top is off in Settings (no pin here either). */
		onTogglePin?: () => void;
		onSpeaker: () => void;
		onEnd: () => void;
	} = $props();
</script>

<div class="pipbar" style="height:{PIP_BAR_H}px" data-tauri-drag-region>
	<span class="state" class:on={active} title={active ? t('pip.active') : t('pip.passive')} aria-label={active ? t('pip.active') : t('pip.passive')}></span>
	<button class="menu" title={unread ? t('pip.unread', { n: unread }) : t('pip.menu')} aria-label={t('pip.menu')} onclick={onMenu}>
		<Icon name="menu" size={15} />
		{#if unread}<span class="badge">{unread > 9 ? '9+' : unread}</span>{/if}
	</button>
	<button title={t('pip.restore')} aria-label={t('pip.restore')} onclick={onRestore}><Icon name="expand" size={15} /></button>
	{#if onTogglePin}
		<button class:on={pinned} aria-pressed={pinned} title={pinned ? t('chrome.unpin') : t('chrome.pin')} aria-label={t('chrome.pin')} onclick={onTogglePin}>
			<Icon name="pin" size={15} />
		</button>
	{/if}
	<button class:off={!speakerOn} aria-pressed={speakerOn} title={t(speakerOn ? 'status.btn.speakerOn' : 'status.btn.speakerOff')} aria-label={t(speakerOn ? 'status.btn.speakerOn' : 'status.btn.speakerOff')} onclick={onSpeaker}>
		<Icon name="speaker" size={15} />
	</button>
	{#if monitors.length > 1 && onMonitor}
		<select
			class="mon"
			title={t('pip.monitor')}
			aria-label={t('pip.monitor')}
			value={monitor}
			onchange={(e) => onMonitor?.(Number(e.currentTarget.value))}
		>
			{#each monitors as m, i (i)}
				<option value={i}>{i + 1}{m.primary ? ' ★' : ''} · {m.width}×{m.height}</option>
			{/each}
		</select>
	{/if}
	<span class="grow" data-tauri-drag-region></span>
	<button class="end" title={t('pip.end')} aria-label={t('pip.end')} onclick={onEnd}><Icon name="x" size={15} /></button>
</div>

<style>
	.pipbar {
		flex: none;
		display: flex;
		align-items: center;
		gap: 2px;
		padding: 0 6px;
		background: var(--surface);
		border-top: 1px solid var(--border);
		user-select: none;
	}
	.state {
		width: 8px;
		height: 8px;
		border-radius: 50%;
		margin: 0 6px 0 2px;
		background: var(--text-faint);
	}
	.state.on {
		background: var(--accent);
	}
	.grow {
		flex: 1;
		align-self: stretch;
	}
	button {
		display: grid;
		place-items: center;
		width: 28px;
		height: 26px;
		border: 0;
		border-radius: 6px;
		background: transparent;
		color: var(--text-muted);
		cursor: pointer;
	}
	button:hover {
		background: var(--surface-2, rgba(127, 127, 127, 0.15));
		color: var(--text);
	}
	button.on,
	button.on:hover {
		color: var(--accent);
		background: color-mix(in srgb, var(--accent) 16%, transparent);
	}
	button.off {
		opacity: 0.55;
	}
	.mon {
		height: 24px;
		max-width: 130px;
		margin-left: 4px;
		border: 1px solid var(--border);
		border-radius: 6px;
		background: var(--surface);
		color: var(--text-muted);
		font-size: 11.5px;
	}
	button.menu {
		position: relative;
	}
	.badge {
		position: absolute;
		top: 0;
		right: -2px;
		min-width: 14px;
		height: 14px;
		padding: 0 3px;
		border-radius: 7px;
		background: var(--accent);
		color: var(--accent-fg, #fff);
		font-size: 9.5px;
		line-height: 14px;
		font-weight: 700;
	}
	button.end:hover {
		color: #fff;
		background: #d33;
	}
</style>
