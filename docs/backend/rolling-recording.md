# Always-on recording and event captures

The station records all the time. There is no start, pause or stop button:
the recording cannot be turned off, so data cannot be lost by forgetting to
press it. The important parts are saved separately so they are easy to find.

## Layout on the station

```
data/
  recordings/
    settings.json                 ring size, pre/post-roll, auto-capture switches
    rolling/
      segment_<UTC stamp>.db      the rolling ring (oldest deleted first)
    events/
      pending-captures.json       captures still recording (survives restarts)
      launch/<stamp>_<label>/     recording.db + manifest.json (+ SD attachments)
      hitl/  abort/  fill/  nitrogen_test/  valve_self_test/  turbocharge/
      cold_flow/  igniter/  ground_test/  daq_dac/  manual/
  groundstation_recording_*.db    legacy session recordings (still listed)
```

On the Pi the `data/` directory is the Docker volume `./data:/app/data`.

## The rolling ring

* The DB actor opens a new SQLite segment at start-up and writes every row
  into it (the same schema as the legacy session files). When a segment passes
  its size (default 64 MB) it is checkpointed, closed (`journal_mode=DELETE`)
  and the next one opens.
* The ring's budget is **50 GB or the disk minus a reserve, whichever is
  smaller**: `min(ring_budget_mb, ring size + free disk - reserve_free_mb)`.
  The reserve (default 8 GB, never below the warning threshold) keeps room
  for event captures, board SD attachments and the OS. When the ring passes
  its budget the oldest closed segments are deleted, oldest first, never the
  active segment and never a segment a pending capture still needs (its
  pre-roll is pinned). `/api/recordings/store` reports the configured
  ceiling (`budget_bytes`) and the budget applied now
  (`effective_budget_bytes`).
* Segment files rather than one large ring file: a closed segment is a
  finished, crash-safe SQLite file; deleting the oldest is one `unlink` (no
  `DELETE`/`VACUUM` on a live file, no write amplification on the SD card);
  a crash can only affect the open segment, whose WAL is recovered at start-up;
  and every existing reader (CSV, report studio, replay, SD import) opens a
  segment like any recording.
* Each new segment starts with the station-mode rows and the latest
  calibration of every sensor, so a segment (and a capture cut from it) is
  self-contained.
* A restart resumes the ring: segments are rediscovered from their names
  (`segment_<UTC stamp>.db`), journals are recovered, and a new segment opens.
* "Reset" of the live history opens a fresh segment instead of deleting rows.

## Event captures

A capture is cut out of the ring (so its pre-roll exists even though the event
started later) into its own folder with a `manifest.json`: event type, label,
event start/end, capture window, pre/post-roll, station mode, backend version,
operator, result, row counts, source segments and whether the ring still held
the full pre-roll. Captures are not counted in the ring budget and are never
deleted automatically; administrators can delete them on the Recordings page.

The copy uses a separate SQLite connection that `ATTACH`es each segment; WAL
readers see only committed rows and never block the writer, so copying while
the ring keeps writing is safe. A copy interrupted by a crash leaves a folder
without a manifest; it is removed at start-up and the capture is retried.

| Event | Starts | Ends | Pre-roll | Post-roll | Max |
| --- | --- | --- | --- | --- | --- |
| launch (`hitl` in HITL mode) | FLIGHT_STATE Launch / in flight | Landed, Recovery, Aborted or Idle | 10 min | 5 min | 2 h |
| abort | FLIGHT_STATE Aborted | leaving Aborted | 5 min | 5 min | 1 h |
| fill, nitrogen_test, valve_self_test, turbocharge, cold_flow, igniter | a sequence run is accepted | its terminal result (passed, failed, cancelled, interrupted) | 2 min (valve self-test 1 min) | 2 min (cold flow 3 min) | 6 h fill, 2 h, 1 h |
| ground_test | integrated ground test active | IGT exited | 2 min | 2 min | 8 h |
| daq_dac | DAQ DAC validation sweep starts | sweep result | 30 s | 30 s | 2 h |
| manual | **Save protected capture** on the Recordings page | immediately | chosen (default 2 min) | 0 | 1 h |

Every value, and whether each event is captured automatically, is set by an
administrator on the Recordings page (`PUT`/`POST /api/recordings/settings`).
Run-keyed captures (sequences, DAC sweeps, manual) that were open when the
station restarted end at the last row before the restart with the result
"Interrupted"; launch, abort and ground-test captures continue while their
state still holds.

