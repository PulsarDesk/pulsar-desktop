//! Client microphone → host (the session's mic side channel): raw s16le 48 kHz mono PCM in
//! `DataMsg::Audio` chunks of 20 ms, the format the host's player takes (`audio_io`).
//!
//! Captured in-process with SDL3 on every desktop OS, on the device picked in the session
//! menu (`audio_devices`, default = the system default microphone). The capture thread never
//! gives up while the mic is on: the picked device unplugged → the system default; nothing
//! opens → silence, retried every second; the pick back → moved back to it; a stream that
//! delivers nothing for [`audio_devices::STALL`] (driver reset) → reopened.
//!
//! Only when SDL's audio subsystem cannot start at all does it fall back to the older
//! recorder process (`audio_io::spawn_mic_recorder`: ffmpeg / parecord / pw-record /
//! arecord, default device only).

use std::io::Read as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pulsar_core::service::DataMsg;

use crate::audio_devices::{self as devs, Kind, Opened, Retry};

/// 20 ms of 48 kHz mono s16.
const CHUNK: usize = 1920;
/// Bytes per ms of 48 kHz mono s16.
const BYTES_PER_MS: usize = 96;

/// Echo cancellation of the mic against the call's far end as played here
/// (`pcm_out::far_end`): the peer must not hear their own voice back from our speakers.
/// Passes the mic through untouched while no far end plays.
struct EchoGuard {
	aec: pulsar_core::audio::aec::AecStream,
}

impl EchoGuard {
	fn new() -> EchoGuard {
		EchoGuard { aec: pulsar_core::audio::aec::AecStream::new() }
	}

	/// `chunk` (s16le mono) whose last sample was captured at `end`.
	fn clean(&mut self, chunk: Vec<u8>, end: Instant) -> Vec<u8> {
		let n = chunk.len() / 2;
		let Some(far) = crate::pcm_out::far_end::window(end, n) else { return chunk };
		let near: Vec<f32> =
			chunk.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32_768.0).collect();
		self.aec
			.process(&far, &near)
			.iter()
			.flat_map(|v| ((v.clamp(-1.0, 1.0) * 32_767.0) as i16).to_le_bytes())
			.collect()
	}
}

/// A running microphone capture. Dropping it stops the capture (and its recorder process,
/// on the fallback path) and waits for the thread.
pub struct Mic {
	stop: Arc<AtomicBool>,
	child: Arc<Mutex<Option<std::process::Child>>>,
	thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Mic {
	fn drop(&mut self) {
		self.stop.store(true, Ordering::SeqCst);
		if let Some(mut c) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
			let _ = c.kill();
			let _ = c.wait(); // reap: kill alone leaves a unix zombie
		}
		if let Some(t) = self.thread.take() {
			let _ = t.join();
		}
	}
}

/// Start capturing into `tx` (the session's side channel). `ffmpeg` = the bundled ffmpeg, for
/// the fallback recorder only.
pub fn start(tx: tokio::sync::mpsc::Sender<DataMsg>, ffmpeg: String) -> std::io::Result<Mic> {
	let stop = Arc::new(AtomicBool::new(false));
	let child = Arc::new(Mutex::new(None));
	let (s, c) = (stop.clone(), child.clone());
	let thread = std::thread::Builder::new().name("mic-capture".into()).spawn(move || {
		if devs::ensure_sdl() {
			run_sdl(&tx, &s);
		} else {
			run_recorder(&tx, &s, &c, &ffmpeg);
		}
		tracing::info!("mic: capture stopped");
	})?;
	Ok(Mic { stop, child, thread: Some(thread) })
}

/// An open SDL recording stream.
struct Rec {
	stream: *mut sdl3_sys::audio::SDL_AudioStream,
	on: Opened,
	/// When it last delivered data (or opened).
	last_data: Instant,
}

impl Rec {
	fn open(on: Opened) -> Option<Rec> {
		use sdl3_sys::audio::*;
		let dev = match on {
			Opened::Default => SDL_AUDIO_DEVICE_DEFAULT_RECORDING,
			Opened::Device(id) => SDL_AudioDeviceID(id),
		};
		let spec = SDL_AudioSpec { format: SDL_AUDIO_S16LE, channels: 1, freq: 48_000 };
		// SAFETY: plain SDL3 calls; AUDIO is initialized (ensure_sdl). The stream opens paused.
		unsafe {
			let stream = SDL_OpenAudioDeviceStream(dev, &spec, None, std::ptr::null_mut());
			if stream.is_null() {
				return None;
			}
			if !SDL_ResumeAudioStreamDevice(stream) {
				SDL_DestroyAudioStream(stream);
				return None;
			}
			Some(Rec { stream, on, last_data: Instant::now() })
		}
	}

	/// Move whatever SDL has captured into `buf`; returns the bytes added.
	fn read(&mut self, buf: &mut Vec<u8>) -> usize {
		use sdl3_sys::audio::{SDL_GetAudioStreamAvailable, SDL_GetAudioStreamData};
		let mut total = 0;
		let mut tmp = [0u8; 4 * CHUNK];
		loop {
			// SAFETY: `stream` is live and owned by self; SDL writes at most `tmp.len()` bytes.
			let got = unsafe {
				if SDL_GetAudioStreamAvailable(self.stream) <= 0 {
					break;
				}
				SDL_GetAudioStreamData(self.stream, tmp.as_mut_ptr().cast(), tmp.len() as i32)
			};
			if got <= 0 {
				break;
			}
			buf.extend_from_slice(&tmp[..got as usize]);
			total += got as usize;
		}
		if total > 0 {
			self.last_data = Instant::now();
		}
		total
	}
}

