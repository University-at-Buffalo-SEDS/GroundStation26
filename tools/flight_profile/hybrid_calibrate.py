#!/usr/bin/env python3
"""Calibrate the hybrid motor model (hybrid_motor.py) to the measured fires
(docs/hybrid-motor-model.md section 5).

Stage 1 (cross-check only): the 2026-10-31 fire is the only one with a
chamber PT. Its tank trace and chamber pressure give the throat that fire
implies (A_t / eta_c*, with that fire's own injector Cd and wall coupling:
it ran a different injector/plumbing setup; its thrust was not measured).
It is reported, not used: the throat it implies is far larger than the
11-16 thrust tail allows (docs/hybrid-motor-model.md section 5.3).

Stage 2 (motor): the accepted fires of the fire archive (the station's seeds
in backend/config/fire_archive_seed: today only the 2026-11-16 fire) fit
injector_cd, eta_cstar, wall_coupling, regression_scale and throat_d_m to
each fire's thrust curve, tank pressure trace (tank.csv, when the archive
has one) and total impulse, with log-normal priors from
motors/hybrid_motor.json. The station repeats
stage 2 on its own archive (backend/src/apogee_planner/hybrid_fit.rs) with
the same residuals, so new accepted fires refine the fit there without this
script; the planner reports the result.

Uncertainty: the Gauss-Newton covariance (J^T J)^-1 of the log-parameters,
scaled by the reduced chi-square when it exceeds 1 (the curve points are
correlated, so this is still optimistic; the planner adds the impulse
sigma of the fire itself).

  python3 tools/flight_profile/hybrid_calibrate.py          # fit, write 'calibrated' into motors/hybrid_motor.json
  python3 tools/flight_profile/hybrid_calibrate.py --check  # fit and print only
"""
from __future__ import annotations

import json
import math
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import hybrid_motor as hm  # noqa: E402

SEEDS = HERE.parent.parent / 'backend' / 'config' / 'fire_archive_seed'
RAW_1031 = HERE / 'motors' / 'raw' / 'fire_20261031_window.csv'
DT = 0.01
# The uncertain parameters only; the preset geometry (throat, injector holes, valve, grain) stays fixed.
FIT_PARAMS = ['injector_cd', 'eta_cstar', 'vapour_cd_scale', 'wall_coupling']
# Diagnostic fits (reported, not used): free the fill reading-to-mass factor and/or the throat, to test whether
# the fire matches the preset configuration.
DIAGNOSTICS = {'fill_scale_free': ['fill_scale'], 'throat_free': ['throat_d_m'], 'fill_and_throat_free': ['fill_scale', 'throat_d_m']}
# The regression law's scale is fitted only when a fire records the fuel it burned (grain weighed before and
# after): otherwise O/F trades against eta_c* and one thrust curve cannot tell them apart.
REGRESSION_PARAM = 'regression_scale'
ALT_START = {'injector_cd': 0.8, 'eta_cstar': 0.9, 'wall_coupling': 1.0, 'vapour_cd_scale': 2.0, 'fill_scale': 0.8,
             'throat_d_m': 0.035}
FUEL_SIGMA_KG = 0.05
THRUST_SIGMA_N = 150.0
THRUST_SIGMA_REL = 0.05
TANK_SIGMA_PSI = 10.0
THRUST_FROM_S = 0.65  # the 11-16 KG1000 is unreadable before this (an assumed ramp)
CURVE_STEP_S = 0.1
CHAMBER_OFFSET_PSI = 3.6  # the 10-31 chamber PT reads 3.6 psig at rest
CHAMBER_SIGMA_PSI = 3.0


def read_curve(path: Path) -> list[tuple[float, float]]:
    pts = []
    for line in path.read_text().splitlines():
        if not line or line.startswith('#') or line.startswith('t_s'):
            continue
        a, b = line.split(',')[:2]
        pts.append((float(a), float(b)))
    return pts


