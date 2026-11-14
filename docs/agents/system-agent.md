# SYSTEM input agent (Windows) — UAC, lock screen, secure desktop

Part of the pulsar-desktop agent docs — index: [`CLAUDE.md`](../../CLAUDE.md).
Status (2026-11-11): **code only, never installed or run on a real machine.** Crate
`crates/pulsar-sysagent` compiles (aarch64 + x86_64 Windows, non-Windows no-op) and its unit
tests pass; the host is **not wired to it yet** (see "Integration points").

## Problem

The host injects with `SendInput` from `pulsar.exe` (High IL when `request_admin` is on).
UIPI blocks injection into higher-integrity windows, so:

- UAC consent (`consent.exe`, **System IL**) on the user desktop (PromptOnSecureDesktop=0):
  remote clicks/keys do nothing.
- Secure desktop / lock screen / Ctrl+Alt+Del screen live on `winsta0\Winlogon`: a user
  process can neither inject there nor capture it (video black/frozen).

Parsec / Sunshine fix it with a LocalSystem service that starts a helper **in the user's
session with a SYSTEM token**; the helper follows the input desktop before `SendInput`.

## Process model

```
SCM ── PulsarSysAgent (LocalSystem, session 0)        pulsar-sysagent.exe service
          │  one helper per logged-on session; kill-on-close job object
          ├── helper (SYSTEM token, session 1, winsta0\default)   pulsar-sysagent.exe helper
          │      └── \\.\pipe\pulsar-sysagent-1  ◄── pulsar.exe host (user A, High IL)
          └── helper (SYSTEM token, session 3)                    (Duo seat / RDP session)
                 └── \\.\pipe\pulsar-sysagent-3  ◄── pulsar.exe host (user B)
```

- **Service** (`src/service.rs`): `StartServiceCtrlDispatcherW`, accepts STOP / SHUTDOWN /
  SESSIONCHANGE. A supervisor reconciles every 2 s and on every session-change event.
- **Helper spawn**: `OpenProcessToken(self)` → `DuplicateTokenEx(TokenPrimary)` →
  `SetTokenInformation(TokenSessionId, s)` → `CreateEnvironmentBlock` →
  `CreateProcessAsUserW(lpDesktop = "winsta0\default", CREATE_SUSPENDED|CREATE_NO_WINDOW)` →
  `AssignProcessToJobObject(kill-on-close job)` → `ResumeThread`. Service death kills
  every helper, so no orphaned SYSTEM processes.
- **Helper** (`src/helper.rs`): checks its own token is LocalSystem, derives the session
  user itself (`WTSQueryUserToken` → TokenUser; it never trusts arguments), creates the pipe,
  one thread per connection.

### Per-session lifecycle

| Event | Action |
| --- | --- |
| Service start | enumerate sessions (`WTSEnumerateSessionsW`); spawn for each session ≠ 0 in state Active/Connected/Disconnected **where `WTSQueryUserToken` succeeds** (a user is logged on) |
| Logon / RDP or Duo seat connect | SESSIONCHANGE → reconcile → spawn (token can lag the event; the 2 s tick retries) |
| Lock (Win+L), UAC | nothing — the helper follows the desktop switch itself |
| Logoff / session gone | `TerminateProcess` the helper |
| Helper exits | respawn with backoff 1, 2, 4 … 60 s; counter resets after a 30 s healthy run (`policy::backoff`) |
| Service stop | terminate all helpers (they release held keys only on a graceful pipe close; see risks) |

## IPC protocol (`src/proto.rs`)

Byte-mode pipe `\\.\pipe\pulsar-sysagent-<session>`. Frame = `u16 LE length` + body
(`u8 tag` + payload, LE). Max body 64 bytes; anything malformed drops the connection.

| Dir | Tag | Message | Payload |
| --- | --- | --- | --- |
| C→H | 0x01 | `Hello` | `"PSAG"` + `u16 version` (=1) |
| C→H | 0x02 | `Mouse` | `i32 dx, i32 dy, i32 data, u32 flags` (= one `MOUSEINPUT`) |
| C→H | 0x03 | `Key` | `u16 vk, u16 scan, u32 flags` (= one `KEYBDINPUT`) |
| C→H | 0x04 | `ClampCursor` | `i32 left, top, width, height` |
| C→H | 0x05 | `Pinch` | `u8 phase, f64 scale` |
| C→H | 0x06 | `ReleaseAll` | — |
| C→H | 0x07 | `Ping` | `u32 seq` |
| H→C | 0x81 | `HelloAck` | `u16 version, u32 session` |
| H→C | 0x82 | `Reject` | `u8 code` (1 = version, 2 = bad hello) |
| H→C | 0x83 | `Pong` | `u32 seq, u8 desktop` (0 Default, 1 Winlogon, 2 other, 3 unknown) |

