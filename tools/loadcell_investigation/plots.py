#!/usr/bin/env python3
"""Figures for docs/loadcell-investigation-2026-11.md.
Usage: plots.py NPZ_DIR OUT_DIR [FIGURE...]   (FIGURE: a function name below; default all)"""
import sys, os
import numpy as np
import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt

K = 16000.0 / 921.0
BLUE, ORANGE, AQUA, YELLOW, RED, INK, MUTED = '#2a78d6', '#eb6834', '#1baf7a', '#eda100', '#e34948', '#0b0b0b', '#52514e'
plt.rcParams.update({'font.size': 9, 'axes.edgecolor': MUTED, 'axes.labelcolor': INK, 'xtick.color': MUTED,
                     'ytick.color': MUTED, 'axes.grid': True, 'grid.color': '#e6e5e1', 'grid.linewidth': 0.6,
                     'axes.spines.top': False, 'axes.spines.right': False, 'lines.linewidth': 1.2,
                     'figure.facecolor': '#fcfcfb', 'axes.facecolor': '#fcfcfb'})

def load(d, name):
    return np.load(os.path.join(d, f'groundstation_recording_{name}.npz'))

def s(z, k, col=0):
    return z[k + '__t'], z[k + '__v'][:, col]

def panels(n, title, h=1.6):
    fig, ax = plt.subplots(n, 1, sharex=True, figsize=(8, h * n + 0.6))
    fig.suptitle(title, x=0.01, ha='left', fontsize=10, color=INK)
    return fig, ax

# Burn start in the 11-16 fire recording: 9.2 s after the first KG1000 sample, where DAQ
# VMAIN returns to 14.2 V and the ox tank starts emptying (report section 2.3).
FIRE_T0_OFFSET_MS = 9200