impl Drop for Rec {
	fn drop(&mut self) {
		// SAFETY: destroys the stream `open` created (closes its logical device).
		unsafe { sdl3_sys::audio::SDL_DestroyAudioStream(self.stream) };
	}
}

/// Where the mic should be now (the pick when present, else the default).
fn want() -> (Option<u32>, Option<Vec<devs::Dev>>) {
	let list = devs::devices(Kind::Mic);
	let w = list.as_deref().and_then(|l| devs::resolve(&devs::pref(Kind::Mic), l));
	(w, list)
}

fn run_sdl(tx: &tokio::sync::mpsc::Sender<DataMsg>, stop: &AtomicBool) {
	let mut rec: Option<Rec> = None;
	let mut retry = Retry::default();
	let mut next_check = Instant::now();
	let mut buf: Vec<u8> = Vec::with_capacity(8 * CHUNK);
	let mut echo = EchoGuard::new();
	// Reopens for a stream that delivered nothing, in a row (logged on the first only).
	let mut stalls: u32 = 0;
	tracing::info!(pick = %devs::pref(Kind::Mic), "mic: capture up (SDL3)");
	while !stop.load(Ordering::SeqCst) {
		let now = Instant::now();
		if rec.is_none() && retry.due(now) {
			let (w, _) = want();
			let first = w.map(Opened::Device).unwrap_or(Opened::Default);
			rec = Rec::open(first).or_else(|| (first != Opened::Default).then(|| Rec::open(Opened::Default)).flatten());
			match &rec {
				Some(r) => {
					let failed = retry.ok();
					if stalls <= 1 {
						tracing::info!(on = ?r.on, after_failures = failed, "mic: recording device open");
					}
				}
				None => {
					if retry.failed(now) {
						tracing::warn!(err = %devs::sdl_error(), "mic: no recording device opens — silent, retrying every second");
					}
				}
			}
			next_check = now + devs::CHECK_EVERY;
		}
		if let Some(r) = rec.as_mut() {
			if r.read(&mut buf) > 0 {
				stalls = 0;
			}
			while buf.len() >= CHUNK {
				let chunk: Vec<u8> = buf.drain(..CHUNK).collect();
				// What is still in `buf` was recorded after this chunk.
				let end = now.checked_sub(Duration::from_millis((buf.len() / BYTES_PER_MS) as u64)).unwrap_or(now);
				let chunk = echo.clean(chunk, end);
				if tx.blocking_send(DataMsg::Audio(chunk)).is_err() {
					return; // the session is gone
				}
			}
			if now >= next_check {
				next_check = now + devs::CHECK_EVERY;
				let (w, list) = want();
				let moved = list.as_deref().is_some_and(|l| devs::needs_reopen(r.on, w, l));
				let stalled = now.duration_since(r.last_data) > devs::STALL;
				if moved || stalled {
					if moved || stalls == 0 {
						tracing::info!(on = ?r.on, to = ?w, moved, stalled, "mic: reopening the recording device");
					}
					if stalled && !moved {
						stalls += 1;
					}
					rec = None;
					buf.clear();
				}
			}
		}
		std::thread::sleep(Duration::from_millis(10));
	}
}

/// The fallback: an external recorder process (default device), restarted when it exits.
fn run_recorder(
	tx: &tokio::sync::mpsc::Sender<DataMsg>,
	stop: &AtomicBool,
	slot: &Mutex<Option<std::process::Child>>,
	ffmpeg: &str,
) {
	let mut retry = Retry::default();
	while !stop.load(Ordering::SeqCst) {
		let now = Instant::now();
		if !retry.due(now) {
			std::thread::sleep(Duration::from_millis(100));
			continue;
		}
		let Some(mut child) = crate::audio_io::spawn_mic_recorder(ffmpeg) else {
			if retry.failed(now) {
				tracing::warn!("mic: no recorder could be started — silent, retrying");
			}
			continue;
		};
		let Some(mut out) = child.stdout.take() else {
			let _ = child.kill();
			let _ = child.wait();
			retry.failed(now);
			continue;
		};
		retry.ok();
		tracing::info!("mic: capture up (recorder process fallback)");
		*slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
		if stop.load(Ordering::SeqCst) {
			break; // Drop ran between the spawn and the store: it will not see this child
		}
		let mut buf = [0u8; 3840];
		let mut echo = EchoGuard::new();
		// A read may end mid-sample: the odd byte waits for the next read.
		let mut odd: Option<u8> = None;
		loop {
			match out.read(&mut buf) {
				Ok(0) | Err(_) => break,
				Ok(n) => {
					let mut bytes: Vec<u8> = odd.take().into_iter().chain(buf[..n].iter().copied()).collect();
					if bytes.len() % 2 == 1 {
						odd = bytes.pop();
					}
					let chunk = echo.clean(bytes, Instant::now());
					if tx.blocking_send(DataMsg::Audio(chunk)).is_err() {
						return;
					}
				}
			}
		}
		// The recorder ended (device gone, killed by Drop): reap it and start another.
		if let Some(mut c) = slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
			let _ = c.kill();
			let _ = c.wait();
		}
		retry.failed(Instant::now());
	}
	if let Some(mut c) = slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
		let _ = c.kill();
		let _ = c.wait();
	}
}
