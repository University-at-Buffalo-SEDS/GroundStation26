#!/bin/sh
# Acts on a power request written by the GroundStation backend into
# <GS dir>/data/host-power/request ("poweroff" or "reboot"). Runs as root from
# gs-host-power.service; the backend container needs no host privileges.
set -eu
dir="${1:?usage: gs-host-power.sh <host-power dir>}"
request="$dir/request"
[ -f "$request" ] || exit 0
action="$(head -c 16 "$request" | tr -d '[:space:]')"
written="$(stat -c %Y "$request" 2>/dev/null || date +%s)"
# Remove first: a request can never repeat (no reboot loop).
rm -f "$request"
# Ignore anything older than two minutes (e.g. left over across a power cut).
age=$(( $(date +%s) - written ))
if [ "$age" -gt 120 ]; then
    logger -t gs-host-power "ignoring stale request '$action'"
    exit 0
fi
case "$action" in
    poweroff) logger -t gs-host-power "GroundStation requested power off"; exec systemctl poweroff ;;
    reboot) logger -t gs-host-power "GroundStation requested reboot"; exec systemctl reboot ;;
    *) logger -t gs-host-power "ignoring unknown request '$action'"; exit 0 ;;
esac
