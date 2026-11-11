// Fetch the Windows driver payloads Pulsar bundles + auto-installs:
//   • Interception   (keyboard capture under ASTER) — interception.dll + installer
//   • ViGEmBus       (virtual Xbox gamepad on the host)        — installer
//   • usbip-win2     (virtual wired DualSense on the host: a USB/IP client + vhci
//     driver that attaches pulsar-core's loopback USB/IP server) — installer for the
//     TARGET arch only (x64 or arm64; kernel drivers must match the native arch)
//   • Virtual-Audio-Driver (sinkless render endpoint we redirect to so the host
//     can stream its audio while staying SILENT, Sunshine-style) — signed
//     inf/sys/cat, plus nefconw to create the device node + install the driver
//
// Mirrors scripts/fetch-ffmpeg.mjs: run once before `tauri build` (CI runs it
// automatically) so a dev clone needs no manual driver download. Files land in
// src-tauri/resources/ (git-ignored, except the small committed interception.dll
// fallback). No-op on non-Windows hosts.
//
// Licensing: Interception is LGPL-3.0 for non-commercial use with explicit
// redistribution rights for the driver + installer (fine for GPLv3 Pulsar).
// ViGEmBus is BSD-3-Clause. Virtual-Audio-Driver is MIT + MS-PL (signed by the
// SignPath Foundation); nefcon (NefCon) is BSD-3-Clause. usbip-win2 is GPL-3.0
// (same license as Pulsar; signed Inno Setup installers). All redistributable.
//
// Usage: bun scripts/fetch-drivers.mjs [--arch x64|arm64]
//   --arch picks the per-arch payloads (usbip-win2); default = this machine's arch.
//   CI passes the matrix's `--os windows --arch arm64` for the cross-built ARM64 job.
// See desktop-app/CLAUDE.md → "Windows drivers".

import { mkdir, writeFile, access, rm, readdir, copyFile, readFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawn } from 'node:child_process';
import { tmpdir } from 'node:os';

const here = dirname(fileURLToPath(import.meta.url));
const resDir = join(here, '..', 'src-tauri', 'resources');
const interceptionDir = join(resDir, 'interception');
const vadDir = join(resDir, 'virtual-audio-driver');
const nefconDir = join(resDir, 'nefcon');
const usbipDir = join(resDir, 'usbip');

// Pinned release assets (bump deliberately; all are signed / from signed sources).
const INTERCEPTION_ZIP =
	'https://github.com/oblitum/Interception/releases/download/v1.0.1/Interception.zip';
const VIGEMBUS_EXE =
	'https://github.com/ViGEm/ViGEmBus/releases/download/v1.22.0/ViGEmBus_1.22.0_x64_x86_arm64.exe';
// Virtual-Audio-Driver: latest SIGNED release (SignPath Foundation cert).
// Zip layout: "Virtual Audio Driver/{VirtualAudioDriver.inf,VirtualAudioDriver.sys,virtualaudiodriver.cat}"
// (filenames bundled verbatim so the signed .cat keeps matching the .sys/.inf).
const VAD_ZIP =
	'https://github.com/VirtualDrivers/Virtual-Audio-Driver/releases/download/25.7.14/Virtual.Audio.Driver.Signed.-.25.7.14.zip';
// nefcon (NefCon) — creates the ROOT\VirtualAudioDriver device node + installs the
// INF silently. Zip layout: "{x64,ARM64,x86}/nefconw.exe".
const NEFCON_ZIP =
	'https://github.com/nefarius/nefcon/releases/download/v1.17.40/nefcon_v1.17.40.zip';