def fire_ignition(d, out):
    """Ignition window: KG1000, KG50, DAQ VMAIN and the gateway (GB) battery rail in
    separate panels on one t - T0 axis. GB BATTERY_VOLTAGE is the gateway LTC2990 V1
    through its 91k/10k divider (volts); BATTERY_CURRENT is its LTC2990 current channel
    (amps). Both arrive at about 1 Hz. Also writes every plotted sample of the window to
    fire-20261116-ignition.csv (wide, full resolution, t_s = t - T0; see write_wide_csv):
    kg1000_adc_pin_v, kg50_p6_input_mv, vmain_v, gb_battery_v, gb_battery_a and
    ox_tank_psi. The 2.4 V full-scale line is a constant and not a CSV column."""
    from datetime import datetime, timezone
    from matplotlib.ticker import MultipleLocator
    z = load(d, '2026-11-17_00-59-46_065')
    t1, k1 = s(z, 'DAQ|KG1000'); T0 = t1[0] + FIRE_T0_OFFSET_MS
    rel = lambda t: (t - T0) / 1000.0
    lo_s, hi_s = -FIRE_T0_OFFSET_MS / 1000.0, 25 - FIRE_T0_OFFSET_MS / 1000.0
    win = lambda t: (rel(t) >= lo_s) & (rel(t) < hi_s)
    t0_utc = datetime.fromtimestamp(T0 / 1000, timezone.utc).strftime('%Y-%m-%d %H:%M:%S.%f')[:-3]
    fig, ax = plt.subplots(6, 1, sharex=True, figsize=(11, 1.55 * 6 + 0.9))
    fig.suptitle(f'2026-11-16 test fire, ignition window. T0 = burn start = {t0_utc} UTC '
                 f'(recording 2026-11-17 00:59:46 UTC + {FIRE_T0_OFFSET_MS / 1000:g} s)',
                 x=0.01, ha='left', fontsize=10, color=INK)
    m1 = win(t1)
    ax[0].plot(rel(t1)[m1], k1[m1] * K, color=BLUE); ax[0].axhline(2.4, color=RED, lw=0.8, ls='--')
    ax[0].set_ylabel('DAQ KG1000\nADC-pin V'); ax[0].text(lo_s + 0.2, 2.55, 'ADC full scale 2.4 V', color=RED, fontsize=8)
    t5, k5 = s(z, 'DAQ|KG50'); m5 = win(t5)
    ax[1].plot(rel(t5)[m5], k5[m5] * 1000, color=ORANGE); ax[1].set_ylabel('DAQ KG50 (P6)\ninput mV')
    tp, vm = s(z, 'DAQ|DAQ_POWER_MONITOR', 2); mp = win(tp)
    ax[2].plot(rel(tp)[mp], vm[mp], color=AQUA, marker='.', ms=2, lw=0.8); ax[2].set_ylabel('DAQ VMAIN V\n(4 Hz)')
    tg, vg = s(z, 'GB|BATTERY_VOLTAGE'); mg = win(tg)
    ax[3].plot(rel(tg)[mg], vg[mg], color=INK, marker='o', ms=3, lw=0.8); ax[3].set_ylabel('Gateway (GB)\nbattery V (1 Hz)')
    # Same volts per division as the VMAIN panel, so a dip of equal size looks equal.
    lo, hi = ax[2].get_ylim(); off = np.nanmedian(vg[mg]) - np.nanmedian(vm[mp & (rel(tp) > 0)])
    ax[3].set_ylim(lo + off, hi + off)
    ax[3].text(lo_s + 0.2, lo + off + 0.15 * (hi - lo), f'GB {np.nanmin(vg[mg]):.2f}–{np.nanmax(vg[mg]):.2f} V in this window; '
               'same V/div as VMAIN panel', fontsize=8, color=MUTED)
    ti, ig = s(z, 'GB|BATTERY_CURRENT'); mi = win(ti)
    ax[4].plot(rel(ti)[mi], ig[mi], color=MUTED, marker='o', ms=3, lw=0.8); ax[4].set_ylabel('Gateway (GB)\nbattery A (1 Hz)')
    ax[4].set_ylim(0, max(1.0, np.nanmax(ig[mi]) * 1.3))
    tf, fp = s(z, 'VB|OX_TANK_PRESSURE'); mf = win(tf)
    ax[5].plot(rel(tf)[mf], fp[mf], color=YELLOW); ax[5].set_ylabel('Ox tank psi')
    ax[5].set_xlabel('t − T0 (s)')
    ax[5].set_xlim(lo_s, hi_s)
    ax[5].xaxis.set_major_locator(MultipleLocator(1)); ax[5].xaxis.set_minor_locator(MultipleLocator(0.25))
    for a in ax:
        a.axvline(0, color=RED, lw=0.9, ls=':')
        a.tick_params(axis='x', which='both', labelbottom=True, labelsize=7.5)
    ax[0].text(0, 1.01, 'T0', transform=ax[0].get_xaxis_transform(), ha='center', va='bottom', fontsize=8, color=RED)
    ax[5].text(0.2, 300, 'supply restored /\nburn starts', fontsize=8, color=MUTED)
    fig.tight_layout(); fig.savefig(os.path.join(out, 'fire-20261116-ignition.png'), dpi=110); plt.close(fig)
    write_wide_csv(os.path.join(out, 'fire-20261116-ignition.csv'), T0,
                   [('kg1000_adc_pin_v', t1[m1], k1[m1] * K), ('kg50_p6_input_mv', t5[m5], k5[m5] * 1000),
                    ('vmain_v', tp[mp], vm[mp]), ('gb_battery_v', tg[mg], vg[mg]),
                    ('gb_battery_a', ti[mi], ig[mi]), ('ox_tank_psi', tf[mf], fp[mf])])

