#!/usr/bin/env python3
"""Manage GroundStation accounts over SSH without Tk or a graphical display."""
import argparse
import sys
from users_config_gui import DEFAULT_USERS_PATH, tui_mode
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--file", type=Path, default=DEFAULT_USERS_PATH,
                        help="The users.json file used by the running backend")
    args = parser.parse_args()
    if not sys.stdin.isatty():
        parser.error("An interactive terminal is required; use ssh -t or users_config_gui.py --cli for scripting")
    return tui_mode(args.file)


if __name__ == "__main__":
    raise SystemExit(main())
