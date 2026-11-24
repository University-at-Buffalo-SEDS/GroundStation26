# Replay bundles (`.gsreplay`)

`GET /api/recordings/{id}/bundle` (view_data) returns one recording as a
self-contained replay bundle. The UI opens it with no station connected; the
format specification for producers and consumers is the UI repository's
`docs/replay-bundle.md`.

`backend/src/recording_bundle.rs` builds the bundle from this station's own
routes. It calls the handlers through an internal router with the caller's
credentials, so every entry is exactly what the station serves for that
resource:

- the replay export;
- `GET /api/recordings/{id}/csv`, byte for byte;
- the recorded mode's layout, boards, calibration and calibration
  configuration, diagrams, map configuration and vehicle geometry;
- the layout of every replay view (`/replay/layout?view=`), the station
  layout and the 3D player's view configuration
  (`GET /api/vehicle_visualization`, through the media handler);
- the replay snapshot, flight snapshot, context, overlays, alerts and
  trajectory, the first page of SD sources, and the recording's own entry of
  `GET /api/recordings`;
- the flight review, apogee planner state, and any burn report or
  fire-archive entry that names the recording.
- the station's five newest flight plans (`derived/flight_plans.json`, when
  there are any), for a predicted-vs-actual overlay offline.
- the recovery search tracks recorded from the recording's start until 6
  hours after its end, as `derived/recovery_search.json`, only when there are
  any ([recovery-search.md](recovery-search.md)).

`GET /api/flight_plans/{id}/bundle` builds the same format for a flight plan
(`recording_bundle::build_plan`, docs/flight-plans.md). It has
`manifest.predicted: true`, synthesized FC rows in `replay.json`, and
`derived/flight_plan.json`.
  fire-archive entry that names the recording, with each report's detail
  and each fire's thrust curve;
- what the station's exports need beyond the CSV
  (`backend/src/recording_bundle_exports.rs`):
  - `config/report_context.json`: the report studio's export-time context
    (`recording_report::context`: valve labels and units from the station
    layout, recorded diagnostic stream schema);
  - `derived/calibration_history.json`: the recording's
    `calibration_history` rows, when it has that table;
  - `exports/motor_export.zip` / `exports/flight_export.zip`: the review
    exports without a range (`flight_review::bundle_exports`), generated at
    the bundle's time and without raster charts, for the ones that apply;
  - `exports/index.json`: which review exports the bundle holds, with file
    name and window.
- for an SD-resolved recording (`backend/src/recording_bundle_sd.rs`):
  - `data/receipt_telemetry.csv`: the rows as received (`main.telemetry`),
    in the CSV of a recording that was never resolved; the bundle's
    `data/telemetry.csv` is the resolved timeline with each row's
    provenance. At most 96 MiB (else listed as missing with 507);
  - the SD source list's later pages (`derived/sd_sources/after_{n}.json`);
  - with `sd_files`, the original SD files byte for byte
    (`sd/sources/{sha256}.csv`, route `/api/recordings/{id}/sd/sources/{sha256}`);
  - the manifest's `sd` object: resolved row count, receipt CSV rows and
    bytes, the SD sources listed and their bytes, and the files included
    within the budget.
  The SD provenance the merge used (resolution summary, each import's
  manifest and calibration metadata, per-row provenance, unresolved
  samples) is already in `replay.json` `sd_provenance`.
- the station records (`backend/src/recording_bundle_records.rs`), byte for
  byte as their routes answer: `records/checklists.json`
  (`GET /api/checklists`), `records/checklists_export.json` and
  `records/checklist_signoffs.csv` (`/api/checklists/export`, `.csv`),
  `records/recovery_history.json` and `.csv` (`/api/recovery/export.json`,
  `.csv`). They are not filtered to the recording: the checklist document
  is the station's one current document and its sign-offs have no recording
  link, and recovery events name a launch session the station does not map
  to recordings; both are small. The checklist routes refuse anonymous
  callers, so an anonymous bundle lists them as missing (403).
    name and window;
  - `derived/review_inputs.json`: what the motor/flight review reads from the
    recording for its whole span (`flight_review::offline`): the review rows
    as `data::read_pool` returns them (values as numbers, `null` or
    `"OVR"`/`"-OVR"`; only `GS_STATION_MODE` and `GS_FLIGHT_MAXIMA` rows keep
    their payload), the message log and station flight states, the DAQ
    launch file rows of a plain event capture, and the capture manifest's
    label, mode, provenance, simulated flag, applied plan and the planner's
    motor for it (`planner_prediction`);
  - `models/hybrid_motor/{hybrid_motor,sweet_v6_cea,sweet_v6_grain}.json`:
    the hybrid motor model files (`apogee_planner::hybrid`), for the burn
    report's predicted curve; only when the recording has load cells and a
    tank PT (about 180 KB deflated).

