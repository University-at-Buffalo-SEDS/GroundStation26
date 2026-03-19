# Backend Architecture

This document describes how the backend is structured and what each source file in `backend/src` is responsible for.

## Runtime Shape

The backend is an Axum-based service plus a set of long-running async/background tasks. It owns:

- radio/device transport
- packet routing and telemetry ingest
- SQLite persistence
- sequence and command policy logic
- HTTP/WebSocket API
- static frontend serving

Most backend subsystems coordinate through `AppState`.

## File-by-File Ownership

### `backend/src/main.rs`

Role:
- backend composition root

Responsibilities:
- load environment/config
- initialize SQLite and shared app state
- load radio link configuration and open links
- start telemetry, safety, sequence, and other background tasks
- configure and launch the Axum server

Why it matters:
- this file is where the backend process is wired together

### `backend/src/state.rs`

Role:
- shared runtime state and cross-task coordination hub

Responsibilities:
- own the database pool handle
- track current flight state
- track board status/freshness
- hold recent telemetry cache
- hold notifications and action policy
- hold network topology snapshot state
- provide broadcast channels used by websocket and tasks

Why it matters:
- almost every backend subsystem touches `AppState`

### `backend/src/telemetry_task.rs`

Role:
- live telemetry ingest and state update engine

Responsibilities:
- consume decoded telemetry from the router
- write telemetry and alerts into SQLite
- update in-memory recent telemetry cache
- update board status and topology-related state
- publish websocket-facing updates
- maintain the current network timestamp source

Why it matters:
- this is the main bridge from device/network traffic into backend state and UI-visible outputs

### `backend/src/web.rs`

Role:
- external API boundary

Responsibilities:
- define the HTTP router
- expose bootstrap endpoints such as `/api/recent`, `/api/layout`, `/api/boards`, `/flightstate`
- expose calibration and diagnostics endpoints
- expose `/ws` for live websocket updates
- serve tiles, favicon, and static frontend assets

Why it matters:
- this file defines the contract the frontend and external tools see

### `backend/src/sequences.rs`

Role:
- groundstation-side sequence/state-machine logic

Responsibilities:
- drive prelaunch/fill progression
- update action policy based on state and sequence phase
- emit operator notifications
- coordinate valve-related command timing and sequence rules

Why it matters:
- this is where the groundstation makes operational decisions before fully flight-owned states take over

### `backend/src/rocket_commands.rs`

Role:
- command translation/routing support

Responsibilities:
- build and route outbound command traffic toward the flight-side network

### `backend/src/safety_task.rs`

Role:
- background safety logic

Responsibilities:
- run safety-specific monitoring and enforcement tasks outside the main telemetry ingest path

### `backend/src/radio.rs`

Role:
- physical transport abstraction

Responsibilities:
- define the `RadioDevice` abstraction
- open and manage serial/UART, SPI, and CAN radio devices
- provide transport-specific error hints

Why it matters:
- this file isolates transport differences from the rest of the router/telemetry stack

### `backend/src/radio_config.rs`

Role:
- transport configuration model

Responsibilities:
- load and validate JSON radio-link configuration
- describe per-link transport choice and parameters

Why it matters:
- backend startup and the config GUI/TUI/CLI all converge on this schema

### `backend/src/gpio.rs`

Role:
- direct GPIO integration layer

Responsibilities:
- platform GPIO setup/control used by the backend for local hardware features

### `backend/src/gpio_panel.rs`

Role:
- GPIO-oriented control surface/task

Responsibilities:
- connect runtime state and action policy to a local GPIO control panel or hardware interface

### `backend/src/loadcell.rs`

Role:
- loadcell calibration model and persistence

Responsibilities:
- define calibration JSON format
- load/save calibration files
- implement capture-zero, capture-span, and refit logic
- expose calibration tab layout config used by the frontend

### `backend/src/layout.rs`

Role:
- backend-side loader for dashboard layout JSON

Responsibilities:
- define the backend copy of the layout schema
- load default layout files from `backend/layout`

Why it matters:
- the frontend consumes this schema through `/api/layout`

### `backend/src/map.rs`

Role:
- offline map/tile bundle helpers

Responsibilities:
- locate map bundles
- detect max native zoom
- support tile serving behavior in `web.rs`

### `backend/src/ring_buffer.rs`

Role:
- shared low-overhead buffering utility

Responsibilities:
- hold recent bounded data where a grow-forever vector would be wrong

### `backend/src/flight_sim.rs`

Role:
- simulation/HITL support

Responsibilities:
- provide backend-side flight simulation helpers for non-flight hardware testing modes

### `backend/src/dummy_packets.rs`

Role:
- test or development packet generation helpers

Responsibilities:
- provide dummy packet sources for testing, development, or simulation paths

## Data and Control Flow

Typical live flow:

1. A radio transport or simulation source produces decoded packet data.
2. `telemetry_task.rs` updates caches, state, and persistence.
3. `state.rs` broadcasts relevant changes.
4. `web.rs` pushes those updates to websocket clients.
5. Operator commands come back from the frontend.
6. `rocket_commands.rs` and sequence/policy code decide whether and how to forward them.

## Persistence Model

SQLite is used for:

- telemetry history
- alert history
- flight state history
- reconnect reseeding

The backend keeps recent data in memory as well so the frontend can recover quickly without relying only on cold DB queries.

## Static Assets and Layout Inputs

The backend also depends on:

- `backend/layout/layout.json`
- `backend/layout/layout_hitl.json`
- `backend/data/...`
- `backend/calibration/...`

It serves the built web frontend from `frontend/dist/public`.
