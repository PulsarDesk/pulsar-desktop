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
//! **Host-silent redirect armed** (game mode's usual case): the default endpoint is then the
//! virtual sink (Steam Streaming Speakers), and its slider is the host's volume. Its loopback
//! is not real-time (46.8k–56.4k frames/s, bursty — audio.md), so it is **paced**: no silence
//! fill, a jitter buffer with a drift-correcting resampler (`pulsar_core::audio::
//! DriftResampler`, 30 ms target growing to 60 ms on underruns) and a wall-clock pacer that
//! sends steady 5 ms blocks. Real endpoints run in real time and skip that (no added latency).
//!
//! Both captures feed one [`RtpAudioSink`] through a [`Mux`]: process loopback runs the whole
//! session; only the active source's PCM reaches the encoder, so a switch never doubles the
//! stream. A switch fades the old source out over 5 ms and the new one in (`SwitchFader`), so
//! it doesn't click; the gap is ≤ 5 ms plus the two captures' latency difference. The endpoint
//! capture is opened only when wanted and switched to once it is up.
//!
//! Not used (stays process loopback): an Include-mode stream (launched game / picked window)
//! while another in-process stream runs on this host (same-host co-op: the endpoint loopback
//! would give each pane the other's game).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pulsar_core::audio::{
	AudioSource, DriftResampler, ProcessLoopbackMode, RouteAction, RouteCapture, RouteInput,
	SourceRouter, StereoConverter, SwitchFader,
};

use super::opus_rtp::RtpAudioSink;

/// How often the controller looks at the pad, the default endpoint's volume and mute.
const POLL: Duration = Duration::from_millis(20);
/// Re-ask which endpoint is the default every this many polls (100 ms).
const RECHECK_EVERY: u32 = 5;
const RATE: u32 = pulsar_core::audio::SAMPLE_RATE;
/// The paced endpoint path's output block (5 ms).
const PACE_FRAMES: usize = RATE as usize / 200;

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
	fader: SwitchFader,
	scratch: Vec<u8>,
}

impl Mux {
	fn new(sink: RtpAudioSink) -> Self {
		Self {
			sink,
			fader: SwitchFader::new(RATE, AudioSource::Process),
			scratch: Vec::new(),
		}
	}

	/// 48 kHz stereo f32le from `src`: faded and passed on, or dropped.
	fn write(&mut self, src: AudioSource, buf: &[u8]) -> std::io::Result<()> {
		if self.sink.retired() {
			return Err(std::io::Error::new(
				std::io::ErrorKind::BrokenPipe,
				"audio stream retired",
			));
		}
		let mut frames: Vec<f32> = buf
			.chunks_exact(4)
			.map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
			.collect();
		if !self.fader.process(src, &mut frames) {
			return Ok(());
		}
		self.scratch.clear();
		self.scratch
			.extend(frames.iter().flat_map(|s| s.to_le_bytes()));
		use std::io::Write;
		self.sink.write_all(&self.scratch)
	}
}

type SharedMux = Arc<Mutex<Mux>>;

/// One capture's way into the [`Mux`].
struct MuxInput {
	mux: SharedMux,
	src: AudioSource,
}

impl std::io::Write for MuxInput {
	fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
		self.mux.lock().unwrap().write(self.src, buf)?;
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
	/// The capture is up (delivered PCM; paced: opened).
	live: AtomicBool,
	/// The capture thread returned (on its own, or after `stop`).
	ended: AtomicBool,
	/// Compensation gain (f32 bits).
	gain: AtomicU32,
}

/// Where the endpoint loopback's converted PCM goes.
enum EndpointOut {
	/// Straight to the mux (a real-time endpoint).
	Direct(MuxInput),
	/// Into the jitter buffer the pacer drains (the virtual sink).
	Paced(Arc<Mutex<DriftResampler>>),
}

