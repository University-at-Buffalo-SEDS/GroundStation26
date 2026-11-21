#!/usr/bin/env python3
import argparse
import os
import subprocess
import sys
from pathlib import Path


def run(cmd: list[str], cwd: Path, env: dict[str, str] | None = None) -> None:
    print(f"Running: {' '.join(cmd)} (cwd={cwd})")
    subprocess.run(cmd, cwd=cwd, check=True, env=env)


# High-detail regions the downloader can add (map_downloader/src/profile.rs
# SELECTABLE_REGIONS). Rough storage for zoom 9-17 satellite JPEG tiles at
# about 25-40 KB each; actual size depends on the imagery.
REGIONS = [
    ("urrg", "URRG launch field (Potter, NY)", 12_474, (300, 490)),
    ("ub", "UB campus (Buffalo, NY)", 7_246, (180, 280)),
    ("midland", "Midland, TX (city and airport)", 18_668, (460, 730)),
]


def parse_regions(value: str) -> list[str]:
    known = {name for name, *_ in REGIONS}
    picked = []
    for name in (part.strip().lower() for part in value.split(",")):
        if not name:
            continue
        if name not in known:
            raise ValueError(f"unknown region {name!r}; choose from {', '.join(sorted(known))}")
        if name not in picked:
            picked.append(name)
    if not picked:
        raise ValueError("choose at least one region")
    return picked


def describe(names: list[str]) -> str:
    low = sum(size[0] for name, _, _, size in REGIONS if name in names)
    high = sum(size[1] for name, _, _, size in REGIONS if name in names)
    return f"~{low}-{high} MB"


def pick_regions_tui() -> list[str]:
    """Checkbox picker: arrows/j/k move, space toggles, a toggles all, enter confirms."""
    import curses

    def run_picker(screen) -> list[str]:
        curses.curs_set(0)
        chosen = [False] * len(REGIONS)
        at = 0
        message = ""
        while True:
            screen.erase()
            screen.addstr(0, 0, "High-detail map regions (zoom 9-17). Select one or more:")
            for index, (name, label, tiles, (low, high)) in enumerate(REGIONS):
                mark = "[x]" if chosen[index] else "[ ]"
                line = f" {mark} {label:<34} {tiles:>6} tiles  ~{low}-{high} MB"
                screen.addstr(2 + index, 0, line, curses.A_REVERSE if index == at else curses.A_NORMAL)
            selected = [REGIONS[i][0] for i, on in enumerate(chosen) if on]
            total = describe(selected) if selected else "nothing selected"
            screen.addstr(3 + len(REGIONS), 0, f"Total: {total}")
            screen.addstr(5 + len(REGIONS), 0, "space: toggle  a: all  enter: download  q: cancel")
            if message:
                screen.addstr(7 + len(REGIONS), 0, message)
            key = screen.getch()
            if key in (curses.KEY_UP, ord("k")):
                at = (at - 1) % len(REGIONS)
            elif key in (curses.KEY_DOWN, ord("j")):
                at = (at + 1) % len(REGIONS)
            elif key == ord(" "):
                chosen[at] = not chosen[at]
            elif key == ord("a"):
                chosen = [not all(chosen)] * len(REGIONS)
            elif key in (curses.KEY_ENTER, 10, 13):
                if selected:
                    return selected
                message = "Select at least one region (space)."
            elif key in (ord("q"), 27):
                raise KeyboardInterrupt

    return curses.wrapper(run_picker)


def pick_regions_text() -> list[str]:
    print("High-detail map regions (zoom 9-17):")
    for name, label, tiles, (low, high) in REGIONS:
        print(f"  {name:<8} {label:<34} {tiles:>6} tiles  ~{low}-{high} MB")
    while True:
        try:
            return parse_regions(input("Regions to download (comma separated, e.g. urrg,ub): "))
        except ValueError as error:
            print(f"  {error}")


