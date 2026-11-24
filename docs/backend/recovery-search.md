# Recovery search coverage

After landing, an operator starts a **recovery search**. Searchers who opt in
on their phone (or on a watch, through the phone) share their GPS track while
the search runs. The station combines the tracks, each widened by a
configurable **swath width** (how far to either side a searcher can spot the
rocket), into a coverage layer. It reports how much of the predicted landing
area has been searched and which parts have not, so the team does not search
the same ground twice and can see the spots it missed.

Code: `backend/src/recovery_search.rs` (session, privacy gating, routes,
persistence), `backend/src/recovery_search/coverage.rs` (the geometry). The
UI side is the Map tab's recovery search panel (the UI repository's
`docs/backend-contract.md`, "Recovery search").

## Privacy

The proximity lock keeps location on the device and sends only an in/out
flag (`docs/proximity-lock.md`). The recovery search is the one feature that
sends a track, so it is narrow and explicit:

- **Opt-in for each device and each session.** A device shares only after its
  user opts in (`POST /api/recovery_search/opt_in` with `sharing: true`). The
  station records the opt-in for that user and device and returns an
  `opt_in_id`. Every upload must carry the current session id, the device id
  and that opt-in id. Opting in is refused while no search is running.
- **Stoppable at any time.** `sharing: false` is always accepted. It voids the
  opt-in id at once. With `delete_my_track: true` it also deletes that
  device's uploaded track.
- **Refused otherwise.** An upload is refused (409 `search_off`) when no
  search is running, (409 `session_mismatch`) for another session, or (403
  `not_opted_in`) without a valid opt-in. Fixes from before the opt-in (more
  than 2 minutes, for clock skew) are rejected. After a stop, fixes recorded
  **before** the stop are still accepted for 30 minutes, so a searcher who
  was out of signal can upload what they buffered. Nothing recorded after the
  stop is accepted.
- **Session-scoped storage.** Starting a new search drops the previous
  tracks and opt-ins. An administrator can clear the tracks at any time
  (`DELETE /api/recovery_search/tracks`). The tracks live in memory and in
  `recovery_search.json` in the data directory, so a station restart in the
  middle of a search keeps them. They are never written to the telemetry
  database. A replay bundle copies them (below). Station messages name the
  device and user ("Ada's iPhone (ada) is sharing a recovery search track"),
  never a place.
- **What viewers see.** Anyone with view permission sees the searchers' names,
  tracks and the coverage: that is the point of the feature. The status
  never contains device ids or opt-in ids, except the caller's own (`you`,
  requested with `?device_id=`).
- **Local-only track.** A user who does not opt in can still draw their own
  track on their own map. The UI keeps it on the device and never sends it.

## Routes

| Route | Permission | |
|---|---|---|
| `GET /api/recovery_search[?device_id=]` | view | status (below) |
| `POST /api/recovery_search/start` | send commands | optional `{"swath_width_m", "target": {"polygon", "source", "label"}}`; 409 `already_active` |
| `POST /api/recovery_search/stop` | send commands | 409 `not_active` |
| `PUT /api/recovery_search/settings` | send commands | `{"swath_width_m"?: 2–500, "target"?: {...} or null}` |
| `POST /api/recovery_search/opt_in` | view, signed in | `{"device_id", "device_name", "platform"?, "sharing", "delete_my_track"?}` |
| `POST /api/recovery_search/track` | view, signed in, opted in | a batch of fixes (below) |
| `DELETE /api/recovery_search/tracks` | admin | clears every track |
| `GET /api/recovery_search/export[?from_ms&to_ms]` | view | the session with its tracks trimmed to the window, without ids; 404 `no_tracks` when empty |

Start, stop, settings and clear answer with the status. Refusals are
`{"error": text, "code": code}`.

`target.polygon` is `[lat, lon]` points, a closed ring without the first point
repeated, 3–200 points and at most 30 km across. `source` is a lowercase name:
`los_prediction`, `flight_plan` or `manual` (the default when an operator
sets it).

### Upload

```json
{"v": 1, "session_id": "rs-…", "opt_in_id": "…", "device_id": "phone-…",
 "points": [{"seq": 17, "t_ms": 1767225600000, "lat": 31.76, "lon": -102.03, "acc_m": 5.0}]}
```

- At most 500 points per batch and 30 batches a minute per device. Unknown
  fields are rejected.
- **Idempotent.** The station keeps one fix per (user, device, `seq`). A
  retried batch counts as `duplicates`, and a late or out-of-order batch merges
  in place. The coverage orders each track by `t_ms`.
- A fix is rejected when its coordinates are invalid or (0, 0), its accuracy
  is worse than 100 m, or its time is outside the opt-in window.
- Limits: 20,000 fixes per device, 200,000 per session and 64 devices (8 per
  user). Past a limit the reply has `full: true`.
- Reply: `{"accepted", "duplicates", "rejected", "full", "stored_points",
  "max_seq"}`. The device drops what it sent once it gets the reply.

### Status

