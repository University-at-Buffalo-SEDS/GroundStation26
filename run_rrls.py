#!/usr/bin/env python3
"""Run GroundStation in RRLS mode with the normal launcher's build/run options."""
import argparse
import os
import runpy
import sys
from pathlib import Path


def main() -> None:
    parser=argparse.ArgumentParser(add_help=False)
    parser.add_argument("--radio-port")
    parser.add_argument("--comms-config")
    args,remaining=parser.parse_known_args(sys.argv[1:])
    if args.radio_port:
        os.environ["GS_RRLS_RADIO_PORT"]=args.radio_port
    if args.comms_config:
        os.environ["GS_RRLS_COMMS_LINK_CONFIG"]=str(Path(args.comms_config).resolve())
    sys.argv[1:]=remaining
    if "--help" in remaining or "-h" in remaining:
        print("RRLS options: --radio-port DEVICE (saved in RRLS config), --comms-config FILE")
    if "--rrls" not in sys.argv[1:]:
        sys.argv.insert(1, "--rrls")
    runpy.run_path(str(Path(__file__).with_name("run_groundstation.py")), run_name="__main__")


if __name__ == "__main__":
    main()
