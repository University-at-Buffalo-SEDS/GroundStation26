#!/usr/bin/env python3
"""Build/extract SQLite tile bundles for backend map serving.

Optimized schema (deduplicated image blobs):
    tile_blobs(id INTEGER PRIMARY KEY, hash BLOB UNIQUE, image BLOB)
    tiles(z INTEGER, x INTEGER, y INTEGER, blob_id INTEGER, PRIMARY KEY(z,x,y)) WITHOUT ROWID
"""

from __future__ import annotations

import argparse
from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait
import hashlib
import math
import os
import sqlite3
import shutil
import sys
import time
from pathlib import Path


DEFAULT_REGION = "north_america"
DEFAULT_MAP_ROOT = Path("backend/data/maps")
DEFAULT_WORKERS = max(1, min(8, (os.cpu_count() - 1) or 1))
DEFAULT_COMMIT_EVERY = 10000
BASE_COVERAGE_MAX_ZOOM = 8
NA_BOUNDS = (-170.0, 5.0, -50.0, 83.0)
BUFFALO_ROCHESTER_BOUNDS = (-79.30, 42.70, -77.25, 43.40)
TEXAS_DESERT_BOUNDS = (-106.80, 29.00, -101.00, 32.60)


def clamp_lat(lat: float) -> float:
    return max(min(lat, 85.05112878), -85.05112878)


def lon_lat_to_tile(lon: float, lat: float, z: int) -> tuple[int, int]:
    n = 1 << z
    x = int((lon + 180.0) / 360.0 * n)
    lat_rad = math.radians(clamp_lat(lat))
    y = int((1.0 - math.log(math.tan(lat_rad) + 1.0 / math.cos(lat_rad)) / math.pi) / 2.0 * n)
    x = max(0, min(n - 1, x))
    y = max(0, min(n - 1, y))
    return x, y


def tile_range_for_bounds(bbox: tuple[float, float, float, float], z: int) -> tuple[int, int, int, int]:
    lon_min, lat_min, lon_max, lat_max = bbox
    x1, y1 = lon_lat_to_tile(lon_min, lat_max, z)
    x2, y2 = lon_lat_to_tile(lon_max, lat_min, z)
    return min(x1, x2), max(x1, x2), min(y1, y2), max(y1, y2)


def tile_in_coverage_bounds(z: int, x: int, y: int) -> bool:
    bboxes = [NA_BOUNDS] if z <= BASE_COVERAGE_MAX_ZOOM else [BUFFALO_ROCHESTER_BOUNDS, TEXAS_DESERT_BOUNDS]
    for bbox in bboxes:
        x_min, x_max, y_min, y_max = tile_range_for_bounds(bbox, z)
        if x_min <= x <= x_max and y_min <= y <= y_max:
            return True
    return False


def iter_tile_files(
    tiles_dir: Path,
    min_zoom: int | None = None,
    max_zoom: int | None = None,
    match_downloader_bounds: bool = False,
):
    with os.scandir(tiles_dir) as z_iter:
        for z_entry in z_iter:
            if not z_entry.is_dir(follow_symlinks=False):
                continue
            try:
                z = int(z_entry.name)
            except ValueError:
                continue
            if min_zoom is not None and z < min_zoom:
                continue
            if max_zoom is not None and z > max_zoom:
                continue
            with os.scandir(z_entry.path) as x_iter:
                for x_entry in x_iter:
                    if not x_entry.is_dir(follow_symlinks=False):
                        continue
                    try:
                        x = int(x_entry.name)
                    except ValueError:
                        continue
                    with os.scandir(x_entry.path) as y_iter:
                        for y_entry in y_iter:
                            if not y_entry.is_file(follow_symlinks=False):
                                continue
                            name = y_entry.name
                            if not name.lower().endswith(".jpg"):
                                continue
                            stem = name[:-4]
                            try:
                                y = int(stem)
                            except ValueError:
                                continue
                            if match_downloader_bounds and not tile_in_coverage_bounds(z, x, y):
                                continue
                            yield z, x, y, Path(y_entry.path)


