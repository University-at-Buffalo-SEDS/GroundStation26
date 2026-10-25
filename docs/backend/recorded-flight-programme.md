# Saved flight in the audience program

The `recorded-replay` scene is selected explicitly by a stream manager through
`POST /api/media/broadcast`. Its `recorded_replay` plan contains the saved recording
name, frozen `upper_id`, timestamp bounds, relative position, server anchor,
speed, paused flag, loop flag and opt-in `follow_saved` flag. Broadcast revisions prevent concurrent operator
writes from silently replacing newer selections. The scene and plan persist in
`_presentation.json`. Camera clip replay remains a separate field and scene.

The operator can use `GET /api/recordings/{id}/replay/flight` to select the latest
saved launch/ascent through recovery, plus five seconds when available. This
freezes the current upper row ID. A recording without flight states uses its full
range. Overlapping wall-clock epochs fail explicitly rather than mixing flights.
By default refresh is an explicit new snapshot. Opt-in Follow newly saved data expands the saved snapshot at most once per five seconds across all viewers, preserves pause/playback position, and refuses a changed flight start. Its setting persists on the server and works without an operator device remaining connected. Every expansion increments the broadcast revision; concurrent stale controls remain protected. Playback never reads live telemetry.

The audience iframe is `/api/media-assets/recorded-vehicle?ticket=...`. Its state,
window and trajectory endpoints share that prefix. A program ticket authorizes
only the configured selection; recording/row identities in queries are assertions,
not selectors. Windows are limited to thirty seconds, thirty-two authoritative
configured types and 512 rows per page. Authentication and ticket revocation are
checked on every request. Model assets use separate model-scoped tickets; an
ordinary program ticket cannot read arbitrary model or recording endpoints.

Only one trajectory is cached, limited to 8192 sampled points. GPS sender selection
and discontinuities follow the saved trajectory API. The client fetches bounded
windows and renders saved timestamps at twenty frames per second; paused identical
frames do not redraw. Trail clipping uses one geometry and binary-search draw
ranges rather than reallocating trajectory buffers for each frame. Missing or
stale GPS shows reference artwork instead of claiming a current location.

Regression coverage includes persisted operator selection and restart, revision
conflicts, viewer write denial, saved snapshot isolation, bounds/path checks,
capability revocation and uploaded-model capability separation. Renderer/browser
checks are in the frontend repository. No endpoint dispatches output commands.
