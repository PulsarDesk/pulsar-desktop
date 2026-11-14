//! DS4 / DualSense touchpad as a mouse on **Windows and macOS** clients.
//!
//! Linux reads the touchpad from its own evdev node (`pulsar_core::input::touchpad_linux`).
//! Windows and macOS have no such node, but the SDL3 pad subsystem (`controllers.rs`)
//! already drives these pads through its HIDAPI drivers and exposes the touchpad contacts
//! and the pad click; while the DualSense passthrough owns a pad, its raw input report
//! supplies them instead. `controllers.rs` publishes that as `PadView::touch`, and this
//! reader feeds each pad's frames into the shared gesture machine
//! (`pulsar_core::input::touchpad_mouse::TouchpadMouse`), so the gestures are the same as
//! on Linux: one finger moves the pointer, two fingers scroll, a press clicks (two-finger
//! press = right click).
//!
//! Gate: only while Pulsar is the focused app and this session's menu is closed (the
//! controller gate in `focus_gate`; "background controller input" does not extend to the
//! mouse — moving the host pointer from a pad on a background window would surprise).
//! A pad disabled in Settings, or locked to another split pane, never drives this session.
//!
//! Untested on Windows / macOS hardware.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pulsar_core::input::touchpad_mouse::TouchpadMouse;
use pulsar_core::service::InputEvent;

/// Whether the pad `uuid` may drive session `play`'s pointer: not disabled, and not locked
/// to a different session (split-screen panes each own their pads).
pub(crate) fn pad_drives_session(disabled: bool, lock_owner: Option<u64>, play: u64) -> bool {
	!disabled && lock_owner.is_none_or(|o| o == play)
}

/// Start the reader for session `play`; it runs until `running` goes false. A silent
/// no-op when the SDL pad subsystem is unavailable.
pub fn spawn(play: u64, tx: tokio::sync::mpsc::Sender<InputEvent>, running: Arc<AtomicBool>) {
	let Some(mgr) = crate::controllers::manager() else {
		return;
	};
	let res = std::thread::Builder::new().name("touchpad-mouse".into()).spawn(move || {
		let wake = mgr.subscribe();
		let mut pads: HashMap<String, TouchpadMouse> = HashMap::new();
		let send = |evs: Vec<InputEvent>| {
			for e in evs {
				let _ = tx.blocking_send(e);
			}
		};
		while running.load(Ordering::SeqCst) {
			// Event-driven: the SDL thread pings on every pad change (touch included). The
			// timeout only re-checks the gate / `running` while nothing moves.
			match wake.rx.recv_timeout(Duration::from_millis(100)) {
				Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
				Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
			}
			let gate = crate::focus_gate::app_focused() && !crate::focus_gate::overlay_open(play);
			let snap = mgr.snapshot();
			// A pad that left (or lost its touchpad) lets go of any button it held.
			pads.retain(|uuid, m| {
				let alive = snap.iter().any(|p| &p.uuid == uuid && p.touch.is_some());
				if !alive {
					send(m.release());
				}
				alive
			});
			for p in &snap {
				let Some(frame) = p.touch else { continue };
				let mine = pad_drives_session(
					crate::controllers::is_controller_disabled(&p.uuid),
					crate::controllers::controller_lock_owner(&p.uuid),
					play,
				);
				let m = pads.entry(p.uuid.clone()).or_default();
				send(m.update(&frame, gate && mine));
			}
		}
		// Session over: never leave the host holding a button.
		for m in pads.values_mut() {
			send(m.release());
		}
	});
	if let Err(e) = res {
		tracing::warn!("touchpad-mouse: reader thread failed to start: {e}");
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn pad_ownership() {
		assert!(pad_drives_session(false, None, 7));
		assert!(pad_drives_session(false, Some(7), 7));
		assert!(!pad_drives_session(false, Some(8), 7));
		assert!(!pad_drives_session(true, None, 7));
	}
}
