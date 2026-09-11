# Frontend API Contract

## Live dashboard cadence

Each `/api/dashboard_status` stat now includes backend-owned `binding` metadata
(`data_type`, optional `sender_id`, `index`, `scale`, `offset`). The operator UI
uses these bindings with live WebSocket telemetry, not the slower HTTP snapshot,
to update numerical values. Missing/stale bound data must show unavailable rather
than falling back to an old snapshot. Older backends without bindings retain the
HTTP-value fallback. Bindings are not user-editable controls.

The backend's default telemetry flush is 20 ms; live frontend renders are coalesced
at 16 ms, including Mission/Vehicle views. Neither imposes a 1 Hz FC cap; a 200 ms
source cadence can update values at 5 Hz. Actual arrival rates still depend on board
configuration, radio transport, connection and device load. Delayed streamer views
continue using delayed program values, never live operator telemetry.

## Mission video and vehicle models

The Actions tab retains its manual controls and presents GSE sequence commands in
the same main button area. Ground setup is shown in both Dashboard/state and Mission
before Launch, then removed for flight/recovery. Its only numeric settings are
nitrogen maximum pressure (`nitrogen_target_psi`) and pressure step (`pressure_step_psi`,
default 50). Safety ceiling, zero-offset allowance and hardware panel mapping remain
backend configuration. A local checkable/resettable checklist is informational only.

`POST /api/gse/self-test-confirmation` accepts/returns `{confirmed:bool}`; it requires
SendCommands plus ValveSelfTest authorization. It returns 409 during an active
sequence or when requesting unlock after nitrogen testing begins. The explicit
Actions checkbox and all backend interlocks must permit self-test. Confirmation is
consumed by testing and is not persisted across backend restart.

Streamer mode renders no header/tab navigation. Without playable video it displays
the full-size backend model; with video it displays a corner 2D rocket. Program state
now contains static `model` configuration and delayed `telemetry.model_state`
(`motions`, `attitude`, `orbit`, `clip`). No live phase/binding data is substituted.

Dashboard is the primary single-stage model view; the separate Vehicle tab has been
removed. Settings → General → Viewing mode → Ground Station view restores instruments
and controls on that device. Streamer is available to any viewer from the same settings
section; **Exit streamer** returns to Dashboard. Neither mode grants command access.

`GET /api/dashboard_status` requires `ViewData`, returns `{phase,t_clock,stats}`, and
is polled approximately every 500 ms. Each stat is `{label,value,unit,precision}`;
backend bindings resolve the value, with missing/stale (>5 s) samples represented by
null. Three fields cycle about every five seconds beside state and T clock. Defaults
are RF GPS altitude/latitude/longitude, calibrated tank pressure, fill mass and fill
percentage. Configure them in backend `_presentation.json`, not in a user-facing editor.
Streamer uses the same fields from delayed snapshots, never this live endpoint.

The media endpoints follow the sibling frontend's `docs/backend-api.md`:
`GET /api/live_streams`, `POST /api/live_streams/control`, and
`GET /api/vehicle_visualization`. Streams use `kind: "webrtc"` and a backend-hosted
iframe player. Stream and model asset URLs carry scoped, expiring tickets so
HTML elements do not need bearer headers. Broadcast control requires an
authenticated `stream_master`/`stream_admin` account role or explicit `StreamControl` permission,
persists its revision, and rejects stale updates. Existing telemetry payloads and
bindings are unchanged. See [setup and API details](../backend/video-and-models.md).

Roles are returned in session `roles`, not `session_type`, and never imply hardware
command permission. `/api/live_streams` includes authoritative `can_manage_stream`,
`can_preview_live`, and a scoped `program_url`. Viewers and streamer mode embed the
delayed program; managers/operators retain separate live previews. Broadcast state
adds `delay_seconds` (3–60, default 10). Stream admins assign/revoke existing accounts
through `GET`/`POST /api/stream-roles`. See [broadcast contract and setup](../backend/broadcast-studio.md).

