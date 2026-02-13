#!/usr/bin/env python3
import argparse
import csv
import sqlite3
from pathlib import Path

script_dir = Path(__file__).parent.resolve()
def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Export telemetry rows from groundstation.db to a CSV file."
    )
    parser.add_argument(
        "--db",
        default=str(script_dir / Path("data") / "groundstation.db"),
        help="Path to the SQLite DB (default: data/groundstation.db)",
    )
    parser.add_argument(
        "--out",
        default=str(script_dir / "telemetry.csv"),
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
        "SELECT timestamp_ms, data_type, values_json, payload_json "
        "FROM telemetry ORDER BY timestamp_ms"
    )

    with sqlite3.connect(str(db_path)) as conn:
        conn.row_factory = sqlite3.Row
        cursor = conn.execute(query)
        col_names = [col[0] for col in cursor.description]

        with out_path.open("w", newline="") as f:
            writer = csv.writer(f)
            writer.writerow(col_names)
            for row in cursor:
                writer.writerow([row[k] for k in col_names])

    print(f"Wrote telemetry CSV to {out_path}")


if __name__ == "__main__":
    main()