def prepare_tile(record: tuple[int, int, int, Path]) -> tuple[int, int, int, bytes, bytes]:
    z, x, y, tile_path = record
    data = tile_path.read_bytes()
    h = hashlib.blake2b(data, digest_size=16).digest()
    return z, x, y, h, data


def count_tiles(
    tiles_dir: Path,
    min_zoom: int | None = None,
    max_zoom: int | None = None,
    match_downloader_bounds: bool = False,
) -> int:
    total = 0
    scanned_files = 0
    start_t = time.time()
    last_print_t = start_t
    with os.scandir(tiles_dir) as z_iter:
        for z_entry in z_iter:
            if not z_entry.is_dir(follow_symlinks=False):
                continue
            try:
                z = int(z_entry.name)
            except ValueError:
                continue
            if min_zoom is not None and z < min_zoom:
                continue
            if max_zoom is not None and z > max_zoom:
                continue
            with os.scandir(z_entry.path) as x_iter:
                for x_entry in x_iter:
                    if not x_entry.is_dir(follow_symlinks=False):
                        continue
                    with os.scandir(x_entry.path) as y_iter:
                        for tile in y_iter:
                            if not tile.is_file(follow_symlinks=False):
                                continue
                            scanned_files += 1
                            if tile.name.lower().endswith(".jpg"):
                                if match_downloader_bounds:
                                    stem = tile.name[:-4]
                                    try:
                                        y = int(stem)
                                    except ValueError:
                                        continue
                                    try:
                                        x = int(x_entry.name)
                                    except ValueError:
                                        continue
                                    if not tile_in_coverage_bounds(z, x, y):
                                        continue
                                total += 1
                            now = time.time()
                            if scanned_files % 10000 == 0 or (now - last_print_t) >= 1.0:
                                elapsed = max(now - start_t, 0.001)
                                rate = scanned_files / elapsed
                                sys.stdout.write(
                                    f"\rcounting tiles... scanned={scanned_files:,} jpg={total:,} rate={rate:,.0f}/s"
                                )
                                sys.stdout.flush()
                                last_print_t = now
    if scanned_files > 0:
        sys.stdout.write(
            f"\rcounting tiles... scanned={scanned_files:,} jpg={total:,} rate={scanned_files/max(time.time()-start_t,0.001):,.0f}/s"
        )
        sys.stdout.flush()
        print()
    return total


def render_progress(prefix: str, done: int, total: int, start_t: float) -> str:
    elapsed = max(time.time() - start_t, 0.001)
    pct = 100.0 if total <= 0 else (done * 100.0 / total)
    rate = done / elapsed
    remain = max(total - done, 0)
    eta = int(remain / max(rate, 0.001))
    eta_m, eta_s = divmod(eta, 60)
    return (
        f"{prefix}: {pct:6.2f}% ({done}/{total}) "
        f"{rate:,.1f} tiles/s ETA {eta_m:02d}:{eta_s:02d}"
    )

def render_progress_bar(prefix: str, done: int, total: int, start_t: float, unique_blobs: int | None = None) -> str:
    elapsed = max(time.time() - start_t, 0.001)
    pct = 100.0 if total <= 0 else (done * 100.0 / total)
    rate = done / elapsed
    remain = max(total - done, 0)
    eta = int(remain / max(rate, 0.001))
    eta_m, eta_s = divmod(eta, 60)

    cols = shutil.get_terminal_size((120, 20)).columns
    bar_width = max(10, min(50, cols - 85))
    fill = 0 if total <= 0 else int((done / total) * bar_width)
    bar = "#" * fill + "-" * (bar_width - fill)
    extra = f" unique={unique_blobs:,}" if unique_blobs is not None else ""
    return (
        f"\r{prefix} [{bar}] {pct:6.2f}% "
        f"{done:,}/{total:,} {rate:,.1f}/s ETA {eta_m:02d}:{eta_s:02d}{extra}"
    )

