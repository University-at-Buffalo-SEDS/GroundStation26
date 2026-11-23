#!/usr/bin/env python3
"""Generate integrated-ground-test (IGT) mock-flight CSVs for the FC SD card.

A physically grounded flight (flightsim.py: USSA-76 atmosphere, component
drag build-up, measured or scaled hybrid thrust curve, rail, weathercocking,
roll, dual-deploy recovery with opening shocks and swing, landing and
tip-over) sampled through datasheet sensor models (sensors.py: BMI088,
BMP390, GPS). Output is the FC's IGT layout, checked with tools/igt_csv.py:

    <out>/IGT/IMU0.CSV  BARO0.CSV  GPS0.CSV   + <out>/summary.json

Assumptions and their sources live in vehicle.json; variants.json lists the
committed datasets (test-data/igt/). Deterministic for a given seed.

Examples:
  # every committed dataset (test-data/igt/*) and its README
  python3 tools/flight_profile/generate.py --all
  # one variant, with overrides
  python3 tools/flight_profile/generate.py --variant nominal_10k --out /tmp/igt --wind 6 --seed 3
  # the measured motor on a custom pad, no sensor noise
  python3 tools/flight_profile/generate.py --out /tmp/clean --site 32.99,-106.97,1401 --no-noise
  # what dry mass reaches 3048 m with the configured motor?
  python3 tools/flight_profile/generate.py --solve-dry-mass 3048

Requires numpy.
"""
from __future__ import annotations

import argparse
import copy
import json
import math
import random
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE.parent))

import flightsim as fs   # noqa: E402
import sensors           # noqa: E402
import igt_csv           # noqa: E402

REPO = HERE.parent.parent
DATA = REPO / 'test-data' / 'igt'


def set_path(cfg: dict, dotted: str, value):
    node = cfg
    keys = dotted.split('.')
    for k in keys[:-1]:
        node = node[k]
    if keys[-1] not in node:
        raise SystemExit(f'unknown config key {dotted}')
    node[keys[-1]] = value


def load_motor(cfg: dict) -> fs.Motor:
    m = cfg['motor']
    return fs.Motor.load(HERE / m['thrust_curve_csv'], m['propellant_mass_kg'], m['nozzle_exit_diameter_m'],
                         m['total_impulse_ns'], m['oscillation_amplitude'], m['oscillation_hz'],
                         m.get('scale_propellant_with_impulse', False), m.get('impulse_scaling_thrust_exponent', 1.0))


def apogee(cfg: dict, seed: int) -> float:
    tr = fs.fly(cfg, random.Random(seed), load_motor(cfg), record=False, stop_at_apogee=True)
    return tr.events.get('apogee_m', 0.0)


def bisect(cfg: dict, seed: int, key: str, lo: float, hi: float, target: float, rising: bool) -> float | None:
    def f(x):
        c = copy.deepcopy(cfg)
        set_path(c, key, x)
        return apogee(c, seed)
    flo, fhi = f(lo), f(hi)
    if (flo - target) * (fhi - target) > 0:
        return None
    for _ in range(30):
        mid = 0.5 * (lo + hi)
        if (f(mid) < target) == rising:
            lo = mid
        else:
            hi = mid
        if hi - lo < 1e-3 * max(abs(mid), 1):
            break
    return 0.5 * (lo + hi)


