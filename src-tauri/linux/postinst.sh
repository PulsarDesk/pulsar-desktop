#!/bin/sh
# Pulsar package post-install (deb + rpm): DualSense passthrough setup on Linux.
# Never fails the install: every step is best effort.

# Group allowed to plug Pulsar's virtual DualSense in (vhci-hcd sysfs attach).
getent group pulsar-vhci >/dev/null 2>&1 || groupadd --system pulsar-vhci 2>/dev/null || true

# Load vhci-hcd now (also at every boot via /usr/lib/modules-load.d/pulsar.conf).
modprobe vhci-hcd 2>/dev/null || true

if command -v udevadm >/dev/null 2>&1; then
	udevadm control --reload-rules 2>/dev/null || true
	# Re-apply to pads and vhci-hcd that are already present.
	udevadm trigger --subsystem-match=hidraw --action=change 2>/dev/null || true
	udevadm trigger --subsystem-match=platform --sysname-match='vhci_hcd.*' --action=add 2>/dev/null || true
fi

cat <<'MSG'
Pulsar: to let a user host a virtual DualSense, add them to the pulsar-vhci group and log
in again:  sudo usermod -aG pulsar-vhci "$USER"
MSG
exit 0
