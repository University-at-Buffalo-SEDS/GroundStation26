#!/usr/bin/env bash
set -euo pipefail

# -----------------------------------------------------------------------------
# iOS App Signer (GUI) equivalent: embed.mobileprovision + sign + package .ipa
# - Does NOT build
# - Does NOT change bundle id
# - Does NOT change version strings
# - Enforces get-task-allow=false
# - Picks signing identity via regex from `security find-identity`
#
# Optional env:
#   CERT_REGEX='Apple Distribution:'   (default)
#   CERT_PICK='newest'|'first'         (default: newest)
# -----------------------------------------------------------------------------

APP="${1:?Missing .app path}"
PROVISION="${2:?Missing .mobileprovision path}"
IPA_OUT="${3:?Missing output .ipa path}"

CERT_REGEX="${CERT_REGEX:-Apple Distribution:}"
CERT_PICK="${CERT_PICK:-newest}"  # newest|first

PB="/usr/libexec/PlistBuddy"
CODESIGN="/usr/bin/codesign"
SECURITY="/usr/bin/security"
PLUTIL="/usr/bin/plutil"

log() { printf "[%s] %s\n" "$(date '+%H:%M:%S')" "$*"; }
die() { printf "[ERROR] %s\n" "$*" >&2; exit 1; }

[[ -d "$APP" ]] || die "App bundle not found: $APP"
[[ -f "$APP/Info.plist" ]] || die "Missing Info.plist in app: $APP"
[[ -f "$PROVISION" ]] || die "Provisioning profile not found: $PROVISION"
[[ -x "$PB" ]] || die "PlistBuddy not found/executable at: $PB"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# -----------------------------------------------------------------------------
# Pick a signing identity from keychain WITHOUT hardcoding PII
# -----------------------------------------------------------------------------
identity_candidates="$($SECURITY find-identity -v -p codesigning \
  | sed -n 's/.*"\(.*\)".*/\1/p' \
  | grep -E "$CERT_REGEX" || true)"

[[ -n "$identity_candidates" ]] || {
  log "No signing identities matched regex: $CERT_REGEX"
  log "Available identities:"
  $SECURITY find-identity -v -p codesigning || true
  die "No matching signing identity found."
}

pick_identity_first() {
  printf "%s\n" "$identity_candidates" | head -n 1
}

# Picks the identity whose cert has the latest NotValidAfter.
# If cert parsing fails for all, returns empty (caller falls back to first).
pick_identity_newest() {
  # Print: epoch<TAB>name for each candidate, sort, take best name.
  printf "%s\n" "$identity_candidates" | while IFS= read -r name; do
    na="$($SECURITY find-certificate -c "$name" -p 2>/dev/null \
      | openssl x509 -noout -enddate 2>/dev/null \
      | head -n 1 \
      | sed 's/^notAfter=//')"

    epoch=0
    if [[ -n "${na:-}" ]]; then
      epoch="$(date -j -f "%b %e %T %Y %Z" "$na" "+%s" 2>/dev/null || echo 0)"
    fi

    printf "%s\t%s\n" "$epoch" "$name"
  done | sort -nr | head -n 1 | cut -f2- || true
}

IDENTITY=""
case "$CERT_PICK" in
  newest)
    IDENTITY="$(pick_identity_newest || true)"
    if [[ -z "$IDENTITY" ]]; then
      IDENTITY="$(pick_identity_first)"
    fi
    ;;
  first)
    IDENTITY="$(pick_identity_first)"
    ;;
  *)
    die "CERT_PICK must be 'newest' or 'first' (got: $CERT_PICK)"
    ;;
esac

[[ -n "$IDENTITY" ]] || die "Failed to pick a signing identity."

log "Using signing identity matching regex: $CERT_REGEX (picked: $CERT_PICK)"
# (intentionally not printing the identity string to keep logs non-PII)

# -----------------------------------------------------------------------------
# 1) Embed provisioning profile
# -----------------------------------------------------------------------------
log "Embedding provisioning profile..."
cp -f "$PROVISION" "$APP/embedded.mobileprovision"

# -----------------------------------------------------------------------------
# 2) Remove old signatures (if any)
# -----------------------------------------------------------------------------
log "Removing old signature artifacts..."
rm -rf "$APP/_CodeSignature" 2>/dev/null || true
rm -f  "$APP/CodeResources" 2>/dev/null || true

# -----------------------------------------------------------------------------
# 3) Extract entitlements and enforce get-task-allow=false
# -----------------------------------------------------------------------------
log "Extracting entitlements from provisioning profile..."
$SECURITY cms -D -i "$PROVISION" > "$TMP/profile.plist"
$PB -x -c "Print :Entitlements" "$TMP/profile.plist" > "$TMP/entitlements.plist"

if $PB -c "Print :get-task-allow" "$TMP/entitlements.plist" >/dev/null 2>&1; then
  $PB -c "Set :get-task-allow false" "$TMP/entitlements.plist" >/dev/null
else
  $PB -c "Add :get-task-allow bool false" "$TMP/entitlements.plist" >/dev/null
fi

$PLUTIL -convert xml1 "$TMP/entitlements.plist" >/dev/null 2>&1 || true

# -----------------------------------------------------------------------------
# 4) Sign nested content first (Frameworks, dylibs, appex)
# -----------------------------------------------------------------------------
sign_path() {
  local p="$1"
  log "Signing: $(basename "$p")"

  # Timestamp can fail offline; make it best-effort.
  # --options runtime is usually harmless; keep it.
  if ! $CODESIGN --force --sign "$IDENTITY" --options runtime --timestamp "$p" 2>/dev/null; then
    $CODESIGN --force --sign "$IDENTITY" --options runtime "$p"
  fi
}

if [[ -d "$APP/Frameworks" ]]; then
  find "$APP/Frameworks" -maxdepth 2 \( -name "*.framework" -o -name "*.dylib" \) -print0 \
    | while IFS= read -r -d '' item; do
        sign_path "$item"
      done
fi

find "$APP" -maxdepth 2 -name "*.appex" -print0 2>/dev/null \
  | while IFS= read -r -d '' appex; do
      sign_path "$appex"
    done

# -----------------------------------------------------------------------------
# 5) Sign the main app with entitlements
# -----------------------------------------------------------------------------
log "Signing main app bundle..."
if ! $CODESIGN --force --sign "$IDENTITY" \
  --entitlements "$TMP/entitlements.plist" \
  --options runtime --timestamp \
  "$APP" 2>/dev/null; then
  $CODESIGN --force --sign "$IDENTITY" \
    --entitlements "$TMP/entitlements.plist" \
    --options runtime \
    "$APP"
fi

# -----------------------------------------------------------------------------
# 6) Verify
# -----------------------------------------------------------------------------
log "Verifying signature..."
$CODESIGN --verify --deep --strict --verbose=2 "$APP"

# -----------------------------------------------------------------------------
# 7) Package IPA (Payload/*.app)
# -----------------------------------------------------------------------------
log "Packaging IPA..."
rm -f "$IPA_OUT" 2>/dev/null || true
mkdir -p "$TMP/Payload"
cp -R "$APP" "$TMP/Payload/"

(
  cd "$TMP"
  /usr/bin/zip -qry "$IPA_OUT" Payload
)

log "✅ Done: $IPA_OUT"
