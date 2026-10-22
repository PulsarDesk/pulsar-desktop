#!/usr/bin/env bash
# AppImage smoke test: run `Pulsar.AppImage --selftest` inside a clean distro container.
#
#   scripts/appimage/smoke.sh <path/to/Pulsar.AppImage> <image> [<image>…]
#   e.g. scripts/appimage/smoke.sh Pulsar_*.AppImage fedora:41 ubuntu:24.04 archlinux:latest
#
# The container first gets only what linuxdeploy's excludelist leaves to the host (X11,
# EGL/GL, gbm, drm, wayland, ALSA, fonts, …) — a desktop that never installed libva or JACK,
# so the bundled copies must carry it. Then libva + libjack are added and it runs again: now
# main.rs must swap in the host's libva (selftest's `libva` line). Any FAIL fails the image.
set -euo pipefail

appimage="$(realpath "$1")"
shift
rc=0
for image in "$@"; do
	echo "::group::smoke $image"
	if docker run --rm -i -v "$appimage:/Pulsar.AppImage:ro" -e APPIMAGE_EXTRACT_AND_RUN=1 \
		"$image" bash -s <<'EOF'
set -euo pipefail
. /etc/os-release
case " $ID ${ID_LIKE:-} " in
*" debian "* | *" ubuntu "*)
	export DEBIAN_FRONTEND=noninteractive
	apt-get update -qq
	t64() { if apt-cache show "${1}t64" >/dev/null 2>&1; then echo "${1}t64"; else echo "$1"; fi; }
	base="libx11-6 libx11-xcb1 libxcb1 libegl1 libgl1 libgbm1 libdrm2 libwayland-client0
		$(t64 libasound2) $(t64 libpipewire-0.3-0)
		libfontconfig1 libfreetype6 libharfbuzz0b libfribidi0 libexpat1 zlib1g libusb-1.0-0
		libgmp10 libgpg-error0 libcom-err2"
	extra="libva2 libva-drm2 libva-x11-2 libva-wayland2 libjack-jackd2-0"
	inst() { apt-get install -y -qq --no-install-recommends "$@" >/dev/null; } ;;
*" fedora "*)
	base="libX11 libX11-xcb libxcb mesa-libEGL mesa-libGL mesa-libgbm libdrm libwayland-client
		alsa-lib pipewire-libs fontconfig freetype harfbuzz fribidi expat zlib libusb1 gmp
		libgpg-error krb5-libs"
	extra="libva pipewire-jack-audio-connection-kit-libs"
	inst() { dnf install -y -q --setopt=install_weak_deps=False "$@" >/dev/null; } ;;
*" arch "*)
	pacman -Sy --noconfirm >/dev/null
	base="libx11 libxcb mesa libdrm wayland alsa-lib libpipewire fontconfig freetype2 harfbuzz
		fribidi expat zlib libusb gmp libgpg-error krb5"
	extra="libva pipewire-jack"
	inst() { pacman -S --noconfirm --needed "$@" >/dev/null; } ;;
*" suse "* | *" opensuse "*)
	base="libX11-6 libX11-xcb1 libxcb1 Mesa-libEGL1 Mesa-libGL1 libgbm1 libdrm2
		libwayland-client0 libasound2 libpipewire-0_3-0 libfontconfig1 libfreetype6 libharfbuzz0
		libfribidi0 libexpat1 libz1 libusb-1_0-0 libgmp10 libgpg-error0 krb5"
	extra="libva2 libva-drm2 libva-x11-2 libva-wayland2 pipewire-libjack-0_3"
	inst() { zypper -q -n install --no-recommends "$@" >/dev/null; } ;;
*)
	echo "FAIL unsupported distro: $ID"
	exit 1 ;;
esac
# shellcheck disable=SC2086
inst $base
cd /tmp
check() {
	echo "--- $1"
	out="$(/Pulsar.AppImage --selftest 2>&1)" || true
	echo "$out"
	! grep -q '^FAIL\|error while loading shared libraries' <<<"$out"
}
check "$PRETTY_NAME, no libva/libjack (bundled copies)"
# shellcheck disable=SC2086
inst $extra
check "$PRETTY_NAME, host libva/libjack"
EOF
	then
		echo "::endgroup::"
		echo "smoke $image: ok"
	else
		echo "::endgroup::"
		echo "::error::AppImage smoke test failed on $image"
		rc=1
	fi
done
exit $rc