`replay.json` gains `layout`, `boards`, `view_config`, `valve_mappings` (from
the layout's valve labels), `vehicle_geometry`, `mode` and
`configuration_provenance` when the export lacks them.

`manifest.json` lists every entry with its SHA-256 and the `route` (path and
query exactly as a client requests it) whose response body the entry is, and
records what the station could not supply. JSON bodies are stored with
sorted keys (some routes serialize hash maps). A client can answer any GET
whose path and query equal an entry's `route` from that entry. Routes that
take a playhead window (`/replay/window`, `/api/recordings/report`) are not
snapshot; a client computes them from `replay.json`.

With these entries the UI answers the recording CSV, the time-range CSV
(`GET /api/recordings/csv`), the report studio's preview, CSV and DAQ SD
CSV/TSV, the default-window review exports, and the checklist and recovery
exports with no station, byte for byte as this station would (UI
`docs/replay-bundle.md`, "Exports offline"), SD-resolved recordings
included. Excel/PDF reports and review exports of a custom window still need
the station.
CSV/TSV, the default-window review exports, and the review and review
exports of any window (`GET /api/flight_review/recording/{id}?from_ms&to_ms`,
`.../motor_export` and `.../flight_export` with a window) with no station,
byte for byte as this station would (UI `docs/replay-bundle.md`, "Exports
offline"). A window is clipped from the inputs as `read_pool` clips its read
and reviewed with the station's code (the UI copies `flight_review/zip.rs`,
`plot.rs`, `burn_report/thrust.rs`, `merge.rs` and `apogee_planner/hybrid.rs`
and ports the rest; its `scripts/check_review_port.py` checks the copies).
Offline review exports carry no PNG charts or PDF report: they are this
station's `?raster=false` answer. Excel/PDF reports and exports of
SD-resolved recordings still need the station.
CSV/TSV, and the default-window review exports with no station, byte for
byte as this station would (UI `docs/replay-bundle.md`, "Exports offline").
It builds the Excel and PDF reports itself with the same content as this
station's (same sheets, cells, charts; same pages, text and plotted points),
and hands out this station's own files for the standard selection when the
bundle carries them (`?reports=true`). Review exports of a custom window and
exports of SD-resolved recordings still need the station.

`contract_fixtures` writes the station's reviews and review exports
(`raster=false`, clock pinned) of custom windows of the bundled flight, a
flight capture and a test-fire capture (DAQ launch file, applied plan) to
`replay-bundle/review/`, with the two captures' bundles; the UI answers the
same requests from the bundles and compares the bytes.

The time-range CSV writes over-range values as `OVR`/`-OVR`, like the
recording CSV, so its row cells are the recording CSV's. It reads each
primary recording's receipt rows (`telemetry`), never an SD-resolved
timeline; offline, an SD-resolved recording's range CSV therefore comes from
`data/receipt_telemetry.csv`, and its reports from the resolved
`data/telemetry.csv` (canonical UTC time and provenance per row, as the
report worker reads `sd_replay_telemetry`).

Options:

- `?tiles=1` adds cached map tiles around the trajectory, from zoom 10 up to
  `max_native_zoom`, lowest zoom first.
- `tiles_max_mb` sets the tile budget: 8 MiB by default, at most 32.
- `?terrain=1` (default: on whenever `tiles=1`) adds `terrain/heightmap.json`,
  the body of `GET /api/terrain/heightmap` centred on the trajectory's first
  fix (the pad), wide enough for the whole track plus 1 km each side
  (2–30 km), 129 × 129 samples (about 45 KB, capped at 1 MiB). It is added
  only when the station has DEM tiles for the area (docs/backend/terrain.md);
  `manifest.terrain` records `{included, bytes, extent_m, size, zoom, source,
  attribution}` or `{included: false, reason}`. `terrain=0` leaves it out.

The trajectory entry (`derived/trajectory.json`) carries the flight-phase
segmentation (`phases`, `events`, a `phase` on each point;
docs/backend/terrain.md "Flight phases"), so a client colours the track by
phase with no station.
- `records=false` leaves out the station records (included by default).
- `sd_files=1` adds an SD-resolved recording's original SD files, in listing
  order within `sd_files_max_mb` (16 MiB by default, at most 64).
- `review=false` leaves out `derived/review_inputs.json` and the hybrid
  motor model (custom review windows then need the station). On by default:
  a ten-minute capture at flight rates gains about 2 MB.
- `?video=1` adds recorded camera segments that overlap the recording;
  `video_max_mb` sets their budget: 32 MiB by default, at most 64. It needs
  stream-manager access, as the camera archive does (403 otherwise).
- `?voice=1` adds recorded crew voice bursts that overlap the recording;
  `voice_max_mb` sets their budget: 8 MiB by default, at most 16. It needs
  the voice permission (403 otherwise).

Flags accept `1`, `true`, `on` or `yes`. Before this change `?tiles=1` was
refused: the query parser read only `true` and `false`.

## Recorded media

`backend/src/recording_bundle_media.rs` adds the media options:

- **What it includes.** Whole files, untrimmed, whose span overlaps the
  recording's `[start_ms, end_ms]`. Within each budget the smallest files go
  first. Files written in the last five seconds are still being recorded and
  are left out.
- **Entries.** `media/video/<stream>/<file>.mp4` (the MediaMTX segment) and
  `media/voice/<stream>/<start_ms>-<speaker>.wav`, stored without
  compression. `media/index.json` lists every clip:
  `{kind, stream, file, path, start_ms, end_ms, bytes, media_type, timing}`.
- **Summary.** The index and the manifest's `media` key (present only when a
  kind was requested) give per kind `requested`, `recorded`, `candidates`,
  `included`, `bytes`, `budget_bytes`, `complete`, and `skipped` with a
  reason for each file left out.
