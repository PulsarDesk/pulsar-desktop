#!/bin/sh
# Pulsar package post-remove (deb + rpm): drop the LAN-discovery firewall opening postinst.sh
# made. Only on a real removal — deb passes "remove"/"purge", rpm passes 0 (1 = upgrade).
# Never fails the removal: every step is best effort.
case "$1" in
remove | purge | 0) ;;
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
