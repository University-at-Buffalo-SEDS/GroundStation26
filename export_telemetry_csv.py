#!/usr/bin/env python3
import argparse
import csv
import sqlite3
from pathlib import Path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Export telemetry rows from groundstation.db to a CSV file."
    )
    parser.add_argument(
        "--db",
        default=str(Path("backend") / "data" / "groundstation.db"),
        help="Path to the SQLite DB (default: backend/data/groundstation.db)",
    )
    parser.add_argument(
        "--out",
        default="telemetry.csv",
        help="Output CSV path (default: telemetry.csv)",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    db_path = Path(args.db)
    if not db_path.exists():
        raise SystemExit(f"DB not found: {db_path}")

    out_path = Path(args.out)
    out_path.parent.mkdir(parents=True, exist_ok=True)

    query = (
        "SELECT timestamp_ms, data_type, values_json, payload_json, "
        "v0, v1, v2, v3, v4, v5, v6, v7 "
        "FROM telemetry ORDER BY timestamp_ms"
    )

    with sqlite3.connect(str(db_path)) as conn:
        conn.row_factory = sqlite3.Row
        rows = conn.execute(query)

        with out_path.open("w", newline="") as f:
            writer = csv.writer(f)
            writer.writerow(rows.keys())
            for row in rows:
                writer.writerow([row[k] for k in row.keys()])

    print(f"Wrote telemetry CSV to {out_path}")


if __name__ == "__main__":
    main()
