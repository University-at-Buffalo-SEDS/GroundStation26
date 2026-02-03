#!/usr/bin/env bash
set -euo pipefail

# scripts/patch_plist.sh
#
# iOS .app bundle post-processing:
#  - copies embedded.mobileprovision into the bundle
#  - copies AppIcon*.png into the bundle
#  - patches Info.plist (display name, icons, privacy strings)
#  - extracts entitlements from embedded.mobileprovision
#  - codesigns the app using an unambiguous identity (SHA-1 hash)
#
# macOS compatibility notes:
#  - Avoids GNU awk features (BSD awk doesn't support match(..., ..., array))
#  - Avoids sed regex features that differ across BSD/GNU sed
#

debug=false

APP_NAME="GroundStation 26"
LEGACY_APP_NAME="GroundstationFrontend"
APP_DIR="./dist/${APP_NAME}.app"
PLIST="${APP_DIR}/Info.plist"

MOBILEPROVISION_SRC="static/embedded.mobileprovision"
ICON_SRC_DIR="./assets"

PB="/usr/libexec/PlistBuddy"
PLUTIL="/usr/bin/plutil"

# Optional: pin selection by Team ID (the "(TEAMID)" in cert name)
# export GS26_TEAM_ID="9W6VP6AYB4"
GS26_TEAM_ID="${GS26_TEAM_ID:-}"

if [[ "$debug" == true ]]; then
  exec 3>&1 4>&2
else
  exec 3>/dev/null 4>/dev/null
fi

log() { printf "[%s] %s\n" "$(date '+%H:%M:%S')" "$*" >&3; }
die() { printf "[ERROR] %s\n" "$*" >&2; exit 1; }

trap 'rc=$?; printf "\n[FAIL] line=%s rc=%s cmd: %s\n" "$LINENO" "$rc" "$BASH_COMMAND" >&2; exit $rc' ERR

run_dbg() {
  log "CMD: $*"
  "$@" 1>&3 2>&4
}

[[ -x "$PB" ]] || die "PlistBuddy not found/executable at: $PB"
[[ -x "$PLUTIL" ]] || die "plutil not found/executable at: $PLUTIL"

# Handle legacy bundle name if build produced a different folder name
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

  # PlistBuddy tokenizes; keep values single-line and escape quotes.
  local safe="$value"
  safe="${safe//$'\n'/ }"
  safe="${safe//\"/\\\"}"

  if plist_has "$key"; then
    pb "Set :$key $safe"
  else
    pb "Add :$key string $safe"
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

# ----------------------------
# Pick newest Apple Development identity (return SHA-1 hash)
# ----------------------------
# We avoid: awk match(..., ..., array) (GNU-only)
# We avoid: relying on label strings for codesign (can be ambiguous)
pick_newest_apple_development_identity_sha1() {
  local tmpout
  tmpout="$(mktemp -t gs26-identities.XXXXXX)"

  # shellcheck disable=SC2064
  trap "rm -f '$tmpout' 2>/dev/null || true" RETURN

  # Parse `security find-identity -v -p codesigning` using portable tools.
  #
  # Example line:
  #  1) 0123ABCD... "Apple Development: you@example.com (TEAMID)"
  #
  # We'll extract:
  #   sha1|Apple Development: ...
  security find-identity -v -p codesigning 2>/dev/null \
    | sed -n 's/^[[:space:]]*[0-9][0-9]*[)]*[[:space:]]*\([0-9A-Fa-f]\{40\}\)[[:space:]]*"\(Apple Development:.*\)".*/\1|\2/p' \
    > "$tmpout"

  [[ -s "$tmpout" ]] || return 1

  local best_epoch=-1
  local best_sha1=""
  local best_name=""

  while IFS='|' read -r sha1 name; do
    [[ -n "${sha1:-}" && -n "${name:-}" ]] || continue

    # Optional team filter
    if [[ -n "$GS26_TEAM_ID" ]]; then
      case "$name" in
        *"(${GS26_TEAM_ID})"*) : ;;
        *) continue ;;
      esac
    fi

    # Extract PEM for this exact sha1 from find-certificate output
    local pem=""
    pem="$(
      security find-certificate -a -Z -p -c "$name" 2>/dev/null \
        | awk -v h="$sha1" '
            BEGIN { want=0; inpem=0 }
            /^SHA-1 hash: / {
              if (index($0, h) > 0) want=1; else want=0;
              inpem=0;
              next
            }
            want && /-----BEGIN CERTIFICATE-----/ { inpem=1 }
            want && inpem { print }
            want && /-----END CERTIFICATE-----/ { exit }
          '
    )"

    if [[ -z "$pem" ]]; then
      log "WARN: Could not extract PEM for identity sha1=$sha1 name=$name"
      continue
    fi

    local enddate=""
    enddate="$(
      printf "%s\n" "$pem" | openssl x509 -noout -enddate 2>/dev/null | sed 's/^notAfter=//'
    )"
    if [[ -z "$enddate" ]]; then
      log "WARN: Could not read notAfter for identity sha1=$sha1 name=$name"
      continue
    fi

    local epoch=""
    epoch="$(
      python3 - <<'PY' "$enddate" 2>/dev/null || true
