<p align="center">
  <img width="120" height="120" src="https://raw.githubusercontent.com/PulsarDesk/.github/main/profile/logo.svg" alt="Pulsar logo">
</p>

<h1 align="center">Pulsar Desktop</h1>

<p align="center">
  Remote desktop and game streaming for <b>Windows, macOS and Linux</b>. Free, open source, no account. ⚡
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

One app does both jobs: connect to another computer, or share this one. Connections go peer-to-peer when they can
and through a relay when they can't, always end-to-end encrypted.

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
| Linux (KDE Plasma, Wayland) | AMD Radeon iGPU + NVIDIA RTX laptop | ✅ hardware | ✅ hardware |
| Linux (Raspberry Pi, Orange Pi 5 / RK3588) | built-in | ✅ | ✅ |
| Windows 10 | NVIDIA MX230 + Intel UHD | ✅ software (no NVENC on this GPU) | ✅ hardware |
| Windows with an AMD GPU | — | ❔ not tested yet | ❔ not tested yet |
| Windows on ARM | — | ❔ builds, not tested | ❔ builds, not tested |
| macOS | — | ❔ builds, not tested | ❔ builds, not tested |
| Linux on GNOME, Intel GPUs | — | ❔ not tested yet | ❔ not tested yet |

The Linux `.AppImage` starts on Fedora 41, Ubuntu 24.04, Debian 12, openSUSE Tumbleweed and Arch (checked on every
release). It needs a system at least as new as Ubuntu 22.04. On Linux, `Pulsar.AppImage --selftest` shows which
encoders work on your machine.

Tried it on something not listed? A short note in an [issue](https://github.com/PulsarDesk/pulsar-desktop/issues)
helps, even when everything worked.

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