/// The endpoint loopback's sink: converts to 48 kHz stereo f32 with the gain applied.
struct EndpointInput {
	conv: StereoConverter,
	out: Vec<u8>,
	state: Arc<EndpointState>,
	to: EndpointOut,
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
		if self.out.is_empty() {
			return Ok(buf.len());
		}
		match &mut self.to {
			EndpointOut::Direct(mux) => mux.write_all(&self.out)?,
			EndpointOut::Paced(rs) => {
				let pcm: Vec<f32> = self
					.out
					.chunks_exact(4)
					.map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
					.collect();
				rs.lock().unwrap().push(&pcm);
			}
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
	/// `paced`: the endpoint is the non-real-time virtual sink (see the module docs).
	fn start(id: String, mux: &SharedMux, gain: f32, paced: bool) -> Self {
		let state = Arc::new(EndpointState::default());
		state.gain.store(gain.to_bits(), Ordering::Relaxed);
		let (st, mux, dev) = (state.clone(), mux.clone(), id.clone());
		let spawned = std::thread::Builder::new()
			.name("audio-endpoint".into())
			.spawn(move || {
				run_endpoint_capture(&dev, &mux, &st, paced);
				st.ended.store(true, Ordering::SeqCst);
			});
		if spawned.is_err() {
			state.ended.store(true, Ordering::SeqCst);
		}
		Self { id, state }
	}

	fn stop(&self) {
		// The thread ends at its next write (≤ 10 ms; a device being re-opened takes longer,
		// but it can no longer write). The pacer checks the flag every 2 ms.
		self.state.stop.store(true, Ordering::SeqCst);
	}
}

fn run_endpoint_capture(id: &str, mux: &SharedMux, state: &Arc<EndpointState>, paced: bool) {
	let mut fmt = match pulsar_core::audio::loopback_format(Some(id)) {
		Ok(f) => f,
		Err(e) => {
			tracing::warn!("audio: default-endpoint loopback unavailable: {e}");
			return;
		}
	};
	let rs = paced.then(|| Arc::new(Mutex::new(DriftResampler::new())));
	if let Some(rs) = &rs {
		// An idle endpoint delivers nothing: the pacer's silence is the stream until it does,
		// so the switch need not wait for the first packet.
		state.live.store(true, Ordering::SeqCst);
		let (rs, mux, st) = (rs.clone(), mux.clone(), state.clone());
		let pacer = std::thread::Builder::new()
			.name("audio-pacer".into())
			.spawn(move || pace(&rs, &mux, &st));
		if pacer.is_err() {
			return;
		}
		tracing::info!(
			"audio: endpoint is the host-silent virtual sink — paced through a jitter buffer"
		);
	}
	loop {
		if fmt.rate != RATE {
			// The in-process encoder is 48 kHz; no general resampler here.
			tracing::warn!(
				rate = fmt.rate,
				"audio: default endpoint is not 48 kHz — keeping process loopback"
			);
			return;
		}
		let to = match &rs {
			Some(rs) => EndpointOut::Paced(rs.clone()),
			None => EndpointOut::Direct(MuxInput {
				mux: mux.clone(),
				src: AudioSource::Endpoint,
			}),
		};
		let sink = EndpointInput {
			conv: StereoConverter::new(fmt.channels, fmt.float, fmt.rate),
			out: Vec::new(),
			state: state.clone(),
			to,
		};
		// Paced: no silence fill — it would land between the sink's bursts.
		match pulsar_core::audio::run_device_loopback_capture(sink, id, fmt, !paced) {
			Ok(Some(new_fmt)) => fmt = new_fmt,
			Ok(None) => return,
			Err(e) => {
				tracing::warn!("audio: default-endpoint loopback ended: {e}");
				return;
			}
		}
	}
}

/// Drain the jitter buffer into the mux at a steady 48 kHz of wall clock, 5 ms per block,
/// until the capture stops or ends.
fn pace(rs: &Mutex<DriftResampler>, mux: &SharedMux, state: &EndpointState) {
	let start = Instant::now();
	let mut sent: u64 = 0;
	let mut block = Vec::with_capacity(PACE_FRAMES * 2);
	let mut bytes = Vec::with_capacity(PACE_FRAMES * 8);
	let mut next_log = Instant::now() + Duration::from_secs(10);
	while !state.stop.load(Ordering::SeqCst) && !state.ended.load(Ordering::SeqCst) {
		let due = (start.elapsed().as_secs_f64() * RATE as f64) as u64;
		if due > sent + RATE as u64 / 5 {
			// The thread stalled (> 200 ms): don't burst the backlog out; resume from now.
			sent = due - due % PACE_FRAMES as u64;
		}
		while sent + PACE_FRAMES as u64 <= due {
			block.clear();
			rs.lock().unwrap().pull(PACE_FRAMES, &mut block);
			bytes.clear();
			bytes.extend(block.iter().flat_map(|s| s.to_le_bytes()));
			if mux
				.lock()
				.unwrap()
				.write(AudioSource::Endpoint, &bytes)
				.is_err()
			{
				return; // stream retired
			}
			sent += PACE_FRAMES as u64;
		}
		if Instant::now() >= next_log {
			let r = rs.lock().unwrap();
			let s = r.stats();
			tracing::info!(
				target_ms = r.target_ms(),
				fill_ms = r.fill() / 48.0,
				ratio = r.ratio(),
				underruns = s.underruns,
				overflows = s.overflows,
				dropped = s.dropped_frames,
				"audio: virtual-sink jitter buffer"
			);
			next_log += Duration::from_secs(10);
		}
		std::thread::sleep(Duration::from_millis(2));
	}
}

/// Start the session's system-audio capture into `sink` with the DualSense routing: process
/// loopback (`pid`, `mode`) now, the endpoint loopback whenever a virtual pad calls for it.
/// `false` = the process loopback did not start (the caller falls back, as before).
pub(super) fn start(sink: RtpAudioSink, pid: u32, mode: ProcessLoopbackMode) -> bool {
	let mux: SharedMux = Arc::new(Mutex::new(Mux::new(sink)));
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
	match mode {
		ProcessLoopbackMode::Exclude => true,
		ProcessLoopbackMode::Include => STREAMS.load(Ordering::SeqCst) <= 1,
	}
}

fn control(mux: SharedMux, mode: ProcessLoopbackMode) {
	let mut router = SourceRouter::new();
	let mut watch: Option<pulsar_core::audio::DefaultRenderWatch> = None;
	let mut capture: Option<EndpointCapture> = None;
	let mut wanted = AudioSource::Process;
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
				// The host-silent virtual sink is the one non-real-time endpoint we tap.
				let paced = super::REDIRECT_SINK_ID.lock().unwrap().as_deref() == Some(id.as_str());
				capture = Some(EndpointCapture::start(
					id,
					&mux,
					d.gain.unwrap_or(1.0),
					paced,
				));
			}
			RouteAction::Stop => {
				if let Some(c) = capture.take() {
					c.stop();
				}
			}
		}
		if d.source != wanted {
			mux.lock().unwrap().fader.switch_to(d.source);
			wanted = d.source;
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
		let mux: SharedMux = Arc::new(Mutex::new(Mux::new(sink)));
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
		mux.lock().unwrap().fader.switch_to(AudioSource::Endpoint);
		// The old source fades out (5 ms), then the new one passes.
		process.write_all(&packet).unwrap();
		endpoint.write_all(&packet).unwrap();
		process.write_all(&packet).unwrap();
		assert_eq!(mux.lock().unwrap().sink.packets, 3);
		assert_eq!(mux.lock().unwrap().fader.active(), AudioSource::Endpoint);
		// Teardown ends both captures, active or not.
		epoch.fetch_add(1, Ordering::SeqCst);
		assert!(process.write(&packet).is_err() && endpoint.write(&packet).is_err());
	}
}
