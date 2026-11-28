#!/usr/bin/env python3
"""Thrust curve from the static-fire load cells, including the KG50 unloading.

The rocket hangs from the 50 kg fill cell (KG50). Thrust first unloads KG50;
only once it exceeds the hanging weight does the rocket lift, close the gap
and press the 1000 kg thrust cell (KG1000). With R(t) the KG50 reading, R_pre
its value just before ignition, F(t) the KG1000 reading (kg) and burned(t)
the propellant mass already expelled:

    thrust(t) = g * (F(t) + (R_pre - R(t)) - burned(t))

This holds on both sides of lift-off: while the rocket hangs, F = 0 and KG50
reads m(t) - T/g; while it is lifted, KG50 reads its zero-load value and KG1000
reads T/g - m(t). Only KG50 differences enter, so its tare does not matter
(its slope does). burned(t) = (R_pre - R_post) * impulse fraction(t), solved by
iteration; R_post is the settled post-burn reading, so the curve ends at zero.

The gap-closing transient (rocket moving between the cells, neither carrying
it) and the drop-back onto KG50 at tail-off are where the formula is not
valid: both are short and flagged in the output, and the binned/smoothed curve
spreads them out (their net impulse is about zero: the momentum gained in the
gap is returned at contact).

Input: motors/raw/fire_20261116_window.csv (the fire window of the GS
recording, values as recorded). Output: the thrust CSV and a JSON summary.

  python3 tools/flight_profile/extract_thrust.py            # writes motors/testfire_20261116.{csv,json:'kg50'}
  python3 tools/flight_profile/extract_thrust.py --check    # print only
"""
from __future__ import annotations

import argparse
import csv
import json
import statistics as st
import sys
from pathlib import Path

G = 9.80665
HERE = Path(__file__).resolve().parent
RAW = HERE / 'motors' / 'raw' / 'fire_20261116_window.csv'
# KG50 slope in the recording vs the earlier fire-night fit (docs/loadcell-investigation-2026-11.md 2.2):
KG50_RECORDED_SLOPE = 22799.0
KG50_ALTERNATE_SLOPE = 12915.0
DT = 0.05


def load(path: Path = RAW) -> dict[str, list[tuple[float, float]]]:
    out: dict[str, list[tuple[float, float]]] = {}
    with path.open() as fh:
        for row in csv.reader(fh):
            if not row or row[0].startswith('#') or row[0] == 't_s':
                continue
            out.setdefault(row[1], []).append((float(row[0]), float(row[2])))
    return out


def med(xs):
    return st.median(xs) if xs else float('nan')


def rolling_median(pts, n=5):
    h = n // 2
    return [(pts[i][0], st.median(v for _, v in pts[max(0, i - h):i + h + 1])) for i in range(len(pts))]


def bins(pts, t0, t1):
    b: dict[int, list[float]] = {}
    for t, v in pts:
        if t0 <= t <= t1:
            b.setdefault(round(t / DT), []).append(v)
    return sorted((k * DT, st.mean(v)) for k, v in b.items())


def interp(pts, t):
    if t <= pts[0][0]:
        return pts[0][1]
    for (a, va), (b, vb) in zip(pts, pts[1:]):
        if a <= t <= b:
            return va + (vb - va) * (t - a) / (b - a) if b > a else va
    return pts[-1][1]


def ma(y, n):
    h = n // 2
    return [st.mean(y[max(0, i - h):i + h + 1]) for i in range(len(y))]


def kg1000_series(raw, start_mode='nominal'):
    """KG1000 (kg) on the DT grid, the 2026-11-16 method (testfire_20261116.json method 1)."""
    pts = [(t, v) for t, v in raw['kg1000_kg'] if -50 < v < 600]
    base = med([v for t, v in pts if 7.0 <= t <= 11.0])
    good = rolling_median([(t, v - base) for t, v in pts if -1.0 <= t <= 12.0])
    good = [(t, v) for t, v in good if t >= 0.65 and not (2.59 <= t <= 2.83)]
    meas = bins(good, 0.65, 12.0)
    grid = [round(i * DT, 3) for i in range(0, int(12.0 / DT) + 1)]
    y = [interp(meas, t) for t in grid]
    ys = ma(ma(y, 3), 3)
    first = ys[grid.index(0.65)]
    out = []
    for t, v in zip(grid, ys):
        if t < 0.65:
            if start_mode == 'nominal':
                v = 0.0 if t <= 0.10 else (first * min(1.0, (t - 0.10) / 0.20))
            elif start_mode == 'low':
                v = 0.0 if t < 0.55 else first * (t - 0.55) / 0.10
            else:
                v = 0.0 if t <= 0.05 else 1.15 * first * min(1.0, (t - 0.05) / 0.10)
        out.append((t, v))
    # The cell no longer carries the rocket once it is back on KG50 (KG1000 at its baseline).
    end = next(t for t, v in out if t > 3 and v < 0.5)
    return [(t, v if t < end else 0.0) for t, v in out], base, end


def kg50_series(raw):
    """KG50 (kg) on the DT grid, spikes removed, clipped to the physical range [zero-load, pre-burn]."""
    pts = raw['kg50_kg']
    pre = med([v for t, v in pts if -3.0 <= t <= -0.2])
    post = med([v for t, v in pts if 12.0 <= t <= 20.0])
    lifted = med([v for t, v in pts if 1.0 <= t <= 4.5])
    clean = rolling_median([(t, v) for t, v in pts if -1.0 <= t <= 12.0], 5)
    meas = bins(clean, -1.0, 12.0)
    grid = [round(i * DT, 3) for i in range(0, int(12.0 / DT) + 1)]
    y = [min(pre, max(lifted, interp(meas, t))) for t in grid]
    return list(zip(grid, y)), pre, post, lifted


