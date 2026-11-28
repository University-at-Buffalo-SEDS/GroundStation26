#!/bin/sh
# One-time host setup for the admin-only "Power off" / "Reboot" buttons.
# Run as root on the station host (the Pi), from anywhere:
#   sudo deploy/host-power/install.sh /path/to/GroundStation26
# The GS directory is the one whose ./data is mounted at /app/data in Docker
# (or the working directory of a native systemd service).
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
gs_dir="$(cd "${1:-$here/../..}" && pwd)"
[ "$(id -u)" -eq 0 ] || { echo "run as root (sudo)" >&2; exit 1; }
install -m 0755 "$here/gs-host-power.sh" /usr/local/sbin/gs-host-power.sh
for unit in gs-host-power.path gs-host-power.service; do
    sed "s|@GS_DIR@|$gs_dir|g" "$here/$unit" > "/etc/systemd/system/$unit"
done
install -d -m 0755 "$gs_dir/data/host-power"
rm -f "$gs_dir/data/host-power/request"
echo "installed $(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$gs_dir/data/host-power/helper-installed"
systemctl daemon-reload
systemctl enable --now gs-host-power.path
echo "GroundStation host power helper installed for $gs_dir"
