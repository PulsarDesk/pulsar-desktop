// Browser/OS-window helpers used by the UI: clipboard read/write, fullscreen, and
// frameless window controls. Re-exported by `api.ts`.

import { invoke, isTauri } from './api.invoke';

/** Copy text to the clipboard. Uses the async Clipboard API, falling back to a
 * hidden textarea + execCommand for webviews that block it. Returns success. */
export async function copyText(text: string): Promise<boolean> {
	// App-side first: the DOM Clipboard API silently fails in an occluded/unfocused
	// WebKitGTK (the Linux native-video path), so in Tauri the OS clipboard is owned
	// by Rust (io_cmds.rs). The DOM paths remain for browser dev.
	if (isTauri) {
		try {
			await invoke<void>('write_clipboard_text', { text });
			return true;
		} catch {
			/* fall through to the DOM paths */
		}
	}
	try {
		if (typeof navigator !== 'undefined' && navigator.clipboard?.writeText) {
			await navigator.clipboard.writeText(text);
			return true;
		}
	} catch {
		/* fall through to the legacy path */
	}
	try {
		const ta = document.createElement('textarea');
		ta.value = text;
		ta.setAttribute('readonly', '');
		ta.style.position = 'fixed';
		ta.style.top = '-1000px';
		ta.style.opacity = '0';
		document.body.appendChild(ta);
		ta.select();
		const ok = document.execCommand('copy');
		document.body.removeChild(ta);
		return ok;
	} catch {
		return false;
	}
}

/** Toggle true, taskbar-covering fullscreen (no-op outside Tauri). */
export async function setFullscreen(on: boolean): Promise<void> {
	if (!isTauri) return;
	// Done Rust-side: manual monitor-cover + always-on-top, which reliably hides the
	// taskbar for our transparent window (plain setFullscreen leaves it visible).
	await invoke<void>('set_window_fullscreen', { on });
}

/** Read the local clipboard text (for "paste to remote"). */
export async function readClipboard(): Promise<string> {
	// App-side first — see copyText (the DOM read is dead in occluded WebKitGTK).
	if (isTauri) {
		try {
			return await invoke<string>('read_clipboard_text');
		} catch {
			/* fall through to the DOM path */
		}
	}
	try {
		if (typeof navigator !== 'undefined' && navigator.clipboard?.readText) {
			return await navigator.clipboard.readText();
		}
	} catch {
		/* ignore */
	}
	return '';
}

/** Is the OS window maximized? GTK's own flag goes stale on Linux (the app runs under
 * XWayland, GDK_BACKEND=x11): KWin maximizes the window but GTK never sees the state change,
 * so `isMaximized()` stays false — and `toggleMaximize()`, which trusts it, never restores.
 * So a window that fills its monitor's work area also counts as maximized. */
export async function isWindowMaximized(): Promise<boolean> {
	if (!isTauri) return false;
	const { getCurrentWindow, currentMonitor } = await import('@tauri-apps/api/window');
	const w = getCurrentWindow();
	if (await w.isMaximized()) return true;
	const [m, size] = await Promise.all([currentMonitor(), w.outerSize()]);
	if (!m) return false;
	const area = m.workArea.size;
	return size.width >= area.width - 2 && size.height >= area.height - 2;
}

/** Control the frameless OS window (no-op outside Tauri, e.g. browser dev). `maximize`
 * toggles maximize ↔ restore. */
export async function windowControl(action: 'minimize' | 'maximize' | 'close') {
	if (!isTauri) return;
	const { getCurrentWindow } = await import('@tauri-apps/api/window');
	const w = getCurrentWindow();
	if (action === 'minimize') await w.minimize();
	else if (action === 'maximize') await ((await isWindowMaximized()) ? w.unmaximize() : w.maximize());
	else await w.close();
}

/** An edge of the frameless window, for `startWindowResize`. */
export type ResizeEdge = 'North' | 'South' | 'East' | 'West' | 'NorthEast' | 'NorthWest' | 'SouthEast' | 'SouthWest';

/** Hand an edge drag to the window manager (frameless window → no native resize border). The
 * WM still enforces the configured minimum size (tauri.conf `minWidth`/`minHeight`). */
export async function startWindowResize(edge: ResizeEdge): Promise<void> {
	if (!isTauri) return;
	const { getCurrentWindow } = await import('@tauri-apps/api/window');
	await getCurrentWindow().startResizeDragging(edge);
}
