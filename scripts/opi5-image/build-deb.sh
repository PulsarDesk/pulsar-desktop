#!/usr/bin/env bash
# Build the Pulsar .deb that goes into the Orange Pi 5B image.
#
# It must be built on arm64 Ubuntu 22.04 with the Rockchip multimedia PPA, the same userland
# the image ships: pulsar-render then links that PPA's libavcodec58, which carries the rkmpp
# hardware decoders. The generic linux-arm64 release links Ubuntu's own libavcodec58 (no
# rkmpp), which is why the image gets its own build.
#
#   INSTALL_DEPS=1 scripts/opi5-image/build-deb.sh   # fresh container/VM: install everything
#   scripts/opi5-image/build-deb.sh                  # deps already present (e.g. on the Pi)
#
# Output: target/release/bundle/deb/Pulsar_<version>_arm64.deb
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"

[ "$(uname -m)" = aarch64 ] || { echo "build-deb: run this on arm64" >&2; exit 1; }
. /etc/os-release
[ "${VERSION_ID:-}" = "22.04" ] || echo "build-deb: warning: expected Ubuntu 22.04, got ${VERSION_ID:-?}" >&2

if [ "${INSTALL_DEPS:-0}" = 1 ]; then
	export DEBIAN_FRONTEND=noninteractive
	sudo=""
	[ "$(id -u)" = 0 ] || sudo="sudo"
	$sudo apt-get update
	$sudo apt-get install -y --no-install-recommends software-properties-common gpg-agent \
		ca-certificates curl git unzip xz-utils file build-essential cmake pkg-config
	# rkmpp-enabled ffmpeg, MPP and RGA. Frozen since the upstream project was archived
	# (2024/2026), but still served by Launchpad and identical to what the image uses.
	$sudo add-apt-repository -y ppa:jjriek/rockchip-multimedia
	$sudo apt-get update
	# Same build deps as the release workflow's Linux job, minus the cross-distro AppImage bits.
	$sudo apt-get install -y --no-install-recommends \
		libwebkit2gtk-4.1-dev libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev \
		patchelf libudev-dev libmpv-dev libxkbcommon-dev \
		ffmpeg libavcodec-dev libavformat-dev libavutil-dev libswscale-dev \
		libavdevice-dev libavfilter-dev \
		libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
		libegl1-mesa-dev libx11-dev libasound2-dev libpulse-dev \
		libxext-dev libxrandr-dev libxcursor-dev libxi-dev libxfixes-dev libxss-dev libxtst-dev \
		libgl1-mesa-dev libgles2-mesa-dev libdrm-dev libgbm-dev libwayland-dev libdbus-1-dev \
		librockchip-mpp-dev librga-dev
	command -v cargo >/dev/null || {
		curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal
	}
	command -v bun >/dev/null || [ -x "$HOME/.bun/bin/bun" ] || {
		curl -fsSL https://bun.sh/install | bash
	}
fi
export PATH="$HOME/.cargo/bin:$HOME/.bun/bin:$PATH"

# The whole point of this build: the libavcodec it links must have the rkmpp decoders.
if ! ffmpeg -hide_banner -decoders 2>/dev/null | grep -q rkmpp; then
	echo "build-deb: this ffmpeg has no rkmpp decoders — is the rockchip-multimedia PPA installed?" >&2
	exit 1
fi

cd "$repo"
bun install --frozen-lockfile
cargo build --release -p pulsar-render
# The bundle ships the renderer and an ffmpeg next to the app (tauri.linux.conf.json).
mkdir -p src-tauri/resources
cp target/release/pulsar-render src-tauri/resources/pulsar-render
cp "$(command -v ffmpeg)" src-tauri/resources/ffmpeg
bun run tauri build --bundles deb
ls -l target/release/bundle/deb/*.deb
