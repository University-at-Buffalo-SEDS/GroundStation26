# Live diagrams

The dashboard's **Live diagrams** tab draws P&ID-style views from live
telemetry: **one combined view per station mode**, opened automatically from
the station mode, plus the system overview. The operator can still pick another
view; a change of station mode returns to the new mode's view. Every view can
be shown **full screen**. The audience programme can show the same views
(delayed with the video).

| View | File | Station modes | Shows |
| --- | --- | --- | --- |
| Flight: fill system and vehicle on the pad | `backend/layout/diagrams/flight.json` | normal, HITL, testing | The fill system (bottles, regulator, AB solenoids, fill manifold, QD, run tank, VB vent/dump/main valves; tank PT, fill level, igniter, fill battery) with the **KG50 under the motor** (fill mass). The vehicle's AV bay: FC, PB, RF on the AV CAN bus; radio to the GS and RRLS; IMU, barometer, GPS; estimator, FC phase, pyro channels, underglow/buzzer, FC SD; the vehicle drawn to scale. No KG1000 and no chamber PT. |
| Test fire: fill system and motor on the stand | `test_fire.json` | test fire | The same fill system, and the motor on the stand: the **KG50** (hanging vehicle), the **KG1000** (thrust, in kg and × g in N) and the **chamber PT** (with "installed"). No AV bay. |
| Integrated ground test: stand and rocket | `ground_test.json` | integrated ground test | The test-fire view, plus, only while the ground test runs, the rocket on the ground as a separate body: FC, PB and RF status, FC phase, ground-test mode, replayed estimator, pyro channels (DRY-RUN), simulated altitude and FC SD. |
| System overview | `network.json` | any (picked by hand) | GS, RRLS, Pico-Fi, gateway, fill and AV CAN buses, every board, with live link health. |

The overview stays a separate view rather than being folded into each mode
view: board and link health for the whole system is a secondary question, and
the mode views already show each board they depend on (AB, GW, DAQ, VB; FC, PB,
RF) as live status chips.

### Mode to view

The layout chooses. Each layout file maps station modes to views:

```json
"live_diagrams": {"normal": "flight", "testing": "flight", "integrated_ground_test": "ground_test"}
```

`layout.json` serves normal, testing and IGT; `layout_hitl.json` maps `hitl`;
`layout_test_fire.json` maps `test_fire`. The served layout (`GET /api/layout`)
carries the view for the mode it is served in as `live_diagram`. A mode the map
does not list gets the built-in default (`diagrams::default_view`, the same as
the table above). A replay of a recording made in another mode gets that mode's
layout, and so its view.

A layout without `live_diagram` (an older station, an older replay bundle) is
still shown correctly: the dashboard picks the view from `test_fire_mode` and
the IGT status. Old per-system ids still resolve: `fill` and `av_bay` show
`flight`, `stand` shows `ground_test`.

### Full screen

Each view has a **Full screen** button. On the web, the desktop app and
Android it uses the Fullscreen API on the diagram (prefixed on Safari). Where
the platform has none for elements (an iPhone) or refuses it, the diagram
covers the whole viewport instead. **Esc**, the platform's own exit and
**Exit full screen** all leave. The SVG scales to fill the screen and stays
crisp and live. The programme page has the same button, shown only while the
pointer moves, so a captured programme never shows it.

Client side: Seds-Ground-Station-Frontend `src/telemetry_dashboard/live_diagrams/`
(`docs/live-diagrams.md` there).

## Editing a topology

The files are declarative JSON. Edit a file when the plumbing or the bay
changes. The station serves the files from `GET /api/diagrams` (ViewData), and
the dashboard picks them up on the next page load. No rebuild is needed.

- `GS_DIAGRAMS_DIR` overrides the directory.
- The file name must equal the topology `id`.
- The station checks the structure (`diagrams::check`): schema, unique ids, and that every edge's nodes and valves exist. A failing file is reported in `errors` and not served.
- The three mode views repeat the fill system; edit it in all three (the tests check that they carry the same valves and fill elements).
- The client validates fully (`live_diagrams::topology::validate`) and falls back to its bundled copy of any file that fails.
- Keep the client copies (`src/telemetry_dashboard/live_diagrams/topologies/`) identical to these files.

### Schema 1

