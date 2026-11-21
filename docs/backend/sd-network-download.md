# Board SD download

The SD import area has a **Board SD download** panel. It lists a board's SD card
over SEDSnet, downloads the files you select, and offers each finished copy to the
SD recovery import. The import, provenance and "SD rows win" merge rules are the
same as for a card read by hand.

## Rules

- Listing, starting and cancelling require command permission (`SdDownload` if a
  user has a command allow-list). Anyone with view permission can see status.
- Allowed only in Idle, Landed and Recovery, and never while a fill/launch
  sequence is active. A running transfer stops within 100 ms of a state
  change. The board resets the stream too.
- One transfer runs at a time, one 512-byte block per request, capped at
  `GS_SD_DOWNLOAD_MAX_BYTES_PER_SEC` (default 16384, range 512 to 262144).
- Data is appended to `sd-downloads/<board>/<folder>/<name>.part` next to the
  recordings. Cancel, pause, failure or restart keep the partial file. Starting
  the same file again resumes after the last byte, once the board's copy of the
  block holding that byte still matches. If it does not match, the download restarts.
  A finished file is renamed without `.part`.
- Each 4 MiB the free disk is checked against the SD import reserve.

## API

| Method | Path | Notes |
| --- | --- | --- |
| GET | `/api/sd_download` | listings, jobs, local copies, gate |
| POST | `/api/sd_download/{board}/list` | refresh the card listing |
| POST | `/api/sd_download/{board}/start` | `{"dir":"LAUNCH","name":"..."}`; resumes |
| POST | `/api/sd_download/jobs/{id}/cancel` | keeps the partial file |
| POST | `/api/recordings/{id}/sd/imports/{token}/download` | `{"board","dir","name"}`; same admin rule and limits as the PUT upload |

Only DAQ serves the stream so far (protocol: DAQ-Board `docs/sd-network-download.md`).
Flight Computer support is not implemented.
