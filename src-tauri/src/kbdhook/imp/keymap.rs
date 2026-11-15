//! Pure scancode/virtual-key → evdev keycode mapping tables for the Windows
//! capture paths (Interception set-1 scancodes + WH_KEYBOARD_LL virtual keys).

/// Interception delivers AT set-1 scancodes; map them to evdev keycodes (the
/// inverse of the host's table). The base block is identity (evdev keycodes were
/// derived from these scancodes); E0-extended keys need the explicit table.
pub(super) fn scancode_to_evdev(code: u16, e0: bool) -> Option<u32> {
	// Win/Menu are always E0-extended and never collide with base scancodes, so
	// map them regardless of whether the E0 flag was reported (be robust to paths
	// that drop it — otherwise the Win key leaks to the local Start menu).
	match code {
		0x5B => return Some(125), // Left Win
		0x5C => return Some(126), // Right Win
		0x5D => return Some(127), // Context Menu
		_ => {}
	}
	if !e0 {
		// The base block is identity; past it the set-1 codes no longer match evdev
		// (Linux atkbd's translation): F13..F24 and the JIS / Korean / ABNT extras.
		return match code {
			1..=88 => Some(code as u32),
			0x64..=0x6E => Some(183 + (code - 0x64) as u32), // F13..F23
			0x76 => Some(194),                               // F24
			0x70 => Some(93),                                // Katakana/Hiragana
			0x73 => Some(89),                                // JIS Ro (\_)
			0x79 => Some(92),                                // Henkan
			0x7B => Some(94),                                // Muhenkan
			0x7D => Some(124),                               // JIS Yen
			0x7E => Some(121),                               // ABNT keypad comma
			0x59 => Some(117),                               // Keypad =
			0xF1 => Some(123),                               // Hanja
			0xF2 => Some(122),                               // Hangul
			_ => None,
		};
	}
	Some(match code {
		0x37 => 99,  // Print Screen (E0 37)
		0x46 => 119, // Ctrl+Break (E0 46) → Pause
		0x1C => 96,  // Numpad Enter
		0x1D => 97,  // Right Ctrl
		0x35 => 98,  // Numpad /
		0x38 => 100, // Right Alt
		0x47 => 102, // Home
		0x48 => 103, // Arrow Up
		0x49 => 104, // Page Up
		0x4B => 105, // Arrow Left
		0x4D => 106, // Arrow Right
		0x4F => 107, // End
		0x50 => 108, // Arrow Down
		0x51 => 109, // Page Down
		0x52 => 110, // Insert
		0x53 => 111, // Delete
		0x5B => 125, // Left Win
		0x5C => 126, // Right Win
		0x5D => 127, // Context Menu
		_ => return None,
	})
}

