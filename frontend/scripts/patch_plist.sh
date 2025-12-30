#!/usr/bin/env bash
set -euo pipefail

APP_NAME="GroundstationFrontend"

# Try common dx serve output locations (debug)
CANDIDATES=(
  "./dist/${APP_NAME}.app/Info.plist"
)

PLIST=""
for c in "${CANDIDATES[@]}"; do
  if [[ -f "$c" ]]; then PLIST="$c"; break; fi
done


if [[ -z "$PLIST" || ! -f "$PLIST" ]]; then
  echo "Could not find iOS Info.plist. Build the app for ios first"
  exit 1
fi

echo "Patching: $PLIST"

plist_set () {
  local key="$1"
  local type="$2"
  local value="$3"
  /usr/libexec/PlistBuddy -c "Set :${key} ${value}" "$PLIST" 2>/dev/null \
    || /usr/libexec/PlistBuddy -c "Add :${key} ${type} ${value}" "$PLIST"
}

apply_patch () {
  plist_set "NSLocalNetworkUsageDescription" "string" \
    "\"This app connects to devices on your local network to get data from the ground station.\""

  plist_set "NSLocationWhenInUseUsageDescription" "string" \
    "\"This app requires location access to help you locate your rocket.\""
}

# Initial apply
apply_patch