"""Sensor models: the truth trajectory (flightsim.Truth, 1 ms) to the FC's
IGT CSV rows (IMU, BARO, GPS) with datasheet noise, bias, quantization,
saturation, baro port lag/IIR, GPS error and dropouts, and optional injected
glitches for the stress variants.

Glitches (variants.json "glitches"): a list of
  {"sensor": "baro"|"imu"|"gps", "kind": ..., "at": T, "dur_s": D, ...}
where T is seconds after T0, or "<event>+<s>" / "<event>-<s>" with an event
from the truth (liftoff_s, burnout_s, apogee_s, drogue_s, main_s, landing_s).
  baro  spike       "pa": added pressure (positive reads lower altitude)
  baro  stuck       output held at its value at T
  baro  noise       "scale": extra white noise, multiple of the nominal sigma
  baro  outliers    "pa", "prob": isolated one-sample outliers of +-pa
  imu   spike       "accel_mps2": [x,y,z] and/or "gyro_dps": [x,y,z] added (then clipped)
  imu   stuck       output held
  gps   dropout     rows omitted
  gps   jump        "east_m", "north_m", "up_m": offset added
"""
from __future__ import annotations

import math

import numpy as np

import flightsim as fs


def event_time(spec, events: dict) -> float:
    if isinstance(spec, (int, float)):
        return float(spec)
    text = str(spec).replace(' ', '')
    for sign in ('+', '-'):
        if sign in text[1:]:
            name, off = text.split(sign, 1)
            base = events[name if name.endswith('_s') else name + '_s']
            return base + (float(off) if sign == '+' else -float(off))
    return events[text if text.endswith('_s') else text + '_s']


def windows(glitches, sensor, kind, events):
    for g in glitches:
        if g['sensor'] == sensor and g['kind'] == kind:
            t0 = event_time(g['at'], events)
            yield t0, t0 + g.get('dur_s', 0.0), g


