// Build Tauri's dynamic-JSON update manifest from a release's assets.
// CLI:  node gen-update-manifest.mjs <assetsDir> <version> <base-download-url> <out.json> [notes] [profile]
// profile: "default" (every desktop platform) or "opi5" (the Orange Pi 5 .deb only — its own
// channel, so a board running the rkmpp build never updates to the generic arm64 bundles).
import { readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

// Match each Tauri target key to the regex that identifies its updater artifact
// (the bundle, NOT its .sig). createUpdaterArtifacts emits these names.
const TARGETS = [
	{ key: 'windows-x86_64', re: /_x64-setup\.exe$/ },
	{ key: 'windows-aarch64', re: /_arm64-setup\.exe$/ },
	{ key: 'linux-x86_64', re: /_amd64\.AppImage$/ },
	{ key: 'linux-aarch64', re: /_aarch64\.AppImage$/ },
	{ key: 'darwin-aarch64', re: /aarch64\.app\.tar\.gz$/ },
	{ key: 'darwin-x86_64', re: /x64\.app\.tar\.gz$/ }
];

// The Orange Pi 5 build: a .deb linked against the Rockchip (rkmpp) ffmpeg. The updater of a
// .deb install asks for `linux-aarch64-deb` first, then `linux-aarch64`; both point at it.
const OPI5_TARGETS = [
	{ key: 'linux-aarch64-deb', re: /_opi5_arm64\.deb$/ },
	{ key: 'linux-aarch64', re: /_opi5_arm64\.deb$/ }
];

// assets: { name -> sigContents|null }. Pure; unit-tested.
export function buildManifest({ version, notes, pubDate, assets, base, profile = 'default' }) {
	const names = Object.keys(assets);
	const platforms = {};
	for (const { key, re } of profile === 'opi5' ? OPI5_TARGETS : TARGETS) {
		const bundle = names.find((n) => re.test(n) && !n.endsWith('.sig'));
		if (!bundle) continue; // target not built this run
		const sig = assets[bundle + '.sig'];
		if (sig == null) throw new Error(`missing signature for ${bundle}`);
		platforms[key] = { url: `${base}/${bundle}`, signature: sig };
	}
	return { version, notes, pub_date: pubDate, platforms };
}

function main() {
	const [assetsDir, version, base, out, notes = '', profile = 'default'] = process.argv.slice(2);
	if (!assetsDir || !version || !base || !out) {
		console.error('usage: gen-update-manifest.mjs <assetsDir> <version> <base-url> <out.json> [notes]');
		process.exit(2);
	}
	const files = readdirSync(assetsDir, { recursive: true }).filter((f) => typeof f === 'string');
	const assets = {};
	for (const f of files) {
		const name = f.split(/[\\/]/).pop();
		assets[name] = name.endsWith('.sig') ? readFileSync(join(assetsDir, f), 'utf8').trim() : null;
	}
	const manifest = buildManifest({
		version,
		notes,
		pubDate: new Date().toISOString(),
		assets,
		base,
		profile
	});
	writeFileSync(out, JSON.stringify(manifest, null, 2));
	console.log(`wrote ${out} with platforms: ${Object.keys(manifest.platforms).join(', ')}`);
}

// Run main() only when invoked directly (not when imported by the test).
if (import.meta.url === `file://${process.argv[1]}`) main();
