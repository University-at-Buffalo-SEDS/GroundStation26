#!/bin/bash

set -e
set -o pipefail
set -u
set -x
set -m
export DEBIAN_FRONTEND=noninteractive
export TZ=Etc/Eastern

python3 download_map.py
/app/groundstation_backend