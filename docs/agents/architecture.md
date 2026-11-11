# Architecture — Node, relay config, UI bridge, stable ID

Part of the pulsar-desktop agent docs — index: [`CLAUDE.md`](../../CLAUDE.md).

## Core concepts

- **`Node`** (pulsar-core `connection`) is the heart: `register()` gets the
  relay-assigned ID; `connect()` does rendezvous + hole-punch and returns a
  `Session` (`Direct` or `Relay` transport, E2E encrypted).
- **Configurable relay**: `Config.relay` + `Config.network_mode`
  (`auto`/`p2p-only`/`relay-only`), user-editable in Settings → Ağ, persisted.
- **UI ↔ core**: `lib/api.ts` calls Tauri commands in Tauri, falls back to a
  deterministic mock so `vite dev` + tests work without the native shell.

## Stable device ID

The relay maps **pubkey → id** (`by_pubkey` in the `relay` repo), so a returning
device keeps its 9-digit ID. Client persists its X25519 identity per-user
(`Identity::load_or_create` → `<app_config_dir>/identity.key`,
`Node::bind_with_identity`). ID is stable across restarts, distinct per OS user
(ASTER seats keep separate IDs). Per-user single-instance guard (Windows `Local\`
named mutex).

## Auth: saved device passwords + pairing

- **Saved password** (Devices → add/edit, `DeviceFormModal`): stored by Rust in
  `<app_config_dir>/device_creds.json` (`device_creds.rs`), DPAPI-sealed on Windows, 0600
  plain elsewhere — never localStorage. `client_authenticate` sends it up front as a PAKE
  offer (and repeats the offer, not the empty probe, while it waits), so a matching
  password never opens the prompt. Relay ids are keyed per relay, addresses alone.
- **Pairing** (`pairing.rs`, wire: core `DataMsg::PairRequest/Unpair/PairResult`): the host
  trusts the client's static X25519 key (`Session::peer_pubkey`, same identity as
  same-device pre-auth) in `<app_config_dir>/paired_devices.json`. A paired key skips the
  password and the Allow/Deny popup and is checked before the per-peer lockout; **2FA still
  applies**. Pairing is never silent: a session that proved a password/OTP pairs at once,
  any other (operator Allow, pre-auth, unattended) gets a pairing popup (`Approve.svelte`,
  `pw = "pair"`). Settings → Güvenlik lists and revokes paired keys; revoking also drops the
  key's 90 s pre-auth grace. The client unpairs locally and, when the host is reachable,
  sends `Unpair`. Decision logic (`gate`, `needs_totp`, `pair_decision`) is unit-tested.

## Startup without a window (locked session)

The host core must come online even when no webview can be created. Inside a LOCKED Windows
session (LogonUI up, e.g. an unattended Duo seat, autostart, a restart after a crash) WebView2
controller creation hangs or fails after ~2 min with `0x800705B4`, and Tauri builds config
windows before `setup` and panics on failure. So the main window is `create:false` in
`tauri.conf.json` and built by `ui_start.rs`: right away in a usable session (normal startup
unchanged); deferred while the session is locked (WTS lock flag / input desktop name, polled
every 2 s) or retried with a backoff after a failed build. Meanwhile `headless.rs` runs
`go_online` itself (same 3→12 s retry as the UI); the UI's first plain `go_online` adopts that
node instead of rebinding, so sessions accepted while locked survive the window showing up. A
60 s watchdog does the same when a built UI never calls `go_online`. While locked the host opens
no approval popup (password only), connections window, toast or remote-cursor window.
Test hooks: `PULSAR_TEST_LOCKED=<secs>`, `PULSAR_TEST_WEBVIEW_FAIL=1`.
