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
* When the ring passes its budget (default 4 GB) the oldest closed segments
  are deleted, oldest first, never the active segment and never a segment a
  pending capture still needs (its pre-roll is pinned).
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

## RRLS

An RRLS records its own ring and captures from what it receives. Recording
has no commands, so nothing is proxied to the primary.
