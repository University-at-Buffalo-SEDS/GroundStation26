# Frontend Architecture

This document describes how the frontend is structured, how data moves through it, and which source files own which
responsibilities.

## Runtime Shape

The frontend is a Dioxus application that supports:

- web via `wasm32`
- desktop/native via Dioxus Desktop
- mobile/native packaging paths through the same Rust UI crate

The frontend is intentionally thin on business logic. It is mainly a stateful UI/runtime around backend-provided
telemetry, layout, and control policy.

## High-Level Data Flow

1. `frontend/src/main.rs` boots the app and platform runtime.
2. `frontend/src/app.rs` owns connection UI and top-level routes.
3. `frontend/src/telemetry_dashboard/mod.rs` seeds state from HTTP and then maintains the live WebSocket session.
4. Tab modules render from shared signals and telemetry caches.

The frontend avoids doing heavy work directly inside the websocket receive hot path. High-rate telemetry is buffered and
then flushed into render-facing state at a controlled cadence.

## File-by-File Ownership

### `frontend/src/main.rs`

Role:

- frontend executable entry point

Responsibilities:

- initialize panic logging/hooks
- choose wasm vs native runtime
- register the native custom protocol used for map tile proxying
- install the desktop window icon

Why it matters:

- this is the runtime boundary between Dioxus and the operating environment

### `frontend/src/app.rs`

Role:

- app shell and outer route/controller

Responsibilities:

- connection page and base URL management
- route switching between connect flow and dashboard
- native-specific helpers such as Android storage lookup
- version page for disconnected flow

Why it matters:

- this is the first frontend layer that knows where the backend is and whether the user is connected

### `frontend/src/telemetry_dashboard/mod.rs`

Role:

- dashboard runtime composition root

Responsibilities:

- HTTP bootstrap of history, alerts, layout, GPS, board status, action policy, notifications, topology, and network time
- WebSocket lifecycle and reconnect handling
- telemetry ingest queue and render-throttled flush path
- latest-value caches and shared signals
- action command send path
- top-level tab strip and tab selection
- dashboard-local persistence helpers

Why it matters:

- this is the single most important frontend file; most dashboard behavior flows through it

### `frontend/src/telemetry_dashboard/layout.rs`

Role:

- frontend layout schema and validation

Responsibilities:

- define the shape returned by `/api/layout`
- validate tab IDs and layout consistency
- provide default top-level tab ordering

Why it matters:

- backend-driven layout only works because this file defines the allowed contract

### `frontend/src/telemetry_dashboard/types.rs`

Role:

- frontend-local DTOs for dashboard rendering

Responsibilities:

- board status types
- network topology types
- telemetry row DTO

Why it matters:

- these types mirror backend payloads closely and are what most dashboard tabs consume

### `frontend/src/telemetry_dashboard/state_tab.rs`

Role:

- primary mission-state dashboard tab

Responsibilities:

- render widgets for the current `FlightState`
- map configured state layouts to telemetry-backed widgets
- show the main operator-facing state view

### `frontend/src/telemetry_dashboard/data_tab.rs`

Role:

- general telemetry data browsing tab

Responsibilities:

- render per-data-type channels
- show charts and latest values according to layout config

### `frontend/src/telemetry_dashboard/data_chart.rs`

Role:

- chart cache and chart ingest engine

Responsibilities:

- retain chart history efficiently
- handle reseed builds from `/api/recent`
- ingest live telemetry into chart caches
- manage chart refit/reset behavior

Why it matters:

- this file protects chart responsiveness under high telemetry rates

### `frontend/src/telemetry_dashboard/actions_tab.rs`

Role:

- command/action tab

Responsibilities:

- render operator command buttons
- apply action-policy enable/disable state
- forward commands to the backend

### `frontend/src/telemetry_dashboard/calibration_tab.rs`

Role:

- calibration UI

Responsibilities:

- render backend-provided calibration layout
- show/edit calibration channels
- call calibration capture/refit endpoints

### `frontend/src/telemetry_dashboard/connection_status_tab.rs`

Role:

- board connectivity and packet age view

Responsibilities:

- render board seen/freshness state
- render latency/age chart UI

### `frontend/src/telemetry_dashboard/latency_chart.rs`

Role:

- shared latency chart rendering helpers

Responsibilities:

- build chart geometry for packet-age/latency displays

### `frontend/src/telemetry_dashboard/detailed_tab.rs`

Role:

- diagnostics and low-level operational visibility tab

Responsibilities:

- show frontend/backend transport state
- show websocket traffic/bandwidth/session info
- show topology summaries and per-board/per-node detail
- expose clock freshness and alert counts

### `frontend/src/telemetry_dashboard/network_topology_tab.rs`

Role:

- topology graph/summary tab

Responsibilities:

- render the backend-provided network topology snapshot
- show nodes, links, and health state

### `frontend/src/telemetry_dashboard/map_tab.rs`

Role:

- mission map tab

Responsibilities:

- render rocket and user positions
- request tiles via browser HTTP or native custom protocol
- apply map config limits

### `frontend/src/telemetry_dashboard/gps.rs`

Role:

- cross-platform user-GPS abstraction

Responsibilities:

- provide a common GPS API for the map/dashboard
- delegate to web/native platform implementations

### `frontend/src/telemetry_dashboard/gps_android.rs`

Role:

- Android GPS and Android UI bridge

Responsibilities:

- call into Android-side helpers through JNI
- receive native location/heading updates
- control Android-specific features like keep-screen-on

### `frontend/src/telemetry_dashboard/gps_apple.rs`

Role:

- Apple-platform GPS integration

Responsibilities:

- provide iOS/macOS location integration where enabled

### `frontend/src/telemetry_dashboard/gps_webview.rs`

Role:

- webview/browser GPS path

Responsibilities:

- integrate browser geolocation behavior for supported runtimes

### `frontend/src/telemetry_dashboard/notifications_tab.rs`

Role:

- persistent notification history tab

Responsibilities:

- render stored notification history
- show timestamps and message text

### `frontend/src/telemetry_dashboard/warnings_tab.rs`

Role:

- warnings history tab

Responsibilities:

- render warning list and timestamps

### `frontend/src/telemetry_dashboard/errors_tab.rs`

Role:

- errors history tab

Responsibilities:

- render error list and timestamps

### `frontend/src/telemetry_dashboard/version_page.rs`

Role:

- version and build-information view used by the dashboard overlay and route-level version page

Responsibilities:

- render application/backend version information in a scrollable format

## State and Signal Model

The dashboard keeps a set of long-lived signals for:

- flight state
- board status
- topology
- warnings/errors
- notifications/history
- GPS points
- action policy
- frontend network metrics

The important design choice is that telemetry rows are not written directly into heavy UI structures on every websocket
message. Instead:

- websocket receive work stays small
- telemetry rows go into queue/cache structures
- render-facing state is updated on a controlled cadence

That prevents UI stalls and chart gaps under load.

## Persistence Model

The frontend persists a few local settings:

- configured backend base URL
- native TLS-skip preference
- some layout and UI state

On web builds this uses browser local storage.
On native builds this uses a JSON file under the app data directory.

## Map Tile Model

Browser builds:

- request `/tiles/{z}/{x}/{y}`

Native builds:

- use the custom `gs26` protocol from `main.rs`
- proxy tile requests through the configured backend

This keeps the same map source model across browser and native builds while letting native apps bypass
mixed-content/browser-origin restrictions.

## Frontend Build Boundary

The frontend build/package logic lives in `frontend/build.py`.

That script owns:

- web bundling
- desktop/mobile packaging
- Android packaging
- Apple bundle/signing paths
- frontend-specific icon handling

The root `build.py` is only a wrapper/orchestrator.