def accepted_fires(root: Path = SEEDS) -> list[dict]:
    fires = []
    for folder in sorted(root.iterdir()):
        rec = json.loads((folder / 'fire.json').read_text())
        rv = rec.get('review', {})
        if rv.get('status') != 'accepted' or rv.get('exclude_from_fit') or rec.get('simulated'):
            continue
        if not rec.get('fill_kg') or not rec.get('impulse_ns') or not (folder / 'thrust.csv').exists():
            continue
        fires.append({
            'id': rec['id'],
            'fill_kg': rec['fill_kg'],
            'tank_psi': rec.get('tank_psi_at_ignition'),
            'supercharge_psi': rec.get('supercharge_psi'),
            'ambient_c': rec.get('ambient_c'),
            'impulse_ns': rec['impulse_ns'],
            'impulse_sigma_ns': rec.get('impulse_sigma_ns') or 0.1 * rec['impulse_ns'],
            'thrust': read_curve(folder / 'thrust.csv'),
            'tank': read_curve(folder / 'tank.csv') if (folder / 'tank.csv').exists() else [],
            'fuel_used_kg': rec.get('fuel_used_kg'),
            'grain': rec.get('grain') or hm.load_spec()['fire_grain_default'],
        })
    return fires


def fire_conditions(f: dict) -> tuple[float, float]:
    """(tank temperature C, supercharge psig) for a fire: saturated at the ignition tank pressure unless a
    supercharge above the vapour pressure at ambient is recorded."""
    sc = f.get('supercharge_psi') or 0.0
    if f.get('tank_psi') is not None and (sc <= 0.0 or f.get('ambient_c') is None):
        return hm.tsat((f['tank_psi'] + 14.696) * hm.PSI) - 273.15, 0.0
    t = f.get('ambient_c') if f.get('ambient_c') is not None else 15.0
    return t, sc


def interp(ts, ys, t):
    if t <= ts[0]:
        return ys[0]
    if t >= ts[-1]:
        return ys[-1]
    lo, hi = 0, len(ts) - 1
    while hi - lo > 1:
        mid = (lo + hi) // 2
        if ts[mid] <= t:
            lo = mid
        else:
            hi = mid
    k = (t - ts[lo]) / (ts[hi] - ts[lo])
    return ys[lo] + (ys[hi] - ys[lo]) * k


def subsample(curve, t_from, step):
    out = []
    nxt = t_from
    for t, v in curve:
        if t + 1e-9 >= nxt:
            out.append((t, v))
            nxt = t + step - 1e-9
    return out


def fire_residuals(p, table, f, ambient_pa):
    tc, sc = fire_conditions(f)
    res = hm.simulate(p, table, f['fill_kg'], tc, sc, ambient_pa, DT)
    s = res['series']
    r = []
    for t, F in subsample(f['thrust'], THRUST_FROM_S, CURVE_STEP_S):
        r.append((interp(s['t'], s['thrust'], t) - F) / (THRUST_SIGMA_N + THRUST_SIGMA_REL * F))
    for t, P in subsample(f['tank'], 0.0, CURVE_STEP_S):
        r.append((interp(s['t'], s['p_tank'], t) / hm.PSI - 14.696 - P) / TANK_SIGMA_PSI)
    r.append((res['summary']['impulse_ns'] - f['impulse_ns']) / f['impulse_sigma_ns'])
    if f.get('fuel_used_kg'):
        r.append((res['summary']['fuel_used_kg'] - f['fuel_used_kg']) / FUEL_SIGMA_KG)
    return r, res


ETA_CSTAR_MAX = 0.95
ETA_BARRIER = 0.01


def prior_residuals(theta, prior):
    r = [(theta[k] - prior[k][0]) / prior[k][1] for k in theta]
    # c* efficiency cannot exceed 1: a soft wall above ETA_CSTAR_MAX (the fill_scale / eta trade-off is otherwise free)
    eta = math.exp(theta['eta_cstar']) if 'eta_cstar' in theta else 0.0
    r.append(max(eta - ETA_CSTAR_MAX, 0.0) / ETA_BARRIER)
    return r


