#!/bin/bash

set -e
set -o pipefail
set -u
set -x
set -m
export DEBIAN_FRONTEND=noninteractive
export TZ=Etc/Eastern

# Default to "false" if not set
ENSURE_MAP_DATA="${ENSURE_MAP_DATA:-false}"

# Only run the downloader if ENSURE_MAP_DATA is truthy
if [[ "$ENSURE_MAP_DATA" == "true" ]] || [[ "$ENSURE_MAP_DATA" == "1" ]]; then
    echo "[entrypoint] ENSURE_MAP_DATA is enabled. Running map_downloader..."
    cd /app/map_downloader
    /app/map_downloader/map_downloader
else
    echo "[entrypoint] ENSURE_MAP_DATA is disabled. Skipping map_downloader."
fi

cd /app
echo "[entrypoint] Starting groundstation_backend..."
exec /app/groundstation_backend     # exec is important for proper signal handling
