//! Startup capability detection (Moonlight model: re-probed on EVERY launch, no
//! persistence — the splash stays up while it runs).
//!
//! One background task probes BOTH directions with the real machinery:
//! - **Encode** (host role): the cached one-frame ffmpeg probes
//!   (`process::validated_encoders`/`validated_codecs`) plus the GStreamer families
//!   (`process::validated_gst_encoders` — Rockchip MPP etc.).
//! - **Decode** (client role): `pulsar-render --probe`, which runs the tiered
//!   decoder chain (zero-copy SoC → hwaccel → software) against canned keyframes.
//!
//! The result lands in `AppState.local_caps` and is pushed to the webview as a
//! `local-caps` event; the Settings UI disables what isn't available, and the
//! host's `QueryStreamCaps` reply reads it for an instant first answer.

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

#[derive(Clone, Debug, Serialize)]
pub(crate) struct EncoderCap {
	/// Wire/UI id (`nvenc`/`vaapi`/`rkmpp`/…/`software`).
	pub id: String,
	/// Which backend serves it (`ffmpeg` or `gst`).
	pub backend: String,
	/// Validated codecs (`h264`/`h265`/`av1`).
	pub codecs: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct DecoderCap {
	pub codec: String,
	pub ok: bool,
	pub name: String,
	pub hw: bool,
	pub tier: String,
	/// Encoder families whose real bitstream this decoder can't decode even though the
	/// codec validates against a conformant sample (e.g. `["nvenc"]` for rkmpp HEVC on
	/// RK3588). The negotiator drops a host-encoder × this-decoder combo that lands here.
	#[serde(default)]
	pub incompatible_with: Vec<String>,
	/// The renderer decodes this codec's 10-bit HDR10 profile (HEVC Main10 / AV1 10-bit) and
	/// can show it — on an HDR monitor, or tone-mapped to SDR (`pulsar-render --probe`
	/// `ten_bit`; Windows really decodes a canned HDR10 keyframe). Gates `StreamReq::hdr`.
	#[serde(default)]
	pub ten_bit: bool,
	/// The renderer really decoded AND presented a 4:4:4 keyframe of this codec (its separate
	/// `--probe-444`, run only when 4:4:4 is wanted — see `with_yuv444`): only then may a
	/// session ask a host for 4:4:4 in it (`StreamReq::decode_yuv444`).
	#[serde(default)]
	pub yuv444: bool,
	/// Highest session pixel rate (width × height × fps) the 4:4:4 decode keeps up with; 0 = no
	/// known limit (a hardware decoder: Windows, macOS). Linux decodes 4:4:4 in software on one
	/// thread and measures it (`pulsar-render --probe-444`): a session above it asks for 4:2:0.
	#[serde(default)]
	pub yuv444_max_px_s: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct LocalCaps {
	pub platform: String,
	pub encoders: Vec<EncoderCap>,
	pub decoders: Vec<DecoderCap>,
}

fn codec_id(c: pulsar_core::pipeline::VCodec) -> &'static str {
	match c {
		pulsar_core::pipeline::VCodec::H264 => "h264",
		pulsar_core::pipeline::VCodec::H265 => "h265",
		pulsar_core::pipeline::VCodec::Av1 => "av1",
	}
}

/// Run the full probe (blocking — call on a background thread).
pub(crate) fn probe_all(app: &AppHandle) -> LocalCaps {
	let platform = if cfg!(windows) {
		"windows"
	} else if cfg!(target_os = "macos") {
		"macos"
	} else {
		"linux"
	}
	.to_string();

	// --- Encode: ffmpeg families (probe-validated), then gst families merged in. ---
	let ffmpeg = crate::process::ffmpeg_bin(app);
	// Linux: the VA-API node settled first (a screen GPU that cannot encode is skipped).
	#[cfg(target_os = "linux")]
	let vaapi = crate::process::settle_vaapi_node(&ffmpeg, &app.state::<crate::state::AppState>().stream_cfg);
	#[cfg(not(target_os = "linux"))]
	let vaapi = app
		.state::<crate::state::AppState>()
		.stream_cfg
		.lock()
		.unwrap()
		.vaapi_device
		.clone();
	// Each phase timed in the log: a probe that never finishes shows WHERE (2026-11-12: a
	// host's startup probe never logged "local caps probed" and the host never answered).
	let t_enc = std::time::Instant::now();
	tracing::info!(ffmpeg = %ffmpeg, "caps probe: encoders");
	let validated = crate::process::validated_encoders(&ffmpeg, &vaapi);
	tracing::info!(ms = t_enc.elapsed().as_millis() as u64, ?validated, "caps probe: encoders validated");
	let mut encoders: Vec<EncoderCap> = validated
		.into_iter()
		.map(|e| {
			#[allow(unused_mut)]
			let mut codecs = if e == pulsar_core::pipeline::HwEncoder::Software {
				// Software always works (libx264 needs no probe); offer its full set.
				vec!["h264".to_string(), "h265".to_string(), "av1".to_string()]
			} else {
				crate::process::validated_codecs(&ffmpeg, e, &vaapi)
					.into_iter()
					.map(|c| codec_id(c).to_string())
					.collect()
			};
			// Windows NVENC: the ffmpeg one-frame probe is the wrong oracle — it targets the
			// display GPU (fails on hybrid laptops where NVENC works via the native SDK) and
			// its name listing passes on GPUs with no NVENC silicon at all (GP108 MX-series).
			// Ask the SDK itself which codecs this GPU's NVENC encodes (empty = no NVENC, so
			// the family is dropped below and Auto picks a working one). Per-codec on purpose:
			// a Kepler/GM107 card does H.264 only, and advertising HEVC there would put HEVC in
			// the client's SDP while the host degrades the wire to H.264 → permanent black.
			#[cfg(windows)]
			if e == pulsar_core::pipeline::HwEncoder::Nvenc {
				codecs = crate::process::nvenc_codecs_isolated()
					.iter()
					.map(|c| match c {
						pulsar_capture::Codec::H264 => "h264".to_string(),
						pulsar_capture::Codec::H265 => "h265".to_string(),
						pulsar_capture::Codec::Av1 => "av1".to_string(),
					})
					.collect();
			}
			EncoderCap {
				id: crate::process::encoder_wire_id(e).to_string(),
				backend: "ffmpeg".to_string(),
				codecs,
			}
		})
		// An encoder family with ZERO validated codecs cannot produce a stream on this
		// machine — advertising it (the old Windows behavior: trust the name listing,
		// vaapi included) made clients and the Auto pick select encoders that die at
		// init with no error surfaced anywhere. Drop them from the caps entirely.
		.filter(|e| !e.codecs.is_empty())
		.collect();
	// NVENC that is merely not answering right now (its probe sits in the NVIDIA driver, or a
	// stream reported a stall) stays LISTED, from the last answer it gave: a client is told the
	// encoder families before the stream, and only a client that was told about NVENC may be
	// moved onto it when the driver is back. The stream start itself skips it meanwhile.
	#[cfg(windows)]
	if !encoders.iter().any(|e| e.id == "nvenc") {
		let codecs = nvenc_listed(&ffmpeg, &crate::process::nvenc_codecs_to_advertise());
		if !codecs.is_empty() {
			tracing::info!(?codecs, "caps probe: NVENC is not answering — listed from its last answer");
			encoders.insert(0, EncoderCap { id: "nvenc".into(), backend: "ffmpeg".into(), codecs });
		}
	}
	#[cfg(target_os = "linux")]
	for (genc, codecs) in crate::process::validated_gst_encoders() {
		let id = genc.wire_id().to_string();
		let codecs: Vec<String> = codecs
			.into_iter()
			.map(|c| codec_id(c).to_string())
			.collect();
		if let Some(existing) = encoders.iter_mut().find(|e| e.id == id) {
			for c in codecs {
				if !existing.codecs.contains(&c) {
					existing.codecs.push(c);
				}
			}
		} else {
			// HW families ahead of the terminal software entry.
			let pos = encoders
				.iter()
				.position(|e| e.id == "software")
				.unwrap_or(encoders.len());
			encoders.insert(
				pos,
				EncoderCap {
					id,
					backend: "gst".to_string(),
					codecs,
				},
			);
		}
	}

	// --- Decode: the renderer's own tiered probe (real canned-frame decodes). ---
	let t_dec = std::time::Instant::now();
	tracing::info!("caps probe: decoders");
	let mut decoders = probe_decoders(app, &platform);
	with_yuv444(app, &mut decoders);
	tracing::info!(ms = t_dec.elapsed().as_millis() as u64, n = decoders.len(), "caps probe: decoders done");

	LocalCaps {
		platform,
		encoders,
		decoders,
	}
}

/// The renderer probe couldn't run (binary missing / unparseable output). Windows keeps a
/// minimal H.264 claim: MF's inbox H.264 decoder ships with every Windows edition that has
/// Media Foundation, and reporting NOTHING would grey out every codec in Settings on a box
/// that can in fact stream. HEVC is deliberately NOT claimed here — it needs the (optional)
/// HEVC Video Extensions, so an unprobed guess is exactly what steered negotiation into a
/// black screen. macOS keeps the pair its fallback players (mpv / ffplay) decode in software
/// on every Mac. Elsewhere, report nothing rather than guessing.
fn probe_unavailable(platform: &str) -> Vec<DecoderCap> {
	if platform == "macos" {
		return ["h264", "h265"]
			.iter()
			.map(|c| DecoderCap {
				codec: c.to_string(),
				ok: true,
				name: "mpv".into(),
				hw: false,
				tier: "software".into(),
				incompatible_with: Vec::new(),
				// The fallback players show no HDR: never ask for it unprobed.
				ten_bit: false,
				yuv444: false,
				yuv444_max_px_s: 0,
			})
			.collect();
	}
	if platform != "windows" {
		return Vec::new();
	}
	vec![DecoderCap {
		codec: "h264".into(),
		ok: true,
		name: "mediafoundation".into(),
		hw: true,
		tier: "hwaccel".into(),
		incompatible_with: Vec::new(),
		ten_bit: false,
		yuv444: false,
		yuv444_max_px_s: 0,
	}]
}

fn probe_decoders(app: &AppHandle, platform: &str) -> Vec<DecoderCap> {
	// Every platform asks its renderer (`pulsar-render --probe`): Windows enumerates the MFTs
	// it can drive (the old hardcoded "MF decodes h264+h265, always" advertised HEVC on
	// machines without the HEVC Video Extensions — a black screen), macOS asks VideoToolbox
	// (`VTIsHardwareDecodeSupported`: H.264 / HEVC always, in hardware or not; AV1 only in
	// hardware), Linux decodes canned frames.
	{
		let render = crate::process::render_bin(app);
		// Linux: choosing between `pulsar-render` and the Rockchip variant may already have
		// run this very probe (process.rs `rkmpp_choice`) — reuse it, don't open MPP twice.
		let text = match crate::process::cached_render_probe(app, &render) {
			Some(text) => text,
			None => {
				let mut cmd = std::process::Command::new(&render);
				cmd.arg("--probe").stderr(std::process::Stdio::null());
				// Probe with the SAME lib resolution the real renderer uses, so the reported
				// decoder/tier match what will actually run (rkmpp HW on RK3588, not software).
				crate::process::apply_render_lib_env(&mut cmd);
				// Windows: the probe must never flash a console window (GUI host).
				crate::process::no_window(&mut cmd);
				// Bounded: a renderer probe that hangs (a wedged decoder init) must not hold the
				// whole caps probe forever.
				let out = output_within(cmd, std::time::Duration::from_secs(15));
				let Ok(out) = out else {
					// Renderer missing: software ffmpeg decode still exists inside it when
					// present; report nothing rather than guessing.
					return probe_unavailable(platform);
				};
				String::from_utf8_lossy(&out.stdout).into_owned()
			}
		};
		let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text.trim()) else {
			return probe_unavailable(platform);
		};
		parsed
			.as_array()
			.map(|arr| {
				arr.iter()
					.filter_map(|e| {
						Some(DecoderCap {
							codec: e.get("codec")?.as_str()?.to_string(),
							ok: e.get("ok")?.as_bool()?,
							name: e
								.get("decoder")
								.and_then(|v| v.as_str())
								.unwrap_or("")
								.to_string(),
							hw: e.get("hw").and_then(|v| v.as_bool()).unwrap_or(false),
							tier: e
								.get("tier")
								.and_then(|v| v.as_str())
								.unwrap_or("")
								.to_string(),
							incompatible_with: e
								.get("incompatible_with")
								.and_then(|v| v.as_array())
								.map(|a| {
									a.iter()
										.filter_map(|x| x.as_str().map(|s| s.to_string()))
										.collect()
								})
								.unwrap_or_default(),
							ten_bit: e.get("ten_bit").and_then(|v| v.as_bool()).unwrap_or(false),
							// Filled by `with_yuv444` (a separate, opt-in probe).
							yuv444: false,
							yuv444_max_px_s: 0,
						})
					})
					.collect()
			})
			.unwrap_or_default()
	}
}

