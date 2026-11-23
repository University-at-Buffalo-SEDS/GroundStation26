#!/usr/bin/env python3
"""Train the learned residual on top of the hybrid motor physics (docs/hybrid-motor-model.md section 6).

Grey box: the physics model (hybrid_motor.py, calibrated) predicts each accepted fire; the residual learns
y = ln(I_measured / I_physics) from the features (fill reading kg, supercharge psig, tank C, initial port
area cm^2) with a Gaussian process (squared-exponential kernel, one length scale per feature). This script
fits the GP hyperparameters by maximising the marginal likelihood (Nelder-Mead in log space, CPU, seconds)
and writes them as residual_model.json. Copy that file to the station's
<data>/apogee_planner/residual_model.json; the station (hybrid_fit.rs) refits the GP on its own archive with
these hyperparameters, and uses it only once it has 3 or more accepted fires. With fewer fires this script
refuses (the defaults stand).

The interface on the station (hybrid_fit.rs `ResidualModel`) maps features to (mean, variance). A small
neural network trained here (on jupiter's GPU if ever worthwhile) would export the same way: plain JSON
weights plus an ensemble or MC-dropout variance, evaluated in Rust with no extra dependencies on the Pi.
Public paraffin/N2O firing data should enter only as a prior on the regression law in motors/hybrid_motor.json,
never as training rows here (geometry- and injector-specific).

  python3 tools/flight_profile/hybrid_residual.py [--archive <data>/fire_archive] [--out residual_model.json]
"""
from __future__ import annotations

import argparse
import json
import math
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import hybrid_calibrate as hc  # noqa: E402
import hybrid_motor as hm  # noqa: E402

MIN_FIRES = 3
DEFAULT = {'length_scales': [2.0, 250.0, 10.0, 8.0], 'signal_sigma': 0.08, 'noise_floor': 0.02}


def points(archive: Path):
    spec = hm.load_spec()
    data = hm.load_data()
    fires = hc.accepted_fires(archive)
    out = []
    for f in fires:
        p = hm.params(spec, grain=f['grain'])
        tc, sc = hc.fire_conditions(f)
        res = hm.simulate(p, data, f['fill_kg'], tc, sc, p['test_ambient_pa'], 0.01)
        area0 = data.grain(p['grain']).at(0.0)[0]
        x = [f['fill_kg'], sc, tc, area0 * 1e4]
        y = math.log(f['impulse_ns'] / res['summary']['impulse_ns'])
        out.append((x, y, (f['impulse_sigma_ns'] / f['impulse_ns']) ** 2, f['id']))
    return out


def nll(theta, pts):
    ls = [math.exp(v) for v in theta[:4]]
    s2 = math.exp(2 * theta[4])
    nf2 = math.exp(2 * theta[5])
    n = len(pts)
    k = [[s2 * math.exp(-0.5 * sum(((a - b) / l) ** 2 for a, b, l in zip(pi[0], pj[0], ls))) for pj in pts] for pi in pts]
    for i, p in enumerate(pts):
        k[i][i] += p[2] + nf2
    lch = [[0.0] * n for _ in range(n)]
    for i in range(n):
        for j in range(i + 1):
            s = sum(lch[i][m] * lch[j][m] for m in range(j))
            if i == j:
                d = k[i][i] - s
                if d <= 0:
                    return 1e9
                lch[i][i] = math.sqrt(d)
            else:
                lch[i][j] = (k[i][j] - s) / lch[j][j]
    y = [p[1] for p in pts]
    z = []
    for i in range(n):
        z.append((y[i] - sum(lch[i][j] * z[j] for j in range(i))) / lch[i][i])
    return 0.5 * sum(v * v for v in z) + sum(math.log(lch[i][i]) for i in range(n))


def nelder_mead(f, x0, iters=400, step=0.5):
    pts = [list(x0)] + [[x + (step if i == j else 0.0) for j, x in enumerate(x0)] for i in range(len(x0))]
    vals = [f(p) for p in pts]
    for _ in range(iters):
        order = sorted(range(len(pts)), key=lambda i: vals[i])
        pts, vals = [pts[i] for i in order], [vals[i] for i in order]
        c = [sum(p[j] for p in pts[:-1]) / (len(pts) - 1) for j in range(len(x0))]
        r = [c[j] + (c[j] - pts[-1][j]) for j in range(len(x0))]
        fr = f(r)
        if fr < vals[0]:
            e = [c[j] + 2 * (c[j] - pts[-1][j]) for j in range(len(x0))]
            fe = f(e)
            pts[-1], vals[-1] = (e, fe) if fe < fr else (r, fr)
        elif fr < vals[-2]:
            pts[-1], vals[-1] = r, fr
        else:
            cc = [c[j] + 0.5 * (pts[-1][j] - c[j]) for j in range(len(x0))]
            fc = f(cc)
            if fc < vals[-1]:
                pts[-1], vals[-1] = cc, fc
            else:
                pts = [pts[0]] + [[pts[0][j] + 0.5 * (p[j] - pts[0][j]) for j in range(len(x0))] for p in pts[1:]]
                vals = [vals[0]] + [f(p) for p in pts[1:]]
    i = min(range(len(pts)), key=lambda k: vals[k])
    return pts[i], vals[i]


def main(argv):
    ap = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    ap.add_argument('--archive', type=Path, default=hc.SEEDS, help='a fire_archive directory (default: the seeds)')
    ap.add_argument('--out', type=Path, default=Path('residual_model.json'))
    a = ap.parse_args(argv)
    pts = points(a.archive)
    print(f'{len(pts)} accepted fire(s):', [(p[3], round(p[1], 4)) for p in pts])
    if len(pts) < MIN_FIRES:
        print(f'need {MIN_FIRES} accepted fires to train the residual; the station keeps it off and uses the defaults')
        return 1
    x0 = [math.log(v) for v in DEFAULT['length_scales']] + [math.log(DEFAULT['signal_sigma']), math.log(DEFAULT['noise_floor'])]
    prior = lambda th: sum(((t - x) / 1.0) ** 2 for t, x in zip(th, x0)) * 0.5  # noqa: E731  weak log-normal prior
    th, v = nelder_mead(lambda t: nll(t, pts) + prior(t), x0)
    out = {'kind': 'gp_se_ard', 'length_scales': [math.exp(t) for t in th[:4]], 'signal_sigma': math.exp(th[4]),
           'noise_floor': math.exp(th[5]), 'source': f'hybrid_residual.py on {len(pts)} fires: {[p[3] for p in pts]}'}
    a.out.write_text(json.dumps(out, indent=1) + '\n')
    print(json.dumps(out, indent=1))
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
