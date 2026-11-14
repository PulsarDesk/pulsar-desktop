// Shapes mirrored from pulsar-core (serde JSON). Keep field names in sync with
// the Rust `Config` (snake_case) and `NetworkMode` (kebab-case) serialization.

export type NetworkMode = 'auto' | 'p2p-only' | 'relay-only';
export type Language = 'tr' | 'en';

export interface Config {
	relay: string;
	/** Password for a relay that requires one (v4 relay auth). Empty = open relay. Never
	 * sent as-is — the client sends a nonce-bound proof; 2FA is a one-shot code, not stored. */
	/** Durable per-relay access keys (relay address → key), issued by the relay after a
	 * successful password/2FA prompt. Never a password: that is prompted and discarded. */
	relay_keys: Record<string, string>;
	/** Run the relay inside this app and register against it (Settings → Ağ toggle). */
	use_local_relay: boolean;
	network_mode: NetworkMode;
	/** Map Pulsar's UDP port on the router (NAT-PMP / UPnP) for direct P2P (default on). */
	port_mapping?: boolean;
	device_name: string;
	language: Language;
	unattended_access: boolean;
	/** Optional persistent connect password (alternative to the rotating one-time password). */
	connect_password?: string;
	/** Stream this host's audio to the client (host → client). */
	transmit_audio: boolean;
	/** Silence this host's local speakers while streaming. */
	mute_host_audio: boolean;
	/** Audio capture source override (empty = platform default). */
	audio_input: string;
	/** Local node listen port for direct/P2P (0 = pick automatically). */
	node_port: number;
	/** Identity presented to peers: 'user' (OS photo) | 'wallpaper' | 'anonymous'. */
	avatar_mode: string;
	/** Obsolete (video always renders natively); kept so old configs round-trip. */
	native_player: boolean;
	/** Two-factor auth for incoming connections: base32 TOTP secret ('' = off). */
	totp_secret?: string;
	/** Same-host co-op input isolation (opt-in ProtoInput component, Windows host). */
	coop_isolation?: boolean;
	/** Hardware acceleration for the APP'S OWN UI (the webview that draws menus/settings) —
	 * NOT the video stream's encode/decode. null = per-device default (off on Orange Pi 5,
	 * on elsewhere); true/false overrides. Applied at startup → needs an app restart. */
	ui_hardware_accel?: boolean | null;
	/** (Windows) Relaunch elevated (UAC) at startup so the host can control elevated app windows.
	 * Default true; applied at startup → needs an app restart. */
	request_admin?: boolean;
	/** Hold a new session on "optimizing stream" until the bitrate settles (default on). */
	pre_optimize_stream?: boolean;
	/** Host: show a controlling client's name next to the pointer (remote sessions). Default true. */
	show_remote_cursor?: boolean;
	/** Client: let the host show this device's name next to the pointer. Default true. */
	share_cursor_label?: boolean;
}

export type Transport = 'direct' | 'relay';

export interface ConnInfo {
	transport: Transport;
	peer: string;
}

/** CLI-parsed auto-connect target (from `pulsar --connect …`), widened to carry
 * the session mode and target app. `mode` defaults to 'remote'; `app` is the
 * host app/game id-or-name ('' = Desktop). Returned by `auto_connect_target`. */
export interface AutoConnectTarget {
	id: string;
	pw: string;
	mode: 'remote' | 'game';
	app: string;
	/** `--nofullscreen`: start the kiosk auto-connect session windowed, not fullscreen. */
	nofullscreen: boolean;
}

export type DeviceCategory = 'pc' | 'server' | 'console';

export interface Device {
	name: string;
	id: string;
	cat: DeviceCategory;
	online: boolean;
	fav: boolean;
	lastSeen: string;
}

export interface ControllerInfo {
	/** Positional index in the backend list. */
	index: number;
	/** Stable device key: gilrs uuid bytes as a lowercase hex string.
	 * Used as the key in `controllerOrder` for player-slot assignments. */
	uuid: string;
	/** OS/driver-reported name, e.g. "Wireless Controller". */
	name: string;
	/** Detected family tag, e.g. "Ds4" / "Xbox". */
	kind: string;
	/** Human label, e.g. "DualShock 4". */
	label: string;
	/** Connected + forwardable right now. */
	connected: boolean;
	/** Battery charge 0..100, or null for a wired pad / unknown. */
	battery?: number | null;
}

export interface SessionStats {
	fps: number;
	latency: number;
	bitrate: number;
}

/** One Pulsar toast notice (toast.rs): `file-in` = a file arrived (`file-del` / `file-mod`
 * reserved for remote delete/rename). Clicking opens `path`'s folder with it selected. */
export type PulsarToast = {
	id: number;
	kind: 'file-in' | 'file-del' | 'file-mod' | 'pad-off';
	name: string;
	peer: string;
	path: string | null;
};
