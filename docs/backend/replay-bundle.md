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
- the replay context, alerts and trajectory;
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

`manifest.json` lists every entry with its SHA-256 and records what the
station could not supply.

Options:

- `?tiles=1` adds cached map tiles around the trajectory, from zoom 10 up to
  `max_native_zoom`, lowest zoom first.
- `tiles_max_mb` sets the tile budget: 8 MiB by default, at most 32.

The ZIP writer is `flight_review::zip` (deflate, no ZIP64). Bundles are
reproducible: the same recording and time give the same bytes.

Import of a bundle back into a station is not implemented. The existing CSV,
database and replay-export downloads are unchanged.

Checks:

- `contract_fixtures` writes `replay-bundle/flight.gsreplay`, its manifest
  and the recording's CSV, and checks the bundle's CSV hash against the CSV
  route.
- `recording_bundle::tests::realistic_capture_bundle_size` (ignored; run it
  with `--ignored --nocapture`) reports sizes. A ten-minute capture of 154,200
  rows (a 20.3 MB database) makes a 4.3 MB bundle.
