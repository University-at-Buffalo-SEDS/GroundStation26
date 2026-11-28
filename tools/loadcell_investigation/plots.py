#!/usr/bin/env python3
"""Figures for docs/loadcell-investigation-2026-11.md.
Usage: plots.py [--mcp-vref V] NPZ_DIR OUT_DIR [FIGURE...]   (FIGURE: a function name below; default all)
--mcp-vref: the board's real MCP3564R reference for MCP-derived volts (KG1000, KG50). The
recordings were converted with the nominal 2.4 V (the default); the fire-campaign DAQ's
reference measures about 2.2 V, so its volts read about 9 % high. Codes, over range and
kg calibrations are ratiometric and do not depend on it."""
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

# MCP3564R codes. KG1000 arrives on the legacy raw scale (code * 2.2104 / 2^27), so
# ADC-pin volts = code * 2.4 / 2^23. With DATA_FORMAT = 11 the ADC reports over range
# (datasheet DS20006391C §5.6, Tables 5-7/5-8): |code| > 2^23 - 1 means the input is
# beyond +/-VREF, accuracy is only kept to about +/-1.05 VREF, and there is no status bit.
# Such samples are not measurements. The 0xB40000 plateau ("3.375 V") is one of them.
CODE_FS = 2 ** 23
LEGACY_PER_CODE = 2.2104 / 2 ** 27
OVER_RANGE_LABEL = 'KG1000 over range (ADC)'
# Firmware of the time converted the STM32 ADC1 VMON channel assuming VDDA = +3V3A = 3.3 V.
VDDA_ASSUMED = 3.3
# Reference the recordings' MCP volts were converted with, and the one to plot them at.
MCP_VREF_RECORDED = 2.4
MCP_VREF = MCP_VREF_RECORDED

def mcp_v(x):
    """MCP-derived volts rescaled from the recorded 2.4 V reference to MCP_VREF."""
    return np.asarray(x, float) * (MCP_VREF / MCP_VREF_RECORDED)

def kg1000_code(raw):
    return np.asarray(raw, float) / LEGACY_PER_CODE

def over_range(code, flag=None):
    """True where an MCP3564R sample is over range: |code| > 2^23 - 1, a published value of
    +/-Inf (the new pipeline's marker), or a set over-range flag. NaN (no sample) is False."""
    code = np.asarray(code, float)
    m = np.abs(np.nan_to_num(code, nan=0.0)) > CODE_FS - 1
    if flag is not None:
        m |= np.nan_to_num(np.asarray(flag, float), nan=0.0) != 0
    return m

def runs(mask):
    """(start, stop) index pairs of the True runs in mask, stop exclusive."""
    d = np.diff(np.concatenate(([0], np.asarray(mask, int), [0])))
    return list(zip(np.flatnonzero(d == 1), np.flatnonzero(d == -1)))

def spans(x, mask):
    """x intervals covered by the True runs: from the first sample of a run to the sample
    after it. Across a telemetry gap (or at the end of the data) the band stops one median
    sample interval after the run's last sample, so a gap is not shown as over range."""
    x = np.asarray(x, float)
    dt = float(np.median(np.diff(x))) if len(x) > 1 else 0.0
    return [(x[a], x[b] if b < len(x) and x[b] - x[b - 1] <= 4 * dt else x[b - 1] + dt) for a, b in runs(mask)]

def shade(ax, intervals, label=None, strong=True):
    """Hatched translucent band over each interval; only the first one carries the label."""
    for i, (a, b) in enumerate(intervals):
        ax.axvspan(a, b, facecolor=RED, alpha=0.13 if strong else 0.07, hatch='///' if strong else None,
                   edgecolor=RED, lw=0, label=label if i == 0 else None)

def without(y, mask):
    """y with masked samples replaced by NaN, so the line breaks there instead of plotting them."""
    return np.where(mask, np.nan, y)

def panels(n, title, h=1.6):
    fig, ax = plt.subplots(n, 1, sharex=True, figsize=(8, h * n + 0.6))
    fig.suptitle(title, x=0.01, ha='left', fontsize=10, color=INK)
    return fig, ax

