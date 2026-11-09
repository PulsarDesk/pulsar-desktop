#!/usr/bin/env bash
# Build the flashable Pulsar image for the Orange Pi 5B.
#
# Base: ubuntu-rockchip v2.4.0 "Ubuntu 22.04 server" for orangepi-5b (Rockchip 5.10 vendor
# kernel, panfork Mesa, MPP/RGA, rkmpp ffmpeg). The project is archived, but its releases and
# PPAs are still served, and it is the same stack the maintainer's Pi runs today. Armbian's
# vendor 6.1 kernel is the maintained successor, but there the multimedia stack (MPP, RGA,
# ffmpeg-rockchip, GPU userspace) would have to be built from source.
#
# On top: a Pulsar kiosk. Autologin on tty1 → X11 + openbox → Pulsar fullscreen in game mode,
# dark theme, 1080p at 120 Hz when the display offers it. No desktop environment.
#
#   sudo scripts/opi5-image/build-image.sh <Pulsar_..._arm64.deb> [out.img.xz]
#
# Needs root (loop devices, mount, chroot) on an arm64 Linux host (the chroot runs the image's
# arm64 binaries natively), plus: curl xz-utils gdisk cloud-guest-utils (growpart) e2fsprogs
# util-linux (losetup, partx).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
deb="$(realpath "${1:?usage: build-image.sh <pulsar.deb> [out.img.xz]}")"
out="$(realpath -m "${2:-pulsar-opi5b.img.xz}")"

[ "$(id -u)" = 0 ] || { echo "build-image: needs root (loop devices, chroot)" >&2; exit 1; }
[ "$(uname -m)" = aarch64 ] || { echo "build-image: run on arm64 (the chroot runs arm64 binaries)" >&2; exit 1; }
for t in curl xz sgdisk growpart e2fsck resize2fs losetup partx chroot; do
	command -v "$t" >/dev/null || { echo "build-image: missing tool: $t" >&2; exit 1; }
done

BASE_URL="https://github.com/Joshua-Riek/ubuntu-rockchip/releases/download/v2.4.0"
BASE="ubuntu-22.04-preinstalled-server-arm64-orangepi-5b.img"
work="${WORK:-$here/work}"
mkdir -p "$work"
cd "$work"

[ -f "$BASE.xz" ] || curl -fL --retry 3 -o "$BASE.xz" "$BASE_URL/$BASE.xz"
curl -fsSL --retry 3 -o "$BASE.xz.sha256" "$BASE_URL/$BASE.xz.sha256"
sha256sum -c "$BASE.xz.sha256"

rm -f pulsar.img
xz -dc "$BASE.xz" > pulsar.img
# Room for X11, WebKitGTK, GStreamer and Pulsar (the server image's root is nearly full).
truncate -s "+${GROW_GB:-3}G" pulsar.img

loop="$(losetup -fP --show pulsar.img)"
root="$work/root"
cleanup() {
	umount -R "$root" 2>/dev/null || true
	losetup -d "$loop" 2>/dev/null || true
}
trap cleanup EXIT

# The root filesystem is the last partition. Grow it into the added space.
part="$(lsblk -lnpo NAME,TYPE "$loop" | awk '$2 == "part" { p = $1 } END { print p }')"
[ -n "$part" ] || { echo "build-image: no partition found in the base image" >&2; exit 1; }
partnum="${part##*p}"
if [ "$(blkid -p -o value -s PTTYPE "$loop")" = gpt ]; then
	sgdisk -e "$loop" # move the backup GPT header to the new end of the disk
fi
growpart "$loop" "$partnum"
partx -u "$loop" # let the kernel see the grown partition
e2fsck -fy "$part" || [ $? -le 1 ]
resize2fs "$part"

mkdir -p "$root"
mount "$part" "$root"
for d in dev dev/pts proc sys run; do
	mount --bind "/$d" "$root/$d"
done
# Name resolution for apt inside the chroot; the image's own resolv.conf comes back after.
mv "$root/etc/resolv.conf" "$root/etc/resolv.conf.pulsar-orig" 2>/dev/null || true
cp -L /etc/resolv.conf "$root/etc/resolv.conf"

cp "$deb" "$root/tmp/pulsar.deb"
rm -rf "$root/tmp/pulsar-rootfs"
cp -r "$here/rootfs" "$root/tmp/pulsar-rootfs"
chroot "$root" /bin/bash /tmp/pulsar-rootfs/customize.sh

rm -rf "$root/tmp/pulsar.deb" "$root/tmp/pulsar-rootfs"
rm -f "$root/etc/resolv.conf"
mv "$root/etc/resolv.conf.pulsar-orig" "$root/etc/resolv.conf" 2>/dev/null || true

umount -R "$root"
e2fsck -fy "$part" || [ $? -le 1 ]
losetup -d "$loop"
trap - EXIT

xz -T0 -6 -c pulsar.img > "$out"
(cd "$(dirname "$out")" && sha256sum "$(basename "$out")" > "$(basename "$out").sha256")
ls -l "$out"
