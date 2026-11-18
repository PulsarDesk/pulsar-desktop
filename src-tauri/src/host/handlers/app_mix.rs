//! The session's system audio with some apps left out (Audio → App sounds, `host/app_audio.rs`).
//!
//! The process loopback leaves out ONE process tree, and the normal capture spends it on Pulsar
//! itself (so the client never hears its own voice back). With apps left out, every other app
//! playing sound is captured on its own (`Include`, its process tree) and the captures are
//! summed ([`pulsar_core::audio::Mixer`]), paced in 10 ms blocks into the same encoder input.
//!
//! - Nothing left out, or no left-out app has an audio session: the single `Exclude` capture,
//!   exactly as before (no mixer, no added delay).
//! - A controller looks every 500 ms: which mode, which apps (a new app joins within that, its
//!   first moment is lost; an app that stopped is dropped). Switching mode stops one side before
//!   starting the other: a short gap, never doubled sound.
//! - An app whose parent is also captured is skipped (the parent's tree already has it), and so
//!   is anything under Pulsar.
//! - What the sum misses: the system-sounds session (it has no process), and an app that only
//!   starts playing between two looks.

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pulsar_core::audio::{Mixer, ProcessLoopbackMode, MIX_CHANNELS};

/// How often the controller looks at the sessions and the left-out list.
const LOOK: Duration = Duration::from_millis(500);
/// The paced block written to the encoder (10 ms at 48 kHz).
const BLOCK_FRAMES: usize = 480;

/// The encoder input, shared by the single capture and the mixer's pacer (only one writes).
struct Out<W: Write> {
	inner: Mutex<W>,
	/// A write failed: the stream was retired (the session re-streamed or ended).
	dead: AtomicBool,
}

impl<W: Write> Out<W> {
	fn write(&self, buf: &[u8]) -> std::io::Result<()> {
		let r = self.inner.lock().unwrap_or_else(|e| e.into_inner()).write_all(buf);
		if r.is_err() {
			self.dead.store(true, Ordering::SeqCst);
		}
		r
	}
}

/// A capture's sink that ends its capture once `stop` is set (the next write fails).
struct Stoppable<F: FnMut(&[u8]) -> std::io::Result<()>> {
	stop: Arc<AtomicBool>,
	f: F,
}

impl<F: FnMut(&[u8]) -> std::io::Result<()>> Write for Stoppable<F> {
	fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
		if self.stop.load(Ordering::Relaxed) {
			return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "capture stopped"));
		}
		(self.f)(buf)?;
		Ok(buf.len())
	}
	fn flush(&mut self) -> std::io::Result<()> {
		Ok(())
	}
}

/// Start the session's system audio into `out` (the encoder input): the single capture of
/// everything but Pulsar (`own`) now, the per-app mix whenever apps are left out. `false` =
/// the single capture did not start (the caller falls back, as before).
pub(super) fn start<W: Write + Send + 'static>(out: W, own: u32) -> bool {
	let out = Arc::new(Out { inner: Mutex::new(out), dead: AtomicBool::new(false) });
	let Some(whole) = start_whole(&out, own) else { return false };
	let _ = std::thread::Builder::new().name("app-mix".into()).spawn(move || control(out, own, whole));
	true
}

/// The single `Exclude(own)` capture; its stop flag.
fn start_whole<W: Write + Send + 'static>(out: &Arc<Out<W>>, own: u32) -> Option<Arc<AtomicBool>> {
	let stop = Arc::new(AtomicBool::new(false));
	let o = out.clone();
	let sink = Stoppable { stop: stop.clone(), f: move |b: &[u8]| o.write(b) };
	super::start_process_capture(sink, own, ProcessLoopbackMode::Exclude).then_some(stop)
}

/// One app's `Include(pid)` capture into the mixer; its stop flag.
fn start_app(mixer: &Arc<Mutex<Mixer>>, pid: u32) -> Option<Arc<AtomicBool>> {
	let stop = Arc::new(AtomicBool::new(false));
	let m = mixer.clone();
	let mut samples: Vec<f32> = Vec::new();
	// A write may end mid-sample: the remainder waits for the next one.
	let mut rest: Vec<u8> = Vec::new();
	let sink = Stoppable {
		stop: stop.clone(),
		f: move |b: &[u8]| {
			rest.extend_from_slice(b);
			let whole = rest.len() / 4 * 4;
			samples.clear();
			samples.extend(rest[..whole].chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])));
			rest.drain(..whole);
			m.lock().unwrap_or_else(|e| e.into_inner()).push(pid as u64, &samples);
			Ok(())
		},
	};
	super::start_process_capture(sink, pid, ProcessLoopbackMode::Include).then_some(stop)
}