def choose_regions(args: argparse.Namespace) -> list[str] | None:
    """--regions wins; otherwise ask on a terminal. None keeps the defaults."""
    if args.regions is not None:
        return parse_regions(args.regions)
    if args.test or args.repair_tile or not sys.stdin.isatty():
        return None
    try:
        return pick_regions_tui()
    except Exception:
        # No curses (or a terminal too small to draw): ask in plain text.
        return pick_regions_text()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Download offline map tiles.")
    parser.add_argument("--profile", choices=["groundstation", "rrls"], default=None,
                        help="Coverage preset: rrls downloads only UB, URRG and Midland into its own bounded dataset; default keeps normal GroundStation coverage.")
    parser.add_argument("--regions", default=None, metavar="urrg,ub,midland",
                        help="High-detail regions to download, one or more of urrg, ub, midland. "
                             "Without it an interactive picker asks (on a terminal).")
    parser.add_argument("--repair-tile", action="append", default=[], metavar="Z/X/Y",
                        help="Refetch only selected cached coordinates (repeat up to32); preserves each old tile until a valid replacement commits. Cannot combine with --test.")
    parser.add_argument("--test",action="store_true",help="Test a few configured map tiles without creating or writing map data; reports direct and fallback routing.")
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
    parser.add_argument(
        "--direct-interface", nargs="?", const="auto", default=None, metavar="auto|NAME",
        help="Bind map HTTP sockets to a physical interface (auto if omitted), bypassing VPN/proxy; fall back to normal networking if unavailable. Does not change network configuration.",
    )
    parser.add_argument(
        "--min-free-mib", type=int, default=None,
        help="Keep at least this much disk space free during bundle download (default: 2048 MiB).",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    if args.test and args.repair_tile:
        print("Error: --test and --repair-tile cannot be combined; --test never writes map data.", file=sys.stderr)
        sys.exit(2)
    repo_root = Path(__file__).resolve().parent
    env = os.environ.copy()
    if args.profile is not None:
        env["MAP_PROFILE"] = args.profile
    try:
        regions = choose_regions(args)
    except ValueError as error:
        print(f"Error: --regions: {error}", file=sys.stderr)
        sys.exit(2)
    if regions is not None:
        env["MAP_REGIONS"] = ",".join(regions)
        print(f"High-detail regions: {', '.join(regions)} ({describe(regions)})")
    if args.direct_to_db and args.via_tiles:
        print("Error: --direct-to-db and --via-tiles are mutually exclusive.", file=sys.stderr)
        sys.exit(2)
    if args.direct_to_db and args.no_bundle:
        print("Error: --direct-to-db requires bundle generation (remove --no-bundle).", file=sys.stderr)
        sys.exit(2)

    if args.direct_interface is not None:
        env["MAP_DIRECT_INTERFACE"] = args.direct_interface
        print(f"Direct map interface requested: {args.direct_interface}")

    if args.min_free_mib is not None:
        if args.min_free_mib < 0:
            print("Error: --min-free-mib must be nonnegative.", file=sys.stderr)
            sys.exit(2)
        env["MAP_MIN_FREE_MIB"] = str(args.min_free_mib)

    if args.max_concurrent is not None:
        env["MAP_MAX_CONCURRENT"] = str(args.max_concurrent)
        print(f"Using MAP_MAX_CONCURRENT={env['MAP_MAX_CONCURRENT']}")
    if args.max_bandwidth_mibps is not None:
        env["MAP_MAX_BANDWIDTH_MIBPS"] = str(args.max_bandwidth_mibps)
        print(f"Using MAP_MAX_BANDWIDTH_MIBPS={env['MAP_MAX_BANDWIDTH_MIBPS']}")
    if args.no_bundle:
        env["MAP_BUILD_BUNDLE"] = "0"
        env["MAP_DIRECT_TO_BUNDLE"] = "0"
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
        binary_args = ["--test"] if args.test else [arg for tile in args.repair_tile for arg in ("--repair-tile", tile)]
        run(
            ["cargo", "run", "--release", "-p", "map_downloader"] + (["--"] + binary_args if binary_args else []),
            cwd=repo_root,
            env=env,
        )
    except subprocess.CalledProcessError as e:
        print("Map downloader exited with error.", file=sys.stderr)
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
