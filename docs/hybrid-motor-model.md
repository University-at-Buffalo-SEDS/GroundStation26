# Hybrid motor model

A physics model of the SWEET_V6 hybrid: N2O from the run tank, through the valve and injector, burning a
paraffin-filled ABS gyroid grain, out through a graphite nozzle. Three places use it:

- the Python flight tools (`tools/flight_profile/hybrid_motor.py`, the reference implementation);
- the station's apogee planner (`backend/src/apogee_planner/hybrid.rs`, a port);
- the firmware simulator's plant (FirmwareSimulator `src/bay/hybrid_motor.rs`, a byte-identical copy of the
  port).

All three load one spec, `tools/flight_profile/motors/hybrid_motor.json`, and two compact tables from the team's
design bundle. They prove they agree against shared test vectors, `motors/hybrid_vectors.json`:

| check | tolerance |
|---|---|
| pure functions | 1e-9 relative |
| whole burns at 10 ms | 1e-6 relative |

To change the physics: edit the Python, run `hybrid_calibrate.py`, then `hybrid_vectors.py`, copy
`motors/{hybrid_motor,sweet_v6_cea,sweet_v6_grain,hybrid_vectors}.json` to `backend/src/apogee_planner/model/`,
port the change to `hybrid.rs`, and copy `hybrid.rs` and the model files to FirmwareSimulator `src/bay/`. Tests in
both repositories fail until the copies match.

## 1. Sources of the inputs

| input | source |
|---|---|
| tank 15.854 L | measured (Rylan 2026-11-23, 967.45 in³); the preset's 146.05 mm × 917.6 mm + 0.469 L closures gives 15.84 L |
| tank bore 146.05 mm | design preset |
| valve Cd 0.7, effective area 5.07e-4 m² | design preset |
| injector 127 × 1.016 mm holes, Cd 0.63 (prior) | design preset |
| throat 41.91 mm, exit 75.95 mm (ε 3.284), 13° half angle, graphite | design preset |
| c* efficiency 0.8 (prior) | design preset |
| regression: paraffin a 3.04e-5, n 0.681; ABS a 1.1787e-5, n 0.799 (SI, r = a G^n) | design preset |
| cylindrical grain: L 330.2 mm, port 50.8 mm, OD 84.99 mm, 0.919 kg paraffin + 0.188 kg ABS | design preset; burned on 2026-11-16 |
| star grain: 10 points, tip r 34.29 mm, root r 12.70 mm, L 203.2 mm, OD 84.99 mm | `motor_design/star_grain.stl`; the flight grain |
| thermochemistry | the preset's NASA CEA surface, sliced (section 3.3) |
| ABS share vs radius | the preset's voxelised gyroid print (section 3.2) |

The design preset is `tools/flight_profile/motor_design/current_single_port_baseline.preset.yaml`. Its full
`.myrap` bundle stays out of git; its sha256 is recorded in `motor_design/README.md`.

Everything marked *assumed* in the spec is listed in section 10.

## 2. Oxidizer feed

**Tank.** The tank holds equilibrium two-phase N2O with an optional nitrogen pad.

- Saturation properties come from ESDU 91022: vapour pressure, liquid and vapour density, enthalpies and
  liquid cp.
- These match NIST to 1 % at -20, 0 and 20 °C (tests in both languages).
- One lumped temperature covers the liquid, the vapour and `wall_coupling` × the Al wall heat capacity.
- Draining liquid evaporates to refill the ullage, and the latent heat cools the tank. This is the standard
  equilibrium blowdown (Whitmore & Chandler, JPP 26(1) 2010; the Aspire Space run-tank model).
- Nitrogen is an ideal gas in the ullage and is not dissolved.
- After the liquid is gone, the N2O vapour and the N2 expand adiabatically: m c_v dT = R T dm, with the wall
  share added to the heat capacity.

**Ambient temperature** sets the tank temperature, and so the vapour pressure and the whole motor. The planner
uses the weather snapshot's surface temperature unless the operator enters a tank temperature.

**Feed loss.** The run valve is an orifice in series, Cd_v × A_v from the preset. The liquid upstream of the
injector is at p_tank - (mdot / Cd_v A_v)² / 2ρ.

**Injector.** The liquid phase uses the Dyer et al. two-phase model (AIAA 2007-5702):

    mdot = Cd A [k/(1+k) √(2ρ_l Δp) + 1/(1+k) G_HEM],  k = √((p1 - p2)/(p_v - p2))

- G_HEM is the homogeneous-equilibrium flux of an isentropic flash, choked at its maximum over the downstream
  pressure.
