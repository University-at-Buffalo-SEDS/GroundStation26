#!/bin/bash

set -e
set -o pipefail
set -u
set -x
set -m
export DEBIAN_FRONTEND=noninteractive
export TZ=Etc/Eastern

/app/map_downloader

/app/groundstation_backend