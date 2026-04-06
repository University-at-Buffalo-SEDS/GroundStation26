# Shared Contracts

This document describes the role of the shared crate and the contracts it creates between frontend, backend, and other
tooling.

## Shared Crate

- `shared/Cargo.toml`
  Cargo manifest for the shared Rust crate.
- `shared/src/lib.rs`
  Shared enums and DTOs used across frontend and backend.

## Why This Crate Exists

The frontend and backend must agree on:

- command names
- flight-state values
- board identity
- board-status DTOs
- telemetry row shape

The shared crate keeps those contracts in one place so serde, command dispatch, and layout/state rendering stay aligned.

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

## Contract Stability Notes

The system depends less on schema discovery and more on stable conventions. In practice that means:

- enum variant names should remain stable unless all consumers are updated together
- telemetry `data_type` strings and `values` index meanings are effectively part of the contract
- sender IDs are part of the contract, not just a display detail