def lm(fun, theta0: dict, iters=25, label=''):
    """Levenberg-Marquardt on a dict of log-parameters; fun(theta) -> residual list."""
    keys = list(theta0)
    th = dict(theta0)
    r = fun(th)
    cost = sum(x * x for x in r)
    lam = 1e-2
    for it in range(iters):
        jac = []
        for k in keys:
            h = 0.02
            t2 = dict(th)
            t2[k] += h
            r2 = fun(t2)
            jac.append([(b - a) / h for a, b in zip(r, r2)])
        n = len(keys)
        jtj = [[sum(jac[i][m] * jac[j][m] for m in range(len(r))) for j in range(n)] for i in range(n)]
        jtr = [sum(jac[i][m] * r[m] for m in range(len(r))) for i in range(n)]
        improved = False
        for _ in range(8):
            a = [[jtj[i][j] + (lam * jtj[i][i] if i == j else 0.0) for j in range(n)] for i in range(n)]
            step = solve(a, [-x for x in jtr])
            t2 = {k: th[k] + s for k, s in zip(keys, step)}
            r2 = fun(t2)
            c2 = sum(x * x for x in r2)
            if c2 < cost:
                th, r, lam = t2, r2, max(lam / 3, 1e-6)
                improved = cost - c2 > 1e-4 * cost
                cost = c2
                break
            lam *= 4
        print(f'{label} it {it} cost {cost:.2f} ' + ' '.join(f'{k}={math.exp(v):.4g}' for k, v in th.items()),
              file=sys.stderr)
        if not improved:
            break
    cov = invert(jtj)
    return th, r, cov, keys


def solve(a, b):
    n = len(b)
    m = [row[:] + [b[i]] for i, row in enumerate(a)]
    for c in range(n):
        piv = max(range(c, n), key=lambda i: abs(m[i][c]))
        m[c], m[piv] = m[piv], m[c]
        for i in range(c + 1, n):
            f = m[i][c] / m[c][c]
            for j in range(c, n + 1):
                m[i][j] -= f * m[c][j]
    x = [0.0] * n
    for i in range(n - 1, -1, -1):
        x[i] = (m[i][n] - sum(m[i][j] * x[j] for j in range(i + 1, n))) / m[i][i]
    return x


def invert(a):
    n = len(a)
    return [list(col) for col in zip(*[solve(a, [1.0 if i == j else 0.0 for i in range(n)]) for j in range(n)])]