def summarize(cfg, truth: fs.Truth, motor: fs.Motor, rows: dict, folder: Path) -> dict:
    ev = truth.events
    import numpy as np
    t = np.array(truth.t)
    f = np.array(truth.f_world)
    fmag = np.linalg.norm(f, axis=1)
    boost = (t >= ev['liftoff_s']) & (t <= ev['burnout_s'])
    pos = np.array(truth.pos)
    vel = np.array(truth.vel)

    def rate(t0, t1):
        sel = (t >= t0) & (t <= t1)
        return float(-np.mean(vel[sel, 2])) if sel.any() else None

    def peak(t0, t1):
        sel = (t >= t0) & (t <= t1)
        return round(float(fmag[sel].max()) / fs.G0, 2)

    land = int(ev['landing_s'] * 1000)
    imu = np.array([r[1:4] for r in rows['IMU']])
    on, off = motor.time_at_fraction_of_peak(0.05)
    sizes = {p.name: p.stat().st_size for p in sorted((folder / 'IGT').glob('*.CSV'))}
    s = {
        'motor': {
            'curve': cfg['motor']['thrust_curve_csv'],
            'total_impulse_ns': round(motor.impulse, 1),
            'scaled': cfg['motor']['total_impulse_ns'] is not None,
            'peak_thrust_n': round(max(motor.f), 1),
            'burn_s_5pct_peak': round(off - on, 2),
            'propellant_kg': round(motor.prop_kg, 2),
        },
        'mass_kg': {'dry': cfg['vehicle']['dry_mass_kg'], 'liftoff': round(truth.extra['mass_liftoff'], 2),
                    'burnout': round(truth.extra['mass_burnout'], 2)},
        'ignition_s': cfg['launch']['ignition_s'],
        'liftoff_s': round(ev['liftoff_s'], 3),
        'rail_exit_s': round(ev['rail_exit_s'], 3),
        'rail_exit_mps': round(ev['rail_exit_mps'], 1),
        'burnout_s': round(ev['burnout_s'], 3),
        'burnout_5pct_peak_s': round(cfg['launch']['ignition_s'] + off, 3),
        'max_speed_mps': round(truth.extra['max_speed'][0], 1),
        'max_speed_s': round(truth.extra['max_speed'][1], 2),
        'max_mach': round(truth.extra['max_mach'][0], 3),
        'max_boost_accel_g': round(float(fmag[boost].max()) / fs.G0, 2),
        'max_accel_any_g': round(float(fmag.max()) / fs.G0, 2),
        'max_imu_axis_mps2': round(float(np.abs(imu).max()), 2),
        'drogue_shock_g': peak(ev['drogue_s'], ev['drogue_s'] + 3.0),
        'main_shock_g': peak(ev['main_s'], ev['main_s'] + 5.0),
        'landing_shock_g': peak(ev['landing_s'], ev['end_s']),
        'apogee_s': round(ev['apogee_s'], 3),
        'apogee_m_agl': round(ev['apogee_m'], 1),
        'apogee_ft_agl': round(ev['apogee_m'] / 0.3048),
        'apogee_m_msl': round(ev['apogee_m'] + cfg['environment']['site_msl_m'], 1),
        'drogue_s': round(ev['drogue_s'], 3),
        'drogue_alt_m': round(ev['drogue_alt_m'], 1),
        'drogue_rate_mps': round(rate(ev['drogue_s'] + 15, ev['main_s'] - 2), 1),
        'main_trigger_s': round(ev['main_trigger_s'], 3),
        'main_s': round(ev['main_s'], 3),
        'main_alt_m': round(ev['main_alt_m'], 1),
        'main_rate_mps': round(rate(ev['landing_s'] - 20, ev['landing_s'] - 0.1), 1),
        'landing_s': round(ev['landing_s'], 3),
        'landing_mps': round(ev['landing_mps'], 2),
        'tipover_s': round(ev.get('tipover_s', ev['landing_s']), 3),
        'data_end_s': round(rows['IMU'][-1][0] / 1000.0, 3),
        'landing_drift_m': round(float(math.hypot(pos[land, 0], pos[land, 1])), 1),
        'landing_bearing_deg': round(math.degrees(math.atan2(pos[land, 0], pos[land, 1])) % 360, 1),
        'rows': {k: len(v) for k, v in rows.items()},
        'bytes': sizes,
        'total_bytes': sum(sizes.values()),
    }
    return s