- The saturated-liquid entropy is integrated along the saturation line, ds = (dh_l - dp_v/ρ_l)/T, on a 0.25 K
  grid.
- After the liquid runs out, the flow is compressible (choked or subsonic) at the tank's gas density, times
  `vapour_cd_scale`.
- Dry vapour underpredicts the measured tail, because the tank still expels a two-phase foam. This is the
  equilibrium model's known weakness (Zimmerman, Waxman, Cantwell & Zilliac, AIAA 2013-4045), so the factor is
  calibrated.

## 3. Combustion chamber

### 3.1 Regression and O/F

The flux is G_ox = mdot_ox / A_port. The paraffin and ABS laws come from the preset.

The front recedes at the area-weighted rate of the two materials:

    ṙ = (1 - f) a_p G^n_p + f a_abs G^n_abs

and consumes both materials:

    mdot_f = [ρ_p (1 - f) + ρ_abs f] P L ṙ

Here f is the ABS share of the surface at the current web. The ABS mass fraction of the fuel enters the CEA
lookup. O/F shifts as the port opens, and as the ABS share changes from the printed inner wall (about 50 % ABS),
through the gyroid (about 8 %), to the outer wall.

`regression_scale` multiplies both laws. It is fitted only when a fire records the fuel it burned (weigh the grain
before and after). Otherwise O/F trades against c* efficiency and one thrust curve cannot separate them.

### 3.2 Port geometry (circular or star)

The port at web w is every point within w of the initial port: the Minkowski sum with a disk, clipped by the
grain OD. The region stays star-shaped about the axis, so its boundary is r(θ, w).

| quantity | how |
|---|---|
| port area A(w) | ½ ∫ min(r, R)² dθ, on 240 angles per half point and 160 web steps |
| burning perimeter | dA/dw; exact for an offset front, and the part on the OD drops out |
| ABS share | area-weighted over each burned shell |

- The circle (N = 0) is the special case. The 2026-11-16 fire is calibrated on it.
- Star tips round off as they burn and burn out into a near-circular port. The last fuel is the sliver between
  the outline and the OD.
- The offset front closes a sharp valley faster than its radial depth: the STL star's last fuel burns at a web of
  14.5 mm (the farthest point of the OD from the outline), not the 29.8 mm radial valley depth.
- `Grain.wall_fraction(w)` is the share of the OD (the chamber liner) already uncovered.
- The summary reports `tip_burnthrough_s` (when the tips reach the OD), `liner_exposed_s` (how long the burn
  continues after that) and `sliver_kg`.

**Star from the STL.** `hybrid_stl.py` slices `motor_design/star_grain.stl` and fits N, the tip radius and the root
radius. The straight-sided star reproduces the slice exactly: area 1345.7 mm², perimeter 451.1 mm. The tips reach
the OD after 8.2 mm of web, the valleys after 29.8 mm.

**Geometry tests.**

- The circle is checked against analytic values.
- The star at w = 0 is checked against N R_t R_r sin(π/N) and 2N × side.
- At w = 4, 12 and 20 mm the star is checked against a 900² raster (Rust) or Monte Carlo (Python) area of the
  offset region inside the OD.
- The perimeter is checked against the raster's dA/dw.

**ABS share vs radius.** `hybrid_preset.py` averages the voxelised print
(`literal_material_field.npz`, 0.35 mm voxels) over height and angle in 0.7 mm radial shells. It then scales the
result so the grain holds the preset's 0.188 kg of ABS (the field reports a 9 % voxelisation error).

The star is the same material (Rylan 2026-11-23): the same printed ABS gyroid infill filled with paraffin; only the
port differs. The print's walls follow its boundaries, so the profile is remapped by distance. A point reads the
preset profile from its nearer wall, using its distance from the initial port outline (the inner wall, now star
shaped) or from the OD (the outer wall). For the circular grain this is exactly the radial profile. The axial
pattern repeats, so the 203.2 mm grain takes the same profile.

### 3.3 Chamber pressure and the CEA table

The chamber pressure is a quasi-steady mass balance:

    Pc = (mdot_ox + mdot_f + mdot_N2) η_c* c*(O/F, Pc, h_ox, w_ABS) / A_t

- It is solved each step together with the injector flow, which depends on Pc (Illinois regula falsi).
- Nitrogen dilutes c* by its mass share.

`motors/sweet_v6_cea.json` (808 KB) is the preset's NASA CEA surface (`cea_surface.npz`, sha256 3468ce63…; FCEA2,
paraffin surrogate C25H52, ABS C3.85H4.85N0.43). It is sliced to the nozzle's ε = 3.284, interpolated between
3.25 and 3.5.

