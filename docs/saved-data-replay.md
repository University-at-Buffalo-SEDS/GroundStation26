# Saved-data replay in every station mode

A replay shows saved data the way the station showed it in the mode it was
recorded in, whatever mode the station is in now. It is strictly read only: it
never sends a command, never feeds the live safety, abort, alert or flight-state
logic, and never starts a capture, a burn report, a fire archive record or a
notification. The live station mode is shown for information and never
changed.

Open it from **Data Export → Recordings**: **Replay** on any capture, attachment
or legacy recording, **Replay with board SD** / **Replay board SD only** on a
capture with board SD files, **Replay a time range of the rolling store**, or
**Replay selected recording**. A fire archive entry has **Replay this fire**
(Settings → Fire archive). The 3D vehicle replay (`recorded-vehicle-replay.md`)
accepts the same ids through the same window API.

## What can be replayed

Every replay id is one URL segment under `/api/recordings/{id}/replay…`:

| id | source |
| --- | --- |
| `groundstation_recording_<stamp>.db` | a legacy session file |
| `rolling.segment_<stamp>.db` | one rolling segment |
| `range.<start_ms>.<end_ms>` | a time range of the rolling store across every segment it touches (up to 6 h, 768 MiB of segments), starting with the station mode and IGT status in force at its start |
| `event.<type>.<folder>.db` | an event capture (launch, abort, fill, nitrogen_test, valve_self_test, turbocharge, ground_test, hitl, manual) |
| `event.<type>.<folder>.<file>.db` | an attachment in a capture folder (an SD import resolved against it) |
| `merge-sd.<capture id>` | the capture with its `board_sd/DAQ|FC/` launch files and lead-ups merged in on network time |
| `sd-only.<capture id>` | those board SD files alone, with the capture's station-mode rows |
| `fire.<fire archive id>` | the fire's capture (with board SD files) when it is still on disk, else its saved thrust curve as `BURN_REPORT_THRUST` rows |

Board SD rows are converted on the fly: only rows on network time are used
(local-clock rows cannot be aligned and are skipped). The DAQ's load cells and
chamber PT become the station's own data types with sender `DAQ_SD`; every
other sensor is `<BOARD>_SD_<SENSOR>` with sender `DAQ_SD` / `FC_SD`. SD rows
never share a series with the radio telemetry.

Composed sources (`range`, `merge-sd`, `sd-only`, `fire`) are built into a
private in-memory database, cached for 15 minutes (three at most). Saved files
are opened read only.

## Endpoints

- `GET …/replay`, `…/replay/window`, `…/replay/flight`, `…/replay/trajectory`,
  `…/replay/export`: as before, for every id above. Window rows of
  DAQ_INPUT_HEALTH carry `decoded` (version 1, 2 and 3 layouts; version 3 adds the ADC over-range channels, VDDA and the applied MCP3564R reference). Rows with ADC over-range values carry `over_range`/`over_range_low` bits (values `null`), read from the recording's `over_range` column.
- `GET …/replay/context`: the recorded context: `recorded_mode` (from the
  GS_STATION_MODE rows, else the capture manifest, else inferred for
  recordings made before that row), `mode_timeline` (every mode change in the
  recording), `provenance` (`hardware`, `firmware_sim`,
  `internal_flight_sim`), `hitl`, `hitl_replay`, `igt`, `test_fire`,
  `default_view`, `view_options`, `tabs`, the two IGT `bodies`, the recorded
  `series`, the capture manifest and board SD entries, the burn report summary,
  compatibility notes and `live_station_mode`.
- `GET …/replay/layout?view=<mode>`: the layout of the recorded mode (or of the
  picked view) with no actions, `replay.read_only = true` and that view's tabs.
- `GET …/replay/overlays`: the burn report thrust curve on absolute network
  time (or the fire archive curve) and the linked fire archive entries.
- `GET …/replay/alerts`: the alerts recorded with the data (at most 500),
  marked `recorded`.

GS_STATION_MODE rows now carry `provenance` and `firmware_sim`, and a new row
is written when the mode changes in process, so a segment that spans a switch
replays each part in its own mode.

## Per mode

- **Normal / flight, testing:** map/GPS, estimator, recovery events.
- **Test fire:** the test-stand (GSE) view, thrust with the burn report
  overlay.
- **HITL:** the HITL banner and configuration. HITL can simulate any
  configuration, so the view picker offers every mode's layout; it opens in
  the integrated ground test view when the recording ran the ground-test
  replay, else in the HITL layout. The picker changes only the replay.
- **Integrated ground test:** two bodies, labelled **Stand (real)** (fill,
  load cells, chamber PT, valves, thrust/burn report) and **Rocket flight
  (replayed)** (the FC's SIMULATED flight from SIM_SAMPLE rows with its
  estimator and recovery events, beside its real at-rest sensors, GPS and pyro
  events). Stand thrust never appears in the rocket's views.
- **`<mode>-firmware-sim`:** the mode's own view with the SIMULATED FIRMWARE
  banner and every series labelled.

A REPLAY banner is always first, the replay panel has its own dashed border,
and recorded alerts are listed as RECORDED history, never as live alerts.

## Older recordings

- BAROMETER_DATA was always recorded in the FC's order (altitude, pressure,
  temperature); the shipped layouts label it that way, and a layout saved
  before the label fix (in an SD import manifest or a portable replay) is
  relabelled.
- DAQ_INPUT_HEALTH version 1, 2 and 3 rows are all decoded.
- Load-cell rows recorded on the DAQ's legacy raw scale are converted as before.
- DAQ SD files in the microsecond (daq-20261123) format are read through the
  network-time `timestamp_ms` column, like the older format.

Not supported: recordings made by the station flight simulator or dummy packets
before the barometer fix hold their values in the old wrong order and are not
corrected; FC SD rows other than the network-time CSV rows are not replayed.

Tests: `backend/src/recording_replay/replay_modes_tests.rs` and the frontend
`saved_replay` tests.
