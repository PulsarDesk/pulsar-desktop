//! Split mode "press to join" (couch co-op): while the gaming split is up, a controller that
//! is not locked to a pane drives NOTHING; pressing A / ✕ on it locks it to the first pane
//! that has no controller yet. Each pane has a player colour (P1 blue, P2 red, P3 green,
//! P4 pink) that is also written to the pad's light bar (DualSense / DS4, via SDL).
//!
//! The frontend sets the pane order with `set_split_join` (the panes' play ids; empty = off)
//! and shows the per-pane prompts / toasts; the always-on Guide watcher
//! (`commands::spawn_guide_watcher`) feeds every pad snapshot to [`tick`]. Re-plugging a pad
//! keeps its pane: locks are keyed by the pad's uuid and survive a disconnect.

use std::collections::HashMap;
use std::sync::Mutex;

use pulsar_core::input::button;
use tauri::{AppHandle, Emitter};

/// Pane play ids in pane order (-1 = an empty pane); no live pane = join mode off.
static PANES: Mutex<Vec<i64>> = Mutex::new(Vec::new());

/// Player colours, by pane index (light bar + UI).
pub const COLORS: [[u8; 3]; 4] = [[0, 90, 255], [255, 30, 40], [0, 200, 70], [255, 40, 200]];

/// Whether join mode is on (unlocked pads are held back from every session).
pub fn active() -> bool {
	PANES.lock().unwrap().iter().any(|p| *p >= 0)
}

/// Paint `uuid`'s light bar with the colour of the pane `play_id` sits in (no-op otherwise).
pub fn paint(uuid: &str, play_id: u64) {
	let idx = PANES.lock().unwrap().iter().position(|p| *p == play_id as i64);
	if let (Some(i), Some(m)) = (idx, crate::controllers::manager()) {
		m.led(uuid.to_string(), COLORS[i % COLORS.len()]);
	}
}

#[derive(Clone, serde::Serialize)]
struct Joined {
	uuid: String,
	name: String,
	play_id: u64,
	pane: usize,
}

/// One pad snapshot from the watcher: an A press on an unlocked, enabled pad joins it to the
/// first pane without a controller. `prev_a` keeps the per-pad A state between calls.
pub fn tick(app: &AppHandle, pads: &[crate::controllers::PadView], prev_a: &mut HashMap<String, bool>) {
	let panes = PANES.lock().unwrap().clone();
	for p in pads {
		let down = p.state.is_pressed(button::A);
		let was = prev_a.insert(p.uuid.clone(), down).unwrap_or(false);
		if !panes.iter().any(|p| *p >= 0) || !down || was {
			continue;
		}
		if crate::controllers::controller_lock_owner(&p.uuid).is_some()
			|| crate::controllers::is_controller_disabled(&p.uuid)
		{
			continue;
		}
		// The first pane that no CONNECTED pad is locked to.
		let taken: Vec<u64> = pads
			.iter()
			.filter_map(|q| crate::controllers::controller_lock_owner(&q.uuid))
			.collect();
		let Some((pane, play_id)) = panes
			.iter()
			.enumerate()
			.filter(|(_, id)| **id >= 0)
			.map(|(i, id)| (i, *id as u64))
			.find(|(_, id)| !taken.contains(id))
		else {
			continue;
		};
		crate::controllers::set_controller_lock(p.uuid.clone(), play_id);
		paint(&p.uuid, play_id);
		tracing::info!(uuid = %p.uuid, name = %p.name, play_id, pane, "split join: controller joined a pane");
		let _ = app.emit("pad-joined", Joined { uuid: p.uuid.clone(), name: p.name.clone(), play_id, pane });
	}
}

/// Frontend: the gaming split's panes (play ids in pane order, -1 = empty pane) — an empty
/// list turns join mode off.
/// Pads already locked to a listed pane get their light bar repainted.
#[tauri::command]
pub(crate) fn set_split_join(play_ids: Vec<i64>) {
	let on = play_ids.iter().any(|p| *p >= 0);
	*PANES.lock().unwrap() = play_ids.clone();
	if let Some(m) = crate::controllers::manager() {
		for p in m.snapshot() {
			if let Some(owner) = crate::controllers::controller_lock_owner(&p.uuid) {
				if play_ids.contains(&(owner as i64)) {
					paint(&p.uuid, owner);
				}
			}
		}
	}
	tracing::info!(on, panes = ?play_ids, "split join mode");
}

/// Frontend: the current controller → pane locks (uuid → play id), so the assignment dialog
/// shows what is set instead of starting blank.
#[tauri::command]
pub(crate) fn controller_locks() -> HashMap<String, u64> {
	crate::controllers::lock_snapshot()
}
