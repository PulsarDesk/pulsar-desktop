import { test } from 'node:test';
import assert from 'node:assert/strict';
import { buildManifest } from './gen-update-manifest.mjs';

test('maps each target to its asset url + signature', () => {
	const assets = {
		'Pulsar_0.1.0-dev.11_x64-setup.exe': null,
		'Pulsar_0.1.0-dev.11_x64-setup.exe.sig': 'SIG_WIN',
		'Pulsar_0.1.0-dev.11_amd64.AppImage': null,
		'Pulsar_0.1.0-dev.11_amd64.AppImage.sig': 'SIG_LINUX_X64',
		'Pulsar_0.1.0-dev.11_aarch64.AppImage': null,
		'Pulsar_0.1.0-dev.11_aarch64.AppImage.sig': 'SIG_LINUX_ARM',
		'Pulsar_aarch64.app.tar.gz': null,
		'Pulsar_aarch64.app.tar.gz.sig': 'SIG_MAC_ARM',
		'Pulsar_x64.app.tar.gz': null,
		'Pulsar_x64.app.tar.gz.sig': 'SIG_MAC_X64'
	};
	const base = 'https://github.com/PulsarDesk/pulsar/releases/download/v0.1.0-dev.11';
	const m = buildManifest({
		version: '0.1.0-dev.11',
		notes: 'n',
		pubDate: '2026-06-14T00:00:00Z',
		assets,
		base
	});

	assert.equal(m.version, '0.1.0-dev.11');
	assert.equal(m.platforms['windows-x86_64'].signature, 'SIG_WIN');
	assert.equal(m.platforms['windows-x86_64'].url, base + '/Pulsar_0.1.0-dev.11_x64-setup.exe');
	assert.equal(m.platforms['linux-x86_64'].signature, 'SIG_LINUX_X64');
	assert.equal(m.platforms['linux-aarch64'].url, base + '/Pulsar_0.1.0-dev.11_aarch64.AppImage');
	assert.equal(m.platforms['darwin-aarch64'].signature, 'SIG_MAC_ARM');
	assert.equal(m.platforms['darwin-x86_64'].url, base + '/Pulsar_x64.app.tar.gz');
});

test('the opi5 profile points a .deb install at the Orange Pi 5 build only', () => {
	const assets = {
		'Pulsar_0.17.0_arm64.deb': null,
		'Pulsar_0.17.0_arm64.deb.sig': 'SIG_GENERIC_DEB',
		'Pulsar_0.17.0_aarch64.AppImage': null,
		'Pulsar_0.17.0_aarch64.AppImage.sig': 'SIG_APPIMAGE',
		'Pulsar_0.17.0_opi5_arm64.deb': null,
		'Pulsar_0.17.0_opi5_arm64.deb.sig': 'SIG_OPI5'
	};
	const base = 'https://github.com/PulsarDesk/pulsar-desktop/releases/download/v0.17.0';
	const m = buildManifest({ version: '0.17.0', notes: 'n', pubDate: 'x', assets, base, profile: 'opi5' });
	assert.deepEqual(Object.keys(m.platforms).sort(), ['linux-aarch64', 'linux-aarch64-deb']);
	assert.equal(m.platforms['linux-aarch64-deb'].url, base + '/Pulsar_0.17.0_opi5_arm64.deb');
	assert.equal(m.platforms['linux-aarch64-deb'].signature, 'SIG_OPI5');
	assert.equal(m.platforms['linux-aarch64'].url, base + '/Pulsar_0.17.0_opi5_arm64.deb');
	// The default manifest never offers the rkmpp build to other arm64 installs.
	const def = buildManifest({ version: '0.17.0', notes: 'n', pubDate: 'x', assets, base });
	assert.equal(def.platforms['linux-aarch64'].url, base + '/Pulsar_0.17.0_aarch64.AppImage');
	assert.equal(def.platforms['linux-aarch64-deb'], undefined);
});
