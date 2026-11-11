//! DualSense-aware host audio capture (Windows, in-process encoder path).
//!
//! Process loopback (the default system-mix capture) is endpoint-agnostic: what a game renders
//! to a virtual DualSense's own endpoint (speaker on ch 1/2, HD haptics on ch 3/4) is captured
//! too and the client hears it in its normal audio (verified 2026-11-11). While a virtual pad
//! with an audio endpoint is plugged in on this host (`ds5::host::audio_pad_plugged`), the
//! stream is fed from the **default render endpoint's loopback** instead, which never carries
//! the pad endpoint's audio. That loopback sits after the endpoint volume, so it is divided by
//! the endpoint's master attenuation (polled every 20 ms, smoothed) — the host's volume does
//! not reach the client. Muted / volume 0 (nothing to recover), the pad's endpoint as the
//! default, or a failing endpoint capture: process loopback, as before (the pad audio leaks
//! again meanwhile; accepted). Decision logic: `pulsar_core::audio::SourceRouter`.
//!
//! Both captures feed one [`RtpAudioSink`] through a [`Mux`]: process loopback runs the whole
//! session; only the active source's PCM reaches the encoder, so a switch neither doubles nor
//! gaps the stream (beyond the two captures' latency difference, a few ms). The endpoint
//! capture is opened only when wanted and switched to once it delivers PCM.
//!
//! Not used (stays process loopback): a host-silent redirect armed (the default is then the
//! virtual sink, whose loopback is not real-time — audio.md), or an Include-mode stream
//! (launched game / picked window) while another in-process stream runs on this host
//! (same-host co-op: the endpoint loopback would give each pane the other's game).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pulsar_core::audio::{
	AudioSource, ProcessLoopbackMode, RouteAction, RouteCapture, RouteInput, SourceRouter,
	StereoConverter,
};

use super::opus_rtp::RtpAudioSink;

/// How often the controller looks at the pad, the default endpoint's volume and mute.
const POLL: Duration = Duration::from_millis(20);
/// Re-ask which endpoint is the default every this many polls (100 ms).
const RECHECK_EVERY: u32 = 5;

/// Live in-process audio streams on this host (each runs one controller).
static STREAMS: AtomicUsize = AtomicUsize::new(0);

struct StreamToken;

impl StreamToken {
	fn new() -> Self {
		STREAMS.fetch_add(1, Ordering::SeqCst);
		Self
	}
}

impl Drop for StreamToken {
	fn drop(&mut self) {
		STREAMS.fetch_sub(1, Ordering::SeqCst);
	}
}

/// The encoder plus which source may write to it.
struct Mux {
	sink: RtpAudioSink,
	active: AudioSource,
}

type SharedMux = Arc<Mutex<Mux>>;

/// One capture's way into the [`Mux`]: written through only while its source is active.
struct MuxInput {
	mux: SharedMux,
	src: AudioSource,
}

impl std::io::Write for MuxInput {
	fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
		let mut m = self.mux.lock().unwrap();
		if m.sink.retired() {
			return Err(std::io::Error::new(
				std::io::ErrorKind::BrokenPipe,
				"audio stream retired",
			));
		}
		if m.active == self.src {
			m.sink.write_all(buf)?;
		}
		Ok(buf.len())
	}
	fn flush(&mut self) -> std::io::Result<()> {
		Ok(())
	}
}

/// Shared between the controller and one endpoint-capture thread.
#[derive(Default)]
struct EndpointState {
	/// Set by the controller: end the capture at its next write.
	stop: AtomicBool,
	/// The capture delivered PCM.
	live: AtomicBool,
	/// The capture thread returned (on its own, or after `stop`).
	ended: AtomicBool,
	/// Compensation gain (f32 bits).
	gain: AtomicU32,
}

/// The endpoint loopback's sink: converts to 48 kHz stereo f32 with the gain applied.
struct EndpointInput {
	conv: StereoConverter,
	out: Vec<u8>,
	state: Arc<EndpointState>,
	mux: MuxInput,
}

