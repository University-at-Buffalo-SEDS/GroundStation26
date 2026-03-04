#!/usr/bin/env python3
import argparse
import subprocess
import sys
from pathlib import Path

import build


def run(cmd: list[str], cwd: Path) -> None:
    print(f"Running: {' '.join(cmd)} (cwd={cwd})")
    subprocess.run(cmd, cwd=cwd, check=True)


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
        run(
            cmd,
            cwd=repo_root,
        )
    except subprocess.CalledProcessError as e:
        print("Backend exited with error.", file=sys.stderr)
        sys.exit(e.returncode)

    except KeyboardInterrupt:
        print("\n\nexiting...")
        exit(0)


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
