//! Pulsar SYSTEM input agent — library side.
//!
//! * [`proto`], [`sddl`], [`geom`], [`policy`]: pure, cross-platform, unit-tested.
//! * [`SysInput`] (Windows): the client the user-level host uses to inject through its
//!   session's SYSTEM helper, so input reaches UAC consent prompts, the lock screen and the
//!   secure desktop. On any error the host falls back to `pulsar_core::input::DesktopInput`.
//!
//! The `pulsar-sysagent` binary (service + helper) lives in `main.rs`.
//! Design + integration: `docs/agents/system-agent.md`.

pub mod geom;
pub mod policy;
pub mod proto;
pub mod sddl;

#[cfg(windows)]
mod client;
#[cfg(windows)]
#[doc(hidden)]
pub mod win;

pub use geom::MonitorRect;
pub use proto::DesktopKind;

#[cfg(windows)]
pub use client::{KeyMap, SysInput, SysInputError, SysPointer};

/// SCM service name (`sc query PulsarSysAgent`).
pub const SERVICE_NAME: &str = "PulsarSysAgent";
