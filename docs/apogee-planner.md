# Apogee planner, burn report and fire archive

The station turns a target apogee and the day's weather into the two numbers the
fill sequence already uses: the **N2O fill mass target** and the **supercharge
(Turbocharge) pressure target** (`config/fill_targets.json`, `nitrous.target_mass_kg`
and `nitrous.target_pressure_psi`). It also measures every burn it sees and keeps the
fires in an archive that the motor model is fitted from.

```
weather provider ──► snapshot (cached) ─┐
.ork import ──► vehicle ────────────────┼─► flight model (Rust port of tools/flight_profile)
fire archive ──► motor fit ─────────────┘         │
                                                  ▼
target apogee ─────────────────────────────► solver ─► plan (fill kg, supercharge psi, band, warnings)
                                                  │
                         operator "Apply to fill targets" (confirm, logged)
                                                  ▼
                      fill targets (same save path and validation as a manual edit)
                                                  ▼
                launch capture manifest (details.apogee_plan) ─► post-flight comparison
burn ─► burn report (provisional from telemetry, final with DAQ SD) ─► fire archive (review) ─► fit
```

Nothing here actuates anything. The planner only writes targets, and only when an
operator approves it.

## 1. Flight model engine

**Choice: a Rust port inside the backend** (`backend/src/apogee_planner/flight.rs`).

- The production image (`Dockerfile`, runtime stage `debian:stable-slim`) installs only
  `ca-certificates`. There is no Python and no numpy at run time, and adding them costs
  about 100 MB on the Pi 5 image plus a second runtime to keep patched. A subprocess
  would also need the `tools/` tree, which the image does not copy.
- `tools/flight_profile/flightsim.py` is pure Python (numpy is only used by
  `generate.py` for summaries), so the port is mechanical: USSA-76 atmosphere, WGS-84
  gravity, the OpenRocket component drag build-up, the rail, the weathercocking
  attitude approximation and the measured/scaled motor curve. The port flies boost and
  coast with the same 1 ms step and the same equations; descent drift uses a simpler
  terminal-velocity model under drogue and main (the Python descent adds swing and
  opening shocks, which do not move the landing point by more than a few metres).
- **Agreement is tested.** `tools/flight_profile/planner_reference.py` runs the Python
  model on a set of cases (measured motor, scaled impulses, hot and cold day, wind,
  heavy and light airframes) with gusts off, and writes
  `backend/src/apogee_planner/fixtures/reference_cases.json`. The Rust test flies the
  same cases and must agree on apogee within 0.5 % (rail exit speed, max Mach and time
  to apogee within 1 %). `tests/test_apogee_reference.py` checks that the fixture is
  current.
- A plan needs about 30-60 flights (bisection plus the uncertainty band). One flight to
  apogee is ~30 000 steps; the Rust model does that in a few ms on the workstation and
  well under a second for a whole plan on a Pi 5.

### OpenRocket

- **Import (runtime, no Java):** `POST /api/apogee_planner/ork` accepts an OpenRocket
  `.ork` file. Modern `.ork` files are ZIP archives holding `rocket.ork` XML; older ones
  are gzip'd or plain XML. All three are read (ZIP local headers with stored/deflate
  entries via `flate2`, which the backend already depends on transitively). From the
  XML the importer takes the nose cone (length, shape, radius), body tubes (length,
  radius, thickness), trapezoidal fin sets (count, root, tip, span, sweep, thickness),
  the motor mount inner tube, mass components and mass/CG overrides, and component
  materials (bulk density). It computes structural masses from geometry and density,
  the CG along the axis, a Barrowman CP (nose + fins) and so the static margin and
  CNα, and fills the vehicle parameters (`length`, `diameter`, `nose_length`, fins,
  `dry_mass` without propellant, `cn_alpha`, `static_margin`). The imported vehicle
  replaces the hand assumptions until it is cleared; the plan names its vehicle source.
- **Cross-check (dev machine only):** `tools/flight_profile/openrocket_crosscheck.py`
  runs OpenRocket itself through `orhelper` (JPype + the OpenRocket JAR) and prints
  OpenRocket's apogee next to the in-repo model's for the same `.ork` and motor. It is
  never needed on the station.
