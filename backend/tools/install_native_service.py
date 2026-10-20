#!/usr/bin/env python3
"""Install native HTTPS/Ethernet settings for an existing GroundStation service."""
import argparse
import os
from pathlib import Path
import re
import subprocess


def drop_in(service):
    if not re.fullmatch(r"[A-Za-z0-9_.@-]+\.service", service):
        raise ValueError("expected a systemd .service name")
    return Path("/etc/systemd/system") / (service + ".d") / "90-groundstation-native.conf"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--service", default="sed-ground-station.service", help="existing service to configure")
    args = parser.parse_args()
    path = drop_in(args.service)
    if os.geteuid() != 0:
        parser.error("run with sudo to install the systemd drop-in")
    subprocess.run(["/usr/bin/systemctl", "cat", args.service], check=True, stdout=subprocess.DEVNULL)
    text = "# Managed by GroundStation native service installer\n[Service]\nUser=root\nEnvironment=GS_HTTPS_BIND_ADDRESS=0.0.0.0:443\n"
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.is_symlink() or (path.exists() and not path.read_text().startswith("# Managed by GroundStation native service installer")):
        parser.error(f"preserving administrator-managed file: {path}")
    path.write_text(text)
    path.chmod(0o644)
    subprocess.run(["/usr/bin/systemctl", "daemon-reload"], check=True)
    print(f"Installed {path}. Restart {args.service} when ready.")


if __name__ == "__main__":
    main()
