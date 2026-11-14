//! macOS virtual key codes (`kVK_*`, the `kCGKeyboardEventKeycode` of a key event) to Linux
//! evdev codes, the key space `InputEvent::Key` carries on the wire (the same space the
//! Windows hook and the Linux evdev grab send). Positional, like those: the host types the
//! key at the same place on ITS layout.

/// `kVK_Function` — no evdev / host equivalent; it only changes what the F-row sends.
pub const KC_FN: u16 = 0x3F;
/// `kVK_CapsLock` — a toggle: macOS reports one `FlagsChanged` per toggle, with no separate
/// release, so it is forwarded as a press + release.
pub const KC_CAPSLOCK: u16 = 0x39;

/// `KBGetLayoutType` answer for an ISO keyboard (`kKeyboardISO`, the four-char code 'ISO ').
pub const KB_LAYOUT_ISO: u32 = u32::from_be_bytes(*b"ISO ");

/// On ISO keyboards macOS reports the key left of 1 as `kVK_ISO_Section` (0x0A) and the key
/// right of Left Shift as `kVK_ANSI_Grave` (0x32) — swapped against their positions. Chromium
/// and SDL undo it the same way (`KBGetLayoutType(LMGetKbdType()) == kKeyboardISO`), so the
/// host gets the key at the right place. Other layouts are passed through.
pub fn layout_fix(kc: u16, iso: bool) -> u16 {
	match (iso, kc) {
		(true, 0x0A) => 0x32,
		(true, 0x32) => 0x0A,
		_ => kc,
	}
}

/// Evdev code of a macOS virtual key code (after [`layout_fix`]); `None` for keys with no
/// host equivalent (they stay local: Fn, the volume keys).
pub fn to_evdev(kc: u16) -> Option<u32> {
	Some(match kc {
		0x00 => 30,  // A
		0x01 => 31,  // S
		0x02 => 32,  // D
		0x03 => 33,  // F
		0x04 => 35,  // H
		0x05 => 34,  // G
		0x06 => 44,  // Z
		0x07 => 45,  // X
		0x08 => 46,  // C
		0x09 => 47,  // V
		0x0A => 86,  // ISO § (IntlBackslash, KEY_102ND)
		0x0B => 48,  // B
		0x0C => 16,  // Q
		0x0D => 17,  // W
		0x0E => 18,  // E
		0x0F => 19,  // R
		0x10 => 21,  // Y
		0x11 => 20,  // T
		0x12 => 2,   // 1
		0x13 => 3,   // 2
		0x14 => 4,   // 3
		0x15 => 5,   // 4
		0x16 => 7,   // 6
		0x17 => 6,   // 5
		0x18 => 13,  // =
		0x19 => 10,  // 9
		0x1A => 8,   // 7
		0x1B => 12,  // -
		0x1C => 9,   // 8
		0x1D => 11,  // 0
		0x1E => 27,  // ]
		0x1F => 24,  // O
		0x20 => 22,  // U
		0x21 => 26,  // [
		0x22 => 23,  // I
		0x23 => 25,  // P
		0x24 => 28,  // Return
		0x25 => 38,  // L
		0x26 => 36,  // J
		0x27 => 40,  // '
		0x28 => 37,  // K
		0x29 => 39,  // ;
		0x2A => 43,  // backslash
		0x2B => 51,  // ,
		0x2C => 53,  // /
		0x2D => 49,  // N
		0x2E => 50,  // M
		0x2F => 52,  // .
		0x30 => 15,  // Tab
		0x31 => 57,  // Space
		0x32 => 41,  // `
		0x33 => 14,  // Delete (Backspace)
		0x35 => 1,   // Escape
		0x36 => 126, // Right Command → Right Meta
		0x37 => 125, // Command → Left Meta
		0x38 => 42,  // Shift
		0x39 => 58,  // Caps Lock
		0x3A => 56,  // Option → Left Alt
		0x3B => 29,  // Control
		0x3C => 54,  // Right Shift
		0x3D => 100, // Right Option → Right Alt (AltGr on a Windows/Linux host)
		0x3E => 97,  // Right Control
		0x40 => 187, // F17
		0x41 => 83,  // Keypad .
		0x43 => 55,  // Keypad *
		0x45 => 78,  // Keypad +
		0x47 => 69,  // Keypad Clear (sits where Num Lock is)
		0x4B => 98,  // Keypad /
		0x4C => 96,  // Keypad Enter
		0x4E => 74,  // Keypad -
		0x4F => 188, // F18
		0x50 => 189, // F19
		0x51 => 117, // Keypad =
		0x52 => 82,  // Keypad 0
		0x53 => 79,  // Keypad 1
		0x54 => 80,  // Keypad 2
		0x55 => 81,  // Keypad 3
		0x56 => 75,  // Keypad 4
		0x57 => 76,  // Keypad 5
		0x58 => 77,  // Keypad 6
		0x59 => 71,  // Keypad 7
		0x5A => 190, // F20
		0x5B => 72,  // Keypad 8
		0x5C => 73,  // Keypad 9
		0x5D => 124, // JIS Yen
		0x5E => 89,  // JIS Underscore (IntlRo)
		0x5F => 95,  // JIS Keypad ,
		0x60 => 63,  // F5
		0x61 => 64,  // F6
		0x62 => 65,  // F7
		0x63 => 61,  // F3
		0x64 => 66,  // F8
		0x65 => 67,  // F9
		0x66 => 123, // JIS Eisu (Lang2)
		0x67 => 87,  // F11
		0x68 => 122, // JIS Kana (Lang1)
		0x69 => 183, // F13
		0x6A => 186, // F16
		0x6B => 184, // F14
		0x6D => 68,  // F10
		0x6E => 127, // Context menu (PC keyboards)
		0x6F => 88,  // F12
		0x71 => 185, // F15
		0x72 => 110, // Help (Insert on PC keyboards)
		0x73 => 102, // Home
		0x74 => 104, // Page Up
		0x75 => 111, // Forward Delete
		0x76 => 62,  // F4
		0x77 => 107, // End
		0x78 => 60,  // F2
		0x79 => 109, // Page Down
		0x7A => 59,  // F1
		0x7B => 105, // Left
		0x7C => 106, // Right
		0x7D => 108, // Down
		0x7E => 103, // Up
		_ => return None,
	})
}