impl std::io::Write for EndpointInput {
	fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
		if self.state.stop.load(Ordering::SeqCst) {
			return Err(std::io::Error::new(
				std::io::ErrorKind::BrokenPipe,
				"endpoint capture stopped",
			));
		}
		self.conv
			.set_gain(f32::from_bits(self.state.gain.load(Ordering::Relaxed)));
		self.out.clear();
		self.conv.convert(buf, &mut self.out);
		self.state.live.store(true, Ordering::SeqCst);
		if !self.out.is_empty() {
			self.mux.write_all(&self.out)?;
		}
		Ok(buf.len())
	}
	fn flush(&mut self) -> std::io::Result<()> {
		Ok(())
	}
}

/// A running endpoint capture as the controller tracks it.
struct EndpointCapture {
	id: String,
	state: Arc<EndpointState>,
}

impl EndpointCapture {
	fn start(id: String, mux: &SharedMux, gain: f32) -> Self {
		let state = Arc::new(EndpointState::default());
		state.gain.store(gain.to_bits(), Ordering::Relaxed);
		let (st, mux, dev) = (state.clone(), mux.clone(), id.clone());
		let spawned = std::thread::Builder::new()
			.name("audio-endpoint".into())
			.spawn(move || {
				run_endpoint_capture(&dev, &mux, &st);
				st.ended.store(true, Ordering::SeqCst);
			});
		if spawned.is_err() {
			state.ended.store(true, Ordering::SeqCst);
		}
		Self { id, state }
	}

	fn stop(&self) {
		// The thread ends at its next write (≤ 10 ms; a device being re-opened takes longer,
		// but it can no longer write).
		self.state.stop.store(true, Ordering::SeqCst);
	}
}

fn run_endpoint_capture(id: &str, mux: &SharedMux, state: &Arc<EndpointState>) {
	let mut fmt = match pulsar_core::audio::loopback_format(Some(id)) {
		Ok(f) => f,
		Err(e) => {
			tracing::warn!("audio: default-endpoint loopback unavailable: {e}");
			return;
		}
	};
	loop {
		if fmt.rate != pulsar_core::audio::SAMPLE_RATE {
			// The in-process encoder is 48 kHz; no resampler here.
			tracing::warn!(
				rate = fmt.rate,
				"audio: default endpoint is not 48 kHz — keeping process loopback"
			);
			return;
		}
		let sink = EndpointInput {
			conv: StereoConverter::new(fmt.channels, fmt.float, fmt.rate),
			out: Vec::new(),
			state: state.clone(),
			mux: MuxInput {
				mux: mux.clone(),
				src: AudioSource::Endpoint,
			},
		};
		match pulsar_core::audio::run_device_loopback_capture(sink, id, fmt) {
			Ok(Some(new_fmt)) => fmt = new_fmt,
			Ok(None) => return,
			Err(e) => {
				tracing::warn!("audio: default-endpoint loopback ended: {e}");
				return;
			}
		}
	}
}

/// Start the session's system-audio capture into `sink` with the DualSense routing: process
/// loopback (`pid`, `mode`) now, the endpoint loopback whenever a virtual pad calls for it.
/// `false` = the process loopback did not start (the caller falls back, as before).
pub(super) fn start(sink: RtpAudioSink, pid: u32, mode: ProcessLoopbackMode) -> bool {
	let mux: SharedMux = Arc::new(Mutex::new(Mux {
		sink,
		active: AudioSource::Process,
	}));
	let process = MuxInput {
		mux: mux.clone(),
		src: AudioSource::Process,
	};
	if !super::start_process_capture(process, pid, mode) {
		return false;
	}
	let token = StreamToken::new();
	let spawned = std::thread::Builder::new()
		.name("audio-route".into())
		.spawn(move || {
			let _token = token;
			control(mux, mode);
		});
	if let Err(e) = spawned {
		// Process loopback keeps streaming; only the DualSense routing is missing.
		tracing::warn!("audio: no source controller ({e}) — process loopback only");
	}
	true
}

/// Whether this stream may use the endpoint loopback right now (see the module docs).
fn allowed(mode: ProcessLoopbackMode) -> bool {
	if super::REDIRECT_SINK_ID.lock().unwrap().is_some() {
		return false;
	}
	match mode {
		ProcessLoopbackMode::Exclude => true,
		ProcessLoopbackMode::Include => STREAMS.load(Ordering::SeqCst) <= 1,
	}
}

