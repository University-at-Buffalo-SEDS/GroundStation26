#!/usr/bin/env python3
import argparse
import os
import subprocess
import sys
from pathlib import Path


def run(cmd: list[str], cwd: Path, env: dict[str, str] | None = None) -> None:
    print(f"Running: {' '.join(cmd)} (cwd={cwd})")
    subprocess.run(cmd, cwd=cwd, check=True, env=env)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Download offline map tiles.")
    parser.add_argument(
        "--max-concurrent",
        type=int,
        default=None,
        help="Set MAP_MAX_CONCURRENT (e.g. 64, 128, 256).",
    )
    parser.add_argument(
        "--max-bandwidth-mibps",
        type=float,
        default=None,
        help="Set MAP_MAX_BANDWIDTH_MIBPS (MiB/s). <=0 disables cap.",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    repo_root = Path(__file__).resolve().parent
    env = os.environ.copy()

    if args.max_concurrent is not None:
        env["MAP_MAX_CONCURRENT"] = str(args.max_concurrent)
        print(f"Using MAP_MAX_CONCURRENT={env['MAP_MAX_CONCURRENT']}")
    if args.max_bandwidth_mibps is not None:
        env["MAP_MAX_BANDWIDTH_MIBPS"] = str(args.max_bandwidth_mibps)
        print(f"Using MAP_MAX_BANDWIDTH_MIBPS={env['MAP_MAX_BANDWIDTH_MIBPS']}")

    try:
        run(
            ["cargo", "run", "--release", "-p", "map_downloader"],
            cwd=repo_root,
            env=env,
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
