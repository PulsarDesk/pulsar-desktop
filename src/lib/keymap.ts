// Browser `KeyboardEvent.code` (physical key) → Linux evdev keycode, for input
// injection through the Wayland RemoteDesktop portal (`notify_keyboard_keycode`).
// Values from linux/input-event-codes.h.
export const EVDEV: Record<string, number> = {
	Escape: 1,
	Digit1: 2, Digit2: 3, Digit3: 4, Digit4: 5, Digit5: 6,
	Digit6: 7, Digit7: 8, Digit8: 9, Digit9: 10, Digit0: 11,
	Minus: 12, Equal: 13, Backspace: 14, Tab: 15,
	KeyQ: 16, KeyW: 17, KeyE: 18, KeyR: 19, KeyT: 20, KeyY: 21,
	KeyU: 22, KeyI: 23, KeyO: 24, KeyP: 25, BracketLeft: 26, BracketRight: 27,
	Enter: 28, ControlLeft: 29,
	KeyA: 30, KeyS: 31, KeyD: 32, KeyF: 33, KeyG: 34, KeyH: 35,
	KeyJ: 36, KeyK: 37, KeyL: 38, Semicolon: 39, Quote: 40, Backquote: 41,
	ShiftLeft: 42, Backslash: 43,
	KeyZ: 44, KeyX: 45, KeyC: 46, KeyV: 47, KeyB: 48, KeyN: 49, KeyM: 50,
	Comma: 51, Period: 52, Slash: 53, ShiftRight: 54,
	NumpadMultiply: 55, AltLeft: 56, Space: 57, CapsLock: 58,
	F1: 59, F2: 60, F3: 61, F4: 62, F5: 63, F6: 64, F7: 65, F8: 66, F9: 67, F10: 68,
	NumLock: 69, ScrollLock: 70,
	Numpad7: 71, Numpad8: 72, Numpad9: 73, NumpadSubtract: 74,
	Numpad4: 75, Numpad5: 76, Numpad6: 77, NumpadAdd: 78,
	Numpad1: 79, Numpad2: 80, Numpad3: 81, Numpad0: 82, NumpadDecimal: 83,
	IntlBackslash: 86, // ISO <>| next to Left Shift
	F11: 87, F12: 88,
	IntlRo: 89, // JIS \_
	Convert: 92, KanaMode: 93, NonConvert: 94, NumpadJpComma: 95,
	NumpadEnter: 96, ControlRight: 97, NumpadDivide: 98, PrintScreen: 99, AltRight: 100,
	Home: 102, ArrowUp: 103, PageUp: 104, ArrowLeft: 105, ArrowRight: 106,
	End: 107, ArrowDown: 108, PageDown: 109, Insert: 110, Delete: 111,
	NumpadEqual: 117, Pause: 119, NumpadComma: 121,
	Lang1: 122, Lang2: 123, // Korean Hangul / Hanja, Japanese Kana / Eisu (macOS)
	IntlYen: 124, // JIS ¥|
	MetaLeft: 125, MetaRight: 126, ContextMenu: 127,
	F13: 183, F14: 184, F15: 185, F16: 186, F17: 187, F18: 188,
	F19: 189, F20: 190, F21: 191, F22: 192, F23: 193, F24: 194
};

/** Map a KeyboardEvent.code to an evdev keycode, or 0 if unknown. */
export function evdevCode(code: string): number {
	return EVDEV[code] ?? 0;
}
