//! `pulsar --ds5-probe`: drive the first connected DualSense through the passthrough
//! client path (`ds5::hid` + `pulsar_core::input::ds5::pad`) with no session and no game.
//! It reads input, puts resistance on R2, turns the lightbar green, plays haptics pulses
//! (BT only), then resets the pad. A developer check for the maintainer's PC.

use std::time::{Duration, Instant};

use pulsar_core::input::ds5::haptics::{Frame, BT_RATE, FRAME_BYTES, FRAME_SAMPLES};
use pulsar_core::input::ds5::pad::{Pad, PadInput};
use pulsar_core::input::ds5::report::{flag, SetState};
use pulsar_core::input::ds5::wire::Ds5Transport;
use pulsar_core::input::ds5::Ds5Features;

use super::hid::{enumerate, SdlHid};

/// One 10.667 ms haptics frame of a sine at `hz` (amplitude 0..=1 per side).
fn tone_frame(phase: &mut f64, hz: f64, left: f32, right: f32) -> Frame {
	let mut f = [0u8; FRAME_BYTES];
	for i in 0..FRAME_SAMPLES {
		let s = (*phase).sin() as f32;
		*phase += 2.0 * std::f64::consts::PI * hz / BT_RATE as f64;
		f[i * 2] = ((s * left * 127.0) as i8) as u8;
		f[i * 2 + 1] = ((s * right * 127.0) as i8) as u8;
	}
	f
}

fn run_for(
	pad: &mut Pad,
	hid: &mut SdlHid,
	dur: Duration,
	mut each: impl FnMut(&mut Pad, &PadInput),
) {
	let end = Instant::now() + dur;
	let mut buf = [0u8; 600];
	while Instant::now() < end {
		let wait = pad.poll(Instant::now(), hid);
		let ms = wait.as_millis().clamp(0, 5) as i32;
		if let Ok(n) = hid.read_timeout(&mut buf, ms) {
			if n > 0 {
				if let Some(inp) = pad.on_input(&buf[..n]) {
					each(pad, &inp);
				}
			}
		}
	}
}

