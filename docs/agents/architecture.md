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
