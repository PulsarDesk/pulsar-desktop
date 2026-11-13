// Locally-persisted UI/host preferences — including the video encoding selection.
// (Relay + network mode live in the core Config; these display/security/general
// prefs persist here until the streaming backend consumes them.)

export type VideoCodec = 'auto' | 'h264' | 'h265' | 'av1';
export type RumbleStrength = 'off' | 'weak' | 'medium' | 'strong';
/** Session audio: auto = the highest the link carries (Raw on a LAN with room, else High). */
export type AudioQuality = 'auto' | 'standard' | 'high' | 'raw';
/** DualSense feature choice: auto = on, but auto-degrade may turn it off on a weak
 *  Bluetooth link; on = always on; off = off. Same tokens as the Rust `Mode`. */
export type Ds5Mode = 'auto' | 'on' | 'off';

/** DualSense passthrough settings (Settings → Kumandalar → DualSense), pushed to Rust with
 *  `set_ds5_settings` (`pulsar_core::input::ds5::degrade::Ds5Prefs`, same JSON names). */
export interface Ds5Settings {
	/** Master switch ("DualSense özelliklerini kullan"). Off = plain gamepad forwarding.
	 *  Applies from the next connection. */
	enabled: boolean;
	triggers: Ds5Mode;
	haptics: Ds5Mode;
	speaker: Ds5Mode;
	mic: Ds5Mode;
	leds: Ds5Mode;
	rumble: Ds5Mode;
	/** "Kolun kulaklık girişi": headphones in the pad's jack get the session audio and this
	 *  device stays silent (PS5 gibi). Shed last. */
	headset: Ds5Mode;
	/** Shed `auto` features (speaker → mic → HD haptics → headset jack) while Bluetooth is weak. */
	autoDegrade: boolean;
	/** Haptics/speaker jitter buffer in ms (advanced). */
	jitterMs: number;
}

export const DS5_DEFAULTS: Ds5Settings = {
	enabled: true,
	triggers: 'auto',
	haptics: 'auto',
	speaker: 'auto',
	mic: 'auto',
	leds: 'auto',
	rumble: 'auto',
	headset: 'auto',
	autoDegrade: true,
	jitterMs: 64
};

/** The DualSense features in UI order, with their i18n label keys. */
export const DS5_FEATURES = [
	{ key: 'triggers', label: 'ds5.triggers' },
	{ key: 'haptics', label: 'ds5.haptics' },
	{ key: 'speaker', label: 'ds5.speaker' },
	{ key: 'mic', label: 'ds5.mic' },
	{ key: 'leds', label: 'ds5.leds' },
	{ key: 'rumble', label: 'ds5.rumble' },
	{ key: 'headset', label: 'ds5.headset' }
] as const satisfies readonly { key: keyof Ds5Settings; label: string }[];

const DS5_MODES: readonly Ds5Mode[] = ['auto', 'on', 'off'];

/** Merge a saved (possibly old or hand-edited) DualSense block over the defaults. */
export function normalizeDs5(saved: unknown): Ds5Settings {
	const out: Ds5Settings = { ...DS5_DEFAULTS };
	if (!saved || typeof saved !== 'object') return out;
	const s = saved as Record<string, unknown>;
	if (typeof s.enabled === 'boolean') out.enabled = s.enabled;
	if (typeof s.autoDegrade === 'boolean') out.autoDegrade = s.autoDegrade;
	for (const f of DS5_FEATURES) {
		const v = s[f.key];
		if (typeof v === 'string' && DS5_MODES.includes(v as Ds5Mode)) out[f.key] = v as Ds5Mode;
	}
	const j = Number(s.jitterMs);
	if (Number.isFinite(j) && j > 0) out.jitterMs = Math.min(500, Math.max(11, Math.round(j)));
	return out;
}

export type Encoder =
	| 'auto'
	| 'nvenc'
	| 'qsv'
	| 'amf'
	| 'videotoolbox'
	| 'vaapi'
	| 'rkmpp'
	| 'software';

