# FC host replay of the IGT datasets

Each set was replayed through FlightComputer26 origin/dev 9a3eade with `tools/igt_replay_host.py`
in that repo. The tool runs the real `ground_test.c` CSV validation and interpolation, `kalman.c`
and `evaluation.c` on the host, on the board's 40 ms baro schedule (`Baro_ODR_25` in `recovery.c`).
The deployments go to a dry-run stub. The datasets are the ones regenerated for the SWEET_V6
vehicle (`tools/flight_profile/vehicle/SWEET_V6.ork`: 4.39 m x 156 mm, 3 fins, boattail, 40.84 kg
dry with the motor hardware, main descent 6.8 m/s; 9.19 kN s measured motor, 14.42 kN s for the
10k ft sets). All five sets pass validation (`IGT CSV ok`), reach Landed and Recovery in order, and
end the replay with no fault (IGT mode 4, reason 0). Times are seconds after Launch acceptance;
"truth" is the generator's model (re-run from `tools/flight_profile` with the same seeds; it
matches each `summary.json`).

| | nominal_10k | measured_motor | high_wind | early_apogee_noise | off_nominal |
|---|---|---|---|---|---|
| truth liftoff | 10.152 | 10.126 | 10.152 | 10.152 | 10.152 |
| FC Launch / Ascent | 10.68 / 10.84 | 10.72 / 10.88 | 10.68 / 10.84 | 10.68 / 10.84 | 10.68 / 10.84 |
| truth burnout (<5% peak) | 17.265 | 15.8 | 17.265 | 17.265 | 17.265 |
| FC Coast | 15.88 | 14.88 | 15.76 | 15.32 | 15.96 |
| truth max speed (m/s) | 220.5 | 144.9 | 219.6 | 220.5 | 220.6 |
| FC estimated max speed (m/s) | 221.45 | 143.03 | 220.53 | 230.37 | 221.71 |
| truth apogee (s, m) | 36.584, 3048.4 | 29.407, 1512.1 | 36.421, 3013.0 | 36.557, 3038.5 | 36.544, 3038.0 |
| FC Apogee state (s) | 36.40 | 29.24 | 36.24 | 36.32 | 36.40 |
| FC estimated apogee (s, m) | 36.60, 3046.69 | 29.52, 1511.42 | 36.52, 3012.46 | 36.60, 3036.58 | 36.76, 3036.72 |
| estimated - truth apogee (m) | -1.7 (-0.06%) | -0.7 (-0.04%) | -0.5 (-0.02%) | -1.9 (-0.06%) | -1.3 (-0.04%) |
| FC drogue (s, est m) | 37.92, 3034.59 | 30.76, 1499.87 | 37.76, 3000.27 | 37.88, 3005.69 | 37.88, 3024.74 |
| FC drogue after true apogee (s) | 1.34 | 1.35 | 1.34 | 1.32 | 1.34 |
| scripted drogue in the data (s) | 37.785 | 30.607 | 37.621 | 37.758 | 37.744 |
| truth crosses 457.2 m (s) | 140.971 | 74.448 | 137.922 | 140.176 | 140.502 |
| FC main (s, est m) | 141.16, 455.14 | 74.64, 454.65 | 138.12, 454.86 | 140.40, 454.64 | 140.72, 454.30 |
| true height at the FC main (m) | 452.7 | 452.5 | 452.3 | 451.9 | 452.0 |
| main opens in the data (s, m) | 141.272, 450.1 | 74.748, 449.9 | 138.222, 449.8 | 140.476, 450.2 | 147.512, 292.8 |
| truth touchdown (s) | 204.03 | 136.746 | 203.208 | 203.822 | 189.998 |
| FC Landed (s) | 207.72 | 140.40 | 206.64 | 207.52 | 193.60 |
| FC Recovery (s) | 212.72 | 145.40 | 211.64 | 212.52 | 198.60 |
| estimator deviations (kf_deviations) | 0 | 0 | 0 | 0 | 0 |
| igt_mode / reason | 4 / 0 | 4 / 0 | 4 / 0 | 4 / 0 | 4 / 0 |

Pass criteria and findings (all five pass):

- The estimator tracks the replay: the estimated apogee is within 0.5-1.9 m (0.02-0.06 %) of the
  truth on every set, including `early_apogee_noise` (-1.9 m).
- No premature drogue: the FC reaches Apogee 0.14-0.24 s before the true apogee (its |v| < 4 m/s
  window) and commands the drogue 1.32-1.35 s after it, 0.12-0.15 s after the 1.2 s the data
  assumes.
- The FC commands the main at an estimated 454.3-455.1 m, 0.19-0.22 s after the true 457.2 m
  crossing, at 451.9-452.7 m true (baro lag, the 40 ms sample spacing and the `MIN_SAMP_REEF`
  window). In `off_nominal` the main physically opens at 300 m; the FC still commands it on time.
- Coast is declared on the velocity trend after `MOTOR_BURN_TIME` (3.5 s), before the hybrid's
  tail-off ends: 0.9-1.9 s before the 5 %-of-peak burnout, near the time of maximum speed. No
  deployment depends on it. The heavier vehicle's 10k sets now burn out at 17.3 s (14.4 kN s on
  the measured curve shape, stretched in time by the square root of the impulse ratio).
- `early_apogee_noise` overestimates the peak speed by 10 m/s (static-port error and baro noise
  in boost) and its apogee is still within 2 m; kf_deviations is 0 on every set.
- In `off_nominal`, the saturated IMU glitch, the 2 s baro freeze, the 15 s GPS dropout and the
  60 m GPS jump leave every decision unchanged; the late physical main only delays Landed.
- Landed comes 3.4-3.7 s after touchdown (`MIN_SAMP_LANDED` samples at 40 ms and the velocity
  window); Recovery follows 5 s later. The 30 s of rest data is ample.
