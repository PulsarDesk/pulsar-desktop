<script lang="ts">
	// Settings → Güvenlik → two-factor auth for incoming connections (RustDesk-style TOTP).
	// On = `config.totp_secret` is set. Turning it on opens the enrollment: a QR + manual code
	// for an authenticator app, then one code to prove it; only a verified secret is saved.
	// A connection a password or unattended access lets in must then also send the current
	// code; one the host user approves on the Allow/Deny popup does not (host.rs).
	import Icon from '#lib/Icon.svelte';
	import { t } from '#lib/i18n.svelte.js';
	import { api } from '#lib/api.js';
	import { closeOnEscape } from '#lib/escapeStack.js';
	import type { Config } from '#lib/types.js';
	import type { TotpSetup } from '#lib/api.types.js';

	let { config = $bindable(), saveConfig }: { config?: Config | null; saveConfig?: () => void } = $props();

	const on = $derived(!!config?.totp_secret);
	let setup = $state<TotpSetup | null>(null);
	let code = $state('');
	let err = $state('');
	let checking = $state(false);

	async function toggle() {
		if (!config) return;
		if (on) {
			config.totp_secret = '';
			saveConfig?.();
			return;
		}
		err = '';
		code = '';
		setup = await api.totpBegin().catch(() => null);
	}

	async function verify() {
		if (!setup || !config || checking) return;
		checking = true;
		const ok = await api.totpVerify(setup.secret, code).catch(() => false);
		checking = false;
		if (!ok) {
			err = t('settings.twofaWrong');
			return;
		}
		config.totp_secret = setup.secret;
		saveConfig?.();
		setup = null;
	}

	$effect(() => {
		if (setup) return closeOnEscape(() => (setup = null));
	});
</script>

<div class="srow">
	<div class="st"><b>{t('settings.twofa')}</b><span>{t('settings.twofaDesc')}</span></div>
	<button class="toggle" aria-label={t('settings.twofa')} class:on aria-pressed={on} onclick={toggle}><span class="knob"></span></button>
</div>

{#if setup}
	<div class="tfmodal" role="dialog" aria-modal="true" aria-label={t('settings.twofaSetup')}>
		<div class="tfcard">
			<div class="tfhdr"><Icon name="shield" size={18} /><span>{t('settings.twofaSetup')}</span></div>
			<p class="tflead">{t('settings.twofaScan')}</p>
			<!-- The QR is SVG rendered app-side (pulsar-relay `generate_totp`) from our own secret. -->
			<!-- eslint-disable-next-line svelte/no-at-html-tags -->
			<div class="tfqr">{@html setup.qrSvg}</div>
			<div class="tfmanual">
				<span>{t('settings.twofaManual')}</span>
				<code class="mono selectable">{setup.secretGrouped}</code>
			</div>
			<p class="tflead">{t('settings.twofaEnter')}</p>
			{#if err}<div class="tferr">{err}</div>{/if}
			<!-- svelte-ignore a11y_autofocus -->
			<input
				class="tfcode mono"
				type="text"
				inputmode="numeric"
				maxlength="6"
				placeholder="000000"
				aria-label={t('settings.twofaEnter')}
				bind:value={code}
				onkeydown={(e) => e.key === 'Enter' && verify()}
				autofocus
			/>
			<div class="tfact">
				<button class="tfbtn ghost" onclick={() => (setup = null)}>{t('pw.cancel')}</button>
				<button class="tfbtn primary" disabled={checking || code.trim().length !== 6} onclick={verify}>{t('settings.twofaEnable')}</button>
			</div>
		</div>
	</div>
{/if}

<style>
	.tfmodal {
		position: fixed;
		inset: 0;
		z-index: 40;
		display: grid;
		place-items: center;
		background: oklch(0.2 0.01 265 / 0.45);
		backdrop-filter: blur(3px);
	}
	.tfcard {
		width: 360px;
		max-width: calc(100% - 32px);
		max-height: calc(100% - 32px);
		overflow-y: auto;
		background: var(--surface);
		border: 1px solid var(--border);
		border-radius: var(--r);
		box-shadow: var(--shadow-lg);
		padding: 20px;
	}
	.tfhdr {
		display: flex;
		align-items: center;
		gap: 9px;
		font-size: 16px;
		font-weight: 700;
	}
	.tflead {
		margin: 12px 0 0;
		font-size: 13px;
		line-height: 1.5;
		color: var(--text-muted);
	}
	.tfqr {
		margin: 14px auto 0;
		width: 180px;
		padding: 10px;
		background: #fff;
		border-radius: var(--r-sm);
	}
	.tfqr :global(svg) {
		display: block;
		width: 100%;
		height: auto;
	}
	.tfmanual {
		margin-top: 12px;
		display: flex;
		flex-direction: column;
		gap: 4px;
		font-size: 12px;
		color: var(--text-faint);
	}
	.tfmanual code {
		font-size: 13px;
		color: var(--text);
		word-break: break-all;
	}
	.tferr {
		margin-top: 10px;
		font-size: 12.5px;
		color: var(--danger-text);
	}
	.tfcode {
		margin-top: 10px;
		width: 100%;
		box-sizing: border-box;
		padding: 10px;
		font-size: 20px;
		letter-spacing: 0.35em;
		text-align: center;
		background: var(--surface-2);
		border: 1px solid var(--border);
		border-radius: var(--r-sm);
		color: var(--text);
	}
	.tfact {
		display: flex;
		justify-content: flex-end;
		gap: 10px;
		margin-top: 16px;
	}
	.tfbtn {
		padding: 8px 16px;
		font-size: 13px;
		font-weight: 600;
		border-radius: var(--r-sm);
		cursor: pointer;
		border: 1px solid var(--border);
	}
	.tfbtn.ghost {
		background: var(--surface-2);
		color: var(--text-muted);
	}
	.tfbtn.primary {
		background: var(--accent);
		border-color: var(--accent);
		color: #fff;
	}
	.tfbtn:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
</style>
