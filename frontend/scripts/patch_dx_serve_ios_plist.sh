#!/usr/bin/env bash
set -euo pipefail

APP_NAME="GroundstationFrontend"

# Try common dx serve output locations (debug)
CANDIDATES=(
  "../target/dx/groundstation_frontend/debug/ios/${APP_NAME}.app/Info.plist"
  "../target/dx/groundstation_frontend/Debug/ios/${APP_NAME}.app/Info.plist"
  "../target/dx/groundstation_frontend/release/ios/${APP_NAME}.app/Info.plist"
  "../target/dx/groundstation_frontend/Release/ios/${APP_NAME}.app/Info.plist"
)

PLIST=""
for c in "${CANDIDATES[@]}"; do
  if [[ -f "$c" ]]; then PLIST="$c"; break; fi
done

# Fallback: search
if [[ -z "$PLIST" ]]; then
  PLIST="$(find ../target/dx -path "*ios*.app/Info.plist" -print -quit || true)"
fi


if [[ -z "$PLIST" || ! -f "$PLIST" ]]; then
  echo "Could not find iOS Info.plist under target/dx. Start dx serve first."
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

  plist_set "NSLocationUsageDescription" "string" \
    "\"Shows your position on the GroundStation map.\""
  # If you do Bonjour/mDNS discovery, you probably also need this:
  # (Add only if relevant — leaving it out is fine for direct IP)
  # /usr/libexec/PlistBuddy -c "Delete :NSBonjourServices" "$PLIST" 2>/dev/null || true
  # /usr/libexec/PlistBuddy -c "Add :NSBonjourServices array" "$PLIST" 2>/dev/null || true
  # /usr/libexec/PlistBuddy -c "Add :NSBonjourServices:0 string _http._tcp" "$PLIST" 2>/dev/null || true
}

# Initial apply
apply_patch

# Re-apply whenever the file changes
LAST_MTIME="$(stat -f "%m" "$PLIST")"
while true; do
  sleep 0.25
  if [[ ! -f "$PLIST" ]]; then
    # dx might be rebuilding; wait for it to reappear
    continue
  fi
  MTIME="$(stat -f "%m" "$PLIST")"
  if [[ "$MTIME" != "$LAST_MTIME" ]]; then
    LAST_MTIME="$MTIME"
    apply_patch
    echo "Re-patched at $(date '+%H:%M:%S')"
  fi
done