def schedule(cfg: dict, ev: dict, end_s: float) -> dict[str, list[int]]:
    out = cfg['output']
    end_ms = int(round(end_s * 1000))
    imu_full = 1000 // out['imu_hz']
    imu_slow = 1000 // out['imu_descent_hz']
    full_until = ev['apogee_s'] + out['imu_full_rate_after_apogee_s']
    win = out['imu_full_rate_window_s']
    dense = [(-1.0, full_until)]
    for name in ('main_s', 'landing_s', 'tipover_s'):
        if name in ev:
            dense.append((ev[name] - win, ev[name] + win))
    imu = []
    t = 0
    while t <= end_ms:
        imu.append(t)
        ts = t / 1000.0
        step = imu_full if any(a <= ts < b for a, b in dense) else imu_slow
        t += step
    baro = list(range(0, end_ms + 1, 1000 // out['baro_hz']))
    gps = list(range(0, end_ms + 1, 1000 // out['gps_hz']))
    return {'IMU': imu, 'BARO': baro, 'GPS': gps}


def fc_altitude(p, p0):
    return 44330.0 * (1.0 - np.power(p / p0, 0.1903))


def imu_rows(cfg, truth: fs.Truth, times, rng, noise: bool, glitches):
    s = cfg['sensors']['imu']
    ev = truth.events
    g0 = fs.G0
    peak = max(truth.thrust) or 1.0
    if noise:
        a_bias = rng.normal(0, s['accel_bias_sigma_mg'] * 1e-3 * g0, 3)
        a_scale = 1 + rng.normal(0, s['accel_scale_sigma'], 3)
        g_bias = rng.normal(0, s['gyro_bias_sigma_dps'], 3)
        g_scale = 1 + rng.normal(0, s['gyro_scale_sigma'], 3)
        a_sig = s['accel_noise_density_ug_rthz'] * 1e-6 * g0 * math.sqrt(s['accel_bandwidth_hz'])
        g_sig = s['gyro_noise_density_dps_rthz'] * math.sqrt(s['gyro_bandwidth_hz'])
    rows = []
    stuck = list(windows(glitches, 'imu', 'stuck', ev))
    spikes = list(windows(glitches, 'imu', 'spike', ev))
    held = None
    for t_ms in times:
        k = min(t_ms, len(truth.t) - 1)
        q = truth.q[k]
        f = np.array(fs.qrot_inv(q, truth.f_world[k]))
        w = np.degrees(np.array(truth.w_body[k]))
        if noise:
            ft = truth.thrust[k] / peak
            buf = s['buffet_mps2_per_10kpa'] * truth.qdyn[k] / 1e4
            vib = np.array([s['vibration_burn_lateral_mps2'] * ft, s['vibration_burn_lateral_mps2'] * ft,
                            s['vibration_burn_axial_mps2'] * ft])
            f = f * a_scale + a_bias + rng.normal(0, 1, 3) * np.sqrt(a_sig ** 2 + vib ** 2 + buf ** 2)
            gv = s['vibration_burn_gyro_dps'] * ft + 2.0 * buf
            w = w * g_scale + g_bias + rng.normal(0, 1, 3) * math.sqrt(g_sig ** 2 + gv ** 2)
        ts = t_ms / 1000.0
        for a, b, g in spikes:
            if a <= ts <= max(b, a + 1e-9):
                f = f + np.array(g.get('accel_mps2', [0, 0, 0]))
                w = w + np.array(g.get('gyro_dps', [0, 0, 0]))
        if noise:
            f = np.round(f / s['accel_lsb_mps2']) * s['accel_lsb_mps2']
            w = np.round(w / s['gyro_lsb_dps']) * s['gyro_lsb_dps']
        f = np.clip(f, -s['accel_range_mps2'], s['accel_range_mps2'])
        w = np.clip(w, -s['gyro_range_dps'], s['gyro_range_dps'])
        row = [t_ms, *f.tolist(), *w.tolist()]
        if any(a <= ts <= b for a, b, _ in stuck):
            if held is None:
                held = row[1:]
            row = [t_ms, *held]
        else:
            held = None
        rows.append(row)
    return rows


def baro_rows(cfg, truth: fs.Truth, times, rng, noise: bool, glitches):
    s = cfg['sensors']['baro']
    env = cfg['environment']
    ev = truth.events
    odr_ms = 1000 // s['odr_hz']
    n = len(truth.t)
    ks = np.arange(0, n, odr_ms)
    h = np.array([truth.pos[k][2] for k in ks])
    tt = ks / 1000.0
    p = np.array([fs.atmosphere(env['site_msl_m'] + hh, env['temp_offset_k'])[1] for hh in h])
    p = p + env['pressure_drift_pa_per_min'] * tt / 60.0
    mach = np.array([truth.mach[k] for k in ks])
    qd = np.array([truth.qdyn[k] for k in ks])
    if noise:
        x = np.clip((mach - 0.75) / 0.2, 0, 1)
        cp = s['port_cp_subsonic'] + (s['port_cp_transonic'] - s['port_cp_subsonic']) * x * x * (3 - 2 * x)
        p = p + cp * qd
        for name in ('drogue_s', 'main_s'):
            if name in ev:
                dt = tt - ev[name]
                rise = np.clip(dt / 0.01, 0, 1)
                p = p + s['ejection_spike_pa'] * rise * np.exp(-np.clip(dt, 0, None) / (s['ejection_spike_s'] / 3)) \
                    * (dt >= 0)
        # AV-bay vent lag (first order), raw noise, BMP390 IIR.
        a = math.exp(-odr_ms / 1000.0 / s['port_lag_s'])
        lag = np.empty_like(p)
        acc = p[0]
        for i, v in enumerate(p):
            acc = a * acc + (1 - a) * v
            lag[i] = acc
        sig = np.full_like(p, s['noise_pa'])
        for t0, t1, g in windows(glitches, 'baro', 'noise', ev):
            sig = np.where((tt >= t0) & (tt <= t1), sig * math.sqrt(1 + g['scale'] ** 2), sig)
        raw = lag + rng.normal(0, 1, len(p)) * sig
        c = s['iir_coef']
        out = np.empty_like(raw)
        y = raw[0]
        for i, v in enumerate(raw):
            y = (c * y + v) / (c + 1)
            out[i] = y
        offset = float(np.clip(rng.normal(0, s['abs_offset_sigma_pa']), -50, 50))
        out = out + offset
    else:
        out = p
    # Glitches on the output.
    for t0, t1, g in windows(glitches, 'baro', 'spike', ev):
        out = np.where((tt >= t0) & (tt <= max(t1, t0 + 1e-9)), out + g['pa'], out)
    for t0, t1, g in windows(glitches, 'baro', 'outliers', ev):
        hit = (tt >= t0) & (tt <= t1) & (rng.random(len(out)) < g['prob'])
        out = np.where(hit, out + g['pa'] * rng.choice([-1.0, 1.0], len(out)), out)
    for t0, t1, g in windows(glitches, 'baro', 'stuck', ev):
        i0 = int(np.searchsorted(tt, t0))
        sel = (tt >= t0) & (tt <= t1)
        out = np.where(sel, out[i0], out)
    pre = tt < cfg['launch']['ignition_s'] - 1.0
    p0 = float(np.mean(out[pre])) if pre.any() else float(out[0])
    alt = fc_altitude(out, p0)
    # AV-bay temperature (die temperature), lagging the ambient lapse.
    amb = env['pad_temp_c'] - 0.0065 * h
    temp = np.empty_like(amb)
    tb = env['pad_temp_c'] + s['bay_temp_offset_c']
    a = math.exp(-odr_ms / 1000.0 / s['bay_temp_tau_s'])
    for i, v in enumerate(amb):
        tb = a * tb + (1 - a) * (v + s['bay_temp_offset_c'])
        temp[i] = tb
    idx = {int(k): i for i, k in enumerate(ks)}
    rows = []
    for t_ms in times:
        # The latest completed conversion at or before t.
        i = idx.get(t_ms - t_ms % odr_ms, len(ks) - 1)
        rows.append([t_ms, float(alt[i]), float(out[i]), float(temp[i])])
    return rows


def gps_rows(cfg, truth: fs.Truth, times, rng, noise: bool, glitches):
    s = cfg['sensors']['gps']
    env = cfg['environment']
    ev = truth.events
    lat0 = math.radians(env['site_lat_deg'])
    e2 = 0.00669437999014
    rm = fs.WGS84_A * (1 - e2) / (1 - e2 * math.sin(lat0) ** 2) ** 1.5
    rn = fs.WGS84_A / math.sqrt(1 - e2 * math.sin(lat0) ** 2)
    drop = [(ev['liftoff_s'] + s['dropout_from_liftoff_s'], ev['burnout_s'] + s['dropout_after_burnout_s'])] \
        if noise and s.get('dropout_after_burnout_s') is not None else []
    drop += [(a, b) for a, b, _ in windows(glitches, 'gps', 'dropout', ev)]
    jumps = list(windows(glitches, 'gps', 'jump', ev))
    gm = np.zeros(3)
    dt = 1.0 / cfg['output']['gps_hz']
    kh = math.exp(-dt / s['horiz_gm_tau_s'])
    sig = np.array([s['horiz_gm_sigma_m'], s['horiz_gm_sigma_m'], s['vert_gm_sigma_m']])
    if noise:
        gm = rng.normal(0, 1, 3) * sig
    rows = []
    for t_ms in times:
        ts = t_ms / 1000.0
        k = min(t_ms, len(truth.t) - 1)
        e, nn, u = truth.pos[k]
        if noise:
            gm = kh * gm + math.sqrt(1 - kh * kh) * sig * rng.normal(0, 1, 3)
            wh = np.array([s['horiz_white_m'], s['horiz_white_m'], s['vert_white_m']]) * rng.normal(0, 1, 3)
            e, nn, u = e + gm[0] + wh[0], nn + gm[1] + wh[1], u + gm[2] + wh[2]
        for a, b, g in jumps:
            if a <= ts <= b:
                e, nn, u = e + g.get('east_m', 0), nn + g.get('north_m', 0), u + g.get('up_m', 0)
        if any(a <= ts < b for a, b in drop):
            continue
        lat = env['site_lat_deg'] + math.degrees(nn / rm)
        lon = env['site_lon_deg'] + math.degrees(e / (rn * math.cos(lat0)))
        rows.append([t_ms, lat, lon, env['site_msl_m'] + u])
    return rows


def make_rows(cfg, truth, seed: int, noise: bool, glitches=()):
    rng = np.random.default_rng(seed)
    end = truth.events['end_s']
    times = schedule(cfg, truth.events, end)
    return {
        'IMU': imu_rows(cfg, truth, times['IMU'], rng, noise, glitches),
        'BARO': baro_rows(cfg, truth, times['BARO'], rng, noise, glitches),
        'GPS': gps_rows(cfg, truth, times['GPS'], rng, noise, glitches),
    }
