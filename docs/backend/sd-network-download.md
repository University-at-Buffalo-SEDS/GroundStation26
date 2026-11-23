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
  FC downloads use the FC link rate: 512 B/s over the AV-bay radio
  (`GS_FC_SD_RADIO_MAX_BYTES_PER_SEC`), 16 KiB/s wired
  (`GS_FC_SD_WIRED_MAX_BYTES_PER_SEC`); see "Flight Computer" below.
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
| GET | `/api/sd_download/fc` | automatic FC collection status (view permission) |
| POST | `/api/sd_download/fc/stop` | stop the automatic FC collection (persists until resumed) |
| POST | `/api/sd_download/fc/resume` | resume it |

## Boards and folders

| Board | Wire directories | Local folder | Answer check |
| --- | --- | --- | --- |
| DAQ | 0 `LAUNCH`, 1 `UTC`, 2 `NO_UTC` (DAQ-Board `docs/sd-network-download.md`) | `sd-downloads/DAQ/<dir>/` | none |
| FC | 0 = card root, shown as `ROOT`; LIST returns `irec*.csv` only | `sd-downloads/FC/ROOT/` | CRC-32 |

**The FC's CRC-32.** The last DATA frame of every OK READ answer from the FC
has flags `0x03` (last + CRC). It carries 4 trailing bytes after the data:
the zlib CRC-32 (LE) of all data bytes of that answer. The station checks it.
On a mismatch it requests the block again and counts the error (`crc_errors`
in the job). Frames without the flag (DAQ) are accepted as before. A READ at
an unaligned offset returns the data up to the end of that 512-byte block,
and the next read continues at offset + length.

**FC gating.** The FC serves only in local Landed or Recovery, or in Startup
while the network FLIGHT_STATE is Startup or Idle. It accepts peers `GS` and
`RRLS`, one session at a time, and closes its file handle after 3 s idle. The
`--ground-test` (IGT) image does not have the service.

## Flight Computer

The FC's launch file is collected automatically after landing. No operator
action is needed: the trigger is the FC's Landed held 5 s, or Recovery. The
collection runs at the lowest priority:

- one block in flight, radio rate 512 B/s (adaptive up to 640 B/s while
  recovery traffic is light);
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
| AV-bay radio at 512 B/s | ≈88 min |
| AV-bay radio at the 640 B/s adaptive ceiling | ≈70 min |
| Via the RRLS (takeover) | no faster: the RF board's radio turn caps all AV-bay egress |
| **Bench, 16 KiB/s** | **≈2.8 min** |

**Bench path.** To power the AV bay off early, reconnect the recovered rocket
by umbilical, USB or CAN. The same service collects at 16 KiB/s. Set
`GS_FC_SD_LINK=wired` if the station still sees an RFD modem on the AV-bay
link.