export interface UiSettings {
	quality: string;
	/** Legacy, unused (never applied to a stream); kept so old stores still parse. */
	res: string;
	/** Legacy, unused (never applied to a stream); kept so old stores still parse. */
	fps: number;
	/** Video bitrate limit in Mbit (0 = automatic). The adaptive controller's ceiling: it
	 *  still lowers the rate on a weak link. Picked in the session menu, applied on connect. */
	bitrate: number;
	/** Stream resolution picked in the session menu ('auto', a preset like '1080p', or 'WxH'),
	 *  applied on the next connect. */
	streamRes: string;
	/** Stream frame rate picked in the session menu (0 = automatic), applied on connect. */
	streamFps: number;
	codec: VideoCodec;
	/** Host-side hardware encoder. */
	encoder: Encoder;
	/** Audio quality asked of the host (Settings + session menu). */
	audioQuality: AudioQuality;
	/** Relative-mouse sensitivity (games that grabbed the mouse); 1 = the mouse's own speed. */
	mouseSens: number;
	/** Remote mode: the cursor may leave the video — leaving hands mouse + keyboard back to
	 * this PC, coming back over the video takes control again (Parsec-style, issue #3). Off =
	 * the cursor stays locked to the video until Ctrl+Alt+Z. Also in the session menu. */
	pointerFree: boolean;
	/** Remote mode: mark the host's pointer ("Host") while the person at the host uses their
	 * own mouse, and show ours separately (issue #7). Default on; also in the session menu. */
	hostCursor: boolean;
	/** Picture upscaling (client): 'off' | 'auto' (the GPU's AI upscaler, else FSR) | 'fsr' |
	 * 'ai' (NVIDIA RTX / Intel Video Super Resolution) | 'sgsr' (Snapdragon GSR; FSR off Windows).
	 * Default 'auto' (AI, else Snapdragon GSR on Qualcomm, else FSR): it only acts when the
	 * picture is scaled UP (the adaptive ladder dropped the stream below the screen's size, or a
	 * smaller host), so a full-size stream is untouched and a lowered one comes back sharp. */
	upscale: 'off' | 'auto' | 'fsr' | 'ai' | 'sgsr';
	/** Allow more than one session to the same device (same host, mode and game): a repeat
	 * connect opens another session instead of switching to the open one. Off by default. */
	duplicateSessions: boolean;
	/** Session status bar under the video (issue #5). Off by default. */
	statusBar: boolean;
	/** Which facts the status bar shows (`StatusBar.svelte` STATUS_FIELDS). */
	statusFields: string[];
	/** The Pulsar menu button lives in the status bar instead of on the video. */
	statusBarMenu: boolean;
	/** Keep the status bar in fullscreen too (off: fullscreen gives the video the whole
	 * screen, issue #10). */
	statusBarFullscreen: boolean;
	/** Quick audio toggles in the status bar (`StatusBar.svelte` STATUS_BUTTONS, issue #9). */
	statusButtons: string[];
	/** Experimental (USB; Windows, Linux, macOS): session audio plays through a cabled DS4's headset jack
	 * while headphones are in. Off by default; untested on hardware. */
	ds4Headset: boolean;
	hdr: boolean;
	unattended: boolean;
	tray: boolean;
	/** Pad Guide/PS button brings a hidden/background Pulsar forward in gaming mode. */
	padWake: boolean;
	/** Controller input reaches the host while another app is focused (Moonlight's
	 * "process gamepad input in the background"). Off by default. */
	padBackground: boolean;
	/** Two-way clipboard sync with every open session (viewed and hosted). On by default;
	 * also switchable from the session overlay. */
	clipboardSync: boolean;
	/** Show the verbose host activity log under "connected devices". */
	debug: boolean;
	/** Moonlight-style frame pacing on the Linux native renderer: buffer ~1-2 frames and
	 * present at a steady cadence to smooth network/decode jitter (slightly higher latency).
	 * Off = present newest immediately (lowest latency). */
	framePacing: boolean;
	/** Always-on mini stats HUD over the video while the overlay is closed. */
	statsHud: boolean;
	/** Parsec-style always-visible overlay-open button (Pulsar mark) over the video. */
	overlayButton: boolean;
	/** Overlay-open button position — egui POINTS from the video's top-left (the
	 * renderer draws there; the webview hotspot mirrors it as CSS px ×1.25).
	 * Drag-movable in-session; default mirrors overlay.rs BTN_POS_DEFAULT. */
	overlayBtnPos: { x: number; y: number };
	/** Feature 1: whether to forward physical controllers to the host (both remote
	 * and game mode). When false the client reads controllers locally but does not
	 * send Gamepad frames. Default true. */
	forwardControllers: boolean;
	/** Feature 3: stable ordering of controllers by gilrs uuid hex. Index = player
	 * slot (0=Player1 …). Unknown UUIDs are appended at the end. Default [] (natural
	 * gilrs enumeration order). */
	controllerOrder: string[];
	/** Per-controller emulation target keyed by gilrs uuid hex: 'auto' (resolve from
	 * detected kind), 'xbox' (Xbox 360), or 'ds4' (DualShock 4). Absent = 'auto'. */
	controllerEmulation: Record<string, 'auto' | 'xbox' | 'ds4'>;
	/** PER-CONTROLLER vibration (rumble) strength, keyed by gilrs/SDL uuid hex: the host's
	 * motor magnitudes are scaled per pad on the client (off = motors stay still, strong =
	 * full). Pushed to the SDL pad manager via `set_controller_rumble`. Absent uuid =
	 * 'medium'. */
	controllerRumble: Record<string, RumbleStrength>;
	/** DualSense headphone level per pad uuid (0-100, default 100) while the session audio
	 *  plays through the pad's headset jack ("Kolun kulaklık girişi"). Set from the in-session
	 *  menu's bars (`ov set hpvol`) or here; pushed with `set_ds5_headset_volume`. */
	headsetVolume: Record<string, number>;
	/** Per-controller DISABLED flag keyed by uuid hex (true = off). A disabled pad isn't
	 * forwarded to the host and doesn't rumble. Absent = enabled. */
	controllerDisabled: Record<string, boolean>;
	/** Feature 2 Piece B: treat the DS4/DS5 touchpad as a relative mouse (pointer
	 * moves + left-click). Default true. */
	touchpadAsMouse: boolean;
	/** App-level personality (persisted): 'remote' = the general remote-desktop app
	 * (left sidebar, host capability); 'game' = the controller-first game-streaming
	 * shell (bottom dock, centered ID + numpad, no hosting). Toggled from the top bar;
	 * the CLI `--mode game` overrides it on launch. Default 'remote'. */
	appMode: 'remote' | 'game';
	/** Auto-install app updates (download + install + relaunch silently at idle
	 * launch). Default OFF — updates are surfaced in the chrome badge + modal and
	 * installed only on explicit user action. Kiosk (--connect) boots always
	 * auto-update regardless (appliance behavior, unchanged). */
	autoUpdate: boolean;
	/** Whether GAMING mode was left in fullscreen — persisted so the app reopens
	 * fullscreen in game mode the way the user last had it (the bottom-dock button /
	 * F11 toggle writes this; restored at startup only when appMode==='game'). Scoped
	 * to game mode so a fullscreen remote session never reopens fullscreen. Default false. */
	gamingFullscreen: boolean;
	/** DualSense passthrough settings. */
	ds5: Ds5Settings;
}

