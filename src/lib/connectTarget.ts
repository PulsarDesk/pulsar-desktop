// Shared connect-target parsing/validation. A target is either a 9-digit relay ID
// (grouped in threes for display) or an address: IP / IP:port / host / host:port
// (has a '.', ':' or a letter). Extracted
// from Home.svelte so the remote connect screen and the gaming-mode home share one
// source of truth (and so the on-screen numpad edits the same canonical form).

/** A target carrying a '.', ':' or a letter is an address (IP / host, optional
 * :port); otherwise a relay ID. */
export const isAddr = (v: string): boolean => /[.:a-z]/i.test(v);

/** Canonical input form: addresses keep only hostname/IP characters (max 261
 * chars = 253-char DNS name + ":65535"); relay IDs are stripped to digits, capped
 * at 9, and grouped in threes ("641 724 395"). */
export const fmtTarget = (v: string): string =>
	isAddr(v)
		? v.replace(/[^0-9a-z.:-]/gi, '').slice(0, 261)
		: v
				.replace(/\D/g, '')
				.slice(0, 9)
				.replace(/(\d{3})(?=\d)/g, '$1 ')
				.trim();

const ipRe = /^\d{1,3}(\.\d{1,3}){3}(:\d{1,5})?$/;
// DNS name with at least one dot (or `localhost`), optional :port. All-numeric
// labels are left to ipRe so a half-typed IP ("192.168.1") is not accepted.
const label = '[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?';
const hostRe = new RegExp(`^(localhost|${label}(\\.${label})+)(:\\d{1,5})?$`, 'i');

/** Whether `v` is a connectable target: a full IPv4 or hostname (optionally :port),
 * or at least 6 ID digits (a partial relay ID the relay can still resolve). */
export const canConnectTarget = (v: string): boolean => {
	if (!isAddr(v)) return v.replace(/\D/g, '').length >= 6;
	const t = v.trim();
	if (ipRe.test(t)) return true;
	return /[a-z]/i.test(t) && hostRe.test(t);
};

/** Write the canonical form back into the input and keep the caret after the same
 * number of significant characters. Needed because deleting a group space yields the
 * same formatted string, so a plain state update leaves the DOM showing "517072 042". */
export function reformatInput(el: HTMLInputElement): string {
	const raw = el.value;
	const caret = el.selectionStart ?? raw.length;
	const sig = raw.slice(0, caret).replace(/\s/g, '').length;
	const out = fmtTarget(raw);
	el.value = out;
	let pos = 0;
	for (let n = 0; pos < out.length && n < sig; pos++) if (out[pos] !== ' ') n++;
	el.setSelectionRange(pos, pos);
	return out;
}

/** Group spaces of a relay ID are not deletable: Backspace/Delete next to one only
 * moves the caret over it. */
export function guardIdSpaces(e: KeyboardEvent & { currentTarget: HTMLInputElement }): void {
	const el = e.currentTarget;
	const v = el.value;
	const s = el.selectionStart;
	if (s === null || s !== el.selectionEnd || isAddr(v)) return;
	if (e.key === 'Backspace' && v[s - 1] === ' ') {
		e.preventDefault();
		el.setSelectionRange(s - 1, s - 1);
	} else if (e.key === 'Delete' && v[s] === ' ') {
		e.preventDefault();
		el.setSelectionRange(s + 1, s + 1);
	}
}
