# Recorded vehicle replay

Open **3D replay**, select a recording, then use Play, Rewind, the timeline and
0.25×–4× speed. Selecting an active recording freezes its current row boundary;
Reload explicitly selects a new snapshot. Missing recorded channels remain
unavailable. The live dashboard and audience program use cameras and 2D artwork.

Replay reads the selected database through authenticated, read-only APIs:

- `GET /api/recordings/{id}/replay` returns the frozen maximum row ID and recorded
  reception-time range.
- `GET /api/recordings/{id}/replay/window` accepts `upper_id`, `after_id`,
  `start_ms`, `end_ms` and a JSON array in `types`. Windows are limited to 30
  seconds and 32 types, with 512 rows per page. The first page includes the last
  earlier saved value for each requested source/type so recorded latched state
  remains available when seeking.

Rendering uses saved values and saved FC phase only. It never substitutes live
telemetry, current calibration or the current flight phase. Changing the native
app's station clears the recording selection; buffers are bound to station,
recording and snapshot. Timeline dragging commits a window request on release.
Animations follow the replay clock, including pause, slow motion and seeking.
The replay timeline uses saved reception timestamps; source timestamps remain
available in API rows and are not presented as a new monotonic time source.

The real-component browser regression is `tests/saved-vehicle-replay.mjs`.
Build with `dx build --platform web`, then run it with Node and Playwright.
`GS_FRONTEND_PUBLIC`, `GS_BACKEND_ROOT`, `PLAYWRIGHT_MODULE` and `CHROME_BINARY`
can select external build output, backend model assets and browser tooling.
The fixture serves local APIs only and never dispatches a hardware command.
