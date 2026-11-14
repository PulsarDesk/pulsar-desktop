<script lang="ts">
	import Icon from '#lib/Icon.svelte';
	import type { Config } from '#lib/types.js';
	import { ui, saveUi } from '#lib/settings.svelte.js';
	import { t, i18n, setLang, LANGS } from '#lib/i18n.svelte.js';
	import { theme, setDark } from '#lib/theme.svelte.js';
	import { api, isTauri } from '#lib/api.js';
	import type { SharedModeStatus } from '#lib/api.types.js';
	import pkg from '../../../../package.json';

	// Start with the computer (+ in the background): read from the OS (it is the source of
	// truth — an installer or another Pulsar may have changed it), written on toggle.
	let autostart = $state(false);
	let autostartBg = $state(false);
	let autostartErr = $state('');
	$effect(() => {
		if (!isTauri) return;
		api.autostartStatus()
			.then(([on, bg]) => {
				autostart = on;
				autostartBg = bg;
			})
			.catch(() => {});
	});
	function applyAutostart(on: boolean, bg: boolean) {
		autostartErr = '';
		const prev = [autostart, autostartBg];
		autostart = on;
		autostartBg = bg;
		api.setAutostart(on, bg).catch((e) => {
			[autostart, autostartBg] = prev;
			autostartErr = String(e);
		});
	}

	let {
		config = $bindable(),
		saveConfig,
		setAvatar
	}: {
		config: Config | null;
		saveConfig: () => void;
		setAvatar: (mode: string) => void;
	} = $props();

	// The shipped version comes from the Tauri shell (tauri.conf / Cargo.toml);
	// the browser mock falls back to package.json's.
	let version = $state(pkg.version);
	if (isTauri) {
		import('@tauri-apps/api/app')
			.then((m) => m.getVersion())
			.then((v) => (version = v))
			.catch(() => {});
	}

	// App log directory (file logging is always on; this is where a black-screen
	// report comes from). The path is shown so it can be found even when no file
	// manager opens.
	let logDir = $state('');
	if (isTauri) {
		api.logDirPath()
			.then((p) => (logDir = p))
			.catch(() => {});
	}
	function openLogs() {
		api.openLogDir()
			.then((p) => (logDir = p))
			.catch(() => {});
	}

	// "Kullanıcılar arası ortak Pulsar": one device ID / config / pairings for the whole PC
	// (machine-wide, admin-only files) instead of one per Windows user. Changing it restarts
	// Pulsar; other users' Pulsar follows on its next start.
	let shared = $state<SharedModeStatus | null>(null);
	let sharedBusy = $state(false);
	let sharedErr = $state('');
	api.sharedModeStatus()
		.then((s) => (shared = s))
		.catch(() => {});
	const sharedNote = $derived.by(() => {
		if (sharedErr) return sharedErr;
		if (!shared) return '';
		if (shared.noAccess) return t('shared.noAccess');
		if (shared.enabled !== shared.active) return t('settings.restartRequired');
		if (shared.active && !shared.held)
			return shared.holder ? t('shared.standbyUser', { user: shared.holder }) : t('shared.standby');
		return '';
	});
	function toggleShared() {
		if (!shared || sharedBusy) return;
		const next = !shared.enabled;
		sharedBusy = true;
		sharedErr = '';
		// On success the app restarts itself; the browser mock just flips.
		api.setSharedMode(next)
			.then(() => {
				if (shared) shared = { ...shared, enabled: next };
			})
			.catch((e) => {
				const msg = e instanceof Error ? e.message : String(e);
				sharedErr = msg === 'SHARED_ELEVATION' ? t('shared.needAdmin') : t('shared.failed');
			})
			.finally(() => (sharedBusy = false));
	}

	// Display name defaults: an unset name falls back to the OS user's name on the
	// wire (host.rs/play.rs already do this), so the input mirrors that — the OS
	// name is the PLACEHOLDER, and a value equal to it (or the legacy "Pulsar
	// Cihazı" default) renders as empty instead of looking like a custom choice.
	// A name equal to the machine HOSTNAME is replaced by the OS name on the wire too
	// (util::announce_name), so it must not show as a custom value either — it did, and the
	// field said "desktop-ab12cd" while peers saw the user's real name.
	let userName = $state('');
	let hostName = $state('');
	api.deviceUserName().then((n) => (userName = n)).catch(() => {});
	api.deviceHostName().then((n) => (hostName = n)).catch(() => {});
	const shownName = $derived.by(() => {
		const v = (config?.device_name ?? '').trim();
		const isHost = !!hostName && v.toLowerCase() === hostName.toLowerCase();
		return v === userName || v === 'Pulsar Cihazı' || isHost ? '' : v;
	});
	// Identity image preview: what peers will actually receive for the chosen mode.
	let avatarPrev = $state<string | null>(null);
	let avatarLoading = $state(false);
	$effect(() => {
		const mode = config?.avatar_mode ?? 'user';
		let live = true;
		avatarLoading = true;
		api.avatarPreview(mode)
			.then((u) => live && (avatarPrev = u))
			.catch(() => live && (avatarPrev = null))
			.finally(() => live && (avatarLoading = false));
		return () => (live = false);
	});

	function onNameChange(e: Event) {
		if (!config) return;
		config.device_name = (e.currentTarget as HTMLInputElement).value.trim();
		saveConfig();
	}

	// App-UI hardware acceleration (webview GPU compositing) — distinct from video encode/decode.
	// The toggle shows the effective state (the per-device default when no explicit pref is set);
	// changing it persists a preference that applies after an app restart.
	let defaultHwaccel = $state(true);
	let hwaccelChanged = $state(false);
	if (isTauri) api.defaultUiHwaccel().then((v) => (defaultHwaccel = v)).catch(() => {});
	const hwaccelOn = $derived(config?.ui_hardware_accel ?? defaultHwaccel);
	function toggleHwaccel() {
		if (!config) return;
		config.ui_hardware_accel = !hwaccelOn;
		saveConfig();
		hwaccelChanged = true;
	}
