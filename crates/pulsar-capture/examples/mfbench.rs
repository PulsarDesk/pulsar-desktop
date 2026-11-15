//! Headless bench of the Media Foundation native path (no window, no UI, loopback only).
//!
//! ```text
//! mfbench synth   [secs=10] [fps=60] [WxH=1920x1080] [h264|h265|av1] [kbps=8000] [--rfi] [--out f] [--dest port]
//! mfbench capture [secs=10] [fps=60] [h264|h265] [kbps=8000] [--out f]
//! mfbench listen  <port> [secs=10] [fps=60] [h264|h265] [--out f]
//! ```
//! * `synth` — synthetic moving content through `MfEncoder` (the encoder alone); scripted at
//!   3 s a bitrate halving, at 5 s a doubling, at 6 s a keyframe request, at 7 s (with `--rfi`)
//!   a reported loss of the frame 3 back.
//! * `capture` — this machine's monitor 0 through the whole native path
//!   (`start_capture_encode` with `NativeEncoder::MediaFoundation`, DXGI capture): same script
//!   through the `CaptureHandle`.
//! * `listen` — only the receiver, for timing another sender (the ffmpeg CLI fallback).
//!
//! The receiver times every access unit's last packet (marker bit) and prints the inter-frame
//! arrival spread (the "clumping" the host's forwarder/pacer sees), the bitrate per 500 ms, the
//! keyframes, and — with `--out` — writes the Annex-B stream (plus `<out>.lossy` with the
//! frames a client would drop after a reported loss, up to the recovery tag) for ffmpeg checks.

