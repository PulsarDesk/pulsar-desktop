//! Linux client: touchpad pinch → host (`InputEvent::Pinch`, or Ctrl + wheel for hosts that
//! do not inject gestures — see `crate::pinch`).
//!
//! The evdev capture grabs keyboards and mice only; touchpads stay with the compositor. This
//! reader opens every touchpad READ-ONLY (no EVIOCGRAB — the compositor keeps working), follows
//! its multitouch contacts with `pinch::TouchTracker`, and forwards a recognised two-finger
//! pinch while the session is controlled (`input_active`). One reader per capture generation;
//! it exits when `enable()`/`disable()` bump the generation. Untested on Linux hardware.
//!
//! The same reader recognises 3 / 4-finger swipes and taps (`crate::gesture`: MT slots +
//! BTN_TOOL_TRIPLETAP / QUADTAP for pads that track fewer slots) and forwards them as
//! `InputEvent::Gesture` while controlled. Not grabbing the touchpad means the compositor
//! (GNOME / KDE) ALSO performs its own gesture locally — there is no way to keep a gesture
//! from the compositor without taking the whole touchpad away from it.

use super::input_active;
use super::linux::GEN;
use crate::gesture::{MtSlots, Recognizer};
use crate::pinch::{Gesture, PinchForwarder, TouchTracker};
use evdev::{AbsoluteAxisCode, EventSummary, KeyCode, PropType, SynchronizationCode};
use pulsar_core::service::InputEvent;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::Ordering;
use tokio::sync::mpsc::Sender;

/// A touchpad as udev's ID_INPUT_TOUCHPAD sees it: multitouch positions + finger tool, and
/// NOT a direct-touch device (touchscreens are INPUT_PROP_DIRECT).
fn is_touchpad(d: &evdev::Device) -> bool {
	let mt = d.supported_absolute_axes().is_some_and(|a| {
		a.contains(AbsoluteAxisCode::ABS_MT_SLOT) && a.contains(AbsoluteAxisCode::ABS_MT_POSITION_X)
	});
	let finger = d.supported_keys().is_some_and(|k| k.contains(KeyCode::BTN_TOOL_FINGER));
	mt && finger && !d.properties().contains(PropType::DIRECT)
}

/// ABS_MT_POSITION_X / _Y resolution (units per mm; 0 when the driver reports none) and
/// (min, max) ranges.
fn axes(d: &evdev::Device) -> ((i32, i32), (i32, i32), (i32, i32)) {
	let (mut res, mut x, mut y) = ((0, 0), (0, 0), (0, 0));
	if let Ok(abs) = d.get_absinfo() {
		for (code, info) in abs {
			if code == AbsoluteAxisCode::ABS_MT_POSITION_X {
				res.0 = info.resolution();
				x = (info.minimum(), info.maximum());
			} else if code == AbsoluteAxisCode::ABS_MT_POSITION_Y {
				res.1 = info.resolution();
				y = (info.minimum(), info.maximum());
			}
		}
	}
	(res, x, y)
}

/// BTN_TOOL_* → finger count.
fn tool_fingers(code: KeyCode) -> Option<usize> {
	Some(match code {
		KeyCode::BTN_TOOL_FINGER => 1,
		KeyCode::BTN_TOOL_DOUBLETAP => 2,
		KeyCode::BTN_TOOL_TRIPLETAP => 3,
		KeyCode::BTN_TOOL_QUADTAP => 4,
		KeyCode::BTN_TOOL_QUINTTAP => 5,
		_ => return None,
	})
}

struct Pad {
	dev: evdev::Device,
	node: std::path::PathBuf,
	track: TouchTracker,
	/// 3 / 4-finger swipes and taps (`crate::gesture`).
	mt: MtSlots,
	rec: Recognizer,
}

pub(super) fn spawn(gen: u64, tx: Sender<InputEvent>) {
	let _ = std::thread::Builder::new()
		.name("pulsar-pinch-rd".into())
		.spawn(move || run(gen, tx));
}