/// 4:4:4 decode caps, merged into the 4:2:0 ones from `--probe`. Only when 4:4:4 is wanted
/// (`PULSAR_YUV444`, opt-in): `pulsar-render --probe-444` creates a hardware D3D11 device and
/// runs real decodes on Windows (a wedged GPU driver can hang there — the NVIDIA one does while
/// another process is stuck in it) and times a software decode on Linux; nobody else should pay
/// that at startup. Its own short timeout, and additive: a hang or a failure here leaves every
/// codec at 4:2:0 and never touches the caps `--probe` reported.
fn with_yuv444(app: &AppHandle, decoders: &mut [DecoderCap]) {
	if std::env::var_os("PULSAR_YUV444").is_none() {
		return;
	}
	let t0 = std::time::Instant::now();
	let mut cmd = std::process::Command::new(crate::process::render_bin(app));
	cmd.arg("--probe-444").stderr(std::process::Stdio::null());
	crate::process::apply_render_lib_env(&mut cmd);
	crate::process::no_window(&mut cmd);
	let out = match output_within(cmd, std::time::Duration::from_secs(6)) {
		Ok(o) => o,
		Err(e) => {
			tracing::warn!(%e, "caps probe: 4:4:4 probe failed — 4:2:0 only");
			return;
		}
	};
	let parsed = serde_json::from_slice::<serde_json::Value>(out.stdout.trim_ascii()).unwrap_or_default();
	for e in parsed.as_array().map(Vec::as_slice).unwrap_or_default() {
		let Some(codec) = e.get("codec").and_then(|v| v.as_str()) else { continue };
		let max = e.get("max_px_s").and_then(|v| v.as_u64()).unwrap_or(0);
		if let Some(d) = decoders.iter_mut().find(|d| d.ok && d.codec == codec) {
			d.yuv444 = true;
			d.yuv444_max_px_s = max;
		}
	}
	tracing::info!(
		ms = t0.elapsed().as_millis() as u64,
		yuv444 = ?decoders.iter().filter(|d| d.yuv444).map(|d| (d.codec.as_str(), d.yuv444_max_px_s)).collect::<Vec<_>>(),
		"caps probe: 4:4:4 decode"
	);
}

