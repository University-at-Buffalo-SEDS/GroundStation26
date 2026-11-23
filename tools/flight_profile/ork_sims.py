#!/usr/bin/env python3
"""The OpenRocket design's own simulations (vehicle/SWEET_V6.ork) next to ours.

The .ork stores six simulations with their full time series. Their motor
("Midland", the team's HRAP design run exported as an .rse) is not embedded,
but every series carries Time, Thrust and Motor mass, so the design motor can
be rebuilt exactly (OpenRocket interpolates the motor file linearly; the 0.1 s
vertices are all on the 0.05 s time steps).

  # rebuild motors/hrap_design_sweet_v6.csv and .json from the .ork
  python3 tools/flight_profile/ork_sims.py --extract-motor
  # fly each stored simulation's conditions with our model and compare
  python3 tools/flight_profile/ork_sims.py
  python3 tools/flight_profile/ork_sims.py --json   # machine-readable

Each simulation is flown with ITS OWN masses (burnout mass and propellant from
its series): five of the six were run before the stage mass override was set
to the measured 40.84 kg, so they describe lighter, older builds. The vehicle
geometry and aero are vehicle.json's (imported from the .ork). Gusts are off in
our model; OpenRocket's wind is uniform with height, so the wind shear is
turned off too. Stdlib only.
"""
from __future__ import annotations

import argparse
import copy
import json
import math
import random
import sys
import xml.etree.ElementTree as ET
import zipfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import flightsim as fs  # noqa: E402

ORK = HERE / 'vehicle' / 'SWEET_V6.ork'
MOTOR_CSV = HERE / 'motors' / 'hrap_design_sweet_v6.csv'
MOTOR_JSON = HERE / 'motors' / 'hrap_design_sweet_v6.json'
MOTOR_SIM = 'IREC_STANDARDREPORT'   # the one simulation OpenRocket marks up to date


def load(path: Path = ORK) -> ET.Element:
    data = path.read_bytes()
    if data[:4] == b'PK\x03\x04':
        with zipfile.ZipFile(path) as z:
            name = next(n for n in z.namelist() if n.lower().endswith(('.ork', '.xml')))
            data = z.read(name)
    return ET.fromstring(data)


def _f(text):
    try:
        return float(text)
    except (TypeError, ValueError):
        return float('nan')


def simulations(root: ET.Element) -> list[dict]:
    out = []
    for s in root.find('simulations'):
        c = s.find('conditions')
        fd = s.find('flightdata')
        br = fd.find('databranch')
        types = br.get('types').split(',')
        rows = [[_f(x) for x in d.text.split(',')] for d in br.findall('datapoint')]
        series = {k: [r[i] for r in rows] for i, k in enumerate(types)}
        wind = c.find("wind[@model='average']")
        sd = _f(wind.findtext('standarddeviation')) if wind is not None else 0.0
        out.append({
            'name': s.findtext('name'),
            'status': s.get('status'),
            'configid': c.findtext('configid'),
            'rod_length_m': _f(c.findtext('launchrodlength')),
            'rod_angle_deg': _f(c.findtext('launchrodangle')),
            'rod_direction_deg': _f(c.findtext('launchroddirection')),
            'into_wind': c.findtext('launchintowind') == 'true',
            'wind_mps': _f(c.findtext('windaverage')),
            'wind_sd_mps': sd,
            'wind_dir_deg': math.degrees(_f(c.findtext('winddirection'))),
            'site_msl_m': _f(c.findtext('launchaltitude')),
            'lat_deg': _f(c.findtext('launchlatitude')),
            'lon_deg': _f(c.findtext('launchlongitude')),
            'atmosphere': c.find('atmosphere').get('model'),
            'timestep_s': _f(c.findtext('timestep')),
            'result': {k: _f(v) for k, v in fd.attrib.items()},
            'events': {e.get('type'): _f(e.get('time')) for e in reversed(br.findall('event'))},
            'series': series,
        })
    return out


def burn(sim: dict) -> tuple[list, list, list]:
    """Time, thrust and motor mass from ignition to the first zero after the burn."""
    s = sim['series']
    t, f, m = s['Time'], s['Thrust'], s['Motor mass']
    end = next(i for i in range(1, len(t)) if f[i] <= 0.0 and t[i] > 0.5)
    return t[:end + 1], f[:end + 1], m[:end + 1]


