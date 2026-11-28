# Flight plans and loss-of-signal prediction

Two read-only computations on the apogee planner's models
(docs/apogee-planner.md):

- **Flight plans.** An operator runs a manual flight or burn simulation and
  keeps the result as a versioned document. The replay engine plays the
  document like a recording, and it can be overlaid on a real flight.
- **Loss-of-signal (LOS) prediction.** When the rocket goes quiet in flight,
  the station keeps flying its last known state forward. It shows where the
  rocket probably is and where it will land, so the recovery team can head
  the right way and get signal back.

Neither one commands anything or changes a setting, and both work in every
station mode. Their JSON shapes are in the UI's `docs/backend-contract.md`
(sections "Flight plans" and "Loss-of-signal prediction").

## Flight plans (`backend/src/flight_plan.rs`)

### Inputs

- `kind`:
  - `flight` gives the trajectory, motor curves and dispersion;
  - `burn` gives the motor curves only.
- The fill: either `fill_kg`, or `target_apogee_m`, in which case the
  planner's solver picks the fill and supercharge.
- `supercharge_psi`, `tank_temp_c` and `motor_model` (`physics` / `scaling`).
- `weather`: `planner` (the planner's source), `manual` (with fields), or
  `cached`. Manual weather works offline.
- The rail: `rail_length_m`, `tilt_deg` and `azimuth_deg`.
- `site`: overrides the station's launch site.
- `dispersion.samples` (0-500, default 50) and `dispersion.seed`.

The GET route returns the defaults, filled from the planner settings and
`vehicle.json`.

### Models

There is one set of physics: the planner's own.

- `planner::prepare_with` builds the same solver input a plan uses. That covers
  the vehicle, the imported `.ork` if any, the drag calibration, the motor fit,
  the physics or scaling motor, and the weather snapshot. A manual run only
  overrides the planner settings and site in a copy.
- The trajectory comes from `flight::fly_traced`. That is `flight::fly` (the
  Rust port of `tools/flight_profile/flightsim.py`) with a trace attached. The
  trace records a point every 0.05 s up to apogee and every 0.5 s in the
  descent, plus the events:
  ignition, liftoff, rail exit, burnout, apogee, drogue, main and landing.
- A test checks that the traced flight's numbers equal the untraced ones.
- The descent is the planner's terminal-velocity drogue/main model
  (`flight::descend`).
- Attitude (an ENU quaternion) is the ascent model's. It is null under the
  parachutes.
- Burn curves (physics motor) come from `hybrid::simulate` at 0.05 s:
  thrust, chamber and tank pressure, oxidizer and fuel mass flow, O/F and N2O
  mass. The thrust carries the learned-residual factor the flight flies.

### Dispersion

`samples` flights at 0.002 s, run on all cores and deterministic per seed. Each
draws, with the 1σ values below:

| Variable | 1σ |
|---|---|
| wind speed | the weather snapshot's σ / speed (capped at 100 %) |
| wind direction (the whole profile turned) | 20° manual / 15° forecast |
| impulse | the motor's impulse σ |
| Cd scale | 5 % |
| drogue and main descent rates | 8 % each |

The landing ellipse is the 95 % (χ² 2-dof) ellipse of the sample covariance.
The apogee mean, sd, p05 and p95 are reported with it, plus every sample's
landing point.

CPU cost, measured in release with `flight_plan_tests::cpu_cost` (run it with
`cargo test --release -p groundstation_backend cpu_cost -- --ignored --nocapture`):

| | Workstation (24 threads) | Pi 5 estimate (4 × A76, about 4× slower per core) |
|---|---|---|
| one dispersion flight (0.002 s step) | 1.9 ms per core | ≈ 8 ms per core |
| plan, no dispersion | 6 ms (scaling motor) / 10 ms (physics) | ≈ 25-40 ms |
| plan, 50 samples (default) | 22-23 ms | ≈ 0.15 s |
| plan, 200 samples | 54-55 ms | ≈ 0.5 s |
| plan, 500 samples (the maximum) | — | ≈ 1.1 s |
| LOS prediction (48 descents, one core) | 1.5 ms | ≈ 6 ms per second of prediction |

### Storage

`<data>/flight_plans/<id>.json`. The id is `fp-<created_ms>-<hash8>`. The
newest 50 plans are kept, and older ones are removed with a log line.

### Routes

| Route | Who |
|---|---|
| `GET /api/flight_plans` | view |
| `POST /api/flight_plans/run` | send commands (operator/admin, as the planner) |
| `GET /api/flight_plans/{id}`, `/csv`, `/json`, `/bundle` | view |
| `DELETE /api/flight_plans/{id}` | send commands |

### Exports

- **CSV:** one row per trajectory point:
  `t_s, phase, event, east_m, north_m, up_m, latitude_deg, longitude_deg, altitude_msl_m, v_*, q*`.
  The `event` column names the events since the previous row. A burn plan's CSV
  is its burn curves.
- **JSON:** the document itself.

### Replay

`/bundle` is a gsreplay v1 bundle:

- `manifest.predicted: true`, `manifest.source.flight_plan` and a label;
- `replay.json` (`"predicted": true`) holds FC `GPS_DATA`, `BAROMETER_DATA`
  (the plan's altitude with the model atmosphere's pressure and temperature)
  and `FLIGHT_STATE` rows (Armed, Ascent, Coast, Descent, Reefing, Landed), at
  10 Hz in ascent and 2 Hz in descent, with ignition at `created_ms`;
- the station's layout, boards, geometry, diagrams and map config;
- `derived/trajectory.json`, `derived/flight_plan.json`, and cached tiles
  around the track.

Recording bundles also carry `derived/flight_plans.json`, holding the five
newest plans.

## Loss-of-signal prediction (`backend/src/los_predict.rs`)

### Inputs

- Every received row from the rocket (`FC` / `RF`) refreshes the signal time.
- `GPS_DATA` rows keep the last 20 s of fixes. The velocity comes from the fix
  1-4 s before the newest fix from the same sender.
- The phase is the FC's detailed phase (`fc_phase`), else the station flight
  state: Launch/Ascent → boost, Coast/Apogee → coast, Descent → drogue,
  Reefing → main.

### Trigger

Every second the station checks whether it is in flight (Launch .. Reefing or
InFlight, before Landed) with no rocket packet for `timeout_s` (default 5 s).
If so, it predicts from the last fix. This works the same in Normal, HITL, IGT
and firmware-sim modes, because they all feed the same telemetry path.

### Model

- **Climbing** (vu > 1 m/s, not under a canopy): point-mass coast with the
  vehicle's Cd build-up (`flight::coast_to_apogee`) to apogee. Then the drogue
  delay, the drogue, and the main at its deploy altitude.
- **Coast / around apogee:** the drogue delay, then the descent.
- **Drogue / main:** the descent at the vehicle's rates (`vehicle.json`
  `recovery`), with density correction.
