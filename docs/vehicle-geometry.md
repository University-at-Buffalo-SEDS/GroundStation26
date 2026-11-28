# Vehicle geometry

The station tells every client what the vehicle looks like, generated from its
OpenRocket design. The dashboard draws whatever it is sent: the 3D vehicle, the
saved-replay scenes, the live-diagram silhouettes and the programme's rocket.
A team with a different rocket drops in its own `.ork`; no UI change is needed.

## Source

The geometry is generated from, in order:

1. the `.ork` an administrator imported in the apogee planner
   (`POST /api/apogee_planner/ork`). The file is kept as
   `data/apogee_planner/vehicle_import.ork` and generated again at every start,
   so a generator change reaches an existing import. Clearing the import
   (`DELETE /api/apogee_planner/ork`) returns to the default;
2. the file named by `GS_VEHICLE_ORK`, read at startup;
3. the team's design built into the binary,
   `tools/flight_profile/vehicle/SWEET_V6.ork`.

The generator is `backend/src/vehicle_geometry.rs`. It reads the `.ork` with the
planner's parser (`apogee_planner/ork.rs`: ZIP, gzip or plain XML, the same
component placement rules), and takes the CG and Barrowman CP from the planner's
import, so the drawing and the planner agree. Nothing runs at build time and
nothing is checked in apart from the `.ork`.

## `GET /api/vehicle/geometry`

ViewData. Answers JSON with `ETag` (the source hash) and honours
`If-None-Match` with 304. The programme page reads the same body from
`GET /api/media-assets/program/vehicle-geometry?ticket=…`; the programme state
carries `vehicle_geometry_hash` so the page fetches it only when it changes. The
saved-replay page (`/api/media-assets/recorded-vehicle/state`) carries it as
`vehicle_geometry`.

Lengths are metres along the axis from the nose tip toward the tail (`x`);
radii are metres from the axis. A recorded reply for the built-in design is the
contract fixture `backend/tests/contract-fixtures/vehicle/geometry.json`.

