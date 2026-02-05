#!/usr/bin/env bash
set -euo pipefail

# scripts/patch_plist.sh
#
# iOS .app bundle post-processing (COSMETIC + METADATA):
#  - Copies Assets.car into the bundle (asset catalog icons)
#  - (Optionally) removes loose AppIcon*.png so Transporter doesn't get confused
#  - Patches Info.plist (display name, icon name, privacy strings)
#  - Adds required Xcode/SDK metadata keys commonly required by Transporter validation
#
# IMPORTANT:
#  - This script does NOT embed provisioning profiles
#  - This script does NOT extract entitlements
#  - This script does NOT codesign

debug=false

APP_NAME="GroundStation 26"
LEGACY_APP_NAME="GroundstationFrontend"
APP_DIR="./dist/${APP_NAME}.app"
PLIST="${APP_DIR}/Info.plist"
VERSION_INPUT="${1:-${APP_VERSION:-}}"

ICON_SRC_DIR="./assets"
ASSETS_CAR_SRC="${ICON_SRC_DIR}/Assets.car"

PB="/usr/libexec/PlistBuddy"

# If true, delete loose AppIcon*.png after copying Assets.car
# Recommended when you want Transporter to rely on the asset catalog only.
REMOVE_LOOSE_ICONS=true

if [[ "$debug" == true ]]; then
  exec 3>&1 4>&2
else
  exec 3>/dev/null 4>/dev/null
fi

log() { printf "[%s] %s\n" "$(date '+%H:%M:%S')" "$*" >&3; }
die() { printf "[ERROR] %s\n" "$*" >&2; exit 1; }

# shellcheck disable=SC2154
trap 'rc=$?; printf "\n[FAIL] line=%s rc=%s cmd: %s\n" "$LINENO" "$rc" "$BASH_COMMAND" >&2; exit $rc' ERR

[[ -x "$PB" ]] || die "PlistBuddy not found/executable at: $PB"

# Handle legacy bundle name if build produced a different folder name
if [[ ! -d "$APP_DIR" && -d "./dist/${LEGACY_APP_NAME}.app" ]]; then
  log "Renaming app bundle to ${APP_NAME}.app"
  mv "./dist/${LEGACY_APP_NAME}.app" "$APP_DIR"
fi

[[ -d "$APP_DIR" ]] || die "App bundle directory not found: $APP_DIR"
[[ -f "$PLIST" ]] || die "Info.plist not found: $PLIST"
[[ -d "$ICON_SRC_DIR" ]] || die "Missing icon source dir: $ICON_SRC_DIR"

pb() {
  local cmd="$1"
  log "PlistBuddy: $cmd"
  "$PB" -c "$cmd" "$PLIST" 1>&3 2>&4
}

pb_try() {
  local cmd="$1"
  log "PlistBuddy (try): $cmd"
  "$PB" -c "$cmd" "$PLIST" 1>&3 2>&4 || return 1
}

plist_has() {
  local key="$1"
  "$PB" -c "Print :$key" "$PLIST" >/dev/null 2>&1
}

ensure_dict() {
  local key="$1"
  if ! plist_has "$key"; then
    pb "Add :$key dict"
  fi
}

ensure_array() {
  local key="$1"
  if ! plist_has "$key"; then
    pb "Add :$key array"
  fi
}

# PlistBuddy "Set :Key value" expects unquoted scalars.
# We keep everything as a single-line token.
set_string() {
  local key="$1"
  local value="$2"

  local safe="$value"
  safe="${safe//$'\n'/ }"
  safe="${safe//\"/\\\"}"

  if plist_has "$key"; then
    pb "Set :$key $safe"
  else
    pb "Add :$key string $safe"
  fi
}

set_bool() {
  local key="$1"
  local value="$2" # true|false
  if plist_has "$key"; then
    pb "Set :$key $value"
  else
    pb "Add :$key bool $value"
  fi
}

set_int() {
  local key="$1"
  local value="$2"
  if plist_has "$key"; then
    pb "Set :$key $value"
  else
    pb "Add :$key integer $value"
  fi
}

