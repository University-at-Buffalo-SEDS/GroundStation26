# FC host replay of the IGT datasets

Each set was replayed through FlightComputer26 `gs-fc-bugfix-20261123` (origin/dev 0ad1ecd plus
the SD-logging-through-Recovery change) with `tools/igt_replay_host.py` in that repo. The tool runs
the real `ground_test.c` CSV validation and interpolation, `kalman.c` and `evaluation.c` on the host,
on the board's 40 ms baro schedule (`Baro_ODR_25` in `recovery.c`). Earlier versions of this page
used a 10 ms schedule, four times the board's rate; the FSM counts samples, so its confidence windows
were four times shorter than in flight. The deployments go to a dry-run stub. The datasets are the
ones regenerated in f77ec79 (KG50 unloading in the thrust curve: 9.19 kN s measured, 12.04 kN s for
the 10k ft sets). All five sets pass validation (`IGT CSV ok`), reach Landed, and end the replay with
no fault. Times are seconds after Launch acceptance; "truth" is the generator's model (re-run from
`tools/flight_profile` with the same seeds; it matches each `summary.json`). The replay feeds the FC
the `alt_m` column, which the generator writes with the FC's corrected altitude (`baro_agl()`: the
ISA pressure altitude of the sample minus that of the pad). The "before the fix" rows (10 ms
schedule, FC 659440e) replay copies whose `alt_m` was recomputed from `pressure_pa` with the old
sea-level form against the pad pressure.

| | nominal_10k | measured_motor | high_wind | early_apogee_noise | off_nominal |
|---|---|---|---|---|---|
| truth liftoff | 10.137 | 10.121 | 10.137 | 10.137 | 10.137 |
| FC Launch / Ascent | 10.60 / 10.76 | 10.64 / 10.80 | 10.60 / 10.76 | 10.60 / 10.76 | 10.60 / 10.76 |
| truth burnout (<5% peak) | 16.637 | 15.8 | 16.637 | 16.637 | 16.637 |
| FC Coast | 15.20 | 14.84 | 15.24 | 14.80 | 15.24 |
| truth max speed (m/s) | 235.8 | 187.9 | 235.1 | 235.9 | 236.0 |
| FC estimated max speed (m/s) | 237.89 | 186.83 | 237.73 | 249.71 | 238.56 |
| truth apogee (s, m) | 35.748, 3048.3 | 32.079, 2137.1 | 35.701, 3029.8 | 35.745, 3043.3 | 35.751, 3042.5 |
| FC Apogee state (s) | 35.56 | 31.88 | 35.52 | 35.48 | 35.60 |
| FC estimated apogee (s, m) | 35.88, 3046.54 | 32.12, 2136.09 | 35.88, 3027.58 | 36.28, 3047.52 | 35.8, 3041.5 |
| estimated - truth apogee (m) | -1.8 (-0.06%) | -1.0 (-0.05%) | -2.2 (-0.07%) | +4.2 (+0.14%) | -1.0 (-0.03%) |
| before the fix (sea-level formula) | +61.2 (+2.0%) | +43.2 (+2.0%) | +60.2 (+2.0%) | +77.0 (+2.5%) | +62.0 (+2.0%) |
| FC drogue (s, est m) | 37.08, 3034.39 | 33.44, 2124.33 | 37.04, 3015.69 | 37.04, 3013.62 | 37.08, 3029.53 |
| scripted drogue in the data (s) | 36.948 | 33.279 | 36.901 | 36.945 | 36.951 |
| truth crosses 457.2 m (s) | 138.96 | 101.34 | 137.76 | 140.04 | 139.72 |
| FC main (s, est m) | 139.16, 454.48 | 101.52, 454.55 | 137.92, 455.08 | 140.24, 455.23 | 139.88, 455.12 |
| true height at the FC main (m) | 452.4 | 452.5 | 453.0 | 452.4 | 453.0 |
| before the fix: true height at the FC main (m) | 448.2 | 448.2 | 448.0 | 449.0 | 444.9 |
| main opens in the data (s, m) | 139.246, 450.0 | 101.616, 450.0 | 138.038, 449.8 | 140.324, 450.1 | 146.514, 292.8 |
| truth touchdown (s) | 211.935 | 172.239 | 211.241 | 212.427 | 193.136 |
| FC Landed (s) | 215.40 | 175.72 | 214.92 | 216.00 | 196.72 |
| FC Recovery (s) | 220.40 | 180.72 | 219.92 | 221.00 | 201.72 |
| estimator deviations (kf_deviations) | 0 | 0 | 0 | 1 | 0 |