#[cfg(windows)]
fn main() {
	use std::sync::{Arc, Mutex};
	use std::time::{Duration, Instant};

	let args: Vec<String> = std::env::args().collect();
	let mode = args.get(1).cloned().unwrap_or_else(|| "synth".into());
	let pos: Vec<&String> = args.iter().skip(2).filter(|a| !a.starts_with("--")).collect();
	let flag = |f: &str| args.iter().any(|a| a == f);
	let opt = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned();
	let codec_of = |s: Option<&&String>| match s.map(|s| s.as_str()) {
		Some("h265") | Some("hevc") => pulsar_capture::Codec::H265,
		Some("av1") => pulsar_capture::Codec::Av1,
		_ => pulsar_capture::Codec::H264,
	};
	let num = |i: usize, d: u64| pos.get(i).and_then(|s| s.parse::<u64>().ok()).unwrap_or(d);
	let out = opt("--out");

	let (port_arg, secs, fps, codec) = match mode.as_str() {
		"listen" => (num(0, 0) as u16, num(1, 10), num(2, 60) as u32, codec_of(pos.get(3))),
		"capture" => (0, num(0, 10), num(1, 60) as u32, codec_of(pos.get(2))),
		_ => (0, num(0, 10), num(1, 60) as u32, codec_of(pos.get(3))),
	};
	let rx = std::net::UdpSocket::bind(("127.0.0.1", port_arg)).expect("bind");
	let _ = socket2::SockRef::from(&rx).set_recv_buffer_size(8 << 20);
	let port = rx.local_addr().unwrap().port();
	rx.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
	let t0 = Instant::now();
	let recv = Arc::new(Mutex::new(Receiver::new(codec)));
	let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
	let rthread = {
		let recv = recv.clone();
		let stop = stop.clone();
		std::thread::spawn(move || {
			let mut buf = vec![0u8; 65536];
			while !stop.load(std::sync::atomic::Ordering::Relaxed) {
				if let Ok(n) = rx.recv(&mut buf) {
					let at = t0.elapsed();
					recv.lock().unwrap().packet(&buf[..n], at);
				}
			}
		})
	};
	// Script marks (since t0), printed next to the receiver's view of them.
	// (wall time since t0, the event's frame RTP timestamp when known, what)
	let mut marks: Vec<(Duration, Option<u32>, String)> = Vec::new();
	let mut losses: Vec<u32> = Vec::new();
	let mut sent_ts: Vec<u32> = Vec::new();

	match mode.as_str() {
		"listen" => {
			println!("listening on 127.0.0.1:{port} for {secs} s");
			std::thread::sleep(Duration::from_secs(secs));
		}
		"capture" => {
			let kbps = num(3, 8000) as u32;
			let cfg = pulsar_capture::CaptureConfig {
				width: 0,
				height: 0,
				fps,
				bitrate_kbps: kbps,
				dest: format!("rtp://127.0.0.1:{port}"),
				codec,
				output_idx: 0,
				low_latency: true,
				intra_refresh: false,
				rfi: flag("--rfi"),
				draw_mouse: true,
				window_hwnd: None,
				encoder: pulsar_capture::NativeEncoder::MediaFoundation,
			};
			let h = pulsar_capture::start_capture_encode(cfg).expect("start_capture_encode (MF)");
			println!("capture started in {} ms", t0.elapsed().as_millis());
			let script = [(3.0, kbps / 2), (5.0, kbps)];
			let mut si = 0;
			let mut idr_done = false;
			while t0.elapsed() < Duration::from_secs(secs) {
				let t = t0.elapsed().as_secs_f32();
				if si < script.len() && t >= script[si].0 {
					h.set_bitrate(script[si].1);
					marks.push((t0.elapsed(), None, format!("bitrate -> {} kbps", script[si].1)));
					si += 1;
				}
				if !idr_done && t >= 6.0 {
					h.request_idr();
					marks.push((t0.elapsed(), None, "keyframe request".into()));
					idr_done = true;
				}
				std::thread::sleep(Duration::from_millis(5));
			}
			let st = h.stats();
			println!("capture stats: {st:?}");
			h.stop();
		}
		_ => {
			let (w, hh) = pos
				.get(2)
				.and_then(|s| s.split_once('x'))
				.and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
				.unwrap_or((1920u32, 1080u32));
			let kbps = num(4, 8000) as u32;
			let f = |s: u64| s * fps as u64;
			let mut events = vec![
				(f(3), pulsar_capture::synth_bench::SynthEvent::Bitrate(kbps / 2)),
				(f(5), pulsar_capture::synth_bench::SynthEvent::Bitrate(kbps * 2)),
				(f(6), pulsar_capture::synth_bench::SynthEvent::Keyframe),
			];
			if flag("--rfi") {
				for s in [7, 8] {
					events.push((f(s) + 7, pulsar_capture::synth_bench::SynthEvent::Loss { back: 3 }));
				}
			}
			let evs = events.clone();
			let cfg = pulsar_capture::synth_bench::SynthConfig {
				width: w,
				height: hh,
				fps,
				frames: secs * fps as u64,
				bitrate_kbps: kbps,
				codec,
				rfi: flag("--rfi"),
				low_latency: !flag("--quality"),
				dest: format!("rtp://127.0.0.1:{}", opt("--dest").and_then(|d| d.parse::<u16>().ok()).unwrap_or(port)),
				events,
			};
			let rep = pulsar_capture::synth_bench::run_synthetic_mf(&cfg).expect("synthetic MF run");
			sent_ts = rep.frames.iter().map(|f| f.2).collect();
			for (at, ev) in &evs {
				if let Some(f) = rep.frames.get(*at as usize) {
					marks.push((Duration::ZERO, Some(f.2), format!("frame {at}: {ev:?}")));
				}
			}
			let mut sub: Vec<u32> = rep.frames.iter().map(|f| f.1).collect();
			sub.sort_unstable();
			let p = |v: &[u32], q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
			println!(
				"encoder \"{}\" {w}x{hh}@{fps} {codec:?}: {} frames, submit (blt+encode+output) p50 {:.2} ms p90 {:.2} p99 {:.2} max {:.2}, errors {}",
				rep.encoder,
				rep.frames.len(),
				p(&sub, 0.5) as f32 / 1000.0,
				p(&sub, 0.9) as f32 / 1000.0,
				p(&sub, 0.99) as f32 / 1000.0,
				p(&sub, 1.0) as f32 / 1000.0,
				rep.submit_errors
			);
			if flag("--slow") {
				let slow: Vec<String> = rep.frames.iter().enumerate().filter(|(_, f)| f.1 > 12_000).map(|(i, f)| format!("{i}:{:.1}", f.1 as f32 / 1000.0)).collect();
				println!("slow submits (>12 ms) frame:ms: {}", slow.join(" "));
			}
			for (ts, r) in &rep.losses {
				println!("loss of frame ts={ts}: {r:?}");
				if r.is_ok() {
					losses.push(*ts);
				}
			}
		}
	}
	std::thread::sleep(Duration::from_millis(300));
	stop.store(true, std::sync::atomic::Ordering::Relaxed);
	let _ = rthread.join();
	let r = recv.lock().unwrap();
	r.report(fps, &marks);
	if !sent_ts.is_empty() {
		let got: std::collections::HashSet<u32> = r.aus.iter().filter(|a| a.complete).map(|a| a.ts).collect();
		let missing: Vec<usize> = sent_ts.iter().enumerate().filter(|(_, t)| !got.contains(t)).map(|(i, _)| i).collect();
		println!("frames submitted {} / received {}; missing frame indices: {:?}", sent_ts.len(), got.len(), missing);
	}
	if flag("--slow") {
		let s: Vec<String> = r.aus.iter().skip(40).take(40).map(|a| format!("{}", a.bytes / 100)).collect();
		println!("AU sizes (x100 B) from AU 40: {}", s.join(" "));
	}
	if let Some(path) = out {
		r.write(&path, &[]);
		if !losses.is_empty() {
			r.write(&format!("{path}.lossy"), &losses);
		}
		println!("wrote {path}{}", if losses.is_empty() { "" } else { " and .lossy" });
	}
}