def write_wide_csv(path, t_ref, series):
    """Write series of different rates to one wide CSV: a row per receive timestamp
    (t_s relative to t_ref, utc_ms, utc_iso) and a column per series, blank where that
    series has no sample at that timestamp. series: [(column, t_ms, values)]. A series
    with several samples in the same millisecond gets one row per sample, so no sample
    is merged or dropped."""
    import csv
    from datetime import datetime, timezone
    rows = {}
    for j, (_, t, v) in enumerate(series):
        occ = {}
        for tt, vv in zip(t.tolist(), v.tolist()):
            n = occ.get(tt, 0); occ[tt] = n + 1
            rows.setdefault((tt, n), [''] * len(series))[j] = '' if vv != vv else f'{vv:.7g}'
    with open(path, 'w', newline='') as f:
        w = csv.writer(f, lineterminator='\n')
        w.writerow(['t_s', 'utc_ms', 'utc_iso'] + [c for c, _, _ in series])
        for (tt, _), vals in sorted(rows.items()):
            iso = datetime.fromtimestamp(tt / 1000, timezone.utc).strftime('%Y-%m-%dT%H:%M:%S.%f')[:-3] + 'Z'
            w.writerow([f'{(tt - t_ref) / 1000:.3f}', tt, iso] + vals)

def fire_overview(d, out):
    """Whole 11-16 fire recording: KG1000, DAQ VMAIN, KG50 and the ADC die temperature on
    one minutes axis from the first KG1000 sample. Also writes every plotted sample to
    fire-20261116-overview.csv (wide, full resolution; see write_wide_csv): kg1000_adc_pin_v,
    vmain_v, kg50_p6_input_mv (only after 0.4 min, as plotted; the panel's y-limits
    3.6-3.95 mV clip a few points that are still in the CSV) and adc_die_temp_c. The
    dashed 0.2126 V calibrated-zero line is a constant and not a CSV column."""
    z = load(d, '2026-11-17_00-59-46_065')
    t1, k1 = s(z, 'DAQ|KG1000'); t0 = t1[0]
    mins = lambda t: (t - t0) / 60000.0
    fig, ax = panels(4, '2026-11-16 test fire, whole recording (28 min)')
    ax[0].plot(mins(t1), k1 * K, color=BLUE, lw=0.6); ax[0].set_ylabel('KG1000 V'); ax[0].set_yscale('log')
    ax[0].axhline(0.2126, color=MUTED, lw=0.8, ls='--'); ax[0].text(20, 0.3, 'calibrated zero 0.213 V', fontsize=8, color=MUTED)
    tp, vm = s(z, 'DAQ|DAQ_POWER_MONITOR', 2)
    ax[1].plot(mins(tp), vm, color=AQUA, lw=0.6); ax[1].set_ylabel('VMAIN V')
    t5, k5 = s(z, 'DAQ|KG50'); g = mins(t5) > 0.4
    ax[2].plot(mins(t5)[g], k5[g] * 1000, color=ORANGE, lw=0.6); ax[2].set_ylabel('KG50 input mV'); ax[2].set_ylim(3.6, 3.95)
    tt, tc = s(z, 'DAQ|DAQ_ADC_TEMPERATURE')
    ax[3].plot(mins(tt), tc, color=YELLOW, lw=0.8); ax[3].set_ylabel('ADC die °C'); ax[3].set_xlabel('minutes')
    fig.tight_layout(); fig.savefig(os.path.join(out, 'fire-20261116-overview.png'), dpi=110); plt.close(fig)
    write_wide_csv(os.path.join(out, 'fire-20261116-overview.csv'), t0,
                   [('kg1000_adc_pin_v', t1, k1 * K), ('vmain_v', tp, vm),
                    ('kg50_p6_input_mv', t5[g], k5[g] * 1000), ('adc_die_temp_c', tt, tc)])

def rail_1106(d, out):
    z = load(d, '2026-11-06_22-51-01_544')
    t1, k1 = s(z, 'DAQ|KG1000'); t0 = t1[0]
    mins = lambda t: (t - t0) / 60000.0
    fig, ax = panels(2, '2026-11-06 17:51 local: KG1000 rails with supply dips and keeps a +16 kg zero shift')
    ax[0].plot(mins(t1), k1 * K, color=BLUE, lw=0.6); ax[0].set_ylabel('KG1000 V'); ax[0].set_yscale('log')
    tp, vm = s(z, 'DAQ|DAQ_POWER_MONITOR', 2)
    ax[1].plot(mins(tp), vm, color=AQUA, lw=0.6); ax[1].set_ylabel('VMAIN V'); ax[1].set_xlabel('minutes')
    fig.tight_layout(); fig.savefig(os.path.join(out, 'rail-20261106.png'), dpi=110); plt.close(fig)