def print_progress_bar(prefix: str, done: int, total: int, start_t: float, unique_blobs: int | None = None) -> None:
    line = render_progress_bar(prefix, done, total, start_t, unique_blobs)
    if not hasattr(print_progress_bar, "_last_len"):
        print_progress_bar._last_len = 0  # type: ignore[attr-defined]
    last_len = int(print_progress_bar._last_len)  # type: ignore[attr-defined]
    pad = " " * max(0, last_len - len(line))
    sys.stdout.write(line + pad)
    sys.stdout.flush()
    print_progress_bar._last_len = len(line)  # type: ignore[attr-defined]


def configure_conn(conn: sqlite3.Connection) -> None:
    conn.executescript(
        """
        PRAGMA journal_mode=OFF;
        PRAGMA synchronous=OFF;
        PRAGMA locking_mode=EXCLUSIVE;
        PRAGMA temp_store=MEMORY;
        PRAGMA cache_size=-262144;
        PRAGMA page_size=8192;
        """
    )


def ensure_dedup_schema(conn: sqlite3.Connection) -> None:
    conn.executescript(
        """
        CREATE TABLE IF NOT EXISTS tile_blobs (
            id INTEGER PRIMARY KEY,
            hash BLOB NOT NULL UNIQUE,
            image BLOB NOT NULL
        );
        CREATE TABLE IF NOT EXISTS tiles (
            z INTEGER NOT NULL,
            x INTEGER NOT NULL,
            y INTEGER NOT NULL,
            blob_id INTEGER NOT NULL,
            PRIMARY KEY (z, x, y)
        ) WITHOUT ROWID;
        """
    )


def detect_legacy_inline_schema(conn: sqlite3.Connection) -> bool:
    rows = conn.execute("PRAGMA table_info(tiles)").fetchall()
    names = {str(r[1]) for r in rows}
    return "image" in names and "blob_id" not in names


