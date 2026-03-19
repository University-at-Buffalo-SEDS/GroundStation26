# Frontend and Backend Architecture

This document describes how the groundstation frontend and backend work, how they communicate, and which modules own which responsibilities.

## System Shape

The system has three main Rust crates:

- `backend/`: Axum server, telemetry ingest, state management, persistence, radio I/O, sequencing, and hardware control.
- `frontend/`: Dioxus dashboard for web, desktop, and mobile.
- `shared/`: shared enums and DTOs such as `FlightState`, `TelemetryCommand`, and board status types.

At runtime:

1. The backend opens radio links, starts the SEDSprintf router, initializes state/DB/tasks, and serves HTTP/WebSocket APIs.
2. The frontend loads layout/config over HTTP, seeds recent history, then switches to WebSocket-driven live updates.
3. Commands originate in the frontend UI and are sent back to the backend, which enforces policy before relaying them to the flight-side network.

## Backend Architecture

### Entry point

`backend/src/main.rs` is the backend composition root. It is responsible for:

- GPIO initialization.
- map tile availability checks.
- SQLite setup and shutdown cleanup.
- application state creation.
- radio link opening and transport selection.
- router/task wiring.
- web server startup.

### State model

`backend/src/state.rs` owns the shared application state. It acts as the coordination point for:

- database pool access.
- current flight state.
- board freshness/seen state.
- notification store.
- action policy.
- network topology snapshots.
- shutdown coordination.
- cross-task channels.

Most live backend subsystems hold an `Arc<AppState>` and publish through it rather than talking to each other directly.

### Telemetry ingest

`backend/src/telemetry_task.rs` consumes decoded packet data from the SEDSprintf router and updates:

- telemetry database rows.
- in-memory caches/ring buffers.
- board status.
- flight state.
- websocket broadcast stream.
- network time/topology related state.

This is the main bridge between physical/network telemetry and the UI-facing state.

### Radio and transport layer

`backend/src/radio.rs` and `backend/src/radio_config.rs` handle the physical transport abstraction.

Current responsibilities:

- load link config from JSON.
- choose the correct transport implementation per link.
- open serial/UART, SPI, or CAN transports.
- provide transport-specific startup hints on failure.

The backend treats these as `RadioDevice` implementations so the rest of the telemetry/router stack does not care which transport a link uses.

### Sequence and operations logic

`backend/src/sequences.rs` owns the groundstation-side sequence state machine and prelaunch automation.

It is responsible for:

- fill-state progression.
- valve command timing/coordination.
- action policy updates that enable/disable UI commands.
- persistent notifications and operator guidance.
- local state promotion through the prelaunch sequence.

Incoming telemetry can still override local state when another board becomes authoritative.

### Safety and control

Supporting modules include:

- `backend/src/safety_task.rs`: safety-specific background logic.
- `backend/src/rocket_commands.rs`: command generation/routing.
- `backend/src/gpio.rs` and `backend/src/gpio_panel.rs`: local GPIO control surfaces.
- `backend/src/loadcell.rs`: loadcell calibration and persistence.
- `backend/src/layout.rs`: loads layout JSON used by the frontend.

### Web/API surface

`backend/src/web.rs` is the external API boundary. It exposes:

- bootstrap endpoints like `/api/recent`, `/api/layout`, `/flightstate`.
- diagnostics endpoints like `/api/boards`, `/api/network_time`, `/api/network_topology`.
- calibration/config endpoints.
- command endpoint(s).
- `/ws` live update stream.
- static frontend bundle serving.

The backend serves the built web frontend from `frontend/dist/public`.

### Persistence

SQLite is the primary persistence layer. The backend stores:

- telemetry rows.
- alerts.
- flight state history.
- other operational data needed to reseed the frontend after reconnect/restart.

The backend aggressively manages WAL/checkpoint behavior at startup and shutdown to avoid stale sidecar files and long restart recovery.

## Frontend Architecture

### Entry point

`frontend/src/main.rs` selects the frontend runtime:

- web build launches the Dioxus web app in the browser.
- native build launches the Dioxus desktop/mobile app.

For native builds it also registers a custom protocol handler used to fetch map tiles through the configured backend.

### App shell and connection flow

`frontend/src/app.rs` owns the outer route structure and connection/bootstrap UI.

Responsibilities include:

- connect/configuration screens.
- base URL persistence and normalization.
- reachability probing for required backend routes.
- native-specific helpers like local-network permission pokes and keep-awake shims.

### Dashboard core

`frontend/src/telemetry_dashboard/mod.rs` is the main runtime module for the dashboard.

It owns:

- HTTP bootstrap/seeding.
- WebSocket connection lifecycle and reconnect logic.
- telemetry buffering and render throttling.
- latest-value caches for widgets.
- notifications state.
- board status, flight state, topology, and network-time signals.
- command send path.

The dashboard intentionally decouples high-rate telemetry ingest from UI render cadence so charts and panels remain responsive.

### Tab modules

Each major dashboard area is implemented as its own module under `frontend/src/telemetry_dashboard/`, for example:

- `state_tab.rs`
- `data_tab.rs`
- `actions_tab.rs`
- `connection_status_tab.rs`
- `network_topology_tab.rs`
- `detailed_tab.rs`
- `calibration_tab.rs`
- `map_tab.rs`
- `notifications_tab.rs`

These modules mostly read from shared signals and cached telemetry rather than talking to the backend directly.

### Layout-driven UI

`frontend/src/telemetry_dashboard/layout.rs` defines the layout/config schema the frontend expects from `/api/layout`.

This allows the backend to control:

- visible top-level tabs.
- state tab sections/widgets.
- data tab channel groupings.
- calibration tab sensor layout.
- network tab presentation.

The frontend validates this layout before using it.

### Data model assumptions

The frontend is lightweight on schema discovery. It largely depends on:

- `data_type` strings.
- stable `sender_id` values.
- stable index ordering within telemetry `values`.

That keeps the runtime simple, but it means backend and frontend must stay aligned on telemetry conventions.

## Frontend/Backend Interaction Model

### Startup

On startup or reconnect, the frontend:

1. fetches recent telemetry.
2. fetches alerts, notifications, action policy, board status, network time, topology, GPS, and flight state.
3. fetches the layout config.
4. opens the WebSocket.
5. transitions to live incremental updates.

### Live updates

During steady state:

- telemetry batches arrive over WebSocket.
- frontend caches latest row/value per type and sender.
- charts and status widgets render from those caches/signals.
- backend also pushes notifications, action policy changes, flight-state changes, and topology updates through the same socket.

### Commands

Operator actions in the frontend become `TelemetryCommand` values.

Flow:

1. user clicks a control.
2. frontend sends a command.
3. backend checks action policy.
4. backend either rejects locally or forwards toward the flight-side network.
5. resulting state/telemetry changes come back through the normal telemetry/websocket path.

## Build and Packaging Model

There are now three Python build entry points:

- `build.py`: compatibility wrapper for full-project builds and Docker builds.
- `frontend/build.py`: frontend-focused builds, packaging, deploy, signing, and platform bundling.
- `backend/build.py`: backend-focused cargo builds and feature selection.

The root script keeps the old interface, but delegates local frontend/backend work to the scoped scripts.

## Operational Notes

- Web frontend assets must exist before the backend can serve the web dashboard.
- Native frontend builds do not require the backend to serve static files, but still require the runtime API.
- HTTPS is recommended for browser geolocation.
- Radio transports are Linux device-node oriented so Raspberry Pi OS and Ubuntu can use the same configuration model.
