# Vehicle source files

`SWEET_V6.ork` is the team's OpenRocket design of the vehicle (OpenRocket 24.12,
rocket "SWEET_V6"), added 2026-11-23. It is the source of truth for the airframe
geometry, component masses, fins and recovery layout used by `../vehicle.json`, the
apogee planner's `.ork` import and the firmware-sim plant.

Notes:
- The stage mass is overridden to 40.84 kg (with `overridesubcomponentsmass`, so it is
  the whole rocket) and its CG to 2.667 m: the measured as-built values. They include
  the 14.32 kg `EntireMotor_nofuel` mass object (the motor hardware without propellant)
  and exclude the propellant. The file's own component build-up is 34.32 kg.
  `../vehicle.json` takes 40.84 kg as the dry mass; the propellant comes from the GS
  fill (planner) or `motor.propellant_mass_kg` (mock flights).
- The design's motor entry ("Midland", 101 mm x 1.73 m) has no thrust data in the file;
  its six stored simulations (2.1-2.9 km apogee) carry the HRAP design curve in their
  time series. `../ork_sims.py --extract-motor` rebuilds it as
  `../motors/hrap_design_sweet_v6.csv` (10.96 kN s, 5.68 kN peak, 9.0 kg), and
  `../ork_sims.py` flies the stored simulations' conditions with our model for
  comparison. Five of the six simulations predate the mass override (lighter builds).
- The ground station flies the measured 2026-11-16 motor by default.
- The N2O run tank volume used everywhere is the measured 967.45 in^3 (15.854 L).

Replace this file when the design changes and update `vehicle.json` from the import:
the planner test `ork_import_reads_the_sweet_v6_design` imports this file and checks
`vehicle.json` against it.