- **Trade-off.** Running OpenRocket on the Pi would need a JRE (~200 MB), JPype and
  a headless OpenRocket, minute-scale start-up, and it would still need our motor model
  exported as a RASP `.eng` file. Its drag model is the same component build-up the
  in-repo model already uses (OpenRocket technical documentation ch. 3.4). We keep its
  **data** (the `.ork` geometry and masses) and our **engine**.

## 2. Motor model as a function of fill

`backend/src/apogee_planner/motor.rs`, `n2o.rs`.

**Base curve.** The 2026-11-16 fire, re-extracted with the KG50 unloading
(`tools/flight_profile/extract_thrust.py`, section 7): 9.19 kN·s (7.6-10.1), 5.65 s
burn, 5.65 kg GS fill reading at ignition, tank 404 psi at valve open (self-pressurised
blowdown; N2O at about -3 °C by vapour pressure).

**Impulse.** `I = c_eff(P̄) · m_fill`, where `m_fill` is the GS fill reading at ignition
(the quantity the planner sets) and

    c_eff(P̄) = c_ref · (P̄ / P̄_ref)^n

- `P̄` is the mean tank pressure during the liquid phase:
  - self-pressurised: `P̄ = k_cool · P_vap(T_N2O)` with `k_cool = 0.85` (evaporative
    cooling during blowdown; on 11-16 the tank fell from 404 to ~330 psi over the
    liquid phase);
  - supercharged to `P_sc > P_vap`: the N2 partial pressure `P_sc − P_vap` expands
    polytropically (k = 1.2) from the initial ullage to the full tank as the liquid
    drains, so `P̄ = k_cool·P_vap + mean((P_sc − P_vap)·(V_u0/V)^k)`.
- `T_N2O` is the ambient temperature from the weather snapshot (the run tank sits in
  the sun/air for the hold; a manual tank-temperature override exists for when the
  tank is chilled or heated).
- `n` (pressure exponent) has a prior of 0.3 ± 0.2. Injector mass flow goes as
  √ΔP and chamber pressure (so c* and Cf) rises with flow; 0.3 is a middle estimate
  for small N2O hybrids. With one fire it cannot be fitted and is held at the prior.

**Thrust curve.** The measured shape scaled on both axes: the thrust axis by the mass
flow and efficiency ratio `a = (P̄/P̄_ref)^(0.5+n)`, the time axis by `b = (I/I_ref)/a`.
A bigger fill burns longer at the same pressure; a harder supercharge burns harder and
shorter. The propellant mass for the trajectory is `m_fill · (1 + 1/OF)` with OF 5 and
4 % residual, as in `vehicle.json`.

**Achievable fill.** Max fill is the liquid that leaves the minimum ullage (default
15 %) in the run tank (default 12 L, `vehicle.json`) at the warmest expected tank
temperature (forecast + 5 °C), plus the vapour: `m_max = ρ_l·V(1−u) + ρ_v·V·u`
(N2O saturated densities and vapour pressure from the ESDU 91022 correlations, as used
in the hybrid literature). 15 % at 20 °C gives 8.3 kg (8.0 kg liquid + 0.3 kg vapour), about the station's default 8 kg target.
The supercharge target must be at or above `P_vap(T)` (lower means no nitrogen is
added) and below the hardware limits in section 4.

**Calibration and fit.** Each accepted archive fire gives a point
`(m_fill, P̄, T, I ± σ_I)`. The fit is weighted least squares in log space:
`ln(I/m) = ln c_ref + n·ln(P̄/P̄_ref)`, with the prior on `n` as a pseudo-observation, so
one fire fits `c_ref` only and `n` starts to move once fires span different pressures.
`σ(c_ref)` shrinks as `1/√(Σw)`. Flights add drag information instead (section 5:
`cd_scale`). Every plan records the fit it used (fire ids, `c_ref`, `n`, σ) so the UI
can show how predictions changed between fits.

**Uncertainty.** The impulse σ for a plan is

    σ_I/I = √(σ_c² + (0.10·|ln(m/m_ref)|)² + (σ_n·ln(P̄/P̄_ref))²)

— the fit, plus extrapolation in fill mass and pressure away from the fires. With the
single 11-16 fire σ_c ≈ 10 % (its 7.6-10.1 kN·s range taken as ±1.4σ). The KG50
calibration is unverified (docs/loadcell-investigation-2026-11.md), so the fire's
"5.65 kg" is a GS reading, not a verified mass; the fit is in GS-reading kg and is only
valid while the fill cell calibration is unchanged. A recalibration should be followed
by re-reviewing the archive.

