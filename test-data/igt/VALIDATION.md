# FC host replay of the IGT datasets

> **Stale since 2026-11-22.** The datasets were regenerated after the motor curve gained the
> KG50 unloading term (`tools/flight_profile/extract_thrust.py`, 9.19 kN s instead of 7.05).
> `nominal_10k` and its variants are re-tuned to 3048 m and `measured_motor` now reaches about
> 2137 m, so the numbers below describe the previous files. The FC host replay
> (`tools/igt_replay_host.py`, not in this repo) has not been re-run on the new files.

Each set was replayed through FlightComputer26 (fix/baro-agl-altitude 659440e, on origin/dev d286b5e)
with `tools/igt_replay_host.py`. That tool runs the real `ground_test.c` CSV validation and
interpolation, `kalman.c` and `evaluation.c` on the host, on the board's 10 ms baro schedule. The
deployments go to a dry-run stub. All five sets pass validation (`IGT CSV ok`), reach Landed, and end
the replay with no fault. Times are seconds after Launch acceptance; "truth" is the generator's model.
The `alt_m` column was regenerated with the FC's corrected altitude (`baro_agl()`: the ISA pressure
altitude of the sample minus that of the pad); pressures, IMU and GPS rows are unchanged.

| | nominal_10k | measured_motor | high_wind | early_apogee_noise | off_nominal |
|---|---|---|---|---|---|
| truth liftoff | 10.032 | 10.028 | 10.032 | 10.032 | 10.032 |
| FC Launch / Ascent | 10.46 / 10.6 | 10.5 / 10.67 | 10.46 / 10.59 | 10.46 / 10.59 | 10.45 / 10.59 |
| truth burnout (<5% peak) | 16.147 | 14.6 | 16.147 | 16.147 | 16.147 |
| FC Coast | 15.06 | 14.06 | 15.1 | 14.2 | 15.0 |
| truth max speed (m/s) | 244.2 | 148.3 | 243.4 | 244.2 | 244.3 |
| FC estimated max speed (m/s) | 246.44 | 146.3 | 246.09 | 264.77 | 246.84 |
| truth apogee (s, m) | 35.642, 3047.5 | 28.085, 1381.2 | 35.596, 3028.8 | 35.637, 3042.2 | 35.645, 3041.8 |
| FC Apogee state (s) | 35.31 | 27.74 | 35.27 | 35.11 | 35.32 |
| FC estimated apogee (s, m) | 35.73, 3045.59 | 28.23, 1380.41 | 35.72, 3026.82 | 34.54, 3046.43 | 35.71, 3039.64 |
| estimated - truth apogee (m) | -1.9 (-0.06%) | -0.8 (-0.06%) | -2.0 (-0.07%) | +4.2 (+0.14%) | -2.2 (-0.07%) |
| before the fix (ee83418 data, sea-level formula) | +61.0 (+2.0%) | +27.6 (+2.0%) | +60.5 (+2.0%) | +67.1 (+2.2%) | +60.6 (+2.0%) |
| FC drogue (s, est m) | 36.89, 3034.96 | 29.34, 1369.74 | 36.85, 3015.68 | 36.89, 3015.34 | 36.9, 3028.32 |
| scripted drogue in the data (s) | 36.843 | 29.285 | 36.796 | 36.837 | 36.846 |
| truth crosses the main altitude (s; 457.2 m, off_nominal 300 m) | 139.68 | 67.782 | 139.816 | 139.87 | 146.398 |
| FC main (s, est m) | 139.8, 456.68 | 67.9, 456.48 | 139.94, 456.52 | 140.01, 456.58 | 139.84, 456.52 |
| true height at the FC main (m) | 454.3 | 454.4 | 454.1 | 453.9 | 454.3 |
| before the fix: true height at the FC main (m) | 448.1 | 448.4 | 447.9 | 449.1 | 445.0 |
| main opens in the data (s, m) | 139.981, 450.1 | 68.082, 450.1 | 140.116, 449.8 | 140.171, 450.1 | 146.698, 292.9 |
| truth touchdown (s) | 215.138 | 140.302 | 213.068 | 212.261 | 191.425 |
| FC Landed (s) | 217.53 | 142.59 | 215.24 | 214.64 | 193.86 |
| FC Recovery (s) | 222.53 | 147.59 | 220.24 | 219.64 | 198.86 |
| estimator deviations (kf_deviations) | 4 | 0 | 0 | 9 | 0 |

Findings:

- The estimator tracks the replay. On a noise-free copy of `nominal_10k`, the estimated apogee is
  3045.4 m, the same as the replayed baro peak (3045.35 m). The IMU, baro and attitude in the data are
  therefore mutually consistent.
- The FC's altitude now matches the truth to about 0.1% at the 890 m site. Before 659440e it used the
  sea-level form `44330 * (1 - (p/p0)^0.1903)` against the pad pressure, which overstates heights by
  about T(0)/T(890 m) - 1 = 2%: apogee read about 60 m high and the main was commanded at about
  448 m true (445-449 m). The remaining -2 m at 10k ft is geopotential versus geometric height (the ISA
  altitude is geopotential; (3937.5^2 - 890^2) / 6371 km = 2.3 m), not a scale error.
- The FC commands the main at an estimated 456.5-456.7 m, about 0.12 s after the true 457.2 m
  crossing, at 454 m true: about 3 m of baro lag (port lag and IIR) at the 25 m/s drogue rate.
- The FC reaches Apogee about 0.3 s before the true apogee (its |v| < 4 m/s window). The drogue
  decision comes about 1.2-1.5 s after the true apogee, consistent with the 1.2 s the data assumes.
- Coast is declared on the velocity trend after `MOTOR_BURN_TIME` (3.5 s), before the hybrid's
  tail-off ends. In `early_apogee_noise`, baro noise and the static-port error move it to 14.2 s,
  mid-burn. Apogee is still found 0.5 s early, without a premature drogue.
- In `off_nominal`, a 24 g / 2000 dps single-row IMU glitch during coast, a 2 s baro freeze and a
  15 s GPS dropout leave every decision unchanged. The late physical main (300 m) only delays
  Landed.
- Landed comes 2.4 s after touchdown (`MIN_SAMP_LANDED` and the velocity window). The 30 s of rest
  data is ample. The kf_deviations count failed spurious-transition checks: the velocity window for
  Landed during tip-over (`nominal_10k`, `early_apogee_noise`) and the velocity trend during the
  noisy boost (`early_apogee_noise`). All are far below `KF_FLUC_THRES` (80).