# Only set if missing (so we don't stomp values injected elsewhere)
set_string_if_missing() {
  local key="$1"
  local value="$2"
  if ! plist_has "$key"; then
    set_string "$key" "$value"
  else
    log "Key already present (skip): :$key"
  fi
}

set_bool_if_missing() {
  local key="$1"
  local value="$2"
  if ! plist_has "$key"; then
    set_bool "$key" "$value"
  else
    log "Key already present (skip): :$key"
  fi
}

set_int_if_missing() {
  local key="$1"
  local value="$2"
  if ! plist_has "$key"; then
    set_int "$key" "$value"
  else
    log "Key already present (skip): :$key"
  fi
}

get_string() {
  local key="$1"
  "$PB" -c "Print :$key" "$PLIST" 2>/dev/null | tr -d '\r' || true
}

array_add_unique() {
  local array_key="$1"
  local value="$2"

  ensure_array "$array_key"

  if "$PB" -c "Print :$array_key" "$PLIST" 2>/dev/null | grep -Fxq "$value"; then
    log "Array already contains '$value' -> :$array_key"
    return 0
  fi

  local idx=0
  while "$PB" -c "Print :$array_key:$idx" "$PLIST" >/dev/null 2>&1; do
    idx=$((idx + 1))
  done

  log "Appending '$value' to :$array_key at index $idx"
  pb "Add :$array_key:$idx string $value"
}

dump_plist_sections() {
  [[ "$debug" == true ]] || return 0
  pb_try "Print :CFBundleIdentifier" || true
  pb_try "Print :CFBundleVersion" || true
  pb_try "Print :CFBundleShortVersionString" || true
  pb_try "Print :MinimumOSVersion" || true
  pb_try "Print :DTPlatformName" || true
  pb_try "Print :DTSDKName" || true
  pb_try "Print :CFBundleIcons" || true
  pb_try "Print :CFBundleIcons~ipad" || true
  pb_try "Print :CFBundleIconFiles" || true
  pb_try "Print :CFBundleIconFile" || true
}

# -----------------------------------------------------------------------------
# 1) Copy asset catalog (Assets.car)
# -----------------------------------------------------------------------------
[[ -f "$ASSETS_CAR_SRC" ]] || die "Missing Assets.car at: $ASSETS_CAR_SRC"
log "Copying Assets.car from $ASSETS_CAR_SRC into bundle"
cp -f "$ASSETS_CAR_SRC" "$APP_DIR/Assets.car"

# Optional: remove loose icons so Transporter uses the asset catalog
if [[ "$REMOVE_LOOSE_ICONS" == true ]]; then
  log "Removing loose AppIcon*.png from bundle (asset catalog only)"
  rm -f "$APP_DIR"/AppIcon*.png 2>/dev/null || true
fi

# -----------------------------------------------------------------------------
# 2) Patch Info.plist
# -----------------------------------------------------------------------------
dump_plist_sections

# Human-facing names
set_string "CFBundleDisplayName" "GS 26"
set_string "CFBundleName" "GroundStation 26"

# Versioning (optional)
if [[ -n "$VERSION_INPUT" ]]; then
  set_string "CFBundleShortVersionString" "$VERSION_INPUT"
  set_string "CFBundleVersion" "$VERSION_INPUT"
else
  log "No version provided; skipping CFBundleShortVersionString/CFBundleVersion"
fi

# Asset-catalog icons:
# With Assets.car, do NOT use CFBundleIconFiles / CFBundleIconFile.
# Instead, set the icon *name* to match the AppIcon set inside the asset catalog.
ensure_dict "CFBundleIcons"
ensure_dict "CFBundleIcons:CFBundlePrimaryIcon"
set_string "CFBundleIcons:CFBundlePrimaryIcon:CFBundleIconName" "AppIcon"

ensure_dict "CFBundleIcons~ipad"
ensure_dict "CFBundleIcons~ipad:CFBundlePrimaryIcon"
set_string "CFBundleIcons~ipad:CFBundlePrimaryIcon:CFBundleIconName" "AppIcon"