/// The apps to capture now (`None` = the single capture will do: nothing left out plays).
fn wanted(own: u32) -> Option<Vec<u32>> {
	let muted = crate::host::app_audio::muted();
	if muted.is_empty() {
		return None;
	}
	let sessions = pulsar_core::audio::audio_session_apps();
	if !sessions.iter().any(|s| muted.contains(&s.exe.to_lowercase())) {
		return None;
	}
	let parents = pulsar_core::audio::parent_pids();
	let mut pids: Vec<u32> = sessions
		.iter()
		.filter(|s| !muted.contains(&s.exe.to_lowercase()))
		.map(|s| s.pid)
		.filter(|&p| p != own && !pulsar_core::audio::descends_from(p, own, &parents))
		.collect();
	pids.sort_unstable();
	pids.dedup();
	// A child of another captured app is in that app's tree already.
	let all = pids.clone();
	pids.retain(|&p| !all.iter().any(|&a| a != p && pulsar_core::audio::descends_from(p, a, &parents)));
	Some(pids)
}

fn control<W: Write + Send + 'static>(out: Arc<Out<W>>, own: u32, whole: Arc<AtomicBool>) {
	let mut whole: Option<Arc<AtomicBool>> = Some(whole);
	let mixer = Arc::new(Mutex::new(Mixer::new()));
	let mut apps: HashMap<u32, Arc<AtomicBool>> = HashMap::new();
	let mut pacer: Option<Arc<AtomicBool>> = None;
	while !out.dead.load(Ordering::SeqCst) {
		match wanted(own) {
			None => {
				if pacer.is_some() || !apps.is_empty() {
					tracing::info!("audio: app sounds back to the single capture");
				}
				if let Some(p) = pacer.take() {
					p.store(true, Ordering::SeqCst);
				}
				for (pid, s) in apps.drain() {
					s.store(true, Ordering::SeqCst);
					mixer.lock().unwrap_or_else(|e| e.into_inner()).remove(pid as u64);
				}
				if whole.is_none() {
					whole = start_whole(&out, own);
				}
			}
			Some(pids) => {
				if let Some(w) = whole.take() {
					tracing::info!(apps = pids.len(), "audio: some apps left out — capturing every other app on its own");
					w.store(true, Ordering::SeqCst);
				}
				apps.retain(|pid, s| {
					let keep = pids.contains(pid);
					if !keep {
						s.store(true, Ordering::SeqCst);
						mixer.lock().unwrap_or_else(|e| e.into_inner()).remove(*pid as u64);
					}
					keep
				});
				for pid in pids {
					if !apps.contains_key(&pid) {
						match start_app(&mixer, pid) {
							Some(s) => {
								apps.insert(pid, s);
							}
							None => tracing::debug!(pid, "audio: an app's capture did not start"),
						}
					}
				}
				if pacer.is_none() {
					pacer = Some(start_pacer(out.clone(), mixer.clone()));
				}
			}
		}
		std::thread::sleep(LOOK);
	}
	// The stream was retired: end everything.
	if let Some(w) = whole {
		w.store(true, Ordering::SeqCst);
	}
	if let Some(p) = pacer {
		p.store(true, Ordering::SeqCst);
	}
	for s in apps.values() {
		s.store(true, Ordering::SeqCst);
	}
}

/// Writes the mixer's sum into `out` every 10 ms (wall clock, absolute deadlines) until stopped
/// or the stream is retired.
fn start_pacer<W: Write + Send + 'static>(out: Arc<Out<W>>, mixer: Arc<Mutex<Mixer>>) -> Arc<AtomicBool> {
	let stop = Arc::new(AtomicBool::new(false));
	let s = stop.clone();
	let _ = std::thread::Builder::new().name("app-mix-pace".into()).spawn(move || {
		let block = Duration::from_millis(10);
		let mut next = Instant::now() + block;
		let mut bytes: Vec<u8> = Vec::with_capacity(BLOCK_FRAMES * MIX_CHANNELS * 4);
		while !s.load(Ordering::SeqCst) {
			let now = Instant::now();
			if next > now {
				std::thread::sleep(next - now);
			} else if now - next > Duration::from_millis(100) {
				// Starved (a suspend, a stall): start the clock again rather than burst.
				next = now;
			}
			next += block;
			let pcm = mixer.lock().unwrap_or_else(|e| e.into_inner()).pull(BLOCK_FRAMES);
			bytes.clear();
			bytes.extend(pcm.iter().flat_map(|v| v.to_le_bytes()));
			if out.write(&bytes).is_err() {
				break;
			}
		}
	});
	stop
}
