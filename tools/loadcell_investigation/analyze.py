#!/usr/bin/env python3
"""Load-cell investigation: per-recording statistics from extract.py .npz files.

Usage: analyze.py OUTDIR NPZ...
Writes OUTDIR/summary.json and OUTDIR/summary.csv.

Units: KG1000 arrives on the legacy raw scale (code*2.2104/2^27); ADC-pin volts
= raw * 16000/921. KG50 in every recording tracks the P6 bridge
(CH2-CH3, also scanned single-ended at 10 Hz) and is P6 input volts; kg figures
use the GS calibration slope in effect in that recording.
"""
import json, sys, os
import numpy as np

K = 16000.0 / 921.0
KG1000_KG_PER_V = 9481.6 / K          # slope in force at every fire, per ADC volt
KG50_P6_KG_PER_V = 22799.137
TOP_RAW, FLOOR_RAW = 0.19, 0.0025      # KG1000 clamp / amplifier floor (legacy raw)

def series(z, key):
    if key + '__t' not in z.files:
        return None, None
    return z[key + '__t'], z[key + '__v']

def eff_cal(t_raw, raw, t_kg, kg):
    n = min(len(raw), len(kg))
    same = t_raw[:n] == t_kg[:n]
    x, y = raw[:n, 0][same], kg[:n, 0][same]
    ok = np.isfinite(x) & np.isfinite(y)
    if ok.sum() < 10 or np.ptp(x[ok]) == 0:
        return None
    m, b = np.polyfit(x[ok], y[ok], 1)
    return float(m), float(b)

def quiet_mask(t, z, pad_ms=20000):
    """True where no valve state changed within pad_ms and no supply dip."""
    bad = np.zeros(len(t), bool)
    for key in ('AB|VALVE_STATE', 'VB|VALVE_STATE'):
        tv, vv = series(z, key)
        if tv is None:
            continue
        vv = np.nan_to_num(vv[:, :7], nan=-1)
        ch = np.flatnonzero(np.any(np.diff(vv, axis=0) != 0, axis=1)) + 1
        for c in tv[ch]:
            bad |= np.abs(t - c) < pad_ms
    tp, pm = series(z, 'DAQ|DAQ_POWER_MONITOR')
    if tp is not None:
        for c in tp[pm[:, 2] < 12.5]:
            bad |= np.abs(t - c) < pad_ms
    return ~bad

def segments(mask, t, min_ms):
    out, start = [], None
    for i, m in enumerate(np.append(mask, False)):
        if m and start is None:
            start = i
        elif not m and start is not None:
            if t[i - 1] - t[start] >= min_ms:
                out.append((start, i))
            start = None
    return out