The helper is a **dumb injector**: the client does keymap (evdev → VK/scan/extended via
`pulsar_core::input::evdev_to_raw_key`), monitor mapping and scroll accumulation
(`src/geom.rs`, same formulas as `DesktopInput`). The helper only whitelists flags
(mouse: move/abs/virtualdesk/L/R/M buttons/wheel/hwheel; key: keyup/extended/unicode, with
`vk == 0` iff unicode) and injects. Version bumps are breaking: host and helper ship in one
installer, so they always match.

Client (`SysInput`, `src/client.rs`): methods enqueue into a bounded queue (1024) drained by
a writer thread, so the host's input closure never blocks on the helper. The writer pings
every 0.5 s and reads the `Pong` on the same thread (a synchronous pipe handle serializes
reads and writes, so a parallel reader would stall the writer). A write error, a bad pong
or a full queue marks it dead → next call returns `Err(Disconnected)`.

## Authentication — who may open a session's pipe

Pipe DACL/SACL (`src/sddl.rs`, tested):

```
O:SYG:SYD:P(D;;GA;;;NU)(A;;GA;;;SY)(A;;0x12019b;;;<session user SID>)S:(ML;;NWNR;;;HI)
```

1. **Other local users** (other Duo seats, RDP users): not in the DACL → ACCESS_DENIED.
   Plus `PIPE_REJECT_REMOTE_CLIENTS` and a NETWORK deny ACE: no SMB access.
2. **Same user, non-elevated process** (Medium-IL malware): blocked by the High mandatory
   label. Without this, any user process could click "Yes" on a consent prompt, which is a
   UAC bypass. `--allow-medium` (service/install flag) relaxes it to ME for standard-user
   machines; read the risk section before using it.
3. **Same user, other session**: `GetNamedPipeClientSessionId` must equal the helper's.
4. Helper re-checks the client token (user SID, integrity RID) via
   `GetNamedPipeClientProcessId` and pins the **image directory**: the client exe must sit
   directly in the helper's dir or its parent (`$INSTDIR\resources` → `$INSTDIR`), or in an
   admin-configured `--client-dir`.
5. User ACE is `0x12019b`, not `GW`: no `FILE_APPEND_DATA` (= FILE_CREATE_PIPE_INSTANCE), so
   the user cannot add a rogue server instance. The client opens with exactly
   `FILE_GENERIC_READ | FILE_WRITE_DATA`.

**Server authentication (anti-squatting).** Any user can create a pipe name first. The helper
uses `FILE_FLAG_FIRST_PIPE_INSTANCE` (exits if the name is taken; the service retries). The
client opens with `SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION` (a fake server cannot
impersonate the elevated host token). Before sending anything, the client checks
`GetNamedPipeServerSessionId` / server PID session == ours **and** pipe owner ==
LocalSystem (`GetSecurityInfo(OWNER)`). A non-admin cannot create an object owned by SYSTEM,
so a squatter gets nothing (worst case: DoS, the host falls back to SendInput).

## Desktop switching (`src/inject.rs`)

Each injecting thread owns a `DesktopFollower`. Every 200 ms, and right after any
`SendInput` that returns 0: `OpenInputDesktop(0, FALSE, GENERIC_ALL)` → name via
`GetUserObjectInformationW(UOI_NAME)` → if it changed, `SetThreadDesktop` (the threads
never create windows/hooks, so this is allowed) and retry the event once. The pinch (touch
injection) worker has its own follower. `Pong.desktop` reports Default/Winlogon to the host.

Held state: the helper tracks each connection's down keys/buttons and releases them when the
pipe closes or errors, so a crashed host cannot leave Shift stuck on the lock screen.

## Failure / fallback

`SysInput::connect(keymap)` is fast and never retries for long:

| Result | Host action |
| --- | --- |
| `Ok` | inject through the helper (all desktops) |
| `Err(NotRunning)` | service absent, not started, or helper still spawning → use `DesktopInput`; retry connect every ~5 s |
| `Err(Untrusted(_))` | **never** send to that pipe; use `DesktopInput`; log a warning (possible squatting) |
| `Err(Handshake(_))` | version mismatch (half-updated install) → `DesktopInput` |
| any method `Err(Disconnected)` | drop the `SysInput`, re-send **this** event via `DesktopInput`, retry later |

Behaviour without the service is exactly today's.

## Integration points (NOT done — host files have parallel WIP; line numbers as of 2026-11-11)

1. `src-tauri/Cargo.toml:81` `[target.'cfg(windows)'.dependencies]` → add
   `pulsar-sysagent = { path = "../crates/pulsar-sysagent" }`.
2. `src-tauri/src/host.rs:1369` (next to `let mut desktop: Option<DesktopInput>`) → add
   `#[cfg(windows)] let mut sys: Option<pulsar_sysagent::SysInput> = None;` plus a
   `sys_retry: Instant`.
3. `host.rs:1706-1711` (lazy `DesktopInput::new()`) → keep it (fallback) and, on Windows, when
   `sys.is_none() && now >= sys_retry`: `pulsar_sysagent::SysInput::connect(pulsar_core::input::evdev_to_raw_key)`;
   on `Err` set `sys_retry = now + 5 s` (warn once for `Untrusted`, debug otherwise).
4. `host.rs:1759` and `host.rs:1813` (`d.set_monitor(Some(MonitorRect {…}))`) → also
   `s.set_monitor(Some(pulsar_sysagent::MonitorRect { /* same 8 fields */ }))`.
5. `host.rs:1714-1845` (dispatch `match other`) → if `sys` is `Some`, dispatch the same
   variants to it (`pointer`, `pointer_relative`, `button`, `scroll`, `key`, `type_char`,
   `pinch`); on `Err` → `sys = None` and fall through to the existing `DesktopInput` arm for
   this event. Easiest: a small `enum HostInjector { Sys(SysInput), Plain(DesktopInput) }` in
   a new `src-tauri/src/host/injector.rs`.
6. `host.rs:1497` (view-only edge) and `host.rs:1636` (window-route switch) → also
   `let _ = s.flush_held();`. `host.rs:2393` teardown: `SysInput`'s Drop flushes; nothing to add.
7. `routed_to_window` (`host.rs:1703`, PostMessage co-op path) stays as is: helper is only for
   the global desktop path.
8. Optional UI: poll `s.desktop()`; on `Winlogon` show the client a banner, e.g.
   "Güvenli masaüstü açık — görüntü aktarılamıyor, klavye/fare çalışıyor" (until phase 2).
9. `host/takeover.rs` `win::run` (the hook `proc`) ignores `LLMHF_INJECTED` — SYSTEM-injected events carry it too, so
   no change. (A `dwExtraInfo` Pulsar marker from windows-input-plan.md would need adding in
   both injectors.)

## Install / uninstall

What exists: the Tauri **NSIS** bundle (`tauri.windows.conf.json`, `installMode: perMachine`
→ Program Files, admin) with `src-tauri/installer-hooks.nsh` (POSTINSTALL only today), and
`crates/pulsar-setup` (per-user `%LOCALAPPDATA%\Programs\Pulsar`, no UAC). No MSI.

- **NSIS (supported path).** Bundle `resources/pulsar-sysagent.exe`: add it to
  `tauri.windows.conf.json` `bundle.resources`, and build it in `release.yml` like
  pulsar-render (`cargo build --release -p pulsar-sysagent [--target …]`, copy to
  `src-tauri/resources/`, per arch). Hooks:
  - `NSIS_HOOK_PREINSTALL`: `IfFileExists "$INSTDIR\resources\pulsar-sysagent.exe"` →
    `ExecWait '"…\pulsar-sysagent.exe" stop'` (unlocks the exe for an update; the job kills
    the helpers).
  - `NSIS_HOOK_POSTINSTALL`: `ExecWait '"$INSTDIR\resources\pulsar-sysagent.exe" install'`
    (creates or reconfigures `PulsarSysAgent`: auto-start, LocalSystem, restart-on-failure
    ×3, Turkish display name/description, then starts it).
  - `NSIS_HOOK_PREUNINSTALL`: `ExecWait '"…\pulsar-sysagent.exe" uninstall'`.
