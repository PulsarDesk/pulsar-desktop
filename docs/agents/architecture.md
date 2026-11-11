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
That is **per-user mode**; with the setting "Kullanıcılar arası ortak Pulsar" on, the identity
and the other data files are machine-wide instead (next section). Every data-file path goes
through `util::data_dir` (`config_path`, `identity_path`, known peers, `pairing.rs`,
`device_creds.rs`); never build one from `app_config_dir()` directly.

## Shared (machine-wide) mode — "Kullanıcılar arası ortak Pulsar"

`src-tauri/src/shared_mode.rs`, Settings → Genel. Windows only (hidden elsewhere).

| | **ON — one Pulsar per PC** | **OFF — per-user (the old behaviour)** |
| - | - | - |
| identity / ID | one, `%ProgramData%\Pulsar\shared\identity.key` | per Windows user, `%APPDATA%\dev.pulsar.app\identity.key` |
| config, `paired_devices.json`, `known_peers.json`, `device_creds.json`, saved-devices list (`peers.json`) | shared dir | per-user dir (saved devices: webview localStorage) |
| hosts | one machine-wide: whoever holds `Global\PulsarSharedHost` | every user's Pulsar (Duo seats hosted separately) |

- **Switch:** `%ProgramData%\Pulsar\mode.json` `{"shared": bool}`. Read once per process
  (`shared_mode::resolved()`, first call in `run()`); a change applies on restart. The toggle
  (`set_shared_mode`) restarts the app; other users' Pulsar follows on its next start.
- **Default (`decide`, unit-tested):** marker present → it wins. No marker: if ANY profile has
  per-user Pulsar data (`identity.key` or `config.json`, not marked provisional) → OFF, and an
  elevated launch writes `{"shared": false}` (so a new Windows user on that PC doesn't flip it).
  Fresh machine + elevated → ON (`{"shared": true}`). Fresh but not elevated (UAC declined) →
  per-user for that launch only; the data is marked `.shared-mode-provisional` and doesn't count
  as an existing install, so the next elevated launch still defaults ON and adopts it.
  **"Default ON" therefore means fresh installs only — an existing per-user ID never changes
  unless someone turns the setting on.**
- **Migration (`migration_plan`, unit-tested):** going ON copies the CURRENT user's files into
  the shared dir only when it has no `identity.key` yet (their ID becomes the PC's); a shared dir
  that already has an identity is kept whole (its pairings belong to it). Copies are read+write so
  they inherit the shared ACL; `device_creds.json` is re-sealed from user-scope to machine-scope
  DPAPI (`"dpapi-m"`). Other users' files are never read or changed. Going OFF only flips the
  marker: every user is back on their untouched per-user files (changes made while shared, e.g.
  new pairings, stay in the shared dir and come back when it is turned on again).
- **Security:** the shared dir has a protected DACL `SYSTEM + Administrators` only;
  `%ProgramData%\Pulsar` itself adds Users read (for `mode.json`). An existing dir must be
  owned by SYSTEM/Administrators and not be a reparse point, else shared mode refuses it (a
  standard user could pre-create it to read the key). So only an **elevated** Pulsar uses shared
  mode; `main.rs` always elevates when the marker is on. A non-elevated one (UAC declined, or a
  standard user) gets `SHARED_NO_ACCESS`: no hosting, a note in Settings. Why not the sysagent:
  it isn't deployed yet, and a key-serving or crypto-oracle pipe would add SYSTEM attack surface
  (and a core `Identity` change) for the standard-user case only; the elevated path is the same
  trust level as reading the key yourself as admin. Brokering through the sysagent is the
  follow-up for standard-user machines.
- **One host (`shared_mode::lease`):** a dedicated thread owns the `Global\PulsarSharedHost`
  mutex (SDDL SYSTEM + Administrators). Every 2 s: not held + our WTS session `WTSActive` → try
  to take it (abandoned = taken) → `shared-host {held:true}` → the UI calls `go_online` (no UI:
  `headless::restart`). Held + session not Active for 2 ticks (fast user switch, RDP takeover,
  disconnect — a LOCKED session stays Active, so a locked host stays up) → mark not held, take
  the node fully offline (`host::go_offline`: serve/session tasks, node, LAN beacon), release,
  `{held:false}`. `bring_online` refuses without the lease (`SHARED_STANDBY:<user>`); the UI
  shows "Bu bilgisayarın Pulsar'ı şu an <user> oturumunda yayında" and keeps its retry loop.
  Logoff/crash releases the mutex via the OS. The holder writes `host.json` (user, session, ID).
- **Not done:** a standby Pulsar has no node, so it can't connect OUT either (would need an
  ephemeral client identity); a session whose user never started Pulsar gets no host (follow-up:
  the SYSTEM sysagent launches `pulsar.exe` in the active session, Parsec/Sunshine-style); Duo
  multi-seat with shared ON = first Active seat hosts; the toggle doesn't notify other users'
  running Pulsar.

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
