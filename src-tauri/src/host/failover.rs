//! Windows: restarting a session's stream from the host side when the NVIDIA driver stops
//! answering, and again when it is back (`process::nv` keeps the driver's state).
//!
//! The native path (DXGI / WGC capture + the NVENC SDK) needs a D3D device on the NVIDIA GPU.
//! While that driver is wedged — measured for 75 min and for 5 h 50 min on a host where
//! another program's process hung in it (2026-11-13) — a fresh native capture builds and then
//! never delivers a frame, or never finishes building; the client sat on "no picture" until
//! the host app was restarted, and stayed on the fallback encoder after the driver recovered.
//!
//! * [`restart`]: a session's native capture stalled → its stream starts again at once; the
//!   stream handler then finds NVENC unavailable and takes the next encoder (AMF, Media
//!   Foundation, software).
//! * [`restart_fallbacks`]: NVENC answered again → every session that streams on a fallback
//!   encoder starts again and returns to the native path — unless its client was never told
//!   about NVENC (the caps it asked for did not list it): a client drops the codecs its
//!   decoder cannot take from the encoder families it was told, so its stream must stay on
//!   one of those.
//!
//! Both go through the session's host-initiated restream channel (the one the display-mode
//! watcher uses) with the session's `force` flag set, so the handler honors the request in
//! full instead of absorbing it as an unchanged bitrate step.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use pulsar_core::service::StreamReq;

struct Entry<R> {
	/// The session's host-initiated restream channel (`serve_with`'s restream branch).
	tx: tokio::sync::mpsc::Sender<R>,
	/// The request the session's FALLBACK stream was started with; `None` while the session
	/// is on the native path (`make_on_stream` keeps it that way).
	last_req: Arc<Mutex<Option<R>>>,
	/// Read and cleared by the session's stream handler: the next request is a restart to
	/// carry out in full.
	force: Arc<AtomicBool>,
	/// Whether the caps this session's client was sent listed NVENC (`true` until it asks).
	nvenc_told: Arc<AtomicBool>,
}

/// The live sessions by id. Generic over the request so the rules are tested without one.
struct Sessions<R>(HashMap<u64, Entry<R>>);

impl<R: Clone> Sessions<R> {
	/// Sessions whose restream receiver is gone have ended.
	fn prune(&mut self) {
		self.0.retain(|_, e| !e.tx.is_closed());
	}

	fn register(
		&mut self,
		sid: u64,
		tx: tokio::sync::mpsc::Sender<R>,
		last_req: Arc<Mutex<Option<R>>>,
		nvenc_told: Arc<AtomicBool>,
	) -> Arc<AtomicBool> {
		self.prune();
		let force = Arc::new(AtomicBool::new(false));
		self.0.insert(sid, Entry { tx, last_req, force: force.clone(), nvenc_told });
		force
	}

	fn restart(&self, sid: u64, req: R) -> bool {
		let Some(e) = self.0.get(&sid) else { return false };
		e.force.store(true, Ordering::SeqCst);
		// Full = a restart is queued already (it becomes the forced one); Closed = session over.
		e.tx.try_send(req).is_ok()
	}

	/// `(sid, queued)` of every session that was on a fallback encoder.
	fn restart_fallbacks(&mut self) -> Vec<(u64, bool)> {
		self.prune();
		let mut out = Vec::new();
		for (sid, e) in self.0.iter() {
			if !e.nvenc_told.load(Ordering::SeqCst) {
				continue;
			}
			let Some(req) = e.last_req.lock().unwrap().clone() else { continue };
			e.force.store(true, Ordering::SeqCst);
			out.push((*sid, e.tx.try_send(req).is_ok()));
		}
		out
	}
}

fn sessions() -> &'static Mutex<Sessions<StreamReq>> {
	static S: OnceLock<Mutex<Sessions<StreamReq>>> = OnceLock::new();
	S.get_or_init(|| Mutex::new(Sessions(HashMap::new())))
}

