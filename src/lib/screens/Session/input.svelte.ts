// Remote-control input engine for a session, lifted out of Session.svelte. Owns the
// `controlling` flag, the absolute-positioning pointer forwarding (VNC-style, not pointer
// lock — which froze the webview), key forwarding with held-key tracking, and the rAF pump
// that sends the latest pointer position at ≈refresh rate. Behaviour is identical to the
// original inline script; inputs are getters so the pump effect tracks them as it did inline.
//
// Instantiated once at component init so its $effect() scopes to + tears down with the
// component (canvas/playId are read lazily through getters).

import { api } from '#lib/api.js';
import { evdevCode } from '#lib/keymap.js';
import type { PenSample, TouchSample } from '#lib/api.types.js';

// Confine the OS cursor to the session's video area (Linux; the app window elsewhere) while
// controlling (best-effort; no-op where unsupported). Rust command, NOT
// window.setCursorGrab(): tao's Linux set_cursor_grab is a silent no-op (`Ok(())`), so the
// backend does a real X11 pointer grab with confine_to on Linux and falls back to tao's grab
// (ClipCursor) on Windows/macOS.
// The VNC-style absolute path deliberately avoids the Pointer Lock API (it froze the
// webview), but without ANY confinement the cursor walks off the window mid-drag.
function grabCursor(on: boolean, id?: number) {
	import('@tauri-apps/api/core')
		.then(({ invoke }) => invoke('confine_pointer', { on, id: id !== undefined && id >= 0 ? id : null }))
		.catch(() => {
			// not running under Tauri (vite dev / tests)
		});
}

// Fire a payload-less Tauri event (same bus the OS-level keyboard hooks use), so the
// VNC path's combos reuse Session.svelte's existing 'kbd-leave'/'overlay-toggle' handlers.
function emitTauri(event: string) {
	import('@tauri-apps/api/event')
		.then(({ emit }) => emit(event))
		.catch(() => {});
}

/** Zoom factor of one synthesized pinch wheel event (Chromium and WebKit report a touchpad
 * pinch as `wheel` with `ctrlKey`; Chromium's own page zoom uses exp(-deltaY / 100)). */
export function pinchWheelFactor(deltaY: number, deltaMode = 0): number {
	if (!Number.isFinite(deltaY)) return 1;
	const px = deltaMode === 1 ? deltaY * 40 : deltaMode === 2 ? deltaY * 800 : deltaY;
	return Math.exp(Math.min(3, Math.max(-3, -px / 100)));
}

/** No pinch wheel step for this long = the synthesized pinch ended (Windows hook: 200 ms). */
export const PINCH_IDLE_MS = 200;

/** WebKit's (non-standard) GestureEvent: a trackpad pinch on macOS. */
type GestureLike = Event & { scale?: number };

type Inputs = {
	playId: () => number;
	wsPort: () => number;
	canvas: () => HTMLCanvasElement;
	mode: () => 'remote' | 'game';
	native: () => boolean;
	streamW: () => number;
	streamH: () => number;
	fitMode: () => 'fit' | 'stretch' | 'original';
	/** "Serbest imleç" applies (setting on + remote mode): no confine, hover in/out hands
	 * control over (Linux/macOS canvas path; Windows does it in the native hook). */
	pointerFree: () => boolean;
	/** Hovering must not take control now (session menu open, stream stopped). */
	hoverBlocked: () => boolean;
	/** The OS-level capture owns control (macOS event tap with the Accessibility permission —
	 * Windows' hook never lets the click reach this canvas anyway): the canvas path stays out
	 * of its way, or every click and key would go to the host twice. */
	osCapture?: () => boolean;
};

export class SessionInput {
	#in: Inputs;