pub fn run() -> i32 {
	let devs = enumerate();
	println!("found {} DualSense HID interface(s)", devs.len());
	for d in &devs {
		println!(
			"  {:?} pid={:04x} serial={} path={:?}",
			d.transport, d.product_id, d.serial, d.path
		);
	}
	let Some(dev) = devs.first() else {
		println!("FAIL no DualSense");
		return 1;
	};
	let mut hid = match SdlHid::open(dev) {
		Ok(h) => h,
		Err(e) => {
			println!("FAIL open: {e}");
			return 1;
		}
	};
	let (mut pad, feats) = match Pad::open(
		&mut hid,
		dev.transport,
		Ds5Features {
			mic: false,
			..Ds5Features::all()
		},
	) {
		Ok(p) => p,
		Err(e) => {
			println!("FAIL Pad::open: {e}");
			return 1;
		}
	};
	for (id, f) in &feats {
		println!("feature 0x{id:02x}: {} bytes", f.len());
	}

	// 1. Input for 2 s.
	let (mut states, mut mics) = (0u32, 0u32);
	let mut last = None;
	run_for(
		&mut pad,
		&mut hid,
		Duration::from_secs(2),
		|_, inp| match inp {
			PadInput::State(s) => {
				states += 1;
				last = Some(*s);
			}
			PadInput::Mic(_) => mics += 1,
		},
	);
	println!(
		"input: {states} state reports in 2 s ({} Hz), {mics} mic packets",
		states / 2
	);
	if let Some(s) = last {
		println!(
			"  sticks L({},{}) R({},{}) L2={} R2={} battery={:x}",
			s[1], s[2], s[3], s[4], s[5], s[6], s[53]
		);
	}
	if states == 0 {
		println!("FAIL no input reports");
		return 1;
	}

	// 2. R2 resistance (feedback, all zones, strength 8) + green lightbar for 6 s.
	let mut st = SetState::default();
	st.0[0] = flag::B0_RIGHT_TRIGGER | flag::B0_LEFT_TRIGGER;
	st.0[1] = flag::B1_LED_COLOR;
	st.0[10] = 0x21; // feedback
	st.0[11] = 0xFF; // zones 0-7
	st.0[12] = 0x03; // zones 8-9
	let strengths: u32 = (0..10).fold(0, |a, i| a | 7 << (3 * i));
	st.0[13..17].copy_from_slice(&strengths.to_le_bytes());
	st.0[21] = 0x05; // left: off
	st.0[44..47].copy_from_slice(&[0, 255, 0]);
	pad.set_output(st);
	// PULSAR_DS5_PROBE_SECS sets how long the trigger phase lasts (default 6 s).
	let secs: u64 = std::env::var("PULSAR_DS5_PROBE_SECS")
		.ok()
		.and_then(|v| v.parse().ok())
		.unwrap_or(6);
	println!("R2 should resist now and the lightbar be green — press R2 ({secs} s)");
	let mut r2_max = 0u8;
	run_for(&mut pad, &mut hid, Duration::from_secs(secs), |_, inp| {
		if let PadInput::State(s) = inp {
			r2_max = r2_max.max(s[6]);
		}
	});
	println!("  R2 max travel seen: {r2_max}/255");

	// 3. Haptics (BT): left pulses, right pulses, then a sweep.
	if dev.transport == Ds5Transport::Bt {
		println!("haptics: left x3, right x3, sweep");
		let mut phase = 0.0;
		let mut plan: Vec<(f64, f32, f32, u32)> = Vec::new(); // (hz, l, r, frames)
		for _ in 0..3 {
			plan.push((90.0, 1.0, 0.0, 24));
			plan.push((90.0, 0.0, 0.0, 24));
		}
		for _ in 0..3 {
			plan.push((90.0, 0.0, 1.0, 24));
			plan.push((90.0, 0.0, 0.0, 24));
		}
		for k in 0..20 {
			plan.push((40.0 + k as f64 * 10.0, 1.0, 1.0, 9));
		}
		let mut frames: std::collections::VecDeque<Frame> = plan
			.iter()
			.flat_map(|&(hz, l, r, n)| (0..n).map(move |_| (hz, l, r)))
			.map(|(hz, l, r)| tone_frame(&mut phase, hz, l, r))
			.collect();
		let total = frames.len();
		let start = Instant::now();
		let mut buf = [0u8; 600];
		// Feed at the real slot rate so the pad's FIFO neither starves nor overflows.
		let mut fed = 0usize;
		while !frames.is_empty()
			|| start.elapsed() < Duration::from_secs_f64(total as f64 * 0.010_667 + 0.3)
		{
			let due = (start.elapsed().as_secs_f64() / 0.010_667) as usize + 4;
			while fed < due {
				match frames.pop_front() {
					Some(f) => pad.push_slot(Some(f), None),
					None => break,
				}
				fed += 1;
			}
			let wait = pad.poll(Instant::now(), &mut hid);
			let _ = hid.read_timeout(&mut buf, wait.as_millis().clamp(0, 2) as i32);
		}
		println!("  pad stats: {:?}", pad.stats());
	} else {
		println!("haptics skipped: USB pad plays haptics through its audio device");
	}

	// 4. Reset: triggers off, lightbar blue.
	let mut st = SetState::default();
	st.0[0] = flag::B0_RIGHT_TRIGGER | flag::B0_LEFT_TRIGGER;
	st.0[1] = flag::B1_LED_COLOR;
	st.0[10] = 0x05;
	st.0[21] = 0x05;
	st.0[44..47].copy_from_slice(&[0, 0, 255]);
	pad.set_output(st);
	run_for(&mut pad, &mut hid, Duration::from_millis(200), |_, _| {});
	println!("PASS probe done; final stats {:?}", pad.stats());
	0
}
