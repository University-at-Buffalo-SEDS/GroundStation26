# Terrain, the 3D map and flight phases

The 3D map (UI map tab, 3D mode) and the 3D replay drape the station's map
imagery over a terrain mesh. Both come from this station, offline: the map
downloader prefetches the imagery and a digital elevation model (DEM) for the
configured regions before the trip, and the station serves them from its
cache. The station never fetches imagery or terrain from the internet while
serving; a tile that was not prefetched is simply absent and clients fall
back (DEM, then flat imagery at pad altitude, then the plain reference ground).

## Sources, attribution and licence

| Layer | Source | Licence / attribution |
| --- | --- | --- |
| Imagery | Esri World Imagery (`services.arcgisonline.com/.../World_Imagery/MapServer/tile/{z}/{y}/{x}`) | "Imagery: Esri, Maxar, Earthstar Geographics and the GIS User Community"; Esri's terms of use apply. |
| DEM | AWS Terrain Tiles, Terrarium PNG (`https://s3.amazonaws.com/elevation-tiles-prod/terrarium/{z}/{x}/{y}.png`), keyless open data (<https://registry.opendata.aws/terrain-tiles/>) | Mapzen/Tilezen terrain sources: SRTM, USGS 3DEP (NED), GMTED2010, ETOPO1 and others. Attribution is required; see <https://github.com/tilezen/joerd/blob/master/docs/attribution.md>. |

Terrarium encodes height as `metres = R * 256 + G + B / 256 - 32768`. The
deepest native zoom is 15. The UI shows both attribution lines in the 3D view.

## Prefetching

`python3 download_map.py` downloads the imagery and then the DEM for the same
regions (`--regions` / the picker). `--no-terrain` skips the DEM,
`--terrain-only` refreshes only the DEM, `--terrain-max-zoom Z` (default 12,
at most 15; `MAP_TERRAIN_MAX_ZOOM`) sets its depth. The DEM covers North
America at zooms 0–6 and the high-detail regions below that, about 5,000
tiles at the default depth for the default regions (a few hundred MB at
most). It goes to `backend/data/maps/<region>/terrain.sqlite3`
(`MAP_TERRAIN_BUNDLE_PATH` overrides), same schema as `tiles.sqlite3`, with
the same resume, reserve and validation rules (map_downloader/README.md).
The RRLS profile downloads its three boxes and shares the dataset budget.

## Routes

| Route | Permission | |
| --- | --- | --- |
| `GET /terrain/{z}/{x}/{y}.png` | none (like `/tiles`) | a raw cached Terrarium tile; 404 when not cached, 400 outside zoom 0–15 |
| `GET /api/terrain/heightmap?lat=&lon=&extent_m=&size=` | view_data | a decoded square grid around a point |
| `GET /api/terrain/status?lat=&lon=&radius_m=` | view_data | what is cached and how much of an area it covers |
| `GET /api/flight_phases` | view_data | the live flight's phases, events and rocket track |

`GET /api/terrain/heightmap`: `extent_m` 200–60,000 (default 12,000), `size`
2–257 (default 129). The station picks the zoom whose pixels match the
sample spacing (at most 64 source tiles, zoom ≤ the deepest cached),
fills a missing tile from its nearest cached ancestor (up to 6 levels) and
samples bilinearly. Rows run north to south, columns west to east, samples
evenly over `[-extent/2, +extent/2]` metres east and north of the point
(equirectangular, mean Earth radius 6,371,008.8 m, as the UI's gpsToLocal).

```json
{"available": true, "format": "gs26.heightmap", "version": 1,
 "lat_deg": 31.758, "lon_deg": -102.046, "extent_m": 12000.0, "size": 129, "spacing_m": 93.75, "zoom": 11,
 "min_m": 861.3, "max_m": 905.8, "encoding": "u16le-base64", "base_m": 861.0, "scale_m": 0.1, "values": "…",
 "exact_fraction": 1.0, "coverage": 1.0, "source": "AWS Terrain Tiles (Terrarium)", "attribution": "Terrain: …"}
```

Height of sample `i` (row-major) = `base_m + u16le(values)[i] * scale_m`.
`exact_fraction` is the share of samples from tiles at `zoom` (the rest came
from ancestors); `coverage` is the share with any DEM (the rest copy their
nearest neighbour). With no DEM for the area the reply is still 200:
`{"available": false, "format": "gs26.heightmap", "version": 1, "reason": "…", "source", "attribution"}`.

`GET /api/terrain/status` (default point: the configured launch site,
radius 6 km): `dem` (`available`, `tiles`, `zooms: [{z, tiles}]`,
`max_zoom`, `source`, `source_url`, `attribution`, `license`), `imagery`
(`available`, `max_zoom`, `source`, `attribution`) and `check`: per zoom
`{z, present, checked, total, fraction}` for the DEM (its deepest five zooms
from 8) and the imagery (zooms 12, 14, 16 and its deepest), with
`dem_complete` / `imagery_complete`. At most 256 tiles per zoom are checked
(strided over larger areas).

## Flight phases

`backend/src/flight_phases.rs` maps the station's flight states and the FC's
detailed phase (its FLIGHT_STATE, the station's `GS_FC_PHASE` rows and the
`flight_state` table) onto the shared vocabulary `pad`, `boost`, `coast`,
`drogue`, `main`, `landed` (apogee is an event):

| FlightState | phase |
| --- | --- |
| Startup … Armed (0–6) | pad |
| Launch, Ascent (7, 8) | boost |
| Coast (9) | coast (burnout event) |
| Apogee (10) | coast, plus the apogee event |
| Descent (11) | drogue |
| Reefing (12) | main |
| Landed, Recovery (13, 14) | landed |
| InFlight (16) | boost until a detailed phase is seen; never hides one |
| Aborted (15) | keeps the previous phase |

Segments are `[start_ms, end_ms)`; a point exactly on a boundary belongs to
the phase that starts there. Events: `liftoff`, `burnout`, `apogee` (at the
Apogee report, else at the drogue edge), `drogue`, `main`, `landing`, each
with the track position interpolated at that time when the track covers it.

- `GET /api/recordings/{id}/replay/trajectory` gains `phases:
  [{phase, start_ms, end_ms}]`, `events: [{kind, t_ms, latitude?,
  longitude?, altitude_m?}]` and `phase` on each point (absent before the
  first state). The last segment ends at the recording's last row.
- `GET /api/flight_phases` is the live equivalent for the current flight (the
  latest launch/HITL capture window, else the station's 20-minute recent
  cache): `{source: "live", window_start_ms, current_phase, phases, events,
  source_sender_id, points: [{timestamp_ms, latitude, longitude, altitude_m,
  phase}], sampled}`. The last segment is open (`end_ms: null`). The rocket
  track uses RF fixes when present, else FC fixes, at most 2,000 points.

Tests: `terrain::tests` (Terrarium decode, grid sampling and ancestor
fallback, no-DEM fallback, coverage checks), `flight_phases::tests`
(segmentation, boundaries, InFlight/Aborted rules, event placement, live
route), `recording_bundle::tests` and `contract_fixtures` (`terrain/*`,
`flight-phases/live`, `replay-trajectory/flight`), `map_downloader`
`layer::tests` and `tests/test_download_map.py`. No test touches the network;
DEM fixtures are synthetic tiles.