/// Spawn the startup probe: runs in the background, stores the result in AppState and
/// pushes it to the webview (`local-caps`). The splash waits for that event (with a
/// safety cap) so the UI never shows un-gated options.
pub(crate) fn spawn_startup_probe(app: AppHandle) {
	static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
	// Watchdog: say so in the log when the probe is still running after 20 s (normal: < 8 s).
	std::thread::spawn(|| {
		for secs in [20u64, 60, 180] {
			std::thread::sleep(std::time::Duration::from_secs(if secs == 20 { 20 } else { secs - 20 }));
			if DONE.load(std::sync::atomic::Ordering::SeqCst) {
				return;
			}
			tracing::error!(secs, "caps probe: still not done (stuck?) — see the last 'caps probe:' line");
		}
	});
	// NVENC answered after it had been unavailable (a probe child that sat in the NVIDIA
	// driver, see `process::nv`): put it back into the caps and bring the streams that fell
	// back to another encoder home.
	#[cfg(windows)]
	{
		let app = app.clone();
		crate::process::on_nvenc_back(move || {
			refresh_nvenc(&app);
			crate::host::failover::restart_fallbacks();
		});
	}
	let spawned = std::thread::Builder::new().name("caps-probe".into()).spawn(move || {
		let t0 = std::time::Instant::now();
		tracing::info!("caps probe: started");
		let caps = probe_all(&app);
		DONE.store(true, std::sync::atomic::Ordering::SeqCst);
		tracing::info!(
			elapsed_ms = t0.elapsed().as_millis() as u64,
			encoders = caps.encoders.len(),
			decoders = caps.decoders.len(),
			"local caps probed"
		);
		*app.state::<crate::state::AppState>()
			.local_caps
			.lock()
			.unwrap() = Some(caps.clone());
		let _ = app.emit("local-caps", caps);
		// The NVENC probe may have answered while the rest of this probe was still running
		// (its hook found no caps to update yet).
		#[cfg(windows)]
		refresh_nvenc(&app);
	});
	if let Err(e) = spawned {
		tracing::error!(%e, "caps probe: thread failed to start");
	}
}