#[cfg(windows)]
struct Au {
	ts: u32,
	first_at: std::time::Duration,
	last_at: std::time::Duration,
	bytes: usize,
	key: bool,
	recovery: bool,
	annexb: Vec<u8>,
	complete: bool,
}

#[cfg(windows)]
struct Receiver {
	codec: pulsar_capture::Codec,
	aus: Vec<Au>,
	fu: Vec<u8>,
	packets: u64,
}

#[cfg(windows)]
impl Receiver {
	fn new(codec: pulsar_capture::Codec) -> Self {
		Receiver { codec, aus: Vec::new(), fu: Vec::new(), packets: 0 }
	}

	fn nal(&mut self, nal: &[u8]) {
		let Some(au) = self.aus.last_mut() else { return };
		if nal.is_empty() {
			return;
		}
		let key = match self.codec {
			pulsar_capture::Codec::H264 => nal[0] & 0x1f == 5,
			_ => matches!((nal[0] >> 1) & 0x3f, 19 | 20 | 21),
		};
		au.key |= key;
		if nal.windows(16).any(|w| w == b"PULSAR-RFI-RECOV") {
			au.recovery = true;
		}
		au.annexb.extend_from_slice(&[0, 0, 0, 1]);
		au.annexb.extend_from_slice(nal);
	}