// usbip-win2 (github.com/vadimgrn/usbip-win2): signed Inno Setup installers, one per
// arch. Pinned by version + the sha256 GitHub publishes for each release asset. The NSIS
// hook (installer-hooks.nsh) and pulsar-setup compare USBIP_VERSION against the installed
// one, so bump the three together (fetch-drivers.mjs, installer-hooks.nsh USBIP_VERSION,
// crates/pulsar-setup/src/drivers.rs USBIP_VERSION).
const USBIP_VERSION = '0.9.8.1';
const USBIP = {
	x64: {
		url: `https://github.com/vadimgrn/usbip-win2/releases/download/v.${USBIP_VERSION}/USBip-${USBIP_VERSION}-x64.exe`,
		sha256: '38cad6d4432b52d5bb9409d9ad03b72fdffc4ada4cd3a48fbeca1a2752a8518a'
	},
	arm64: {
		url: `https://github.com/vadimgrn/usbip-win2/releases/download/v.${USBIP_VERSION}/USBip-${USBIP_VERSION}-arm64.exe`,
		sha256: 'cab7ff97f79275eeb5c5b8bb2eb3111ff6b6c4eead07d9bec9be5bd1e3a35800'
	}
};

function targetArch() {
	const i = process.argv.indexOf('--arch');
	const a = i >= 0 ? process.argv[i + 1] : process.arch;
	if (a === 'arm64' || a === 'aarch64') return 'arm64';
	if (a === 'x64' || a === 'x86_64' || a === 'amd64') return 'x64';
	throw new Error(`unsupported --arch ${a} (x64 | arm64)`);
}

async function sha256(p) {
	return createHash('sha256').update(await readFile(p)).digest('hex');
}

async function exists(p) {
	try {
		await access(p);
		return true;
	} catch {
		return false;
	}
}

async function download(url, dest) {
	process.stdout.write(`fetch ${url}\n`);
	const res = await fetch(url, { redirect: 'follow' });
	if (!res.ok) throw new Error(`${res.status} ${res.statusText} for ${url}`);
	const buf = Buffer.from(await res.arrayBuffer());
	await mkdir(dirname(dest), { recursive: true });
	await writeFile(dest, buf);
	process.stdout.write(`  -> ${dest} (${buf.length} bytes)\n`);
}

// Some shells (e.g. Steam) leak env vars containing NUL bytes, which node's
// spawn rejects. Build a clean copy that drops any such values (mirrors
// scripts/fetch-ffmpeg.mjs).
function cleanEnv() {
	const out = {};
	for (const [k, v] of Object.entries(process.env)) {
		if (typeof v === 'string' && !v.includes('\x00')) out[k] = v;
	}
	return out;
}

function run(cmd, args) {
	return new Promise((resolve, reject) => {
		const p = spawn(cmd, args, { stdio: 'inherit', env: cleanEnv() });
		p.on('error', reject);
		p.on('exit', (code) =>
			code === 0 ? resolve() : reject(new Error(`${cmd} exited ${code}`))
		);
	});
}

// Extract a .zip into destDir. We're Windows-only here, so PowerShell's
// Expand-Archive (always present on Win10/11) is the dependency-free choice.
async function unzip(zip, destDir) {
	await mkdir(destDir, { recursive: true });
	await run('powershell', [
		'-NoProfile',
		'-NonInteractive',
		'-Command',
		`Expand-Archive -LiteralPath '${zip}' -DestinationPath '${destDir}' -Force`
	]);
}

// Pull the Virtual-Audio-Driver signed inf/sys/cat into resources/virtual-audio-driver/.
// The NSIS hook + the redirect code expect a FLAT folder of those three files.
async function fetchVirtualAudioDriver() {
	const inf = join(vadDir, 'VirtualAudioDriver.inf');
	if (await exists(inf)) return; // idempotent: already extracted.

	const work = join(tmpdir(), `pulsar-vad-${Date.now()}`);
	await mkdir(work, { recursive: true });
	try {
		const zip = join(work, 'vad.zip');
		await download(VAD_ZIP, zip);
		await unzip(zip, work);

		// Zip nests the payload under "Virtual Audio Driver/"; flatten it.
		const inner = join(work, 'Virtual Audio Driver');
		const srcDir = (await exists(inner)) ? inner : work;
		await mkdir(vadDir, { recursive: true });
		for (const f of await readdir(srcDir)) {
			if (/\.(inf|sys|cat)$/i.test(f)) {
				await copyFile(join(srcDir, f), join(vadDir, f));
				process.stdout.write(`  -> ${join(vadDir, f)}\n`);
			}
		}
		if (!(await exists(inf)))
			throw new Error('VirtualAudioDriver.inf not found in extracted zip');
	} finally {
		await rm(work, { recursive: true, force: true });
	}
}