def build_bundle(
    tiles_dir: Path,
    bundle: Path,
    remove_source: bool,
    workers: int,
    commit_every: int,
    max_in_flight: int | None,
    no_vacuum: bool,
    resume: bool,
    min_zoom: int | None,
    max_zoom: int | None,
    match_downloader_bounds: bool,
) -> None:
    if not tiles_dir.exists() or not tiles_dir.is_dir():
        raise SystemExit(f"tiles directory not found: {tiles_dir}")

    bundle = bundle.resolve()
    bundle.parent.mkdir(parents=True, exist_ok=True)
    tmp_bundle = bundle.with_suffix(bundle.suffix + ".tmp")
    db_path = tmp_bundle
    resumed = False
    if resume:
        if bundle.exists():
            db_path = bundle
            resumed = True
        elif tmp_bundle.exists():
            db_path = tmp_bundle
            resumed = True
    else:
        if tmp_bundle.exists():
            tmp_bundle.unlink()

    print(f"building bundle: {tiles_dir} -> {bundle}")
    if resumed:
        print(f"resume enabled: continuing existing database at {db_path}")
    conn = sqlite3.connect(db_path)
    configure_conn(conn)
    conn.execute(f"PRAGMA threads={max(1, workers)}")
    ensure_dedup_schema(conn)

    total_tiles = count_tiles(
        tiles_dir,
        min_zoom=min_zoom,
        max_zoom=max_zoom,
        match_downloader_bounds=match_downloader_bounds,
    )
    print(f"found {total_tiles:,} tiles to bundle")
    existing_tiles = int(conn.execute("SELECT COUNT(*) FROM tiles").fetchone()[0])
    if existing_tiles > 0:
        print(f"existing rows in bundle: {existing_tiles:,}")
    print(f"starting bundle writes with workers={workers}")

    inserted_tiles = existing_tiles
    unique_blobs = 0
    start_t = time.time()
    last_print_t = start_t

    cur = conn.cursor()
    source_iter = iter_tile_files(
        tiles_dir,
        min_zoom=min_zoom,
        max_zoom=max_zoom,
        match_downloader_bounds=match_downloader_bounds,
    )
    if existing_tiles > 0:
        base_iter = source_iter
        check_cur = conn.cursor()

        def missing_tiles():
            for z, x, y, tile_path in base_iter:
                if check_cur.execute(
                    "SELECT 1 FROM tiles WHERE z = ? AND x = ? AND y = ?",
                    (z, x, y),
                ).fetchone() is not None:
                    continue
                yield z, x, y, tile_path

        source_iter = missing_tiles()
    if workers <= 1:
        prepared_iter = (prepare_tile(r) for r in source_iter)
        executor = None
    else:
        executor = ThreadPoolExecutor(max_workers=workers)
        in_flight_limit = max_in_flight if max_in_flight is not None else max(workers * 2, 16)

        def bounded_prepared():
            pending = set()
            source_exhausted = False
            while True:
                while not source_exhausted and len(pending) < in_flight_limit:
                    try:
                        rec = next(source_iter)
                    except StopIteration:
                        source_exhausted = True
                        break
                    pending.add(executor.submit(prepare_tile, rec))

                if not pending:
                    break

                done, pending = wait(pending, return_when=FIRST_COMPLETED)
                for fut in done:
                    yield fut.result()

        prepared_iter = bounded_prepared()

    conn.execute("BEGIN")
    try:
        for z, x, y, h, data in prepared_iter:
            row = cur.execute(
                """
                INSERT INTO tile_blobs (hash, image) VALUES (?, ?)
                ON CONFLICT(hash) DO UPDATE SET hash = excluded.hash
                RETURNING id
                """,
                (h, data),
            ).fetchone()
            if row is None:
                raise RuntimeError("failed to resolve blob id from upsert")
            blob_id = int(row[0])

            cur.execute(
                "INSERT OR REPLACE INTO tiles (z, x, y, blob_id) VALUES (?, ?, ?, ?)",
                (z, x, y, blob_id),
            )

            inserted_tiles += 1
            if inserted_tiles % commit_every == 0:
                conn.commit()
                conn.execute("BEGIN")
            now = time.time()
            if inserted_tiles % 5000 == 0 or (now - last_print_t) >= 1.0:
                print_progress_bar("bundle", inserted_tiles, total_tiles, start_t)
                last_print_t = now
        conn.commit()
    finally:
        if executor is not None:
            executor.shutdown(wait=True)

    if no_vacuum:
        conn.executescript("ANALYZE; PRAGMA optimize;")
    else:
        conn.executescript("ANALYZE; PRAGMA optimize; VACUUM;")
    unique_blobs = int(conn.execute("SELECT COUNT(*) FROM tile_blobs").fetchone()[0])
    conn.close()

    if db_path != bundle:
        if bundle.exists():
            bundle.unlink()
        db_path.rename(bundle)
    print_progress_bar("bundle", inserted_tiles, total_tiles, start_t, unique_blobs)
    print()
    print(
        f"bundle complete: {bundle} ({inserted_tiles} tiles, {unique_blobs} unique blobs)"
    )

    if remove_source:
        print(f"removing source tiles directory: {tiles_dir}")
        for p in sorted(tiles_dir.rglob("*"), reverse=True):
            if p.is_file() or p.is_symlink():
                p.unlink()
            elif p.is_dir():
                p.rmdir()
        tiles_dir.rmdir()