	// Control via absolute positioning (VNC-style). Click the screen to start controlling;
	// the remote cursor follows yours over the canvas.
	controlling = $state(false);
	// Controllable once the video relay is up. (We used to disable control whenever the host
	// was reached over loopback — but that wrongly blocked legitimate cases like ASTER
	// multiseat: a *different* seat on the same box is fine to drive.) A getter (not a
	// $derived field) so it doesn't dereference #in before the constructor assigns it.
	//
	// The native (Linux) renderer decodes directly (no UDP→WS relay), so `wsPort` is 0 there —
	// which used to leave native REMOTE sessions with no working control path (the evdev grab
	// is a separate mechanism that no-ops unless its capture thread is up, e.g. controlling a
	// PHONE host). Allow the VNC-style absolute path on native remote too: it sends continuous
	// absolute pointer + buttons, which every host (desktop uinput, phone cursor/gesture)
	// injects. Game mode is untouched — it keeps the relative evdev-grab path for FPS mouselook.
	get controllable() {
		if (this.#in.osCapture?.()) return false;
		return this.#in.wsPort() > 0 || (this.#in.native() && this.#in.mode() === 'remote');
	}

	#moveDirty = false;
	#nx = 0;
	#ny = 0;
	#armingClick = false;
	#lastScroll = 0;
	// Every key we've forwarded as "down" and not yet released. We release EXACTLY these
	// (no blind modifier list) so a key can never stay stuck on the host — including the
	// Win key. The old code excluded Win to avoid a spurious lone Win-up popping Start,
	// but since we only release keys actually held, releasing Win here is correct.
	#heldKeys = new Set<number>();
	// Physical codes (KeyboardEvent.code) we resolved to a Unicode char and forwarded as a
	// one-shot `Char` (layout-independent). These have NO held key-down on the host, so their
	// key-up is suppressed (mirrors the Linux native hook's char_keys) and they're never put in
	// #heldKeys — nothing to release on blur/disengage.
	#charKeys = new Set<string>();
	// Free cursor: a pass over the video may take control. Cleared by an explicit release
	// (Ctrl+Alt+Z, Ctrl+Shift+F11, the native detach combos) until the pointer has left the
	// video once — the user asked to let go, hovering must not grab it straight back.
	#hoverArmed = true;
	// Control came from hovering after the first time this session: the in-control hint stays
	// quiet (it would flash on every pass over the video).
	quietHint = $state(false);
	#hoverEngaged = false;
	// Touchpad pinch being forwarded (`input_pinch`). `gesture` = it comes from WebKit's
	// gesture* events (macOS), else from synthesized Ctrl + wheel (ended by an idle timer).
	#pinchOn = false;
	#pinchGesture = false;
	#pinchScale = 1;
	#pinchTimer: ReturnType<typeof setTimeout> | undefined;
	// Pen / touch (PointerEvent pointerType 'pen' / 'touch'): forwarded as their own state
	// messages, not as the mouse — the Rust side turns them into the mouse for a host that
	// does not inject pen / touch. Moves are batched into the rAF pump like the pointer;
	// touch-down / lift and pen contact edges go out at once.
	#pen: PenSample | null = null;
	#penDirty = false;
	// Which end of each pen (pointerId) last touched: a hovering pen reports no buttons, so
	// the eraser would flip to false on every hover move (a tool switch on the host).
	#penEraser = new Map<number, boolean>();
	#touches = new Map<number, TouchSample>();
	#touchDirty = false;
	#flushQueued = false;

	constructor(inputs: Inputs) {
		this.#in = inputs;

		// While controlling: a rAF pump sends the latest pointer position at ≈refresh
		// rate (never per raw event), plus window-level keyboard capture.
		$effect(() => {
			if (!this.controlling || typeof window === 'undefined') return;
			const playId = this.#in.playId();
			const pump = () => {
				if (this.#moveDirty) {
					this.#moveDirty = false;
					api.inputPointer(playId, this.#nx, this.#ny).catch(() => {});
				}
				this.#flushPenTouch();
				raf = requestAnimationFrame(pump);
			};
			let raf = requestAnimationFrame(pump);
			const kd = (e: KeyboardEvent) => this.onKey(e, true);
			const ku = (e: KeyboardEvent) => this.onKey(e, false);
			// Focus stolen (Start menu opened by Win, Alt+Tab, etc.) → the webview won't get
			// the key-ups, so release everything held on the host to avoid stuck keys.
			const onBlur = () => {
				this.#releasePenTouch();
				this.#releaseButtons();
				this.#releaseHeldKeys();
				this.#pinchEnd(playId);
				// Don't keep the OS cursor confined while another window has focus.
				grabCursor(false);
			};
			// Free cursor: never confined (leaving the video is how control is handed back).
			const free = this.#in.pointerFree();
			const onFocus = () => {
				if (!free) grabCursor(true);
			};
			window.addEventListener('keydown', kd, true);
			window.addEventListener('keyup', ku, true);
			window.addEventListener('blur', onBlur);
			window.addEventListener('focus', onFocus);
			// macOS trackpad pinch (WKWebView). Not in Svelte's element typings → addEventListener.
			const cv = this.#in.canvas();
			cv.addEventListener('gesturestart', this.onGestureStart);
			cv.addEventListener('gesturechange', this.onGestureChange);
			cv.addEventListener('gestureend', this.onGestureEnd);
			// Confined to this session's video container (falls back to the app window).
			if (!free) grabCursor(true, this.#in.playId());
			return () => {
				cancelAnimationFrame(raf);
				window.removeEventListener('keydown', kd, true);
				window.removeEventListener('keyup', ku, true);
				window.removeEventListener('blur', onBlur);
				window.removeEventListener('focus', onFocus);
				cv.removeEventListener('gesturestart', this.onGestureStart);
				cv.removeEventListener('gesturechange', this.onGestureChange);
				cv.removeEventListener('gestureend', this.onGestureEnd);
				this.#pinchEnd(playId);
				// Covers stopControl AND component unmount while controlling.
				grabCursor(false);
			};
		});
	}

	#norm(e: PointerEvent) {
		const p = this.#videoXY(e);
		if (!p) return;
		[this.#nx, this.#ny] = p;
	}
	/** The event's position as 0..1 over the video (letterbox-aware, see #norm's notes). */
	#videoXY(e: PointerEvent): [number, number] | null {
		const r = this.#in.canvas().getBoundingClientRect();
		if (r.width <= 0 || r.height <= 0) return null;
		let x = (e.clientX - r.left) / r.width;
		let y = (e.clientY - r.top) / r.height;
		// FIT (default): the video is aspect-fit (letterboxed) inside the canvas, so the video
		// rect is SMALLER than the canvas in one axis. Normalizing over the whole canvas then
		// mismatches the host's coordinate space — a portrait phone in a wider window is
		// pillarboxed, so a small vertical mouse move mapped over the short canvas height
		// over-scaled onto the tall phone screen ("move up a little → cursor jumps"). Map over
		// the ACTUAL video rect instead. STRETCH fills the canvas (no correction); ORIGINAL is
		// left as canvas-relative (rare, native-size/cropped — no simple linear rect).
		const sw = this.#in.streamW();
		const sh = this.#in.streamH();
		if (this.#in.fitMode() === 'fit' && sw > 0 && sh > 0) {
			const canvasAR = r.width / r.height;
			const videoAR = sw / sh;
			if (canvasAR > videoAR) {
				// Pillarbox: bars left/right, video fills the height.
				const vw = r.height * videoAR;
				const ox = (r.width - vw) / 2;
				x = (e.clientX - r.left - ox) / vw;
			} else {
				// Letterbox: bars top/bottom, video fills the width.
				const vh = r.width / videoAR;
				const oy = (r.height - vh) / 2;
				y = (e.clientY - r.top - oy) / vh;
			}
		}
		return [Math.min(1, Math.max(0, x)), Math.min(1, Math.max(0, y))];
	}
	/** Pen state from a PointerEvent (buttons: 1 = tip, 2 = barrel, 32 = eraser). The eraser
	 * flag is only reliable in contact and on the lift (button 5); while the pen hovers it
	 * keeps the end that last touched, until the pen leaves. */
	#penSample(e: PointerEvent, kind: 'down' | 'move' | 'up' | 'leave'): PenSample | null {
		const p = this.#videoXY(e);
		const inRange = kind !== 'leave';
		const contact = inRange && (e.buttons & (1 | 32)) !== 0;
		let eraser = (e.buttons & 32) !== 0 || e.button === 5;
		if (contact || kind === 'up') this.#penEraser.set(e.pointerId, eraser);
		else eraser = eraser || (this.#penEraser.get(e.pointerId) ?? false);
		if (!inRange) this.#penEraser.delete(e.pointerId);
		if (!p) return null;
		return {
			x: p[0],
			y: p[1],
			// Devices without pressure report 0.5 while touching, 0 while hovering.
			pressure: contact ? e.pressure || 0.5 : 0,
			tiltX: e.tiltX || 0,
			tiltY: e.tiltY || 0,
			twist: e.twist || 0,
			inRange,
			contact,
			eraser,
			barrel: (e.buttons & 2) !== 0
		};
	}
	/** Forward a pen sample: edges (contact / range / buttons) now, moves on the next frame. */
	#sendPen(next: PenSample) {
		const prev = this.#pen;
		this.#pen = next.inRange ? next : null;
		const edge =
			!prev ||
			!next.inRange ||
			prev.contact !== next.contact ||
			prev.barrel !== next.barrel ||
			prev.eraser !== next.eraser;
		if (edge) {
			this.#penDirty = false;
			api.inputPen(this.#in.playId(), next).catch(() => {});
		} else {
			this.#penDirty = true;
			this.#scheduleFlush();
		}
	}
	/** Send the batched pen / touch moves (the rAF pump while controlling, else one frame). */
	#flushPenTouch() {
		const playId = this.#in.playId();
		if (this.#penDirty && this.#pen) {
			this.#penDirty = false;
			api.inputPen(playId, this.#pen).catch(() => {});
		}
		if (this.#touchDirty) {
			this.#touchDirty = false;
			api.inputTouch(playId, [...this.#touches.values()]).catch(() => {});
		}
	}
	#scheduleFlush() {
		if (this.controlling || this.#flushQueued || typeof requestAnimationFrame === 'undefined') return;
		this.#flushQueued = true;
		requestAnimationFrame(() => {
			this.#flushQueued = false;
			this.#flushPenTouch();
		});
	}
	#sendTouches() {
		this.#touchDirty = false;
		api.inputTouch(this.#in.playId(), [...this.#touches.values()]).catch(() => {});
	}
	/** Lift the pen and every finger on the host (blur, release, end of control). */
	#releasePenTouch() {
		const playId = this.#in.playId();
		if (this.#pen) {
			const out = { ...this.#pen, inRange: false, contact: false, pressure: 0 };
			this.#pen = null;
			this.#penDirty = false;
			if (playId >= 0) api.inputPen(playId, out).catch(() => {});
		}
		if (this.#touches.size) {
			this.#touches.clear();
			this.#touchDirty = false;
			if (playId >= 0) api.inputTouch(playId, []).catch(() => {});
		}
	}
	/** Native game mode (Linux / macOS canvas): the evdev / native capture takes only the
	 * keyboard and mouse, so a pen or a finger on the video reaches this canvas without any
	 * "controlling" state — forward it straight away (the Windows renderer does the same with
	 * WM_POINTER while engaged). */
	#gamePenTouch(e: PointerEvent) {
		return (
			!this.controlling &&
			this.#in.native() &&
			this.#in.mode() === 'game' &&
			(e.pointerType === 'pen' || e.pointerType === 'touch')
		);
	}
	/** A pen / touch event while controlling: forwarded as pen / touch, never as the mouse. */
	#penTouch(e: PointerEvent, kind: 'down' | 'move' | 'up' | 'leave'): boolean {
		if (e.pointerType === 'pen') {
			const s = this.#penSample(e, kind);
			if (s) this.#sendPen(s);
			return true;
		}
		if (e.pointerType === 'touch') {
			if (kind === 'down' || kind === 'move') {
				const p = this.#videoXY(e);
				if (!p) return true;
				const known = this.#touches.has(e.pointerId);
				if (kind === 'move' && !known) return true; // a finger we never saw land
				this.#touches.set(e.pointerId, { id: e.pointerId >>> 0, x: p[0], y: p[1], pressure: e.pressure || 0 });
				if (known) {
					this.#touchDirty = true;
					this.#scheduleFlush();
				} else this.#sendTouches();
			} else if (this.#touches.delete(e.pointerId)) {
				this.#sendTouches();
			}
			return true;
		}
		return false;
	}
	#releaseButtons() {
		const playId = this.#in.playId();
		if (playId < 0) return;
		api.inputButton(playId, 0, false).catch(() => {});
		api.inputButton(playId, 1, false).catch(() => {});
		api.inputButton(playId, 2, false).catch(() => {});
	}
	#releaseHeldKeys() {
		// Char keys are one-shot (no held VK on the host), so there's nothing to release —
		// just drop the tracking so a later key-up isn't wrongly suppressed.
		this.#charKeys.clear();
		const playId = this.#in.playId();
		if (playId < 0) {
			this.#heldKeys.clear();
			return;
		}
		for (const c of this.#heldKeys) api.inputKey(playId, c, false).catch(() => {});
		this.#heldKeys.clear();
	}
	startControl = () => {
		if (this.controllable && !this.controlling) {
			this.controlling = true;
			this.#in.canvas().focus();
			// Capture OS-reserved keys (Win/Alt+Tab/Ctrl+Esc) for the remote — without an
			// OS-level hook the local WM swallows them before the webview ever sees a keydown.
			// KEYBOARD-ONLY (kbdCaptureStart's mouse param defaults to false): the Linux evdev
			// grab then takes just the keyboards, so the pointer events this VNC path relies on
			// keep flowing to the webview. (The old blanket "skip on native" predates the
			// keyboard-only mode — it was avoiding the full kb+mouse grab.) The native grab is
			// focus-gated and needs an explicit engage; Windows' LL-hook path ignores it.
			const playId = this.#in.playId();
			if (playId >= 0) {
				api.kbdCaptureStart(playId).catch(() => {});
				if (this.#in.native()) api.kbdEngage().catch(() => {});
			}
		}
	};
	stopControl = () => {
		if (this.controlling) {
			this.#releasePenTouch();
			this.controlling = false;
			api.kbdCaptureStop().catch(() => {});
			this.#releaseButtons();
			this.#releaseHeldKeys();
		}
	};
	// The native capture was RELEASED from outside the webview (Ctrl+Alt+Z / 3×RightCtrl →
	// kbd-detach): drop the canvas control too, or `.video.control`'s `cursor: none` keeps
	// the local cursor hidden over the video (the pass-through renderer leaves the pointer to
	// this canvas). The capture thread stays armed — a detach only disengages it — so no
	// kbdCaptureStop; the next video click re-engages via startControl as usual.
	detach = () => {
		this.#hoverArmed = false; // an explicit release (combo): no hover re-grab
		if (this.controlling) {
			this.#releasePenTouch();
			this.controlling = false;
			this.#releaseButtons();
			this.#releaseHeldKeys();
		}
	};
	#hostButton = (b: number) => (b === 2 ? 1 : b === 1 ? 2 : 0);

	// Free cursor: the pointer is over the video with no button down, Pulsar is focused and
	// nothing blocks it → take control without a click.
	#tryHover(e: PointerEvent) {
		if (this.controlling || !this.#in.pointerFree() || !this.#hoverArmed || e.buttons !== 0) return;
		if (this.#in.hoverBlocked() || typeof document === 'undefined' || !document.hasFocus()) return;
		this.startControl();
		if (!this.controlling) return;
		this.quietHint = this.#hoverEngaged; // the first hover engage keeps its hint
		this.#hoverEngaged = true;
		this.#norm(e);
		this.#moveDirty = true;
	}
	onEnter = (e: PointerEvent) => this.#tryHover(e);
	onMove = (e: PointerEvent) => {
		if (this.#gamePenTouch(e)) {
			this.#penTouch(e, 'move');
			return;
		}
		if (!this.controlling) {
			this.#tryHover(e);
			return;
		}
		if (this.#penTouch(e, 'move')) return;
		this.#norm(e);
		this.#moveDirty = true;
	};
	// Pointer left the canvas → stop re-sending the last position. Free cursor: leaving the
	// video hands control back (a drag keeps it — the pointer is captured until the button is
	// up, and the leave then arrives with no button held).
	clearMove = (e?: PointerEvent) => {
		// A pen leaving the video leaves range on the host too; a finger sliding off lifts.
		if (e && (this.controlling || this.#gamePenTouch(e)) && this.#penTouch(e, 'leave')) return;
		this.#moveDirty = false;
		if (!this.#in.pointerFree()) return;
		this.#hoverArmed = true;
		if (this.controlling && (e?.buttons ?? 0) === 0) this.stopControl();
	};
	// The click that takes control must not reach the host AT ALL — neither its down
	// (swallowed below) nor its up (which would otherwise fire a stray button-up,
	// e.g. a right-click context menu on the host).
	onDown = (e: PointerEvent) => {
		if (this.#gamePenTouch(e)) {
			e.preventDefault();
			this.#penTouch(e, 'down');
			return;
		}
		if (!this.controlling) {
			this.startControl();
			this.#armingClick = true;
			// Seed the host pointer at the clicked spot (position only — the BUTTON stays
			// swallowed). Control then starts where the user clicked, and a phone host
			// creates/shows its cursor overlay immediately instead of only on the first
			// post-click move. The pump picks #moveDirty up on its first tick.
			if (this.controlling) {
				this.#norm(e);
				this.#moveDirty = true;
			}
			return; // the focusing click's press/release isn't forwarded
		}
		e.preventDefault();
		if (this.#penTouch(e, 'down')) {
			// Keep the stroke's events when it leaves the video (touch is captured implicitly).
			try {
				this.#in.canvas().setPointerCapture(e.pointerId);
			} catch {
				/* no capture support */
			}
			return;
		}
		// Free cursor: keep a drag's events (and the remote's button) until release even if it
		// leaves the video; pointerleave then comes after the up.
		if (this.#in.pointerFree()) {
			try {
				this.#in.canvas().setPointerCapture(e.pointerId);
			} catch {
				/* no capture support — the leave just releases earlier */
			}
		}
		this.#norm(e);
		this.#moveDirty = true;
		api.inputButton(this.#in.playId(), this.#hostButton(e.button), true).catch(() => {});
	};
	onUp = (e: PointerEvent) => {
		if (this.#gamePenTouch(e)) {
			e.preventDefault();
			this.#penTouch(e, 'up');
			return;
		}
		if (this.#armingClick) {
			this.#armingClick = false;
			return; // swallow the focusing click's release
		}
		if (!this.controlling) return;
		e.preventDefault();
		if (this.#penTouch(e, 'up')) return;
		api.inputButton(this.#in.playId(), this.#hostButton(e.button), false).catch(() => {});
	};
	#pinchBegin(gesture: boolean) {
		const playId = this.#in.playId();
		this.#pinchEnd(playId);
		this.#pinchOn = true;
		this.#pinchGesture = gesture;
		this.#pinchScale = 1;
		api.inputPinch(playId, 0, 1).catch(() => {});
	}
	#pinchUpdate(scale: number) {
		if (!this.#pinchOn || !Number.isFinite(scale) || scale <= 0) return;
		this.#pinchScale = Math.min(20, Math.max(0.05, scale));
		api.inputPinch(this.#in.playId(), 1, this.#pinchScale).catch(() => {});
	}
	#pinchEnd(playId = this.#in.playId()) {
		clearTimeout(this.#pinchTimer);
		this.#pinchTimer = undefined;
		if (!this.#pinchOn) return;
		this.#pinchOn = false;
		api.inputPinch(playId, 2, this.#pinchScale).catch(() => {});
	}
	onGestureStart = (e: GestureLike) => {
		if (!this.controlling) return;
		e.preventDefault(); // never zoom the webview itself
		this.#pinchBegin(true);
	};
	onGestureChange = (e: GestureLike) => {
		if (!this.controlling) return;
		e.preventDefault();
		if (this.#pinchOn && this.#pinchGesture) this.#pinchUpdate(e.scale ?? 1);
	};
	onGestureEnd = (e: GestureLike) => {
		if (!this.controlling) return;
		e.preventDefault();
		if (!this.#pinchOn || !this.#pinchGesture) return;
		this.#pinchUpdate(e.scale ?? this.#pinchScale);
		this.#pinchEnd();
	};
	// The browser took the pointer over (a system gesture, palm rejection): lift it.
	onCancel = (e: PointerEvent) => {
		if (this.controlling || this.#gamePenTouch(e)) this.#penTouch(e, 'leave');
	};
	onWheel = (e: WheelEvent) => {
		if (!this.controlling) return;
		e.preventDefault();
		// Touchpad pinch: the engine reports it as Ctrl + wheel with NO Ctrl key held (a Ctrl
		// the user holds was forwarded as a key and is in #heldKeys — that is a real Ctrl +
		// wheel and scrolls below, zooming on the host because its Ctrl is down).
		if (e.ctrlKey && !this.#heldKeys.has(29) && !this.#heldKeys.has(97)) {
			if (this.#pinchOn && this.#pinchGesture) return; // WebKit already sends gesture*
			if (!this.#pinchOn) this.#pinchBegin(false);
			this.#pinchUpdate(this.#pinchScale * pinchWheelFactor(e.deltaY, e.deltaMode));
			clearTimeout(this.#pinchTimer);
			this.#pinchTimer = setTimeout(() => this.#pinchEnd(), PINCH_IDLE_MS);
			return;
		}
		const now = performance.now();
		if (now - this.#lastScroll < 30) return;
		this.#lastScroll = now;
		api.inputScroll(this.#in.playId(), e.deltaX, e.deltaY).catch(() => {});
	};
	onKey(e: KeyboardEvent, down: boolean) {
		if (!this.controlling) return;
		// Parsec-style leave combo: Ctrl+Shift+F11 (or Cmd+Shift+F11 for mac users)
		// releases control. preventDefault also keeps F11 from the webview.
		if (down && (e.ctrlKey || e.metaKey) && e.shiftKey && e.code === 'F11') {
			e.preventDefault();
			this.#hoverArmed = false;
			this.stopControl();
			return;
		}
		// Ctrl+Alt+Z — the detach combo the engage hint advertises. On desktop-host
		// sessions the NATIVE hook (evdev grab / WH_KEYBOARD_LL) intercepts it, but this
		// VNC-style path (phone host / native remote) deliberately never arms that hook —
		// so handle it here too, otherwise the combo is forwarded to the host and control
		// never releases. stopControl() releases held keys/buttons (incl. this Ctrl+Alt).
		if (down && e.ctrlKey && e.altKey && e.code === 'KeyZ') {
			e.preventDefault();
			this.#hoverArmed = false;
			this.stopControl();
			return;
		}
		// Ctrl+Shift+Q (leave/end) and Ctrl+Shift+M (game-overlay toggle) are emitted by
		// the OS-level hook on desktop-host sessions ('kbd-leave' / 'overlay-toggle').
		// This VNC-style path never arms that hook, so emit the SAME events from here —
		// Session.svelte's existing listeners handle both identically.
		if (down && e.ctrlKey && e.shiftKey && e.code === 'KeyQ') {
			e.preventDefault();
			this.stopControl();
			emitTauri('kbd-leave');
			return;
		}
		if (down && e.ctrlKey && e.shiftKey && e.code === 'KeyM') {
			e.preventDefault();
			emitTauri('overlay-toggle');
			return;
		}
		const playId = this.#in.playId();
		// WYSIWYG / layout-independent text (mirrors the Linux native hook): a printable key
		// with no SHORTCUT modifier (Ctrl/Meta/Alt — but AltGr is NOT a shortcut) resolves via
		// THIS client's keyboard layout to a Unicode codepoint and is sent as `Char`, so the host
		// inserts that exact character regardless of its own active layout. Without this every key
		// goes through the positional evdev→VK table, so non-US layouts mistype and AltGr symbols
		// (@ € { }) become e.g. macOS Option dead-keys. Shortcuts and non-text keys (Enter, arrows,
		// F-keys, modifiers) still take the `Key` path so VK-level semantics are preserved.
		const altGraph = e.getModifierState && e.getModifierState('AltGraph');
		const isShortcut = !altGraph && (e.ctrlKey || e.metaKey || e.altKey);
		// Game mode must deliver raw scancodes/VK so games can see movement keys (WASD, digits,
		// space) via DirectInput / RawInput / GetAsyncKeyState — KEYEVENTF_UNICODE and CGEvent
		// Unicode injection are invisible to those APIs. The Char path is only correct for
		// remote-desktop mode where layout-independent text entry matters.
		const printable =
			this.#in.mode() === 'remote' &&
			e.key.length === 1 &&
			e.key.codePointAt(0)! >= 0x20 &&
			// Space and Tab carry positional/VK semantics (button activation, focus, pan, scroll,
			// page-down) that Unicode text injection (KEYEVENTF_UNICODE / CGEvent set_string)
			// cannot reproduce — Win32 controls and browsers react to VK_SPACE/VK_TAB key events,
			// not WM_CHAR. Exclude them so they take the evdev/VK Key path even in remote mode.
			e.code !== 'Space' &&
			e.code !== 'Tab' &&
			!isShortcut;
		// Char path is resolved BEFORE the evdev-code guard: international/ISO keys
		// (IntlBackslash, IntlRo, IntlYen, etc.) have no entry in the EVDEV table so
		// evdevCode returns 0 — but they carry a valid e.key printable character and must
		// still reach the host via inputChar. The evdev code is only required for the Key
		// (raw scancode/VK) path below.
		if (down && printable) {
			e.preventDefault();
			// If this physical key was previously held as a raw VK (e.g. the user pressed
			// Ctrl+'a' so the first down took the Key path, then released Ctrl while holding
			// 'a' so the auto-repeat now arrives here as printable), release the stale VK on
			// the host BEFORE switching to the Char path. Without this the held VK is never
			// released: the matching key-UP below will hit #charKeys.delete and return early,
			// leaving the host key stuck. Sending a key-up for a not-held VK is a harmless
			// host no-op (symmetric with C4's fix in the Linux native evdev path).
			const c = evdevCode(e.code);
			if (c && this.#heldKeys.delete(c)) {
				api.inputKey(playId, c, false).catch(() => {});
			}
			// One-shot char insert (no held VK on the host); remember it so the key-up is
			// suppressed and so blur/disengage doesn't try to release a key never held.
			this.#charKeys.add(e.code);
			api.inputChar(playId, e.key).catch(() => {});
			return;
		}
		// Key-up for a key that was sent as a one-shot Char: suppress it (no held VK to
		// release on the host). Also checked before the evdev guard for the same reason —
		// IntlBackslash key-ups would otherwise fall through and be dropped by !code.
		if (!down && this.#charKeys.delete(e.code)) return;
		const code = evdevCode(e.code);
		if (!code) return;
		e.preventDefault();
		if (down) {
			// Track held keys so focus loss (the Win key popping Start, Alt+Tab) can release
			// them on the host — otherwise the key-up never arrives (the webview lost focus)
			// and the key stays stuck, e.g. Win held → every letter becomes a Win+letter shortcut.
			// If this physical key was previously tracked as a one-shot Char (e.g. 'a' was held
			// without a modifier, then Ctrl was pressed mid-hold so the auto-repeat now carries
			// ctrlKey=true), drop the stale #charKeys entry. Without this the key-UP handler
			// hits the `#charKeys.delete` branch first and suppresses the matching
			// inputKey(code, false), leaving the host key stuck.
			this.#charKeys.delete(e.code);
			this.#heldKeys.add(code);
		} else {
			this.#heldKeys.delete(code);
		}
		api.inputKey(playId, code, down).catch(() => {});
	}
}