def generate(cfg: dict, out: Path, seed: int, noise: bool, glitches=(), quiet=False) -> dict:
    t0 = time.time()
    motor = load_motor(cfg)
    truth = fs.fly(cfg, random.Random(seed), motor)
    rows = sensors.make_rows(cfg, truth, seed + 1000, noise, glitches)
    igt_csv.write_set(out, rows)
    env = cfg['environment']
    problems = igt_csv.check_set(out, igt_csv.Limits(env['site_lat_deg'], env['site_lon_deg'], env['site_msl_m']))
    for p in problems:
        print('IGT ' + p, file=sys.stderr)
    if problems:
        raise SystemExit(f'{out}: the FC would refuse this set')
    s = summarize(cfg, truth, motor, rows, out)
    s['seed'] = seed
    s['noise'] = noise
    s['site'] = [env['site_lat_deg'], env['site_lon_deg'], env['site_msl_m']]
    s['wind'] = {'mps': env['wind_mps'], 'from_deg': env['wind_from_deg'], 'gust_sigma_mps': env['gust_sigma_mps']}
    s['glitches'] = list(glitches)
    (out / 'summary.json').write_text(json.dumps(s, indent=2) + '\n')
    if not quiet:
        print(f"{out.name}: apogee {s['apogee_m_agl']:.0f} m ({s['apogee_ft_agl']} ft) at {s['apogee_s']:.1f} s, "
              f"Mach {s['max_mach']:.2f}, burnout {s['burnout_s']:.1f} s, drogue {s['drogue_s']:.1f} s, "
              f"main {s['main_s']:.1f} s, landed {s['landing_s']:.1f} s, drift {s['landing_drift_m']:.0f} m, "
              f"{s['total_bytes'] / 1e6:.2f} MB ({time.time() - t0:.0f} s)")
    return s


def build_variant(base: dict, v: dict, tuned: dict, seed_override=None) -> tuple[dict, int, list]:
    cfg = copy.deepcopy(base)
    for k, val in v.get('set', {}).items():
        set_path(cfg, k, val)
    if v.get('motor_from'):
        set_path(cfg, 'motor.total_impulse_ns', tuned[v['motor_from']])
    seed = seed_override if seed_override is not None else v.get('seed', 1)
    if v.get('tune'):
        tgt = v['tune']['apogee_m']
        imp = bisect(cfg, seed, 'motor.total_impulse_ns', 3000.0, 40000.0, tgt, True)
        if imp is None:
            raise SystemExit(f"{v['name']}: no impulse in 3-40 kN s reaches {tgt} m")
        set_path(cfg, 'motor.total_impulse_ns', round(imp, 1))
        tuned[v['name']] = round(imp, 1)
    return cfg, seed, v.get('glitches', [])


README_HEAD = """# IGT mock-flight datasets

Replay data for the integrated ground test (docs/integrated-ground-test.md):
each folder's `IGT/` directory goes to the root of the FC's SD card
(docs/integrated-ground-test.md section 11, "Using the committed mock flights").
Generated by `tools/flight_profile/generate.py --all` from
`tools/flight_profile/vehicle.json` (assumptions with sources) and
`tools/flight_profile/variants.json`; regenerating gives identical files.

t is seconds after T0, the FC's acceptance of Launch (ignition at 10 s, as on
the stand). Altitudes are above the pad; site {site}.
The motor is the 2026-11-16 static fire (DAQ thrust load cell, 7.05 kN s,
`tools/flight_profile/motors/testfire_20261116.json`). Reaching 10,000 ft
with the assumed {dry} kg dry mass needs about {imp10k:.1f} kN s, so the 10k
sets scale the measured curve (shape kept) by the square root of the impulse
ratio on both the thrust and the time axis (a larger tank with a somewhat
larger injector), and the propellant at the measured delivered Isp. `measured_motor` flies the measured motor
unscaled.

"""


