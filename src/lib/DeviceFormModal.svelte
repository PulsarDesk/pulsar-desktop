<script lang="ts">
	// Add / edit a saved device (name + ID-or-IP + picture + saved password, and pairing when
	// editing). ONE dialog for both shells:
	// the remote Devices screen and the gaming home's Devices section open this same modal.
	import Icon from '#lib/Icon.svelte';
	import Modal from '#lib/Modal.svelte';
	import { addPeer, updatePeer, normalizeId, fmtPeerId } from '#lib/peers.svelte.js';
	import { t } from '#lib/i18n.svelte.js';
	import { api } from '#lib/api.js';
	import { authDone } from '#lib/authDone.js';
	import type { Action } from 'svelte/action';

	type Props = {
		/** The device being edited (its ORIGINAL id), or null to add a new one. */
		device: { id: string; name: string; image?: string } | null;
		onClose: () => void;
		/** Optional small print under the form (gaming mode: editing needs keyboard/mouse). */
		note?: string;
		/** Gaming mode: register the controls with the pad nav (and confine it to this dialog). */
		navItem?: Action<HTMLElement>;
		/** Gaming mode: the ID field opens the on-screen ID keypad instead of taking typing;
		 * call `set` with the picked ID. */
		pickId?: (current: string, set: (id: string) => void) => void;
	};
	const noop: Action<HTMLElement> = () => {};
	let { device, onClose, note = '', navItem = noop, pickId }: Props = $props();

	const ICONS = ['monitor', 'devices', 'gaming', 'keyboard'] as const;
	// Snapshot of the device at open time; the form edits local copies.
	const start = (() => device)();
	let newName = $state(start?.name ?? '');
	let newId = $state(start ? fmtPeerId(start.id) : '');
	let newImage = $state(start?.image ?? 'icon:monitor');
	let fileInput = $state<HTMLInputElement | null>(null);

	// The device's custom connect password. Kept by the Rust side (DPAPI-sealed on Windows),
	// never in localStorage — loaded for the edit dialog, saved on submit, sent up front on
	// connect so a matching password never opens the prompt.
	let newPw = $state('');
	let savedPw = '';
	let showPw = $state(false);
	// Pairing with the device (edit dialog only: it needs a known target).
	let paired = $state(false);
	let pairBusy = $state(false);
	let pairMsg = $state('');
	let pairErr = $state(false);
	if (start) {
		api
			.getDevicePassword(start.id)
			.then((pw) => {
				// Don't clobber what the user already typed while this loaded.
				if (!newPw) newPw = pw;
				savedPw = pw;
			})
			.catch(() => {});
		api
			.deviceCreds([start.id])
			.then((m) => (paired = !!m[start.id]?.paired))
			.catch(() => {});
	}

	/** Persist the password field for `id` (only when it changed). */
	async function savePassword(id: string) {
		if (newPw === savedPw && start && id === start.id) return;
		await api.setDevicePassword(id, newPw);
		savedPw = newPw;
	}

	async function togglePair() {
		if (!start || pairBusy) return;
		pairBusy = true;
		pairErr = false;
		pairMsg = paired ? '' : t('devices.pairBusy');
		try {
			if (paired) {
				const remote = await api.unpairDevice(start.id);
				paired = false;
				pairMsg = remote ? t('devices.unpairOk') : t('devices.unpairLocal');
				pairErr = !remote;
			} else {
				// Pair with what the field says: the password typed here authenticates the
				// pairing connect (no prompt when it matches).
				if (normalizeId(newId) === start.id) await savePassword(start.id).catch(() => {});
				const r = await api.pairDevice(start.id);
				paired = r.paired;
				pairErr = !r.paired;
				pairMsg = r.paired
					? t('devices.pairOk')
					: r.reason === 'denied'
						? t('devices.pairDenied')
						: r.reason === 'timeout'
							? t('devices.pairTimeout')
							: r.reason === 'unsupported'
								? t('devices.pairUnsupported')
								: t('devices.pairFailed', { e: r.reason });
			}
		} catch (e) {
			pairErr = true;
			pairMsg = t('devices.pairFailed', { e: String(e) });
		} finally {
			authDone(start.id);
			pairBusy = false;
		}
	}

	// Display id input as-is (IPs welcome); an all-digit relay id is capped at its 9 digits
	// and grouped (a 10th digit used to drop it out of the id form, ungrouped and unlimited).
	const tidyId = (v: string) => {
		const despaced = v.replace(/\s/g, '');
		return /^\d+$/.test(despaced)
			? despaced
					.slice(0, 9)
					.replace(/(\d{3})(?=\d)/g, '$1 ')
					.trim()
			: v.trim();
	};

	// Uploaded picture → small cover-cropped data URL (96px) so the persisted
	// store never carries multi-MB originals.
	function onPickImage(e: Event) {
		const file = (e.currentTarget as HTMLInputElement).files?.[0];
		(e.currentTarget as HTMLInputElement).value = '';
		if (!file) return;
		const url = URL.createObjectURL(file);
		const img = new Image();
		img.onload = () => {
			const S = 96;
			const c = document.createElement('canvas');
			c.width = S;
			c.height = S;
			const g = c.getContext('2d');
			if (g) {
				const scale = Math.max(S / img.width, S / img.height);
				const w = img.width * scale;
				const h = img.height * scale;
				g.drawImage(img, (S - w) / 2, (S - h) / 2, w, h);
				newImage = c.toDataURL('image/jpeg', 0.82);
			}
			URL.revokeObjectURL(url);
		};
		img.onerror = () => URL.revokeObjectURL(url);
		img.src = url;
	}

	function submit() {
		const id = normalizeId(newId);
		if (!id) return;
		if (start) {
			updatePeer(start.id, { name: newName, newId: id, image: newImage });
			// A new id is another device: what was saved for the old one goes.
			if (id !== start.id) api.forgetDeviceCreds(start.id).catch(() => {});
		} else {
			addPeer(newName.trim() || t('devices.defaultName'), id, 'pc', newImage);
		}
		savePassword(id).catch(() => {});
		onClose();
	}