```json
{
  "schema": 1, "id": "fill", "title": "Fill system", "description": "…",
  "view_box": [0, 0, 1200, 660], "stale_ms": 5000,
  "sources": ["repo paths the topology was derived from"],
  "fluids":  [{"id": "n2", "label": "Nitrogen gas", "slot": 2, "dashed": true}],
  "frames":  [{"id": "ground", "label": "…", "x": 16, "y": 48, "w": 560, "h": 596, "when": "igt"}],
  "nodes":   [{"id": "tank", "kind": "tank", "label": "Run tank", "x": 680, "y": 160, "w": 130, "h": 300,
               "pressure": {"telemetry": "PRESSURE_TRANSDUCER_CALIBRATED", "sender": "VB"},
               "level": {"telemetry": "LOADCELL_FILL_PERCENT"}}],
  "valves":  [{"id": "qd", "label": "Quick disconnect", "x": 586, "y": 308, "kind": "qd",
               "state": {"telemetry": "VALVE_STATE", "index": 6}, "open_when": "low",
               "command": "RetractPlumbing", "open_text": "Connected", "closed_text": "Disconnected"}],
  "edges":   [{"id": "fill_line", "from": "manifold", "to": "tank", "through": ["qd"], "fluid": "inherit",
               "points": [[456, 308], [680, 308]], "min_dp_psi": 5}],
  "readings":[{"id": "tank_pt", "label": "Tank PT (VB)", "x": 830, "y": 170,
               "value": {"telemetry": "PRESSURE_TRANSDUCER_CALIBRATED", "sender": "VB"},
               "unit": "psi", "precision": 0, "warn_above": 850}]
}
```

**Frames** may carry `"vehicle": {"x", "y", "w", "h", "highlight": ["avionics"]}`:
the station's vehicle (docs/vehicle-geometry.md) drawn nose up, true to scale, centred
in that box, with the sections of the listed roles highlighted and the separations
marked. The stand's IGT frame and the AV-bay view's `vehicle` frame use it.

**Node kinds.** `bottle`, `regulator`, `junction`, `tank`, `chamber`,
`atmosphere`, `board`, `sensor`, `battery`, `station`, `bus`, `load_cell`,
`pyro` and `output`.

- A `junction` has exactly one outgoing pipe.
- Its outgoing pipe may use `"fluid": "inherit"`.

**Node fields.**

- `pressure`: drives the flow rule.
- `fallback_psi`: used when `pressure` has no fresh value. The chamber's `0` means it is open to the atmosphere through the nozzle.
- `level`: liquid level, 0–100 %.
- `status`: online.

**Valve kinds.** `solenoid`, `servo`, `qd` and `manual`.

- A `manual` valve is a hand valve the station does not monitor. It has no `state` and counts as open.
- `open_when: "low"` inverts the value. The QD reports "retracted".
- `command` names the action-policy control. Its `requested` field is the commanded position.

**Edge kinds.** `pipe` (default), `can`, `radio`, `uart`, `ethernet`, `power`
and `mechanical`.

- Only pipes carry a `fluid` and pass `through` valves.
- `follows` copies another pipe's flow (the bottle stub follows its branch).
- Links use `health`.

**Bindings.** Every value names exactly one source:

- `{"telemetry": TYPE, "sender"?: ID, "index"?: n, "scale"?: k}`: the latest row. Its age is the receipt age.
- Any binding may add `"stale_ms"`, which overrides the topology's (and a reading's) `stale_ms`. The flight view keeps the fill system at 5 s and gives the AV bay's radio data (boards, links, readings) 10 s.
- `{"constant": v, "assumed": true}`: a nominal value, drawn with "≈".
- `{"board": SENDER}`: up while the station hears the board.
- `{"control": CMD}`: the action policy's confirmed `actuated`.
- `{"status": KEY}`, one of:
  - `flight_state`, `fill_phase`, `fc_phase`
  - `estimator`, `estimator_rate_hz`
  - `igt_mode`
  - `pyro_drogue`, `pyro_main`, `pyro_continuity` (never reported; drawn "not reported by the FC")
  - `fc_sd`
  - `radio_rssi`
  - `provenance`
  - `topology:SENDER`

**`when`.** `"igt"` or `"not_igt"` shows an element only in (or outside) an
integrated ground test.

## State rules

These are implemented twice, identically:

- dashboard: `live_diagrams/derive.rs`;
- programme: `station_ui/scripts/program-diagram.js`.

Both are tested on the same topology, by Rust unit tests and
`tests/station-program-diagram.test.cjs`.