/// Windows virtual-key → Linux evdev keycode — the inverse of
/// `pulsar_core::input::evdev_to_vk`. Kept in lock-step with that table.
pub(super) fn vk_to_evdev(vk: u16) -> Option<u32> {
	Some(match vk {
		0x1B => 1,                             // Escape
		0x31..=0x39 => 2 + (vk - 0x31) as u32, // '1'..'9' → Digit1..9
		0x30 => 11,                            // '0'
		0xBD => 12,                            // VK_OEM_MINUS
		0xBB => 13,                            // VK_OEM_PLUS
		0x08 => 14,                            // Backspace
		0x09 => 15,                            // Tab
		0x51 => 16,
		0x57 => 17,
		0x45 => 18,
		0x52 => 19,
		0x54 => 20, // Q W E R T
		0x59 => 21,
		0x55 => 22,
		0x49 => 23,
		0x4F => 24,
		0x50 => 25, // Y U I O P
		0xDB => 26,
		0xDD => 27, // [ ]
		0x0D => 28, // Enter
		0xA2 => 29, // Left Ctrl
		0x41 => 30,
		0x53 => 31,
		0x44 => 32,
		0x46 => 33,
		0x47 => 34,
		0x48 => 35, // A S D F G H
		0x4A => 36,
		0x4B => 37,
		0x4C => 38, // J K L
		0xBA => 39,
		0xDE => 40,
		0xC0 => 41, // ; ' `
		0xA0 => 42, // Left Shift
		0xDC => 43, // backslash
		0x5A => 44,
		0x58 => 45,
		0x43 => 46,
		0x56 => 47,
		0x42 => 48,
		0x4E => 49,
		0x4D => 50, // Z X C V B N M
		0xBC => 51,
		0xBE => 52,
		0xBF => 53,                             // , . /
		0xA1 => 54,                             // Right Shift
		0x6A => 55,                             // Numpad *
		0xA4 => 56,                             // Left Alt (VK_LMENU)
		0x20 => 57,                             // Space
		0x14 => 58,                             // Caps Lock
		0x70..=0x79 => 59 + (vk - 0x70) as u32, // F1..F10
		0x90 => 69,                             // Num Lock
		0x91 => 70,                             // Scroll Lock
		0x67 => 71,
		0x68 => 72,
		0x69 => 73, // Numpad 7 8 9
		0x6D => 74, // Numpad -
		0x64 => 75,
		0x65 => 76,
		0x66 => 77, // Numpad 4 5 6
		0x6B => 78, // Numpad +
		0x61 => 79,
		0x62 => 80,
		0x63 => 81,  // Numpad 1 2 3
		0x60 => 82,  // Numpad 0
		0x6E => 83,  // Numpad .
		0x7A => 87,  // F11
		0x7B => 88,  // F12
		0x6F => 98,  // Numpad / (VK_DIVIDE)
		0xA3 => 97,  // Right Ctrl
		0xA5 => 100, // Right Alt (VK_RMENU)
		0x24 => 102, // Home
		0x26 => 103, // Arrow Up
		0x21 => 104, // Page Up
		0x25 => 105, // Arrow Left
		0x27 => 106, // Arrow Right
		0x23 => 107, // End
		0x28 => 108, // Arrow Down
		0x22 => 109, // Page Down
		0x2D => 110, // Insert
		0x2E => 111, // Delete
		0x5B => 125, // Left Win
		0x5C => 126, // Right Win
		0x5D => 127, // Context Menu (VK_APPS)
		// Bare VK_SHIFT/CONTROL/MENU: fold to the left variant so they still
		// reach the host (the host treats L/R identically for these).
		0x10 => 42,
		0x11 => 29,
		0x12 => 56,
		// The rest of `pulsar_core::input::keytables::CLIENT_KEYS`: ISO / JIS / Korean / ABNT
		// extras, Print Screen, Pause, F13..F24.
		0xE2 => 86,                               // VK_OEM_102 (ISO <>|, JIS \_ on JIS layouts)
		0xC1 => 89,                               // VK_ABNT_C1 (JIS Ro / ABNT /?)
		0x1C => 92,                               // VK_CONVERT (Henkan)
		0x1D => 94,                               // VK_NONCONVERT (Muhenkan)
		0xF2 => 93,                               // VK_DBE_HIRAGANA / VK_OEM_COPY (Katakana/Hiragana)
		0x6C => 95,                               // VK_SEPARATOR (JIS keypad comma)
		0x2C => 99,                               // VK_SNAPSHOT (Print Screen)
		0x92 => 117,                              // VK_OEM_NEC_EQUAL (keypad =)
		0x13 => 119,                              // VK_PAUSE
		0xC2 => 121,                              // VK_ABNT_C2 (keypad comma)
		0x15 => 122,                              // VK_HANGUL / VK_KANA
		0x19 => 123,                              // VK_HANJA
		0x7C..=0x87 => 183 + (vk - 0x7C) as u32, // VK_F13..VK_F24
		_ => return None,
	})
}

/// Keys the LL hook passes through to THIS machine even while engaged: the volume / media /
/// browser keys (VK_BROWSER_BACK..VK_LAUNCH_APP2), so the local speakers stay under local
/// control. Every other key without an evdev mapping is swallowed while engaged — a key the
/// remote cannot get must not act on the local desktop behind the session either.
pub(super) fn local_only_vk(vk: u16) -> bool {
	matches!(vk, 0xA6..=0xB7)
}

#[cfg(test)]
mod tests {
	use super::{local_only_vk, scancode_to_evdev, vk_to_evdev};
	use pulsar_core::input::keytables::CLIENT_KEYS;

