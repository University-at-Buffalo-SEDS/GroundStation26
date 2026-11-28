# Stream manager (studio scenes)

The visual stream manager in the UI (Seds-Ground-Station-Frontend
`docs/broadcast-studio.md`, "Stream manager") edits layouts and scenes kept on
the station and takes a scene to the programme with a cut or a fade. The code
is `backend/src/media/studio.rs`; the wire schema is the UI's
`docs/backend-contract.md` section 5.12.1.

## Model

- **Layouts**: slot geometry in percent of a 16:9 frame, stacking order 0–9.
  Up to 32 layouts of 1–12 slots, each slot at least 5 % and inside the frame.
- **Scenes**: a layout plus a source per slot (`sources: {slot: source}`),
  and whether the programme's telemetry bar stays. Up to 24 scenes; a source
  fills one slot; one live diagram per scene (the programme page draws one).
- **Programme**: `broadcast.layout == "studio"` with
  `broadcast.studio.program_scene_id`, the last take's transition
  (`cut`, or `fade` up to 5000 ms) and `take_seq`, which the programme page
  watches to start the transition.
- **Sources**, listed by `GET /api/studio` with their state now: the relay's
  cameras (`camera:<stream>`, `live`/`no_signal`, hidden flag, ticketed
  preview), automatic cameras (`camera-auto:1..4`, the n-th visible live
  camera), the live diagrams from `backend/layout/diagrams` (`diagram:<id>`;
  `diagram:mode` follows the station mode, see `docs/live-diagrams.md`),
  telemetry views (cards, plots, T-clock, map, rocket graphic, recorded
  replay), transparent overlays (lower third, T-clock bug, telemetry strip)
  and graphics (title, slate). Telemetry state is `live` while the station
  received a row in the last 5 s, `stale` after, `no_signal` with none.

The presentation file stores the studio in `broadcast.studio`. A presentation
saved before this change gets the default layouts (the five aspect presets
and a main-plus-two) and nine default scenes (pad camera with lower third,
camera grid, camera + diagram, live diagram, telemetry, rocket graphic,
interview, recovery map, stand-by slate). Nothing changes on air until the
first take: until then the classic `layout` keeps working.

## Routes

All need stream-manager access (`can_manage_stream`).

| Route | Body | Notes |
| --- | --- | --- |
| `GET /api/studio` | | Layouts, scenes, sources, presets, Program, `revision` |
| `POST /api/studio/layouts` | `{revision, layouts}` | 409 on a stale revision; 400 when a saved scene still fills a layout or slot it removes |
| `POST /api/studio/scenes` | `{revision, scenes}` | 409 stale; 400 unknown source, two diagrams, or deleting the Program scene |
| `POST /api/studio/take` | `{scene_id, transition}` | No revision: a cut always applies. Bumps the broadcast revision, so a stale broadcast form cannot revert it |

`POST /api/live_streams/control` keeps `broadcast.studio` as saved (a form
never edits it) and accepts `layout: "studio"` only while the Program scene
exists. The rundown's per-phase scene does not override a studio programme.

## Programme page

The programme is composed in the viewer's browser (`program.html`); the
station only serves HLS camera segments and a delayed telemetry snapshot. So
a telemetry source is a live element of the page, not a server-side render:
no encoder, no headless browser and no per-frame work on the station.
`GET /api/media-assets/program/state` resolves the scene into `studio`
(slots with their sources, automatic cameras resolved to streams), returns
exactly the scene's live cameras in `streams`, and the topology of its
diagram. Query `scene=<id>` (the manager's Preview monitor) or
`source=<id>` (one source full frame, an OBS browser source) draws something
not on air and needs stream-manager access; both draw live even during an
instant replay. The snapshot carries diagram inputs while any scene shows a
diagram (the default scenes do).

## Cost

Measured on the development workstation (Ryzen 9 7900X), 2026-11-24:

- Station: the 4 Hz programme snapshot takes 21 µs without and 94 µs with
  diagram inputs (release build,
  `cargo test --release -p groundstation_backend measure_programme_snapshot_cost -- --ignored --nocapture`),
  0.04 % of one core. A Pi 5 core is roughly 3–4 times slower: about 0.15 %.
  Each viewer's state poll (2 Hz) reads that retained snapshot.
- Viewer: headless Chromium drawing a 1280×720 scene of a live diagram,
  T-clock, cards and a lower third at the page's 10 Hz tick: 3–4 % of one
  core on its main thread, 11 % across all browser processes (software
  compositing). This is spent in each viewer's (or OBS's) browser, not on the
  station.

## Tests

`media::studio` (model, validation, resolution and the routes end to end,
including the programme state and the access checks) and the
`contract_fixtures` `studio/*` files, which the UI decodes.