</script>

<div class="subgroup">{t('settings.subProfile')}</div>
<div class="subdesc">{t('settings.subProfileDesc')}</div>
<div class="srow">
	<div class="st"><b>{t('settings.language')}</b><span>{t('settings.languageDesc')}</span></div>
	<div class="seg">
		{#each LANGS as l (l.value)}
			<button class:active={i18n.lang === l.value} onclick={() => setLang(l.value)}>{l.label}</button>
		{/each}
	</div>
</div>
<div class="srow">
	<div class="st"><b>{t('settings.displayName')}</b><span>{t('settings.displayNameDesc')}</span></div>
	<div class="field" style="width:220px">
		<Icon name="devices" size={15} />
		{#if config}
			<input
				value={shownName}
				placeholder={userName}
				onchange={onNameChange}
				aria-label={t('settings.displayName')}
			/>
		{/if}
	</div>
</div>
<div class="srow">
	<div class="st"><b>{t('settings.avatar')}</b><span>{t('settings.avatarDesc')}</span></div>
	<div
		class="avprev"
		class:loading={avatarLoading}
		title={avatarPrev ? t('settings.avatarPreview') : t('settings.avatarNone')}
	>
		{#if avatarPrev}<img src={avatarPrev} alt={t('settings.avatarPreview')} />{:else}<Icon name="x" size={14} />{/if}
	</div>
	<div class="seg">
		{#each [['user', t('settings.avatarUser')], ['wallpaper', t('settings.avatarWall')], ['anonymous', t('settings.avatarAnon')]] as [v, l] (v)}
			<button class:active={config?.avatar_mode === v} onclick={() => setAvatar(v)}>{l}</button>
		{/each}
	</div>
</div>
<div class="subgroup">{t('settings.subApp')}</div>
<div class="subdesc">{t('settings.subAppDesc')}</div>
<div class="srow">
	<div class="st"><b>{t('settings.darkTheme')}</b><span>{t('settings.darkThemeDesc')}</span></div>
	<button class="toggle" aria-label={t('settings.darkTheme')} class:on={theme.dark} aria-pressed={theme.dark} onclick={() => setDark(!theme.dark)}><span class="knob"></span></button>
</div>
{#if shared?.supported}
	<div class="srow">
		<div class="st">
			<b>{t('settings.sharedMode')}</b>
			<span>{t('settings.sharedModeDesc')}{#if sharedNote} <strong class="restart">· {sharedNote}</strong>{/if}</span>
		</div>
		<button class="toggle" aria-label={t('settings.sharedMode')} class:on={shared.enabled} aria-pressed={shared.enabled} disabled={sharedBusy} onclick={toggleShared}><span class="knob"></span></button>
	</div>
{/if}
<div class="srow">
	<div class="st"><b>{t('settings.tray')}</b><span>{t('settings.trayDesc')}</span></div>
	<button class="toggle" aria-label={t('settings.tray')} class:on={ui.tray} aria-pressed={ui.tray} onclick={() => { ui.tray = !ui.tray; saveUi(); api.setTray(ui.tray).catch(() => {}); }}><span class="knob"></span></button>
</div>
<div class="srow">
	<div class="st"><b>{t('settings.autoUpdate')}</b><span>{t('settings.autoUpdateDesc')}</span></div>
	<button class="toggle" aria-label={t('settings.autoUpdate')} class:on={ui.autoUpdate} aria-pressed={ui.autoUpdate} onclick={() => { ui.autoUpdate = !ui.autoUpdate; saveUi(); }}><span class="knob"></span></button>
</div>
<div class="srow">
	<div class="st"><b>{t('settings.clipboardSync')}</b><span>{t('settings.clipboardSyncDesc')}</span></div>
	<button class="toggle" aria-label={t('settings.clipboardSync')} class:on={ui.clipboardSync} aria-pressed={ui.clipboardSync} onclick={() => { ui.clipboardSync = !ui.clipboardSync; saveUi(); api.setClipboardSync(ui.clipboardSync).catch(() => {}); }}><span class="knob"></span></button>
</div>
<div class="srow">
	<div class="st">
		<b>{t('settings.uiHwaccel')}</b>
		<span>{t('settings.uiHwaccelDesc')}{#if hwaccelChanged} <strong class="restart">· {t('settings.restartRequired')}</strong>{/if}</span>
	</div>
	<button class="toggle" aria-label={t('settings.uiHwaccel')} class:on={hwaccelOn} aria-pressed={hwaccelOn} onclick={toggleHwaccel}><span class="knob"></span></button>
</div>
<div class="srow">
	<div class="st">
		<b>{t('settings.autostart')}</b>
		<span>{autostartErr || t('settings.autostartDesc')}</span>
	</div>
	<button class="toggle" aria-label={t('settings.autostart')} class:on={autostart} aria-pressed={autostart} onclick={() => applyAutostart(!autostart, autostartBg)}><span class="knob"></span></button>
</div>
<div class="srow" class:dim={!autostart}>
	<div class="st">
		<b>{t('settings.autostartBg')}</b>
		<span>{t('settings.autostartBgDesc')}</span>
	</div>
	<button class="toggle" aria-label={t('settings.autostartBg')} class:on={autostartBg} aria-pressed={autostartBg} disabled={!autostart} onclick={() => applyAutostart(autostart, !autostartBg)}><span class="knob"></span></button>
</div>
<div class="srow">
	<div class="st">
		<b>{t('settings.duplicateSessions')}</b>
		<span>{t('settings.duplicateSessionsDesc')}</span>
	</div>
	<button class="toggle" aria-label={t('settings.duplicateSessions')} class:on={ui.duplicateSessions} aria-pressed={ui.duplicateSessions} onclick={() => { ui.duplicateSessions = !ui.duplicateSessions; saveUi(); }}><span class="knob"></span></button>
</div>
<div class="subgroup">{t('settings.subPad')}</div>
<div class="subdesc">{t('settings.subPadDesc')}</div>
<div class="srow">
	<div class="st"><b>{t('settings.padWake')}</b><span>{t('settings.padWakeDesc')}</span></div>
	<button class="toggle" aria-label={t('settings.padWake')} class:on={ui.padWake} aria-pressed={ui.padWake} onclick={() => { ui.padWake = !ui.padWake; saveUi(); api.setPadWake(ui.padWake).catch(() => {}); }}><span class="knob"></span></button>
</div>
<div class="srow">
	<div class="st"><b>{t('settings.padBackground')}</b><span>{t('settings.padBackgroundDesc')}</span></div>
	<button class="toggle" aria-label={t('settings.padBackground')} class:on={ui.padBackground} aria-pressed={ui.padBackground} onclick={() => { ui.padBackground = !ui.padBackground; saveUi(); api.setPadBackground(ui.padBackground).catch(() => {}); }}><span class="knob"></span></button>
</div>
<div class="srow">
	<div class="st"><b>{t('settings.touchpadMouse')}</b><span>{t('settings.touchpadMouseDesc')}</span></div>
	<button class="toggle" aria-label={t('settings.touchpadMouse')} class:on={ui.touchpadAsMouse} aria-pressed={ui.touchpadAsMouse} onclick={() => { ui.touchpadAsMouse = !ui.touchpadAsMouse; saveUi(); }}><span class="knob"></span></button>
</div>
<div class="subgroup">{t('settings.subDiag')}</div>
<div class="subdesc">{t('settings.subDiagDesc')}</div>
<div class="srow">
	<div class="st"><b>{t('settings.debug')}</b><span>{t('settings.debugDesc')}</span></div>
	<button class="toggle" aria-label={t('settings.debug')} class:on={ui.debug} aria-pressed={ui.debug} onclick={() => { ui.debug = !ui.debug; saveUi(); }}><span class="knob"></span></button>
</div>
<div class="srow">
	<div class="st">
		<b>{t('settings.logs')}</b>
		<span>{t('settings.logsDesc')}{#if logDir} <span class="mono path selectable">{logDir}</span>{/if}</span>
	</div>
	<button class="act" onclick={openLogs}>{t('settings.openLogs')}</button>
</div>
<div class="srow">
	<div class="st"><b>{t('settings.version')}</b><span>{t('settings.versionDesc')}</span></div>
	<span class="mono ver selectable">Pulsar v{version}</span>
</div>

<style>
	.avprev {
		flex: none;
		width: 40px;
		height: 40px;
		margin-left: auto;
		margin-right: 10px;
		border-radius: 10px;
		overflow: hidden;
		display: grid;
		place-items: center;
		background: var(--surface-2);
		border: 1px solid var(--border);
		color: var(--text-faint);
		transition: opacity var(--dur) var(--ease);
	}
	.avprev.loading {
		opacity: 0.5;
	}
	.avprev img {
		width: 100%;
		height: 100%;
		object-fit: cover;
	}
	.act {
		flex-shrink: 0;
		padding: 8px 14px;
		border: 1px solid var(--border);
		border-radius: 10px;
		background: var(--surface, transparent);
		color: var(--text);
		font: inherit;
		font-weight: 600;
		cursor: pointer;
	}
	.act:hover {
		border-color: var(--accent);
		color: var(--accent-fg);
	}
	.path {
		word-break: break-all;
		opacity: 0.8;
	}
	.srow {
		display: flex;
		align-items: center;
		gap: 20px;
		padding: 16px 0;
		border-bottom: 1px solid var(--border);
	}
	.st {
		flex: 1;
	}
	.st b {
		font-size: 14px;
		font-weight: 600;
		display: block;
	}
	.st span {
		font-size: 12.5px;
		color: var(--text-faint);
		margin-top: 3px;
		line-height: 1.45;
		display: block;
		max-width: 46ch;
	}
	.ver {
		font-size: 13px;
		color: var(--text-muted);
	}
	.restart {
		color: var(--accent-fg);
		font-weight: 600;
	}
</style>
