// A LAN beacon may carry no name (a host without a hostname, or an old one that sent none):
// the lists then show a localized generic label instead of a blank row, and a saved peer
// never gets an empty name.

import type { LanDevice } from './api.types';
import { t } from './i18n.svelte';

const PLATFORMS: Record<string, string> = {
	windows: 'Windows',
	linux: 'Linux',
	macos: 'macOS',
	android: 'Android',
	ios: 'iOS'
};

/** The display name of a discovered device: its announced name, else a generic label. */
export function lanName(d: Pick<LanDevice, 'name' | 'platform'>): string {
	const name = d.name?.trim();
	if (name) return name;
	const platform = PLATFORMS[d.platform?.toLowerCase?.() ?? ''];
	return platform ? t('devices.lanUnnamedOn', { platform }) : t('devices.lanUnnamed');
}

/** The list with every empty name replaced by [`lanName`]. */
export function withLanNames(list: LanDevice[]): LanDevice[] {
	return list.map((d) => (d.name?.trim() ? d : { ...d, name: lanName(d) }));
}
