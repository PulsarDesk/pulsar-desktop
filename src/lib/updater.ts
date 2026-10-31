import { check } from '@tauri-apps/plugin-updater';
import { relaunch } from '@tauri-apps/plugin-process';
import { api } from './api.commands';
import { update as updateState } from './update.svelte';
import { ui } from './settings.svelte';

/**
 * Silently check for a newer release on this build's channel and, if found,
 * download + install + relaunch. Appliance/kiosk UX: no prompt. NEVER call this
 * while a session is live or about to auto-connect — it must not interrupt
 * remote control. Any failure (offline, signature, download, no Tauri context)
 * is swallowed so a broken updater can never block launch.
 *
 * `timeoutMs` bounds only the manifest HTTP request (the Tauri plugin applies it
 * to `check()`). `overallTimeoutMs` bounds the ENTIRE flow including the
 * download+install phase, which the plugin otherwise runs with no timeout — so a
 * stalled download can't hang a boot path that sequences `connect` after this.
 *
 * `isBusy` is re-evaluated at two points inside `run()`: (1) after the slow
 * `check()` round-trip, before committing to `download()`; and (2) immediately
 * before `install()`, after the download completes. This covers the boot path
 * (where `overallTimeoutMs` causes `Promise.race` to reject and boot to continue,
 * but the still-running `run()` promise finishes the download later) and the idle
 * path (where there is no overall cap but a download can be slow). If a session is
 * live at either guard point, the update is abandoned — the next boot will pick it
 * up. The installed-but-not-relaunched state is safe: the new binary is in place
 * and will be used on the next normal launch.
 *
 * NOTE on Windows: `install()` spawns the (quiet) NSIS installer and calls
 * process::exit(0) — it never returns. The second guard (before `install()`) is
 * therefore the ONLY effective busy-check on the Windows code path, because the
 * older `downloadAndInstall()`-then-check pattern was unreachable. The download
 * itself is non-destructive, so splitting into `download()` + guard + `install()`
 * restores the session-protection guarantee on Windows.
 */
export async function silentUpdateCheck(opts?: {
	timeoutMs?: number;
	overallTimeoutMs?: number;
	isBusy?: () => boolean;
}): Promise<void> {
	const run = async () => {
		// On Linux the updater self-replaces the file pointed to by $APPIMAGE. When the
		// AppImage is launched without FUSE (e.g. `--appimage-extract-and-run`, or the raw
		// `--no-bundle` dev binary), $APPIMAGE is unset and the plugin falls back to a
		// throwaway extracted temp binary — so download()+install() would silently rewrite a
		// temp file (or error) instead of the deployed AppImage, leaving the appliance stuck
		// on the old version while the updater appears to "succeed". Skip loudly instead.
		if (!(await api.selfUpdatePossible().catch(() => true))) {
			console.warn(
				'[updater] skipped: self-update unavailable (running without $APPIMAGE — ' +
					'launch the AppImage with FUSE, not --appimage-extract-and-run, to enable updates)'
			);
			return;
		}
		const update = await check(opts?.timeoutMs ? { timeout: opts.timeoutMs } : undefined);
		if (!update) return; // already up to date
		// A session may have started during the (slow) manifest fetch above. Re-check
		// before committing to the download so we don't interrupt remote control.
		if (opts?.isBusy?.()) {
			console.warn('[updater] skipped: a session started during the update check');
			return;
		}
		// Phase 1: non-destructive download only.
		await update.download();
		// Re-check busy state BEFORE the destructive install: the download may have
		// taken longer than overallTimeoutMs (boot path) or longer than the user took
		// to start a session (idle path). Either way, if a session is now live we must
		// NOT tear it down — defer silently and let the next boot pick up the update.
		// On Windows install() spawns the NSIS installer and exits the process, so
		// this is the ONLY effective post-download guard on Windows — the old pattern
		// (check after downloadAndInstall()) was physically unreachable on Windows.
		if (opts?.isBusy?.()) {
			console.warn('[updater] update downloaded but a session is live — deferring relaunch to next boot');
			return;
		}
		// Phase 2: destructive install. On Windows this spawns the (quiet) NSIS
		// installer and calls process::exit(0), so it never returns here — the
		// installer's /R flag auto-relaunches the app. On macOS/Linux install()
		// replaces the binary in place and returns, and relaunch() below does the
		// actual restart.
		await update.install();
		// macOS/Linux only — on Windows install() already exited the process above.
		await relaunch();
	};
	try {
		if (opts?.overallTimeoutMs) {
			// Cap the whole check+download+install so a stalled download can never
			// block boot. On timeout we give up on this update and continue launch.
			await Promise.race([
				run(),
				new Promise<void>((_, reject) =>
					setTimeout(
						() => reject(new Error(`updater timed out after ${opts.overallTimeoutMs}ms`)),
						opts.overallTimeoutMs
					)
				)
			]);
		} else {
			await run();
		}
	} catch (e) {
		// Offline / no endpoint / signature mismatch / download stall / browser mock:
		// log + continue. Never throw into launch.
		console.warn('[updater] skipped:', e);
	}
}

