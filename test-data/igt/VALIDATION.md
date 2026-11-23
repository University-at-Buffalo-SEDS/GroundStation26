# FC host replay of the IGT datasets

Each set was replayed through FlightComputer26 (origin/dev ee83418) with `tools/igt_replay_host.py`.
That tool runs the real `ground_test.c` CSV validation and interpolation, `kalman.c` and
`evaluation.c` on the host, on the board's 10 ms baro schedule. The deployments go to a dry-run stub.
All five sets pass validation (`IGT CSV ok`), reach Landed, and end the replay with no fault.
Times are seconds after Launch acceptance; "truth" is the generator's model.

| | nominal_10k | measured_motor | high_wind | early_apogee_noise | off_nominal |
|---|---|---|---|---|---|
| truth liftoff | 10.032 | 10.028 | 10.032 | 10.032 | 10.032 |
| FC Launch / Ascent | 10.46 / 10.6 | 10.5 / 10.67 | 10.46 / 10.59 | 10.46 / 10.59 | 10.45 / 10.59 |
| truth burnout (<5% peak) | 16.147 | 14.6 | 16.147 | 16.147 | 16.147 |
| FC Coast | 15.07 | 14.07 | 15.11 | 14.2 | 15.01 |
| truth max speed (m/s) | 244.2 | 148.3 | 243.4 | 244.2 | 244.3 |
| FC estimated max speed (m/s) | 250.97 | 148.93 | 250.63 | 269.54 | 251.34 |
| truth apogee (s, m) | 35.642, 3047.5 | 28.085, 1381.2 | 35.596, 3028.8 | 35.637, 3042.2 | 35.645, 3041.8 |
| FC Apogee state (s) | 35.33 | 27.77 | 35.29 | 35.12 | 35.35 |
| FC estimated apogee (s, m) | 35.73, 3108.52 | 28.23, 1408.85 | 35.72, 3089.31 | 34.54, 3109.29 | 35.71, 3102.38 |
| estimated - truth apogee (m) | +61.0 (+2.0%) | +27.6 (+2.0%) | +60.5 (+2.0%) | +67.1 (+2.2%) | +60.6 (+2.0%) |
| FC drogue (s, est m) | 36.89, 3097.66 | 29.34, 1397.97 | 36.85, 3077.93 | 36.89, 3077.56 | 36.89, 3091.91 |
| scripted drogue in the data (s) | 36.843 | 29.285 | 36.796 | 36.837 | 36.846 |
| truth crosses the main altitude (s; 457.2 m, off_nominal 300 m) | 139.68 | 67.782 | 139.816 | 139.87 | 146.398 |
| FC main (s, est m) | 140.06, 455.0 | 68.15, 455.18 | 140.19, 454.77 | 140.21, 449.04 | 140.24, 456.52 |
| main opens in the data (s, m) | 139.981, 450.1 | 68.082, 450.1 | 140.116, 449.8 | 140.171, 450.1 | 146.698, 292.9 |
| truth touchdown (s) | 215.138 | 140.302 | 213.068 | 212.261 | 191.425 |
| FC Landed (s) | 217.6 | 142.6 | 215.25 | 214.65 | 193.87 |
| FC Recovery (s) | 222.6 | 147.6 | 220.25 | 219.65 | 198.87 |
| estimator deviations (kf_deviations) | 0 | 0 | 0 | 4 | 0 |

Findings:

- The estimator tracks the replay. On a noise-free copy of `nominal_10k`, the estimated apogee
  (3108.3 m) equals the FC's own baro formula applied to the true apogee pressure (3108.1 m).
  The IMU, baro and attitude in the data are therefore mutually consistent.
- About +2% of altitude comes from the FC, not the data. `44330 * (1 - (p/p0)^0.1903)` is the
  sea-level ISA form. Applied with p0 = pad pressure at an 890 m site, it overstates heights by
  about T(0)/T(890 m) - 1 = 2%. So the FC commands the main at an estimated 457 m, which is
  about 448 m true, and reports apogee about 60 m high at 10k ft. A site-elevation-aware formula
  would remove this, for example the hypsometric equation with the pad temperature.
- The FC's main decision comes about 0.4 s after the true 457.2 m crossing, at about 448 m true, from
  the same 2% scale.
- The FC reaches Apogee about 0.3 s before the true apogee (its |v| < 4 m/s window). The drogue
  decision comes about 1.5 s after the true apogee, consistent with the 1.2 s the data assumes.
- Coast is declared on the velocity trend after `MOTOR_BURN_TIME` (3.5 s), before the hybrid's
  tail-off ends. In `early_apogee_noise`, baro noise and the static-port error move it to 14.2 s,
  mid-burn. Apogee is still found 0.5 s early, without a premature drogue.
- In `off_nominal`, a 24 g / 2000 dps single-row IMU glitch during coast, a 2 s baro freeze and a
  15 s GPS dropout leave every decision unchanged. The late physical main (300 m) only delays
  Landed.
- Landed comes 2.4-2.5 s after touchdown (`MIN_SAMP_LANDED` and the velocity window). The 30 s
  of rest data is ample.
