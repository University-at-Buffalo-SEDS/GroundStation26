# Offline maps

Run `./download_map.py` normally. Existing bundle tiles are retained and skipped on
restart; do **not** delete the database to use these fixes. Parallel HTTP downloads
feed one bounded SQLite batch writer, while resume scans run once per zoom before
writing starts. A failed read or commit stops the run with an error.

To bind only the downloader's HTTP sockets to a physical interface:

```sh
./download_map.py --direct-interface        # auto-detect physical default
./download_map.py --direct-interface en0    # explicit macOS interface
./download_map.py --direct-interface eth0   # explicit Linux interface
```

This disables HTTP proxies only for direct requests. A single six-second probe
checks the route; failure uses the normal route/proxy settings. A later direct
transport/body failure switches the remaining run to normal networking. No routes,
firewall rules, Wi-Fi settings or DNS configuration are changed. DNS continues to
follow the operating system's resolver policy. Unsupported platforms or interface
permissions fall back normally.

The default disk reserve is 2048 MiB. Override with `--min-free-mib N` or
`MAP_MIN_FREE_MIB=N`. Low space pauses the download with a terminal error, retaining
committed tiles for a later run. Both direct downloads and directory-to-bundle packaging checkpoint their WAL
rather than making a full-database VACUUM copy. Packaging also checks the reserve
and commits at most 16 MiB of image data at a time; a disk-space failure keeps
both committed bundle data and input tiles, then exits unsuccessfully.

Each tile body is limited to 512 KiB, sufficient for the source's 256-pixel JPEG/PNG
raster tiles. Oversized responses are terminal for that tile. HTTP response buffers
can retain up to configured concurrency × 512 KiB; queued/batched payload copies
add at most 32 MiB. SQLite cache, coordinate sets, HTTP/TLS buffers and runtime
state consume additional memory. HTTP concurrency and existing bandwidth limits
remain configurable as before.

Default coverage caches North America at zooms 0–8 and Buffalo/Rochester, West
Texas, and the URRG launch/recovery area near Potter, NY at zooms 9–17. The URRG
bounds are longitude -77.30 to -77.05, latitude 42.60 to 42.80 (about 20 × 22 km),
around the [official URRG field map](https://www.urrg.us/content.aspx) at
4270 NY-364. Both download modes use the same coverage, de-duplicate overlapping
tiles, and retain the existing bundle on resume. `--test` includes one URRG tile
at the configured high zoom and remains a small read-only connection probe.

## Compact RRLS dataset

```sh
python3 download_map.py --profile rrls --test  # read-only, four small probes at most
python3 download_map.py --profile rrls --min-free-mib 30720
python3 run_rrls.py --frontend-dev
```

The RRLS profile writes `backend/data/maps/rrls/tiles.sqlite3`, independently of
GroundStation's `north_america` dataset. At every zoom 0–17 it selects only:

| Area | Longitude bounds | Latitude bounds |
| --- | --- | --- |
| UB North, South and Downtown campuses | -78.91 to -78.74 | 42.87 to 43.04 |
| URRG Potter launch and recovery vicinity | -77.30 to -77.05 | 42.60 to 42.80 |
| Midland city, airport and nearby recovery vicinity | -102.30 to -101.95 | 31.85 to 32.10 |

UB locations follow the university's official [North](https://www.buffalo.edu/admissions/visit.to-north-campus.html),
[South](https://www.buffalo.edu/admissions/visit.to-south-campus.html), and
[Downtown](https://www.buffalo.edu/home/visiting-ub.to-downtown-campus.html)
campus directions. The Midland airport is near 31°57'N, 102°12'W on the
[FAA airport diagram](https://aeronav.faa.gov/afd/22JAN2026/SC_rear_22JAN2026.pdf).
This is a small local Midland box, not the full West Texas launch/recovery area.
Outside the boxes, coarse boundary tiles may still cover adjacent land; the
profile never requests the bulk continental or West Texas bounding boxes.

The default plan has 38,413 distinct tiles across all zooms. Each image is capped
at 512 KiB; even allowing 64 KiB per tile for SQLite pages/indexes plus 64 MiB setup,
the conservative plan is about 22.72 GB. Actual imagery is usually much smaller.
`MAP_MAX_ZOOM` can reduce coverage resolution; RRLS rejects values above 17.
HTTP concurrency and bandwidth settings are unchanged. `MAP_PROFILE=rrls` is
also supported. An explicit `--profile` overrides that environment variable.

RRLS requires resumable direct-to-DB mode. It rejects `--via-tiles`,
`--no-bundle`, and `--no-bundle-resume`, rather than creating duplicate copies or
deleting a previous bundle. The dataset has a **50,000,000,000-byte cap** (decimal
50 GB, stricter than 50 GiB), counting the main database, WAL, sidecars, nested
files and temporary/legacy files in its directory. An exclusive OS file lock
prevents two RRLS downloaders writing the same dataset. Symlink entries are
rejected. The lock is released automatically on process exit, including crashes.

SQLite's main file is capped at 23 GB via `max_page_count` on every connection.
The single writer admits at most 32 tiles/16 MiB of image data per transaction,
disables dirty-page spills, and requires a successful WAL TRUNCATE checkpoint
before and after each batch. One transaction can therefore emit at most one WAL
frame per database page. Admission reserves the full 23 GB main-file cap, a full
main-file-sized WAL with frame headers, 128 MiB for transient/index/allocation
overhead, and the measured size of every other dataset file. SQLite temporary
storage stays in memory; initialization uses one schema transaction, and RRLS
finalization only checkpoints (it skips optional ANALYZE/optimize writes). No
VACUUM copy or legacy tile copy is made. These
bounds protect the peak size, rather than checking only after a large commit.
Do not use an unrelated process to write into the dataset during download.

A pinned backend reader can prevent WAL truncation: the downloader stops with
an actionable error and retains committed tiles. Run it again once those reads
finish. A page cap, disk reserve, existing oversized database, extra files, or
budget failure likewise stops without deleting data. Preexisting data already
over the cap is refused; it is never silently removed to make room. Existing
tiles are checked individually on every run, so changed bounds queue missing
tiles even after an older run completed a zoom.

`run_rrls.py` and `run_groundstation.py --rrls` automatically serve the `rrls`
dataset and initially center the map at UB. Normal GroundStation continues to
serve `north_america`. Explicit `MAP_REGION` selects another dataset for either
role and for the downloader; use the same value on both sides. For example:

```sh
MAP_REGION=my_rrls python3 download_map.py --profile rrls
MAP_REGION=my_rrls python3 run_rrls.py --frontend-dev
```

`MAP_BUNDLE_PATH`/`--bundle-path` still selects a custom download output; its
parent directory becomes the budgeted dataset. Serving a custom filename still
requires placing it as `tiles.sqlite3` in the selected backend dataset. Avoid
placing an unrelated large dataset in that same directory. `--test` does not
create or lock a dataset, and samples only the configured local regions plus
the shared zoom 0 tile. The default GroundStation profile and its download paths
remain unchanged.