def calibration_steps(out):
    # Points from the live GroundStation calibration (GET /api/loadcell_calibration, 2026-11-22), ADC-pin / input volts.
    kg1000 = [(20.4, 0.24977598), (40.8, 0.28683203), (61.2, 0.32496646), (81.6, 0.36300135), (102.0, 0.398651)]
    kg50 = [(5.0, 0.004191), (10.0, 0.004523), (15.0, 0.004628), (20.0, 0.004947)]  # temperature_captures (unshifted)
    fig, ax = plt.subplots(1, 2, figsize=(8, 2.8))
    fig.suptitle('Sensitivity of each calibration step relative to the whole-fit slope', x=0.01, ha='left', fontsize=10)
    for a, pts, name, color in ((ax[0], kg1000, 'KG1000 (20.4 kg steps)', BLUE), (ax[1], kg50, 'KG50 on P6 (5 kg steps)', ORANGE)):
        x = np.array([p[1] for p in pts]); y = np.array([p[0] for p in pts])
        m, b = np.polyfit(x, y, 1)
        r2 = 1 - np.sum((y - (m * x + b)) ** 2) / np.sum((y - y.mean()) ** 2)
        ratio = (np.diff(x) * m) / np.diff(y)
        labels = [f'{y[i]:g}→{y[i+1]:g}' for i in range(len(y) - 1)]
        a.bar(labels, ratio, color=color, width=0.5); a.axhline(1, color=MUTED, lw=0.8, ls='--')
        a.set_ylim(0, 1.8); a.set_title(f'{name}, R² = {r2:.4f}', fontsize=9)
        for i, v in enumerate(ratio): a.text(i, v + 0.04, f'{v:.2f}', ha='center', fontsize=8)
    ax[0].set_ylabel('step sensitivity / fit slope')
    fig.tight_layout(); fig.savefig(os.path.join(out, 'calibration-steps.png'), dpi=110); plt.close(fig)

def kg50_cross_check(d, out):
    fig, ax = plt.subplots(1, 2, figsize=(8, 2.8))
    fig.suptitle('KG50 telemetry vs the same P6 bridge scanned single-ended (CH2-CH3, 10 Hz)', x=0.01, ha='left', fontsize=10)
    for a, name, title in ((ax[0], '2026-10-30_23-23-48_776', '2026-10-30 site, N2 fill (bridge CM 1.138 V)'),
                           (ax[1], '2026-11-09_13-12-38_375', '2026-11-09 bench (bridge CM 0.169 V)')):
        z = load(d, name)
        t5, k5 = s(z, 'DAQ|KG50'); t0 = t5[0]
        ts = z['DAQ|DAQ_SDADC_VOLTAGES__t']; sv = z['DAQ|DAQ_SDADC_VOLTAGES__v']
        a.plot((t5 - t0) / 60000, k5 * 1000, color=ORANGE, lw=0.7, label='KG50 telemetry (P6, PGA 16)')
        a.plot((ts - t0) / 60000, (sv[:, 0] - sv[:, 1]) * 1000 * 0.88, color=BLUE, lw=0.9, label='0.88 × (CH2 − CH3) single-ended')
        a.set_title(title, fontsize=9); a.set_xlabel('minutes'); a.set_ylabel('mV')
    ax[0].legend(fontsize=7, loc='upper right', frameon=False)
    fig.tight_layout(); fig.savefig(os.path.join(out, 'kg50-p6-cross-check.png'), dpi=110); plt.close(fig)

if __name__ == '__main__':
    d, out = sys.argv[1], sys.argv[2]
    os.makedirs(out, exist_ok=True)
    figs = {'fire_ignition': lambda: fire_ignition(d, out), 'fire_overview': lambda: fire_overview(d, out),
            'rail_1106': lambda: rail_1106(d, out), 'calibration_steps': lambda: calibration_steps(out),
            'kg50_cross_check': lambda: kg50_cross_check(d, out)}
    for name in sys.argv[3:] or figs:
        figs[name]()
