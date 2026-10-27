#!/bin/sh
# Installs gs-video-sender as a systemd service that starts on boot and
# publishes the Pi CSI camera (if any) plus every connected USB camera,
# including each camera's built-in microphone.
#
# Run on the Pi from a checkout of this repository, as a sudo-capable user:
#   sh video_sender/install_pi.sh [--binary PATH]
#
#   --binary PATH       install a prebuilt aarch64 gs-video-sender instead of
#                       building (building takes a while on 1-2 GB Pis)
#   GS_VIDEO_PASSWORD   set to install non-interactively; otherwise prompted
#
# Works on Raspberry Pi OS and Ubuntu. Safe to re-run after pulling changes:
# the binary and unit are upgraded, existing config and password are kept
# (a missing usb_cameras block is added to older configs).
set -eu
cd "$(dirname "$0")/.."

CONFIG=/var/lib/gs-video-sender/config.json
ENVIRONMENT=/etc/gs-video-sender/environment
BINARY=
while [ $# -gt 0 ]; do
    case "$1" in
        --binary) BINARY=$2; shift 2 ;;
        *) echo "usage: $0 [--binary PATH]" >&2; exit 2 ;;
    esac
done

step() { printf '\n==> %s\n' "$*"; }

step "Installing packages"
sudo apt-get update
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y ffmpeg v4l-utils alsa-utils python3
# rpicam-apps (CSI camera) exists only on Raspberry Pi OS.
if ! sudo DEBIAN_FRONTEND=noninteractive apt-get install -y rpicam-apps; then
    echo "rpicam-apps unavailable on this OS; only USB cameras will stream"
fi

if [ -z "$BINARY" ]; then
    step "Building gs-video-sender"
    sudo DEBIAN_FRONTEND=noninteractive apt-get install -y build-essential pkg-config git curl
    if ! command -v cargo >/dev/null 2>&1 && [ -f "$HOME/.cargo/env" ]; then
        . "$HOME/.cargo/env"
    fi
    if ! command -v cargo >/dev/null 2>&1; then
        echo "Installing Rust (rustup, minimal profile) for $(id -un)"
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
        . "$HOME/.cargo/env"
    fi
    # The release profile uses fat LTO; Pis with little RAM need swap to link.
    BUILD_SWAP=
    mem_kb=$(awk '/^MemTotal:/ {print $2}' /proc/meminfo)
    swap_kb=$(awk '/^SwapTotal:/ {print $2}' /proc/meminfo)
    if [ $((mem_kb + swap_kb)) -lt 3000000 ]; then
        BUILD_SWAP=/var/tmp/gs-video-build.swap
        echo "Low memory: adding a temporary 2 GiB swap file for the build"
        sudo fallocate -l 2G "$BUILD_SWAP"
        sudo chmod 600 "$BUILD_SWAP"
        sudo mkswap "$BUILD_SWAP" >/dev/null
        sudo swapon "$BUILD_SWAP"
        trap 'sudo swapoff "$BUILD_SWAP" 2>/dev/null; sudo rm -f "$BUILD_SWAP"' EXIT
        export CARGO_BUILD_JOBS=2
    fi
    # Some workspace dependencies are git repositories; the git CLI is more
    # reliable than cargo's built-in client behind proxies.
    CARGO_NET_GIT_FETCH_WITH_CLI=true cargo build --release -p gs-video-sender
    BINARY=target/release/gs-video-sender
fi
[ -x "$BINARY" ] || { echo "binary not found: $BINARY" >&2; exit 1; }

step "Installing service files"
if ! id gs-video >/dev/null 2>&1; then
    sudo useradd --system --user-group --no-create-home gs-video
fi
# Camera, encoder and microphone device access.
for group in video render audio; do
    if getent group "$group" >/dev/null; then
        sudo usermod -aG "$group" gs-video
    fi
done
sudo install -m 0755 "$BINARY" /usr/local/bin/gs-video-sender
sudo install -d -m 0750 /etc/gs-video-sender
sudo install -d -o gs-video -g gs-video -m 0750 /var/lib/gs-video-sender

if ! sudo test -e "$CONFIG"; then
    sudo install -o gs-video -g gs-video -m 0600 video_sender/config.example.json "$CONFIG"
    # Without a CSI camera the example "front" stream would retry forever.
    if ! { command -v rpicam-hello >/dev/null 2>&1 &&
        rpicam-hello --list-cameras 2>/dev/null | grep -q '^ *0 *:'; }; then
        echo "No CSI camera detected: disabling CSI streams in the new config"
        CSI=off
    else
        CSI=on
    fi
else
    CSI=keep
fi
# Upgrade older configs and apply the CSI decision without disturbing other settings.
sudo python3 - "$CONFIG" "$CSI" video_sender/config.example.json <<'EOF'
import json, os, sys
path, csi, example = sys.argv[1:]
with open(path) as f:
    config = json.load(f)
changed = False
if "usb_cameras" not in config:
    with open(example) as f:
        config["usb_cameras"] = json.load(f)["usb_cameras"]
    print("Added usb_cameras to existing config")
    changed = True
if csi == "off":
    for stream in config.get("streams", []):
        changed |= stream.get("enabled", False)
        stream["enabled"] = False
if changed:
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        json.dump(config, f, indent=2)
        f.write("\n")
    stat = os.stat(path)
    os.chown(tmp, stat.st_uid, stat.st_gid)
    os.chmod(tmp, 0o600)
    os.replace(tmp, path)
EOF

if ! sudo test -s "$ENVIRONMENT"; then
    password=${GS_VIDEO_PASSWORD:-}
    if [ -z "$password" ]; then
        [ -t 0 ] || { echo "set GS_VIDEO_PASSWORD for a non-interactive install" >&2; exit 1; }
        printf 'GroundStation video password: '
        stty -echo
        read -r password
        stty echo
        echo
    fi
    [ -n "$password" ] || { echo "password must not be empty" >&2; exit 1; }
    printf 'GS_VIDEO_PASSWORD=%s\n' "$password" | sudo sh -c "umask 077; cat > '$ENVIRONMENT'"
    unset password
fi
sudo -u gs-video /usr/local/bin/gs-video-sender check "$CONFIG"

step "Enabling gs-video-sender at boot"
sudo install -m 0644 video_sender/gs-video-sender.service /etc/systemd/system/gs-video-sender.service
sudo systemctl daemon-reload
sudo systemctl enable gs-video-sender
sudo systemctl restart gs-video-sender
sleep 3
sudo -u gs-video /usr/local/bin/gs-video-sender status || true
echo
echo "Installed. Logs: sudo journalctl -u gs-video-sender -f"
echo "Status:          sudo -u gs-video gs-video-sender status"