## Disk space

* The ring itself stops at the reserve (8 GB free), so the thresholds below
  are reached only when captures and other files use the reserve.
* Below `warn_free_mb` (2 GB) operators get a warning every 10 minutes.
* Below `critical_free_mb` (512 MB) the ring shrinks, oldest segment first,
  until that much is free again. Event captures are never deleted
  automatically; the warning says to download and delete old captures.

## API

| Route | |
| --- | --- |
| `GET /api/recordings` | every recording: `kind` = `rolling`, `capture`, `attachment` or `legacy` |
| `GET /api/recordings/store` | ring status, captures in progress, captures grouped by type, legacy files, settings |
| `POST /api/recordings/snapshot` | `{label, before_s, after_s}` manual capture (SendCommands) |
| `PUT/POST /api/recordings/settings` | administrators |
| `DELETE /api/recordings/{id}`, `POST /api/recordings/{id}/delete` | administrators; captures and legacy files only, audited |
| `GET /api/recordings/{id}/file` | the SQLite file (not the segment being written) |
| `GET /api/recordings/{id}/manifest` | a capture's manifest |

Recording ids stay one URL segment, so CSV export, the report studio, replay
and SD import take any of them: `groundstation_recording_<stamp>.db` (legacy),
`rolling.segment_<stamp>.db` and `event.<type>.<folder>.db`. A board SD
import into a capture is saved in that capture's folder and listed as its
attachment (`event.<type>.<folder>.groundstation_recording_<token>.db`). The
date-range CSV and the "all recordings" report read the ring and the legacy
files (captures are copies of ring data).

`/api/recording_status` and the `recording_status` WebSocket message carry
`always_on` and `captures` (in progress) for the status indicator shown in the
Actions tab.

## Board SD launch files

FC (`irec_LAUNCH-*.csv`) and DAQ (`DAQ_LAUNCH_*.CSV`) launch files follow the
GS launch capture automatically, in every station mode (launch, HITL run or,
without a launch capture, the integrated ground test capture):

* **DAQ, over the network.** When a launch capture (not a HITL run) is saved, the station
  lists the DAQ's `/LAUNCH` folder as soon as SD downloads are allowed
  (Idle, Landed or Recovery, nothing running) and downloads every launch
  file it does not have, one at a time.
* **FC (and DAQ), by SD import.** The FC does not serve its card over the
  network; a launch file imported against any recording (Data Export › SD
  recovery) is matched as well. The SD import itself is unchanged.
* **Matching.** A file's network-UTC rows (`time_source=network`) must
  overlap a capture window (±5 s); a launch or HITL capture wins over an
  overlapping ground-test capture. Files with only local time cannot be
  dated and stay where they are. The order does not matter: files downloaded
  before the capture was saved are attached when it is.
* **Stored** unchanged in `<capture>/board_sd/<BOARD>/<file>`, listed in
  the manifest's `board_sd` (board, file, bytes, SHA-256, first/last UTC,
  rows, how it was matched and where it came from), downloadable from
  `GET /api/recordings/{id}/board_sd/{board}/{file}`. The same file offered
  twice is attached once.
* **Lead-up.** Boards switch to their launch file only when they learn of the
  launch: the DAQ at T0 (its launch file holds exactly T0..T+20 s, from the
  `DAQ_LOG_CLOCK` deadline published at Launch acceptance), the FC at the
  accepted Launch command (about the 10 s countdown before ignition) or
  detected boost. Neither can hold a minute of full-rate rows in RAM (the
  FC's 32/16 KiB SD buffers cover under a second; the DAQ already uses
  740 of 768 KiB of RAM), and copying file tails on the card during the
  launch would load the writer at the worst moment. So the lead-up is cut on
  the station: when the board's ordinary file of the same boot is also on
  the station (DAQ `/UTC` download or an SD import), its last 60 s before the
  launch file's first row is saved as `<file>.leadup.csv`. 60 s covers
  arming, the countdown, igniter and pilot transients and the pre-liftoff
  sensor baseline; the GS capture itself keeps 10 min of network telemetry.

## RRLS

An RRLS records its own ring and captures from what it receives. Recording
has no commands, so nothing is proxied to the primary.