## 3. Weather plugin

`backend/src/apogee_planner/weather.rs`. A `WeatherSnapshot` holds surface temperature,
pressure, wind speed/direction and gusts, winds aloft as `(height AGL, speed, from)`
levels, temperatures aloft, the provider, its request (what was sent), the valid time
and the fetch time.

Providers:

| Provider | Network | Notes |
|---|---|---|
| `manual` (default) | none | Operator enters surface temperature, pressure (or "standard"), wind, gusts and an optional wind at 1 km/3 km. |
| `open_meteo` | HTTPS GET `api.open-meteo.com/v1/forecast` | Free, no key. Off until an administrator enables "online weather". |
| `cached` | none | The last fetched snapshot, with its age. Used automatically when a fetch fails. |
| station sensors | — | The station has no weather sensors today (only the Pi CPU temperature), so there is no provider. |

**Privacy.** The Open-Meteo request carries only the launch site rounded to 0.01°
(~1 km), the hourly variables and `forecast_days`. No key, no station name, no user,
a generic `User-Agent`. The settings page shows the exact URL before it is enabled and
every snapshot stores the URL it came from. The site comes from the station's launch
site setting (`LAUNCH_SITE`, falling back to the FC's compiled site).

**Parsing.** Hourly `temperature_2m`, `surface_pressure`, `wind_speed_10m`,
`wind_direction_10m`, `wind_gusts_10m`, and at 1000-500 hPa `temperature`,
`wind_speed`, `wind_direction`, `geopotential_height` (`wind_speed_unit=ms`,
`timezone=GMT`). The hour nearest the planned launch time is used; pressure levels below
the site are dropped and heights converted to AGL. Tests use a recorded response
(`fixtures/open_meteo_20261122.json`); there is no network in any test.

**Use in the flight model.** The atmosphere is the USSA-76 lapse profile shifted to
the observed surface temperature and pressure (temperature offset = observed − standard
at the site, sea-level pressure solved from the observed station pressure). Wind is the
surface wind with the 1/7 power law up to the first aloft level, then linear
interpolation between levels (direction by vector components). Gusts are applied as a
±σ case in the band, not as random noise, so a plan is deterministic. The surface
temperature drives `P_vap` and the max fill.

**Field use.** With no internet the station uses manual entry or the cached forecast;
the plan shows the weather's age and source and warns when a forecast is more than 6 h
old or the valid time is more than 3 h from the planned launch.

## 4. Solver

`backend/src/apogee_planner/solver.rs`.

**Policy: fill first, then pressure.** The supercharge target is held at the planner's
nominal (default: the current `nitrous.target_pressure_psi`, clamped to the limits) and
the fill mass is solved by bisection on apogee(m). Only if the target needs more than
the maximum fill does the solver raise the supercharge (bisection on pressure at max
fill) up to the pressure limit; only if the target is below the minimum-fill apogee
does it lower the supercharge toward no supercharge. Rationale: fill mass is the
quantity the station measures and cuts off precisely, pressure changes move the motor
furthest from the one fire we have (largest model uncertainty), and keeping the
nominal pressure keeps the burn the team has seen.

**Limits.**

- Max fill: ullage rule above, and the station's own limit (the fill target never above
  the max fill at the warm-case temperature).
- Min fill: planner setting, default 3.0 kg (the 2026-10-31 fire burned with 4 kg; below
  ~3 kg the liquid phase is too short for a stable burn on this injector).
- Max supercharge pressure: the lowest of the planner's MEOP setting (default 750 psi),
  `gse.pressure_ceiling_psi − gse.turbocharge_pressure_deadband_psi` when set (the
  Turbocharge controller refuses a target at or above the ceiling), and
  `gse.overpressure_relief_psi − 50 psi`. A warning is raised if the warm-case vapour
  pressure plus the nitrogen partial pressure exceeds MEOP.
- Fill target tolerance: the plan rounds the fill to 0.05 kg, inside the
  `gse.fill_tolerance_kg` band the cutoff lands in.