/**
 * Idle-launch update check with EXPLICIT-consent UX (the non-kiosk path):
 * finds a newer release and surfaces it in the `update` store — the chrome badge
 * lights up and the modal shows version + notes + an Install button. Nothing is
 * downloaded or installed unless the user clicks Install, EXCEPT when the
 * `autoUpdate` setting (default OFF) is on AND this install can self-update —
 * then it behaves like the old silent flow (download+install+relaunch), still
 * respecting `isBusy` so a live session is never interrupted.
 *
 * Installs that CANNOT self-update (flatpak / package manager / raw binary /
 * non-FUSE AppImage) still get the badge + modal — being outdated must be
 * visible — but with the Install button disabled and manual instructions shown.
 */
export async function checkForUpdateUi(isBusy?: () => boolean): Promise<void> {
	try {
		const update = await check({ timeout: 15000 });
		if (!update) return; // up to date
		updateState.handle = update;
		updateState.from = update.currentVersion;
		updateState.to = update.version;
		updateState.notes = plainNotes(await notesFor(update.version, update.body ?? ''));
		updateState.installable = await api.selfUpdatePossible().catch(() => true);
		updateState.available = true;
		if (ui.autoUpdate && updateState.installable && !isBusy?.()) {
			await installUpdate(isBusy);
		}
	} catch (e) {
		console.warn('[updater] check skipped:', e);
	}
}

/**
 * Download + install the pending update (from `checkForUpdateUi`), streaming
 * progress into the `update` store for the modal's download/install/restart
 * stages. Aborts before each destructive step if `isBusy` reports a live session.
 */
