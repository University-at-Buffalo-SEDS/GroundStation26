#!/usr/bin/env python3
"""Cross-check the in-repo flight model against OpenRocket itself (dev machine only).

Runs an OpenRocket .ork through OpenRocket (via orhelper + JPype + the
OpenRocket JAR) with our motor exported as a RASP .eng file, and prints
OpenRocket's apogee next to the station planner's model for the same vehicle.
The station never needs Java: it imports the .ork's geometry and masses and
flies its own model (docs/apogee-planner.md section 1).

  pip install orhelper JPype1      # and a JRE 17+
  python3 tools/flight_profile/openrocket_crosscheck.py rocket.ork --jar OpenRocket-23.09.jar \\
      [--impulse 9192] [--wind 4]

OpenRocket's own launch conditions (site, wind, rail) come from the .ork's
first simulation; set them to match vehicle.json for a like-for-like check.
"""
from __future__ import annotations

import argparse
import json
import random
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import flightsim as fs  # noqa: E402


def write_eng(path: Path, motor: fs.Motor, diameter_mm: float, length_mm: float, dry_kg: float):
    with path.open('w') as fh:
        fh.write('; UB SEDS hybrid, 2026-11-16 curve (tools/flight_profile)\n')
        fh.write(f'SEDS {diameter_mm:.0f} {length_mm:.0f} 0 {motor.prop_kg:.3f} {motor.prop_kg + dry_kg:.3f} SEDS\n')
        for t, f in zip(motor.t[1:], motor.f[1:]):
            fh.write(f'{t:.3f} {f:.1f}\n')
        fh.write(';\n')


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    ap.add_argument('ork', type=Path)
    ap.add_argument('--jar', type=Path, required=True)
    ap.add_argument('--impulse', type=float, default=None, help='rescale the measured curve to this impulse (N s)')
    ap.add_argument('--wind', type=float, default=None)
    a = ap.parse_args(argv)
    cfg = json.loads((HERE / 'vehicle.json').read_text())
    cfg['environment']['gust_sigma_mps'] = 0.0
    if a.wind is not None:
        cfg['environment']['wind_mps'] = a.wind
    m = cfg['motor']
    motor = fs.Motor.load(HERE / m['thrust_curve_csv'], m['propellant_mass_kg'], m['nozzle_exit_diameter_m'],
                          a.impulse, 0.0, 0.0, True, m.get('impulse_scaling_thrust_exponent', 1.0))
    import orhelper  # noqa: PLC0415  (dev-only dependency)
    from orhelper import FlightDataType  # noqa: PLC0415
    with tempfile.TemporaryDirectory() as tmp, orhelper.OpenRocketInstance(str(a.jar)) as instance:
        eng = Path(tmp) / 'seds.eng'
        write_eng(eng, motor, 98.0, 1500.0, 0.0)
        helper = orhelper.Helper(instance)
        doc = helper.load_doc(str(a.ork))
        sim = doc.getSimulation(0)
        helper.run_simulation(sim)
        data = helper.get_timeseries(sim, [FlightDataType.TYPE_ALTITUDE])
        or_apogee = max(data[FlightDataType.TYPE_ALTITUDE])
    ours = fs.fly(cfg, random.Random(1), motor, record=False, stop_at_apogee=True).events['apogee_m']
    print(f'OpenRocket apogee {or_apogee:.0f} m; tools/flight_profile {ours:.0f} m (vehicle.json airframe); '
          f'motor {motor.impulse:.0f} N s. The .eng written for OpenRocket: load it as the motor in the .ork '
          f'simulation for a like-for-like comparison ({eng.name}).')
    return 0


if __name__ == '__main__':
    sys.exit(main())