fn gesture_clock() -> u64 {
	static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
	T0.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

fn send(tx: &Sender<InputEvent>, evs: Vec<InputEvent>) {
	for ev in evs {
		let _ = tx.try_send(ev);
	}
}

fn run(gen: u64, tx: Sender<InputEvent>) {
	let mut pads: Vec<Pad> = Vec::new();
	let mut fwd = PinchForwarder::new();
	let mut rescan_at = std::time::Instant::now();
	let mut logged = false;
	while GEN.load(Ordering::SeqCst) == gen {
		// Hotplug: (re)scan for touchpads every few seconds.
		if std::time::Instant::now() >= rescan_at {
			rescan_at = std::time::Instant::now() + std::time::Duration::from_secs(3);
			for (node, dev) in evdev::enumerate() {
				if pads.iter().any(|p| p.node == node) || !is_touchpad(&dev) {
					continue;
				}
				let ((rx, ry), x, y) = axes(&dev);
				tracing::info!(name = dev.name().unwrap_or("?"), rx, ry, ?x, ?y, "pinch/gesture: reading touchpad");
				pads.push(Pad {
					dev,
					node,
					track: TouchTracker::new(rx, ry),
					mt: MtSlots::new(x, y),
					rec: Recognizer::new(),
				});
			}
			if pads.is_empty() && !logged {
				logged = true;
				tracing::info!("pinch: no readable touchpad (none attached, or no access to /dev/input)");
			}
		}
		if pads.is_empty() {
			std::thread::sleep(std::time::Duration::from_millis(500));
			continue;
		}
		// Control left the session mid-pinch: close it on the host.
		if fwd.active() && !input_active() {
			send(&tx, fwd.end());
		}
		let mut pfds: Vec<libc::pollfd> = pads
			.iter()
			.map(|p| libc::pollfd { fd: p.dev.as_raw_fd(), events: libc::POLLIN, revents: 0 })
			.collect();
		let n = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, 200) };
		if n <= 0 {
			continue;
		}
		let mut gone = Vec::new();
		for (i, pfd) in pfds.iter().enumerate() {
			if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
				gone.push(i);
				continue;
			}
			if pfd.revents & libc::POLLIN == 0 {
				continue;
			}
			let pad = &mut pads[i];
			let events: Vec<evdev::InputEvent> = match pad.dev.fetch_events() {
				Ok(e) => e.collect(),
				Err(_) => {
					gone.push(i);
					continue;
				}
			};
			for ev in events {
				let g = match ev.destructure() {
					EventSummary::AbsoluteAxis(_, code, v) => {
						match code {
							AbsoluteAxisCode::ABS_MT_SLOT => {
								pad.track.set_slot(v);
								pad.mt.set_slot(v);
							}
							AbsoluteAxisCode::ABS_MT_TRACKING_ID => {
								pad.track.set_tracking_id(v);
								pad.mt.set_tracking_id(v);
							}
							AbsoluteAxisCode::ABS_MT_POSITION_X => {
								pad.track.set_x(v);
								pad.mt.set_x(v);
							}
							AbsoluteAxisCode::ABS_MT_POSITION_Y => {
								pad.track.set_y(v);
								pad.mt.set_y(v);
							}
							_ => {}
						}
						None
					}
					EventSummary::Key(_, code, v) => {
						if let Some(n) = tool_fingers(code) {
							pad.mt.set_tool(n, v != 0);
						}
						None
					}
					EventSummary::Synchronization(_, SynchronizationCode::SYN_REPORT, _) => {
						let t = gesture_clock();
						if let Some((fingers, kind)) = pad.rec.frame(t, &pad.mt.contacts()) {
							// Only while the session is controlled; the compositor still performs
							// the gesture locally too (the touchpad is read, not grabbed).
							let forward = input_active() && crate::gesture::forwarding();
							tracing::info!(fingers, ?kind, forward, "touchpad gesture");
							if forward {
								send(&tx, vec![crate::gesture::event(fingers, kind)]);
							}
						}
						pad.track.frame()
					}
					EventSummary::Synchronization(_, SynchronizationCode::SYN_DROPPED, _) => {
						pad.mt.reset();
						pad.rec.reset();
						pad.track.reset()
					}
					_ => None,
				};
				match g {
					// Only a pinch that starts while the session is controlled goes to the host.
					Some(Gesture::Begin) if input_active() => {
						let mut out = fwd.begin(crate::pinch::host_pinch());
						out.extend(fwd.update(pad.track.scale()));
						send(&tx, out);
					}
					Some(Gesture::Update(s)) if fwd.active() => send(&tx, fwd.update(s)),
					Some(Gesture::End) => send(&tx, fwd.end()),
					_ => {}
				}
			}
		}
		for i in gone.into_iter().rev() {
			let pad = pads.remove(i);
			tracing::info!(node = %pad.node.display(), "pinch: touchpad gone");
			send(&tx, fwd.end());
		}
	}
	send(&tx, fwd.end());
}
