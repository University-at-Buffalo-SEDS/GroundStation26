# Frontend API Contract

This document describes what the Dioxus frontend expects from the backend in order to boot, seed state, stay connected, and render the dashboard correctly.

## Overview

The frontend has two data paths:

- HTTP for bootstrap, configuration, and point-in-time reads.
- WebSocket for live telemetry, warnings/errors, flight state, notifications, topology, and other incremental updates.

The frontend is resilient to partial failures, but several endpoints are effectively required for a fully working dashboard.

## Base URL and Transport Expectations

- Web builds default to the current browser origin unless a custom base URL is configured.
- Native builds default to `http://localhost:3000` unless a custom base URL is configured.
- The frontend can connect over HTTP or HTTPS.
- Native mode can skip TLS verification for self-signed local deployments.
- The backend must expose the HTTP API and WebSocket on the same base host.

## Required HTTP Endpoints

### `GET /api/recent`

Purpose:
- Seeds recent telemetry history into the frontend on startup/reconnect.

Expected response:
- JSON array of `TelemetryRow`.

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

Notes:
- The frontend assumes timestamps are milliseconds since epoch.
- `data_type` and `sender_id` are used as routing keys for charts, cards, and state widgets.

### `GET /api/alerts?minutes=20`

Purpose:
- Seeds warnings/errors history shown in alerts and diagnostics views.

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

### `GET /api/layout`

Purpose:
- Provides dashboard layout configuration.

Expected behavior:
- Must return a layout matching the schema in `frontend/src/telemetry_dashboard/layout.rs`.
- Invalid or unknown tab IDs will break validation and cause the frontend to fall back or log errors.

Critical expectations:
- `main_tabs` should only include tabs known to the frontend.
- State/data/calibration/network section structures must be internally consistent.

### `GET /api/map_config`

Purpose:
- Tells the map tab the maximum native tile zoom.

Expected response:
```json
{
  "max_native_zoom": 12
}
```

### `GET /flightstate`

Purpose:
- Seeds the current flight state before live WebSocket updates arrive.

Expected response:
- A JSON-serialized `groundstation_shared::FlightState` enum value.

### `GET /api/gps`

Purpose:
- Seeds current rocket position for the map.

Expected response:
```json
{
  "rocket": {
    "lat": 42.9,
    "lon": -78.8
  }
}
```

### `GET /api/boards`

Purpose:
- Seeds per-board freshness/seen state used by connection and detailed status views.

Expected response:
- `BoardStatusMsg` from `shared/src/lib.rs`.

### `GET /api/network_time`

Purpose:
- Seeds backend clock time and is polled periodically by the frontend.

Expected response:
```json
{
  "timestamp_ms": 1742400000000
}
```

Frontend use:
- Estimates frontend↔backend RTT.
- Computes backend clock age and rough delta to frontend wall clock.
- Powers the Detailed tab diagnostics.

### `GET /api/network_topology`

Purpose:
- Seeds the live router/network topology graph.

Expected response:
- `NetworkTopologyMsg` as produced by the backend state layer.

### `GET /api/notifications`

Purpose:
- Seeds persistent notifications.

Expected response:
- JSON array of persistent notification objects.

### `GET /api/action_policy`

Purpose:
- Seeds button enable/disable policy for action and state controls.

Expected response:
- `ActionPolicyMsg`.

## Optional HTTP Endpoints

### Calibration

- `GET /api/calibration_config`
- `GET /api/calibration`
- `POST /api/calibration`
- `POST /api/calibration/capture_zero`
- `POST /api/calibration/capture_span`
- `POST /api/calibration/refit`

These are required for the Calibration tab to function correctly.

### Legacy aliases

The backend also serves:
- `GET/POST /api/loadcell_calibration`
- `POST /api/loadcell_calibration/capture_zero`
- `POST /api/loadcell_calibration/capture_span`
- `POST /api/loadcell_calibration/refit`

### Tiles and favicon

- `GET /tiles/{z}/{x}/{y}`
- `GET /favicon`
- `GET /favicon.ico`

The map tab can still render some UI without tiles, but the map experience is degraded.

### Valve state

- `GET /valvestate`

This is currently a point-in-time convenience endpoint rather than the primary live data path.

## Command Endpoint

### `POST /api/command`

Purpose:
- Sends a command encoded as `TelemetryCommand`.

Expected request body:
```json
"Abort"
```

Notes:
- The backend may return `403` if command policy currently disallows the command.
- The current frontend primarily sends commands over WebSocket, but the HTTP helper exists and the endpoint remains part of the contract.

## WebSocket Endpoint

### `GET /ws`

Purpose:
- Main live update channel.

Command messages sent from frontend:
```json
{ "cmd": "Abort" }
```

Live messages sent by backend use tagged JSON:
```json
{ "ty": "TelemetryBatch", "data": [ ... ] }
{ "ty": "Warning", "data": { ... } }
{ "ty": "Error", "data": { ... } }
{ "ty": "FlightState", "data": { ... } }
{ "ty": "BoardStatus", "data": { ... } }
{ "ty": "NetworkTopology", "data": { ... } }
{ "ty": "Notifications", "data": [ ... ] }
{ "ty": "ActionPolicy", "data": { ... } }
{ "ty": "NetworkTime", "data": { ... } }
```

Important expectations:
- The `ty` tag must match the backend/frontend serde variant names exactly.
- `TelemetryBatch` is the main high-rate feed and should be preferred over single-row ad hoc messages.
- The backend should send initial snapshots quickly after connection so the frontend does not sit in an unseeded state.

## Telemetry Semantics the Frontend Depends On

- `timestamp_ms` must be monotonic enough for charts and freshness calculations.
- `sender_id` should be stable per board, typically `GS`, `FC`, `RF`, `PB`, `VB`, `GW`, `AB`, `DAQ`.
- `values` ordering must stay consistent per `data_type`; the frontend does not have an out-of-band schema registry.
- Some UI pieces assume known data types like `VALVE_STATE`, `GPS`, `GPS_DATA`, or `ROCKET_GPS`.

## Static Asset Expectations

- The backend must serve `frontend/dist/public` as the frontend bundle root.
- Precompressed `.br` and `.gz` assets are used when present.
- Native desktop/mobile builds package their own frontend bundle and use the backend only for runtime API traffic.

## Failure Modes the Frontend Handles

- Missing optional endpoints usually degrade one tab or feature.
- Missing `/api/layout`, `/api/recent`, `/flightstate`, or `/ws` prevents a correct dashboard session.
- Invalid JSON bodies surface as user-visible errors or silent degraded panels depending on the feature.

## Backend Checklist

For a fully working frontend session, the backend should provide:

- `GET /api/recent`
- `GET /api/alerts`
- `GET /api/layout`
- `GET /api/map_config`
- `GET /flightstate`
- `GET /api/gps`
- `GET /api/boards`
- `GET /api/network_time`
- `GET /api/network_topology`
- `GET /api/notifications`
- `GET /api/action_policy`
- `GET /ws`
- Static frontend assets under `frontend/dist/public`
