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
	<button title={t('pip.menu')} aria-label={t('pip.menu')} onclick={onMenu}><Icon name="menu" size={15} /></button>
	<button title={t('pip.restore')} aria-label={t('pip.restore')} onclick={onRestore}><Icon name="expand" size={15} /></button>
	{#if onTogglePin}
		<button class:on={pinned} aria-pressed={pinned} title={pinned ? t('chrome.unpin') : t('chrome.pin')} aria-label={t('chrome.pin')} onclick={onTogglePin}>
			<Icon name="pin" size={15} />
		</button>
	{/if}
	<button class:off={!speakerOn} aria-pressed={speakerOn} title={t(speakerOn ? 'status.btn.speakerOn' : 'status.btn.speakerOff')} aria-label={t(speakerOn ? 'status.btn.speakerOn' : 'status.btn.speakerOff')} onclick={onSpeaker}>
		<Icon name="speaker" size={15} />
	</button>
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
	button.on {
		color: var(--accent);
	}
	button.off {
		opacity: 0.55;
	}
	button.end:hover {
		color: #fff;
		background: #d33;
	}
</style>