| axis | points kept |
|---|---|
| chamber pressure | all 20 (1.4 to 62 bar) |
| O/F | all 12 (3 to 50) |
| N2O enthalpy | 10 liquid (about -14 to +30 °C) + 6 saturated-vapour |
| ABS fraction | all 7 |

Each point stores c*, chamber γ, vacuum C_F and pe/pc. Lookups are multilinear in ln Pc, O/F, h and w_ABS.

**N2O enthalpy.** The tank state maps onto the surface's enthalpy basis as the CEA gas ΔHf of 81.6 kJ/mol plus the
real-fluid enthalpy relative to the ideal gas at 298 K. That uses ESDU plus the NIST Shomate ideal-gas cp.
Liquid at -14 °C and +30 °C lands within 60 cal/mol of the surface's own end points.

**Validation against the full surface.** At the 20 dropped enthalpy states, the compact table's p99 error is below
1e-4 for every field.

The full surface itself has **93 isolated C_F/pe cells that flip between two values** (about 1.38 and 1.30 at this
ε). They are listed in the file under `full_surface_glitches`. A 3-point median along enthalpy repairs them before
slicing; c* and γ have no such cells. Tell whoever generates the surface about them.

**Independent cross-check.** `hybrid_cstar.py` recomputes paraffin/N2O c* with Cantera, using the same NASA Glenn
data CEA uses: HP equilibrium plus a shifting-equilibrium throat, with C32H66 at -938 kJ/mol (Karabeyoglu et al.).
It agrees with the CEA surface to 2 % over O/F 5 to 10 (a test checks this). Peak c* is about 1600 m/s near
O/F 7.

### 3.4 Nozzle

- **Flowing full:** C_F = η_n (C_F,vac - (pe/pc) ε) + (pe/pc - pa/pc) ε, from the CEA values. Thrust therefore
  depends on altitude.
- **Separated** (pe < 0.4 pa, Summerfield): the ideal C_F for the table's γ, with the nozzle cut where the wall
  pressure reaches 0.4 pa.
- η_n = (1 + cos 13°)/2 = 0.987, the divergence factor.
- Throat erosion is linear and off by default (the preset says none).

In flight, the motor's thrust curve is computed at the pad's ambient pressure, and the flight model adds
(p_pad - p) A_e (`Motor.thrust_at`).

## 4. Outputs

`simulate()` and `Burn` give, every step:

- thrust
- Pc
- tank pressure and temperature
- N2O and N2 mass
- ox and fuel flows
- O/F
- ABS share
- web
- port area
- liner exposure
- propellant left
- propellant CG from the nozzle exit (liquid settled aft in the tank, gas above it, fuel at the grain)

The summary has impulse, peak, burn time (≥ 5 % of peak), liquid-phase end, ox and fuel used, mean O/F, Isp, tip
burn-through, liner exposure and sliver.

The flight model takes (t, F, propellant left): the propellant curve replaces the impulse-fraction mass model.

## 5. Calibration

`hybrid_calibrate.py` does the following:

1. **Stage 2, used:** fits the accepted fires of the fire archive. Today that is the shipped 2026-11-16 seed, with
   its thrust curve and now its ox tank trace (`tank.csv`).
2. **Fit set:** only the uncertain parameters: `injector_cd`, `eta_cstar`, `vapour_cd_scale` and `wall_coupling`.
   The preset geometry stays fixed.
3. **Residuals:**
   - thrust every 0.1 s from 0.65 s, with σ = 150 N + 5 % (the KG1000 startup gap before 0.65 s is an assumed
     ramp);
   - tank pressure every 0.1 s, with σ = 10 psi;
   - total impulse, with the fire's σ (900 N·s);
   - log-normal priors;
   - a soft wall on η_c* above 0.95.
4. **Covariance:** Gauss-Newton, inflated by the reduced χ² when it is above 1. It is optimistic because the curve
   points are correlated, so the planner uses the fires' impulse σ (≥ 5 %), not the parameter σ.

The station runs the same stage 2 on its own archive (`hybrid_fit.rs`), so every newly accepted fire refines the
model there. With only the seed it reproduces the Python values to within 5 % (a test checks this).

**Result** (`calibrated` in the spec):

| parameter | prior | fitted |
|---|---|---|
| injector Cd | 0.63 | 0.476 |
| c* efficiency | 0.80 | 0.903 |
| vapour-tail Cd scale | 1.0 | 1.81 |
| wall coupling | 0.3 | 0.56 |

