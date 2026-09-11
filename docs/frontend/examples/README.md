# Media and dashboard contract examples

These JSON files are mirrored byte-for-byte in the sibling frontend's
`docs/api-examples/`. They illustrate current contracts, not live telemetry or
usable credentials. Replace `EXAMPLE_*_TICKET` values with server-issued scoped URLs.

- [Single-stage model](vehicle-visualization.json): actual stock schema and named
  phase motions; no upper fins, airbrakes or fabricated sensor bindings.
- [Manager streams](live-streams.json): three cameras and a customized two-stat
  profile, including capability flags and delayed program URL.
- [Viewer streams](live-streams-viewer.json): no live preview URLs or management access.
- [Dashboard status](dashboard-status.json): six default server-resolved fields,
  with null for an unavailable pressure reading.
- [Program state](program-state.json): delayed telemetry plus current editorial state;
  telemetry may instead be null while history warms.
- [Role list](stream-roles.json) and [role change](stream-role-change.request.json):
  admin-only API examples; assignment does not grant hardware commands.

The [presentation profile](../../backend/presentation.example.json) shows backend
binding configuration. Empty/missing `stats` inherits defaults; nonempty replaces
them. Merge intentional settings into an existing `_presentation.json`, preserving
model selection and revision state. No frontend binding editor is required.

Use the [API contract](../api.md) and [broadcast guide](../../backend/broadcast-studio.md)
for permissions, status codes, delay behavior and deployment details.