def vertices(t, f, tol_n=0.01):
    """Drops points that lie on the straight line between their neighbours."""
    keep = [0]
    for i in range(1, len(t) - 1):
        a, b = keep[-1], i + 1
        k = (t[i] - t[a]) / (t[b] - t[a])
        if abs(f[a] + (f[b] - f[a]) * k - f[i]) > tol_n:
            keep.append(i)
    keep.append(len(t) - 1)
    return keep


def impulse(t, f):
    return sum(0.5 * (f[i] + f[i + 1]) * (t[i + 1] - t[i]) for i in range(len(t) - 1))


def extract_motor(root: ET.Element) -> tuple[str, dict]:
    sims = simulations(root)
    ref = next(s for s in sims if s['name'] == MOTOR_SIM)
    t, f, m = burn(ref)
    # Every stored simulation flew the same curve (they differ in propellant
    # mass; their printed sample times are rounded, hence the 0.1 % tolerance).
    for s in sims:
        ts, fs_, _ = burn(s)
        grid = dict(zip(ts, fs_))
        diff = max(abs(grid[x] - y) for x, y in zip(t, f) if x in grid)
        assert diff < 1e-3 * max(f), (s['name'], diff)
    keep = vertices(t, f)
    tv, fv, mv = [t[i] for i in keep], [f[i] for i in keep], [m[i] for i in keep]
    # The vertices reproduce every stored sample.
    for x, y in zip(t, f):
        j = max(i for i in range(len(tv)) if tv[i] <= x)
        if j < len(tv) - 1:
            yy = fv[j] + (fv[j + 1] - fv[j]) * (x - tv[j]) / (tv[j + 1] - tv[j])
            assert abs(yy - y) < 0.05, (x, y, yy)
    total = impulse(tv, fv)
    peak = max(fv)
    on = next(x for x, y in zip(tv, fv) if y >= 0.05 * peak)
    off = next(x for x, y in zip(reversed(tv), reversed(fv)) if y >= 0.05 * peak)
    # Motor mass against burnt impulse fraction: OpenRocket depletes it in proportion.
    cum, worst = 0.0, 0.0
    for i in range(1, len(tv)):
        cum += 0.5 * (fv[i] + fv[i - 1]) * (tv[i] - tv[i - 1])
        worst = max(worst, abs((mv[0] - mv[i]) / mv[0] - cum / total))
    props = sorted({round(burn(s)[2][0], 3) for s in sims})
    motor = root.find('.//motormount/motor')
    mount = next(e for e in root.iter('innertube') if e.find('motormount') is not None)
    hw = next(e for e in root.iter('masscomponent') if e.findtext('name') == 'EntireMotor_nofuel')
    # Its station: the outer components are laid end to end; it sits 'top'-relative in its tube.
    x, hw_x = 0.0, None
    for c in root.find('rocket/subcomponents/stage/subcomponents'):
        if hw in c.iter('masscomponent'):
            hw_x = x + float(hw.findtext('axialoffset'))
        x += float((c.findtext('length') or '0').split()[-1])
    lines = [
        '# UB SEDS N2O hybrid, design motor of the OpenRocket model SWEET_V6',
        f'# (tools/flight_profile/vehicle/SWEET_V6.ork, simulation "{MOTOR_SIM}"). The .ork names the',
        f'# motor "{motor.findtext("designation")}" ({float(motor.findtext("diameter")) * 1000:.0f} mm x '
        f'{float(motor.findtext("length")):.2f} m) and the rocket comment',
        '# says the thrust data is from HRAP; the motor file is not embedded, so this is',
        '# rebuilt from the stored time series (OpenRocket interpolates linearly; these are',
        '# the vertices). motor_mass_kg is OpenRocket\'s propellant mass for that run (the',
        '# motor entry has no dry mass: the hardware is the EntireMotor_nofuel mass object).',
        '# tools/flight_profile/ork_sims.py --extract-motor rebuilds this file.',
        't_s,thrust_n,motor_mass_kg',
    ]
    lines += [f'{x:.3f},{y:.3f},{z:.4f}' for x, y, z in zip(tv, fv, mv)]
    summary = {
        'motor': 'UB SEDS N2O hybrid, HRAP design curve as flown by the OpenRocket model SWEET_V6',
        'source': f'tools/flight_profile/vehicle/SWEET_V6.ork, simulation {MOTOR_SIM} (OpenRocket 24.12), '
                  'Time/Thrust/Motor mass columns; the motor file itself is not in the .ork',
        'ork_motor_entry': {
            'manufacturer': motor.findtext('manufacturer'), 'designation': motor.findtext('designation'),
            'diameter_m': float(motor.findtext('diameter')), 'length_m': float(motor.findtext('length')),
            'digest': motor.findtext('digest'),
        },
        'total_impulse_ns': round(total, 1),
        'peak_thrust_n': round(peak, 1),
        'peak_thrust_s': tv[fv.index(peak)],
        'burn_time_s': round(off - on, 2),
        'burn_time_definition': 'time thrust >= 5% of peak (0.1-3.4 s)',
        'thrust_zero_s': tv[-1],
        'thrust_note': 'OpenRocket logs burnout at 4.0 s; the last non-zero vertex is 3.9 s (40 N)',
        'average_thrust_n': round(total / (off - on), 1),
        'propellant_mass_kg': round(mv[0], 3),
        'propellant_mass_other_sims_kg': props,
        'propellant_note': f'OpenRocket carries {mv[0]:.1f} kg of motor mass to 0 at burnout in {MOTOR_SIM} (and in the '
                           'current configuration); TESTFIRE_NOTFULLOXTANK and Simulations 1-3 use the same curve '
                           f'with {min(props):.1f} kg. The mass falls in proportion to the burnt impulse '
                           f'(max deviation {worst * 100:.2f} %), the same rule as flightsim.Motor. Whether it is '
                           'N2O alone or N2O plus the fuel grain is not stated in the file.',
        'delivered_isp_s': round(total / (mv[0] * fs.G0), 1),
        'motor_hardware': {
            'mass_kg': round(float(hw.findtext('mass')), 4),
            'what': 'EntireMotor_nofuel mass object in the Oxidizer Tank tube (comment: the mass of the entire motor '
                    'without fuel, paired with the .rse thrust file); part of the 40.84 kg stage override',
            'cg_from_nose_m': round(hw_x, 4),
        },
        'combustion_chamber_tube': {
            'name': mount.findtext('name'),
            'length_m': float(mount.findtext('length')),
            'outer_diameter_m': round(2 * float(mount.findtext('outerradius')), 5),
            'wall_m': round(float(mount.findtext('thickness')), 6),
            'material': mount.find('material').text,
            'mass_override_kg': float(mount.findtext('overridemass') or 'nan'),
        },
        'not_in_the_ork': 'grain dimensions, nozzle throat/exit, injector, O/F and chamber pressure: the .ork models '
                          'the motor only as a thrust curve, a propellant mass and a hardware mass object',
    }
    return '\n'.join(lines) + '\n', summary


