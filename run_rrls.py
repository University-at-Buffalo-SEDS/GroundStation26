#!/usr/bin/env python3
"""Run GroundStation in RRLS mode with the normal launcher's build/run options."""
import runpy
import sys
from pathlib import Path


def main() -> None:
    if "--rrls" not in sys.argv[1:]:
        sys.argv.insert(1, "--rrls")
    runpy.run_path(str(Path(__file__).with_name("run_groundstation.py")), run_name="__main__")


if __name__ == "__main__":
    main()