| 2026-11-16 | measured | model |
|---|---|---|
| impulse | 9.19 kN·s (±0.9) | 8.65 kN·s (-5.9 %) |
| burn time (5 % of peak) | 5.65 s | 5.00 s |
| peak thrust | 3.3 kN (startup, partly interpolated) | 2.78 kN |
| thrust RMS (0.65 s to end) | | 284 N |
| tank pressure RMS | | 5.7 psi |
| liquid phase ends (tank knee) | about 2.9-3.0 s | 3.19 s |
| reduced χ² | | 0.94 |

The model has peak Pc 249 psia, mean O/F 11.5 and Isp 145 s on 5.6 kg N2O + 0.49 kg fuel.

**With the preset's own values** (Cd 0.63, η 0.8), before fitting, the model predicts 7.41 kN·s, 3.08 kN peak, a
4.05 s burn and a 2.66 s liquid phase.

**Was the 11-16 fire this configuration?** Yes, most likely, with a reduced fill:

- A fit that also frees the throat gives 38.1 mm against the preset's 41.9 mm.
- A fit that frees the fill reading-to-mass factor gives 0.97 against 1.
- Both are within the fit's reach of the preset geometry, and neither improves the fit much (χ² 0.99 and 0.89).
- The fitted injector Cd (0.48 against 0.63) is the clearest difference. It also absorbs any feed losses the
  model lacks, such as the QD, the run line and the valve's real opening.

**The 2026-10-31 chamber PT (cross-check, stage 1).** This is the only fire with Pc: about 30 psig on a
341 psig tank.

- Its Pc implies an equivalent throat of 69 mm, far larger than the 11-16 fit and the preset's 41.9 mm.
- That fire ran a different injector/plumbing setup with about 0.7 kg/s of ox, and it is rejected in the archive.
- Either its nozzle differed, or the chamber PT read about 2.7× low (scale and divider: compare
  `daq_chamber_pressure.h`).
- Re-check the PT on the next fire with it connected; the model then fits Pc directly.

## 6. Learned residual (grey box)

The physics stays primary. Reasons: one usable fire so far, extrapolation to other fills and temperatures, and a
planner that is already fast.

The design allows a learned correction later (`hybrid_fit.rs`, `ResidualModel`):