import sys
from datetime import datetime, timezone
s = sys.argv[1].strip()
fmts = ["%b %d %H:%M:%S %Y %Z", "%b  %d %H:%M:%S %Y %Z", "%b %e %H:%M:%S %Y %Z"]
for f in fmts:
    try:
        dt = datetime.strptime(s, f)
        if dt.tzinfo is None:
            dt = dt.replace(tzinfo=timezone.utc)
        print(int(dt.timestamp()))
        sys.exit(0)
    except Exception:
        pass
sys.exit(1)
PY
    )"

    if [[ -z "$epoch" ]]; then
      log "WARN: Could not parse notAfter date '$enddate' for sha1=$sha1 name=$name"
      continue
    fi

    log "Candidate identity: sha1=$sha1 name=$name notAfter='$enddate' epoch=$epoch"

    if (( epoch > best_epoch )); then
      best_epoch="$epoch"
      best_sha1="$sha1"
      best_name="$name"
    fi
  done < "$tmpout"

  if [[ -n "$best_sha1" ]]; then
    log "Selected identity: sha1=$best_sha1 name=$best_name epoch=$best_epoch"
    printf "%s" "$best_sha1"
    return 0
  fi

  # Fallback: first Apple Development identity
  best_sha1="$(head -n 1 "$tmpout" | cut -d'|' -f1)"
  [[ -n "$best_sha1" ]] || return 1
  printf "%s" "$best_sha1"
  return 0
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

# Normalize iPad icon filename variant
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

# NOTE: array_add_unique prevents duplicates, but if the existing array contains
# differing variants it may still grow over time. See optional "reset arrays" note below.
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

log "Finding Apple Development signing identity (newest cert) (SHA-1)"
IDENTITY_SHA1="$(pick_newest_apple_development_identity_sha1 || true)"
[[ -n "$IDENTITY_SHA1" ]] || die "No 'Apple Development' codesigning identity found."

log "Using identity SHA-1: $IDENTITY_SHA1"

rm -rf "$APP_DIR/_CodeSignature" 2>/dev/null || true

log "Signing app bundle"
run_dbg codesign --force --deep --timestamp=none --sign "$IDENTITY_SHA1" \
  --entitlements /tmp/gs26-entitlements.plist \
  "$APP_DIR"

log "Verifying signature"
run_dbg codesign --verify --deep --strict --verbose=4 "$APP_DIR"

log "✅ Patch + sign complete"

# Optional: if you want icon arrays to be stable across repeated runs, uncomment:
# pb_try "Delete :CFBundleIcons:CFBundlePrimaryIcon:CFBundleIconFiles" || true
# pb "Add :CFBundleIcons:CFBundlePrimaryIcon:CFBundleIconFiles array"
# pb_try "Delete :CFBundleIcons~ipad:CFBundlePrimaryIcon:CFBundleIconFiles" || true
# pb "Add :CFBundleIcons~ipad:CFBundlePrimaryIcon:CFBundleIconFiles array"