```json
{"format": "gs26.recovery_search", "version": 1,
 "active": true, "session_id": "rs-…", "started_ms": 0, "started_by": "operator", "stopped_ms": null, "stopped_by": null,
 "cleared_ms": null, "cleared_by": null,
 "accepting_uploads": true, "late_upload_until_ms": null,
 "settings": {"swath_width_m": 30.0},
 "limits": {"swath_width_m": [2.0, 500.0], "max_batch_points": 500, "max_points_per_device": 20000, "max_points_total": 200000,
            "max_devices": 64, "late_upload_grace_s": 1800, "max_accuracy_m": 100.0, "polygon_max_points": 200},
 "target": {"polygon": [[31.76, -102.03]], "source": "los_prediction", "label": "95% landing area", "set_ms": 0, "set_by": "station"},
 "stats": {"percent_covered": 42.5, "target_m2": 250000, "covered_in_target_m2": 106250, "uncovered_m2": 143750, "swath_m2": 180000,
           "cell_m": 3.75, "target_cell_m": 3.75, "searchers": 3, "sharing": 2, "points": 5200, "distance_m": 12400},
 "searchers": [{"id": "26de433f44f4", "name": "Ada's iPhone", "username": "ada", "platform": "ios", "color": "#0ea5e9",
                "sharing": true, "points": 2100, "distance_m": 5100, "first_ms": 0, "last_ms": 0, "opted_in_ms": 0, "opted_out_ms": null}],
 "overlay": {"id": "search_coverage:rs-…", "kind": "search_coverage", "label": "Search coverage 42%", "predicted": false,
             "tracks": [{"id": "searcher:26de433f44f4", "phase": null, "label": "Ada's iPhone", "style": "search", "color": "#0ea5e9", "points": [[31.76, -102.03, null]]}],
             "markers": [],
             "areas": [{"id": "search_swath-0", "kind": "search_swath", "label": "Searched", "polygon": [[31.76, -102.03]]},
                       {"id": "uncovered-0", "kind": "uncovered", "label": "Not yet searched", "polygon": [[31.76, -102.03]]}]},
 "you": {"device_id": "phone-…", "id": "…", "sharing": true, "opt_in_id": "…", "points": 2100, "max_seq": 2100}}
```

- The `stats` area fields and `percent_covered` are null without a target.
- `you` is null without `?device_id=` or for an anonymous caller.
- `overlay` is a MapOverlay (the flight-predict contract) of kind
  `search_coverage`. Each searcher's track has a stable id and a colour from
  `#0ea5e9 #eab308 #ec4899 #14b8a6 #f97316 #8b5cf6`, and is thinned to 2,000
  displayed points. The coverage itself uses every fix.

### Target area hook

The loss-of-signal prediction (or a flight plan) sets the predicted landing
area with:

```rust
pub(crate) fn set_target_area(state: &AppState, polygon: &[[f64; 2]], source: &str) -> Result<bool, String>
```

`source` must be a lowercase name other than `manual`. The hook returns
`Ok(false)` and changes nothing when an operator has set a manual area. Remove
that area (`PUT … {"target": null}`) to follow the prediction again. An
operator can also copy the prediction's polygon from the UI ("Use predicted
landing area"), which stores it with source `los_prediction` or `flight_plan`.

## Coverage algorithm

The work is done on a bounded raster in a local east/north frame in metres
(equirectangular around a reference point):

1. Each track is ordered by time and split into segments. A pause of more
   than 120 s or a jump of more than 150 m breaks a segment: a GPS dropout is
   not coverage.
2. A cell is covered when its centre lies within half the swath width of a
   segment (a capsule). Overlapping tracks set the same cells, so nothing is
   counted twice: that is the union.
3. The target polygon is scan-converted onto its own grid. The percentage is
   covered target cells divided by target cells. The remaining target cells
   are the uncovered area.
4. The covered cells (over the extent of all tracks) and the uncovered cells
   are traced into polygons along the cell edges. A component with holes
   becomes one weakly simple ring, cut by a zero-width bridge from each hole up
   to its outer boundary. Any renderer then fills it correctly, and a missed
   patch inside a searched area stays visible as a hole. Rings are simplified
   (Douglas–Peucker, starting at half a cell) to at most 400 points. At most
   64 polygons of each kind are drawn, the largest first. Uncovered specks
   under 2 cells are not drawn but are still counted.

The cell size is the swath width / 8, clamped to 2–5 m (3.75 m for the
default 30 m swath). It grows only when an area would need more than 250,000
cells, so each grid is bounded. The cost is O(cells + fixes × cells per
capsule). A large search (10 searchers, 8,460 fixes, a 1.2 × 0.6 km area,
114,000 cells) computes in about 34 ms in a debug build on a desktop. Expect
a few milliseconds in release, and roughly three times that on a Raspberry Pi
5. The result is cached per change. The status route recomputes only after a
change and runs off the async runtime.

## Replay bundles

`GET /api/recordings/{id}/bundle` adds `derived/recovery_search.json` when
the session has fixes between the recording's first row and 6 hours after
its last row. The file is the `export` body (the status shape without ids,
with `window`). A bundle without search fixes is unchanged. The UI replay
draws its overlay on the replay map.

## Checks

- `recovery_search::coverage::tests`: a straight track through a rectangle
  (7.5 %), overlapping and half-overlapping tracks (no double counting), lanes
  one swath apart (everything covered), a missed spot inside a loop (bridged
  hole), a GPS gap, no tracks, bounded grids, tracing.
- `recovery_search::tests`: opt-in gating (no session, no opt-in, a wrong id,
  another user, another session, unknown fields, opting out, anonymous), the
  time window and the late-upload grace, retried and out-of-order batches
  merging to the same coverage, permissions, session scoping and clearing,
  the target hook, the export window and persistence.
- `contract_fixtures` writes `recovery-search/*.json`.
- `large_search_cost` (ignored) prints the timing above.
