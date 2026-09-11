# System Overview

This document describes how the full system fits together across frontend, backend, shared contracts, and build/runtime
boundaries.

## Top-Level Components

- Sibling `Seds-Ground-Station-Frontend` repository
  Native and web Dioxus UI; the root build helper selects/clones its release or dev branch.
- `backend/`
  Axum server, telemetry ingest, persistence, sequencing, and hardware/radio control.
- `shared/gse_sequence/`
  Hardware-independent GSE sequence logic; HTTP/WebSocket DTOs are mirrored across repositories.
- `map_downloader/`
  Utility crate for map-related workflows.

## End-to-End Runtime Flow

1. The backend starts, opens transports, initializes state, and serves the API.
2. The frontend connects, fetches seed data, then opens a websocket.
3. Telemetry flows into the backend through radios/simulation.
4. The backend persists and republishes relevant state.
5. The frontend renders that state and sends commands back.

## Build Boundaries

For these contracts on `dev`, build with `python3 build.py --frontend-dev` (or
`python3 build.py docker --frontend-dev`) so the cached frontend checkout uses `dev`.
The default build selects the stable frontend branch and may not include the new UI.
Direct Docker builds use `--build-arg FRONTEND_DEV=TRUE`. Deploy both sides together
and recreate the video relay using its updated configuration.

The primary Dashboard is a single-stage model view. Ground Station view is a local
settings toggle for the instrument/control dashboard. Streamer is a separate delayed
audience program available to all viewers, with manager/admin roles required only
for editing or role assignment. Backend bindings supply cycling data and T clock;
the audience uses delayed snapshots, not live control data. See the
[broadcast guide](../backend/broadcast-studio.md) for setup and limitations.

- `build.py`
  Repository-level wrapper. Keeps old entry points but delegates actual work.
- `frontend/build.py`
  Frontend build, packaging, signing, and platform bundling.
- `backend/build.py`
  Backend-only cargo-oriented build entry point.

This split matters because the frontend is a real native app target in its own right and is not just a static asset pack
for the backend.

## Frontend/Backend Coupling

The frontend and backend are coupled in these ways:

- shared Rust DTOs and enums
- HTTP route shapes
- websocket message tags and payload types
- telemetry `data_type` and `sender_id` conventions
- backend-provided layout/config

They are intentionally decoupled in these ways:

- separate build scripts
- native frontend packaging does not require the backend to serve static files at runtime
- physical transport details stay backend-side behind the API boundary

## Source Tree Summary

### Frontend files

- `frontend/src/main.rs`
  runtime entry point and native protocol setup
- `frontend/src/app.rs`
  app shell and connection flow
- `frontend/src/telemetry_dashboard/*.rs`
  dashboard runtime plus individual tabs and support modules

### Backend files

- `backend/src/main.rs`
  backend entry point
- `backend/src/state.rs`
  shared runtime state
- `backend/src/telemetry_task.rs`
  telemetry ingest
- `backend/src/web.rs`
  API surface
- `backend/src/sequences.rs`
  sequence logic
- `backend/src/radio*.rs`
  transport abstraction and config

### Shared files

- `shared/gse_sequence/src/lib.rs`
  hardware-independent sequence logic; mirrored API types are documented in
  [Shared contracts](../shared/contracts.md).

## Operational Notes

- The web frontend depends on the backend serving `frontend/dist/public`.
- Native frontends still depend on the backend API, but not on backend static-file serving.
- Map behavior depends on tile assets under backend-managed data locations.
- Raspberry Pi OS and Ubuntu are both supported by keeping device access generic at the Linux device-node level where
  practical.