</script>

<Modal title={start ? t('devices.editTitle') : t('devices.add')} {onClose} navModal={navItem !== noop}>
	<div class="addform">
		<div class="addgrid">
			<!-- Left: the device's picture. The big tile previews the pick and opens the file
			     picker; the chips below swap between the built-in icons. -->
			<div class="imgcol">
				<button
					use:navItem
					class="preview"
					class:photo={!newImage.startsWith('icon:')}
					onclick={() => fileInput?.click()}
					title={t('devices.imageHint')}
					aria-label={t('devices.imageHint')}
				>
					{#if newImage.startsWith('icon:')}
						<Icon name={newImage.slice(5)} size={44} />
					{:else}
						<img class="uimg" src={newImage} alt="" />
					{/if}
					<span class="pv-up"><Icon name="edit" size={14} /></span>
				</button>
				<div class="imgrow" role="group" aria-label={t('devices.image')}>
					{#each ICONS as ic (ic)}
						<button
							use:navItem
							class="imgopt"
							class:active={newImage === 'icon:' + ic}
							aria-label={ic}
							aria-pressed={newImage === 'icon:' + ic}
							onclick={() => (newImage = 'icon:' + ic)}><Icon name={ic} size={17} /></button
						>
					{/each}
				</div>
				<input type="file" accept="image/*" bind:this={fileInput} onchange={onPickImage} style="display:none" />
			</div>

			<!-- Right: what the device is called and how to reach it. -->
			<div class="fieldcol">
				<label class="fl" for="dev-name">{t('devices.name')}</label>
				<div class="field">
					<input use:navItem id="dev-name" bind:value={newName} placeholder={t('devices.name')} aria-label={t('devices.name')} />
				</div>
				<label class="fl" for="dev-id">{t('devices.idOrIp')}</label>
				<div class="field">
					<Icon name="connect" size={15} />
					<input
						use:navItem
						id="dev-id"
						value={newId}
						readonly={!!pickId}
						onclick={() => pickId?.(newId, (id) => (newId = tidyId(id)))}
						oninput={(e) => {
							// Write back too: at the 9-digit cap `newId` doesn't change, so Svelte wouldn't
							// touch the field and the extra digit would stay on screen.
							newId = e.currentTarget.value = tidyId(e.currentTarget.value);
						}}
						placeholder="000 000 000 · 192.168.1.42"
						aria-label={t('devices.idOrIp')}
						style="font-family:var(--font-mono)"
					/>
				</div>
				<label class="fl" for="dev-pw">{t('devices.password')}</label>
				<div class="field pwfield">
					<Icon name="lock" size={15} />
					<input
						use:navItem
						id="dev-pw"
						type={showPw ? 'text' : 'password'}
						bind:value={newPw}
						autocomplete="off"
						spellcheck="false"
						placeholder={t('devices.passwordPlaceholder')}
						aria-label={t('devices.password')}
					/>
					<button
						use:navItem
						type="button"
						class="pwtoggle"
						aria-pressed={showPw}
						aria-label={showPw ? t('devices.hidePassword') : t('devices.showPassword')}
						title={showPw ? t('devices.hidePassword') : t('devices.showPassword')}
						onclick={() => (showPw = !showPw)}><Icon name={showPw ? 'eyeOff' : 'eye'} size={16} /></button
					>
				</div>
				<span class="hint">{t('devices.passwordHint')}</span>
			</div>
		</div>
		{#if start}
			<!-- Pairing: the host trusts this client's key, so it stops asking for a password
			     (its 2FA, if on, still applies). -->
			<div class="pairrow">
				<div class="pairtxt">
					<b>
						{t('devices.pairing')}
						{#if paired}<span class="pairbadge"><Icon name="check" size={12} />{t('devices.paired')}</span>{/if}
					</b>
					<span>{t('devices.pairDesc')}</span>
					{#if pairMsg}<span class="pairmsg" class:err={pairErr} role="status">{pairMsg}</span>{/if}
				</div>
				<button class="btn btn-ghost" use:navItem disabled={pairBusy} onclick={togglePair}>
					<Icon name="link" size={15} />{paired ? t('devices.unpair') : t('devices.pair')}
				</button>
			</div>
		{/if}
		<div class="factions">
			{#if note}<span class="note">{note}</span>{/if}
			<button class="btn btn-ghost" use:navItem onclick={onClose}>{t('devices.cancel')}</button>
			<button class="btn btn-primary" use:navItem onclick={submit}>{start ? t('devices.saveBtn') : t('devices.addBtn')}</button>
		</div>
	</div>
</Modal>

<style>
	.addform {
		display: flex;
		flex-direction: column;
	}
	.addgrid {
		display: grid;
		grid-template-columns: auto 1fr;
		gap: 22px;
		align-items: start;
	}
	.imgcol {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 10px;
	}
	.preview {
		position: relative;
		width: 124px;
		height: 124px;
		border-radius: var(--r-lg);
		border: 1px solid var(--border);
		background: var(--accent-soft);
		color: var(--accent-fg);
		display: grid;
		place-items: center;
		overflow: hidden;
		padding: 0;
		cursor: pointer;
		transition: border-color var(--dur);
	}
	.preview:hover {
		border-color: var(--accent);
	}
	.preview.photo {
		background: var(--surface-2);
	}
	/* "Change picture" badge, pinned bottom-right; always visible so the tile reads as clickable. */
	.pv-up {
		position: absolute;
		right: 7px;
		bottom: 7px;
		width: 28px;
		height: 28px;
		border-radius: 50%;
		display: grid;
		place-items: center;
		background: var(--surface);
		color: var(--text-muted);
		border: 1px solid var(--border);
		box-shadow: var(--shadow-sm);
	}
	.preview:hover .pv-up {
		color: var(--accent-fg);
		border-color: var(--accent);
	}
	.imgrow {
		display: grid;
		grid-template-columns: repeat(4, 1fr);
		gap: 6px;
		width: 124px;
	}
	.imgopt {
		aspect-ratio: 1;
		border: 1px solid var(--border);
		border-radius: 8px;
		background: var(--surface);
		color: var(--text-muted);
		cursor: pointer;
		display: grid;
		place-items: center;
		padding: 0;
	}
	.imgopt.active {
		border-color: var(--accent);
		color: var(--accent-fg);
		background: var(--accent-soft);
	}
	.uimg {
		width: 100%;
		height: 100%;
		object-fit: cover;
	}
	.fieldcol {
		display: flex;
		flex-direction: column;
		gap: 6px;
		min-width: 0;
	}
	.fieldcol .fl {
		font-size: 12px;
		font-weight: 600;
		color: var(--text-muted);
	}
	.fieldcol .fl + .field {
		margin-bottom: 8px;
	}
	.pwfield input {
		font-family: var(--font-mono);
	}
	.pwtoggle {
		border: none;
		background: transparent;
		color: var(--text-muted);
		padding: 2px;
		display: grid;
		place-items: center;
		cursor: pointer;
		border-radius: 6px;
	}
	.pwtoggle:hover,
	.pwtoggle[aria-pressed='true'] {
		color: var(--accent-fg);
	}
	.hint {
		font-size: 11.5px;
		color: var(--text-faint);
		line-height: 1.4;
	}
	.pairrow {
		display: flex;
		align-items: center;
		gap: 16px;
		margin-top: 18px;
		padding-top: 14px;
		border-top: 1px solid var(--border);
	}
	.pairtxt {
		flex: 1;
		min-width: 0;
		display: flex;
		flex-direction: column;
		gap: 3px;
	}
	.pairtxt b {
		font-size: 13px;
		font-weight: 600;
		display: flex;
		align-items: center;
		gap: 8px;
	}
	.pairtxt > span {
		font-size: 12px;
		color: var(--text-faint);
		line-height: 1.4;
	}
	.pairbadge {
		display: inline-flex;
		align-items: center;
		gap: 4px;
		font-size: 11px;
		font-weight: 600;
		padding: 2px 8px;
		border-radius: 999px;
		background: var(--accent-soft);
		color: var(--accent-fg);
	}
	.pairtxt .pairmsg {
		color: var(--text-muted);
	}
	.pairtxt .pairmsg.err {
		color: var(--danger, #d14343);
	}
	.factions {
		display: flex;
		align-items: center;
		justify-content: flex-end;
		gap: 10px;
		margin-top: 22px;
		padding-top: 16px;
		border-top: 1px solid var(--border);
	}
	.note {
		margin-right: auto;
		font-size: 11.5px;
		color: var(--text-faint);
	}
</style>