	fn packet(&mut self, p: &[u8], at: std::time::Duration) {
		if p.len() < 13 {
			return;
		}
		self.packets += 1;
		let marker = p[1] & 0x80 != 0;
		let ts = u32::from_be_bytes([p[4], p[5], p[6], p[7]]);
		let cc = (p[0] & 0x0f) as usize;
		let pl = &p[12 + cc * 4..];
		if self.aus.last().is_none_or(|a| a.ts != ts) {
			self.aus.push(Au { ts, first_at: at, last_at: at, bytes: 0, key: false, recovery: false, annexb: Vec::new(), complete: false });
			self.fu.clear();
		}
		{
			let au = self.aus.last_mut().unwrap();
			au.bytes += p.len();
			au.last_at = at;
			au.complete = marker;
		}
		match self.codec {
			pulsar_capture::Codec::H264 => match pl[0] & 0x1f {
				28 if pl.len() > 2 => {
					let (s, e) = (pl[1] & 0x80 != 0, pl[1] & 0x40 != 0);
					if s {
						self.fu.clear();
						self.fu.push((pl[0] & 0xe0) | (pl[1] & 0x1f));
					}
					self.fu.extend_from_slice(&pl[2..]);
					if e {
						let n = std::mem::take(&mut self.fu);
						self.nal(&n);
					}
				}
				24 => {
					let mut i = 1;
					while i + 2 <= pl.len() {
						let n = u16::from_be_bytes([pl[i], pl[i + 1]]) as usize;
						let v = pl[i + 2..(i + 2 + n).min(pl.len())].to_vec();
						self.nal(&v);
						i += 2 + n;
					}
				}
				_ => {
					let v = pl.to_vec();
					self.nal(&v);
				}
			},
			pulsar_capture::Codec::H265 => match (pl[0] >> 1) & 0x3f {
				49 if pl.len() > 3 => {
					let (s, e) = (pl[2] & 0x80 != 0, pl[2] & 0x40 != 0);
					if s {
						self.fu.clear();
						self.fu.push((pl[0] & 0x81) | ((pl[2] & 0x3f) << 1));
						self.fu.push(pl[1]);
					}
					self.fu.extend_from_slice(&pl[3..]);
					if e {
						let n = std::mem::take(&mut self.fu);
						self.nal(&n);
					}
				}
				48 => {
					let mut i = 2;
					while i + 2 <= pl.len() {
						let n = u16::from_be_bytes([pl[i], pl[i + 1]]) as usize;
						let v = pl[i + 2..(i + 2 + n).min(pl.len())].to_vec();
						self.nal(&v);
						i += 2 + n;
					}
				}
				_ => {
					let v = pl.to_vec();
					self.nal(&v);
				}
			},
			pulsar_capture::Codec::Av1 => {}
		}
	}