/// NVENC codecs as caps ids, kept to those the bundled ffmpeg has compiled in (the rule of
/// `process::validated_codecs`: the ffmpeg fallback path must be able to use them too).
#[cfg(windows)]
fn nvenc_listed(ffmpeg: &str, codecs: &[pulsar_capture::Codec]) -> Vec<String> {
	use pulsar_core::pipeline::{HwEncoder, VCodec};
	let listed = HwEncoder::Nvenc.available_codecs(&crate::process::encoders_text(ffmpeg));
	codecs
		.iter()
		.map(|c| match c {
			pulsar_capture::Codec::H264 => VCodec::H264,
			pulsar_capture::Codec::H265 => VCodec::H265,
			pulsar_capture::Codec::Av1 => VCodec::Av1,
		})
		.filter(|c| listed.contains(c))
		.map(|c| codec_id(c).to_string())
		.collect()
}

/// Put NVENC (back) into the stored caps with the codecs it encodes right now, and push the
/// caps to the webview again. Only ever ADDS: while NVENC is merely not answering it stays
/// listed (the stream start skips it by `process::nvenc_codecs_now`) — the Settings UI must
/// not lose the host's saved encoder choice over a driver that is busy for an hour.
#[cfg(windows)]
pub(crate) fn refresh_nvenc(app: &AppHandle) {
	let codecs = nvenc_listed(&crate::process::ffmpeg_bin(app), &crate::process::nvenc_codecs_now());
	if codecs.is_empty() {
		return;
	}
	let state = app.state::<crate::state::AppState>();
	let snapshot = {
		let mut g = state.local_caps.lock().unwrap();
		let Some(caps) = g.as_mut() else { return };
		match caps.encoders.iter_mut().find(|e| e.id == "nvenc" && e.backend == "ffmpeg") {
			Some(e) if e.codecs == codecs => return,
			Some(e) => e.codecs = codecs,
			// First in the list, where `process::validated_encoders` puts it.
			None => caps.encoders.insert(0, EncoderCap { id: "nvenc".into(), backend: "ffmpeg".into(), codecs }),
		}
		caps.clone()
	};
	tracing::info!(encoders = ?snapshot.encoders.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), "local caps: NVENC is back");
	let _ = app.emit("local-caps", snapshot);
}

