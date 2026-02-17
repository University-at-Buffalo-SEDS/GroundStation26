#!/usr/bin/env python3
import subprocess
import sys
from pathlib import Path


def run(cmd: list[str], cwd: Path) -> None:
    print(f"Running: {' '.join(cmd)} (cwd={cwd})")
    subprocess.run(cmd, cwd=cwd, check=True)


def main() -> None:
    repo_root = Path(__file__).resolve().parent
    try:
        run(
            ["cargo", "run", "--release", "-p", "map_downloader"],
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
        print("\nError: download_map failed because a required tool/file is missing.", file=sys.stderr)
        print(f"  Missing: {missing}", file=sys.stderr)
        print("Hint: ensure `cargo` is installed and the `map_downloader` crate exists.", file=sys.stderr)
        sys.exit(127)
    except subprocess.CalledProcessError as e:
        print("\nError: map download command failed.", file=sys.stderr)
        print(f"  Command : {' '.join(str(x) for x in e.cmd)}", file=sys.stderr)
        print(f"  Exit    : {e.returncode}", file=sys.stderr)
        print("Hint: rerun the command directly to inspect detailed error output.", file=sys.stderr)
        sys.exit(e.returncode)
    except Exception as e:
        print(f"\nError: download_map failed unexpectedly: {e}", file=sys.stderr)
        sys.exit(1)
    except KeyboardInterrupt:
        print("\n\nexiting...")
        exit(0)