# Burn start in the 11-16 fire recording: 9.2 s after the first KG1000 sample, where DAQ
# VMAIN returns to 14.2 V and the ox tank starts emptying (report section 2.3).
FIRE_T0_OFFSET_MS = 9200

def fire_ignition(d, out):
    """Ignition window: KG1000, KG50, DAQ VMAIN and the gateway (GB) battery rail in
    separate panels on one t - T0 axis. KG1000 over-range samples are not plotted; their
    intervals are shaded in every panel, and DAQ VMAIN inside them is marked as the VDDA
    artifact (the gateway battery is the reference). GB BATTERY_VOLTAGE is the gateway LTC2990 V1
    through its 91k/10k divider (volts); BATTERY_CURRENT is its LTC2990 current channel
    (amps). Both arrive at about 1 Hz. Also writes every plotted sample of the window to
    fire-20261116-ignition.csv (wide, full resolution, t_s = t - T0; see write_wide_csv):
    kg1000_adc_pin_v, kg50_p6_input_mv, vmain_v, gb_battery_v, gb_battery_a and
    ox_tank_psi. kg1000_adc_pin_v keeps the over-range samples the figure leaves out (their
    linear value, e.g. the 3.375 V plateau; over range where |code| > 2^23 - 1). The 2.4 V
    full-scale line is a constant and not a CSV column."""
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
    m1 = win(t1); ov = over_range(kg1000_code(k1)); band = spans(rel(t1)[m1], ov[m1])
    ax[0].plot(rel(t1)[m1], without(mcp_v(k1[m1] * K), ov[m1]), color=BLUE); ax[0].axhline(MCP_VREF, color=RED, lw=0.8, ls='--')
    ax[0].set_ylabel(f'DAQ KG1000\nADC-pin V\n(VREF {MCP_VREF:g} V)')
    ax[0].text(lo_s + 0.2, MCP_VREF + 0.05, f'ADC full scale = VREF = {MCP_VREF:g} V', color=RED, fontsize=8)
    ax[0].set_ylim(0, 2.9)
    ax[0].text(lo_s + 0.2, 1.2, f'over range: code > 2^23−1 (plateau 0xB40000 = 1.40625 × full scale, '
               f'"{1.40625 * MCP_VREF:.3f} V"),\nan overloaded ADC code, not a reading; not plotted', fontsize=8, color=RED)
    t5, k5 = s(z, 'DAQ|KG50'); m5 = win(t5)
    ax[1].plot(rel(t5)[m5], mcp_v(k5[m5]) * 1000, color=ORANGE); ax[1].set_ylabel('DAQ KG50 (P6)\ninput mV')
    tp, vm = s(z, 'DAQ|DAQ_POWER_MONITOR', 2); mp = win(tp)
    ax[2].plot(rel(tp)[mp], vm[mp], color=AQUA, marker='.', ms=2, lw=0.8); ax[2].set_ylabel('DAQ VMAIN V\n(4 Hz)')
    tg, vg = s(z, 'GB|BATTERY_VOLTAGE'); mg = win(tg)
    ax[3].plot(rel(tg)[mg], vg[mg], color=INK, marker='o', ms=3, lw=0.8); ax[3].set_ylabel('Gateway (GB)\nbattery V (1 Hz)')
    # Same volts per division as the VMAIN panel, so a dip of equal size looks equal.
    lo, hi = ax[2].get_ylim(); off = np.nanmedian(vg[mg]) - np.nanmedian(vm[mp & (rel(tp) > 0)])
    ax[3].set_ylim(lo + off, hi + off)
    ax[3].text(lo_s + 0.2, lo + off + 0.15 * (hi - lo), f'GB {np.nanmin(vg[mg]):.2f}–{np.nanmax(vg[mg]):.2f} V in this window; '
               'same V/div as VMAIN panel; the battery reference', fontsize=8, color=MUTED)
    # VMAIN read through a lifted VDDA: implied +3V3A = 3.3 V x battery / VMAIN read.
    pre = mp & (rel(tp) < 0)
    vdda = VDDA_ASSUMED * np.nanmedian(vg[mg & (rel(tg) < 0)]) / np.nanmedian(vm[pre])
    ax[2].text(lo_s + 0.2, lo + 0.35 * (hi - lo), f'DAQ VMAIN during KG1000 over range is a VDDA\n'
               f'artifact: +3V3A lifted by the D24 clamp,\nimplied ≈ {vdda:.2f} V (3.3 V × GB / VMAIN)', fontsize=8, color=RED)
    ti, ig = s(z, 'GB|BATTERY_CURRENT'); mi = win(ti)
    ax[4].plot(rel(ti)[mi], ig[mi], color=MUTED, marker='o', ms=3, lw=0.8); ax[4].set_ylabel('Gateway (GB)\nbattery A (1 Hz)')
    ax[4].set_ylim(0, max(1.0, np.nanmax(ig[mi]) * 1.3))
    tf, fp = s(z, 'VB|OX_TANK_PRESSURE'); mf = win(tf)
    ax[5].plot(rel(tf)[mf], fp[mf], color=YELLOW); ax[5].set_ylabel('Ox tank psi')
    ax[5].set_xlabel('t − T0 (s)')
    ax[5].set_xlim(lo_s, hi_s)
    ax[5].xaxis.set_major_locator(MultipleLocator(1)); ax[5].xaxis.set_minor_locator(MultipleLocator(0.25))
    for i, a in enumerate(ax):
        shade(a, band, OVER_RANGE_LABEL if i == 0 else None, strong=i in (0, 2))
        a.axvline(0, color=RED, lw=0.9, ls=':')
        a.tick_params(axis='x', which='both', labelbottom=True, labelsize=7.5)
    ax[0].legend(fontsize=7.5, loc='upper right', frameon=False)
    ax[0].text(0, 1.01, 'T0', transform=ax[0].get_xaxis_transform(), ha='center', va='bottom', fontsize=8, color=RED)
    ax[5].text(0.2, 300, 'KG1000 back in range, VMAIN\nartifact ends / burn starts', fontsize=8, color=MUTED)
    fig.tight_layout(); fig.savefig(os.path.join(out, 'fire-20261116-ignition.png'), dpi=110); plt.close(fig)
    write_wide_csv(os.path.join(out, 'fire-20261116-ignition.csv'), T0,
                   [('kg1000_adc_pin_v', t1[m1], mcp_v(k1[m1] * K)), ('kg50_p6_input_mv', t5[m5], mcp_v(k5[m5]) * 1000),
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

def battery_reference(ax, z, x):
    """Gateway battery points on a VMAIN panel, when the recording has them."""
    if 'GB|BATTERY_VOLTAGE__t' in z.files:
        tg, vg = s(z, 'GB|BATTERY_VOLTAGE')
        ax.plot(x(tg), vg, color=INK, ls='none', marker='o', ms=1.6, label='gateway battery (reference)')

def kg1000_log_panels(z, title, n):
    """Shared top of the minute-scale figures: KG1000 (log, over range removed and shaded)
    and DAQ VMAIN with the gateway battery as reference."""
    t1, k1 = s(z, 'DAQ|KG1000'); t0 = t1[0]
    mins = lambda t: (t - t0) / 60000.0
    ov = over_range(kg1000_code(k1)); band = spans(mins(t1), ov)
    fig, ax = panels(n, title)
    ax[0].plot(mins(t1), without(mcp_v(k1 * K), ov), color=BLUE, lw=0.6); ax[0].set_ylabel(f'KG1000 V\n(VREF {MCP_VREF:g} V)')
    ax[0].set_yscale('log'); ax[0].axhline(MCP_VREF, color=RED, lw=0.8, ls='--')
    tp, vm = s(z, 'DAQ|DAQ_POWER_MONITOR', 2)
    ax[1].plot(mins(tp), vm, color=AQUA, lw=0.6, label='DAQ VMAIN'); ax[1].set_ylabel('VMAIN V')
    battery_reference(ax[1], z, mins)
    ax[1].set_title('VMAIN "dips" occur with KG1000 over range: +3V3A (STM32 VDDA) lifted by the D24 clamp, '
                    'not a supply dip', loc='left', fontsize=7.5, color=RED)
    for i, a in enumerate(ax):
        shade(a, band, OVER_RANGE_LABEL if i == 0 else None, strong=i < 2)
    ax[0].legend(fontsize=7, loc='lower right', frameon=False); ax[1].legend(fontsize=7, loc='lower right', frameon=False)
    return fig, ax, mins

def fire_overview(d, out):
    """Whole 11-16 fire recording: KG1000, DAQ VMAIN, KG50 and the ADC die temperature on
    one minutes axis from the first KG1000 sample. Also writes every plotted sample to
    fire-20261116-overview.csv (wide, full resolution; see write_wide_csv): kg1000_adc_pin_v
    (including the over-range samples the figure leaves out), vmain_v, kg50_p6_input_mv
    (only after 0.4 min, as plotted; the panel's y-limits 3.6-3.95 mV clip a few points
    that are still in the CSV) and adc_die_temp_c; MCP volts at --mcp-vref, as plotted. The dashed 0.2126 V
    calibrated-zero line is a constant and not a CSV column."""
    z = load(d, '2026-11-17_00-59-46_065')
    fig, ax, mins = kg1000_log_panels(z, '2026-11-16 test fire, whole recording (28 min)', 4)
    z0 = float(mcp_v(0.2126)); ax[0].axhline(z0, color=MUTED, lw=0.8, ls='--')
    ax[0].text(20, 0.3, f'calibrated zero {z0:.3f} V', fontsize=8, color=MUTED)
    t5, k5 = s(z, 'DAQ|KG50'); g = mins(t5) > 0.4
    ax[2].plot(mins(t5)[g], mcp_v(k5[g]) * 1000, color=ORANGE, lw=0.6); ax[2].set_ylabel('KG50 input mV')
    ax[2].set_ylim(*mcp_v([3.6, 3.95]))
    tt, tc = s(z, 'DAQ|DAQ_ADC_TEMPERATURE')
    ax[3].plot(mins(tt), tc, color=YELLOW, lw=0.8); ax[3].set_ylabel('ADC die °C'); ax[3].set_xlabel('minutes')
    fig.tight_layout(); fig.savefig(os.path.join(out, 'fire-20261116-overview.png'), dpi=110); plt.close(fig)
    t1, k1 = s(z, 'DAQ|KG1000')
    tp, vm = s(z, 'DAQ|DAQ_POWER_MONITOR', 2)
    write_wide_csv(os.path.join(out, 'fire-20261116-overview.csv'), t1[0],
                   [('kg1000_adc_pin_v', t1, mcp_v(k1 * K)), ('vmain_v', tp, vm),
                    ('kg50_p6_input_mv', t5[g], mcp_v(k5[g]) * 1000), ('adc_die_temp_c', tt, tc)])

def rail_1106(d, out):
    z = load(d, '2026-11-06_22-51-01_544')
    fig, ax, mins = kg1000_log_panels(z, '2026-11-06: DAQ VMAIN dips only with KG1000 over range; '
                                      'no telemetry 10.9–38 min', 2)
    ax[1].set_xlabel('minutes')
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
    args = sys.argv[1:]
    if args[:1] == ['--mcp-vref']:
        MCP_VREF = float(args[1]); args = args[2:]
    d, out = args[0], args[1]
    os.makedirs(out, exist_ok=True)
    figs = {'fire_ignition': lambda: fire_ignition(d, out), 'fire_overview': lambda: fire_overview(d, out),
            'rail_1106': lambda: rail_1106(d, out), 'calibration_steps': lambda: calibration_steps(out),
            'kg50_cross_check': lambda: kg50_cross_check(d, out)}
    for name in args[2:] or figs:
        figs[name]()
