# Offline map data

The terrain database is runtime data, excluded from Git by the `data/` ignore
rule. Pulling the repository or rebuilding does not install or restore it.

From the GroundStation repository root, run `python3 download_map.py` to populate
`backend/data/maps/north_america/tiles.sqlite3`. The downloader resumes existing
work. Its default coverage includes North America and higher-detail launch and
local regions. Do not delete the database to rebuild the application.

GroundStation and RRLS serve their own local map data to all connected web and
native clients. Docker mounts `./backend/data` into `/app/backend/data`; native
systemd installations must use the repository root as their working directory.

The backend detects a database downloaded after startup. An absent or unfinished
bundle is retried rather than permanently cached as missing or as the wrong
SQLite schema. Legacy JPEG directories remain supported. Clients retry visible
missing tiles at a bounded interval without disabling zoom-out; already loaded
tiles reuse their caches. Initial maps still require the actual downloaded data.

To diagnose a blank map, request an expected tile such as
`/tiles/5/8/11.jpg` (southern Ontario). A missing tile returns HTTP204; an available
exact or ancestor-derived tile returns a JPEG and an `X-GS26-Tile-Source` header.
Check the service's startup log and working directory if no terrain is available.