export const CODECS: { value: VideoCodec; label: string }[] = [
	{ value: 'auto', label: 'Otomatik' },
	{ value: 'h264', label: 'H.264' },
	{ value: 'h265', label: 'H.265' },
	{ value: 'av1', label: 'AV1' }
];

export const ENCODERS: { value: Encoder; label: string }[] = [
	{ value: 'auto', label: 'Otomatik (en iyi donanım)' },
	{ value: 'nvenc', label: 'NVIDIA NVENC' },
	{ value: 'qsv', label: 'Intel QuickSync' },
	{ value: 'amf', label: 'AMD AMF' },
	{ value: 'videotoolbox', label: 'Apple VideoToolbox' },
	{ value: 'vaapi', label: 'VA-API (Linux)' },
	{ value: 'rkmpp', label: 'Rockchip MPP' },
	{ value: 'software', label: 'Yazılım (CPU)' }
];

export const EMULATION_TARGETS = [
	{ value: 'auto', label: 'Otomatik' },
	{ value: 'xbox', label: 'Xbox 360' },
	{ value: 'ds4', label: 'DualShock 4' }
] as const;

export const RUMBLE_LEVELS: { value: RumbleStrength; label: string }[] = [
	{ value: 'off', label: 'Kapalı' },
	{ value: 'weak', label: 'Zayıf' },
	{ value: 'medium', label: 'Orta' },
	{ value: 'strong', label: 'Güçlü' }
];

