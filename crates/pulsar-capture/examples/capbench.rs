//! Capture-path benchmark: streams one monitor through the native capture+NVENC pipeline to a
//! local UDP sink and prints the live `CaptureStats` once a second.
//!
//! ```text
//! capbench [output_idx=0] [fps=120] [seconds=10] [h264|h265|av1] [--no-anim]
//! ```
//! `PULSAR_CAPTURE=dxgi|wgc` pins the source, `PULSAR_EMIT_ON_ARRIVAL=0` restores the old
//! slot-deadline emission. Unless `--no-anim`, a small window on the monitor repaints every
//! ~2 ms so there is fresh content to measure (a static desktop has no present→encoded latency).
//! Run it inside the desktop session (DXGI duplication fails from a service / SSH session).

#[cfg(windows)]
fn main() {
	use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
	use std::sync::Arc;
	use std::time::Duration;

	let args: Vec<String> = std::env::args().collect();
	let num = |i: usize, d: u64| args.get(i).and_then(|s| s.parse::<u64>().ok()).unwrap_or(d);
	let output_idx = num(1, 0) as u32;
	let fps = num(2, 120) as u32;
	let secs = num(3, 10);
	let codec = match args.get(4).map(String::as_str) {
		Some("h264") => pulsar_capture::Codec::H264,
		Some("av1") => pulsar_capture::Codec::Av1,
		_ => pulsar_capture::Codec::H265,
	};
	let anim = !args.iter().any(|a| a == "--no-anim");
	// A second capture of the same monitor from this process (the two-sessions case).
	let twice = args.iter().any(|a| a == "--twice");

	let rx = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind sink");
	let port = rx.local_addr().unwrap().port();
	let bytes = Arc::new(AtomicU64::new(0));
	{
		let bytes = bytes.clone();
		std::thread::spawn(move || {
			let mut buf = vec![0u8; 65536];
			while let Ok(n) = rx.recv(&mut buf) {
				bytes.fetch_add(n as u64, Ordering::Relaxed);
			}
		});
	}
	let stop_anim = Arc::new(AtomicBool::new(false));
	let anim_thread = anim.then(|| {
		let stop = stop_anim.clone();
		std::thread::spawn(move || unsafe { animate(output_idx, &stop) })
	});
	std::thread::sleep(Duration::from_millis(300));

	let cfg = pulsar_capture::CaptureConfig {
		width: 0,
		height: 0,
		fps,
		bitrate_kbps: 20_000,
		dest: format!("rtp://127.0.0.1:{port}"),
		codec,
		output_idx,
		low_latency: true,
		intra_refresh: false,
		rfi: false,
		draw_mouse: true,
		window_hwnd: None,
	};
	let h = pulsar_capture::start_capture_encode(cfg.clone()).expect("start_capture_encode");
	let h2 = twice.then(|| {
		std::thread::sleep(Duration::from_secs(1));
		pulsar_capture::start_capture_encode(cfg).expect("second start_capture_encode")
	});

	let mut last_bytes = 0u64;
	let (mut p50s, mut p90s, mut ticks) = (Vec::new(), Vec::new(), Vec::new());
	for t in 1..=secs {
		std::thread::sleep(Duration::from_secs(1));
		let s = h.stats();
		let b = bytes.load(Ordering::Relaxed);
		println!(
			"t={t:>3}s src={:<4} lat p50 {:>5.2} p90 {:>5.2} ms | tick p50 {:>5.2} p90 {:>5.2} ms | fresh {:>3}/s | {:>5.1} Mbit/s",
			s.source,
			s.latency_p50_us as f32 / 1000.0,
			s.latency_p90_us as f32 / 1000.0,
			s.submit_p50_us as f32 / 1000.0,
			s.submit_p90_us as f32 / 1000.0,
			s.fresh_per_s,
			(b - last_bytes) as f32 * 8.0 / 1e6,
		);
		last_bytes = b;
		if let Some(h2) = h2.as_ref() {
			let s2 = h2.stats();
			println!(
				"        2nd src={:<4} tick p50 {:>5.2} ms | fresh {:>3}/s",
				s2.source,
				s2.submit_p50_us as f32 / 1000.0,
				s2.fresh_per_s
			);
		}
		if t >= 2 && s.latency_p50_us > 0 {
			p50s.push(s.latency_p50_us);
			p90s.push(s.latency_p90_us);
			ticks.push(s.submit_p50_us);
		}
	}
	let med = |v: &mut Vec<u32>| {
		v.sort_unstable();
		v.get(v.len() / 2).copied().unwrap_or(0) as f32 / 1000.0
	};
	println!(
		"SUMMARY lat p50 {:.2} p90 {:.2} ms | tick {:.2} ms",
		med(&mut p50s),
		med(&mut p90s),
		med(&mut ticks)
	);
	if let Some(h2) = h2 {
		h2.stop();
	}
	h.stop();
	stop_anim.store(true, Ordering::Relaxed);
	if let Some(t) = anim_thread {
		let _ = t.join();
	}
}

