#!/usr/bin/env python3
"""Build/extract SQLite tile bundles for backend map serving.

Optimized schema (deduplicated image blobs):
    tile_blobs(id INTEGER PRIMARY KEY, hash BLOB UNIQUE, image BLOB)
    tiles(z INTEGER, x INTEGER, y INTEGER, blob_id INTEGER, PRIMARY KEY(z,x,y)) WITHOUT ROWID
"""

from __future__ import annotations

import argparse
import hashlib
import sqlite3
from pathlib import Path


DEFAULT_REGION = "north_america"
DEFAULT_MAP_ROOT = Path("backend/data/maps")


def iter_tile_files(tiles_dir: Path):
    for z_dir in sorted((p for p in tiles_dir.iterdir() if p.is_dir()), key=lambda p: p.name):
        try:
            z = int(z_dir.name)
        except ValueError:
            continue
        for x_dir in sorted((p for p in z_dir.iterdir() if p.is_dir()), key=lambda p: p.name):
            try:
                x = int(x_dir.name)
            except ValueError:
                continue
            for tile in sorted((p for p in x_dir.iterdir() if p.is_file()), key=lambda p: p.name):
                if tile.suffix.lower() != ".jpg":
                    continue
                try:
                    y = int(tile.stem)
                except ValueError:
                    continue
                yield z, x, y, tile


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


def build_bundle(tiles_dir: Path, bundle: Path, remove_source: bool) -> None:
    if not tiles_dir.exists() or not tiles_dir.is_dir():
        raise SystemExit(f"tiles directory not found: {tiles_dir}")

    bundle.parent.mkdir(parents=True, exist_ok=True)
    tmp_bundle = bundle.with_suffix(bundle.suffix + ".tmp")
    if tmp_bundle.exists():
        tmp_bundle.unlink()

    print(f"building bundle: {tiles_dir} -> {bundle}")
    conn = sqlite3.connect(tmp_bundle)
    configure_conn(conn)
    ensure_dedup_schema(conn)

    inserted_tiles = 0
    unique_blobs = 0
    hash_to_blob_id: dict[bytes, int] = {}

    with conn:
        cur = conn.cursor()
        for z, x, y, tile_path in iter_tile_files(tiles_dir):
            data = tile_path.read_bytes()
            h = hashlib.blake2b(data, digest_size=16).digest()

            blob_id = hash_to_blob_id.get(h)
            if blob_id is None:
                cur.execute(
                    "INSERT OR IGNORE INTO tile_blobs (hash, image) VALUES (?, ?)",
                    (h, data),
                )
                if cur.rowcount == 1:
                    blob_id = int(cur.lastrowid)
                    unique_blobs += 1
                else:
                    row = cur.execute(
                        "SELECT id FROM tile_blobs WHERE hash = ?",
                        (h,),
                    ).fetchone()
                    if row is None:
                        raise RuntimeError("failed to resolve blob_id after insert/ignore")
                    blob_id = int(row[0])
                hash_to_blob_id[h] = blob_id

            cur.execute(
                "INSERT OR REPLACE INTO tiles (z, x, y, blob_id) VALUES (?, ?, ?, ?)",
                (z, x, y, blob_id),
            )

            inserted_tiles += 1
            if inserted_tiles % 50000 == 0:
                print(
                    f"bundle progress: inserted {inserted_tiles} tiles ({unique_blobs} unique blobs)"
                )

    conn.executescript("ANALYZE; PRAGMA optimize; VACUUM;")
    conn.close()

    if bundle.exists():
        bundle.unlink()
    tmp_bundle.rename(bundle)
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
    for z, x, y, image in rows:
        out = output_dir / str(z) / str(x) / f"{y}.jpg"
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_bytes(image)
        extracted += 1
        if extracted % 50000 == 0:
            print(f"extract progress: {extracted} tiles")

    conn.close()
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
        region_root = DEFAULT_MAP_ROOT / args.region
        tiles_dir = args.tiles_dir or (region_root / "tiles")
        bundle = args.bundle or (region_root / "tiles.sqlite")
        build_bundle(tiles_dir=tiles_dir, bundle=bundle, remove_source=args.remove_source)
        return

    if args.cmd == "extract":
        extract_bundle(bundle=args.bundle, output_dir=args.output_dir)
        return

    raise SystemExit(f"unknown command: {args.cmd}")


if __name__ == "__main__":
    main()