def extract_bundle(bundle: Path, output_dir: Path) -> None:
    if not bundle.exists() or not bundle.is_file():
        raise SystemExit(f"bundle not found: {bundle}")

    output_dir.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(bundle)

    total_rows = int(conn.execute("SELECT COUNT(*) FROM tiles").fetchone()[0])
    print(f"found {total_rows:,} tiles to extract")

    if detect_legacy_inline_schema(conn):
        rows = conn.execute("SELECT z, x, y, image FROM tiles ORDER BY z, x, y")
    else:
        rows = conn.execute(
            """
            SELECT t.z, t.x, t.y, b.image
            FROM tiles t
            JOIN tile_blobs b ON b.id = t.blob_id
            ORDER BY t.z, t.x, t.y
            """
        )

    extracted = 0
    start_t = time.time()
    last_print_t = start_t
    for z, x, y, image in rows:
        out = output_dir / str(z) / str(x) / f"{y}.jpg"
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_bytes(image)
        extracted += 1
        now = time.time()
        if extracted % 5000 == 0 or (now - last_print_t) >= 1.0:
            print_progress_bar("extract", extracted, total_rows, start_t)
            last_print_t = now

    conn.close()
    print_progress_bar("extract", extracted, total_rows, start_t)
    print()
    print(f"extract complete: {output_dir} ({extracted} tiles)")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Build/extract map tile SQLite bundles.")
    sub = parser.add_subparsers(dest="cmd", required=True)

    p_build = sub.add_parser("build", help="Build a tiles.sqlite bundle from tiles directory")
    p_build.add_argument(
        "--region",
        default=DEFAULT_REGION,
        help=f"Map region under {DEFAULT_MAP_ROOT} (default: {DEFAULT_REGION})",
    )
    p_build.add_argument(
        "--tiles-dir",
        type=Path,
        default=None,
        help="Tiles directory override (default: backend/data/maps/<region>/tiles)",
    )
    p_build.add_argument(
        "--bundle",
        type=Path,
        default=None,
        help="Output bundle path override (default: backend/data/maps/<region>/tiles.sqlite)",
    )
    p_build.add_argument(
        "--remove-source",
        action="store_true",
        help="Delete source tiles directory after successful build",
    )
    p_build.add_argument(
        "--workers",
        type=int,
        default=DEFAULT_WORKERS,
        help=f"Parallel read/hash workers (default: {DEFAULT_WORKERS})",
    )
    p_build.add_argument(
        "--commit-every",
        type=int,
        default=DEFAULT_COMMIT_EVERY,
        help=f"Commit interval in rows to cap memory usage (default: {DEFAULT_COMMIT_EVERY})",
    )
    p_build.add_argument(
        "--max-in-flight",
        type=int,
        default=None,
        help="Maximum prepared tiles buffered from worker threads (default: workers*2).",
    )
    p_build.add_argument(
        "--no-vacuum",
        action="store_true",
        help="Skip final VACUUM to reduce memory and temp-disk pressure.",
    )
    p_build.add_argument(
        "--no-resume",
        action="store_true",
        help="Disable resume behavior and rebuild from scratch.",
    )
    p_build.add_argument(
        "--min-zoom",
        type=int,
        default=None,
        help="Only include tiles with z >= min-zoom.",
    )
    p_build.add_argument(
        "--max-zoom",
        type=int,
        default=None,
        help="Only include tiles with z <= max-zoom.",
    )
    p_build.add_argument(
        "--match-downloader-bounds",
        action="store_true",
        help="Only include tiles within downloader coverage bounds (NA low zoom; Buffalo/Rochester + West Texas high zoom).",
    )

    p_extract = sub.add_parser("extract", help="Extract tiles from a tiles.sqlite bundle")
    p_extract.add_argument("--bundle", type=Path, required=True, help="Path to bundle sqlite file")
    p_extract.add_argument(
        "--output-dir",
        type=Path,
        required=True,
        help="Destination tiles directory",
    )

    return parser.parse_args()


def main() -> None:
    args = parse_args()
    if args.cmd == "build":
        if (
            args.min_zoom is not None
            and args.max_zoom is not None
            and args.min_zoom > args.max_zoom
        ):
            raise SystemExit("--min-zoom cannot be greater than --max-zoom")
        region_root = DEFAULT_MAP_ROOT / args.region
        tiles_dir = args.tiles_dir or (region_root / "tiles")
        bundle = args.bundle or (region_root / "tiles.sqlite")
        build_bundle(
            tiles_dir=tiles_dir,
            bundle=bundle,
            remove_source=args.remove_source,
            workers=max(1, args.workers),
            commit_every=max(1, args.commit_every),
            max_in_flight=(max(1, args.max_in_flight) if args.max_in_flight is not None else None),
            no_vacuum=args.no_vacuum,
            resume=not args.no_resume,
            min_zoom=args.min_zoom,
            max_zoom=args.max_zoom,
            match_downloader_bounds=args.match_downloader_bounds,
        )
        return

    if args.cmd == "extract":
        extract_bundle(bundle=args.bundle, output_dir=args.output_dir)
        return

    raise SystemExit(f"unknown command: {args.cmd}")


if __name__ == "__main__":
    main()
