# Usage modes + CLI/headless start

Part of the pulsar-desktop agent docs — index: [`CLAUDE.md`](../../CLAUDE.md).

## Two usage modes (product direction — keep consistent everywhere)

Pulsar is ONE app with **two mode-aware personalities**, chosen at connect time
(`startConnect(target, mode: 'remote' | 'game')`). Mode drives **menu content,
overlay content, the look, and the encode profile**:

| | **Remote Desktop** (AnyDesk/RustDesk) | **Game Streaming** (Moonlight/Parsec) |
| - | - | - |
| Focus | general remote control + management | lowest latency, gaming |
| Menu | **full**: resolution/quality · codec/encoder · **file transfer · clipboard · multi-monitor** · chat · mic · reverse-direction · settings | **slim, game-only**: codec · bitrate (Mbit) · **fps** · resolution · quality/perf · encoder/decoder · controllers · end. **NO file/clipboard/mic/multi-monitor** |
| Overlay | thin info strip (connection/transport) | perf HUD (latency/fps/bitrate) + controller status + leave-combo hint |
| Look | neutral/general | gaming (cyan accent, immersive — `data-gaming`) |
| Encode | quality-focused | low-latency (mode-aware on the host) |

Established with the maintainer: **entering game streaming makes the whole app
gaming-focused; remote desktop makes it general remote-control-focused.**

## CLI / headless start (kiosk / appliance — esp. Orange Pi)

`pulsar --connect <id|ip> [--connect-pw <pw>] [--mode game]` auto-connects on
launch (splash, no home screen; `AppState.auto_connect`). Game mode's target app
defaults to **"Desktop"** — *always present, NOT deletable* (every host exposes a
"Desktop" entry). `pulsar --relay --session-rate 10mbit …` runs the app as a
relay (`relay_mode.rs`, same `pulsar_relay::Limits` flags as the relay repo).
