#!/usr/bin/env python3
import subprocess
import sys
from pathlib import Path

import build


def run(cmd: list[str], cwd: Path) -> None:
    print(f"Running: {' '.join(cmd)} (cwd={cwd})")
    subprocess.run(cmd, cwd=cwd, check=True)


def print_usage() -> None:
    print("Usage: run_groundstation.py [testing]")
    sys.exit(1)


def main() -> None:

    testing_mode = False
    args = [a.strip().lower() for a in sys.argv[1:]]
    if len(args) > 3:
        print("Error: Too many arguments.", file=sys.stderr)
        print_usage()

    for arg in args:
        if arg == "testing":
            testing_mode = True
        else:
            print(f"Error: Invalid argument '{arg}'.", file=sys.stderr)
            print_usage()

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