Vehicle configuration adds `motions` for named GLB nodes, phase states and telemetry
bindings. The bundled offline renderer implements rotation, translation, scale and
visibility with interpolation. `POST /api/vehicle_visualization` saves configuration
with hardware/configuration permission, returning `{ "saved": true }`; the existing
PUT returns 204. See [model mappings](../backend/model-animations.md).

Machine-readable examples are in [examples](examples/README.md), mirrored in the
sibling frontend's `docs/api-examples/`. Backend binding/profile configuration is
illustrated by [presentation.example.json](../backend/presentation.example.json).
Its custom settings must be merged into an existing profile, not blindly copied
over broadcast revisions, model selection or operator labels.

This document describes the backend surface the Dioxus frontend depends on to boot, seed state, remain connected, and
render the dashboard correctly.

The goal is not just to list routes. It is to describe what the frontend expects each route to mean, how often it is
used, and what failures degrade the UI.

## Transport Model

The frontend uses two transport paths:

- HTTP for bootstrap, point-in-time reads, layout/config, map config, calibration, and recovery.
- WebSocket for live telemetry, alerts, board state, topology, notifications/messages, action policy, and clock updates.

The frontend is designed to survive reconnects and partial data loss, but a fully working dashboard assumes the backend
exposes the same host for HTTP and WebSocket.

## Base URL and Connection Expectations

- Web builds default to the current browser origin unless overridden by the connection UI.
- Native builds default to `http://localhost:3000` unless overridden.
- The frontend assumes `/ws` is reachable at the same base host as the HTTP API.
- Native builds may intentionally skip TLS verification for local/self-signed deployments.
- Browser geolocation and some secure-browser behaviors work best with HTTPS.

## Startup Flow

On first connection or reconnect, the frontend does roughly the following:

1. Probe the configured base URL.
2. Fetch layout and point-in-time state over HTTP.
3. Seed recent telemetry history and alerts.
4. Seed board status, flight state, network time, topology, notifications/messages, action policy, and GPS.
5. Open the WebSocket.
6. Switch to incremental live updates.

The frontend can tolerate these returning in a slightly different order, but it assumes they all describe the same
backend instance and same mission state.

## Required HTTP Endpoints

### `GET /api/recent`

Purpose:

- Seed recent telemetry rows used by charts, latest-value widgets, and reconnect recovery.

Expected response:

- Default/fallback: JSON array of `TelemetryRow`.
- Preferred when the client sends `Accept: application/x-ndjson`: NDJSON stream with one `TelemetryRow` JSON object per line.

Shape:

```json
[
  {
    "timestamp_ms": 1742400000000,
    "data_type": "VALVE_STATE",
    "sender_id": "VB",
    "values": [1.0, 0.0, 0.0]
  }
]
```

NDJSON shape:

```text
{"timestamp_ms":1742400000000,"data_type":"VALVE_STATE","sender_id":"VB","values":[1.0,0.0,0.0]}
{"timestamp_ms":1742400000020,"data_type":"PT","sender_id":"FC","values":[512.4]}
```

Frontend expectations:

- `timestamp_ms` is milliseconds since Unix epoch.
- `data_type` is the primary routing key for widgets and charts.
- `sender_id` must stay stable across frontend/backend/device builds.
- `values` ordering must remain stable for a given `data_type`.
- NDJSON mode uses `Content-Type: application/x-ndjson; charset=utf-8`.
- Each NDJSON line is one complete `TelemetryRow` object followed by `\n`.
- Streamed rows are not wrapped in `[` or `]`.
- Empty history is either an empty NDJSON body or `[]` in array mode.

Failure impact:

- The dashboard still connects live, but charts and latest-value views start cold and reconnects produce visible gaps.

### `GET /api/alerts?minutes=20`

Purpose:

- Seed warning/error history shown in warning and error tabs.

Expected response:

```json
[
  {
    "timestamp_ms": 1742400000000,
    "severity": "warning",
    "message": "GPS fix lost"
  }
]
```

Frontend expectations:

- `severity` is `"warning"` or `"error"`.
- Timestamps must be comparable to telemetry timestamps.

Failure impact:

- Historical alerts are missing until live websocket alerts arrive.

### `GET /api/boards`

Purpose:

- Seed per-board seen/freshness state for connection and diagnostics views.

Expected response:

- `BoardStatusMsg`.

Shape:

```json
{
  "boards": [
    {
      "board": "ValveBoard",
      "sender_id": "VB",
      "seen": true,
      "last_seen_ms": 1742400000000,
      "age_ms": 120
    }
  ]
}
```

Frontend expectations:

- `board` names must deserialize to the shared enum.
- `age_ms` is already backend-computed freshness, not frontend-local RTT.

### `GET /api/layout`

Purpose:

- Provide the top-level tab list and the layout for state/data/network/calibration views.
- Provide the backend-defined action ordering and action presentation hints.

Expected response:

- JSON matching `frontend/src/telemetry_dashboard/layout.rs`.

Critical expectations:

- `main_tabs` may only contain known IDs:
    - `state`
    - `connection-status`
    - `map`
    - `actions`
    - `calibration`
    - `notifications`
    - `warnings`
    - `errors`
    - `data`
    - `network-topology`
    - `detailed`
- Tab IDs must be unique.
- `actions_tab.actions` order is authoritative and must be rendered in-order by the frontend.
- Action layout entries may include:
  - `illuminated: bool` to request backend-driven default illumination for that action.
  - `spacer_before: bool` and `spacer_after: bool` to request a visual separator around that action.
- Action layout entries may also include row-flow hints:
  - `new_row_before: bool` and `new_row_after: bool` to force the next action(s) onto a new row.
  - `spacer_row_before: bool` and `spacer_row_after: bool` to insert a blank spacer row before or after an action group.
- Frontend-specific action highlighting should not be hardcoded for particular commands; it should follow backend layout plus live `ActionPolicyMsg` updates.
- Data tab channel labels and boolean label sets must line up with actual telemetry value ordering.
- State widgets must reference valid data types and valid per-value indexes.

Failure impact:

- Invalid layouts are rejected or cause degraded UI behavior.
- Layout cache version changes on the frontend can force a reload of default layout ordering.

### `GET /api/calibration_config`

Purpose:

- Provide the calibration-tab UI schema.

Expected response:

- Calibration layout JSON loaded by the backend from `backend/config/calibration_config.json`, or
  from `GS_CALIBRATION_CONFIG_PATH` when set.
- The response includes `capture_target_samples`, top-level backend-supported `fit_modes`, and
  `sensors`.
- Each sensor provides `id`, `label`, `data_type`, `channel`, `fit_color`, `raw_label`,
  `expected_label`, optional `formatter`, and optional per-sensor `fit_modes`.
- The frontend must render calibration channels, labels, colors, and regression choices from this
  response. These values are intentionally not hard-coded in the frontend so future sensors can be
  added by backend config.
- When present, `formatter` uses the same shape as the data-tab formatter payload so calibration
  raw values can match the backend-defined display precision.

Supported regression ids:

- `best`
- `linear`
- `linear_zero`
- `parabolic` / `poly2`
- `parabolic_zero` / `poly2_zero`
- `cubic` / `poly3`
- `cubic_zero` / `poly3_zero`
- `quartic` / `poly4`
- `quartic_zero` / `poly4_zero`

Failure impact:

- Calibration UI cannot render correctly even if calibration endpoints exist.

### `GET /api/map_config`

Purpose:

- Tell the map tab the highest native tile zoom level available.

Expected response:

```json
{
  "max_native_zoom": 12
}
```

Failure impact:

- The map still renders, but zoom behavior may be wrong or conservative.

### `GET /flightstate`

Purpose:

- Seed current flight state before websocket updates arrive.

Expected response:

- A JSON-serialized `FlightState` enum value, for example:

```json
"NitrousFill"
```

Failure impact:

- The state tab boots in a stale/default state until the backend pushes a live state update.

### `GET /api/launch_clock`

Purpose:

- Seed the launch-clock state used by the top bar on connect and reconnect.

Expected response:

```json
{
  "kind": "t_minus",
  "anchor_timestamp_ms": 1742400000000,
  "duration_ms": 10000
}
```

Clock kinds:

- `idle`: no backend-started countdown is active.
- `t_minus`: `anchor_timestamp_ms` is the backend network time when countdown started and `duration_ms` is the countdown length.
- `t_plus`: `anchor_timestamp_ms` is T0 in backend network time and `duration_ms` is null.