At the 10 ms schedule the same sets gave apogee errors of -1.8, -0.9, -2.2, +14.0 and -1.0 m, the
main at 453-454 m true, the Apogee state 0.35-0.4 s before the true apogee, the drogue 1.19-1.25 s
after it, Landed 2.2-2.4 s after touchdown and kf_deviations 0, 5, 0, 12, 0.

Findings:

- The estimator tracks the replay. On a noise-free copy of `nominal_10k` (`generate.py --variant
  nominal_10k --no-noise`), the estimated apogee is 3046.2 m, against a replayed baro peak of
  3046.15 m. The IMU, baro and attitude in the data are therefore mutually consistent.
- The FC's altitude matches the truth to about 0.1% at the 890 m site. The old sea-level form
  `44330 * (1 - (p/p0)^0.1903)` against the pad pressure overstates heights by about
  T(0)/T(890 m) - 1 = 2%: apogee would read 43-62 m high and the main would be commanded at about
  448 m true (445-449 m). The remaining -1 to -2 m at 10k ft is geopotential versus geometric height
  (the ISA altitude is geopotential; (3938^2 - 890^2) / 6371 km = 2.3 m), not a scale error.
- The FC commands the main at an estimated 454.5-455.2 m, 0.16-0.22 s after the true 457.2 m
  crossing, at 452.4-453.0 m true: about 4-5 m below the setting, from baro lag (port lag and IIR)
  and the 40 ms sample spacing (1 m at the 25 m/s drogue rate) plus the `MIN_SAMP_REEF` window.
- The FC reaches Apogee 0.15-0.27 s before the true apogee (its |v| < 4 m/s window). The drogue
  decision comes 1.30-1.36 s after the true apogee, 0.09-0.17 s after the 1.2 s the data assumes.
  With 40 ms samples the confirmation windows take four times longer than on the 10 ms schedule.
- Coast is declared on the velocity trend after `MOTOR_BURN_TIME` (3.5 s), before the hybrid's
  tail-off ends: 1.4 s before the 5%-of-peak burnout on the 10k sets and 1.0 s before it on
  `measured_motor`, near the time of maximum speed in both. In `early_apogee_noise`, baro noise and
  the static-port error move it to 14.80 s, 1.8 s before burnout. No deployment depends on it.
- `early_apogee_noise` has the largest apogee error, +4.2 m (+0.14%), with the estimator's peak
  0.53 s after the true apogee. The 3x baro noise window spans apogee (from 5 s before); fewer, 40 ms
  apart, noisy samples move the estimate less than the 10 ms schedule did (+14 m). The Apogee state
  and the drogue are on time (35.48 s and 37.04 s, 1.30 s after the true apogee), so there is no
  premature drogue.
- In `off_nominal`, a 24 g / 2000 dps (saturated) single-row IMU glitch during coast, a 2 s baro freeze and a 15 s GPS
  dropout and a 60 m GPS jump leave every decision unchanged. The late physical main (300 m) only delays Landed.
- Landed comes 3.5-3.7 s after touchdown (`MIN_SAMP_LANDED` samples at 40 ms and the velocity
  window). The 30 s of rest data is ample. Recovery follows 5 s later without pausing sensor
  acquisition or SD logging. The one kf_deviation (`early_apogee_noise`) is a failed spurious-transition
  check in the noisy boost, far below `KF_FLUC_THRES` (80).
