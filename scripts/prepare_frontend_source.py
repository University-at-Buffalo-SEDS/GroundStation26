#!/usr/bin/env python3
"""Resolve frontend-owned backend assets without a checkout inside this repo."""
import argparse
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
import build


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--frontend-dev", action="store_true")
    args = parser.parse_args()
    source = build.prepare_browser_asset_source(
        ROOT, args.frontend_dev, refresh=args.frontend_dev
    )
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps({"version": 1, "source_dir": str(source)}) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