def write_readme(summaries: dict, variants: list, base: dict, solved: dict, imp10k: float):
    lines = [README_HEAD.format(site=', '.join(str(x) for x in (base['environment']['site_lat_deg'],
                                                                   base['environment']['site_lon_deg'],
                                                                   base['environment']['site_msl_m'])) + ' m MSL',
                                dry=base['vehicle']['dry_mass_kg'],
                                imp10k=imp10k / 1000)]
    if solved:
        lines.append('Solved with the model (same seed):\n')
        for k, v in solved.items():
            lines.append(f'- {k}: {v}')
        lines.append('')
    lines.append('| | ' + ' | '.join(v['name'] for v in variants) + ' |')
    lines.append('|---|' + '---|' * len(variants))
    rows = [
        ('purpose', lambda s, v: v['description']),
        ('total impulse (kN s)', lambda s, v: f"{s['motor']['total_impulse_ns'] / 1000:.2f}" + (' scaled' if s['motor']['scaled'] else ' measured')),
        ('liftoff mass (kg)', lambda s, v: s['mass_kg']['liftoff']),
        ('wind (m/s from deg, gust sigma)', lambda s, v: f"{s['wind']['mps']} from {s['wind']['from_deg']}, {s['wind']['gust_sigma_mps']}"),
        ('liftoff (s)', lambda s, v: s['liftoff_s']),
        ('rail exit (s, m/s)', lambda s, v: f"{s['rail_exit_s']}, {s['rail_exit_mps']}"),
        ('burnout: thrust < 5% peak / zero (s)', lambda s, v: f"{s['burnout_5pct_peak_s']} / {s['burnout_s']}"),
        ('max speed (m/s at s)', lambda s, v: f"{s['max_speed_mps']} at {s['max_speed_s']}"),
        ('max Mach', lambda s, v: s['max_mach']),
        ('max boost accel (g)', lambda s, v: s['max_boost_accel_g']),
        ('shocks: drogue / main / landing+tip-over (g)', lambda s, v: f"{s['drogue_shock_g']} / {s['main_shock_g']} / {s['landing_shock_g']}"),
        ('apogee (m AGL / ft)', lambda s, v: f"{s['apogee_m_agl']} / {s['apogee_ft_agl']}"),
        ('apogee time (s)', lambda s, v: s['apogee_s']),
        ('drogue (s, m)', lambda s, v: f"{s['drogue_s']}, {s['drogue_alt_m']}"),
        ('drogue rate (m/s)', lambda s, v: s['drogue_rate_mps']),
        ('main (s, m)', lambda s, v: f"{s['main_s']}, {s['main_alt_m']}"),
        ('main rate (m/s)', lambda s, v: s['main_rate_mps']),
        ('landing (s, m/s)', lambda s, v: f"{s['landing_s']}, {s['landing_mps']}"),
        ('data end (s)', lambda s, v: s['data_end_s']),
        ('landing drift (m, bearing)', lambda s, v: f"{s['landing_drift_m']}, {s['landing_bearing_deg']}"),
        ('rows IMU/BARO/GPS', lambda s, v: '/'.join(str(s['rows'][k]) for k in ('IMU', 'BARO', 'GPS'))),
        ('size (kB)', lambda s, v: round(s['total_bytes'] / 1000)),
    ]
    for label, fn in rows:
        lines.append(f'| {label} | ' + ' | '.join(str(fn(summaries[v['name']], v)) for v in variants) + ' |')
    lines.append('')
    lines.append('Event times are the truth model\'s. The scripted deployments are what the FC is expected to '
                 'command (drogue about 1.2 s after apogee, main 0.3 s after crossing 457.2 m), except where a '
                 'variant says otherwise; the replay is open loop, so the FC\'s own pyro decisions do not change '
                 'the data. Each folder\'s summary.json has the full numbers, seed and glitch list.\n')
    for v in variants:
        if v.get('glitches') or v.get('notes'):
            lines.append(f"**{v['name']}**: {v.get('notes', '')}")
            for g in v.get('glitches', []):
                lines.append(f'- `{json.dumps(g)}`')
            lines.append('')
    (DATA / 'README.md').write_text('\n'.join(lines) + '\n')


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--config', type=Path, default=HERE / 'vehicle.json')
    ap.add_argument('--variants', type=Path, default=HERE / 'variants.json')
    ap.add_argument('--all', action='store_true', help='regenerate every dataset in variants.json under test-data/igt/')
    ap.add_argument('--variant', help='start from one variants.json entry')
    ap.add_argument('--out', type=Path)
    ap.add_argument('--seed', type=int)
    ap.add_argument('--no-noise', action='store_true', help='ideal sensors (truth, no noise/bias/vibration/dropouts)')
    ap.add_argument('--wind', type=float, help='mean wind at 10 m (m/s)')
    ap.add_argument('--wind-from', type=float, help='bearing the wind blows from (deg)')
    ap.add_argument('--gust', type=float, help='gust sigma (m/s); 0 = steady')
    ap.add_argument('--site', help='lat,lon,msl of the pad (must match the FC launch site setting)')
    ap.add_argument('--impulse', type=float, help='rescale the thrust curve to this total impulse (N s)')
    ap.add_argument('--apogee-m', type=float, help='rescale the impulse to reach this apogee')
    ap.add_argument('--solve-dry-mass', type=float, metavar='APOGEE_M', help='print the dry mass for this apogee and exit')
    ap.add_argument('--set', action='append', default=[], metavar='KEY=JSON', help='override any vehicle.json key, e.g. vehicle.dry_mass_kg=28')
    a = ap.parse_args(argv)

    base = json.loads(a.config.read_text())
    variants = json.loads(a.variants.read_text())['variants']

    def apply_cli(cfg):
        env = cfg['environment']
        if a.wind is not None:
            env['wind_mps'] = a.wind
        if a.wind_from is not None:
            env['wind_from_deg'] = a.wind_from
        if a.gust is not None:
            env['gust_sigma_mps'] = a.gust
        if a.site:
            env['site_lat_deg'], env['site_lon_deg'], env['site_msl_m'] = (float(x) for x in a.site.split(','))
        if a.impulse:
            cfg['motor']['total_impulse_ns'] = a.impulse
        for kv in a.set:
            k, _, v = kv.partition('=')
            set_path(cfg, k, json.loads(v))

    if a.solve_dry_mass:
        cfg = copy.deepcopy(base)
        apply_cli(cfg)
        seed = a.seed or 1
        m = bisect(cfg, seed, 'vehicle.dry_mass_kg', 1.0, 150.0, a.solve_dry_mass, False)
        print(f'dry mass for {a.solve_dry_mass:.0f} m: ' + (f'{m:.1f} kg' if m else 'not reachable in 1-150 kg'))
        return 0

    if a.all:
        tuned, summaries = {}, {}
        for v in variants:
            cfg, seed, gl = build_variant(base, v, tuned)
            summaries[v['name']] = generate(cfg, DATA / v['name'], seed, True, gl)
        solved = {}
        cfg = copy.deepcopy(base)
        m = bisect(cfg, 1, 'vehicle.dry_mass_kg', 1.0, 150.0, 3048.0, False)
        if m:
            solved['dry mass that reaches 3048 m with the measured motor'] = f'{m:.1f} kg'
        else:
            sweep = []
            for dry in (5.0, 10.0, 15.0, 20.0, 25.0, base['vehicle']['dry_mass_kg']):
                c = copy.deepcopy(base)
                c['vehicle']['dry_mass_kg'] = dry
                sweep.append((apogee(c, 1), dry))
            best = max(sweep)
            solved['dry mass that reaches 3048 m with the measured motor'] = (
                f'none: the measured motor peaks at about {best[0]:.0f} m near {best[1]:.0f} kg dry '
                '(lighter airframes are drag-limited); ' +
                ', '.join(f'{d:g} kg: {a:.0f} m' for a, d in sweep))
        imp10k = tuned.get('nominal_10k', float('nan'))
        solved['total impulse that reaches 3048 m with the assumed airframe (measured curve shape)'] = \
            f'{imp10k:.0f} N s'
        full = copy.deepcopy(base)
        full['motor']['total_impulse_ns'] = 7051.0 * 8.0 / 5.6
        solved['apogee with a full 8 kg N2O fill (impulse and propellant scaled by 8/5.6 = 10.1 kN s)'] = \
            f'{apogee(full, 1):.0f} m'
        write_readme(summaries, variants, base, solved, imp10k)
        return 0

    cfg, seed, gl = (copy.deepcopy(base), 1, [])
    tuned = {}
    if a.variant:
        chain = []
        v = next(x for x in variants if x['name'] == a.variant)
        if v.get('motor_from'):
            chain.append(next(x for x in variants if x['name'] == v['motor_from']))
        for x in chain:
            build_variant(base, x, tuned)
        cfg, seed, gl = build_variant(base, v, tuned, a.seed)
    if a.seed is not None:
        seed = a.seed
    apply_cli(cfg)
    if a.apogee_m:
        imp = bisect(cfg, seed, 'motor.total_impulse_ns', 3000.0, 40000.0, a.apogee_m, True)
        if imp is None:
            raise SystemExit('no impulse in 3-40 kN s reaches that apogee')
        cfg['motor']['total_impulse_ns'] = imp
    if not a.out:
        raise SystemExit('--out DIR (or --all)')
    generate(cfg, a.out, seed, not a.no_noise, gl)
    return 0


if __name__ == '__main__':
    sys.exit(main())
