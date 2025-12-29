#!/usr/bin/env bash
set -euo pipefail

APP_NAME="GroundstationFrontend"

# ✅ Set these to what you want
BUNDLE_ID="com.rylanmeilutis.groundstation26"
BUNDLE_NAME="GroundStation26"
BUNDLE_DISPLAY_NAME="GroundStation 26"

# Try common dx serve output locations (debug)
CANDIDATES=(
  "../target/dx/groundstation_frontend/debug/macos/${APP_NAME}.app/Contents/Info.plist"
  "../target/dx/groundstation_frontend/Debug/macos/${APP_NAME}.app/Contents/Info.plist"
  "../target/dx/groundstation_frontend/release/macos/${APP_NAME}.app/Contents/Info.plist"
  "../target/dx/groundstation_frontend/Release/macos/${APP_NAME}.app/Contents/Info.plist"
)

PLIST=""
for c in "${CANDIDATES[@]}"; do
  if [[ -f "$c" ]]; then PLIST="$c"; break; fi
done

if [[ -z "$PLIST" ]]; then
  PLIST="$(find ../target/dx -path "*macos*.app/Contents/Info.plist" -print -quit || true)"
fi

if [[ -z "$PLIST" || ! -f "$PLIST" ]]; then
  echo "Could not find MACOS Info.plist under target/dx. Start dx serve first."
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
  # ------------------------------------------------------------
  # ✅ CRITICAL: bundle identity (fixes kMDItemCFBundleIdentifier = null)
  # ------------------------------------------------------------
  plist_set "CFBundleIdentifier" "string" "\"${BUNDLE_ID}\""

  # Nice-to-have (some tooling expects these)
  plist_set "CFBundleName" "string" "\"${BUNDLE_NAME}\""
  plist_set "CFBundleDisplayName" "string" "\"${BUNDLE_DISPLAY_NAME}\""

  # If you want: stable version strings (optional)
  # plist_set "CFBundleShortVersionString" "string" "\"0.1.0\""
  # plist_set "CFBundleVersion" "string" "\"1\""

  # ------------------------------------------------------------
  # ✅ Permissions strings
  # ------------------------------------------------------------
  plist_set "NSLocalNetworkUsageDescription" "string" \
    "\"This app connects to devices on your local network to get data from the ground station.\""

  plist_set "NSLocationWhenInUseUsageDescription" "string" \
    "\"This app requires location access to show your position on the GroundStation map.\""

  # On macOS, NSLocationUsageDescription is generally not needed anymore,
  # but keeping it doesn’t hurt.
  plist_set "NSLocationUsageDescription" "string" \
    "\"Shows your position on the GroundStation map.\""
}

# Initial apply
apply_patch

# Re-apply whenever the file changes
LAST_MTIME="$(stat -f "%m" "$PLIST")"
while true; do
  sleep 0.25
  if [[ ! -f "$PLIST" ]]; then
    continue
  fi
  MTIME="$(stat -f "%m" "$PLIST")"
  if [[ "$MTIME" != "$LAST_MTIME" ]]; then
    LAST_MTIME="$MTIME"
    apply_patch
    echo "Re-patched at $(date '+%H:%M:%S')"
  fi
done