def flight_config(base: dict, sim: dict) -> tuple[dict, float, float]:
    """vehicle.json set to a stored simulation's conditions and masses."""
    cfg = copy.deepcopy(base)
    t, f, m = burn(sim)
    prop = m[0]
    dry = sim['series']['Mass'][0] - prop
    cfg['vehicle']['dry_mass_kg'] = dry
    cfg['motor']['residual_fraction'] = 0.0
    cfg['motor']['oscillation_amplitude'] = 0.0
    env = cfg['environment']
    env.update({'site_lat_deg': sim['lat_deg'], 'site_lon_deg': sim['lon_deg'], 'site_msl_m': sim['site_msl_m'],
                'temp_offset_k': 0.0, 'wind_mps': sim['wind_mps'], 'wind_from_deg': sim['wind_dir_deg'],
                'wind_shear_exponent': 0.0, 'gust_sigma_mps': 0.0})
    lau = cfg['launch']
    lau.update({'rail_length_m': sim['rod_length_m'], 'tilt_deg': sim['rod_angle_deg'], 'ignition_s': 0.0,
                'azimuth_deg': sim['wind_dir_deg'] if sim['into_wind'] else sim['rod_direction_deg']})
    return cfg, dry, prop


def fly(cfg: dict, motor: fs.Motor) -> dict:
    tr = fs.fly(cfg, random.Random(1), motor, record=True, stop_at_apogee=True)
    ev = tr.events
    acc = 0.0
    for i in range(1, len(tr.t)):
        if tr.phase[i] in ('rail', 'boost'):
            dv = [tr.vel[i][k] - tr.vel[i - 1][k] for k in range(3)]
            acc = max(acc, math.sqrt(sum(x * x for x in dv)) / (tr.t[i] - tr.t[i - 1]))
    return {
        'apogee_m': ev['apogee_m'], 'time_to_apogee_s': ev['apogee_s'] - cfg['launch']['ignition_s'],
        'max_velocity_mps': tr.extra['max_speed'][0], 'max_mach': tr.extra['max_mach'][0],
        'rail_exit_mps': ev['rail_exit_mps'], 'max_accel_mps2': acc,
    }