- **Target:** y = ln(I_measured / I_physics) per accepted fire.
- **Features:** fill reading, supercharge, tank temperature and initial port area.
- **Default model:** a Gaussian process (squared-exponential, a length scale per feature; defaults 2 kg, 250 psi,
  10 °C and 8 cm², signal σ 0.08, noise floor 0.02 plus each fire's own σ).
- **When it is on:** only from 3 accepted fires. Its mean multiplies the planner's thrust curve, and its variance
  adds to the impulse σ, which widens the apogee band.
- **Training:** `tools/flight_profile/hybrid_residual.py` fits the hyperparameters by marginal likelihood (CPU,
  seconds, runs on jupiter). It writes `residual_model.json`; copy that to the station's
  `<data>/apogee_planner/residual_model.json`. The station refits the GP on its own archive with those values.
- **Export path for a network:** a small network trained on the 5080 would export as plain JSON weights with an
  ensemble or MC-dropout variance behind the same trait. That means no ONNX runtime on the Pi.
- **Public data:** no public, documented paraffin/N2O firing set with full geometry was found. Published
  regression laws (e.g. Stanford's paraffin/N2O 0.155 G^0.5 mm/s) belong only as priors on the regression law in
  the spec, never as residual training rows: they are geometry- and injector-specific.

## 7. Integration

**Python flight tools.**

- `flightsim.Motor.hybrid(fill, tank_c, supercharge, pad_pa)` builds the physics motor (altitude-corrected
  thrust, propellant mass from the burn).
- `vehicle.json` `motor.model` is `measured` (the datasets keep it) or `hybrid`, with a `motor.hybrid` block.
- `generate.py --motor hybrid --fill 8` flies the physics motor.

**Apogee planner.**

- The setting `motor_model` is `physics` (default) or `scaling` (the fitted impulse-per-kg curve scaling, kept as
  the fallback).
- The physics motor flies the spec's flight grain (the STL star) with the station's fit.
- A plan reports `motor_model`, `physics_fit` (values, σ, fires, fit quality), `flight_grain` and
  `residual_active`.
- The solver searches fill first, then supercharge, as before. Every trial burn runs the hybrid model at the
  tank temperature and supercharge.

**Burn report.**

- Each report keeps the ox tank trace (`tank_curve`) and the chamber trace (`chamber_curve`).
- It also keeps `predicted`: the calibrated model's thrust, tank and chamber curves for the report's fill and
  tank pressure, for an overlay of predicted against measured.
- The fire archive stores `tank.csv` from the report, which the next fit uses. Its record gains `grain` and
  `fuel_used_kg`, editable through PATCH.
- `GET /api/fire_archive/{id}/thrust` also returns the tank trace.

**Firmware simulator.** The plant builds a `hybrid_motor::Motor` from its tank at ignition and steps it as the
tank's authority during the burn. Thrust drives the stand's load cells, Pc the DAQ chamber PT, and p_tank the VB
tank PT. The old Isp model stays as `motor.model = "isp"`.

**HITL ground plant** (`flight_sim/ground_plant.rs`).

- The tank is the measured 15.85 L, with a maximum of 11 kg of liquid. 12 kg would break the 95 % rule.
- With the pilot open, liquid leaves at the hybrid model's flow for the plant's tank pressure
  (`hybrid::ox_flow_at`).

## 8. Tests

**Python:** `tests/test_hybrid_motor.py`.

- N2O properties against NIST
- the enthalpy map
- the injector
- c*/CEA, plus the Cantera cross-check
- the compact-table validation
- the nozzle C_F
- regression
- grain geometry against the STL and a raster
- the 11-16 regression check (impulse within 8 %, burn time within 15 %, thrust RMS under 320 N)
- the temperature effect
- liner exposure
- vectors current and backend copies identical
- `flightsim` flying the physics motor

**Rust:** `apogee_planner::hybrid` tests.

- the shared vectors (properties, injector, CEA, nozzle, grains, five full burns)
- NIST points
- the 11-16 check
- the circle and star geometry
- altitude and temperature effects

**Also:**

- `hybrid_fit` checks that the station fit matches the Python fit, and that the residual turns on and learns.
- The planner adds a physics 10 kft plan test.
- `ground_plant` checks the tank and the pilot flow.
- In FirmwareSimulator, the model's own tests run unchanged, a copy-identity test checks the files against GS,
  and plant burn tests drive the PTs and load cells.

## 9. Results

At the 8 kg design fill (pad at 890 m; `python3 tools/flight_profile/hybrid_motor.py --fill 8 --tank-c 15
--grain <name>` and `generate.py --motor hybrid --fill 8` reproduce these):

| grain | tank | impulse | peak | burn | peak Pc | O/F (mean, range) | tip burn-through / liner exposed | sliver | SWEET_V6 apogee |
|---|---|---|---|---|---|---|---|---|---|
| cylinder 330 mm | 15 °C | 12.88 kN·s | 3.79 kN | 5.59 s | 324 psia | 12.6 (6.7-13.4) | no | 0.48 kg | 2652 m |
| cylinder 330 mm | 20 °C | 12.89 kN·s | 4.05 kN | 5.40 s | 343 psia | 12.8 | no | 0.48 kg | 2676 m |
| star 203 mm | 15 °C | 13.32 kN·s | 4.32 kN | 5.35 s | 363 psia | 11.0 (5.4-15.4) | 3.58 s / 1.88 s | 0.09 kg | 2830 m |
| star 203 mm | 20 °C | 13.35 kN·s | 4.63 kN | 5.16 s | 387 psia | 11.1 | 3.53 s / 1.74 s | 0.09 kg | 2864 m |

The star burns more fuel (0.72 against 0.63 kg, mean O/F 11 against 12.7) from a shorter, lighter grain (0.81
against 1.11 kg). Its tips reach the liner at about 3.6 s, about the end of the liquid phase. For the last 1.7 to
1.9 s of the burn, up to 44 % of the liner sees the flame: a thermal point to check on the chamber liner.

## 10. What Rylan should supply

**Measured on the hardware** (each replaces an assumption):

- **Weigh the grain before and after each fire.** This turns on the regression-law fit and is the single best new
  datum.
- **The star's own print file (.3mf/G-code).** The remapped preset profile is close, but the star's walls and infill
  density can then be voxelised directly, as the preset's were.
- **Throat diameter before and after a fire.** The preset says 41.91 mm; erosion is not modelled.
- **Chamber PT check.** Is the 2026-10-31 reading about 2.7× low? Connect the PT on the next fire.
- **Tank wall mass and the run-line/QD layout.** These give the injector-path losses that the fitted Cd 0.48
  absorbs today.
- **Ignition delay and the igniter puck mass** (the preset leaves the puck null).
- **Ambient temperature and supercharge at every fire.** The burn report takes them from the applied plan.

**Assumed in the spec:**

| item | assumed value |
|---|---|
| tank wall share | 6.6 kg |
| tank and grain stations (CG output) | 0.95 m and 0.27 m from the nozzle exit |
| static-fire ambient | 99 kPa |
