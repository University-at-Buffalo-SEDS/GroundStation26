# Saved station positions

Administration → Station locations stores independent GroundStation and RRLS
coordinates. Enter latitude/longitude, optionally altitude, or press **Use this
device’s location** under the relevant station. Location access runs only after
that action; denial leaves the draft intact. Save applies both positions, and
Clear removes a position only after Save.

The backend persists positions atomically in `backend/config/station_positions.json`
(or `GS_STATION_POSITIONS_PATH`). The file is runtime configuration, excluded from
Git. Concurrent edits receive HTTP 409 and preserve the unsaved form until Reload.
Application administrators may edit; telemetry viewers may read.

`GET /api/antenna/status` is the canonical map/alignment snapshot: fresh live GPS
wins, then a saved static position, then the last stale live fix. Saved positions
have `manual: true` and a source label distinguishing typed coordinates from a
copied device location. They never create a rocket GPS lock or masquerade as a
live radio GPS report. Optional saved altitude has unknown datum and does not
claim a valid antenna elevation reference.

`GET /api/station_positions` returns the saved configuration and revision;
`POST` takes `expected_revision`, `ground_station` and `rrls`. Each coordinate
contains `latitude`, `longitude`, optional `altitude_m`/`accuracy_m`, and source
`manual` or `device_location`. These settings currently persist on the selected
host; automatic station GPS and paired-settings replication remain separate
integration work.

The isolated `station_positions_preview` example and
`tests/station-positions-browser.mjs` test bounds, synthetic device location,
reload persistence, concurrent-edit rejection, explicit removal, location denial,
and touch layouts at 320, 390, 844 and 1280 pixels without a real station or GPS.
