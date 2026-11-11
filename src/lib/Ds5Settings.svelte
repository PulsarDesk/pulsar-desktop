<script lang="ts">
	// DualSense passthrough settings (part of the controllers area): the master switch,
	// per-feature Otomatik / Açık / Kapalı, automatic feature shedding on a weak Bluetooth
	// link, and the advanced jitter buffer. Every change is saved locally and pushed to Rust
	// (`set_ds5_settings`), which applies it live to running pads.
	import { api } from '#lib/api.js';
	import { t } from '#lib/i18n.svelte.js';
	import { ui, saveUi, DS5_FEATURES, type Ds5Mode } from '#lib/settings.svelte.js';
	import type { Action } from 'svelte/action';

	const noopAction: Action<HTMLElement> = () => {};
	let { navItem = noopAction }: { navItem?: Action<HTMLElement> } = $props();

	const MODES: { v: Ds5Mode; k: string }[] = [
		{ v: 'auto', k: 'ds5.modeAuto' },
		{ v: 'on', k: 'ds5.modeOn' },
		{ v: 'off', k: 'ds5.modeOff' }
	];

	let advanced = $state(false);

	function push() {
		saveUi();
		api.setDs5Settings($state.snapshot(ui.ds5)).catch(() => {});
	}

	function setMode(key: (typeof DS5_FEATURES)[number]['key'], v: Ds5Mode) {
		ui.ds5[key] = v;
		push();
	}

	function setJitter(e: Event) {
		const n = Math.round(Number((e.currentTarget as HTMLInputElement).value));
		ui.ds5.jitterMs = Number.isFinite(n) ? Math.min(500, Math.max(11, n)) : 64;
		push();
	}
</script>

<section class="ds5" aria-labelledby="ds5-title">
	<div class="row head">
		<div class="txt">
			<b id="ds5-title">{t('ds5.enabled')}</b>
			<span>{t('ds5.enabledDesc')}</span>
		</div>
		<button
			class="toggle"
			class:on={ui.ds5.enabled}
			use:navItem
			aria-label={t('ds5.enabled')}
			aria-pressed={ui.ds5.enabled}
			onclick={() => {
				ui.ds5.enabled = !ui.ds5.enabled;
				push();
			}}><span class="knob"></span></button
		>
	</div>

	<div class="body" class:off={!ui.ds5.enabled}>
		<p class="hint">{t('ds5.modesHint')}</p>
		<ul class="feats">
			{#each DS5_FEATURES as f (f.key)}
				<li class="seg-row">
					<span class="seg-lab">{t(f.label)}</span>
					<div class="seg" role="group" aria-label={t(f.label)}>
						{#each MODES as m (m.v)}
							{@const on = ui.ds5[f.key] === m.v}
							<button
								class="seg-btn"
								class:on
								use:navItem
								disabled={!ui.ds5.enabled}
								aria-pressed={on}
								onclick={() => setMode(f.key, m.v)}>{t(m.k)}</button
							>
						{/each}
					</div>
				</li>
			{/each}
		</ul>
		<p class="hint">{t('ds5.headsetDesc')}</p>

		<div class="row">
			<div class="txt">
				<b>{t('ds5.autoDegrade')}</b>
				<span>{t('ds5.autoDegradeDesc')}</span>
			</div>
			<button
				class="toggle"
				class:on={ui.ds5.autoDegrade}
				use:navItem
				disabled={!ui.ds5.enabled}
				aria-label={t('ds5.autoDegrade')}
				aria-pressed={ui.ds5.autoDegrade}
				onclick={() => {
					ui.ds5.autoDegrade = !ui.ds5.autoDegrade;
					push();
				}}><span class="knob"></span></button
			>
		</div>

		<button
			class="adv-btn"
			use:navItem
			aria-expanded={advanced}
			onclick={() => (advanced = !advanced)}>{advanced ? '▾' : '▸'} {t('ds5.advanced')}</button
		>
		{#if advanced}
			<div class="row">
				<div class="txt">
					<label for="ds5-jitter"><b>{t('ds5.jitter')}</b></label>
					<span>{t('ds5.jitterDesc')}</span>
				</div>
				<input
					id="ds5-jitter"
					class="num mono"
					type="number"
					min="11"
					max="500"
					step="1"
					disabled={!ui.ds5.enabled}
					value={ui.ds5.jitterMs}
					onchange={setJitter}
				/>
			</div>
		{/if}
	</div>
</section>

<style>
	.ds5 {
		margin-top: 12px;
		padding: 12px;
		border: 1px solid var(--border);
		border-radius: var(--r-sm, 6px);
		background: var(--surface-2, var(--surface));
		display: flex;
		flex-direction: column;
		gap: 10px;
	}
	.row {
		display: flex;
		align-items: center;
		gap: 12px;
	}
	.txt {
		display: flex;
		flex-direction: column;
		gap: 2px;
		flex: 1;
		min-width: 0;
	}
	.txt b {
		font-size: 13px;
		font-weight: 600;
	}
	.txt span {
		font-size: 11.5px;
		color: var(--text-faint);
	}
	.toggle {
		flex: none;
	}
	.body {
		display: flex;
		flex-direction: column;
		gap: 10px;
	}
	.body.off {
		opacity: 0.55;
	}
	.hint {
		font-size: 11.5px;
		color: var(--text-faint);
		margin: 0;
	}
	.feats {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.seg-row {
		display: flex;
		align-items: center;
		gap: 10px;
	}
	.seg-lab {
		font-size: 12px;
		color: var(--text-muted);
		font-weight: 600;
	}
	.seg {
		display: flex;
		gap: 4px;
		margin-left: auto;
	}
	.seg-btn {
		font-size: 11.5px;
		font-family: inherit;
		color: var(--text-muted);
		background: var(--surface-3, var(--surface));
		border: 1px solid var(--border-strong, var(--border));
		border-radius: var(--r-sm, 4px);
		padding: 3px 9px;
		cursor: pointer;
		transition: all var(--dur) var(--ease);
	}
	.seg-btn:hover:not(:disabled) {
		color: var(--text);
		background: var(--accent-soft);
	}
	.seg-btn.on {
		color: var(--text-on-accent, #fff);
		background: var(--accent);
		border-color: var(--accent);
	}
	.seg-btn:disabled,
	.toggle:disabled {
		cursor: default;
	}
	.adv-btn {
		align-self: flex-start;
		font-size: 11.5px;
		font-family: inherit;
		color: var(--text-muted);
		background: none;
		border: none;
		padding: 0;
		cursor: pointer;
	}
	.adv-btn:hover {
		color: var(--text);
	}
	.num {
		width: 7ch;
		font-size: 12px;
		padding: 4px 6px;
		color: var(--text);
		background: var(--surface);
		border: 1px solid var(--border-strong, var(--border));
		border-radius: var(--r-sm, 4px);
	}
</style>
