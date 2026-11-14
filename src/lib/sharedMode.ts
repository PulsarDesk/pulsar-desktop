// "Kullanıcılar arası ortak Pulsar" (shared_mode.rs): which i18n key a backend error code maps
// to. Windows needs an elevated Pulsar; Linux/macOS need admin-group membership plus the
// system password prompt (pkexec / the macOS administrator dialog), so the copy differs.

/** i18n key for a `set_shared_mode` rejection. */
export function sharedErrorKey(msg: string, unix: boolean): string {
	switch (msg) {
		case 'SHARED_ELEVATION':
			return unix ? 'shared.needAdminUnix' : 'shared.needAdmin';
		case 'SHARED_CANCELLED':
			return 'shared.cancelled';
		case 'SHARED_DENIED':
			return 'shared.denied';
		default:
			return 'shared.failed';
	}
}

/** i18n key for "shared mode is on but this Pulsar can't use the shared files". */
export function sharedNoAccessKey(unix: boolean): string {
	return unix ? 'shared.noAccessUnix' : 'shared.noAccess';
}

/** i18n key for a `go_online` shared-mode rejection (`SHARED_NO_ACCESS` / `SHARED_REPAIR`),
 * `null` for any other message. `SHARED_REPAIR` (Linux/macOS): the machine identity exists
 * but this account cannot read it — re-running the setup (toggle off/on) fixes the modes. */
export function sharedHostErrorKey(msg: string, unix: boolean): string | null {
	if (msg === 'SHARED_NO_ACCESS') return sharedNoAccessKey(unix);
	if (msg === 'SHARED_REPAIR') return 'shared.repairUnix';
	return null;
}

/** This webview runs on Linux or macOS (for messages shown without a status object). */
export const IS_UNIX_UI =
	typeof navigator !== 'undefined' && !/Windows/i.test(navigator.userAgent);