/** Per-platform encoder families: a platform shows ONLY its own backends (foreign
 * entries never render); availability within the family is a separate, probe-driven
 * disabled state (see `caps.svelte.ts`). `auto` + `software` exist everywhere —
 * software is the universal terminal fallback. */
const PLATFORM_ENCODERS: Record<string, Encoder[]> = {
	linux: ['auto', 'nvenc', 'vaapi', 'qsv', 'rkmpp', 'software'],
	windows: ['auto', 'nvenc', 'amf', 'qsv', 'software'],
	macos: ['auto', 'videotoolbox', 'software']
};

export function encodersForPlatform(platform: string): { value: Encoder; label: string }[] {
	const family = PLATFORM_ENCODERS[platform] ?? PLATFORM_ENCODERS.linux;
	return ENCODERS.filter((e) => family.includes(e.value));
}

const DEFAULTS: UiSettings = {
	quality: 'auto',
	res: '1440p',
	fps: 60,
	bitrate: 0,
	streamRes: 'auto',
	streamFps: 0,
	codec: 'auto',
	encoder: 'auto',
	audioQuality: 'auto',
	mouseSens: 1,
	pointerFree: false,
	hostCursor: true,
	duplicateSessions: false,
	upscale: 'auto',
	statusBar: false,
	// The always-on stats HUD's set (codec · fps · latency · decode · bitrate).
	statusFields: ['codec', 'fps', 'latency', 'decode', 'bitrate'],
	statusBarMenu: false,
	statusButtons: ['speaker', 'mic'],
	statusBarFullscreen: false,
	ds4Headset: false,
	hdr: false,
	unattended: true,
	tray: true,
	padWake: false,
	padBackground: false,
	clipboardSync: true,
	debug: false,
	// Off by default (2026-11-08): pacing buffers ~1-2 frames — latency the maintainer would
	// rather not pay by default. On = smoother under network/decode jitter.
	framePacing: false,
	statsHud: false,
	overlayButton: true,
	overlayBtnPos: { x: 90, y: 70 },
	forwardControllers: true,
	controllerOrder: [],
	controllerEmulation: {},
	controllerRumble: {},
	headsetVolume: {},
	controllerDisabled: {},
	touchpadAsMouse: true,
	appMode: 'remote',
	autoUpdate: false,
	gamingFullscreen: false,
	ds5: DS5_DEFAULTS
};

const KEY = 'pulsar.ui.v1';
const hasLS = typeof localStorage !== 'undefined';

