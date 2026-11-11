#!/bin/bash
# Runs INSIDE the image (chroot) from build-image.sh: turns the ubuntu-rockchip server image
# into a Pulsar kiosk. /tmp/pulsar.deb and /tmp/pulsar-rootfs/ are staged by the caller.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
files=/tmp/pulsar-rootfs/files

apt-get update
# Display stack: plain X11 + openbox, no desktop environment. The Linux renderer's proven path
# on this board is X11 (pulsar-render's EGL/DRM_PRIME present). Mesa is the image's panfork
# build; MPP/RGA/rkmpp ffmpeg come from the rockchip-multimedia PPA the image already has.
apt-get install -y --no-install-recommends \
	xserver-xorg-core xserver-xorg-input-libinput xinit x11-xserver-utils openbox dbus-x11 \
	libgl1-mesa-dri libegl1 libgles2 \
	librockchip-mpp1 librga2 ffmpeg \
	gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
	pulseaudio \
	fonts-noto-core fonts-noto-color-emoji \
	python3
# udev rules that give the video group /dev/mpp_service, /dev/rga, /dev/dma_heap (PPA package).
apt-get install -y --no-install-recommends rockchip-multimedia-config || \
	echo "customize: rockchip-multimedia-config not available — check /dev/mpp_service permissions" >&2
# Pulsar itself; apt resolves the .deb's Depends (libwebkit2gtk-4.1, libmpv, …).
apt-get install -y /tmp/pulsar.deb

# The kiosk user: no password (autologin only, not in sudo). Admin access is the image's own
# `ubuntu` account over SSH (cloud-init forces a password change at first login).
if ! id pulsar >/dev/null 2>&1; then
	useradd -m -s /bin/bash pulsar
fi
passwd -l pulsar
# pulsar-vhci (created by the .deb): plug the host's virtual DualSense into vhci-hcd.
for g in audio video render input plugdev pulsar-vhci; do
	getent group "$g" >/dev/null && usermod -aG "$g" pulsar
done

install -D -m 0644 "$files/getty-autologin.conf" /etc/systemd/system/getty@tty1.service.d/autologin.conf
install -D -m 0755 "$files/pulsar-kiosk" /usr/local/bin/pulsar-kiosk
install -D -m 0644 -o pulsar -g pulsar "$files/xinitrc" /home/pulsar/.xinitrc
install -D -m 0644 -o pulsar -g pulsar "$files/bash_profile" /home/pulsar/.bash_profile
install -D -m 0644 -o pulsar -g pulsar "$files/openbox-rc.xml" /home/pulsar/.config/openbox/rc.xml

# Boot straight to the kiosk: text target (no display manager), tty1 autologin starts X.
systemctl set-default multi-user.target
systemctl enable getty@tty1.service

apt-get clean
rm -rf /var/lib/apt/lists/*
echo "customize: done"