fn control(mux: SharedMux, mode: ProcessLoopbackMode) {
	let mut router = SourceRouter::new();
	let mut watch: Option<pulsar_core::audio::DefaultRenderWatch> = None;
	let mut capture: Option<EndpointCapture> = None;
	let mut active = AudioSource::Process;
	let mut tick: u32 = 0;
	loop {
		if mux.lock().unwrap().sink.retired() {
			break;
		}
		let pad = crate::ds5::host::audio_pad_plugged();
		// No pad: no COM, no polling — the stream is exactly today's process loopback.
		let obs = if pad {
			if watch.is_none() {
				watch = pulsar_core::audio::DefaultRenderWatch::new()
					.map_err(|e| tracing::warn!("audio: default endpoint watch unavailable: {e}"))
					.ok();
			}
			watch
				.as_mut()
				.and_then(|w| w.poll(tick % RECHECK_EVERY == 0))
		} else {
			watch = None;
			None
		};
		let failed = capture
			.as_ref()
			.is_some_and(|c| c.state.ended.load(Ordering::SeqCst));
		if failed {
			capture = None;
		}
		let cap = match &capture {
			_ if failed => RouteCapture::Failed,
			None => RouteCapture::Idle,
			Some(c) if c.state.live.load(Ordering::SeqCst) => RouteCapture::Live(&c.id),
			Some(c) => RouteCapture::Starting(&c.id),
		};
		let d = router.step(
			RouteInput {
				pad_plugged: pad,
				allowed: allowed(mode),
				endpoint: obs.as_ref(),
				capture: cap,
			},
			Instant::now(),
		);
		if let (Some(c), Some(g)) = (&capture, d.gain) {
			c.state.gain.store(g.to_bits(), Ordering::Relaxed);
		}
		match d.action {
			RouteAction::None => {}
			RouteAction::Start(id) => {
				capture = Some(EndpointCapture::start(id, &mux, d.gain.unwrap_or(1.0)));
			}
			RouteAction::Stop => {
				if let Some(c) = capture.take() {
					c.stop();
				}
			}
		}
		if d.source != active {
			mux.lock().unwrap().active = d.source;
			active = d.source;
			match d.source {
				AudioSource::Endpoint => tracing::info!(
					gain = d.gain.unwrap_or(1.0),
					"audio: virtual DualSense plugged — streaming the default endpoint's loopback \
					 (host volume compensated; the pad's own audio stays on the pad)"
				),
				AudioSource::Process => tracing::info!(
					pad,
					muted = obs.as_ref().map(|o| o.muted),
					"audio: streaming process loopback (no pad, or the default endpoint is muted / at 0 / \
					 the pad / unavailable)"
				),
			}
		}
		tick = tick.wrapping_add(1);
		std::thread::sleep(POLL);
	}
	if let Some(c) = capture.take() {
		c.stop();
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::io::Write;
	use std::sync::atomic::AtomicU64;

	#[test]
	fn only_the_active_source_reaches_the_encoder() {
		let rx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
		let epoch: crate::process::ProcEpoch = Arc::new(AtomicU64::new(0));
		let sink = RtpAudioSink::new(
			rx.local_addr().unwrap(),
			super::super::opus_rtp::Codec::L16,
			epoch.clone(),
		)
		.unwrap();
		let mux: SharedMux = Arc::new(Mutex::new(Mux {
			sink,
			active: AudioSource::Process,
		}));
		let mut process = MuxInput {
			mux: mux.clone(),
			src: AudioSource::Process,
		};
		let mut endpoint = MuxInput {
			mux: mux.clone(),
			src: AudioSource::Endpoint,
		};
		// 5 ms of 48 kHz stereo f32 = one L16 packet.
		let packet = vec![0u8; 240 * 2 * 4];
		process.write_all(&packet).unwrap();
		endpoint.write_all(&packet).unwrap();
		assert_eq!(
			mux.lock().unwrap().sink.packets,
			1,
			"the inactive source is dropped"
		);
		mux.lock().unwrap().active = AudioSource::Endpoint;
		process.write_all(&packet).unwrap();
		endpoint.write_all(&packet).unwrap();
		assert_eq!(mux.lock().unwrap().sink.packets, 2);
		// Teardown ends both captures, active or not.
		epoch.fetch_add(1, Ordering::SeqCst);
		assert!(process.write(&packet).is_err() && endpoint.write(&packet).is_err());
	}
}
