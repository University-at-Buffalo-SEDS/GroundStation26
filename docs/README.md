# Documentation

This directory is split by runtime concern so frontend, backend, shared-contract, and whole-system docs can evolve
independently.

## Structure

- `docs/frontend/api.md`
  Frontend-facing API contract, endpoint details, websocket message shapes, bootstrap flow, and backend expectations.
- `docs/frontend/architecture.md`
  Frontend runtime architecture and file-by-file ownership for `frontend/src`.
- `docs/backend/architecture.md`
  Backend runtime architecture and file-by-file ownership for `backend/src`.
- `docs/shared/contracts.md`
  Shared Rust types and the contracts they create between frontend, backend, and devices.
- `docs/system/overview.md`
  End-to-end runtime flow, build boundaries, packaging, and operational notes.

## Recommended Reading Order

1. Start with `docs/system/overview.md`.
2. Read `docs/shared/contracts.md`.
3. Read the frontend or backend architecture doc depending on which side you are changing.
4. Use `docs/frontend/api.md` when changing HTTP or WebSocket behavior.