Frontend expectations:

- Remaining T-minus time is `duration_ms - (network_now_ms - anchor_timestamp_ms)`, clamped at zero.
- Once `t_minus` starts, the backend preserves the first countdown anchor and ignores repeated launch commands or stale prelaunch state packets that would reset it.
- Once `t_plus` starts, the backend preserves the first T0 anchor and ignores all later attempts to reset it to idle, restart `t_minus`, or re-anchor `t_plus`.
- Clients should keep displaying `T- 00:00.00` at countdown completion until the backend sends `t_plus`.

### `GET /api/gps`

Purpose:

- Seed the current rocket GPS point for the map.

Expected response:

```json
{
  "rocket": {
    "lat": 42.9,
    "lon": -78.8
  }
}
```

Frontend expectations:

- `rocket` may be `null`.
- Latitude/longitude are decimal degrees.

### `GET /api/network_time`

Purpose:

- Seed backend clock time and feed periodic RTT/clock-delta estimation.

Expected response:

```json
{
  "timestamp_ms": 1742400000000
}
```

Frontend use:

- frontend-to-backend HTTP RTT estimate
- smoothed RTT estimate
- backend clock age
- frontend/backend wall-clock delta
- detailed diagnostics tab

### `GET /api/network_topology`

Purpose:

- Seed the current router/network topology graph and diagnostics tables.

Expected response:

- `NetworkTopologyMsg`.

Frontend expectations:

- `generated_ms` is the backend time the snapshot was created.
- Nodes and links come from SEDSnet discovery rather than a frontend or backend-assumed board topology.
- Node `kind`, `status`, `group`, `sender_id`, and `detail` are rendered directly.
- Node `stats` contains SEDSnet-reported `packets_sent`, `packets_received`, `bytes_sent`, and `bytes_received` counters.

### Firmware OTA endpoints

Firmware updates use the LaunchCore delta protocol over a routed SEDSnet v4 P2P stream. They require a session with
`send_commands`; a restricted command allowlist must also include `FirmwareUpdate`. Read-only status endpoints require
`view_data`. Only one update may be active at a time.

The sibling 2026 firmware repositories currently expose this live receiver only in `ActuatorBoard26`, on port 4510.
The other board entries are reported as unsupported until their application firmware implements the same receiver.
ActuatorBoard's build may also emit a full-image `.seds` fallback when a delta cannot be generated or does not fit; that
artifact is intentionally rejected here because its supported workflow is UART bootloader recovery, not live SEDSnet OTA.

- `GET /api/firmware/targets` lists `FC`, `RF`, `PB`, `VB`, `GB`, `AB`, and `DAQ`, including discovery, live-OTA support,
  workflow, and the reason an entry is unavailable.
- `POST /api/firmware/flash/{board}` accepts a raw `.seds` firmware artifact as the request body and returns `202 Accepted` with
  a job object. Set `Content-Type: application/octet-stream` and the required `X-Firmware-Filename` header; the filename
  must end in `.seds`.
- `GET /api/firmware/updates` returns recent jobs.
- `GET /api/firmware/updates/{id}` returns progress for one job.
- `POST /api/firmware/updates/{id}/cancel` aborts an active transfer and resets its stream.

Example:

```sh
curl -X POST \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/octet-stream" \
  -H "X-Firmware-Filename: ActuationBoard.seds" \
  --data-binary @ActuationBoard.seds \
  http://localhost:3000/api/firmware/flash/AB
```

The returned `phase` progresses through `queued`, `connecting`, `beginning`, `transferring`, `finishing`, `rebooting`,
and `completed`, or ends as `failed`/`cancelled`. `bytes_sent`, `total_bytes`, `progress_percent`, `board_max_bytes`, and
`message` provide operator feedback. A completed job means the board accepted the artifact and requested its LaunchCore
reboot; boot confirmation happens on the board after restart.

### `GET /api/notifications`

Purpose:

- Seed the current operator-notification set.

Expected response:

- Array of notification objects.
- Entries render in the frontend Notifications tab.

Shape:

```json
[
  {
    "id": 17,
    "timestamp_ms": 1742400000000,
    "message": "Sequence entered ArmedReady",
    "persistent": true
  }
]
```

### `GET /api/messages`

Purpose:

- Seed the current telemetry/backend message set.

Expected response:

- Array of message objects.
- Entries render in the frontend Messages tab.

Shape:

```json
[
  {
    "id": 41,
    "timestamp_ms": 1742400000500,
    "message": "FC: Flight computer entered ArmedReady",
    "persistent": false
  }
]
```

### `GET /api/action_policy`

Purpose:

- Seed current UI command enable/disable policy.

Expected response:

- `ActionPolicyMsg`.

Frontend expectations:

- The frontend uses this to disable or allow action buttons, not merely to decorate the UI.

## Optional or Feature-Specific HTTP Endpoints

Telemetry rates are compile-time board settings, not managed network variables.
There is no telemetry-rate HTTP endpoint.

### Calibration mutation endpoints

- `GET /api/calibration`
- `POST /api/calibration`
- `POST /api/calibration/capture_zero`
- `POST /api/calibration/capture_span`
- `POST /api/calibration/refit`

Legacy aliases still exposed:

- `GET /api/loadcell_calibration`
- `POST /api/loadcell_calibration`
- `POST /api/loadcell_calibration/capture_zero`
- `POST /api/loadcell_calibration/capture_span`
- `POST /api/loadcell_calibration/refit`

These are required for the calibration tab to be interactive.

Successful load-cell calibration changes also publish the four linear
coefficients through the retained `DAQ_LOADCELL_CALIBRATION` network variable.
DAQ persists the value and starts a new SD log before recording samples with
the new calibration.

Calibration documents keep legacy fields for `ch1` and `iadc`. Additional channels use
`extra_channels[channel_id]` with generic `linear`, `zero_raw`, `points`, and `fit` fields. Generic
points use `{ "expected": number, "raw": number }`.

### `POST /api/notifications/{id}/dismiss`

Purpose:

- Dismiss a persistent notification for all connected clients.

Expected behavior:

- `204 No Content` on success.
- `404 Not Found` if the ID no longer exists.

### `GET /tiles/{z}/{x}/{y}`

Purpose:

- Serve JPEG map tiles to browser builds.

Frontend expectations:

- `204 No Content` is acceptable for missing tiles.
- The frontend also supports native custom-protocol tile fetches that proxy through this route.

### `GET /favicon` and `GET /favicon.ico`

Purpose:

- Browser asset convenience endpoints.

### `GET /valvestate`

Purpose:

- Point-in-time convenience endpoint for the latest `VALVE_STATE` row.

Current role:

- Not the primary live path. WebSocket telemetry remains the authoritative live feed.

## Command Submission

### `POST /api/command`

Purpose:

- Send a `TelemetryCommand` to the backend over HTTP.

Expected request body:

```json
"Abort"
```

Expected behavior:

- `200 OK` when accepted.
- `403 Forbidden` when action policy currently blocks the command.

Current frontend use:

- The dashboard primarily sends commands over WebSocket.
- This endpoint still forms part of the documented API and is useful for tooling/tests.

## WebSocket Endpoint

### `GET /ws`

Purpose:

- Main live data path for the dashboard.

### Commands sent by frontend

Shape:

```json
{ "cmd": "Abort" }
```

The `cmd` value must deserialize into the backend command enum exposed by the API contract.

### Messages sent by backend

The backend sends tagged JSON objects:

```json
{ "ty": "TelemetryBatch", "data": [ ... ] }
{ "ty": "Warning", "data": { ... } }
{ "ty": "Error", "data": { ... } }
{ "ty": "FlightState", "data": { ... } }
{ "ty": "LaunchClock", "data": { ... } }
{ "ty": "BoardStatus", "data": { ... } }
{ "ty": "NetworkTopology", "data": { ... } }
{ "ty": "Notifications", "data": [ ... ] }
{ "ty": "Messages", "data": [ ... ] }
{ "ty": "ActionPolicy", "data": { ... } }
{ "ty": "NetworkTime", "data": { ... } }
```

Notes:

- The exact `ty` casing matters.
- The frontend currently expects `TelemetryBatch`; it does not rely on a `Telemetry` single-row message from the
  backend.
- The backend sends initial snapshots for notifications, action policy, topology, and network time immediately after
  websocket connect.

### Live message semantics

- `TelemetryBatch`
  High-rate telemetry rows. This is the hot path for charts and latest-value caches.
- `Warning` and `Error`
  Append to the respective alert lists.
- `FlightState`
  Updates the current mission state signal.
- `LaunchClock`
  Replaces the current launch-clock snapshot. Semantics match `GET /api/launch_clock`.
- `BoardStatus`
  Replaces the board-freshness view.
- `NetworkTopology`
  Replaces the current topology snapshot.
- `Notifications`
  Replaces the current active operator-notification set. This is for backend/operator workflow
  notifications such as fill completion or action-required notices.
- `Messages`
  Replaces the current telemetry/backend message set. This is for telemetry-network text
  messages such as flight-computer or router status strings.
- `ActionPolicy`
  Replaces current command enable/disable policy.
- `NetworkTime`
  Updates backend clock reference.

## Frontend Data Model Assumptions

The frontend is intentionally simple about schema discovery. It assumes:

- `data_type` strings are stable.
- `sender_id` values are stable.
- `values` index meanings are stable for a given `data_type`.
- timestamps are in epoch milliseconds.

If any of those change without corresponding frontend changes, the UI may still render but show wrong labels, wrong
channels, or wrong state.

## Backend Behaviors the Frontend Relies On

- `/api/recent` should return enough history to make reconnects look continuous.
- WebSocket initial snapshots should arrive quickly after connect.
- `BoardStatus`, `ActionPolicy`, and `NetworkTopology` should be treated as full-state replacement messages, not deltas.
- `Messages` should be treated as a full-state replacement message, not a delta stream.
- Clock endpoints should use a single consistent backend clock source.
- Alerts, notifications, and telemetry timestamps should all be comparable.

## Failure and Degradation Notes

- If only WebSocket fails, the frontend can still show seeded data but becomes stale.
- If only `/api/recent` fails, charts start empty and reseed poorly after reconnect.
- If `/api/layout` fails, the dashboard cannot safely render its configured structure.
- If `/api/network_time` fails, the detailed diagnostics lose RTT and clock-delta visibility.
- If `/api/action_policy` and websocket policy updates fail, the frontend may show controls in the wrong enabled state.
# GSE sequence integration

`GET /api/gse/status` returns `phase`, `message`, `nitrogen_passed`,
`self_test_locked`, `current_step_psi`, nullable `pressure_psi`, nullable `baseline`
(average/min/max/noise in psi and sample count), and five nullable boolean `valves`
ordered pilot, vent, dump, nitrogen, nitrous. Poll every 500 ms while visible.

`GET /api/gse/config` and authenticated `POST`/`PUT` expose `nitrogen_target_psi`,
nullable `pressure_ceiling_psi`, nullable `maximum_zero_offset_psi`, `grouped_panel`,
and runtime-only `dry_self_test_confirmed`. Edits are rejected during sequencing.
New command names are `StartFill`, `PauseFill`, `CancelFill`, `ValveSelfTest`, and
`NitrogenTest`; use existing action policy and command permissions, never direct
browser valve sequencing. Action layout entries accept an optional `group` string.
See [sequence behavior and limits](../backend/gse-sequences.md).
# Prelaunch model selection

`GET /api/vehicle_visualization` and delayed program `model` include
`ground_model_url`. The stock rocket defaults this to
`/assets/models/gse-site.glb` (tanks, manifold, plumbing, tower, and rocket).
Use this scene during Startup/Idle/PreFill/FillTest/NitrogenFill/NitrousFill/Armed;
use `model_url` at Launch and later. Unknown phases show the rocket only.
The delayed program selects using delayed telemetry, never current live phase.
Ground scenes remain upright and do not receive rocket-only node transforms.
Vehicle configuration writes currently accept the bundled ground URL or an empty
value; custom stored rocket models continue to use the existing model URL contract.
Empty stage component cards are not displayed; bindings remain backend-owned.
