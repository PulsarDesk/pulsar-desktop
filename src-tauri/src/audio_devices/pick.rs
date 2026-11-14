//! The device-choice rules for `audio_devices`, std only (no SDL, no Tauri) so they can be
//! unit-tested anywhere.

use std::time::{Duration, Instant};

/// An SDL audio device: (id, name).
pub type Dev = (u32, String);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
	/// What the client sends to the host.
	Mic,
	/// Where the session's audio plays.
	Speaker,
}

/// What an open stream is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opened {
	/// SDL's default device (follows the OS default on its own).
	Default,
	/// One physical device, by SDL id.
	Device(u32),
}

/// How long a failed open waits before the next try.
pub const RETRY: Duration = Duration::from_secs(1);
/// How often an open stream checks its device is still the right one.
pub const CHECK_EVERY: Duration = Duration::from_millis(500);
/// A stream that moved no data this long is treated as dead and reopened.
pub const STALL: Duration = Duration::from_millis(1500);

/// The token for "the system default" in the renderer lines.
pub const DEV_DEFAULT: &str = "*";


/// The device a stream should be on: `Some(id)` = the picked device, present now; `None` =
/// the system default (no pick, or the pick is not connected). An exact name first, then the
/// same name ignoring case / surrounding blanks (some backends vary those across replugs).
pub fn resolve(pref: &str, devs: &[Dev]) -> Option<u32> {
	let pref = pref.trim();
	if pref.is_empty() {
		return None;
	}
	devs.iter()
		.find(|(_, n)| n == pref)
		.or_else(|| devs.iter().find(|(_, n)| n.trim().eq_ignore_ascii_case(pref)))
		.map(|(id, _)| *id)
}

/// Whether a stream open on `opened` must be reopened, given where it should be (`want`, from
/// [`resolve`] or an override such as a DS4 headset jack) and the devices present now.
pub fn needs_reopen(opened: Opened, want: Option<u32>, devs: &[Dev]) -> bool {
	match (opened, want) {
		// Its device went away (unplugged, Bluetooth dropped, driver removed it).
		(Opened::Device(id), _) if !devs.iter().any(|(d, _)| *d == id) => true,
		(Opened::Device(id), Some(w)) => id != w,
		// Pick cleared (or gone): back to the default.
		(Opened::Device(_), None) => true,
		// The pick (re)appeared, or a new one was made.
		(Opened::Default, Some(_)) => true,
		// SDL moves a default stream with the OS default itself.
		(Opened::Default, None) => false,
	}
}

/// A retry gate: once per [`RETRY`] after a failure, logged on the first failure of a streak
/// only (an unplugged-for-an-hour headset must not fill the log).
#[derive(Debug, Default)]
pub struct Retry {
	next: Option<Instant>,
	failures: u32,
}

impl Retry {
	/// An attempt may run now.
	pub fn due(&self, now: Instant) -> bool {
		self.next.is_none_or(|t| now >= t)
	}
	/// The attempt failed. Returns true for the first failure of a streak (log it loudly).
	pub fn failed(&mut self, now: Instant) -> bool {
		self.failures += 1;
		self.next = Some(now + RETRY);
		self.failures == 1
	}
	/// The attempt worked. Returns how many failures came before it.
	pub fn ok(&mut self) -> u32 {
		self.next = None;
		std::mem::take(&mut self.failures)
	}
}


/// A device name as one space-free token: bytes other than `A-Z a-z 0-9 - _ . ~` become
/// `%XX`; the empty name (system default) is [`DEV_DEFAULT`]. Same as pulsar-render's
/// `overlay::dev_encode`.
pub fn encode(name: &str) -> String {
	if name.is_empty() {
		return DEV_DEFAULT.to_string();
	}
	let mut s = String::with_capacity(name.len());
	for b in name.bytes() {
		if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
			s.push(b as char);
		} else {
			s.push_str(&format!("%{b:02X}"));
		}
	}
	s
}

/// Inverse of [`encode`] (`None` for a malformed token).
pub fn decode(tok: &str) -> Option<String> {
	if tok == DEV_DEFAULT {
		return Some(String::new());
	}
	let b = tok.as_bytes();
	let mut out = Vec::with_capacity(b.len());
	let mut i = 0;
	while i < b.len() {
		if b[i] == b'%' {
			let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
			out.push(u8::from_str_radix(hex, 16).ok()?);
			i += 3;
		} else {
			out.push(b[i]);
			i += 1;
		}
	}
	String::from_utf8(out).ok()
}

