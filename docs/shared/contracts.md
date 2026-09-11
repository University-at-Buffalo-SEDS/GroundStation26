# Shared Contracts

This document describes the role of the shared crate and the contracts it creates between frontend, backend, and other
tooling.

## Source of truth

- `shared/gse_sequence/src/lib.rs`: shared, hardware-independent GSE sequence logic.
- `backend/src/types.rs` and `backend/src/telemetry_db.rs`: backend telemetry,
  flight-state, board and launch-clock wire types.
- `backend/src/auth.rs`: session, permission and stream-role wire types.
- `backend/src/media/contract.rs` and `backend/src/media/program.rs`: model,
  broadcast, role-management and dashboard-status contracts.
- The sibling frontend defines matching deserializers in `src/auth.rs` and
  `src/telemetry_dashboard/`; examples live in its `docs/api-examples/`.

There is no workspace-wide shared DTO crate at `shared/src/lib.rs`. Both sides'
serialized shapes must be kept compatible and example fixtures checked when changed.

## Why This Crate Exists

The frontend and backend must agree on:

- command names
- flight-state values
- board identity
- board-status DTOs
- telemetry row shape
- launch-clock DTO shape and monotonic countdown/T-plus semantics

These source modules and documented fixtures must stay aligned across repositories.

## Dashboard, program and model contracts

`/api/dashboard_status` serves live `{phase,t_clock,stats}` to the primary model
dashboard. `/api/media-assets/program/state` serves editorial camera/layout state
plus delayed telemetry to the streamer iframe. The latter must never substitute
current dashboard values when its history is warming or unavailable. Both display
nullable server-resolved statistics; bindings belong to backend `_presentation.json`.

`/api/live_streams` capabilities are authoritative: `can_manage_stream` controls
broadcast editing, `can_preview_live` controls access to undelayed camera feeds, and
`program_url` is a scoped audience URL. Account `roles` are independent of
`session_type` and hardware `send_commands`. Explicit `stream_viewer` overrides
legacy `StreamControl` except for a stream admin. See [broadcast rules](../backend/broadcast-studio.md).

The built-in vehicle is single-stage (`stage-1`) with aft fins only; stage/model
storage still supports custom multi-stage profiles. Asset tickets are transport
capabilities, not configuration values to persist or permission grants to edit models.

## Important Shared Types

### `TelemetryCommand`

Used for:

- HTTP command submission
- websocket command messages
- backend command routing/policy

Implication:

- renaming or reordering variants changes the control-plane contract

### `FlightState`

Used for:

- backend mission state
- frontend current state rendering
- sequence transitions
- persisted state history

Implication:

- changes here affect UI state tabs, backend policy, and persisted values

### `Board`

Used for:

- canonical board names
- sender ID mapping
- board status rendering

Implication:

- `sender_id()` and `from_sender_id()` are critical for joining telemetry sender IDs to UI board names

### `BoardStatusEntry` and `BoardStatusMsg`

Used for:

- `/api/boards`
- websocket board status updates
- connection status tab
- detailed diagnostics tab

### `TelemetryRow`

Used for:

- `/api/recent` array responses
- `/api/recent` NDJSON line payloads
- websocket telemetry batches
- chart ingest
- latest-value caching

Important fields:

- `timestamp_ms`
- `data_type`
- `sender_id`
- `values`

Implication:

- this is the core data-plane record shared between backend and frontend
- `/api/recent` may transport this schema either as a JSON array or as newline-delimited JSON objects; the row schema itself must not change between modes

### `LaunchClockMsg`

Used for:

- `/api/launch_clock`
- websocket `LaunchClock` messages
- reconnect/reseed launch-clock recovery

Important fields:

- `kind`: `idle`, `t_minus`, or `t_plus`
- `anchor_timestamp_ms`: backend network timestamp for countdown start or T0
- `duration_ms`: countdown duration for `t_minus`, otherwise null

Implication:

- `t_minus` is monotonic once started; the first countdown anchor is preserved until a `t_plus` transition.
- `t_plus` is final for a launch; the first T0 anchor is preserved and later stale packets must not reset or re-anchor it.

## Contract Stability Notes

The system depends less on schema discovery and more on stable conventions. In practice that means:

- enum variant names should remain stable unless all consumers are updated together
- telemetry `data_type` strings and `values` index meanings are effectively part of the contract
- sender IDs are part of the contract, not just a display detail
