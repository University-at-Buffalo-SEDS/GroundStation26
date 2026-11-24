#!/usr/bin/env python3
"""Fail when cargo reports a warning from this repository's own crates.

Runs `cargo check` for each mode below with JSON diagnostics and counts
rustc warnings whose package is a local path (registry and git dependencies
are capped by cargo and are not ours to fix) plus Cargo manifest lints such as
unused dependencies. Build-script notices from dependencies are ignored.

Usage: python3 scripts/check_warnings.py [mode ...]   (default: every mode)
"""
import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MODES = {
    "host": ["--workspace", "--all-targets"],
    "host-firmware-sim": ["--workspace", "--all-targets", "--features", "groundstation_backend/firmware_sim"],
}
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
    names = argv or list(MODES)
    unknown = [n for n in names if n not in MODES]
    if unknown:
        sys.exit(f"unknown mode(s) {unknown}; choose from {list(MODES)}")
    ok = all([check(n, MODES[n]) for n in names])
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main(sys.argv[1:])