	fn report(&self, fps: u32, marks: &[(std::time::Duration, Option<u32>, String)]) {
		let aus: Vec<&Au> = self.aus.iter().filter(|a| a.complete).collect();
		if aus.len() < 3 {
			println!("receiver: {} access units ({} packets) — too few", aus.len(), self.packets);
			return;
		}
		let nominal = 1_000_000.0 / fps as f64;
		// Skip the first second (start-up keyframes).
		let steady: Vec<&&Au> = aus.iter().filter(|a| a.last_at.as_secs_f64() > aus[0].last_at.as_secs_f64() + 1.0).collect();
		let mut gaps: Vec<f64> = steady.windows(2).map(|w| (w[1].last_at - w[0].last_at).as_micros() as f64).collect();
		let n = gaps.len() as f64;
		let mean = gaps.iter().sum::<f64>() / n;
		let sd = (gaps.iter().map(|g| (g - mean).powi(2)).sum::<f64>() / n).sqrt();
		let clumped = gaps.iter().filter(|g| **g < nominal * 0.25).count();
		let late = gaps.iter().filter(|g| **g > nominal * 1.75).count();
		gaps.sort_by(|a, b| a.partial_cmp(b).unwrap());
		let p = |q: f64| gaps[((gaps.len() - 1) as f64 * q).round() as usize] / 1000.0;
		// Send-time jitter vs the RTP clock (what the client's delay estimator sees).
		let off: Vec<f64> = steady
			.iter()
			.map(|a| a.last_at.as_micros() as f64 / 1000.0 - a.ts.wrapping_sub(steady[0].ts) as f64 / 90.0)
			.collect();
		let omin = off.iter().cloned().fold(f64::MAX, f64::min);
		let mut offs: Vec<f64> = off.iter().map(|o| o - omin).collect();
		offs.sort_by(|a, b| a.partial_cmp(b).unwrap());
		let op = |q: f64| offs[((offs.len() - 1) as f64 * q).round() as usize];
		println!(
			"receiver: {} AUs ({} packets), steady {} intervals @ nominal {:.2} ms: p1 {:.2} p50 {:.2} p90 {:.2} p99 {:.2} max {:.2} ms, sd {:.2} ms, clumped(<25%) {:.1} %, late(>175%) {:.1} %",
			aus.len(),
			self.packets,
			gaps.len(),
			nominal / 1000.0,
			p(0.01),
			p(0.5),
			p(0.9),
			p(0.99),
			p(1.0),
			sd / 1000.0,
			clumped as f64 * 100.0 / n,
			late as f64 * 100.0 / n
		);
		println!(
			"receiver: arrival vs RTP clock (queueing seen by the client's delay estimator): p50 {:.2} p90 {:.2} p99 {:.2} max {:.2} ms; AU spread first->last packet p90 {:.2} ms",
			op(0.5),
			op(0.9),
			op(0.99),
			op(1.0),
			{
				let mut s: Vec<f64> = steady.iter().map(|a| (a.last_at - a.first_at).as_micros() as f64 / 1000.0).collect();
				s.sort_by(|a, b| a.partial_cmp(b).unwrap());
				s[((s.len() - 1) as f64 * 0.9).round() as usize]
			}
		);
		// Bitrate per 500 ms.
		let end = aus.last().unwrap().last_at.as_secs_f64();
		let mut line = String::from("kbps/500ms:");
		let mut t = 0.0;
		while t < end {
			let b: usize = aus.iter().filter(|a| (t..t + 0.5).contains(&a.last_at.as_secs_f64())).map(|a| a.bytes).sum();
			line.push_str(&format!(" {:.1}s={}", t, b * 8 * 2 / 1000));
			t += 0.5;
		}
		println!("{line}");
		let keys: Vec<String> = aus
			.iter()
			.filter(|a| a.key)
			.map(|a| format!("{:.3}s({}B)", a.last_at.as_secs_f64(), a.bytes))
			.collect();
		println!("keyframes: {}", keys.join(" "));
		let recs: Vec<String> = aus.iter().filter(|a| a.recovery).map(|a| format!("{:.3}s ts={}", a.last_at.as_secs_f64(), a.ts)).collect();
		if !recs.is_empty() {
			println!("recovery-tagged frames: {}", recs.join(", "));
		}
		let mut sizes: Vec<usize> = steady.iter().filter(|a| !a.key).map(|a| a.bytes).collect();
		sizes.sort_unstable();
		if !sizes.is_empty() {
			println!(
				"P-frame bytes p50 {} p90 {} max {}",
				sizes[sizes.len() / 2],
				sizes[(sizes.len() - 1) * 9 / 10],
				sizes[sizes.len() - 1]
			);
		}
		for (at, ts, what) in marks {
			// Where the event's frame is in the received stream, and the first keyframe /
			// recovery frame from it on (in frames, and ms of arrival after the event's frame).
			let start = match ts {
				Some(ts) => aus.iter().position(|a| a.ts == *ts),
				None => aus.iter().position(|a| a.last_at >= *at),
			};
			let Some(i0) = start else {
				println!("event {what}: frame not received");
				continue;
			};
			let t_ev = if ts.is_some() { aus[i0].last_at } else { *at };
			let key = aus[i0..].iter().position(|a| a.key);
			let rec = aus[i0..].iter().position(|a| a.recovery);
			let fmt = |k: Option<usize>, name: &str| {
				k.map(|k| format!(", {name} +{k} frames ({:.1} ms)", (aus[i0 + k].last_at.as_secs_f64() - t_ev.as_secs_f64()) * 1000.0))
					.unwrap_or_default()
			};
			println!("event {what} at {:.3}s{}{}", t_ev.as_secs_f64(), fmt(key, "next keyframe"), fmt(rec, "next recovery frame"));
		}
	}

	/// The Annex-B stream; for every lost frame in `drop_from`, the frames from it up to (not
	/// including) the next recovery-tagged one are left out, as the client would.
	fn write(&self, path: &str, drop_from: &[u32]) {
		let mut v = Vec::new();
		let mut dropping = false;
		for a in self.aus.iter().filter(|a| a.complete) {
			if drop_from.contains(&a.ts) {
				dropping = true;
			}
			if dropping && (a.recovery || a.key) {
				dropping = false;
			}
			if !dropping {
				v.extend_from_slice(&a.annexb);
			}
		}
		std::fs::write(path, v).expect("write out");
	}
}

#[cfg(not(windows))]
fn main() {
	eprintln!("mfbench: Windows only");
}