def stats_channel(t, v, quiet, kg_per_unit, rails=None):
    ok = np.isfinite(v) & quiet
    if rails:
        ok &= (v > rails[0]) & (v < rails[1])
    res = {'quiet_fraction': float(ok.mean())}
    segs = segments(ok, t, 120000)
    drifts, noise, noise1s = [], [], []
    for a, b in segs:
        tt, vv = (t[a:b] - t[a]) / 60000.0, v[a:b]
        g = np.isfinite(vv)
        if g.sum() < 100:
            continue
        p = np.polyfit(tt[g], vv[g], 1)
        drifts.append(p[0] * kg_per_unit)
        noise.append(np.std(np.diff(vv[g])) / np.sqrt(2) * kg_per_unit)
        sec = (t[a:b] // 1000)
        u, inv = np.unique(sec, return_inverse=True)
        means = np.bincount(inv, np.nan_to_num(vv)) / np.bincount(inv)
        noise1s.append(np.std(np.diff(means)) / np.sqrt(2) * kg_per_unit)
    if drifts:
        res.update(quiet_segments=len(drifts),
                   drift_kg_per_min_median=float(np.median(np.abs(drifts))),
                   drift_kg_per_min_max=float(np.max(np.abs(drifts))),
                   noise_kg_sample=float(np.median(noise)),
                   noise_kg_1s=float(np.median(noise1s)))
    return res

def gaps(t, factor=5.0):
    d = np.diff(t)
    med = np.median(d)
    return int((d > max(factor * med, 200)).sum()), float(d.max() / 1000.0)

def corr_with(t, v, z, key, col, quiet):
    tx, xv = series(z, key)
    if tx is None:
        return None
    sec = t // 10000
    u, inv = np.unique(sec[quiet], return_inverse=True)
    if len(u) < 10:
        return None
    vm = np.bincount(inv, v[quiet]) / np.bincount(inv)
    xs = tx // 10000
    xm = np.array([np.nanmean(xv[xs == s, col]) if np.any(xs == s) else np.nan for s in u])
    g = np.isfinite(vm) & np.isfinite(xm)
    if g.sum() < 10 or np.std(xm[g]) == 0 or np.std(vm[g]) == 0:
        return None
    return float(np.corrcoef(vm[g], xm[g])[0, 1])

def analyze(path):
    z = np.load(path)
    name = os.path.basename(path)[len('groundstation_recording_'):-4]
    t1, k1 = series(z, 'DAQ|KG1000')
    t5, k5 = series(z, 'DAQ|KG50')
    _, w1 = series(z, 'DAQ|LOADCELL_WEIGHT_KG')
    _, w5 = series(z, 'DAQ|LOADCELL_50_WEIGHT_KG')
    tp, pm = series(z, 'DAQ|DAQ_POWER_MONITOR')
    tf, fp = series(z, 'VB|OX_TANK_PRESSURE')
    r = {'recording': name, 'minutes': float((t1[-1] - t1[0]) / 60000.0),
         'rate_hz': float(len(t1) / max(1, (t1[-1] - t1[0]) / 1000.0))}
    r['vmain_min'] = float(np.nanmin(pm[:, 2])) if tp is not None else None
    r['vmain_median'] = float(np.nanmedian(pm[:, 2])) if tp is not None else None
    r['tank_psi_max'] = float(np.nanmax(fp)) if tf is not None else None
    tt, tc = series(z, 'DAQ|DAQ_ADC_TEMPERATURE')
    r['adc_temp_range_c'] = [float(np.nanmin(tc)), float(np.nanmax(tc))] if tt is not None else None
    # KG1000
    c1 = eff_cal(t1, k1, *series(z, 'DAQ|LOADCELL_WEIGHT_KG'))
    v1 = k1[:, 0] * K
    q1 = quiet_mask(t1, z)
    r['kg1000'] = {
        'gs_calibration_kg_per_raw': c1, 'median_v': float(np.nanmedian(v1)),
        'top_rail_samples': int((k1[:, 0] >= TOP_RAW).sum()),
        'floor_rail_samples': int((k1[:, 0] <= FLOOR_RAW).sum()),
        'gaps': gaps(t1),
        **stats_channel(t1, v1, q1, KG1000_KG_PER_V, (FLOOR_RAW * K, TOP_RAW * K)),
        'corr_10s_adc_temp': corr_with(t1, v1, z, 'DAQ|DAQ_ADC_TEMPERATURE', 0, q1 & np.isfinite(v1)),
        'corr_10s_vmain': corr_with(t1, v1, z, 'DAQ|DAQ_POWER_MONITOR', 2, q1 & np.isfinite(v1)),
    }
    if t5 is not None:
        c5 = eff_cal(t5, k5, *series(z, 'DAQ|LOADCELL_50_WEIGHT_KG'))
        v5 = k5[:, 0]
        med = float(np.nanmedian(v5))
        # P6 lines are also scanned single-ended (CH2, CH3) in the 10 Hz aux pass.
        ts, sv = series(z, 'DAQ|DAQ_SDADC_VOLTAGES')
        p6 = {}
        if ts is not None:
            se = sv[:, 0] - sv[:, 1]
            cm = (sv[:, 0] + sv[:, 1]) / 2
            p6 = {'p6_common_mode_v': float(np.nanmedian(cm)), 'p6_single_ended_diff_v': float(np.nanmedian(se)),
                  'kg50_over_single_ended_diff': float(med / np.nanmedian(se))}
        per = c5[0] if c5 else KG50_P6_KG_PER_V
        q5 = quiet_mask(t5, z)
        r['kg50'] = {'gs_calibration_kg_per_v': c5, 'median_v': med, **p6,
                     'gaps': gaps(t5),
                     **stats_channel(t5, v5, q5, per),
                     'corr_10s_adc_temp': corr_with(t5, v5, z, 'DAQ|DAQ_ADC_TEMPERATURE', 0, q5),
                     'corr_10s_vmain': corr_with(t5, v5, z, 'DAQ|DAQ_POWER_MONITOR', 2, q5)}
    return r

def main():
    out = sys.argv[1]
    os.makedirs(out, exist_ok=True)
    rows = [analyze(p) for p in sys.argv[2:]]
    json.dump(rows, open(os.path.join(out, 'summary.json'), 'w'), indent=1)
    cols = ['recording', 'minutes', 'vmain_min', 'tank_psi_max']
    with open(os.path.join(out, 'summary.csv'), 'w') as f:
        f.write(','.join(cols + ['kg1000_median_v', 'kg1000_top', 'kg1000_floor', 'kg1000_noise_kg', 'kg1000_noise1s_kg', 'kg1000_drift_kg_min',
                                 'kg50_median_mv', 'kg50_gs_kg_per_v', 'p6_cm_v', 'kg50_over_se_diff', 'kg50_noise_kg', 'kg50_noise1s_kg', 'kg50_drift_kg_min', 'kg50_corr_temp']) + '\n')
        for r in rows:
            a, b = r['kg1000'], r.get('kg50', {})
            vals = [r[c] for c in cols] + [a['median_v'], a['top_rail_samples'], a['floor_rail_samples'], a.get('noise_kg_sample'), a.get('noise_kg_1s'), a.get('drift_kg_per_min_median'),
                    b.get('median_v', float('nan')) * 1000 if b else None, (b.get('gs_calibration_kg_per_v') or [None])[0] if b else None, b.get('p6_common_mode_v'), b.get('kg50_over_single_ended_diff'), b.get('noise_kg_sample'), b.get('noise_kg_1s'), b.get('drift_kg_per_min_median'), b.get('corr_10s_adc_temp')]
            f.write(','.join('' if v is None else (f'{v:.4g}' if isinstance(v, float) else str(v)) for v in vals) + '\n')
    print(open(os.path.join(out, 'summary.csv')).read())

if __name__ == '__main__':
    main()
