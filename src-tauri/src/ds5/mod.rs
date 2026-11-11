//! DualSense passthrough, desktop side: raw HID to the physical pad (client) and, later,
//! the local bridge mode and host integration. Engine logic lives in
//! `pulsar_core::input::ds5`; design in pulsar-core `docs/dualsense.md`.

pub mod audio;
pub mod bridge;
pub mod client;
pub mod client_audio;
pub mod endpoint;
pub mod headset;
pub mod hid;
pub mod policy;
pub mod host;
pub mod host_duo;
pub mod probe;