export async function installUpdate(isBusy?: () => boolean): Promise<void> {
	const u = updateState.handle;
	// DEV-only mock: the design preview (Ctrl+Alt+U / ?mockUpdate) sets the store fields but
	// no real Update handle, so Install here drives a SIMULATED download → install → restart
	// with zero contact with the real updater — it can never actually replace the binary.
	// Compiled out of release builds; the real path below is untouched.
	if (import.meta.env.DEV && !u && updateState.available && updateState.installable) {
		if (updateState.phase === 'downloading' || updateState.phase === 'installing') return;
		const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
		updateState.error = '';
		updateState.phase = 'downloading';
		updateState.total = 86 * 1024 * 1024;
		updateState.received = 0;
		const step = updateState.total / 24;
		while (updateState.received < updateState.total) {
			await sleep(70);
			updateState.received = Math.min(updateState.total, updateState.received + step);
		}
		updateState.phase = 'installing';
		await sleep(1100);
		updateState.phase = 'restarting';
		await sleep(1400);
		// A real restart relaunches the app; the mock just closes so it can be replayed.
		updateState.phase = 'idle';
		updateState.open = false;
		updateState.available = false;
		return;
	}
	if (!u || !updateState.installable) return;
	if (updateState.phase === 'downloading' || updateState.phase === 'installing') return;
	try {
		updateState.error = '';
		updateState.phase = 'downloading';
		updateState.received = 0;
		updateState.total = 0;
		await u.download((ev) => {
			if (ev.event === 'Started') updateState.total = ev.data.contentLength ?? 0;
			else if (ev.event === 'Progress') updateState.received += ev.data.chunkLength;
			else if (ev.event === 'Finished') updateState.received = updateState.total || updateState.received;
		});
		// The download can be slow — never tear down a session that started meanwhile.
		// (On Windows install() spawns the NSIS installer and exits the process, so this
		// pre-install guard is the only effective one there.)
		if (isBusy?.()) {
			updateState.phase = 'idle';
			console.warn('[updater] downloaded but a session is live — deferring install');
			return;
		}
		updateState.phase = 'installing';
		await u.install();
		updateState.phase = 'restarting';
		await relaunch();
	} catch (e) {
		updateState.phase = 'error';
		updateState.error = String(e);
		console.warn('[updater] install failed:', e);
	}
}

/** How often a long-running launch re-checks for a release (the boot check runs once). */
export const PERIODIC_CHECK_MS = 60 * 60 * 1000;

/**
 * Re-run `checkForUpdateUi` every `everyMs` so an app left open for days still learns
 * about new releases (it used to check only at launch). A tick is skipped while a
 * session is live, while the update modal is open (the user is looking at that offer —
 * don't swap its handle underneath them) or while an install is in flight. Returns a
 * stop function.
 */
export function startPeriodicUpdateCheck(isBusy: () => boolean, everyMs = PERIODIC_CHECK_MS): () => void {
	const timer = setInterval(() => {
		if (isBusy() || updateState.open || updateState.phase !== 'idle') return;
		void checkForUpdateUi(isBusy);
	}, everyMs);
	return () => clearInterval(timer);
}

const RELEASES_API = 'https://api.github.com/repos/PulsarDesk/pulsar-desktop/releases/tags/';

/**
 * The manifest's notes, or — when they're missing or only the "Pulsar x.y.z" placeholder
 * older release runs wrote — the GitHub release's changelog for that version. Any failure
 * keeps what the manifest had.
 */
async function notesFor(version: string, manifestNotes: string): Promise<string> {
	const n = manifestNotes.trim();
	if (n && !/^Pulsar v?\d+\.\d+\.\d+\S*$/.test(n)) return n;
	try {
		const r = await fetch(RELEASES_API + 'v' + version.replace(/^v/, ''), {
			headers: { Accept: 'application/vnd.github+json' },
			signal: AbortSignal.timeout(8000)
		});
		if (!r.ok) return n;
		const body = ((await r.json()) as { body?: string }).body ?? '';
		return body.trim() || n;
	} catch {
		return n;
	}
}

/**
 * semantic-release changelog markdown → readable plain text for the modal: drops the
 * "## [x.y.z](compare-url) (date)" title (the modal shows the versions), commit-hash links
 * and markdown syntax.
 */
export function plainNotes(md: string): string {
	return md
		.replace(/\r/g, '')
		.replace(/^#{1,3}\s*\[?v?\d+\.\d+\.\d+[^\n]*\n?/m, '') // version title
		.replace(/\s*\(\[[0-9a-f]{7,40}\]\([^)]*\)\)/g, '') // ([abc1234](commit-url))
		.replace(/\[([^\]]*)\]\([^)]*\)/g, '$1') // [text](url) → text
		.replace(/^#{1,6}\s*/gm, '') // headings
		.replace(/\*\*([^*]+)\*\*/g, '$1') // bold
		.replace(/^[ \t]*[*-][ \t]+/gm, '• ') // bullets
		.replace(/\n{3,}/g, '\n\n')
		.trim();
}
