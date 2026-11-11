//! Short in-stream notices raised outside the session loop (DualSense auto-degrade,
//! the one-time Wi-Fi/power hint). The play loop (`play/hold.rs`) drains them on its 2 s
//! tick and shows them like the pick-shortfall notice: a renderer toast, or a
//! `play-notice` event for the webview when there is no native renderer.

use std::collections::HashMap;
use std::sync::Mutex;

static QUEUE: Mutex<Option<HashMap<u64, Vec<String>>>> = Mutex::new(None);

/// Queue `text` for play session `play_id`.
pub(crate) fn push(play_id: u64, text: impl Into<String>) {
	let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
	q.get_or_insert_with(HashMap::new)
		.entry(play_id)
		.or_default()
		.push(text.into());
}

/// Take the session's queued notices (oldest first).
pub(crate) fn take(play_id: u64) -> Vec<String> {
	let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
	q.as_mut()
		.and_then(|m| m.remove(&play_id))
		.unwrap_or_default()
}

#[cfg(test)]
mod tests {
	#[test]
	fn queue_is_per_session() {
		super::push(7, "a");
		super::push(8, "b");
		super::push(7, "c");
		assert_eq!(super::take(7), vec!["a", "c"]);
		assert!(super::take(7).is_empty());
		assert_eq!(super::take(8), vec!["b"]);
	}
}