def motor_for(sim: dict, prop: float) -> fs.Motor:
    t, f, _ = burn(sim)
    m = fs.Motor(list(t), [max(0.0, x) for x in f], prop, 0.0)
    m._integrate()
    return m


def compare(base: dict, root: ET.Element, exit_d: float) -> list[dict]:
    rows = []
    for sim in simulations(root):
        cfg, dry, prop = flight_config(base, sim)
        motor = motor_for(sim, prop)
        motor.exit_d = exit_d
        ours = fly(cfg, motor)
        r = sim['result']
        theirs = {'apogee_m': r['maxaltitude'], 'time_to_apogee_s': r['timetoapogee'],
                  'max_velocity_mps': r['maxvelocity'], 'max_mach': r['maxmach'],
                  'rail_exit_mps': r['launchrodvelocity'], 'max_accel_mps2': r['maxacceleration']}
        s = sim['series']
        coast = [i for i, x in enumerate(s['Time']) if 4.5 <= x <= theirs['time_to_apogee_s'] - 1.0]
        or_cd = sum(s['Drag coefficient'][i] for i in coast) / max(len(coast), 1)
        rows.append({'name': sim['name'], 'status': sim['status'], 'dry_kg': round(dry, 3), 'prop_kg': round(prop, 3),
                     'rod_m': sim['rod_length_m'], 'rod_deg': sim['rod_angle_deg'], 'wind_mps': sim['wind_mps'],
                     'wind_sd_mps': sim['wind_sd_mps'], 'site_msl_m': round(sim['site_msl_m'], 1),
                     'or_mean_coast_cd': round(or_cd, 3), 'openrocket': theirs,
                     'ours': {k: round(v, 3) for k, v in ours.items()}})
    return rows


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    ap.add_argument('--ork', type=Path, default=ORK)
    ap.add_argument('--config', type=Path, default=HERE / 'vehicle.json')
    ap.add_argument('--extract-motor', action='store_true', help='rewrite motors/hrap_design_sweet_v6.{csv,json}')
    ap.add_argument('--json', action='store_true')
    ap.add_argument('--set', action='append', default=[], metavar='KEY=JSON', help='override a vehicle.json key')
    a = ap.parse_args(argv)
    root = load(a.ork)
    if a.extract_motor:
        csv_text, summary = extract_motor(root)
        MOTOR_CSV.write_text(csv_text)
        MOTOR_JSON.write_text(json.dumps({'summary': summary}, indent=2) + '\n')
        print(f"{MOTOR_CSV.name}: {summary['total_impulse_ns']:.0f} N s, peak {summary['peak_thrust_n']:.0f} N, "
              f"burn {summary['burn_time_s']} s, propellant {summary['propellant_mass_kg']} kg")
        return 0
    base = json.loads(a.config.read_text())
    for kv in a.set:
        k, _, v = kv.partition('=')
        node = base
        keys = k.split('.')
        for x in keys[:-1]:
            node = node[x]
        node[keys[-1]] = json.loads(v)
    # OpenRocket's base drag ignores the exhaust; so does this comparison.
    rows = compare(base, root, 0.0)
    if a.json:
        print(json.dumps(rows, indent=1))
        return 0
    keys = [('apogee_m', 'apogee m', 0), ('max_velocity_mps', 'max v m/s', 1), ('time_to_apogee_s', 't apo s', 2),
            ('rail_exit_mps', 'rail m/s', 1), ('max_accel_mps2', 'max a m/s2', 1)]
    print(f"{'simulation':24} {'status':9} {'dry+prop kg':>12} {'rod m/deg':>10} {'wind':>9} "
          + ' '.join(f'{lbl:>22}' for _, lbl, _ in keys))
    for r in rows:
        cells = []
        for k, _, d in keys:
            o, u = r['openrocket'][k], r['ours'][k]
            cells.append(f'{o:9.{d}f} {u:9.{d}f} {100 * (u - o) / o:+4.0f}%'.rjust(22))
        print(f"{r['name']:24} {r['status']:9} {r['dry_kg']:6.2f}+{r['prop_kg']:4.2f} "
              f"{r['rod_m']:5.2f}/{r['rod_deg']:<3g} {r['wind_mps']:4.1f}~{r['wind_sd_mps']:<3.1f} " + ' '.join(cells))
    print('(each cell: OpenRocket, ours, difference)')
    return 0


if __name__ == '__main__':
    sys.exit(main())
