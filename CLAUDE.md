# Pulsar desktop app

Cross-platform (Windows/macOS/Linux) remote-desktop + game-streaming app. See
`../CLAUDE.md` (umbrella dir) for the product and multi-repo architecture.

**Stack:** **Tauri 2** shell · **SvelteKit (Svelte 5)** UI · **Rust**. One app is
both the *client* (connect to others) and the *host* (share this machine).

This file is the **index**. Read the topic file before working in its area:

| Topic | File | Read before touching… |
| --- | --- | --- |
| Where things live | [docs/agents/layout.md](docs/agents/layout.md) | anything — repo map (`crates/`, `src-tauri/`, `src/`, `scripts/`) |
| Architecture | [docs/agents/architecture.md](docs/agents/architecture.md) | `Node`/sessions, relay config, the UI↔Rust bridge, the stable device ID |
| Host capture/encode | [docs/agents/host-encode.md](docs/agents/host-encode.md) | capture paths per OS, encoders, codecs, `caps.rs` probe |
| Client video | [docs/agents/client-video.md](docs/agents/client-video.md) | `pulsar-render`, native video on Linux/RK3588, the gaming overlay |
| Windows drivers | [docs/agents/windows-drivers.md](docs/agents/windows-drivers.md) | keyboard capture (Interception), ViGEmBus, the installer payload |
| Audio | [docs/agents/audio.md](docs/agents/audio.md) | the Opus/RTP audio stream, mute policy, WASAPI loopback |
| Usage modes + CLI | [docs/agents/modes.md](docs/agents/modes.md) | remote-vs-game behaviour, menus, `--connect` / `--relay` flags |
| UI conventions | [docs/agents/ui.md](docs/agents/ui.md) | modals, Escape, text selection, window controls, theming |
| Status | [docs/agents/status.md](docs/agents/status.md) | what's done vs scaffolded, adaptive streaming, unverified hardware |

## Must-know rules

- **The shared engine is NOT in this repo.** `pulsar-core`, `pulsar-proto` and
  `pulsar-relay` are **git dependencies** (`Cargo.toml` `[workspace.dependencies]`),
  plus `[patch.crates-io] vigem-client = { git = …/pulsar-core }`. After pushing
  pulsar-core/proto, bump this repo with `cargo update`. A sibling checkout lives at
  `../pulsar-core/` but is *not* a path dep.
- This repo has its **own `rust-toolchain.toml`** (stable; some deps need
  edition 2024 — run `rustup update stable` if you hit an edition error). Use
  **`bun run tauri dev`** to run the app — `cargo tauri` would need a
  separately-installed `cargo-tauri`. **Package manager: bun, never npm.**
- **Never push a behaviour change before the maintainer tested it** in the real app.
- **Video renders natively (`pulsar-render`), never in the webview** — see client-video.md.

## Run & build

```bash
bun install
bun run tauri dev      # the real app (NOT `cargo tauri`)
bun run tauri build    # installers (bundles ffmpeg — see below)
bun run dev            # UI only in a browser (mock)
bun run build          # static SPA → build/
```

> **Bundled ffmpeg:** host capture/encode fallback ships ffmpeg inside the app at
> `src-tauri/resources/ffmpeg[.exe]` — git-ignored, fetched per-platform by
> `scripts/fetch-ffmpeg.mjs` (CI runs it automatically). At runtime `ffmpeg_bin()`
> (`src-tauri/src/process.rs`) resolves the bundled copy first, then PATH.

Dev-loop gotchas:

- `tauri dev` never rebuilds the `pulsar-render` sidecar — run `cargo build -p pulsar-render`
  first, or a stale binary (old libav sonames) leaves the Linux viewer black.
- A change under `src-tauri/` (code, `capabilities/`) makes `tauri dev` rebuild and relaunch;
  if the old window is still open the new instance hits the single-instance guard and exits,
  taking `tauri dev` (and vite) with it. Close the app and start `tauri dev` again.

## Test

```bash
cargo check -p pulsar-tauri     # Tauri bridge compiles against the engine
cargo test -p pulsar-capture    # workspace default-member tests
bun run test:unit               # UI components (vitest)
bun run check                   # svelte-check
# engine tests live in ../pulsar-core (its own repo): cargo test there
```