| Field | Meaning |
|---|---|
| `schema`, `version` | `"gs26.vehicle_geometry"`, `1`. Clients refuse other values and draw a generic rocket. A breaking change bumps `version`. |
| `source` | `kind` (`default`, `env`, `import`), `file`, `hash` (`fnv1a64:` of the `.ork` bytes), `rocket`, `creator`, `designer`. |
| `length_m`, `diameter_m`, `span_m` | Overall length, largest body diameter, and the extent across the fins (body + 2 spans). |
| `cg_m`, `cp_m`, `stability_cal` | Dry CG (with the design's overrides), Barrowman CP and the margin in calibres; `null` if the planner cannot import the file. |
| `profile` | Outer mold line `[[x, r], …]` from the tip to the tail: the nose and curved transitions are sampled (nose 33 points, transitions 17), tubes are their end points. Steps (shoulders) appear as two points at one `x`. |
| `sections[]` | Each nose, tube and transition: `name`, `kind`, `role`, `x0_m`/`x1_m`, `r0_m`/`r1_m`, `shape`, `shape_parameter`, `material`, `color` (`#rrggbb` from the design's appearance, else `null`). |
| `nose` | `shape` (`conical`, `ogive`, `ellipsoid`, `power`, `parabolic`, `haack`), `shape_parameter`, `length_m`, `radius_m`, `shoulder_length_m`, `shoulder_radius_m`. |
| `fins[]` | Trapezoidal fin sets: `count`, `x_m` (root leading edge), `root_radius_m`, `root_chord_m`, `tip_chord_m`, `span_m`, `sweep_m`, `thickness_m`, `cross_section`, `cant_deg`, `angle_offset_deg`, and `planform` `[[axial, radial], …]` from the root leading edge. Other fin types are listed in `warnings`. |
| `internals[]` | Inner tubes, couplers, bulkheads and centring rings: `role` (`motor_mount`, `tank`, `coupler`, `bulkhead`, `centering_ring`, `tube`), extent and radii, for cutaways. |
| `tanks[]` | Tanks, `external` when they are part of the airframe (a section with role `tank`). |
| `recovery[]` | Parachutes and streamers: `role` (`drogue` for the first apogee device, else `main`), packed extent, `diameter_m`, `cd`, `deploy_event`, `deploy_altitude_m`, `deploy_delay_s`, `line_length_m`, `line_count`, and `separation_x_m`. |
| `separations[]` | Where the airframe opens: a recovery device leaves through the nearer end of its section. `fore`/`aft` name the sections either side. |
| `rail_guides[]` | Rail buttons and launch lugs (also mass objects named like one). |
| `motor` | The motor mount's motor entry: `designation`, `manufacturer`, `diameter_m`, `length_m`, aft-aligned `x0_m`/`x1_m`, `mount`. |
| `nozzle` | Exit plane `x_m`, `exit_radius_m`, `throat_radius_m`, `length_m`. OpenRocket stores no nozzle, so it is sized from the motor mount (`estimated: true`). |
| `parts[]` | Named parts (below) with their axial extent. |
| `svg` | A ready side profile in metres, nose at the origin pointing left: `view_box`, `body`, `fins` (one fin up, the others projected). |
| `warnings[]` | What the generator could not draw. |

**Section roles** come from the component names and contents: `nose`,
`avionics` (avionics, AV bay, electronics, e-bay), `payload`, `recovery`
(recovery, parachute, chute), `tank` (oxidiser, tank, N2O, nitrous, LOX),
`motor` (contains the motor mount, or motor/engine/combustion/fin can),
`boattail` (a narrowing aft transition) and `airframe`. Diagrams highlight by role.

**Named parts.** The bundled `vehicle.glb` and the vehicle configuration's
`motions` address nodes by name; a client building the vehicle from this reply
uses the same names: `stage-1`, `nose`, `airframe-upper` (nose to the first
separation), `airframe-join` (the coupler across it), `airframe-lower`,
`gimbal-pivot` (motor mount front), `engine`, `motor-flame` (nozzle exit),
`fin-pivot-0…N-1`, `drogue-parachute` and `main-parachute` (at their
separation).

## The built-in design (SWEET_V6.ork)

| | |
|---|---|
| Length | 4.387 m (14.4 ft) |
| Body | 156.5 mm (6.16 in); oxidiser tank 152.4 mm |
| Nose | Haack (Von Kármán, C = 0), 533.4 mm, 50.8 mm shoulder |
| Fins | 3, root 250.8 mm, tip 138.1 mm, span 174.6 mm, sweep 82.6 mm, 6.6 mm rounded carbon fibre, leading edge at 3.961 m |
| Sections | nose · avionics tube 0.386 m · payload tube 0.559 m · recovery tube 0.578 m · DITW container 0.697 m · oxidiser tank 0.813 m (aluminium, exposed) · lower body tube 0.667 m · Haack boattail 0.149 m to 108 mm |
| Recovery | reefed 1.778 m at apogee and main 3.556 m at 457 m, both out of the recovery tube's aft joint at 2.056 m |
| Motor | Midland 101 mm × 1.73 m in the combustion chamber (0.721 m, 101.4 mm OD) |
| CG / CP | 2.667 m (stage override) / 3.213 m, 3.49 cal dry |

## Change the vehicle

- At the station: Apogee planner → import the `.ork` (administrator). Every
  client redraws on its next fetch.
- For a deployment: set `GS_VEHICLE_ORK=/path/to/rocket.ork`.
- For the team default: replace `tools/flight_profile/vehicle/SWEET_V6.ork`
  (see that folder's README), then refresh the fixture:

```bash
cargo test -p groundstation_backend vehicle_geometry
GS_UPDATE_CONTRACT_FIXTURES=1 cargo test -p groundstation_backend contract_fixtures
```

and copy `backend/tests/contract-fixtures/vehicle/geometry.json` into the UI's
`tests/fixtures/backend-contract/` (`scripts/check_backend_contract_fixtures.py --update`).
`vehicle_geometry::tests::sweet_v6_geometry_matches_the_ork` checks the built-in
numbers against the design; update it with the design.
