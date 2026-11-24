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
- [Camera recordings](backend/video-recordings.md): original MP4 storage, review, and downloads.
- [Crew voice](backend/crew-voice.md): browser calls, push-to-talk, and microphone controls.
- [Model animations](backend/model-animations.md): single-stage default and backend
  binding configuration; custom multi-stage profiles remain supported.
- [GSE sequences](backend/gse-sequences.md): grouped control and equipment scene.
- [Per-account client settings](backend/client-settings.md): watch companion preferences
  synced to the signed-in account.
- [Watch direct link](backend/watch-direct.md): watch tokens, the compact watch status and
  watch-eligible page actions.
- [Vehicle geometry](vehicle-geometry.md): the airframe the dashboard draws, generated
  from the station's OpenRocket design and served at `GET /api/vehicle/geometry`.
- [Live diagrams](live-diagrams.md): the declarative fill, AV-bay, network and stand
  topologies, the valve/flow rules and the programme scenes.
- [Raw stream feed](backend/raw-stream-feed.md): read-only WebSocket/NDJSON feed of every
  telemetry stream for overlays and external scripts; stream viewer tokens.
- [Replay bundles](backend/replay-bundle.md): `.gsreplay` export of one recording for replay with no station.
- [Flight plans and loss-of-signal prediction](flight-plans.md): manual flight/burn simulations kept
  as predicted flights, and the drift prediction that guides recovery when the rocket goes quiet.
- [Frontend contract fixtures](backend/contract-fixtures.md): real replies the UI decodes;
  how to regenerate and run them.
- [Station mode switch](backend/station-modes.md): switch build and simulation
  from the UI by a controlled restart, on the primary and the RRLS.
- [Always-on recording](backend/rolling-recording.md): the rolling ring, protected
  event captures, disk policy and the recordings API.
- [Station power](backend/host-power.md): admin power off / reboot and the one-time
  host helper setup.

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

- [Application administration](application-administration.md): account roles, permissions and session revocation.
- [Personnel proximity lock](proximity-lock.md): opt-in lock on arming and filling while people are near the rocket; on-device geofencing, stale policy, override, RRLS.
- [Recovery search coverage](backend/recovery-search.md): opt-in searcher tracks, swath coverage of the predicted landing area, uncovered spots.
- [Optional RRLS radio voice](rrls-radio-voice.md): opt-in push-to-talk and radio budget.
- [Pico-Fi over Ethernet](picofi-ethernet.md): TCP client/server setup.

- [Flight command controls](flight-command-controls.md): threshold meanings and custom-layout migration.

- [Software acceptance](software-acceptance-2026-10-16.md): tested paths, platform results and device-validation limits.