	#[test]
	fn vk_table_reaches_every_client_key() {
		// Every key a host can type is reachable from the Windows hook, except two a VK cannot
		// tell apart: the JIS Yen key (VK_OEM_5, like backslash on JIS layouts) and Numpad
		// Enter (VK_RETURN, like Enter). Both reach the host through Interception / the webview.
		let reached: std::collections::HashSet<u32> = (0u16..=0xFF).filter_map(vk_to_evdev).collect();
		for &k in CLIENT_KEYS {
			if k == 124 || k == 96 {
				continue;
			}
			assert!(reached.contains(&(k as u32)), "evdev {k} unreachable from a VK");
		}
	}

	#[test]
	fn interception_reaches_every_client_key() {
		let mut reached = std::collections::HashSet::new();
		for code in 0u16..=0xFF {
			for e0 in [false, true] {
				if let Some(k) = scancode_to_evdev(code, e0) {
					reached.insert(k);
				}
			}
		}
		// Pause arrives as an E1 sequence, handled before this table (imp.rs). The JIS keypad
		// comma's scancode 0x5C is read as Right Win (kept E0-less on purpose: some paths drop
		// the flag, and a lost Win key opens the local Start menu).
		for &k in CLIENT_KEYS {
			if k == 119 || k == 95 {
				continue;
			}
			assert!(reached.contains(&(k as u32)), "evdev {k} unreachable from a scancode");
		}
	}

	#[test]
	fn extras_map_to_their_evdev_codes() {
		assert_eq!(vk_to_evdev(0xE2), Some(86)); // ISO <>|
		assert_eq!(vk_to_evdev(0x2C), Some(99)); // Print Screen
		assert_eq!(vk_to_evdev(0x13), Some(119)); // Pause
		assert_eq!(vk_to_evdev(0x7C), Some(183)); // F13
		assert_eq!(vk_to_evdev(0x87), Some(194)); // F24
		assert_eq!(scancode_to_evdev(0x56, false), Some(86));
		assert_eq!(scancode_to_evdev(0x64, false), Some(183)); // F13
		assert_eq!(scancode_to_evdev(0x76, false), Some(194)); // F24
		assert_eq!(scancode_to_evdev(0x37, true), Some(99)); // Print Screen
		assert_eq!(scancode_to_evdev(0x7D, false), Some(124)); // Yen
		assert!(local_only_vk(0xAF)); // volume up stays local
		assert!(!local_only_vk(0x41));
	}
	#[test]
	fn win_keys_map_to_meta() {
		assert_eq!(vk_to_evdev(0x5B), Some(125)); // LWin → MetaLeft
		assert_eq!(vk_to_evdev(0x5C), Some(126)); // RWin → MetaRight
		assert_eq!(vk_to_evdev(0x41), Some(30)); // A
		assert_eq!(vk_to_evdev(0x1B), Some(1)); // Esc
	}

	#[test]
	fn interception_scancodes_map_to_evdev() {
		// Base block is identity (evdev derived from set-1 scancodes).
		assert_eq!(scancode_to_evdev(0x1E, false), Some(30)); // A
		assert_eq!(scancode_to_evdev(0x01, false), Some(1)); // Esc
		assert_eq!(scancode_to_evdev(0x1C, false), Some(28)); // Enter
		assert_eq!(scancode_to_evdev(0x1D, false), Some(29)); // Left Ctrl
		assert_eq!(scancode_to_evdev(0x2A, false), Some(42)); // Left Shift
														// E0-extended keys need the explicit table.
		assert_eq!(scancode_to_evdev(0x5B, true), Some(125)); // Left Win
		assert_eq!(scancode_to_evdev(0x1D, true), Some(97)); // Right Ctrl
		assert_eq!(scancode_to_evdev(0x38, true), Some(100)); // Right Alt
		assert_eq!(scancode_to_evdev(0x48, true), Some(103)); // Arrow Up
														// Out-of-range / unmapped → None (passed through locally).
		assert_eq!(scancode_to_evdev(0xF0, false), None);
		assert_eq!(scancode_to_evdev(0x2A, true), None); // PrtScn fake-shift
	}
}
