# Windows drivers — keyboard capture + virtual gamepad

Part of the pulsar-desktop agent docs — index: [`CLAUDE.md`](../../CLAUDE.md).

## Windows drivers: keyboard capture + virtual gamepad

Both are **bundled and auto-installed** (GPLv3 build is license-clear):

- **Keyboard capture under ASTER** (`src-tauri/src/kbdhook.rs` + `kbdhook/imp.rs`).
  `WH_KEYBOARD_LL` misses ASTER-multiseat physical keys, so we load the
  **Interception** driver's `interception.dll` at runtime (`libloading`) and
  capture below the hook layer; LL-hook is the fallback. Set-1 scancodes → evdev
  via `scancode_to_evdev`; leave-combo + suppress/forward logic shared
  (`handle_key`). Interception: LGPL-3.0 non-commercial w/ redistribution — fine
  for GPLv3; commercial Pulsar would need their commercial license.
- **Virtual gamepad** (pulsar-core `input`, `vigem.rs`): **ViGEmBus** (X360 + DS4
  via the vendored `vigem-client` fork). Linux uses uinput; macOS is a no-op stub.

**Bundling:** `interception.dll` ships next to the exe; the NSIS installer runs
the Interception + ViGEmBus silent installers (elevated) with a "restart
required" notice. Payloads fetched by `scripts/fetch-drivers.mjs` into
`src-tauri/resources/`.

**In-app updates (Windows):** the updater runs the NSIS installer **quietly**
(`plugins.updater.windows.installMode: "quiet"`), so no installer wizard appears. Right
before `install()` the frontend calls `launch_update_window`, which copies
`resources/pulsar-update.exe` to `%TEMP%` and starts it as
`--updating <ver> --old-pid <pid> --app <exe>`. That exe is `crates/pulsar-setup` built
**without** a payload; its `--updating` screen shows progress and closes once the new
build runs in the same session. If the installer never starts (UAC declined) or the app
does not come back, it offers "Pulsar'ı aç", which passes `--update-failed` so the stale
update marker does not block the launch. CI builds it in the "Build update window" step.
A local full `tauri build` needs it too:
`cargo build --release -p pulsar-setup && cp target/release/pulsar-setup.exe src-tauri/resources/pulsar-update.exe`.
Mock run for screenshots: `pulsar-setup --updating 0.0.0 --mock-updating`.
