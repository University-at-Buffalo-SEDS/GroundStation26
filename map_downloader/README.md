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
