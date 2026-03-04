#!/usr/bin/env python3
"""
Fast map archive utility for very large map datasets.

Features:
- compress:   pack maps directory into .tar.zst using zstd + multithreading
- decompress: unpack .tar.zst archive locally using zstd + multithreading
- fetch-unpack: copy archive from remote machine and unpack locally

Examples:
  python3 map_archive.py compress \
    --maps-dir backend/data/maps \
    --output /data/maps.tar.zst

  python3 map_archive.py decompress \
    --archive /data/maps.tar.zst \
    --dest backend/data

  python3 map_archive.py fetch-unpack \
    --remote user@host:/data/maps.tar.zst \
    --dest backend/data \
    --local-archive /tmp/maps.tar.zst
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
from pathlib import Path

DEFAULT_MAPS_DIR = Path("backend/data/maps")
DEFAULT_ARCHIVE_NAME = "maps.tar.zst"
DEFAULT_FAST_LEVEL = 100  # Maximum speed mode in zstd fast presets.
DEFAULT_THREADS = 0  # 0 == auto/all cores for zstd.


def _run(cmd: list[str]) -> None:
    print("Running:", " ".join(cmd))
    subprocess.run(cmd, check=True)


def _require_tool(tool: str) -> None:
    if shutil.which(tool) is None:
        raise SystemExit(f"Missing required tool on PATH: {tool}")


def _zstd_prog_compress(threads: int, fast_level: int) -> str:
    # Keep long-distance matching disabled for speed; fastest preset requested.
    return f"zstd -T{threads} --fast={fast_level}"


def _zstd_prog_decompress(threads: int) -> str:
    return f"zstd -d -T{threads}"


def compress_maps(maps_dir: Path, output: Path, threads: int, fast_level: int) -> None:
    _require_tool("tar")
    _require_tool("zstd")

    maps_dir = maps_dir.resolve()
    if not maps_dir.exists() or not maps_dir.is_dir():
        raise SystemExit(f"Maps directory not found: {maps_dir}")

    output = output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)

    # Archive stores the top-level maps directory name to make unpack predictable.
    parent = maps_dir.parent
    root_name = maps_dir.name

    cmd = [
        "tar",
        "-I",
        _zstd_prog_compress(threads=threads, fast_level=fast_level),
        "-cf",
        str(output),
        "-C",
        str(parent),
        root_name,
    ]
    _run(cmd)
    print(f"Created archive: {output}")


def decompress_maps(archive: Path, dest: Path, threads: int) -> None:
    _require_tool("tar")
    _require_tool("zstd")

    archive = archive.resolve()
    if not archive.exists() or not archive.is_file():
        raise SystemExit(f"Archive not found: {archive}")

    dest = dest.resolve()
    dest.mkdir(parents=True, exist_ok=True)

    cmd = [
        "tar",
        "-I",
        _zstd_prog_decompress(threads=threads),
        "-xf",
        str(archive),
        "-C",
        str(dest),
    ]
    _run(cmd)
    print(f"Extracted archive into: {dest}")


def fetch_archive(remote: str, local_archive: Path) -> None:
    local_archive = local_archive.resolve()
    local_archive.parent.mkdir(parents=True, exist_ok=True)

    # Prefer rsync for resume/progress on large transfers; fallback to scp.
    if shutil.which("rsync"):
        _run(["rsync", "-avP", "--partial", remote, str(local_archive)])
        return

    if shutil.which("scp"):
        _run(["scp", remote, str(local_archive)])
        return

    raise SystemExit("Missing transfer tool: install rsync (preferred) or scp")


def fetch_and_unpack(
    remote: str,
    local_archive: Path,
    dest: Path,
    threads: int,
    delete_archive: bool,
) -> None:
    fetch_archive(remote=remote, local_archive=local_archive)
    decompress_maps(archive=local_archive, dest=dest, threads=threads)
    if delete_archive:
        local_archive.unlink(missing_ok=True)
        print(f"Deleted local archive: {local_archive}")


def _build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        description="Compress/decompress/fetch map archives using zstd for maximum speed."
    )
    sub = p.add_subparsers(dest="cmd", required=True)

    p_comp = sub.add_parser("compress", help="Compress maps folder into .tar.zst")
    p_comp.add_argument(
        "--maps-dir",
        type=Path,
        default=DEFAULT_MAPS_DIR,
        help=f"Path to maps directory (default: {DEFAULT_MAPS_DIR})",
    )
    p_comp.add_argument(
        "--output",
        type=Path,
        default=Path(DEFAULT_ARCHIVE_NAME),
        help=f"Output archive path (default: {DEFAULT_ARCHIVE_NAME})",
    )
    p_comp.add_argument(
        "--threads",
        type=int,
        default=DEFAULT_THREADS,
        help="zstd threads (0 = auto/all cores, default: 0)",
    )
    p_comp.add_argument(
        "--fast-level",
        type=int,
        default=DEFAULT_FAST_LEVEL,
        help="zstd --fast level (higher = faster, default: 100)",
    )

    p_decomp = sub.add_parser("decompress", help="Decompress .tar.zst archive")
    p_decomp.add_argument("--archive", type=Path, required=True, help="Input .tar.zst archive")
    p_decomp.add_argument(
        "--dest",
        type=Path,
        default=Path("backend/data"),
        help="Destination directory for extraction (default: backend/data)",
    )
    p_decomp.add_argument(
        "--threads",
        type=int,
        default=DEFAULT_THREADS,
        help="zstd threads for decompression (0 = auto/all cores, default: 0)",
    )

    p_fetch = sub.add_parser(
        "fetch-unpack",
        help="Fetch remote .tar.zst archive and unpack locally",
    )
    p_fetch.add_argument(
        "--remote",
        required=True,
        help="Remote archive spec, e.g. user@host:/path/maps.tar.zst",
    )
    p_fetch.add_argument(
        "--local-archive",
        type=Path,
        default=Path(DEFAULT_ARCHIVE_NAME),
        help=f"Local path to store downloaded archive (default: {DEFAULT_ARCHIVE_NAME})",
    )
    p_fetch.add_argument(
        "--dest",
        type=Path,
        default=Path("backend/data"),
        help="Destination directory for extraction (default: backend/data)",
    )
    p_fetch.add_argument(
        "--threads",
        type=int,
        default=DEFAULT_THREADS,
        help="zstd threads for decompression (0 = auto/all cores, default: 0)",
    )
    p_fetch.add_argument(
        "--delete-archive",
        action="store_true",
        help="Delete downloaded archive after successful extraction",
    )

    return p


def main() -> None:
    args = _build_parser().parse_args()

    if args.cmd == "compress":
        compress_maps(
            maps_dir=args.maps_dir,
            output=args.output,
            threads=args.threads,
            fast_level=args.fast_level,
        )
        return

    if args.cmd == "decompress":
        decompress_maps(archive=args.archive, dest=args.dest, threads=args.threads)
        return

    if args.cmd == "fetch-unpack":
        fetch_and_unpack(
            remote=args.remote,
            local_archive=args.local_archive,
            dest=args.dest,
            threads=args.threads,
            delete_archive=args.delete_archive,
        )
        return

    raise SystemExit(f"Unsupported command: {args.cmd}")


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("\nInterrupted.", file=sys.stderr)
        raise SystemExit(130)
    except subprocess.CalledProcessError as e:
        print("\nCommand failed.", file=sys.stderr)
        print(f"  Exit code: {e.returncode}", file=sys.stderr)
        print(f"  Command: {' '.join(str(x) for x in e.cmd)}", file=sys.stderr)
        raise SystemExit(e.returncode)
    except FileNotFoundError as e:
        missing = e.filename or "<unknown>"
        print(f"\nMissing required file/tool: {missing}", file=sys.stderr)
        raise SystemExit(127)
    except Exception as e:
        print(f"\nUnexpected error: {e}", file=sys.stderr)
        raise SystemExit(1)
