<script lang="ts">
	// Gaming-mode connect pop-up: the ID box + on-screen numpad (type with a controller or
	// the keyboard, Enter/Bağlan to continue). It ONLY collects the host id — the fetched
	// games are then shown on a separate in-stage screen (GamesScreen), not in this pop-up.
	// Fully pad-navigable: the modal root carries `data-navmodal` so GamepadNav confines
	// focus to it, and it pushes its close handler so B / Esc dismiss the pop-up.
	import { onMount, onDestroy } from 'svelte';
	import Icon from '#lib/Icon.svelte';
	import Numpad from './Numpad.svelte';
	import { t } from '#lib/i18n.svelte.js';
	import { canConnectTarget, fmtTarget, guardIdSpaces, reformatInput } from '#lib/connectTarget.js';
	import { portal } from '#lib/portal.js';
	import { openModal, closeModal } from '#lib/overlayModals.svelte.js';
	import { closeOnEscape } from '#lib/escapeStack.js';
	import type { GamepadNav } from '#lib/gamepadNav.svelte.js';

	type Props = {
		nav: GamepadNav;
		/** Continue with the entered host id → the shell opens the games screen. */
		onPick: (id: string) => void;
		onClose: () => void;
		/** 'add' = the same ID box + keypad used to SAVE a device (gaming home → Devices). */
		mode?: 'connect' | 'add' | 'id';
		/** Pre-filled ID (editing an existing device's ID from the device form). */
		initial?: string;
	};
	let { nav, onPick, onClose, mode = 'connect', initial = '' }: Props = $props();
	const navItem = nav.item;

	let target = $state(fmtTarget((() => initial)()));
	let inputEl = $state<HTMLInputElement>();
	const canConnect = $derived(canConnectTarget(target));

	function setTarget(v: string) {
		target = fmtTarget(v);
	}
	function go() {
		if (!canConnect) return;
		const id = fmtTarget(target);
		onClose();
		onPick(id);
	}

	let unEscape = () => {};
	onMount(() => {
		unEscape = closeOnEscape(() => onClose());
		// Hide every pane's native render while this numpad is up, so a neighbour pane's live
		// video (composited over the webview on Linux) can't occlude it (split-mode #8).
		openModal();
		nav.pushBack(onClose);
		// Focus the ID box next frame (after items register) so the keyboard can type
		// immediately and controller focus is confined to the modal.
		requestAnimationFrame(() => {
			if (inputEl) nav.focus(inputEl);
			else nav.focusFirst();
		});
	});
	onDestroy(() => {
		unEscape();
		closeModal();
		nav.popBack(onClose);
		nav.focusFirst(); // return focus to the shell
	});
</script>

<div
	class="backdrop"
	use:portal
	role="presentation"
	onclick={(e) => {
		if (e.target === e.currentTarget) onClose();
	}}
>
	<div class="modal" data-navmodal role="dialog" aria-modal="true" aria-label={t(mode === 'id' ? 'gaming.idTitle' : mode === 'add' ? 'gaming.addTitle' : 'gaming.title')}>
		<button class="close" use:navItem onclick={onClose} aria-label={t('gaming.close')}>
			<Icon name="x" size={16} />
		</button>

		<div class="mhead">{t(mode === 'id' ? 'gaming.idTitle' : mode === 'add' ? 'gaming.addTitle' : 'gaming.title')}</div>
		<div class="idfield">
			<Icon name="connect" size={20} />
			<input
				bind:this={inputEl}
				use:navItem
				value={target}
				oninput={(e) => (target = reformatInput(e.currentTarget))}
				onkeydown={(e) => (e.key === 'Enter' ? go() : guardIdSpaces(e))}
				placeholder="000 000 000"
				aria-label={t('home.targetAria')}
				inputmode="numeric"
			/>
		</div>
		<div class="hint">{t('home.idOrIp')}</div>

		<Numpad value={target} setValue={setTarget} {navItem} />

		<button class="btn btn-primary go" use:navItem disabled={!canConnect} onclick={go}>
			<Icon name={mode === 'id' ? 'check' : mode === 'add' ? 'plus' : 'gaming'} size={18} />
			{t(mode === 'id' ? 'gaming.idDone' : mode === 'add' ? 'gaming.addDevice' : 'gaming.connect')}
		</button>
	</div>
</div>

<style>
	.backdrop {
		position: fixed;
		inset: 0;
		/* Above the shared Modal (70/71): the keypad can open over the device form. */
		z-index: 80;
		display: grid;
		place-items: center;
		background: color-mix(in oklch, var(--bg) 40%, transparent);
		backdrop-filter: blur(8px);
		padding: 24px;
	}
	.modal {
		position: relative;
		width: 100%;
		max-width: 380px;
		max-height: 90vh;
		overflow-y: auto;
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 14px;
		padding: 26px 24px 22px;
		background: var(--surface);
		border: 1px solid var(--border-strong);
		border-radius: var(--r-xl);
		box-shadow: var(--shadow-lg), var(--shadow-accent);
	}
	.close {
		position: absolute;
		top: 12px;
		right: 12px;
		width: 30px;
		height: 30px;
		display: grid;
		place-items: center;
		border: 1px solid var(--border);
		border-radius: 8px;
		background: var(--surface-2);
		color: var(--text-muted);
		cursor: pointer;
	}
	.close:hover {
		background: var(--surface-3);
		color: var(--text);
	}
	.mhead {
		font-family: var(--font-display);
		font-size: 19px;
		font-weight: 600;
		letter-spacing: -0.02em;
	}
	.idfield {
		display: flex;
		align-items: center;
		gap: 12px;
		width: 100%;
		padding: 13px 16px;
		background: var(--surface-2);
		border: 1px solid var(--border-strong);
		border-radius: var(--r-lg);
		color: var(--accent-fg);
	}
	.idfield input {
		flex: 1;
		min-width: 0;
		border: none;
		background: transparent;
		outline: none;
		color: var(--text);
		font-family: var(--font-mono);
		font-size: 26px;
		font-weight: 600;
		letter-spacing: 0.08em;
		text-align: center;
	}
	.hint {
		font-size: 12px;
		color: var(--text-faint);
		text-align: center;
	}
	.go {
		justify-content: center;
		width: 100%;
		padding: 13px;
		font-size: 16px;
	}
</style>