- **pulsar-setup (per-user): must NOT register the service.** A LocalSystem service whose exe
  sits in a user-writable dir is a direct privilege escalation. `install` refuses any path
  outside `%ProgramFiles%` unless `--allow-any-path` is passed. If the bootstrapper wants
  it later: an elevated sub-step that copies the exe to `%ProgramFiles%\Pulsar\SysAgent\` and
  runs `install --client-dir "%LOCALAPPDATA%\Programs\Pulsar"`. That client dir is
  user-writable, so the image pin is then weaker (only the IL check remains).
- Logs: `<exe dir>\logs\sysagent.log` (admin-only dir; deliberately not `%ProgramData%`,
  where a user could pre-plant a link for SYSTEM to write through).
- Dev test (maintainer, elevated, on a test PC): copy the release exe under
  `C:\Program Files\Pulsar\resources\`, run `install --client-dir <repo>\target\debug`,
  run the host, trigger a UAC prompt, check `logs\sysagent.log`; `uninstall` afterwards.

## Security considerations

- The helper is SYSTEM code reachable from a user process: keep the parser tiny (fixed-size
  messages, flag whitelist, 64-byte cap) and never add file/process/registry operations to
  the protocol.
- Everything that can open the pipe can drive UAC prompts = elevation. That is why it is
  High-IL only. An elevated admin process can already elevate; `--allow-medium` for a
  standard user means any process of that user can type into a credential prompt (it still
  needs the admin password), so leave it off by default.
- Pipe-name squatting → DoS only (owner check). Client SQOS = identification only.
- Remote input on the lock screen = the remote user can type the Windows password. That is
  the feature, but it must stay behind the existing session approval / unattended policy.
  Pulsar never sees the password beyond the keystroke stream.
- Ctrl+Alt+Del cannot be injected with `SendInput`. It needs `SendSAS(FALSE)` from the
  **service** (plus `SoftwareSASGeneration` policy). Phase 2: helper → service request,
  rate-limited.
- Service death kills the helpers (job). An abrupt kill skips the release of held keys.
  Low risk, and the next injection's key-up fixes it.

## Phase 2 — capturing the secure desktop

The user-level host cannot see `winsta0\Winlogon`: DXGI Desktop Duplication returns
`DXGI_ERROR_ACCESS_LOST` / `E_ACCESSDENIED` on the switch, and a user process cannot attach
to that desktop. Plan:

1. Helper gets a capture mode: a thread does `SetThreadDesktop(Winlogon)`, creates its own
   D3D11 device, and runs `IDXGIOutput1::DuplicateOutput` on the streamed output. It
   recreates the duplication on every `ACCESS_LOST` and on every desktop switch (same
   follower logic).
2. Frames go to the host as a **shared keyed-mutex texture**: `CreateTexture2D(MISC_SHARED_NTHANDLE | KEYEDMUTEX)`,
   `IDXGIResource1::CreateSharedHandle`, `DuplicateHandle` into the host PID. Add new
   protocol messages (`CaptureStart{output}`, `FrameReady{idx, pts}`). The host's
   `pulsar-capture` DXGI→NVENC path consumes the texture in place of its own duplication
   while `Pong.desktop == Winlogon`. Encoding stays in the host, so NVENC/RFI/RTP stay
   unchanged.
3. Alternative (Sunshine model): the whole host runs as SYSTEM. Rejected for now: it would
   move the network-facing stack into SYSTEM.
4. Same mechanism later enables pre-logon / unattended access with no user session (a host
   in session 0 or at the logon screen). Out of scope for this crate's phase 1.

## Open risks / unverified

- Never run: session-change timing, `CreateProcessAsUserW` from the service, Winlogon
  `SetThreadDesktop`, touch injection on the secure desktop, and Duo seat sessions are all
  unverified on real hardware.
- Whether Duo sessions report `WTSActive` and give a `WTSQueryUserToken` token is unknown.
- `GetSecurityInfo(OWNER)` on a pipe handle opened with `0x12008b` access is believed to work
  (READ_CONTROL is included) but is untested.
- A hung helper leaves the client's writer thread blocked in `ReadFile` (leaked thread; the
  host falls back). Fix if seen: overlapped I/O + `CancelIoEx`.
- Cargo.lock gains the `pulsar-sysagent` entry; the local `.cargo/config.toml` path patch
  rule still applies (never commit Cargo.lock with it on).
