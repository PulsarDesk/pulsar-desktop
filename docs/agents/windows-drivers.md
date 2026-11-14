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
  via the vendored `vigem-client` fork). Linux uses uinput; macOS an `IOHIDUserDevice`
  virtual HID pad (`mac_hid.rs`, needs the virtual-HID entitlement — see status.md). No
  ViGEmBus → the client gets a `PadUnavailable` notice (`vigem_missing`) and the host a toast.
- **Virtual DualSense** (pulsar-core `input::ds5`): **usbip-win2** — see below.

**Bundling:** `interception.dll` ships next to the exe; the NSIS installer runs
the Interception + ViGEmBus + usbip-win2 silent installers (elevated) with a "restart
required" notice. Payloads fetched by `scripts/fetch-drivers.mjs` into
`src-tauri/resources/` (`bun scripts/fetch-drivers.mjs [--arch x64|arm64]`; default = this
machine's arch). Every payload must also be listed under `bundle.resources` in
`tauri.windows.conf.json`, or Tauri never copies it into `$INSTDIR` and the hook's
`IfFileExists` silently skips it. (Until 2026-11-11 `ViGEmBus_Setup.exe` was fetched but not listed, so no NSIS
install ever ran it. `install-interception.exe` is still neither extracted nor listed: the
Interception step is a no-op, see the umbrella `windows-input-plan.md`.) For a plain `cargo check`/`build`, zero-byte stubs at the
listed paths are enough (`resources/usbip/` needs at least one file, e.g. an empty
`USBip-x64.exe`, because the glob must match).

**Uninstall policy:** Pulsar's uninstallers (NSIS and `pulsar-setup --uninstall`) remove
none of the drivers (Interception, ViGEmBus, Virtual-Audio-Driver, usbip-win2). They are
shared system components with their own uninstallers, and removing a kernel driver another
app uses would break it.

## usbip-win2 (virtual wired DualSense on the host)

**Why:** a DualSense's haptics, adaptive triggers, speaker, mic and lightbar only work in
games (and in Windows' own audio stack) when the pad is a real USB device, not a HID-only
ViGEm target. pulsar-core runs a tiny USB/IP server on `127.0.0.1` that emulates a wired
DualSense (`input/ds5/{usbip,device,virtual_pad}.rs`); usbip-win2's vhci driver attaches it,
so Windows sees a genuine USB `VID_054C&PID_0CE6` device. Design: pulsar-core
`docs/dualsense.md`.

**Where the host uses it:** `pulsar_core::input::ds5::attach::locate` looks for
`%ProgramFiles%\USBip\usbip.exe`, then `usbip.exe` on `PATH`, and runs
`usbip attach -r 127.0.0.1 -b <busid>` / `detach` (elevated process). Desktop
`src-tauri/src/ds5/host.rs::available()` advertises `FEAT_DS5` only when that lookup
succeeds, so a host without usbip-win2 just does not offer the virtual DualSense.

**Version:** **0.9.8.1** (github.com/vadimgrn/usbip-win2), signed Inno Setup installers
`USBip-0.9.8.1-x64.exe` / `USBip-0.9.8.1-arm64.exe`. `fetch-drivers.mjs` pins URL + the
sha256 GitHub publishes, fetches ONLY the target arch into `resources/usbip/USBip-<arch>.exe`
(deletes the other arch's file) and fails on a hash mismatch. `tauri.windows.conf.json`
bundles `resources/usbip/*`. CI passes `${{ matrix.ffmpeg }}` (`--arch arm64` for the
cross-built ARM64 job). Bump the version in three places together: `fetch-drivers.mjs`
(`USBIP_VERSION` + both sha256), `installer-hooks.nsh` (`USBIP_VERSION`),
`crates/pulsar-setup/src/drivers.rs` (`USBIP_VERSION`).

**Install (only when missing or older):** both installers read `DisplayVersion` from the Inno
uninstall key `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{199505b0-b93d-4521-a8c7-897818e0205a}_is1`
(64-bit view). Missing key + no `%ProgramFiles%\USBip\usbip.exe` → install; key older than
the bundled version → install; otherwise skip. A `usbip.exe` without the key (manual or
source install) is left alone. Command: `USBip-<arch>.exe /VERYSILENT /SUPPRESSMSGBOXES
/NORESTART /SP- /NOICONS /COMPONENTS=main,client /TASKS=vcredist`. The flags skip the
GUI (`wusbip.exe`), SDK and PDB components, so the `desktopicon` task (tied to `gui`) never
runs, and `/NOICONS` (the script sets `AllowNoIcons=yes`) skips the Start-menu group; `main`
and `client` are `fixed`, `vcredist` is kept because `usbip.exe` needs the VC++ runtime (no reboot was needed on the test PC; the generic restart notice covers it).
- **NSIS** (`installer-hooks.nsh`, perMachine, already elevated): picks the file by the
  NATIVE arch (`${IsNativeARM64}` / `${IsNativeAMD64}`). A kernel driver must match the
  native arch, so an x64 Pulsar on an ARM64 PC finds no matching file and skips. Runs on
  updates too, but the version check makes that a no-op until the bundled version changes.
- **pulsar-setup** (per-user, no UAC for the app itself, `src/drivers.rs`): after the files,
  shortcuts and registry entry, the interactive install runs the bundled installer through
  its OWN UAC prompt (`Start-Process -Verb RunAs`), progress line "Sanal DualSense sürücüsü
  (usbip-win2)…". A declined prompt or failure never fails the Pulsar install. The headless
  `/S` path never runs it (no surprise UAC prompt). CI copies `resources/usbip/USBip-*.exe`
  into the setup payload.

**Windows Firewall (both installers):** Windows asks "allow access?" once per program that
listens (pulsar, pulsar-render, ffmpeg, ffplay). Instead the app adds one inbound allow rule
per program, named `Pulsar (<file>)` and scoped to its path: `pulsar.exe --firewall-setup` /
`--firewall-remove` (`src-tauri/src/firewall.rs`, needs elevation). NSIS runs them in its
post-install / pre-uninstall hooks (already elevated). pulsar-setup checks the rule
(`netsh … show rule`, no elevation needed) and when it is missing runs the app through ONE UAC
prompt (`src/firewall.rs`); the headless `/S` path skips it. An elevated app also re-checks at
startup. Rules are per path, so the per-user installs of several Windows users on one PC each
keep their own rule; removal deletes only this install's.

**usbipd-win coexistence:** usbipd-win (Microsoft's WSL USB tool, a USB/IP *server*) often
owns TCP 3240 on dev machines. Our server binds an ephemeral loopback port and passes it with
`--tcp-port` (pulsar-core `attach.rs`), so the two do not conflict; both can stay installed.

**Altyapı/lisans:** usbip-win2 is **GPL-3.0** (same license as Pulsar), signed by its
author; redistributing its unmodified installer is compatible with GPLv3 Pulsar (the source
is the upstream GitHub release).

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

## Mouse input inside a Duo seat (investigated 2026-11-11)

Report: in The Witcher 3 (`witcher3.exe`, dx12, imports DINPUT8 + Raw Input + `SetCursorPos`)
on a host running in a Duo seat (`rdp-tcp#0`, session 2), with the host pointer lock on,
the camera spun without stopping after a tiny mouse move.

Checked against the live seat with the throwaway probes in `scripts/windows/`. Each probe
ran as a scheduled task inside the seat (`schtasks /create … /it /ru <seat user>`; the SYSTEM
case used `CreateProcessAsUser` into the seat, as `pulsar-sysagent` does):

- Relative `SendInput` (`MOUSEEVENTF_MOVE`, no ABSOLUTE flag) from the seat user and from
  SYSTEM: Raw Input sees **relative** packets (`usFlags = 0`, `lLastX/lLastY` = the
  injected delta, `hDevice = NULL`). The cursor moves, and a `SetCursorPos` recenter holds.
- DirectInput8 system mouse (background, non-exclusive, relative format):
  `DIPROP_AXISMODE = REL`, and `GetDeviceState` returns the injected deltas, then 0 when idle.
- Seat geometry is consistent: one 1920×1080 monitor at (0,0), and the clip covers the
  whole screen. So the clamp in `pointer_relative` (`SetCursorPos` back onto the streamed
  monitor) is a no-op there.
- While idle, the seat produced no mouse packets from other devices (DuoRdp, Duo's own
  Sunshine).

So the theory that "RDS turns injected relative moves into absolute packets" is **false**
for Duo 1.6 seats. The cause of the spin is still open. The next step is to capture during
a reproduction: run `rawmouse-probe.ps1 -Listen 20000` in the seat while the game spins.
It logs every packet the game can see, with its device and the cursor position.

What Duo offers for input (Duo 1.6 binaries, static analysis):

- `DuoController.dll` exports only `DuoController_{Initialize,Uninitialize,CreateController,
  SendReport,RemoveController}`, for controller types 0–3 (Xbox One, DS4, DualSense, Edge).
  It has **no mouse or keyboard device**.
- Duo's bundled `sunshine.exe` runs as SYSTEM in each seat and injects with plain
  `SendInput`. It uses synthetic pointer devices only for touch and pen. This is the same
  path as Pulsar.
- `DuoRds.dll` is a WRDS protocol provider (`GetInputHandles`, IPC `Global\Duo_RDS_*` for
  connect/config only). `DuoRdp.exe` is a headless aardwolf RDP client that keeps the seat
  connected. `DuoAcRes64.dll` (the per-process shim) hooks HID opens, registry and the
  Steam IPC, not mouse input.

No other relative mouse source exists in a seat: ViGEm has no mouse, and no virtual HID
mouse driver is installed. Duo's per-seat device mechanism covers pads only, so Pulsar
keeps `SendInput` for the mouse in Duo seats.
