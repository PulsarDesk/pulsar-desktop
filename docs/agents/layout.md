# Layout — where things live

Part of the pulsar-desktop agent docs — index: [`CLAUDE.md`](../../CLAUDE.md).

## Layout

```
pulsar-desktop/
  crates/pulsar-capture/  # Windows-only native capture+encode: DXGI Desktop
                          #   Duplication (+ WGC) → NVENC SDK → hand-rolled RTP
                          #   (the Sunshine technique, no ffmpeg). Also
                          #   nvenc_codecs() real-silicon probe via nvEncodeAPI64.dll.
  crates/pulsar-render/   # native renderer PROCESS for ALL platforms: Linux
                          #   rkmpp/EGL, Windows Media Foundation + D3D11 zero-copy
                          #   decode, egui in-session overlay, `--probe` decoder probe.
  crates/pulsar-setup/    # egui branded Windows bootstrapper installer
                          #   (Discord-style, embedded payload, silent/uninstall).
  src-tauri/              # `pulsar-tauri` crate (bin name `pulsar`)
    src/lib.rs            #   command registration / glue
    src/host.rs,host/     #   host side; handlers.rs = capture/encode/audio spawn
    src/caps.rs           #   startup capability probe (see below)
    src/process.rs        #   ffmpeg spawning, ffmpeg_bin(), encoder validation
    src/viewer.rs         #   UDP→WebSocket relay (now audio-only path to webview)
    src/native_view*/     #   embedded native video (spawn.rs: spawn_mpv & co.)
    src/kbdhook.rs,kbdhook/ # OS-key capture: Windows Interception/LL-hook, Linux evdev
    src/relay_mode.rs     #   `pulsar --relay …` — the app can run as a relay
    src/{auth,avatar,connections,files,fs_browse,audio_io,io_cmds,…}.rs
    tauri.conf.json       #   frameless 1200×780 window; updater plugin (minisign)
  src/                    # SvelteKit frontend
    routes/+page.svelte   #   shell; routes/page/{Tabs,SplitPicker,Chrome,Sidebar,
                          #     GamingShell,UpdateModal,…}.svelte — multi-session
                          #     tabs + split view
    lib/api.ts            #   bridge, split into api.{commands,dom,events,invoke,types}.ts
    lib/i18n/             #   4-language lazy i18n: en/tr/ru/kk (+ src-tauri/lang/)
    lib/screens/          #   Home, Devices, Settings/(4 tabs), Connecting, Session/,
                          #     Gaming/(6 comps), Games/, Approve, Connections,
                          #     FilesWindow, HostChat
  scripts/                # fetch-ffmpeg.mjs, fetch-drivers.mjs, fetch-player.mjs,
                          #   gen-update-manifest.mjs, bump-version.mjs
```