- **Routes.** These entries carry no `route`: the UI reads them by path and
  plays them against the replay clock. The format and the player are in the
  UI's `docs/replay-bundle.md`, "Recorded media".
- **Camera timing.** MediaMTX names segments
  `%Y-%m-%d_%H-%M-%S-%f` in the receiver's local time. The station takes the
  one quarter-hour UTC offset that puts that name less than 15 minutes
  before the file's last write. Segments are about a minute long, so the
  answer is unique and needs no time-zone setting. The last write is the
  segment's end. A file whose write time does not fit (copied or touched) is
  read as UTC and one minute long, and its `timing` says so.
- **Voice.** Bursts come from the opt-in recorder (`crew-voice.md`,
  "Recording"). When `GS_VOICE_RECORDINGS_DIR` is not set, `voice.recorded`
  is false and the index notes that this station does not record voice.
- **Sizes.** Camera video is large compared with the caps. At 2 Mbit/s a
  one-minute segment is about 15 MB, so the 32 MiB default holds two
  segments and the 64 MiB maximum four. Voice is 1.9 MB per minute of
  speech. `recording_bundle::tests::realistic_media_bundle_size` measures a
  ten-minute capture with two cameras and 30 bursts: 32.9 MB of media at the
  defaults and 62.9 MB at the maximum. With every option at its maximum a
  bundle stays under the 192 MiB a reader (and the import) accepts.
- `?reports=true` (or `1`) adds the report studio's standard reports, all
  channels over the whole recording, as the report worker writes them
  (`recording_report::standard`, queued behind a running report job):
  `exports/report.xlsx`, `exports/report.pdf` and `exports/reports.json`
  (each report's file name and size, or why it was left out: no openpyxl or
  matplotlib, the PDF's 20,000-sample limit, the budget). Their document
  time is the bundle's time, so the bundle stays reproducible.
- `reports_max_mb` sets the reports' joint budget: 16 MiB by default, at
  most 64. For the contract fixture's flight (601 rows, 16 channels) the
  reports are 89 KB (Excel) and 234 KB (PDF).

The report worker pins an Excel/PDF document's time (core properties, ZIP
entry times, PDF `CreationDate`) to the request's `generated_ms` when given.
It draws the PDF with the PDF base fonts (Helvetica, Courier), ASCII minus
signs and unsimplified lines: the text is selectable and every recorded
sample is plotted, and the UI can build the same pages offline.

The ZIP writer is `flight_review::zip` (deflate, no ZIP64). Bundles are
reproducible: the same recording and time give the same bytes.

## Import

`POST /api/recordings/import` (administrators; body: the `.gsreplay` bytes,
at most 256 MiB) stores the bundle's recording on this station
(`backend/src/recording_import.rs`):

- The archive is checked like the UI reader: stored or deflated entries, no
  ZIP64 or encryption, safe relative names, no duplicates, CRC-32, at most
  192 MiB unpacked; `format` must be `gsreplay` and `version` at most 1;
  every listed entry must be present with its size and SHA-256.