- **Valve.**
  - Open or closed comes from the confirmed telemetry (`VALVE_STATE`, the station's confirmed umbilical states).
  - While a command is pending, the action policy carries `requested`, and a `requested` that differs from the confirmed state is drawn **pending** (amber, dashed).
  - A running QD retraction is drawn pending as well.
  - Older than `stale_ms`: **stale**, grey, with its age and the last state.
  - Never reported: **unknown**.
- **Pipe.**
  - Flows only when every valve on its path is open AND the upstream pressure exceeds the downstream pressure by `min_dp_psi`.
  - A pipe into a junction is judged against the junction's outgoing valves and its destination.
  - **Blocked** (faded) when a valve is closed. A pending open from a closed valve is still closed.
  - **Unknown** (grey dashes, never animated) when a valve or pressure is stale or missing.
  - **Static** (solid, not animated) when the path is open with no differential.
- **Fluid.** A junction's outgoing pipe carries the fluids flowing in. Both supplies flowing draws two colours.
- **Link.** Up when its board is fresh (data links animate). Down, with "not heard for N s", when stale.
- **Reading.**
  - Stale readings grey out with "· N s ago".
  - `warn_above`/`warn_below` add ▲. The tank PT warns at the 850 psi default relief.

## Fill topology: sources

- Supply rule: FirmwareSimulator `src/bay/native_plant.rs` `Supply::paths`. Nitrous or nitrogen reaches the tank only with the AB valve open, the QD connected and the bottle open.
- Bottles and regulator: `docs/firmware-sim.md`.
  - The regulator is set at 1000 psi; the N2O vapour pressure is about 745 psi at 20 °C. Both are drawn as assumed values.
  - The tank is 15.85 L (measured 967.45 in³). The vent passes vapour; the dump passes liquid from the bottom.
- Valve order: `VALVE_STATE` [pilot, normally-open (vent), dump, igniter, nitrogen, nitrous, retracted] (`telemetry_task.rs` `valve_state_values`).
- Pins:
  - AB `N2_SIG` PB13, `N20_SIG` PB14, `IGNITER_SIG` PB12.
  - VB pilot solenoid PB11.
  - VB vent and dump are servos.
- Relief: 850 psi relief and 950 psi abort, both defaults (`gse/overpressure.rs`).
- Not measured, so not drawn as live values:
  - supply line pressures;
  - fluid temperatures (the DAQ ADC die temperature is labelled as such);
  - umbilical continuity;
  - pyro continuity;
  - a magnetometer (the FC reports none);
  - the igniter current in amperes (unavailable: sensor supply mismatch, `docs/igniter-sense.md`).

## Audience programme

The Stream Manager's camera layout, and each rundown scene, offer:

- `diagram-mode`: the view of the station's current mode
- `diagram-flight`
- `diagram-test_fire`
- `diagram-ground_test`
- `diagram-network`

A saved scene of the old set (`diagram-fill`, `diagram-av_bay`,
`diagram-stand`) shows its successor.

While such a scene can be selected, each 250 ms programme snapshot adds
`telemetry.diagram` (`diagrams::program_inputs`). It contains:

- the latest rows of the referenced types, with their ages;
- board freshness;
- valve controls (`actuated`, `requested`);
- station statuses.

Snapshots are delayed like the video. `/api/media-assets/program/state` adds the
scene's `diagram_topology`. The programme page draws it with
`/assets/program-diagram.js`, read-only, at 10 Hz. With no diagram scene
configured, nothing extra is computed.

## Provenance

The badge beside the title reads one of:

- **Live**
- **SIMULATED FIRMWARE**
- **SIMULATED**
- **IGT: FC flight data SIMULATED (replay)**
- **IGT: live fill and stand**

The existing SIMULATED and IGT banners stay on every view. In IGT the ground
test view draws the rocket as a separate body; its simulated altitude comes
from `SIM_BAROMETER_DATA`.

## Load cells

- **KG50** (`LOADCELL_50_WEIGHT_KG`): in every mode view. In flight it sits under the motor and gives the fill mass; on the stand it carries the hanging vehicle.
- **KG1000** (`LOADCELL_WEIGHT_KG`, the 1000 kg thrust cell): test fire and the ground test only, in kg and as a force (× g, in N).
- **Chamber PT** (`COMBUSTION_CHAMBER_PRESSURE`, PX309-1KG5V): test fire and the ground test only. The flight view draws the chamber open to the atmosphere through the nozzle.

An ADC over-range channel shows `OVR` (or `−OVR`), never a number, in the
dashboard and the programme.