/// `Command::output` with a deadline (the child is killed past it → Err TimedOut).
pub(crate) fn output_within(mut cmd: std::process::Command, limit: std::time::Duration) -> std::io::Result<std::process::Output> {
	use std::io::Read as _;
	cmd.stdout(std::process::Stdio::piped());
	let mut child = cmd.spawn()?;
	#[cfg(windows)]
	crate::job::assign(&child);
	let mut so = child.stdout.take();
	let reader = std::thread::spawn(move || {
		let mut buf = Vec::new();
		if let Some(s) = so.as_mut() {
			let _ = s.read_to_end(&mut buf);
		}
		buf
	});
	let t0 = std::time::Instant::now();
	let status = loop {
		if let Some(st) = child.try_wait()? {
			break st;
		}
		if t0.elapsed() >= limit {
			let _ = child.kill();
			// Reaped elsewhere (see process::probe_ok): never wait on a stuck child here.
			std::thread::spawn(move || {
				let _ = child.wait();
			});
			tracing::error!(ms = t0.elapsed().as_millis() as u64, "caps probe: renderer probe did not finish — killed");
			return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "probe timed out"));
		}
		std::thread::sleep(std::time::Duration::from_millis(50));
	};
	let stdout = reader.join().unwrap_or_default();
	Ok(std::process::Output { status, stdout, stderr: Vec::new() })
}

/// Tauri command: the probed caps (None while the startup probe is still running —
/// the frontend also listens for the `local-caps` event).
#[tauri::command]
pub(crate) fn local_caps(state: tauri::State<'_, crate::state::AppState>) -> Option<LocalCaps> {
	state.local_caps.lock().unwrap().clone()
}
