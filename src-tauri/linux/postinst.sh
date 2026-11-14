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

# Shared (machine-wide) mode default for a fresh machine, like Windows: ON when nobody decided
# yet (no /var/lib/pulsar/mode.json) and no user has Pulsar data. Untested on Linux hardware.
# >>> pulsar-machine shared-mode default (generated: pulsar_machine::postinst_shared_default)
pulsar_shared_default() {
	if [ -e '/var/lib/pulsar/mode.json' ] || [ -L '/var/lib/pulsar/mode.json' ]; then return 0; fi
	for p in /root/.config /home/*/.config; do
		d="$p/dev.pulsar.app"
		if { [ -f "$d/identity.key" ] || [ -f "$d/config.json" ]; } && [ ! -e "$d/.shared-mode-provisional" ]; then return 0; fi
	done
	G=''
	for g in sudo wheel admin; do
		if getent group "$g" >/dev/null 2>&1; then G=$g; break; fi
	done
	if [ -z "$G" ]; then return 0; fi
	( set -eu
umask 022
M='/var/lib/pulsar'
S="$M/shared"
chk() { if [ -L "$1" ] || { [ -e "$1" ] && [ ! -d "$1" ]; }; then echo "$1: not a plain directory" >&2; exit 3; fi; }
own() { o=$(stat -c %u "$1"); if [ "$o" != 0 ]; then echo "$1: owned by uid $o" >&2; exit 4; fi; }
chk "$M"; mkdir -p "$M"; chk "$M"; own "$M"
chown 0:0 "$M"; chmod 755 "$M"
chk "$S"; mkdir -p "$S"; chk "$S"; own "$S"
chown 0:"$G" "$S"; chmod 2770 "$S"
find "$S" -mindepth 1 -maxdepth 1 -type f -exec chgrp "$G" {} + -exec chmod 660 {} +
T="$M/mode.json.tmp"; rm -f "$T"
printf '{"shared":true}\n' > "$T"; chmod 644 "$T"; mv -f "$T" "$M/mode.json"
) && echo "Pulsar: shared (machine-wide) mode is on for this new install (group $G)."
}
pulsar_shared_default || true
# <<< pulsar-machine shared-mode default

cat <<'MSG'
Pulsar: to let a user host a virtual DualSense, add them to the pulsar-vhci group and log
in again:  sudo usermod -aG pulsar-vhci "$USER"
MSG
exit 0
