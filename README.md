<p align="center">
  <img width="120" height="120" src="https://avatars.githubusercontent.com/u/290144039?s=240" alt="Pulsar logo">
</p>

<h1 align="center">Pulsar Desktop</h1>

<p align="center">
  Remote desktop and game streaming for <b>Windows, macOS and Linux</b>. A free, open-source alternative to TeamViewer, AnyDesk and Parsec. ⚡
</p>

<p align="center">
  <img src="https://img.shields.io/maintenance/yes/2026?style=for-the-badge" alt="Maintained">
  <a href="https://github.com/PulsarDesk/pulsar-desktop/releases"><img src="https://img.shields.io/github/v/release/PulsarDesk/pulsar-desktop?include_prereleases&style=for-the-badge" alt="Release"></a>
  <a href="https://github.com/PulsarDesk/pulsar-desktop/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/PulsarDesk/pulsar-desktop/ci.yml?branch=main&style=for-the-badge&label=CI" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/PulsarDesk/pulsar-desktop?style=for-the-badge" alt="License"></a>
</p>

---

## 🚀 Project Status
**Current Status:** Alpha (0.x). Connecting, remote control, game streaming, file transfer, clipboard and chat work.
Expect rough edges and [report what you find](#-reporting-a-bug).

<p align="center">
  <img src="docs/screenshots/connect.png" width="49%" alt="Connect screen">
  <img src="docs/screenshots/game-mode.png" width="49%" alt="Game mode">
</p>

One app does both jobs: connect to another computer, or share this one. Connections go peer-to-peer when they can
and through a relay when they can't, always end-to-end encrypted.

- **Holds up on weak links:** it measures the connection before the picture starts, then keeps adjusting bitrate,
  error correction and resolution, so a phone hotspot or a busy Wi-Fi stays usable.
- **Audio:** Opus at up to 320 kb/s, or uncompressed on a local network (Settings -> Display -> Audio quality,
  *Auto* picks the highest the link carries). On a Windows host its own volume and mute don't affect what you hear.
- **Games:** when a game grabs the mouse, Pulsar sends raw mouse motion (with a sensitivity setting), passes
  controllers through, and can keep the host's speakers silent.

---

## 📦 Other Pulsar Repositories
- [**pulsar-mobile**](https://github.com/PulsarDesk/pulsar-mobile) - Pulsar for Android
- [**relay**](https://github.com/PulsarDesk/relay) - Run your own relay server
- [**pulsar-core**](https://github.com/PulsarDesk/pulsar-core) - The shared engine behind both apps

---

## ⚡ Trying Pulsar
Download the installer for your system from the [latest release](https://github.com/PulsarDesk/pulsar-desktop/releases/latest):

| System | File |
|---|---|
| Windows | `_x64-setup.exe` (or `_arm64-setup.exe`) |
| macOS | `_aarch64.dmg` (Apple Silicon) or `_x64.dmg` (Intel) |
| Linux | `.AppImage`, `.deb` or `.rpm` (x64 and arm64) |
| Orange Pi 5 (RK3588) | `_opi5_arm64.deb`: hardware video decoding; needs Ubuntu 22.04 with the `ppa:jjriek/rockchip-multimedia` packages |

Open Pulsar, share your **ID and one-time password** with the other side, or type theirs to connect.

- **Windows:** the installer is not code-signed, so SmartScreen may warn: "More info" -> "Run anyway".
- **macOS:** the app is not notarized: right-click it -> Open the first time.
- **Linux on Wayland:** sharing your screen asks for permission once per session (the system's screen-share dialog).

---

## 🖥️ Where it has been tested
Every computer falls back to software encoding and decoding when its graphics card can't be used, so a session
should always show a picture, only slower.

| System | Graphics | Share this computer | Connect to others |
|---|---|---|---|
| Windows 11 | NVIDIA RTX 3080 laptop | ✅ hardware (NVENC) | ✅ hardware |
| Windows 11 on ARM | Snapdragon (Adreno) | ✅ hardware (Media Foundation) | ✅ hardware |
| Linux (KDE Plasma, Wayland) | AMD Radeon iGPU + NVIDIA RTX laptop | ✅ hardware | ✅ hardware |
| Linux (Raspberry Pi, Orange Pi 5 / RK3588) | built-in | ✅ | ✅ hardware on the Orange Pi 5 |
| Windows 10 | NVIDIA MX230 + Intel UHD | ✅ software (no NVENC on this GPU) | ✅ hardware |
| Windows with an AMD GPU | — | ❔ not tested yet | ❔ not tested yet |
| macOS | — | ❔ builds, not tested | ❔ builds, not tested |
| Linux on GNOME, Intel GPUs | — | ❔ not tested yet | ❔ not tested yet |

The Linux `.AppImage` starts on Fedora 41, Ubuntu 24.04, Debian 12, openSUSE Tumbleweed and Arch (checked on every
release). It needs a system at least as new as Ubuntu 22.04. On Linux, `Pulsar.AppImage --selftest` shows which
encoders work on your machine.

Tried it on something not listed? A short note in an [issue](https://github.com/PulsarDesk/pulsar-desktop/issues)
helps, even when everything worked.

---

## 🎮 Controllers
Plug a controller into the computer you connect **from** (USB or Bluetooth). Pulsar reads it through SDL3, so any
pad your system sees works: Xbox 360 / One / Series, PlayStation (DualShock 3, DualShock 4, DualSense), Switch Pro
and generic pads. On the computer you connect **to**, games see a virtual controller:

| Your controller | Windows host | Linux host | macOS host |
|---|---|---|---|
| Xbox, Switch Pro, generic | Xbox 360 pad: buttons, sticks, triggers (no rumble yet) | virtual pad: buttons, sticks, triggers (no rumble yet) | ❌ not supported |
| DualShock 3 / 4 | DualShock 4: rumble, touchpad, PS button | virtual pad (no rumble yet) | ❌ not supported |
| DualSense | **a real DualSense** (with "Use DualSense features" on): adaptive triggers, HD haptics, lightbar and LEDs, speaker, headset jack, microphone, gyro, touchpad, over USB or Bluetooth | **a real DualSense**, same as Windows | ❌ not supported |

- **Which virtual pad:** *Automatic* picks DualShock 4 for PlayStation pads and Xbox 360 for the rest. You can pick
  per controller in the Controllers panel (the Games screen, or the controller button in game mode) or in the
  session menu.
- **Drivers:** the Windows installer adds ViGEmBus (virtual Xbox / DualShock 4 pads) and usbip-win2 (the virtual
  DualSense). Linux uses the kernel's own `uinput` and `vhci-hcd`.
- **Headphones in the controller:** with headphones in a DualSense, the session sound plays there instead of on your
  computer (like a PS5). The same for a USB DualShock 4 (v2 or Sony's USB adapter) is **experimental** and off by
  default: Controllers panel -> "DS4 headset jack (experimental)".
- **Background:** controllers only reach the host while Pulsar is the focused window, unless you turn on
  Settings -> General -> "Controller input in the background".
- **Not tested yet:** the DualSense microphone and a USB-cabled DualSense on the connecting side, and the DS4 headset
  jack. Tried one? Tell us in an [issue](https://github.com/PulsarDesk/pulsar-desktop/issues).

---

## 🐞 Reporting a bug
[Open an issue](https://github.com/PulsarDesk/pulsar-desktop/issues/new/choose) and include:

- Pulsar version and your system on **both sides** (Windows / macOS / Linux, and X11 or Wayland on Linux)
- Graphics card on both sides
- What you did and what happened (black screen, no connection, crash, ...)
- The log file: Settings -> General -> **Open log folder**
- On Linux: the output of `Pulsar.AppImage --selftest`

---

## 🛠️ Requirements
**To use:** Windows 10 or newer, macOS, or a 64-bit Linux (x64 or arm64). A graphics card with hardware video
encoding helps but is not required.

**For contributors / development:** Rust stable, [bun](https://bun.sh); on Linux also the GTK/WebKit and GStreamer
development packages.

---

## 🤝 Contributing
Everyone can open [issues](https://github.com/PulsarDesk/pulsar-desktop/issues) and pull requests. Irrelevant or
abusive issues may be closed or removed. Commits follow [Conventional Commits](https://www.conventionalcommits.org).

### Development Guide
```bash
git clone https://github.com/PulsarDesk/pulsar-desktop.git
cd pulsar-desktop
bun install
bun run tauri dev        # run the app
bun run test:unit        # UI tests
```

More: [docs/development.md](docs/development.md) (layout, encoders per platform, tests, releases).

---

## 📄 License
Pulsar is licensed under **GNU GPLv3 or later** (GPL-3.0-or-later). See the [LICENSE](LICENSE) file for details.