- **Wind:** the weather the station flies. That is the applied plan's, else
  the last plan's, else the cached forecast, else the manual entry, so it works
  offline. The profile is the power law up to 10 m, then the forecast levels.
- **Ground:** the launch site's altitude (no terrain model).
- **Uncertainty:** 48 Monte Carlo descents (1σ values below), giving the 95 %
  ellipses of the position now and of the landing. The position ellipse grows
  with the time since the fix.

| Variable | 1σ |
|---|---|
| wind speed and direction | as for plans |
| descent rates | 10 % |
| fix position | 5 m |
| velocity estimate | 3 m/s |

### Output

`GET /api/los_prediction` returns:

- the state;
- the last fix;
- the predicted position now (with its σ);
- the landing, its time and its ellipse;
- the bearing and distance from the station (the station GPS, else the launch
  site);
- a MapOverlay with kind `los_prediction`. It holds the predicted path
  coloured by phase, the last fix, the predicted position and landing markers,
  and the two areas, all labelled "Predicted: no signal".

The UI computes the bearing and distance from the viewer's own location. The
watch status gains a `rocket_predicted` target while predicting.

### Resolution

When a packet comes back, the state becomes `resolved`. At the first newer GPS
fix, the prediction for that fix's time is compared with the fix. The error is
logged to `<data>/los_prediction/comparisons.jsonl`, sent as a station
message, and kept in `history` (the last 10).