/// The renderer line for one kind: `micdevs|spkdevs <pick> <dev>…` (names deduplicated).
pub fn line(kind: Kind, pick: &str, devs: &[Dev]) -> String {
	let mut s = String::from(match kind {
		Kind::Mic => "micdevs ",
		Kind::Speaker => "spkdevs ",
	});
	s.push_str(&encode(pick));
	let mut seen: Vec<&str> = Vec::new();
	for (_, n) in devs {
		if n.is_empty() || seen.contains(&n.as_str()) {
			continue;
		}
		seen.push(n);
		s.push(' ');
		s.push_str(&encode(n));
	}
	s
}

#[cfg(test)]
mod tests {
	use super::*;

	fn devs() -> Vec<Dev> {
		vec![(3, "Speakers (Realtek)".into()), (7, "Headset (WH-1000XM4)".into())]
	}

	#[test]
	fn the_pick_resolves_by_name_else_the_default() {
		assert_eq!(resolve("", &devs()), None, "no pick = default");
		assert_eq!(resolve("Headset (WH-1000XM4)", &devs()), Some(7));
		assert_eq!(resolve(" headset (wh-1000xm4) ", &devs()), Some(7), "case / blanks");
		assert_eq!(resolve("USB Mic", &devs()), None, "not connected = default");
		assert_eq!(resolve("Headset (WH-1000XM4)", &[]), None, "nothing at all");
	}

	#[test]
	fn an_unplugged_pick_falls_back_and_comes_back() {
		let all = devs();
		let without: Vec<Dev> = vec![all[0].clone()];
		// On the headset; it is unplugged: reopen (on the default).
		assert!(needs_reopen(Opened::Device(7), resolve("Headset (WH-1000XM4)", &without), &without));
		// On the default while it is away: stay.
		assert!(!needs_reopen(Opened::Default, resolve("Headset (WH-1000XM4)", &without), &without));
		// It is back (a new SDL id): move back to it.
		let back: Vec<Dev> = vec![all[0].clone(), (9, "Headset (WH-1000XM4)".into())];
		let want = resolve("Headset (WH-1000XM4)", &back);
		assert_eq!(want, Some(9));
		assert!(needs_reopen(Opened::Default, want, &back));
		assert!(!needs_reopen(Opened::Device(9), want, &back));
	}

	#[test]
	fn the_default_stream_stays_and_a_changed_pick_moves() {
		// No pick: SDL follows the OS default by itself, even when devices come and go.
		assert!(!needs_reopen(Opened::Default, None, &devs()));
		assert!(!needs_reopen(Opened::Default, None, &[]));
		// A different pick, or the pick cleared.
		assert!(needs_reopen(Opened::Device(3), Some(7), &devs()));
		assert!(needs_reopen(Opened::Device(3), None, &devs()));
		assert!(!needs_reopen(Opened::Device(3), Some(3), &devs()));
	}

	#[test]
	fn retries_wait_and_log_once_per_streak() {
		let t0 = Instant::now();
		let mut r = Retry::default();
		assert!(r.due(t0));
		assert!(r.failed(t0), "first failure is logged");
		assert!(!r.due(t0 + Duration::from_millis(500)));
		assert!(r.due(t0 + RETRY));
		assert!(!r.failed(t0 + RETRY), "later ones are not");
		assert_eq!(r.ok(), 2);
		assert!(r.due(t0));
		assert!(r.failed(t0), "a new streak logs again");
	}

	#[test]
	fn names_travel_as_single_tokens() {
		for n in ["Mikrofon (Realtek(R) Audio)", "Kulaklık ğüşİ", "a%b", "plain-name_1.2~"] {
			let e = encode(n);
			assert!(!e.contains(' ') && !e.is_empty());
			assert_eq!(decode(&e).as_deref(), Some(n));
		}
		assert_eq!(encode(""), DEV_DEFAULT);
		assert_eq!(decode(DEV_DEFAULT).as_deref(), Some(""));
		assert_eq!(decode("x%4"), None);
		// The renderer's encoder is the same (pulsar-render overlay::dev_encode): a fixed case.
		assert_eq!(encode("Mic A (USB)"), "Mic%20A%20%28USB%29");
	}

	#[test]
	fn the_line_lists_the_pick_and_unique_names() {
		let d: Vec<Dev> = vec![(1, "A B".into()), (2, "A B".into()), (3, "C".into()), (4, String::new())];
		assert_eq!(line(Kind::Mic, "", &d), "micdevs * A%20B C");
		assert_eq!(line(Kind::Speaker, "Gone", &[]), "spkdevs Gone");
	}
}