def combine(f1000, kg50, pre, post, kg50_scale=1.0, iterations=4):
    """thrust (N) = g * (F + s*(R_pre - R) - burned), burned = s*(R_pre - R_post) * impulse fraction."""
    total_burned = kg50_scale * (pre - post)
    frac = [0.0] * len(f1000)
    thrust = []
    for _ in range(iterations):
        thrust = [max(0.0, G * (f + kg50_scale * (pre - r) - total_burned * fr))
                  for (t, f), (_, r), fr in zip(f1000, kg50, frac)]
        cum = [0.0]
        for i in range(1, len(thrust)):
            cum.append(cum[-1] + 0.5 * (thrust[i] + thrust[i - 1]) * DT)
        frac = [c / cum[-1] for c in cum]
    return [(t, v) for (t, _), v in zip(f1000, thrust)]


def trim(curve):
    """Thrust to zero once it stays below 1% of peak (KG50 noise floor), keeping one zero."""
    peak = max(v for _, v in curve)
    last = max(i for i, (_, v) in enumerate(curve) if v >= 0.01 * peak)
    out = curve[:last + 1] + [(round(curve[last][0] + DT, 3), 0.0)]
    return [(0.0, 0.0)] + [p for p in out if p[0] > 0]


def stats(curve):
    imp = sum((b[0] - a[0]) * (a[1] + b[1]) / 2 for a, b in zip(curve, curve[1:]))
    pk = max(curve, key=lambda x: x[1])
    ts = [t for t, v in curve if v >= 0.05 * pk[1]]
    burn = ts[-1] - ts[0]
    return {'total_impulse_ns': round(imp, 1), 'peak_thrust_n': round(pk[1], 1), 't_peak_s': pk[0],
            'burn_time_5pct_s': round(burn, 2), 'burn_end_zero_s': curve[-1][0],
            'average_thrust_n': round(imp / burn, 1)}


def extract(path: Path = RAW):
    raw = load(path)
    res = {}
    k50, pre, post, lifted = kg50_series(raw)
    scale_alt = KG50_ALTERNATE_SLOPE / KG50_RECORDED_SLOPE
    curves = {}
    for name, mode, scale in [('nominal', 'nominal', 1.0), ('low_startup', 'low', 1.0),
                              ('high_startup', 'high', 1.0), ('kg50_alternate_slope', 'nominal', scale_alt),
                              ('kg1000_only', 'nominal', 0.0)]:
        f, base, end = kg1000_series(raw, mode)
        c = trim(combine(f, k50, pre, post, scale))
        curves[name] = c
        res[name] = stats(c)
    kg1000_imp = res['kg1000_only']['total_impulse_ns']
    summary = {
        'kg50_pre_burn_kg': round(pre, 2), 'kg50_lifted_plateau_kg': round(lifted, 2),
        'kg50_post_burn_kg': round(post, 2),
        'hanging_mass_at_ignition_kg': round(pre - lifted, 2),
        'mass_lost_in_burn_kg': round(pre - post, 2),
        'kg1000_end_s': end, 'kg1000_baseline_kg': round(base, 2),
        'variants': res,
        'kg50_term_ns': round(res['nominal']['total_impulse_ns'] - kg1000_imp, 1),
        'quality_flags': quality(raw, pre, lifted),
    }
    return curves, summary


def quality(raw, pre, lifted):
    k50 = raw['kg50_kg']
    spikes = [t for t, v in k50 if 0 <= t <= 6 and (v < lifted - 5 or v > pre + 5)]
    k1k = raw['kg1000_kg']
    rail = [t for t, v in k1k if -1 <= t <= 7 and not (-50 < v < 600)]
    lift_t = next((t for t, v in rolling_median([p for p in k50 if p[0] >= 0]) if v < lifted + 2), None)
    drop = next((t for t, v in rolling_median([p for p in k50 if p[0] >= 4]) if v > lifted + 2), None)
    return {
        'kg50_spikes_during_burn': len(spikes),
        'kg50_spike_note': 'supply-dip/ground signature (docs/loadcell-investigation-2026-11.md 2.3); removed by the rolling median',
        'kg1000_rail_or_open_lead_samples': len(rail),
        'kg1000_startup_gap_s': [0.0, 0.65],
        'lift_off_kg50_s': lift_t,
        'gap_closing_transient_s': [lift_t, 0.65] if lift_t is not None else None,
        'drop_back_onto_kg50_s': drop,
        'kg50_slope_unverified': True,
        'kg50_pre_burn_during_supply_sag': True,
    }


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    ap.add_argument('--check', action='store_true', help='print the result, write nothing')
    ap.add_argument('--out', type=Path, default=HERE / 'motors' / 'testfire_20261116.csv')
    a = ap.parse_args(argv)
    curves, summary = extract()
    json.dump(summary, sys.stdout, indent=1)
    print()
    if a.check:
        return 0
    with a.out.open() as fh:
        header = [line for line in fh if line.startswith('#')]
    with a.out.open('w') as fh:
        fh.writelines(header)
        fh.write('t_s,thrust_n\n')
        for t, v in curves['nominal']:
            fh.write(f'{t:.3f},{v:.1f}\n')
    return 0


if __name__ == '__main__':
    sys.exit(main())