# iPad multitasking requires a launch screen.
# Since MinimumOSVersion >= 14, use UILaunchScreen dictionary (no storyboard file needed).
ensure_dict "UILaunchScreen"
set_bool_if_missing "UILaunchScreen:UILaunchScreenRequiresPersistentWiFi" "false" 2>/dev/null || true
# Provide at least one key; BackgroundColor is enough for validation.
set_string "UILaunchScreen:UILaunchScreenBackgroundColor" "#1E1F22"


# Clean up loose-icon keys if they exist (avoid validator ambiguity)
if plist_has "CFBundleIconFiles"; then
  pb "Delete :CFBundleIconFiles"
fi
if plist_has "CFBundleIconFile"; then
  pb "Delete :CFBundleIconFile"
fi
# Some builds may have added this under CFBundleIcons primary icon
if plist_has "CFBundleIcons:CFBundlePrimaryIcon:CFBundleIconFiles"; then
  pb "Delete :CFBundleIcons:CFBundlePrimaryIcon:CFBundleIconFiles"
fi
if plist_has "CFBundleIcons~ipad:CFBundlePrimaryIcon:CFBundleIconFiles"; then
  pb "Delete :CFBundleIcons~ipad:CFBundlePrimaryIcon:CFBundleIconFiles"
fi

# Privacy strings
set_string "NSLocalNetworkUsageDescription" \
  "This app connects to devices on your local network to get data from the ground station."
set_string "NSLocationWhenInUseUsageDescription" \
  "This app requires location access to help you locate your rocket."

# -----------------------------------------------------------------------------
# 2b) Add required/expected bundle keys for iOS uploads
# -----------------------------------------------------------------------------
set_string_if_missing "CFBundlePackageType" "APPL"
set_bool_if_missing   "LSRequiresIPhoneOS" "true"

if ! plist_has "MinimumOSVersion"; then
  set_string "MinimumOSVersion" "14.0"
fi

# -----------------------------------------------------------------------------
# 2c) Add Xcode/SDK metadata keys Transporter validates for some packages
# -----------------------------------------------------------------------------
set_string_if_missing "DTPlatformName" "iphoneos"

MIN_OS="$(get_string "MinimumOSVersion")"
SDK_VER_FALLBACK="${MIN_OS%%.*}.${MIN_OS#*.}"
if [[ "$SDK_VER_FALLBACK" == "$MIN_OS" ]]; then
  SDK_VER_FALLBACK="$MIN_OS"
fi

SDK_NAME_ENV="${SDK_NAME:-iphoneos}"
SDK_VER_ENV="${SDK_VERSION:-$SDK_VER_FALLBACK}"

set_string_if_missing "DTSDKName" "${SDK_NAME_ENV}${SDK_VER_ENV}"
set_string_if_missing "DTPlatformVersion" "${SDK_VER_ENV}"

if [[ -n "${XCODE_VERSION_ACTUAL:-}" ]]; then
  set_string_if_missing "DTXcode" "${XCODE_VERSION_ACTUAL}"
fi
if [[ -n "${XCODE_PRODUCT_BUILD_VERSION:-}" ]]; then
  set_string_if_missing "DTXcodeBuild" "${XCODE_PRODUCT_BUILD_VERSION}"
fi

if [[ -n "${MACOSX_DEPLOYMENT_TARGET:-}" ]]; then
  set_string_if_missing "BuildMachineOSBuild" "${MACOSX_DEPLOYMENT_TARGET}"
fi

ensure_dict "UILaunchScreen"
set_string "UILaunchScreen:UILaunchScreenBackgroundColor" "#1E1F22"

# Required by App Store validation for some non-Xcode bundles
# CFBundleSupportedPlatforms must be an array with a single entry: iPhoneOS
if plist_has "CFBundleSupportedPlatforms"; then
  pb "Delete :CFBundleSupportedPlatforms"
fi
pb "Add :CFBundleSupportedPlatforms array"
pb "Add :CFBundleSupportedPlatforms:0 string iPhoneOS"

dump_plist_sections

# -----------------------------------------------------------------------------
# 3) Ensure we do NOT leave old signing artifacts around
# -----------------------------------------------------------------------------
rm -f "$APP_DIR/embedded.mobileprovision" 2>/dev/null || true
rm -rf "$APP_DIR/_CodeSignature" 2>/dev/null || true

log "✅ Patch complete (UNSIGNED)"
