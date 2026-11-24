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
CSV/TSV, and the default-window review exports with no station, byte for
byte as this station would (UI `docs/replay-bundle.md`, "Exports offline").
Excel/PDF reports, review exports of a custom window, and exports of
SD-resolved recordings still need the station.

The time-range CSV writes over-range values as `OVR`/`-OVR`, like the
recording CSV, so its row cells are the recording CSV's. It reads each
primary recording's receipt rows (`telemetry`), never an SD-resolved
timeline.

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

Responses: `201` `{"id", "label", "rows", "alerts", "start_ms", "end_ms",
"source", "csv_sha256"}`; `400` an unreadable or unsafe archive, a manifest
that is not `gsreplay`, a listed entry missing or differing, or a malformed
CSV; `401`/`403` not signed in or not an administrator; `409` `{"error",
"id"}` the same source recording with the same CSV was already imported;
`413` larger than 256 MiB; `422` a newer bundle version, or a recording
merged with board SD data (its CSV has `canonical_utc_ms`), which cannot be
imported yet.

Checks:

- `contract_fixtures` writes `replay-bundle/flight.gsreplay`, its manifest
  and the recording's CSV, and checks the bundle's CSV hash against the CSV
  route. It also writes this station's answers to the export requests the UI
  answers offline (`replay-bundle/exports/`, listed in
  `exports/requests.json`), for the bundled flight and for a second
  recording with the report cases the flight lacks.
- `recording_import::tests` builds bundles of recordings with every CSV rule
  (over-range markers, legacy type names, formula-like text, quoted line
  breaks, NULLs, the IGT provenance column), imports them into a fresh store
  and checks the CSV export byte for byte, the replay samples, the alerts,
  the listing, the 409 on a second import, administrator-only access and
  every refusal.
- `recording_bundle::tests::realistic_capture_bundle_size` (ignored; run it
  with `--ignored --nocapture`) reports sizes. A ten-minute capture of 154,200
  rows (a 20.3 MB database) makes a 4.3 MB bundle.