/// A 480x480 top-most, non-activating popup centred on the monitor that repaints every ~2 ms.
#[cfg(windows)]
unsafe fn animate(output_idx: u32, stop: &std::sync::atomic::AtomicBool) {
	use windows::core::w;
	use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
	use windows::Win32::Graphics::Gdi::{
		CreateSolidBrush, DeleteObject, FillRect, GdiFlush, GetDC, ReleaseDC, HGDIOBJ,
	};
	use windows::Win32::UI::WindowsAndMessaging::{
		CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, PeekMessageW, RegisterClassW,
		TranslateMessage, MSG, PM_REMOVE, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
		WS_POPUP, WS_VISIBLE,
	};

	unsafe extern "system" fn wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
		DefWindowProcW(h, m, w, l)
	}

	// Centred on the monitor: the measured pixel is the frame's centre (rotation-proof).
	let (x, y) = pulsar_capture::display_rect(output_idx)
		.map(|r| (r.mon_left + r.mon_width / 2 - 240, r.mon_top + r.mon_height / 2 - 240))
		.unwrap_or((60, 60));
	let class = w!("PulsarCapBench");
	let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), lpszClassName: class, ..Default::default() };
	RegisterClassW(&wc);
	let Ok(hwnd) = CreateWindowExW(
		WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
		class,
		w!("capbench"),
		WS_POPUP | WS_VISIBLE,
		x,
		y,
		480,
		480,
		None,
		None,
		None,
		None,
	) else {
		eprintln!("capbench: no animation window");
		return;
	};
	let rc = RECT { left: 0, top: 0, right: 480, bottom: 480 };
	let mut n: u32 = 0;
	while !stop.load(std::sync::atomic::Ordering::Relaxed) {
		let mut msg = MSG::default();
		while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
			let _ = TranslateMessage(&msg);
			DispatchMessageW(&msg);
		}
		let dc = GetDC(Some(hwnd));
		// The paint counter as the colour (R = n low byte, G = high byte, B = 0x80 marker) so the
		// capture thread can tell which paint a frame shows (PULSAR_BENCH_PIXEL=1, see bench.rs).
		let c = n & 0xFFFF;
		let brush = CreateSolidBrush(COLORREF((c & 0xFF) | (c & 0xFF00) | (0x80 << 16)));
		FillRect(dc, &rc, brush);
		let _ = GdiFlush();
		pulsar_capture::bench::mark_paint(c, pulsar_capture::bench::now_ns());
		let _ = DeleteObject(HGDIOBJ(brush.0));
		ReleaseDC(Some(hwnd), dc);
		n = n.wrapping_add(1);
		std::thread::sleep(std::time::Duration::from_millis(2));
	}
	let _ = DestroyWindow(hwnd);
}

#[cfg(not(windows))]
fn main() {
	eprintln!("capbench is Windows-only");
}
