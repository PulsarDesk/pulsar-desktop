//! macOS native host path: ScreenCaptureKit (pulsar-core `macos_capture`) → VideoToolbox
//! (`vt.rs`) → the shared RTP packetizer (`rtp.rs`), behind the same [`crate::CaptureHandle`]
//! contract as the Windows DXGI/WGC → NVENC path: live bitrate, keyframe on request, live
//! display switch, liveness + progress counters, capture stats.
//!
//! What it does not do (VideoToolbox has no API for them): reference-frame invalidation (a
//! reported loss is declined, so the client's NACK repair or keyframe request heals it) and
//! periodic intra refresh (the request falls back to keyframes).
//!
//! Untested on macOS hardware.

pub mod annexb;
pub mod policy;

#[cfg(target_os = "macos")]
mod capture;
#[cfg(target_os = "macos")]
mod vt;

#[cfg(target_os = "macos")]
pub use capture::{display_rect, list_displays, start_capture_encode};
