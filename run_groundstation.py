#!/usr/bin/env python3
import argparse
import signal
import subprocess
import sys
import time
from pathlib import Path

import build


def warn_if_db_sidecars_present(repo_root: Path) -> None:
    db = repo_root / "data" / "groundstation.db"
    sidecars = [
        Path(f"{db}-wal"),
        Path(f"{db}-shm"),
        Path(f"{db}.wal"),
        Path(f"{db}.shm"),
        Path(f"{db}-journal"),
        Path(f"{db}.journal"),
    ]
    # Give the backend a brief window to finish shutdown cleanup.
    deadline = time.monotonic() + 2.0
    lingering = {p for p in sidecars if p.exists()}
    while lingering and time.monotonic() < deadline:
        time.sleep(0.1)
        lingering = {p for p in sidecars if p.exists()}

    if lingering:
        print(
            "Warning: SQLite sidecar files still present after backend exit: "
            + ", ".join(str(p) for p in sorted(lingering)),
            file=sys.stderr,
        )


def run(cmd: list[str], cwd: Path) -> None:
    print(f"Running: {' '.join(cmd)} (cwd={cwd})")
    proc = subprocess.Popen(cmd, cwd=cwd)
    code: int
    try:
        code = proc.wait()
    except KeyboardInterrupt:
        print("\nInterrupt received, requesting graceful backend shutdown...")
        if proc.poll() is None:
            proc.send_signal(signal.SIGINT)
            try:
                code = proc.wait(timeout=12)
            except subprocess.TimeoutExpired:
                proc.terminate()
                try:
                    code = proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    code = proc.wait()
        else:
            code = proc.returncode
    finally:
        warn_if_db_sidecars_present(cwd)

    if code != 0:
        raise subprocess.CalledProcessError(code, cmd)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Build frontend and run groundstation backend."
    )
    parser.add_argument(
        "mode",
        nargs="?",
        choices=["testing"],
        help="Legacy positional mode. Use 'testing' to enable backend testing feature.",
    )
    parser.add_argument(
        "--testing",
        action="store_true",
        help="Enable backend 'testing' feature.",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    testing_mode = args.testing or args.mode == "testing"

    cmd = ["cargo", "run", "--release", "-p", "groundstation_backend"]
    if testing_mode:
        cmd.extend(["--features", "testing"])
    repo_root = Path(__file__).resolve().parent
    build.build_frontend(repo_root / "frontend")
    try:
        run(cmd, cwd=repo_root)
    except subprocess.CalledProcessError as e:
        print("Backend exited with error.", file=sys.stderr)
        sys.exit(e.returncode)


if __name__ == "__main__":
    try:
        main()
    except FileNotFoundError as e:
        missing = e.filename or "<unknown>"
        print("\nError: run_groundstation failed because a required tool/file is missing.", file=sys.stderr)
        print(f"  Missing: {missing}", file=sys.stderr)
        print("Hint: ensure Rust toolchain is installed (`cargo`) and repo paths are valid.", file=sys.stderr)
        sys.exit(127)
    except subprocess.CalledProcessError as e:
        print("\nError: run_groundstation command failed.", file=sys.stderr)
        print(f"  Command : {' '.join(str(x) for x in e.cmd)}", file=sys.stderr)
        print(f"  Exit    : {e.returncode}", file=sys.stderr)
        print("Hint: rerun the printed command directly for full output.", file=sys.stderr)
        sys.exit(e.returncode)
    except Exception as e:
        print(f"\nError: run_groundstation failed unexpectedly: {e}", file=sys.stderr)
        print("Hint: run with a clean build (`python3 build.py`) and retry.", file=sys.stderr)
        sys.exit(1)
    except KeyboardInterrupt:
        print("\n\nexiting...")
        exit(0)
