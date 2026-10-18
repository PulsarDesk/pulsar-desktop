// Escape closes the TOPMOST open modal — every modal registers its close here while it is up.
// One capture-phase window listener runs only the newest entry and stops the event, so a stacked
// dialog (a prompt over a picker) never also closes the one beneath it, and the gaming nav's own
// Escape → back (a bubble-phase window listener) doesn't fire a second close.
const stack: Array<() => void> = [];

function onKeydown(e: KeyboardEvent) {
	if (e.key !== 'Escape' || stack.length === 0) return;
	e.preventDefault();
	e.stopImmediatePropagation();
	stack[stack.length - 1]();
}

/** Close on Escape while registered; returns the unregister (an `$effect` / `onDestroy` teardown). */
export function closeOnEscape(close: () => void): () => void {
	const entry = () => close(); // unique per registration, even when two modals share a handler
	if (stack.length === 0) window.addEventListener('keydown', onKeydown, true);
	stack.push(entry);
	return () => {
		const i = stack.indexOf(entry);
		if (i >= 0) stack.splice(i, 1);
		if (stack.length === 0) window.removeEventListener('keydown', onKeydown, true);
	};
}
