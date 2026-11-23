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
- One transfer runs at a time, with the SD transfer protocol v2
  ([sd-transfer.md](sd-transfer.md)): streamed 16 KiB blocks in SDX (lossless
  binary rows, about 5-6x smaller than the CSV), selective ACKs, a window
  and a rate the station sets. The rate counts DATA bytes (SDX plus message
  headers): `GS_SD_DOWNLOAD_MAX_BYTES_PER_SEC` (default 16384, range 512 to
  262144) for the DAQ. FC downloads use the FC link rate: 768 B/s over the
  AV-bay radio (`GS_FC_SD_RADIO_MAX_BYTES_PER_SEC`), 64 KiB/s wired
  (`GS_FC_SD_WIRED_MAX_BYTES_PER_SEC`); see "Flight Computer" below.
- Data is appended to `sd-downloads/<board>/<folder>/<name>.part` next to the
  recordings: the verified bytes in file order. Cancel, pause, failure or
  restart keep the partial file. Starting the same file again resumes after
  the last byte, once the board's copy of the last 512 bytes still matches.
  If it does not match, the download restarts. A finished copy's SHA-256 is
  compared with the board's (HASH) before it is renamed without `.part`; a
  mismatch discards it and fetches the file once more.
- Each 4 MiB the free disk is checked against the SD import reserve.

## API

| Method | Path | Notes |
| --- | --- | --- |
| GET | `/api/sd_download` | listings, jobs, local copies, gate |
| POST | `/api/sd_download/{board}/list` | refresh the card listing |
| POST | `/api/sd_download/{board}/start` | `{"dir":"LAUNCH","name":"..."}`; resumes |
| POST | `/api/sd_download/jobs/{id}/cancel` | keeps the partial file |
| POST | `/api/recordings/{id}/sd/imports/{token}/download` | `{"board","dir","name"}`; same admin rule and limits as the PUT upload |
| GET | `/api/sd_download/fc` | automatic FC collection status (view permission) |
| POST | `/api/sd_download/fc/stop` | stop the automatic FC collection (persists until resumed) |
| POST | `/api/sd_download/fc/resume` | resume it |

## Boards and folders

| Board | Wire directories | Local folder | DATA message, window |
| --- | --- | --- | --- |
| DAQ | 0 `LAUNCH`, 1 `UTC`, 2 `NO_UTC` (DAQ-Board `docs/sd-network-download.md`) | `sd-downloads/DAQ/<dir>/` | 1 KiB, 16 |
| FC | 0 = card root, shown as `ROOT`; LIST returns `irec*.csv` only | `sd-downloads/FC/ROOT/` | radio 460 B, 6; wired 512 B, 8 |

**Checks.** Every block carries its raw length and zlib CRC-32; a block that
fails either is requested again (a new OPEN at the committed offset) and
counted (`crc_errors` in the job). The whole file is then checked by SHA-256
against the board's.

**FC gating.** The FC serves only in local Landed or Recovery, or in Startup
while the network FLIGHT_STATE is Startup or Idle. It accepts peers `GS` and
`RRLS`, one session at a time, and closes its file handle after 3 s idle. The
`--ground-test` (IGT) image does not have the service.

## Flight Computer

The FC's launch file is collected automatically after landing. No operator
action is needed: the trigger is the FC's Landed held 5 s, or Recovery. The
collection runs at the lowest priority:

- radio: 768 B/s of DATA (about 4.6 KB/s of CSV), rising by 128 B/s every
  2 s up to 1536 B/s while recovery traffic is light, halved on losses;
- it pauses for commands (10 s), stalled recovery GPS, radio congestion,
  another SD transfer, or an operator stop;
- it resumes across link drops and restarts.

The files are attached to the launch, HITL or ground-test capture with their
link and `# seds-sd` format. See `docs/data-pipeline.md` §3.4a for the trigger,
pause rules, the RRLS store-and-forward (P2P 4512) and the "AV bay can be
powered off" readiness.

**Typical 2.7 MB launch file, by path:**

| Path | Time |
| --- | --- |
| AV-bay radio at 768 B/s of DATA | ≈9.7 min (v1: 88 min) |
| AV-bay radio at the 1536 B/s adaptive ceiling | ≈4.9 min (v1: 70 min) |
| Via the RRLS (takeover) | no faster from the FC: the RF board's radio turn caps all AV-bay egress |
| **Bench, 64 KiB/s of DATA** | **seconds; the FC's SD reads rule** |

**Bench path.** To power the AV bay off early, reconnect the recovered rocket
by umbilical, USB or CAN. The same service collects at up to 64 KiB/s of DATA. Set
`GS_FC_SD_LINK=wired` if the station still sees an RFD modem on the AV-bay
link.