**Unreachable targets.** If the target needs more than max fill at max pressure the plan
is marked `unreachable_high` with the maximum achievable apogee (its band too) and
proposes max fill and max pressure only as information; Apply is disabled. Likewise
`unreachable_low` for targets below the minimum-fill apogee.

**Outputs.** Fill mass, supercharge pressure, predicted apogee (nominal and the ±1σ band
from impulse, wind and temperature uncertainty), rail exit speed (warning below
30.5 m/s, the IREC/SA Cup minimum), max Mach, time to apogee, burnout time, landing
drift distance and bearing, the fill needed at ±1σ impulse, the limits that applied
and every warning.

**Band.** One-at-a-time perturbations: impulse ±σ_I, wind ±σ_w (manual 2 m/s, forecast
1.5 m/s + 0.25 m/s per hour of lead time, plus the gust σ), temperature ±σ_T (manual
3 °C, forecast 1.5 °C + 0.2 °C/h). `σ_apogee = √Σ(Δ_i)²`.

## 5. Approval flow

- Settings → Mission → Apogee planner. An administrator sets the target apogee (ft or
  m AGL), the weather source, online weather opt-in, manual weather, tank and limit
  parameters, and imports a `.ork`. Anyone who may send commands can press **Compute**.
- The plan shows fill mass, supercharge pressure, the apogee band, warnings, the weather
  used and its age, the vehicle and motor-fit sources.
- **Apply to fill targets** opens a confirm dialog listing the old and new targets and
  the plan inputs. On confirm, `POST /api/apogee_planner/apply {plan_id}` writes the two
  targets through `crate::web::save_fill_targets` — the same function the manual
  `POST /api/fill_targets` uses (operator admission claim, permission, canonicalisation,
  atomic save, broadcast). It refuses (409) when:
  - the station mode is Test fire or Testing (no flight to plan; the planner still
    computes there so archived burns can be studied);
  - the flight state is not Idle or PreFill, or a GSE fill/turbocharge session is
    active (mid-fill or later);
  - the plan is unreachable, stale (older than 30 min) or computed from different
    settings than the current ones.
- Every apply is logged: a message row ("Apogee plan applied by X: ...") and a JSON line
  in `<data>/apogee_planner/applied.jsonl` with who, when, the full plan and its inputs.
- The last applied plan goes into the next launch capture's manifest
  (`details.apogee_plan`) and into the fire archive record of that burn.
