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

# Firewall: LAN discovery ("on your network") is the one unsolicited inbound flow — a UDP
# multicast beacon on 21117. Sessions are hole-punched (we send first), so a stateful firewall
# already admits them. Open 21117/udp only where a firewall is actually ACTIVE (the Windows
# installer does the same with its allow rules); ufw/firewalld definitions ship in
# /etc/ufw/applications.d/pulsar and /usr/lib/firewalld/services/pulsar.xml.
# Only on the FIRST install (deb: "configure" with no previously configured version in $2;
# rpm: $1 = 1), so an update never brings back a rule the user removed on purpose.
first_install=0
case "$1" in
configure) [ -z "$2" ] && first_install=1 ;;
1) first_install=1 ;;
esac
if [ "$first_install" = 1 ]; then
	if command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q '^Status: active'; then
		ufw allow Pulsar >/dev/null 2>&1 || true
	fi
	if command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then
		firewall-cmd --reload >/dev/null 2>&1 || true # pick up the new service definition
		firewall-cmd --permanent --add-service=pulsar >/dev/null 2>&1 || true
		firewall-cmd --reload >/dev/null 2>&1 || true
	fi
fi

cat <<'MSG'
Pulsar: to let a user host a virtual DualSense, add them to the pulsar-vhci group and log
in again:  sudo usermod -aG pulsar-vhci "$USER"
MSG
exit 0
