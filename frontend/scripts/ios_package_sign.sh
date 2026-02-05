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

# Decode provisioning profile once (used for cert matching + entitlements)
$SECURITY cms -D -i "$PROVISION" > "$TMP/profile.plist"

# -----------------------------------------------------------------------------
# Pick a signing identity from keychain WITHOUT hardcoding PII
# -----------------------------------------------------------------------------
identity_candidates="$($SECURITY find-identity -v -p codesigning \
  | sed -n 's/.*"\(.*\)".*/\1/p' \
  | grep -E "$CERT_REGEX" || true)"

all_identities="$($SECURITY find-identity -v -p codesigning \
  | sed -n 's/.*"\(.*\)".*/\1/p' || true)"

identity_hashes="$($SECURITY find-identity -v -p codesigning \
  | sed -n 's/.*\([0-9A-F]\{40\}\).*/\1/p' || true)"

profile_hashes=""
extract_profile_hashes() {
  $PLUTIL -extract DeveloperCertificates xml1 -o - "$TMP/profile.plist" 2>/dev/null \
    | awk '
        /<data>/{flag=1; next}
        /<\/data>/{flag=0; printf "\n"; next}
        { if (flag) { gsub(/^[ \t]+|[ \t]+$/, ""); printf "%s", $0 } }
      ' \
    | while IFS= read -r b64; do
        [[ -n "$b64" ]] || continue
        if echo "$b64" | base64 -D >/dev/null 2>&1; then
          cert_der="$(echo "$b64" | base64 -D 2>/dev/null || true)"
        else
          cert_der="$(echo "$b64" | base64 -d 2>/dev/null || true)"
        fi
        [[ -n "$cert_der" ]] || continue
        echo "$cert_der" \
          | openssl x509 -inform DER -noout -fingerprint -sha1 2>/dev/null \
          | sed 's/^SHA1 Fingerprint=//' \
          | tr -d ':' \
          | tr '[:lower:]' '[:upper:]'
      done
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
IDENTITY_HASH=""

log "Extracting allowed signing certs from provisioning profile..."
profile_hashes="$(extract_profile_hashes || true)"

if [[ -n "$profile_hashes" ]]; then
  log "Provisioning profile specifies signing certificates; matching keychain identities..."
  matched_identities=""
  matched_hashes=""
  while IFS= read -r hash; do
    [[ -n "$hash" ]] || continue
    idx=0
    while IFS= read -r id_hash; do
      idx=$((idx + 1))
      if [[ "$id_hash" == "$hash" ]]; then
        name="$(printf "%s\n" "$all_identities" | sed -n "${idx}p")"
        if [[ -n "$name" ]]; then
          matched_identities="${matched_identities}${name}"$'\n'
          matched_hashes="${matched_hashes}${hash}"$'\n'
        fi
      fi
    done <<< "$identity_hashes"
  done <<< "$profile_hashes"

  identity_candidates="$(printf "%s" "$matched_identities" | sed '/^$/d' || true)"
  identity_hash_candidates="$(printf "%s" "$matched_hashes" | sed '/^$/d' || true)"
fi

[[ -n "$identity_candidates" ]] || {
  log "No signing identities matched the provisioning profile/cert regex."
  log "Available identities:"
  $SECURITY find-identity -v -p codesigning || true
  die "No matching signing identity found."
}

case "$CERT_PICK" in
  newest)
    IDENTITY="$(printf "%s\n" "$identity_candidates" | while IFS= read -r name; do
      na="$($SECURITY find-certificate -c "$name" -p 2>/dev/null \
        | openssl x509 -noout -enddate 2>/dev/null \
        | head -n 1 \
        | sed 's/^notAfter=//')"

      epoch=0
      if [[ -n "${na:-}" ]]; then
        epoch="$(date -j -f "%b %e %T %Y %Z" "$na" "+%s" 2>/dev/null || echo 0)"
      fi

      printf "%s\t%s\n" "$epoch" "$name"
    done | sort -nr | head -n 1 | cut -f2- || true)"
    if [[ -z "$IDENTITY" ]]; then
      IDENTITY="$(printf "%s\n" "$identity_candidates" | head -n 1)"
    fi
    ;;
  first)
    IDENTITY="$(printf "%s\n" "$identity_candidates" | head -n 1)"
    ;;
  *)
    die "CERT_PICK must be 'newest' or 'first' (got: $CERT_PICK)"
    ;;
esac

if [[ -n "${identity_hash_candidates:-}" ]]; then
  idx=0
  while IFS= read -r name; do
    idx=$((idx + 1))
    if [[ "$name" == "$IDENTITY" ]]; then
      IDENTITY_HASH="$(printf "%s\n" "$identity_hash_candidates" | sed -n "${idx}p")"
      break
    fi
  done <<< "$identity_candidates"
fi

if [[ -z "$IDENTITY_HASH" ]]; then
  # fallback: map by full list index
  idx=0
  while IFS= read -r name; do
    idx=$((idx + 1))
    if [[ "$name" == "$IDENTITY" ]]; then
      IDENTITY_HASH="$(printf "%s\n" "$identity_hashes" | sed -n "${idx}p")"
      break
    fi
  done <<< "$all_identities"
fi

[[ -n "$IDENTITY" ]] || die "Failed to pick a signing identity."

log "Using signing identity matching regex/profile (picked: $CERT_PICK)"
# (intentionally not printing the identity string to keep logs non-PII)
if [[ -n "$IDENTITY_HASH" ]]; then
  log "Using identity hash: ...${IDENTITY_HASH: -6}"
fi
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
  local signer="${IDENTITY_HASH:-$IDENTITY}"

  # Timestamp can fail offline; make it best-effort.
  # --options runtime is usually harmless; keep it.
  if ! $CODESIGN --force --sign "$signer" --options runtime --timestamp "$p" 2>/dev/null; then
    $CODESIGN --force --sign "$signer" --options runtime "$p"
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
signer="${IDENTITY_HASH:-$IDENTITY}"
if ! $CODESIGN --force --sign "$signer" \
  --entitlements "$TMP/entitlements.plist" \
  --options runtime --timestamp \
  "$APP" 2>/dev/null; then
  $CODESIGN --force --sign "$signer" \
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