- `data/telemetry.csv` becomes `recording.db` of an event capture of kind
  `imported`: `recordings/events/imported/<stamp>_<slug>/` with
  `manifest.json` (`details.import` holds the source station and recording,
  the CSV and bundle SHA-256, the generator and modes). Its id is
  `event.imported.<folder>.db`, so it cannot collide with a recording made
  here, and it is listed by `GET /api/recordings` and the store.
- Row ids, reception and source times, sender, canonical type, values and
  payload are kept; `OVR`/`-OVR` markers go back into the `over_range`
  column, and the `'` the export puts before formula-like text is removed.
  The imported recording's CSV export therefore equals the bundle's CSV byte
  for byte (the IGT provenance column is recomputed from the same rows).
  Recorded alerts come from `derived/alerts.json`.
- The bundle's configuration entries are not installed; the station's own
  configuration labels the replay.
- An SD-resolved recording (its `data/telemetry.csv` has `canonical_utc_ms`
  and `provenance_json`) imports from two CSVs: `data/receipt_telemetry.csv`
  becomes `telemetry` and `data/telemetry.csv` becomes
  `sd_replay_telemetry` (canonical time, provenance, over-range masks), with
  `sd_replay_loadcell_units`. `sd_resolution_manifest` comes from
  `replay.json` `sd_provenance.resolution`, `sd_imports` from
  `sd_provenance.imports` and the bundled source list, and
  `sd_source_chunks` from any bundled original SD files. The imported
  recording's CSV, receipt rows, SD source list, original files, replay
  rows and `sd_provenance` imports equal the source's. Not carried: the SD
  samples (`sd_samples` stays empty; each resolved SD row keeps its record
  in its provenance, and the unresolved ones remain only in the bundle's
  `replay.json`) and the coverage segments, so the imported replay has no
  `valid_until_ms`.

Responses: `201` `{"id", "label", "rows", "alerts", "start_ms", "end_ms",
"source", "csv_sha256"}`; `400` an unreadable or unsafe archive, a manifest
that is not `gsreplay`, a listed entry missing or differing, or a malformed
CSV; `401`/`403` not signed in or not an administrator; `409` `{"error",
"id"}` the same source recording with the same CSV was already imported;
`413` larger than 256 MiB; `422` a newer bundle version, or an SD-resolved
recording whose bundle lacks `data/receipt_telemetry.csv` (made by an older
station).

Checks:

- `contract_fixtures` writes `replay-bundle/flight.gsreplay`, its manifest
  and the recording's CSV, and checks the bundle's CSV hash against the CSV
  route. It also writes this station's answers to the export requests the UI
  answers offline (`replay-bundle/exports/`, listed in
  `exports/requests.json`), for the bundled flight and for a second
  recording with the report cases the flight lacks. A third recording
  (`contract_fixtures_sd.rs`) is SD-resolved, synthetic but with the SD
  import and resolver's tables and row shapes: physical SD records,
  median-bin and paired rows, an over-range row, an ambiguous boot's
  labelled series, network rows (one converted from the legacy raw scale);
  its bundle with the original SD file is `replay-bundle/sd.gsreplay` and
  its exports are the `sd` case of `exports/requests.json`.
- `recording_bundle_records::tests` checks each record entry against its
  route byte for byte, the anonymous 403s and `records=false`.
  recording with the report cases the flight lacks. With the report Python
  packages (`GS_REPORT_PYTHON` pointing at an environment with
  `backend/report-requirements.txt`) it writes the Excel and PDF reports of
  both recordings (standard and custom selections, `generated_ms` pinned,
  `exports/report-documents.json`) and checks that a `?reports=true` bundle
  carries the standard ones byte for byte; without them it leaves those
  files as they are and says so.
- `recording_import::tests` builds bundles of recordings with every CSV rule
  (over-range markers, legacy type names, formula-like text, quoted line
  breaks, NULLs, the IGT provenance column), imports them into a fresh store
  and checks the CSV export byte for byte, the replay samples, the alerts,
  the listing, the 409 on a second import, administrator-only access and
  every refusal; and that an SD-resolved bundle imports with both
  timelines (and is refused without its receipt rows).
- `recording_bundle::tests::realistic_capture_bundle_size` (ignored; run it
  with `--ignored --nocapture`) reports sizes. A ten-minute capture of 154,200
  rows (a 20.3 MB database) makes a 4.3 MB bundle.
