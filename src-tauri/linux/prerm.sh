#!/bin/sh
# Pulsar package pre-remove (deb + rpm): drop the LAN-discovery firewall opening postinst.sh
# made. It runs BEFORE the files are deleted, so the ufw app profile
# (/etc/ufw/applications.d/pulsar) and the firewalld service (pulsar.xml) still exist and the
# rule can be resolved by name (in postrm they are gone: `ufw delete allow Pulsar` fails and
# firewalld reloads with a dangling service). Only on a real removal: deb passes "remove"
# (not "upgrade" / "deconfigure" / "failed-upgrade"), rpm passes 0 (1 = upgrade).
# Never fails the removal: every step is best effort.
case "$1" in
remove | 0) ;;
*) exit 0 ;;
esac

if command -v ufw >/dev/null 2>&1; then
	ufw delete allow Pulsar >/dev/null 2>&1 || true
fi
if command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then
	firewall-cmd --permanent --remove-service=pulsar >/dev/null 2>&1 || true
	firewall-cmd --reload >/dev/null 2>&1 || true
fi
exit 0
