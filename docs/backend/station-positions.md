# Saved station positions

Administration → Station locations stores independent GroundStation and RRLS
coordinates. Enter latitude/longitude (a decimal comma such as `43,02` is
accepted), optionally altitude, or press **Use this device’s location** under the
relevant station. Location access runs only after that action and makes a single
fresh request (no watch, high accuracy, no cached fix, 20 s device timeout and a
30 s overall limit that also covers an unanswered permission prompt). The result
only fills that station's draft, labelled with its accuracy; denial, timeout or an
unavailable location leaves the draft intact with a specific message. Nothing is
stored until Save. Save applies both rows, and Clear removes a position only after
Save. Each row shows whether it was typed or copied from a device, and the page
shows when and by whom positions were last saved.

Where the device location comes from:

- Web: the browser's geolocation. It needs a secure context, so open the station
  over `https://` (port 443, self-signed certificate); over plain HTTP on port
  3000 the button explains this instead of failing silently. `localhost` counts
  as secure.
- Android app: the WebView's geolocation (HTTPS scheme, location runtime
  permission from `AndroidManifest.xml`).
- Desktop apps and iOS: the WebView does not grant page geolocation, so the app
  asks the operating system once — GeoClue on Linux, the Windows location service,
  and CoreLocation on macOS/iOS (`NSLocationWhenInUseUsageDescription`). Denied
  or restricted access is reported as such.

If a save meets HTTP 409 because positions changed elsewhere (another
administrator or a paired-station import), rows the operator did not edit are
refreshed with the newly saved values and edited rows keep their draft; the
operator saves again explicitly. A row edited on both sides is named so the
operator can choose to replace it or Reload.

The backend persists positions atomically in `backend/config/station_positions.json`
(or `GS_STATION_POSITIONS_PATH`). The file is runtime configuration, excluded from
Git. Concurrent edits receive HTTP 409 and preserve the unsaved form until Reload.
Application administrators may edit; telemetry viewers may read.

`GET /api/antenna/status` is the canonical map/alignment snapshot: fresh live GPS
wins, then a saved static position, then the last stale live fix. Saved positions
have `manual: true` and a source label distinguishing typed coordinates from a
copied device location. The map tab polls this snapshot and draws GS and RRLS
badges (solid for live GPS, dashed for a saved fallback) with a legend giving
coordinates, provenance, accuracy and fix age. They never create a rocket GPS lock or masquerade as a
live radio GPS report. Optional saved altitude has unknown datum and does not
claim a valid antenna elevation reference.

`GET /api/station_positions` returns the saved configuration and revision;
`POST` takes `expected_revision`, `ground_station` and `rrls`. Each coordinate
contains `latitude`, `longitude`, optional `altitude_m`/`accuracy_m`, and source
`manual` or `device_location`.

Paired installations share the saved coordinate content through the signed
mission-settings channel, using its existing bounded radio allowance. Each host
keeps its own file revision, editor and save time; live GPS and motion are never
part of this document. Imports wait for the existing safe Idle state. Concurrent
offline edits remain conflicts until an application administrator selects a
version in Paired settings. A simultaneous local edit also blocks replacement.
Synchronization does not send commands, publish a synthetic GPS lock or change
sequence qualification. Automatic station GPS remains separate from saved
configuration.

The isolated `station_positions_preview` example and
`tests/station-positions-browser.mjs` test bounds, decimal commas, synthetic device
location (success, denied, timeout, unavailable, unanswered prompt, insecure
page), the single-request options, reload persistence, per-row conflict merging,
explicit removal, and touch layouts at 320, 390, 844 and 1280 pixels without a
real station or GPS.
