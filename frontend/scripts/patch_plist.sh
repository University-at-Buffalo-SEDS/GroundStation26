#!/usr/bin/env bash
set -euo pipefail

debug=false

APP_NAME="GroundStation 26"
LEGACY_APP_NAME="GroundstationFrontend"
APP_DIR="./dist/${APP_NAME}.app"
PLIST="${APP_DIR}/Info.plist"

MOBILEPROVISION_SRC="static/embedded.mobileprovision"
ICON_SRC_DIR="./assets"

PB="/usr/libexec/PlistBuddy"
PLUTIL="/usr/bin/plutil"

if [[ "$debug" == true ]]; then
  exec 3>&1 4>&2
else
  exec 3>/dev/null 4>/dev/null
fi

log() { printf "[%s] %s\n" "$(date '+%H:%M:%S')" "$*" >&3; }
die() { printf "[ERROR] %s\n" "$*" >&2; exit 1; }

# shellcheck disable=SC2154
trap 'rc=$?; printf "\n[FAIL] line=%s rc=%s cmd: %s\n" "$LINENO" "$rc" "$BASH_COMMAND" >&2; exit $rc' ERR

run_dbg() {
  log "CMD: $*"
  "$@" 1>&3 2>&4
}

[[ -x "$PB" ]] || die "PlistBuddy not found/executable at: $PB"
[[ -x "$PLUTIL" ]] || die "plutil not found/executable at: $PLUTIL"
if [[ ! -d "$APP_DIR" && -d "./dist/${LEGACY_APP_NAME}.app" ]]; then
  log "Renaming app bundle to ${APP_NAME}.app"
  mv "./dist/${LEGACY_APP_NAME}.app" "$APP_DIR"
fi

[[ -d "$APP_DIR" ]] || die "App bundle directory not found: $APP_DIR"
[[ -f "$PLIST" ]] || die "Info.plist not found: $PLIST"
[[ -f "$MOBILEPROVISION_SRC" ]] || die "Missing provisioning profile: $MOBILEPROVISION_SRC"
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

set_string() {
  local key="$1"
  local value="$2"
  if plist_has "$key"; then
    pb "Set :$key $value"
  else
    pb "Add :$key string $value"
  fi
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
  pb_try "Print :CFBundleIcons" || true
  pb_try "Print :CFBundleIcons~ipad" || true
}

# 1) Copy embedded.mobileprovision
log "Copying embedded.mobileprovision into bundle"
cp -f "$MOBILEPROVISION_SRC" "$APP_DIR/embedded.mobileprovision"

# 2) Copy icons
log "Copying AppIcon*.png from $ICON_SRC_DIR into bundle"
shopt -s nullglob
ICONS=( "$ICON_SRC_DIR"/AppIcon*.png )
shopt -u nullglob
[[ ${#ICONS[@]} -gt 0 ]] || die "No AppIcon*.png found in $ICON_SRC_DIR"

for f in "${ICONS[@]}"; do
  cp -f "$f" "$APP_DIR/"
done

if [[ -f "$APP_DIR/AppIcon76x76@2x~ipad.png" && ! -f "$APP_DIR/AppIcon76x76@2x.png" ]]; then
  mv "$APP_DIR/AppIcon76x76@2x~ipad.png" "$APP_DIR/AppIcon76x76@2x.png"
fi
if [[ -f "$APP_DIR/AppIcon76x76@2x~ipad.png" ]]; then
  rm -f "$APP_DIR/AppIcon76x76@2x~ipad.png"
fi

# 3) Patch Info.plist
dump_plist_sections

set_string "CFBundleDisplayName" "GS 26"
set_string "CFBundleName" "GroundStation 26"

ensure_dict "CFBundleIcons"
ensure_dict "CFBundleIcons:CFBundlePrimaryIcon"
ensure_array "CFBundleIcons:CFBundlePrimaryIcon:CFBundleIconFiles"
set_string  "CFBundleIcons:CFBundlePrimaryIcon:CFBundleIconName" "AppIcon"

ensure_dict "CFBundleIcons~ipad"
ensure_dict "CFBundleIcons~ipad:CFBundlePrimaryIcon"
ensure_array "CFBundleIcons~ipad:CFBundlePrimaryIcon:CFBundleIconFiles"
set_string  "CFBundleIcons~ipad:CFBundlePrimaryIcon:CFBundleIconName" "AppIcon"

array_add_unique "CFBundleIcons:CFBundlePrimaryIcon:CFBundleIconFiles" "AppIcon60x60"
array_add_unique "CFBundleIcons~ipad:CFBundlePrimaryIcon:CFBundleIconFiles" "AppIcon60x60"
array_add_unique "CFBundleIcons~ipad:CFBundlePrimaryIcon:CFBundleIconFiles" "AppIcon76x76"

set_string "NSLocalNetworkUsageDescription" \
  "This app connects to devices on your local network to get data from the ground station."
set_string "NSLocationWhenInUseUsageDescription" \
  "This app requires location access to help you locate your rocket."

dump_plist_sections

# 4) Decode profile + extract entitlements as REAL plist + sign
log "Decoding embedded.mobileprovision -> /tmp/gs26-profile.plist"
security cms -D -i "$APP_DIR/embedded.mobileprovision" > /tmp/gs26-profile.plist 2>/tmp/gs26-profile.err || {
  cat /tmp/gs26-profile.err >&2 || true
  die "Failed to decode embedded.mobileprovision"
}
[[ -s /tmp/gs26-profile.plist ]] || die "Decoded profile plist is empty: /tmp/gs26-profile.plist"

log "Extracting Entitlements via plutil -> /tmp/gs26-entitlements.plist"
"$PLUTIL" -extract Entitlements xml1 -o /tmp/gs26-entitlements.plist /tmp/gs26-profile.plist 2>/tmp/gs26-entitlements.err || {
  cat /tmp/gs26-entitlements.err >&2 || true
  die "Failed to extract Entitlements using plutil"
}
[[ -s /tmp/gs26-entitlements.plist ]] || die "Entitlements plist is empty: /tmp/gs26-entitlements.plist"

log "Finding Apple Development signing identity"
IDENTITY="$(
  security find-identity -v -p codesigning 2>/dev/null \
    | sed -n 's/.*"\(Apple Development:.*\)".*/\1/p' \
    | head -n 1
)"
[[ -n "$IDENTITY" ]] || die "No 'Apple Development' codesigning identity found."

log "Using identity: $IDENTITY"

rm -rf "$APP_DIR/_CodeSignature" 2>/dev/null || true

log "Signing app bundle"
run_dbg codesign --force --deep --timestamp=none --sign "$IDENTITY" --entitlements /tmp/gs26-entitlements.plist "$APP_DIR"

log "Verifying signature"
run_dbg codesign --verify --deep --strict --verbose=4 "$APP_DIR"

log "✅ Patch + sign complete"
