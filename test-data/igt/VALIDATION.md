# FC host replay of the IGT datasets

Each set was replayed through FlightComputer26 origin/dev 659440e (the baro altitude fix, on top of
d286b5e) with `tools/igt_replay_host.py` in that repo. The tool runs the real `ground_test.c` CSV
validation and interpolation, `kalman.c` and `evaluation.c` on the host, on the board's 10 ms baro
schedule. The deployments go to a dry-run stub. The datasets are the ones regenerated in f77ec79 (KG50
unloading in the thrust curve: 9.19 kN s measured, 12.04 kN s for the 10k ft sets). All five sets pass
validation (`IGT CSV ok`), reach Landed, and end the replay with no fault. Times are seconds after
Launch acceptance; "truth" is the generator's model (re-run from `tools/flight_profile` with the same
seeds; it matches each `summary.json`). The replay feeds the FC the `alt_m` column, which the generator
writes with the FC's corrected altitude (`baro_agl()`: the ISA pressure altitude of the sample minus
that of the pad). The "before the fix" rows replay copies whose `alt_m` was recomputed from
`pressure_pa` with the old sea-level form against the pad pressure.

| | nominal_10k | measured_motor | high_wind | early_apogee_noise | off_nominal |
|---|---|---|---|---|---|
| truth liftoff | 10.137 | 10.121 | 10.137 | 10.137 | 10.137 |
| FC Launch / Ascent | 10.53 / 10.66 | 10.54 / 10.69 | 10.52 / 10.66 | 10.53 / 10.66 | 10.52 / 10.65 |
| truth burnout (<5% peak) | 16.637 | 15.8 | 16.637 | 16.637 | 16.637 |
| FC Coast | 15.01 | 14.68 | 14.99 | 14.03 | 14.92 |
| truth max speed (m/s) | 235.8 | 187.9 | 235.1 | 235.9 | 236.0 |
| FC estimated max speed (m/s) | 237.3 | 186.45 | 236.99 | 248.57 | 237.76 |
| truth apogee (s, m) | 35.748, 3048.3 | 32.079, 2137.1 | 35.701, 3029.8 | 35.745, 3043.3 | 35.751, 3042.5 |
| FC Apogee state (s) | 35.4 | 31.72 | 35.36 | 35.35 | 35.44 |
| FC estimated apogee (s, m) | 35.88, 3046.52 | 32.11, 2136.12 | 35.7, 3027.59 | 36.3, 3057.23 | 35.8, 3041.5 |
| estimated - truth apogee (m) | -1.8 (-0.06%) | -0.9 (-0.04%) | -2.2 (-0.07%) | +14.0 (+0.46%) | -1.0 (-0.03%) |
| before the fix (sea-level formula) | +61.2 (+2.0%) | +43.2 (+2.0%) | +60.2 (+2.0%) | +77.0 (+2.5%) | +62.0 (+2.0%) |
| FC drogue (s, est m) | 37.0, 3035.38 | 33.33, 2125.6 | 36.95, 3017.02 | 36.93, 3035.09 | 37.0, 3030.78 |
| scripted drogue in the data (s) | 36.948 | 33.279 | 36.901 | 36.945 | 36.951 |
| truth crosses the main altitude (s; 457.2 m, off_nominal 300 m) | 138.946 | 101.316 | 137.737 | 140.024 | 146.213 |
| FC main (s, est m) | 139.07, 456.64 | 101.44, 456.48 | 137.86, 456.56 | 140.19, 456.48 | 139.82, 456.58 |
| true height at the FC main (m) | 454.2 | 454.2 | 454.2 | 453.3 | 454.1 |
| before the fix: true height at the FC main (m) | 448.2 | 448.2 | 448.0 | 449.0 | 444.9 |
| main opens in the data (s, m) | 139.246, 450.0 | 101.616, 450.0 | 138.038, 449.8 | 140.324, 450.1 | 146.514, 292.8 |
| truth touchdown (s) | 211.935 | 172.239 | 211.241 | 212.427 | 193.136 |
| FC Landed (s) | 214.18 | 174.54 | 213.71 | 214.81 | 195.51 |
| FC Recovery (s) | 219.18 | 179.54 | 218.71 | 219.81 | 200.51 |
| estimator deviations (kf_deviations) | 0 | 5 | 0 | 12 | 0 |

Findings:

- The estimator tracks the replay. On a noise-free copy of `nominal_10k` (`generate.py --variant
  nominal_10k --no-noise`), the estimated apogee is 3046.2 m, against a replayed baro peak of
  3046.15 m. The IMU, baro and attitude in the data are therefore mutually consistent.
- The FC's altitude matches the truth to about 0.1% at the 890 m site. The old sea-level form
  `44330 * (1 - (p/p0)^0.1903)` against the pad pressure overstates heights by about
  T(0)/T(890 m) - 1 = 2%: apogee would read 43-62 m high and the main would be commanded at about
  448 m true (445-449 m). The remaining -1 to -2 m at 10k ft is geopotential versus geometric height
  (the ISA altitude is geopotential; (3938^2 - 890^2) / 6371 km = 2.3 m), not a scale error.
- The FC commands the main at an estimated 456.5-456.6 m, 0.12-0.17 s after the true 457.2 m
  crossing, at 453-454 m true: about 3-4 m of baro lag (port lag and IIR) at the 25 m/s drogue rate.
- The FC reaches Apogee 0.35-0.4 s before the true apogee (its |v| < 4 m/s window). The drogue
  decision comes 1.19-1.25 s after the true apogee, within 0.05 s of the 1.2 s the data assumes.
- Coast is declared on the velocity trend after `MOTOR_BURN_TIME` (3.5 s), before the hybrid's
  tail-off ends: 1.6 s before the 5%-of-peak burnout on the 10k sets and 1.1 s before it on
  `measured_motor`, near the time of maximum speed in both. In `early_apogee_noise`, baro noise and
  the static-port error move it to 14.03 s, 2.6 s before burnout, after three failed trend checks.
  No deployment depends on it.
- `early_apogee_noise` is the only set with an apogee error above a few metres: +14 m, with the
  estimator's peak 0.55 s after the true apogee. The 3x baro noise window spans apogee (from 5 s
  before), and the filtered baro peaks at 3059 m at 35.46 s. The Apogee state and the drogue are on
  time (35.35 s and 36.93 s, 1.19 s after the true apogee), so there is no premature drogue.
- In `off_nominal`, a 24 g / 2000 dps (saturated) single-row IMU glitch during coast, a 2 s baro freeze and a 15 s GPS
  dropout and a 60 m GPS jump leave every decision unchanged. The late physical main (300 m) only delays Landed.
- Landed comes 2.2-2.4 s after touchdown (`MIN_SAMP_LANDED` and the velocity window). The 30 s of
  rest data is ample. The kf_deviations count failed spurious-transition checks: the velocity window
  for Landed during tip-over (`measured_motor`, `early_apogee_noise`) and the velocity trend during the
  noisy boost (`early_apogee_noise`). All are far below `KF_FLUC_THRES` (80).