function load(): UiSettings {
	if (!hasLS) return { ...DEFAULTS, ds5: { ...DS5_DEFAULTS } };
	try {
		const raw = localStorage.getItem(KEY);
		if (!raw) return { ...DEFAULTS, ds5: { ...DS5_DEFAULTS } };
		const saved = JSON.parse(raw) as Partial<UiSettings> & { decoder?: string };
		delete saved.decoder; // removed: the decoder is auto-selected and shown read-only
		// 'quicksync' was the old UI value; the wire/host vocabulary is 'qsv'.
		if ((saved.encoder as string) === 'quicksync') saved.encoder = 'qsv';
		// One-time move to the new pacing default: every earlier save stored `framePacing: true`
		// (the old default) whether or not the user ever touched it, so flip it once.
		const migrated = saved as Partial<UiSettings> & { pacingOffV2?: boolean };
		if (!migrated.pacingOffV2) {
			migrated.framePacing = false;
			migrated.pacingOffV2 = true;
		}
		// One-time move to the 'auto' upscaling default (v0.26/0.27 saved 'off', the old default,
		// for everyone): an explicit FSR / AI pick stays.
		const up = migrated as Partial<UiSettings> & { upscaleAutoV1?: boolean };
		if (!up.upscaleAutoV1) {
			if (up.upscale === undefined || up.upscale === 'off') up.upscale = 'auto';
			up.upscaleAutoV1 = true;
		}
		return { ...DEFAULTS, ...migrated, ds5: normalizeDs5(migrated.ds5) };
	} catch {
		return { ...DEFAULTS, ds5: { ...DS5_DEFAULTS } };
	}
}

export const ui = $state<UiSettings>(load());

// A monotonically-rising tick bumped on every successful save. The Settings
// screen watches it (via $effect) to surface the "saved" toast — the tabs call
// saveUi() directly, so this is the only channel back up to the screen.
export const saveTick = $state({ n: 0 });

// A separate tick bumped ONLY when the CORE config (relay/unattended/avatar/audio…)
// is persisted — distinct from saveTick, which fires on every UI-only twiddle too
// (overlay-button drag, in-session stream controls). The shell (+page) re-fetches its
// config copy off THIS, so it doesn't churn IPC on UI-only saves.
export const configTick = $state({ n: 0 });

export function saveUi() {
	if (hasLS) localStorage.setItem(KEY, JSON.stringify($state.snapshot(ui)));
	saveTick.n++;
}

/** Return the player-slot index (0-based) for the given controller UUID.
 * Reads the live `ui.controllerOrder` permutation so reorders apply without a
 * page reload. Returns `ui.controllerOrder.length` (i.e. "append") for UUIDs
 * not yet in the list — callers should treat that as "last slot + 1". */
export function slotOf(uuid: string): number {
	const i = ui.controllerOrder.indexOf(uuid);
	return i >= 0 ? i : ui.controllerOrder.length;
}

/**
 * Swap two slot indices in a controller order array. Pure helper — no side effects.
 * Used by the overlay `ctrlswap` handler (Session.svelte) so the logic can be
 * unit-tested without a DOM or Tauri context.
 *
 * @param order  - Current order array (UUIDs at each slot position). Mutated in place.
 * @param seedFn - Called for each slot index that is missing from `order`; should
 *                 return the UUID of the pad that currently occupies that slot, or ''
 *                 if unknown. Called once per missing slot, in ascending slot order.
 * @param i      - First slot index to swap.
 * @param j      - Second slot index to swap.
 * @returns true if the swap was applied, false if i === j or indices are invalid.
 */
export function applyCtrlSwap(
	order: string[],
	seedFn: (slot: number) => string,
	i: number,
	j: number
): boolean {
	if (!Number.isFinite(i) || !Number.isFinite(j) || i === j || i < 0 || j < 0) return false;
	const maxSlot = Math.max(i, j);
	while (order.length <= maxSlot) {
		order.push(seedFn(order.length));
	}
	[order[i], order[j]] = [order[j], order[i]];
	return true;
}
