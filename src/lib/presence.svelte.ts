// Peer presence for the recents / devices dots. There is no relay presence query yet, so a
// peer is only KNOWN to be reachable when it is seen on this LAN (discovery beacons) or a
// session to it is live right now. Anything else is "unknown" — never "offline": a peer
// reached over the internet would otherwise always read as offline, even mid-session.
import { normalizeId } from '#lib/peers.svelte.js';

const live = $state<{ ids: Set<string> }>({ ids: new Set() });

/** The hosts with an ACTIVE session right now (set by the session manager's owner). */
export function setLivePeers(ids: string[]) {
	live.ids = new Set(ids.map((id) => normalizeId(id)));
}

/** A session to `id` is live right now. */
export function isLive(id: string): boolean {
	return live.ids.has(normalizeId(id));
}
