#!/usr/bin/env python3
"""Fail when cargo reports a warning from this repository's own crates.

Runs `cargo check` for each mode below with JSON diagnostics and counts
rustc warnings whose package is a local path (registry and git dependencies
are capped by cargo and are not ours to fix) plus Cargo manifest lints such as
unused dependencies. Build-script notices from dependencies are ignored.

The Raspberry Pi modes build the `raspberry_pi` feature (rppal, Linux only):
"pi" on the host, "pi-aarch64" for the Pi's own target (needs the
aarch64-unknown-linux-gnu Rust target and an aarch64 C cross compiler). They
are skipped on other host OSes unless named explicitly.

Usage: python3 scripts/check_warnings.py [mode ...]   (default: every mode)
"""
import json
import platform
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MODES = {
    "host": ["--workspace", "--all-targets"],
    "host-firmware-sim": ["--workspace", "--all-targets", "--features", "groundstation_backend/firmware_sim"],
    "pi": ["--workspace", "--all-targets", "--features", "groundstation_backend/raspberry_pi"],
    "pi-aarch64": [
        "--workspace", "--all-targets", "--features", "groundstation_backend/raspberry_pi",
        "--target", "aarch64-unknown-linux-gnu",
    ],
}
LINUX_ONLY = {"pi", "pi-aarch64"}
# Cargo's own lints on Cargo.toml ("unused dependency", "unused field ...").
MANIFEST_LINT_PREFIXES = ("warning: unused dependency", "warning: unused field")


def check(name, args):
    cmd = ["cargo", "check", "--message-format=json", *args]
    proc = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True)
    found = []
    for line in proc.stdout.splitlines():
        if not line.startswith("{"):
            continue
        msg = json.loads(line)
        if msg.get("reason") != "compiler-message":
            continue
        diag = msg["message"]
        local = msg.get("package_id", "").startswith("path+")
        if local and diag["level"] == "warning" and diag.get("spans"):
            found.append(diag["rendered"].rstrip())
    found += [l for l in proc.stderr.splitlines() if l.startswith(MANIFEST_LINT_PREFIXES)]
    if proc.returncode:
        print(proc.stderr[-4000:], file=sys.stderr)
    print(f"{name}: exit={proc.returncode} warnings={len(found)}")
    for item in found:
        print(item)
    return proc.returncode == 0 and not found


def main(argv):
    names = argv or [n for n in MODES if n not in LINUX_ONLY or platform.system() == "Linux"]
    unknown = [n for n in names if n not in MODES]
    if unknown:
        sys.exit(f"unknown mode(s) {unknown}; choose from {list(MODES)}")
    ok = all([check(n, MODES[n]) for n in names])
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main(sys.argv[1:])