/// Register session `sid`; returns its `force` flag for the stream handler.
pub(crate) fn register(
	sid: u64,
	tx: tokio::sync::mpsc::Sender<StreamReq>,
	last_req: Arc<Mutex<Option<StreamReq>>>,
	nvenc_told: Arc<AtomicBool>,
) -> Arc<AtomicBool> {
	sessions().lock().unwrap().register(sid, tx, last_req, nvenc_told)
}

/// Session `sid`'s native capture stalled: start its stream again with `req` (the request the
/// stalled capture was started with). Returns whether the restart was queued.
pub(crate) fn restart(sid: u64, req: StreamReq) -> bool {
	sessions().lock().unwrap().restart(sid, req)
}

/// NVENC is available again: restart every session that streams on a fallback encoder.
pub(crate) fn restart_fallbacks() {
	for (sid, queued) in sessions().lock().unwrap().restart_fallbacks() {
		tracing::info!(sid, queued, "NVENC is back → restarting the stream on the native path");
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn table() -> Sessions<u32> {
		Sessions(HashMap::new())
	}

	fn told(v: bool) -> Arc<AtomicBool> {
		Arc::new(AtomicBool::new(v))
	}

	#[test]
	fn a_stalled_session_gets_a_forced_restart() {
		let mut t = table();
		let (tx, mut rx) = tokio::sync::mpsc::channel(4);
		let force = t.register(1, tx, Arc::new(Mutex::new(None)), told(true));
		assert!(!force.load(Ordering::SeqCst));
		assert!(t.restart(1, 5000));
		assert!(force.load(Ordering::SeqCst));
		assert_eq!(rx.try_recv().unwrap(), 5000);
		assert!(!t.restart(2, 1), "unknown session");
	}

	#[test]
	fn a_full_queue_still_forces_the_restart_already_in_it() {
		let mut t = table();
		let (tx, _rx) = tokio::sync::mpsc::channel(1);
		let force = t.register(1, tx, Arc::new(Mutex::new(None)), told(true));
		assert!(t.restart(1, 1));
		force.store(false, Ordering::SeqCst);
		assert!(!t.restart(1, 2), "queue full");
		assert!(force.load(Ordering::SeqCst));
	}

	#[test]
	fn only_sessions_on_a_fallback_encoder_return() {
		let mut t = table();
		let (tx_n, mut rx_n) = tokio::sync::mpsc::channel(4);
		let (tx_f, mut rx_f) = tokio::sync::mpsc::channel(4);
		let native = t.register(1, tx_n, Arc::new(Mutex::new(None)), told(true));
		let fallback = t.register(2, tx_f, Arc::new(Mutex::new(Some(7000))), told(true));
		assert_eq!(t.restart_fallbacks(), vec![(2, true)]);
		assert!(!native.load(Ordering::SeqCst));
		assert!(rx_n.try_recv().is_err());
		assert!(fallback.load(Ordering::SeqCst));
		assert_eq!(rx_f.try_recv().unwrap(), 7000);
	}

	#[test]
	fn a_client_that_was_not_told_about_nvenc_stays_on_its_encoder() {
		let mut t = table();
		let (tx, mut rx) = tokio::sync::mpsc::channel(4);
		let nvenc_told = told(false);
		let force = t.register(1, tx, Arc::new(Mutex::new(Some(7000))), nvenc_told.clone());
		assert!(t.restart_fallbacks().is_empty());
		assert!(!force.load(Ordering::SeqCst));
		assert!(rx.try_recv().is_err());
		// It asked for the caps again and NVENC was listed this time.
		nvenc_told.store(true, Ordering::SeqCst);
		assert_eq!(t.restart_fallbacks(), vec![(1, true)]);
	}

	#[test]
	fn an_ended_session_is_dropped() {
		let mut t = table();
		let (tx, rx) = tokio::sync::mpsc::channel(4);
		t.register(1, tx, Arc::new(Mutex::new(Some(1))), told(true));
		drop(rx);
		assert!(t.restart_fallbacks().is_empty());
		assert!(!t.0.contains_key(&1));
		assert!(!t.restart(1, 1));
	}
}
