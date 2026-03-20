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
    parser.add_argument(
        "--no-bundle",
        action="store_true",
        help="Disable automatic generation of backend/data/maps/<region>/tiles.sqlite3.",
    )
    parser.add_argument(
        "--bundle-path",
        type=Path,
        default=None,
        help="Set MAP_BUNDLE_PATH output path for generated tile sqlite bundle.",
    )
    parser.add_argument(
        "--no-bundle-resume",
        action="store_true",
        help="Disable incremental/resumable bundle updates and rebuild bundle DB from scratch.",
    )
    parser.add_argument(
        "--keep-tiles",
        action="store_true",
        help="Keep backend/data/maps/<region>/tiles after successful bundle build (default removes tiles and keeps "
             "DB).",
    )
    parser.add_argument(
        "--direct-to-db",
        action="store_true",
        help="Download tiles directly into the bundle DB (default behavior).",
    )
    parser.add_argument(
        "--via-tiles",
        action="store_true",
        help="Use legacy flow: download into tiles directory before bundling.",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    repo_root = Path(__file__).resolve().parent
    env = os.environ.copy()
    if args.direct_to_db and args.via_tiles:
        print("Error: --direct-to-db and --via-tiles are mutually exclusive.", file=sys.stderr)
        sys.exit(2)
    if args.direct_to_db and args.no_bundle:
        print("Error: --direct-to-db requires bundle generation (remove --no-bundle).", file=sys.stderr)
        sys.exit(2)

    if args.max_concurrent is not None:
        env["MAP_MAX_CONCURRENT"] = str(args.max_concurrent)
        print(f"Using MAP_MAX_CONCURRENT={env['MAP_MAX_CONCURRENT']}")
    if args.max_bandwidth_mibps is not None:
        env["MAP_MAX_BANDWIDTH_MIBPS"] = str(args.max_bandwidth_mibps)
        print(f"Using MAP_MAX_BANDWIDTH_MIBPS={env['MAP_MAX_BANDWIDTH_MIBPS']}")
    if args.no_bundle:
        env["MAP_BUILD_BUNDLE"] = "0"
        print("Using MAP_BUILD_BUNDLE=0")
    if args.bundle_path is not None:
        env["MAP_BUNDLE_PATH"] = str(args.bundle_path)
        print(f"Using MAP_BUNDLE_PATH={env['MAP_BUNDLE_PATH']}")
    if args.no_bundle_resume:
        env["MAP_BUNDLE_RESUME"] = "0"
        print("Using MAP_BUNDLE_RESUME=0")
    if args.keep_tiles:
        env["MAP_KEEP_TILES"] = "1"
        print("Using MAP_KEEP_TILES=1")
    if args.direct_to_db:
        env["MAP_DIRECT_TO_BUNDLE"] = "1"
        print("Using MAP_DIRECT_TO_BUNDLE=1")
    if args.via_tiles:
        env["MAP_DIRECT_TO_BUNDLE"] = "0"
        print("Using MAP_DIRECT_TO_BUNDLE=0")

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
