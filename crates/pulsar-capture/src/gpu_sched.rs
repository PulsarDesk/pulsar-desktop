//! Process-wide GPU scheduling priority class (`D3DKMTSetProcessSchedulingPriorityClass`), the
//! Sunshine technique on top of the per-device `SetGPUThreadPriority(7)`.
//!
//! On a GPU the game keeps at 100 % (measured 2026-11-11: Witcher 3 with frame generation on a
//! hybrid RTX 3080 Laptop host), our capture copy and encode submit wait behind the game's
//! queue — tick p50 20-50 ms, 18-43 fresh frames/s while the game showed 60. A REALTIME class
//! lets the scheduler run our small capture work first. It needs SeIncreaseBasePriorityPrivilege
//! (an elevated / SYSTEM process); without it the call fails and we keep the normal class.
//!
//! `PULSAR_GPU_SCHED=realtime` (default) | `high` | `off`.

use std::ffi::c_void;
use std::sync::Once;

#[link(name = "gdi32")]
extern "system" {
	fn D3DKMTSetProcessSchedulingPriorityClass(process: *mut c_void, class: i32) -> i32;
}
#[link(name = "kernel32")]
extern "system" {
	fn GetCurrentProcess() -> *mut c_void;
}

/// D3DKMT_SCHEDULINGPRIORITYCLASS values.
const CLASS_HIGH: i32 = 4;
const CLASS_REALTIME: i32 = 5;

/// The class to request for a `PULSAR_GPU_SCHED` value (`None` = leave it alone).
fn class_for(setting: Option<&str>) -> Option<i32> {
	match setting.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
		Some("off") | Some("0") | Some("normal") => None,
		Some("high") => Some(CLASS_HIGH),
		_ => Some(CLASS_REALTIME),
	}
}

/// Raise this process's GPU scheduling class once (first capture device created).
pub(crate) fn raise_once() {
	static ONCE: Once = Once::new();
	ONCE.call_once(|| {
		let setting = std::env::var("PULSAR_GPU_SCHED").ok();
		let Some(class) = class_for(setting.as_deref()) else {
			crate::encode_log("gpu sched: left at normal (PULSAR_GPU_SCHED)");
			return;
		};
		// SAFETY: plain Win32 calls; GetCurrentProcess returns a pseudo-handle.
		let mut status = unsafe { D3DKMTSetProcessSchedulingPriorityClass(GetCurrentProcess(), class) };
		let mut applied = class;
		if status != 0 && class == CLASS_REALTIME {
			// Not privileged enough for REALTIME (unelevated): HIGH needs no privilege.
			status = unsafe { D3DKMTSetProcessSchedulingPriorityClass(GetCurrentProcess(), CLASS_HIGH) };
			applied = CLASS_HIGH;
		}
		crate::encode_log(&format!(
			"gpu sched: process class {} (status 0x{:08x})",
			if status == 0 {
				if applied == CLASS_REALTIME { "REALTIME" } else { "HIGH" }
			} else {
				"unchanged"
			},
			status as u32
		));
	});
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn setting_maps_to_a_class() {
		assert_eq!(class_for(None), Some(CLASS_REALTIME));
		assert_eq!(class_for(Some("high")), Some(CLASS_HIGH));
		assert_eq!(class_for(Some(" OFF ")), None);
		assert_eq!(class_for(Some("realtime")), Some(CLASS_REALTIME));
	}
}