// Device-dependent modifier bits of `CGEventFlags` (IOLLEvent.h `NX_DEVICE*KEYMASK`): they
// tell the left and right key of a pair apart.
const DEV_LCTL: u64 = 0x0000_0001;
const DEV_LSHIFT: u64 = 0x0000_0002;
const DEV_RSHIFT: u64 = 0x0000_0004;
const DEV_LCMD: u64 = 0x0000_0008;
const DEV_RCMD: u64 = 0x0000_0010;
const DEV_LALT: u64 = 0x0000_0020;
const DEV_RALT: u64 = 0x0000_0040;
const DEV_RCTL: u64 = 0x0000_2000;
// Device-independent bits (`kCGEventFlagMask*`): the pair as a whole.
const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CONTROL: u64 = 0x0004_0000;
const FLAG_ALT: u64 = 0x0008_0000;
const FLAG_CMD: u64 = 0x0010_0000;

/// Whether the modifier key `kc` is down after a `FlagsChanged` event with these flags
/// (`None` = not a left/right modifier key). The device-dependent bit decides; an event
/// without any device bits for that pair (some synthesized events) falls back to the
/// pair's device-independent bit.
pub fn modifier_down(kc: u16, flags: u64) -> Option<bool> {
	let (own, pair, any) = match kc {
		0x3B => (DEV_LCTL, DEV_LCTL | DEV_RCTL, FLAG_CONTROL),
		0x3E => (DEV_RCTL, DEV_LCTL | DEV_RCTL, FLAG_CONTROL),
		0x38 => (DEV_LSHIFT, DEV_LSHIFT | DEV_RSHIFT, FLAG_SHIFT),
		0x3C => (DEV_RSHIFT, DEV_LSHIFT | DEV_RSHIFT, FLAG_SHIFT),
		0x3A => (DEV_LALT, DEV_LALT | DEV_RALT, FLAG_ALT),
		0x3D => (DEV_RALT, DEV_LALT | DEV_RALT, FLAG_ALT),
		0x37 => (DEV_LCMD, DEV_LCMD | DEV_RCMD, FLAG_CMD),
		0x36 => (DEV_RCMD, DEV_LCMD | DEV_RCMD, FLAG_CMD),
		_ => return None,
	};
	if flags & pair != 0 {
		Some(flags & own != 0)
	} else {
		Some(flags & any != 0)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn letters_and_digits_are_positional() {
		assert_eq!(to_evdev(0x00), Some(30)); // A
		assert_eq!(to_evdev(0x0C), Some(16)); // Q
		assert_eq!(to_evdev(0x2E), Some(50)); // M
		assert_eq!(to_evdev(0x06), Some(44)); // Z
		assert_eq!(to_evdev(0x12), Some(2)); // 1
		assert_eq!(to_evdev(0x1D), Some(11)); // 0
		assert_eq!(to_evdev(0x67), Some(87)); // F11
	}

	#[test]
	fn modifiers_map_left_and_right() {
		assert_eq!(to_evdev(0x3B), Some(29));
		assert_eq!(to_evdev(0x3E), Some(97));
		assert_eq!(to_evdev(0x37), Some(125));
		assert_eq!(to_evdev(0x36), Some(126));
		assert_eq!(to_evdev(0x3A), Some(56));
		assert_eq!(to_evdev(0x3D), Some(100));
		assert_eq!(to_evdev(KC_FN), None);
	}

	#[test]
	fn no_two_keys_share_a_code() {
		let mut seen = std::collections::HashMap::new();
		for kc in 0u16..0x80 {
			if let Some(code) = to_evdev(kc) {
				if let Some(prev) = seen.insert(code, kc) {
					panic!("evdev {code} mapped from both {prev:#x} and {kc:#x}");
				}
			}
		}
	}

	#[test]
	fn iso_keyboards_swap_section_and_grave() {
		// ISO: the key left of 1 (reported 0x0A) is KEY_GRAVE on the host, the key right of
		// Left Shift (reported 0x32) is KEY_102ND.
		assert_eq!(to_evdev(layout_fix(0x0A, true)), Some(41));
		assert_eq!(to_evdev(layout_fix(0x32, true)), Some(86));
		// ANSI / JIS: unchanged.
		assert_eq!(layout_fix(0x0A, false), 0x0A);
		assert_eq!(layout_fix(0x32, false), 0x32);
		// Every other key is untouched on ISO too.
		assert_eq!(layout_fix(0x00, true), 0x00);
		assert_eq!(KB_LAYOUT_ISO, 0x4953_4F20);
	}

	#[test]
	fn volume_keys_stay_local() {
		// kVK_VolumeUp / kVK_VolumeDown / kVK_Mute: no host injector acts on them, so they
		// keep controlling the Mac's own volume while engaged.
		for kc in 0x48..=0x4A {
			assert_eq!(to_evdev(kc), None);
		}
	}

	#[test]
	fn modifier_state_from_device_bits() {
		// Left Shift down (device bit + generic bit).
		assert_eq!(modifier_down(0x38, DEV_LSHIFT | FLAG_SHIFT), Some(true));
		// Right Shift released while Left Shift is still held: generic bit still set, the
		// right key's own device bit is not.
		assert_eq!(modifier_down(0x3C, DEV_LSHIFT | FLAG_SHIFT), Some(false));
		assert_eq!(modifier_down(0x3E, DEV_RCTL | FLAG_CONTROL), Some(true));
		assert_eq!(modifier_down(0x3B, 0), Some(false));
		assert_eq!(modifier_down(0x37, DEV_LCMD | FLAG_CMD), Some(true));
		assert_eq!(modifier_down(0x36, DEV_LCMD | FLAG_CMD), Some(false));
		assert_eq!(modifier_down(0x3D, DEV_RALT | FLAG_ALT), Some(true));
		// Synthesized event without device bits: the generic bit decides.
		assert_eq!(modifier_down(0x3A, FLAG_ALT), Some(true));
		assert_eq!(modifier_down(0x00, FLAG_ALT), None);
	}
}