### Replay

`POST /api/los_prediction/simulate` runs the same prediction for a simulated
cut (the last fix, phase and time since the fix) on a recording.

### Validation

**Flight model.**

- **Rust against Python.** The trajectory is the planner's flight model, and
  its Rust port agrees with `flightsim.py` within 0.5 % (`reference_cases`).
- **The nominal_10k mock flight** (`flight_plan_tests::validation_against_the_mock_flight`).
  The flight is `tools/flight_profile` with the full Python descent (gusts and
  swing), the same motor, and 4 m/s wind from 300°.
  - Apogee: 3046 m against the mock's 3048 m (−0.07 %).
  - Landing: 30 m from the mock's landing point. The drift is 1023 m @ 89.0°
    against 1012 m @ 87.4°.
  - Landing time: 196 s against 194 s after ignition.
- **SWEET_V6's stored OpenRocket sims** (`tools/flight_profile/ork_sims.py`).
  OpenRocket's apogee comes first, ours second:

  | Simulation | Apogee (OR / ours) | Difference |
  |---|---|---|
  | TESTFIRE_NOTFULLOXTANK | 2729 / 2678 m | −2 % |
  | IREC_STANDARDREPORT | 2112 / 2079 m | −2 % |
  | Simulation 1 | 2799 / 2747 m | −2 % |
  | Simulation 2 | 2780 / 2722 m | −2 % |
  | Simulation 3 | 2902 / 2837 m | −2 % |
  | Simulation 4 | 2076 / 2054 m | −1 % |

  Peak speed and acceleration agree within 0.5 %. Ours is lower in apogee
  because of the +10 % friction and 0.02 Cd protuberance allowance
  (docs/apogee-planner.md).
- **A plan against the planner.** A plan for a given fill flies exactly the
  planner's own flight for that fill (`a_plan_flies_the_planners_models_and_stores`).

**LOS prediction.** `los_predict_tests::signal_cuts_on_the_mock_flight_predict_the_landing`
cuts the nominal_10k signal at points along the flight. It gives the error of
the predicted landing, the 95 % landing ellipse, and the error of the predicted
position 20 s after the cut:

| Cut (s after start) | Phase | Landing error | 95 % ellipse | +20 s position error | True landing inside |
|---|---|---|---|---|---|
| 25 | coast (climbing) | 58 m | 1311 × 717 m | 54 m | yes |
| 36 | coast (apogee) | 52 m | 1281 × 694 m | 64 m | yes |
| 45 | drogue | 26 m | 1219 × 657 m | 22 m | yes |
| 80 | drogue | 19 m | 954 × 511 m | 13 m | yes |
| 120 | drogue | 45 m | 655 × 346 m | 7 m | yes |
| 145 | main | 61 m | 435 × 224 m | 12 m | yes |
| 180 | main | 31 m | 164 × 82 m | 23 m | yes |

The ellipses come from the manual-weather wind σ (the planner's 2 m/s plus
gusts, capped at 50 % of the speed), so they are deliberately wide; a forecast
narrows them. The live test drives the state machine from the same flight: it
goes quiet for 20 s, then a fix returns and the logged error is checked.

There is no firmware-sim Normal flight recording with GPS motion on this
workstation (the sim captures found are pad, fill and test-fire), so the mock
flight is the dataset.
