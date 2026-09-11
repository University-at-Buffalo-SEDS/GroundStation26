# Documentation

This directory is split by runtime concern so frontend, backend, shared-contract, and whole-system docs can evolve
independently.

## Structure

- [Dashboard and media API contract](frontend/api.md): primary model view,
  backend-owned statistics, permissions and live/delayed data boundaries.
- [Broadcast studio](backend/broadcast-studio.md): stream roles, viewing modes,
  audience delay, API behavior and integration checks.
- [Video and stage models](backend/video-and-models.md): receiver/sender deployment
  and model administration.
- [Model animations](backend/model-animations.md): single-stage default and backend
  binding configuration; custom multi-stage profiles remain supported.
- [GSE sequences](backend/gse-sequences.md): grouped control and equipment scene.

- `docs/frontend/api.md`
  Frontend-facing API contract, endpoint details, websocket message shapes, bootstrap flow, and backend expectations.
- `docs/frontend/architecture.md`
  Frontend runtime architecture and file-by-file ownership for the sibling UI repository's `src`.
- `docs/backend/architecture.md`
  Backend runtime architecture and file-by-file ownership for `backend/src`.
- `docs/backend/i2c.md`
  Backend-facing I2C transport contract for the Pico bridge link.
- `docs/shared/contracts.md`
  Shared Rust types and the contracts they create between frontend, backend, and devices.
- `docs/system/overview.md`
  End-to-end runtime flow, build boundaries, packaging, and operational notes.

## Recommended Reading Order

1. Start with `docs/system/overview.md`.
2. Read `docs/shared/contracts.md`.
3. Read the frontend or backend architecture doc depending on which side you are changing.
4. Use `docs/frontend/api.md` when changing HTTP or WebSocket behavior.
5. Use `docs/backend/i2c.md` when changing the Linux-to-Pico I2C transport.