- **After a flight** the planner shows predicted vs actual apogee (FC apogee if
  reported, else the GS flight maxima). "Add calibration point" solves the `cd_scale`
  that makes the model hit the actual apogee with the plan's weather and the burn's
  measured impulse (if a burn report exists, else the plan's), and stores it; the
  planner uses the mean `cd_scale` of the stored flight points.

## 6. Sim link

`GET /api/apogee_planner/sim_environment` returns the snapshot the plan used as

```json
{"environment": {"ambient_temp_c": 18.0, "surface_pressure_pa": 91000.0,
  "wind_mps": 4.0, "wind_from_deg": 300.0, "gust_sigma_mps": 1.0,
  "winds_aloft": [{"h_agl_m": 1000.0, "speed_mps": 8.0, "from_deg": 290.0}]},
 "fill": {"nitrous_target_mass_kg": 6.4, "nitrous_target_pressure_psi": 700.0},
 "source": "open_meteo", "valid_time_ms": 0, "fetched_ms": 0}
```

The firmware-backed sim (`FirmwareSimulator`, `scripts/gs_bay.py` control file) can
take the `environment` object under `control.json → "environment"` to set its ambient
temperature (N2O vapour pressure in its plant) and wind, so a planned fill can be
rehearsed. The sim's plant does not read it yet; see "Notes for the sim" below.

## 7. Thrust from the stand load cells

The rocket hangs from the 50 kg fill cell (KG50). Thrust first unloads KG50; once it
exceeds the hanging weight the rocket lifts, closes the gap and presses the 1000 kg
thrust cell (KG1000). With `R(t)` the KG50 reading, `R_pre` its value just before
ignition, `R_post` its settled value after the burn and `F(t)` the KG1000 reading in kg:

    thrust(t) = g · (F(t) + (R_pre − R(t)) − burned(t)),   burned(t) = (R_pre − R_post) · impulse fraction(t)

- This holds on both sides of lift-off (hanging: F = 0 and KG50 reads m(t) − T/g;
  lifted: KG50 at its zero-load value and KG1000 reads T/g − m(t)). Only KG50
  *differences* enter, so its tare convention does not matter, but its slope does.
- **Gap-closing transient:** between KG50 reaching its zero-load plateau and KG1000
  taking load neither cell carries the rocket; flagged and bridged (its net impulse is
  ~0, the momentum gained in the gap is returned at contact).
- **Drop-back:** at tail-off the rocket settles back onto KG50; the vapour tail is then
  visible only as KG50 returning to `R_post`. An impact spike, if present, is flagged
  and median-filtered.
- On 2026-11-16: R_pre 5.65, plateau −38.23, R_post −6.56 kg (recorded slope). The KG50
  term adds 2.09 kN·s: 9.19 kN·s total vs 7.11 from KG1000 alone. KG50 spikes during the
  burn match the supply-dip signature and are removed; the pre-burn value was taken
  during the 11 V sag. With the earlier 12 915 kg/V KG50 fit the total is 8.27 kN·s.
- The 2026-10-31 fire: KG50 sat on a flat −17.5 kg plateau (lifted) from 1 to 4.5 s while
  KG1000 never responded, so thrust above the hanging weight is unknown. It is archived
  as a lower bound only and excluded from the fit.

## 8. Burn report (automatic, after every burn)

`backend/src/burn_report/`. Runs in HITL, integrated ground test and test fire modes
(and Normal, for a flight burn the DAQ sees on the pad).

1. When a launch / ground-test / HITL capture closes, a **provisional** report is
   computed at once from the GS telemetry in the capture (`LOADCELL_WEIGHT_KG`,
   `LOADCELL_50_WEIGHT_KG`, tank PT, chamber PT, valve states).
2. The DAQ SD `/LAUNCH` file (opened at Launch acceptance, to T+20 s) is fetched as soon
   as the station is in a safe state (Idle, Landed, Recovery), in these modes too, and
   ahead of the low-priority lead-up fetch.
3. When it arrives the report is recomputed as **final** from the merged series: both
   sources aligned on network time, the DAQ SD rows preferred for the load cells (full
   rate, lossless), gaps filled from whichever source has data, provenance kept per
   sample.
4. Outputs: total impulse, burn time (5 % of peak), peak and average thrust, the curve,
   propellant consumed (fill reading at ignition minus after), effective Isp, chamber
   pressure stats if the chamber PT is valid, uncertainty, and quality flags (KG1000
   clamp/open lead, supply-dip signatures, missing samples, gap-closing transient,
   source coverage). Saved into the capture folder as `burn_report.json` and
   `burn_thrust.csv`, announced with a notification, and fed into the fire archive as a
   pending record.
5. HITL and simulated data are marked `simulated` and excluded from the fit by default.

**Implementation notes** (`backend/src/burn_report.rs`, `burn_report/thrust.rs`,
`burn_report/merge.rs`):

- The live provisional report is computed 40 s after a Launch/HITL capture starts (or
  when it ends, if sooner) from the station's recent telemetry cache; the saved
  provisional report is recomputed from the capture's `recording.db` when it is
  materialized; the final report when a DAQ launch file is attached
  (`recording_store::board_sd::offer_file` queues the folder). A final report is
  never replaced by a provisional one; an unchanged recompute raises no second
  notification. After a restart, captures with a DAQ file but no final report are
  recomputed.
- The ignition reference is the first VB `VALVE_STATE` main-valve open with load-cell
  data around it, else the first clear KG50 unloading (3 kg below its 3 s median) or
  KG1000 load, minus 0.15 s.
- DAQ SD rows used: `kg1000_network`, `kg50_network`, `chamber_pressure_raw_volts_psi`
  (preferred over telemetry) and `valve_pressure_psi` (the VB tank PT relayed; the
  station's own VB telemetry is preferred), network-time rows only, `value` else
  `calibrated_value`. Station rows use `source_timestamp_ms` when present. The
  preferred source covers a stretch when its neighbouring samples are within 250 ms;
  the other source fills the rest.
- KG1000 samples at a rail (outside −50..600 kg), within 60 ms of one, or more than
  max(40 kg, 35 %) from their 7-sample median are dropped. If the first trusted sample
  comes after 0.15 s, the start is bridged with a ramp (0.10-0.30 s) to that level and
  flagged. The KG50 floor is the mode of the readings below both the pre- and
  post-burn levels when it is held within ±1 kg for 0.5 s; the rocket counts as lifted
  only if KG1000 also took load (a flat floor without KG1000 load is flagged
  `kg1000_no_load`, a lower bound). KG50 samples more than 5 kg outside
  [floor, pre-burn] are dropped as supply-dip spikes.
- σ = √((0.10·I_KG1000,measured)² + (0.6·I_startup bridge)² + (0.25·I_KG50 term)²).
- On the committed 11-16 window the Rust extraction gives 9.11 kN·s (Python 9.19).
- The DAQ launch-file fetch is scheduled after every Launch, HITL and ground-test
  capture in every mode, polls the safe-state gate every second, and the lead-up fetch
  yields to it while it is scheduled or running.

## 9. Fire archive

`<data>/fire_archive/<YYYY-MM-DD>_<label>/` holds `fire.json` and `thrust.csv` per fire,
outside the rolling store and the event captures and never deleted automatically. Each
record holds the fill (KG50 before/after fill, at ignition), tank pressure and
temperature at ignition, the supercharge target, ambient temperature and weather (from
the plan's snapshot or manual entry), the thrust curve, burn time and impulse, chamber
pressure if valid, the result, the applied plan and its predictions, the data-quality
flags and the review state (`pending` / `accepted` / `rejected`, `exclude_from_fit`,
notes). The archive is seeded with 2026-11-16 (accepted) and 2026-10-31 (lower bound,
excluded). An operator reviews each new record before it enters the fit.

Record ids are `<YYYY-MM-DD>_<HHMMSS>_<kind>` (UTC of the ignition reference) for burn
reports and `<date>_<label>` for imports; a recomputed report updates the same record
(numbers and flags) and keeps its review. The seeds are compiled in from
`backend/config/fire_archive_seed/` and written only when missing, so a review of a
seed survives restarts. The 2026-10-31 seed is `rejected` and excluded (lower bound
only). The motor fit reads `fire_archive::fit_inputs(state)`: accepted, not excluded,
not simulated records with a fill and an impulse (σ defaults to 10 % when missing).
The supercharge recorded for a burn is the station's nitrous pressure target when the
report was saved; ambient temperature and weather come from the launch capture's
`details.apogee_plan` (`plan.weather`) when present.

## 10. API

| Route | Who | |
|---|---|---|
| `GET /api/apogee_planner` | view | settings, last plan, applied plan, weather cache, motor fit, apply gate, post-flight comparison |
| `PUT /api/apogee_planner/settings` | admin | planner settings |
| `POST /api/apogee_planner/weather/fetch` | send commands | fetch from the selected online provider (refused unless enabled) |
| `POST /api/apogee_planner/compute` | send commands | compute a plan from the current settings and weather |
| `POST /api/apogee_planner/apply` | send commands | `{plan_id}`; writes fill targets (gated, logged) |
| `POST /api/apogee_planner/ork` / `DELETE` | admin | import a `.ork` (raw body) / back to the default vehicle |
| `POST /api/apogee_planner/calibrate_flight` | send commands | `{actual_apogee_m?}` add a drag calibration point from the last flight |
| `GET /api/apogee_planner/sim_environment` | view | weather snapshot for the sim |
| `GET /api/burn_reports`, `GET /api/burn_reports/{id}` | view | burn reports (provisional/final) |
| `GET /api/fire_archive` | view | archived fires |
| `PATCH /api/fire_archive/{id}` | send commands | review: status, exclude_from_fit, notes, metadata |
| `POST /api/fire_archive/import` | admin | import a past fire record |

## Notes for the sim

The firmware sim's load-cell model should produce the stand behaviour of section 7 so
the burn report sees what the real stand produces: KG50 reads hanging mass (rocket +
propellant) minus thrust/g until thrust exceeds the weight, then its zero-load value
(plus the supply-dip spikes if those faults are modelled); KG1000 reads 0 until lift-off
and thrust/g − m(t) after, with a short gap-closing delay (tens of ms) and contact
spike; the rocket drops back onto KG50 when thrust falls below the weight. The plant
should also accept `control.json → "environment"` (section 6) for ambient temperature
(N2O vapour pressure) and wind.