// Pull nefconw.exe for x64 + ARM64 into resources/nefcon/{x64,arm64}/nefconw.exe.
async function fetchNefcon() {
	const x64 = join(nefconDir, 'x64', 'nefconw.exe');
	const arm64 = join(nefconDir, 'arm64', 'nefconw.exe');
	if ((await exists(x64)) && (await exists(arm64))) return; // idempotent.

	const work = join(tmpdir(), `pulsar-nefcon-${Date.now()}`);
	await mkdir(work, { recursive: true });
	try {
		const zip = join(work, 'nefcon.zip');
		await download(NEFCON_ZIP, zip);
		await unzip(zip, work);
		for (const [arch, dest] of [
			['x64', x64],
			['ARM64', arm64]
		]) {
			const src = join(work, arch, 'nefconw.exe');
			if (!(await exists(src)))
				throw new Error(`nefconw.exe not found for ${arch} in extracted zip`);
			await mkdir(dirname(dest), { recursive: true });
			await copyFile(src, dest);
			process.stdout.write(`  -> ${dest}\n`);
		}
	} finally {
		await rm(work, { recursive: true, force: true });
	}
}

// Pull the usbip-win2 installer for the target arch into
// resources/usbip/USBip-<arch>.exe (tauri.windows.conf.json bundles resources/usbip/*).
// Exactly one file lives there: the other arch's installer is removed so a local
// arch switch never ships both. A file whose hash does not match is re-downloaded.
async function fetchUsbip(arch) {
	const { url, sha256: want } = USBIP[arch];
	const dest = join(usbipDir, `USBip-${arch}.exe`);
	await mkdir(usbipDir, { recursive: true });
	for (const f of await readdir(usbipDir)) {
		if (f !== `USBip-${arch}.exe`) await rm(join(usbipDir, f), { force: true });
	}
	if ((await exists(dest)) && (await sha256(dest)) === want) {
		process.stdout.write(`usbip-win2 ${USBIP_VERSION} ${arch}: present, sha256 ok
`);
		return;
	}
	await download(url, dest);
	const got = await sha256(dest);
	if (got !== want) {
		await rm(dest, { force: true });
		throw new Error(`usbip-win2 ${arch} sha256 mismatch: got ${got}, want ${want}`);
	}
	process.stdout.write(`  sha256 ok (${got})
`);
}

async function main() {
	if (process.platform !== 'win32') {
		process.stdout.write('fetch-drivers: non-Windows host — skipping.\n');
		return;
	}
	await mkdir(interceptionDir, { recursive: true });

	// interception.dll ships committed as a fallback; only fetch the full release
	// (which carries the command-line installer the NSIS hook runs) if missing.
	const interceptionZip = join(interceptionDir, 'Interception.zip');
	if (!(await exists(interceptionZip))) {
		await download(INTERCEPTION_ZIP, interceptionZip);
		process.stdout.write(
			'  note: extract command-line-installer\\install-interception.exe + library\\x64\\interception.dll\n'
		);
	}

	const vigem = join(resDir, 'ViGEmBus_Setup.exe');
	if (!(await exists(vigem))) {
		await download(VIGEMBUS_EXE, vigem);
	}

	// Virtual sink for Sunshine-style host-silent audio (redirect default endpoint
	// here, capture ITS loopback, restore on teardown — never mute the real one).
	await fetchVirtualAudioDriver();
	await fetchNefcon();

	// Virtual wired DualSense on the host (pulsar_core::input::ds5::attach).
	await fetchUsbip(targetArch());

	process.stdout.write('fetch-drivers: done.\n');
}

main().catch((e) => {
	console.error('fetch-drivers failed:', e.message);
	process.exit(1);
});