def load_1031():
    ch = {}
    for line in RAW_1031.read_text().splitlines():
        if line.startswith('#') or line.startswith('t_s'):
            continue
        t, k, v = line.split(',')
        ch.setdefault(k, []).append((float(t), float(v)))

    def binned(pts, t0, t1, step=0.1):
        b = {}
        for t, v in pts:
            if t0 <= t < t1:
                b.setdefault(int((t - t0) / step), []).append(v)
        return [(t0 + (i + 0.5) * step, sorted(v)[len(v) // 2]) for i, v in sorted(b.items())]
    return {
        'fill_kg': 3.98,
        'tank_psi': 341.6,
        'tank': binned(ch['tank_psi'], 0.0, 9.0),
        'chamber': [(t, v - CHAMBER_OFFSET_PSI) for t, v in binned(ch['chamber_psig'], 0.3, 5.4)],
    }


def stage1(spec, table, base):
    """Throat: the 10-31 chamber and tank traces with that fire's own injector Cd and wall coupling."""
    f = load_1031()
    tc = hm.tsat((f['tank_psi'] + 14.696) * hm.PSI) - 273.15
    amb = base['test_ambient_pa']

    def fun(th):
        p = dict(base)
        p['injector_cd'] = math.exp(th['cd'])
        p['wall_coupling'] = math.exp(th['wall'])
        p['throat_d_m'] = math.exp(th['throat'])
        res = hm.simulate(p, table, f['fill_kg'], tc, 0.0, amb, DT)
        s = res['series']
        r = [(interp(s['t'], s['p_tank'], t) / hm.PSI - 14.696 - P) / TANK_SIGMA_PSI for t, P in f['tank']]
        r += [((interp(s['t'], s['pc'], t) - amb) / hm.PSI - P) / CHAMBER_SIGMA_PSI for t, P in f['chamber']]
        r += [(th['cd'] - math.log(0.65)) / 0.5, (th['wall'] - math.log(0.3)) / 1.0]
        return r
    th0 = {'cd': math.log(0.3), 'wall': math.log(0.3), 'throat': math.log(0.05)}
    th, r, cov, keys = lm(fun, th0, label='stage1')
    p = dict(base)
    p.update(injector_cd=math.exp(th['cd']), wall_coupling=math.exp(th['wall']), throat_d_m=math.exp(th['throat']))
    res = hm.simulate(p, table, f['fill_kg'], tc, 0.0, amb, DT)
    s = res['series']
    pc_rms = math.sqrt(sum(((interp(s['t'], s['pc'], t) - amb) / hm.PSI - P) ** 2 for t, P in f['chamber']) / len(f['chamber']))
    tank_rms = math.sqrt(sum((interp(s['t'], s['p_tank'], t) / hm.PSI - 14.696 - P) ** 2 for t, P in f['tank']) / len(f['tank']))
    k_at = math.pi / 4 * p['throat_d_m'] ** 2 / base['eta_cstar']
    sig_throat = math.sqrt(cov[keys.index('throat')][keys.index('throat')])
    return {
        'fire': '2026-10-31_static_fire',
        'throat_d_m': p['throat_d_m'],
        'throat_sigma_rel': sig_throat,
        'at_over_eta_m2': k_at,
        'eta_cstar_assumed': base['eta_cstar'],
        'injector_cd_1031': p['injector_cd'],
        'wall_coupling_1031': p['wall_coupling'],
        'chamber_rms_psi': pc_rms,
        'tank_rms_psi': tank_rms,
        'ox_used_kg': res['summary']['ox_used_kg'],
        'liquid_end_s': res['summary']['liquid_end_s'],
        'predicted_impulse_ns': res['summary']['impulse_ns'],
    }


def fit_params(fires, extra=()):
    return FIT_PARAMS + list(extra) + ([REGRESSION_PARAM] if any(f.get('fuel_used_kg') for f in fires) else [])


def stage2(spec, table, base, fires, extra=(), label='stage2'):
    names = fit_params(fires, extra)
    prior = {}
    for k in names:
        node = spec['geometry'].get(k) or spec['model'].get(k)
        prior[k] = (math.log(node['value']), node.get('sigma', 0.3 * node['value']) / node['value'])
    amb = base['test_ambient_pa']

    def params_for(th):
        p = dict(base)
        for k in names:
            p[k] = math.exp(th[k])
        return p

    def fun(th):
        p = params_for(th)
        r = []
        for f in fires:
            r += fire_residuals(dict(p, grain=hm.grain_config(spec, f['grain'])), table, f, amb)[0]
        return r + prior_residuals(th, prior)
    # Multi-start: the prior, and a high-flow / low-efficiency start (the 11-16 fire sits there); keep the best.
    best = None
    for start in ({}, ALT_START):
        th0 = {k: (math.log(start[k]) if k in start else prior[k][0]) for k in names}
        th, r, cov, keys = lm(fun, th0, label=label)
        cost = sum(x * x for x in r)
        if best is None or cost < best[0]:
            best = (cost, th, r, cov, keys)
    _, th, r, cov, keys = best
    p = params_for(th)
    ndata = len(r) - len(names) - 1
    chi2 = sum(x * x for x in r[:ndata])
    red = chi2 / max(ndata - len(names), 1)
    scale = max(red, 1.0)
    cov = [[c * scale for c in row] for row in cov]
    quality = []
    for f in fires:
        _, res = fire_residuals(dict(p, grain=hm.grain_config(spec, f['grain'])), table, f, amb)
        s, sm = res['series'], res['summary']
        pts = subsample(f['thrust'], THRUST_FROM_S, CURVE_STEP_S)
        f_rms = math.sqrt(sum((interp(s['t'], s['thrust'], t) - F) ** 2 for t, F in pts) / len(pts))
        tank = subsample(f['tank'], 0.0, CURVE_STEP_S)
        t_rms = math.sqrt(sum((interp(s['t'], s['p_tank'], t) / hm.PSI - 14.696 - P) ** 2 for t, P in tank) / len(tank)) if tank else None
        meas_on = next(t for t, F in f['thrust'] if F >= 0.05 * max(x for _, x in f['thrust']))
        meas_off = next(t for t, F in reversed(f['thrust']) if F >= 0.05 * max(x for _, x in f['thrust']))
        quality.append({
            'fire': f['id'],
            'impulse_ns': round(sm['impulse_ns'], 1),
            'measured_impulse_ns': f['impulse_ns'],
            'impulse_error_pct': round(100 * (sm['impulse_ns'] / f['impulse_ns'] - 1), 2),
            'burn_time_s': round(sm['burn_time_s'], 3),
            'measured_burn_time_s': round(meas_off - meas_on, 3),
            'peak_thrust_n': round(sm['peak_thrust_n'], 1),
            'thrust_rms_n': round(f_rms, 1),
            'tank_rms_psi': round(t_rms, 2) if t_rms is not None else None,
            'liquid_end_s': round(sm['liquid_end_s'], 3),
            'peak_pc_psia': round(sm['peak_pc_pa'] / hm.PSI, 1),
            'mean_of': round(sm['mean_of'], 3),
            'fuel_used_kg': round(sm['fuel_used_kg'], 3),
            'isp_s': round(sm['isp_s'], 1),
            'ox_used_kg': round(sm['ox_used_kg'], 3),
            'grain': f['grain'],
        })
    return p, {k: math.exp(th[k]) for k in names}, keys, cov, red, quality


def main(argv):
    spec = hm.load_spec()
    table = hm.load_data()
    spec_clean = json.loads(json.dumps(spec))
    spec_clean['calibrated'] = {'values': {}}
    base = hm.params(spec_clean, grain=spec['fire_grain_default'])
    s1 = stage1(spec, table, base)
    fires = accepted_fires()
    p, vals, keys, cov, red, quality = stage2(spec, table, base, fires)
    diagnostics = {}
    for name, extra in DIAGNOSTICS.items():
        _, dv, dk, dc, dred, dq = stage2(spec, table, base, fires, extra, label=name)
        diagnostics[name] = {'values': {k: round(v, 6) for k, v in dv.items()},
                             'sigma_rel': {k: round(math.sqrt(dc[i][i]), 4) for i, k in enumerate(dk)},
                             'reduced_chi2': round(dred, 3), 'quality': dq}
    # prior-only prediction (the preset as given) for the record
    pp = dict(base)
    preset_q = []
    for f in fires:
        _, res = fire_residuals(dict(pp, grain=hm.grain_config(spec, f['grain'])), table, f, base['test_ambient_pa'])
        preset_q.append({'fire': f['id'], **{k: round(v, 3) for k, v in res['summary'].items()}})
    values = dict(vals)
    out = {
        'values': {k: round(v, 6) for k, v in values.items()},
        'log_covariance': {'params': keys, 'matrix': [[round(c, 8) for c in row] for row in cov]},
        'sigma_rel': {k: round(math.sqrt(cov[i][i]), 4) for i, k in enumerate(keys)},
        'reduced_chi2': round(red, 3),
        'fires': [f['id'] for f in fires],
        'quality': quality,
        'stage1_throat': {k: (round(v, 6) if isinstance(v, float) else v) for k, v in s1.items()},
        'preset_prediction': preset_q,
        'diagnostics': diagnostics,
        'settings': {'dt_s': DT, 'thrust_sigma': f'{THRUST_SIGMA_N} N + {THRUST_SIGMA_REL * 100:.0f} %',
                     'tank_sigma_psi': TANK_SIGMA_PSI, 'thrust_from_s': THRUST_FROM_S, 'curve_step_s': CURVE_STEP_S},
        'note': 'written by hybrid_calibrate.py; the station refits stage 2 on its own archive (hybrid_fit.rs)',
    }
    print(json.dumps(out, indent=1))
    if '--check' not in argv:
        spec['calibrated'] = out
        hm.SPEC_PATH.write_text(json.dumps(spec, indent=2) + '\n')
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
