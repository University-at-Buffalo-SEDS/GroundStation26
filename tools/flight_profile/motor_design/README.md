# Motor design preset

`current_single_port_baseline.preset.yaml` is the team's hybrid motor design preset
(format `hybrid_sim_fixed_motor_replay_preset_v1`, "Current single-port motor
baseline", added 2026-11-23). It describes the paraffin-filled ABS gyroid grain burned
with N2O: grain, injector, nozzle and tank geometry, regression and efficiency
calibration, and limits.

The full `.myrap` bundle (15.8 MB) also carries the NASA CEA performance surface
(`artifacts/cea_surface.npz`), the voxelised gyroid material field and the source
print file. It is kept outside the repository because of its size:

- file: `SWEET_V6 motor preset .myrap`, sha256 `57b2828849326afa3c9d95dd034903a47153b51deeea17ba7d9918b5b19832c9`
- workspace copy: `.workspace-progress/myrap-20261123/sim.myrap`

The preset references those artifacts by sha256. Parts of the calibration are marked
provisional in the preset (vapour-tail flow prior, ignition puck mass).

## Star grain (current design)

`star_grain.stl` is the star-port grain the team is switching to (added 2026-11-23),
replacing the cylindrical port in the preset above. Measured from the STL (mm):

- 10-point star, prismatic (constant cross-section along the length)
- star tip radius 34.29 (1.35 in), valley radius 12.70 (0.50 in)
- initial port area 1346 mm^2 (equivalent circle 41.4 mm), burning perimeter 451 mm
- outer diameter 84.99, length 203.2 (8 in)
- web 8.2 mm beyond the star tips, 29.8 mm behind the valleys

The STL is the source of truth for the grain geometry; the motor model's star
parameters are derived from it.
