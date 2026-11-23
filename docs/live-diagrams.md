# Live diagrams

The dashboard's **Live diagrams** tab draws four P&ID-style views from live
telemetry. The audience programme can show the same views (delayed with the
video).

| Diagram | File | Shows |
| --- | --- | --- |
| Fill system | `backend/layout/diagrams/fill.json` | Bottles, regulator, AB solenoids, fill manifold, QD, run tank, VB vent/dump/main valves, chamber; tank PT, fill mass/level, chamber PT, igniter, fill battery |
| AV bay in flight | `av_bay.json` | FC, PB, RF on the AV CAN bus; radio to the GS and RRLS; IMU, barometer, GPS; estimator, flight phase, pyro channels, underglow/buzzer, FC SD |
| System overview | `network.json` | GS, RRLS, Pico-Fi, gateway, fill and AV CAN buses, every board, with live link health |
| Test stand | `stand.json` | Tank, main valve, chamber, KG50 and KG1000; in IGT a second body: the rocket on the ground with its replaying FC |

Client side: Seds-Ground-Station-Frontend `src/telemetry_dashboard/live_diagrams/`
(`docs/live-diagrams.md` there).

## Editing a topology

The files are declarative JSON. Edit a file when the plumbing or the bay
changes. The station serves the files from `GET /api/diagrams` (ViewData), and
the dashboard picks them up on the next page load. No rebuild is needed.

- `GS_DIAGRAMS_DIR` overrides the directory.
- The file name must equal the topology `id`.
- The station checks the structure (`diagrams::check`): schema, unique ids, and that every edge's nodes and valves exist. A failing file is reported in `errors` and not served.
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

- `diagram-fill`
- `diagram-av_bay`
- `diagram-network`
- `diagram-stand`

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

The existing SIMULATED and IGT banners stay on every view. In IGT the stand
diagram draws the rocket as a separate body; its simulated altitude comes from
`SIM_BAROMETER_DATA`.
