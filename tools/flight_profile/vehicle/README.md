# Vehicle source files

`SWEET_V6.ork` is the team's OpenRocket design of the vehicle (OpenRocket 24.12,
rocket "SWEET_V6"), added 2026-11-23. It is the source of truth for the airframe
geometry, component masses, fins and recovery layout used by `../vehicle.json`, the
apogee planner's `.ork` import and the firmware-sim plant.

Notes:
- The stage mass is overridden to 40.84 kg in the file.
- The design's motor entry ("Midland", 101 mm x 1.73 m) and its six stored
  simulations (2.1-2.9 km apogee) use OpenRocket's own motor curves; the ground
  station models the motor separately from measured test-fire data.
- The N2O run tank volume used everywhere is the measured 967.45 in^3 (15.854 L).

Replace this file when the design changes and re-run the import so `vehicle.json`
stays in sync.
