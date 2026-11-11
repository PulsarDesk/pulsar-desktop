// A connect flow that runs OUTSIDE the session manager (the Devices dialog's Pair /
// Unpair) can still raise the host's password prompt (`auth-prompt`). The page owns that
// prompt queue; it registers here so such a flow can close its prompt once the host has
// answered (e.g. the host user clicked Allow while the prompt was still open).

let handler: ((target: string) => void) | null = null;

/** The page registers its `closePwFor`. Returns the unregister function. */
export function setAuthDoneHandler(h: (target: string) => void): () => void {
	handler = h;
	return () => {
		if (handler === h) handler = null;
	};
}

/** The auth for `target` is over (accepted, refused or failed): drop its open prompt. */
export function authDone(target: string) {
	handler?.(target);
}
