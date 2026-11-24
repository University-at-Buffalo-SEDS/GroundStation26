"""Truth model for the mock flight: atmosphere, aerodynamics, motor and a
3-DOF point mass with an attitude (weathercock / pendulum) approximation.

Everything is in a local East-North-Up frame with its origin at the pad, z up.
The vehicle attitude is a body->world quaternion (w, x, y, z); body z is the
vehicle axis (nose), the FC's own frame (docs/integrated-ground-test.md 4.2).

The integrator runs at a fixed 1 ms step and records, per step, the kinematic
acceleration. The accelerometer's specific force is then exactly
f = a - g (world), so the replayed IMU, baro and GPS are mutually consistent:
integrating the replayed IMU through the right attitude reproduces the
trajectory the baro and GPS see.

Sources (also listed in vehicle.json):
  * U.S. Standard Atmosphere 1976 (NOAA-S/T 76-1562), troposphere layer.
  * WGS-84 normal gravity (Somigliana) with the free-air gradient.
  * OpenRocket Technical Documentation (S. Niskanen, 2013), ch. 3.4:
    skin friction (eqs. 3.81-3.84), base drag (3.86/3.94), fin leading-edge
    pressure drag; transonic rise approximated (no published 6 in model).
  * Barrowman-style pitch dynamics: restoring moment q*A*CNa*SM, rod inertia.
"""
from __future__ import annotations

import bisect
import csv
import math
import random
from dataclasses import dataclass, field
from pathlib import Path

G0 = 9.80665
R_AIR = 287.05287
GAMMA = 1.4
R_EARTH = 6356766.0           # USSA-76 geopotential radius
WGS84_A = 6378137.0


# ------------------------------------------------------------ environment

def atmosphere(h_msl: float, temp_offset_k: float = 0.0, p_sl: float = 101325.0):
    """USSA-76 troposphere/tropopause with an optional uniform temperature
    offset (hot/cold day). Returns (T K, p Pa, rho kg/m3, a m/s, mu Pa s)."""
    hg = R_EARTH * h_msl / (R_EARTH + h_msl)
    t0 = 288.15 + temp_offset_k
    if hg < 11000.0:
        t = t0 - 0.0065 * hg
        p = p_sl * (t / t0) ** (G0 / (R_AIR * 0.0065))
    else:
        t11 = t0 - 0.0065 * 11000.0
        p11 = p_sl * (t11 / t0) ** (G0 / (R_AIR * 0.0065))
        t = t11
        p = p11 * math.exp(-G0 * (hg - 11000.0) / (R_AIR * t11))
    rho = p / (R_AIR * t)
    a = math.sqrt(GAMMA * R_AIR * t)
    mu = 1.458e-6 * t ** 1.5 / (t + 110.4)   # Sutherland
    return t, p, rho, a, mu


def gravity(lat_deg: float, h_msl: float) -> float:
    s2 = math.sin(math.radians(lat_deg)) ** 2
    g = 9.7803253359 * (1 + 0.00193185265241 * s2) / math.sqrt(1 - 0.00669437999013 * s2)
    return g - 3.086e-6 * h_msl


class Wind:
    """Mean wind with a power-law profile plus Gauss-Markov gusts per axis
    (a first-order stand-in for the Dryden spectrum, MIL-F-8785C)."""

    def __init__(self, speed_mps: float, from_deg: float, shear_exp: float,
                 gust_sigma: float, gust_tau_s: float, rng: random.Random, ref_h: float = 10.0,
                 cap: float = 1.4):
        to = math.radians(from_deg + 180.0)
        self.dir = (math.sin(to), math.cos(to))     # E, N unit
        self.speed, self.shear, self.ref_h, self.cap = speed_mps, shear_exp, ref_h, cap
        self.sigma, self.tau, self.rng = gust_sigma, max(gust_tau_s, 0.05), rng
        self.g = [0.0, 0.0, 0.0]
        self.t_last = 0.0

    def step(self, dt: float):
        if self.sigma <= 0.0:
            return
        k = math.exp(-dt / self.tau)
        s = self.sigma * math.sqrt(1 - k * k)
        self.g[0] = k * self.g[0] + s * self.rng.gauss(0, 1)
        self.g[1] = k * self.g[1] + s * self.rng.gauss(0, 1)
        self.g[2] = k * self.g[2] + 0.4 * s * self.rng.gauss(0, 1)

    def at(self, h_agl: float):
        prof = (max(h_agl, 1.0) / self.ref_h) ** self.shear
        sp = self.speed * min(prof, self.cap)
        gscale = min(1.0, max(h_agl, 0.0) / 5.0) if h_agl < 5 else 1.0
        return (sp * self.dir[0] + self.g[0] * gscale,
                sp * self.dir[1] + self.g[1] * gscale,
                self.g[2] * gscale)


# ------------------------------------------------------------ vehicle

@dataclass
class Motor:
    t: list[float]
    f: list[float]
    prop_kg: float
    exit_d: float
    osc_amp: float = 0.0
    osc_hz: float = 0.0
    impulse: float = 0.0
    cum: list[float] = field(default_factory=list)
    # Physics motor (hybrid_motor.py): propellant left on the same time base (overrides the impulse-fraction
    # mass model and the residual fraction), and the nozzle exit area with the ambient pressure the curve was
    # computed at, so thrust gains (pa_ref - p_air) * exit area with altitude. None/0 = the measured curve as is.
    prop_curve: list[float] | None = None
    exit_area_m2: float = 0.0
    pa_ref: float = 0.0

    @classmethod
    def load(cls, path: Path, prop_kg: float, exit_d: float, scale_to_ns: float | None,
             osc_amp: float, osc_hz: float, scale_prop: bool = False,
             thrust_exponent: float = 1.0) -> 'Motor':
        t, f = [], []
        with path.open() as h:
            for row in csv.reader(h):
                if not row or row[0].startswith('#') or not row[0].strip()[0].isdigit():
                    continue
                t.append(float(row[0]))
                f.append(max(0.0, float(row[1])))
        m = cls(t, f, prop_kg, exit_d, osc_amp, osc_hz)
        m._integrate()
        if scale_to_ns:
            # Impulse ratio k split between the thrust axis (k^e) and the time
            # axis (k^(1-e)): e = 1 is a harder motor, e = 0 a longer burn
            # (bigger tank, same injector), 0.5 both.
            k = scale_to_ns / m.impulse
            m.f = [x * k ** thrust_exponent for x in m.f]
            m.t = [x * k ** (1.0 - thrust_exponent) for x in m.t]
            m._integrate()
            if scale_prop:
                m.prop_kg *= k
        return m

    def _integrate(self):
        self.cum = [0.0]
        for i in range(1, len(self.t)):
            self.cum.append(self.cum[-1] + 0.5 * (self.f[i] + self.f[i - 1]) * (self.t[i] - self.t[i - 1]))
        self.impulse = self.cum[-1]

    @property
    def burn_s(self) -> float:
        return self.t[-1]

    def thrust(self, tb: float) -> float:
        if tb <= 0.0 or tb >= self.t[-1]:
            return 0.0
        i = bisect.bisect_right(self.t, tb) - 1
        k = (tb - self.t[i]) / (self.t[i + 1] - self.t[i])
        base = self.f[i] + (self.f[i + 1] - self.f[i]) * k
        if self.osc_amp:
            base *= 1.0 + self.osc_amp * math.sin(2 * math.pi * self.osc_hz * tb)
        return base

    def burnt_fraction(self, tb: float) -> float:
        if tb <= 0.0:
            return 0.0
        if tb >= self.t[-1]:
            return 1.0
        i = bisect.bisect_right(self.t, tb) - 1
        k = (tb - self.t[i]) / (self.t[i + 1] - self.t[i])
        part = self.cum[i] + (self.f[i] + 0.5 * (self.f[i + 1] - self.f[i]) * k) * (tb - self.t[i])
        return part / self.impulse

    @classmethod
    def hybrid(cls, fill_kg: float, tank_c: float, supercharge_psig: float, ambient_pa: float, exit_d: float = 0.0,
               osc_amp: float = 0.0, osc_hz: float = 0.0, overrides: dict | None = None) -> 'Motor':
        """The physics motor for a fill (hybrid_motor.py, calibrated spec) at the pad's ambient pressure."""
        import hybrid_motor as hm
        p = hm.params(hm.load_spec(), overrides)
        res = hm.simulate(p, hm.load_data(), fill_kg, tank_c, supercharge_psig, ambient_pa, 0.01)
        t, f, pl = hm.curve_for_flight(res, 0.05)
        m = cls(t, f, pl[0], exit_d or p['exit_d_m'], osc_amp, osc_hz)
        m.prop_curve = pl
        m.exit_area_m2 = math.pi / 4.0 * p['exit_d_m'] ** 2
        m.pa_ref = ambient_pa
        m._integrate()
        m.summary = res['summary']
        return m

    def thrust_at(self, tb: float, p_air: float) -> float:
        f = self.thrust(tb)
        if f > 0.0 and self.exit_area_m2 > 0.0:
            f += (self.pa_ref - p_air) * self.exit_area_m2
        return max(f, 0.0)

    def prop_left(self, tb: float) -> float:
        if tb <= 0.0:
            return self.prop_curve[0]
        if tb >= self.t[-1]:
            return self.prop_curve[-1]
        i = bisect.bisect_right(self.t, tb) - 1
        k = (tb - self.t[i]) / (self.t[i + 1] - self.t[i])
        return self.prop_curve[i] + (self.prop_curve[i + 1] - self.prop_curve[i]) * k

    def time_at_fraction_of_peak(self, frac: float) -> tuple[float, float]:
        peak = max(self.f)
        on = next(t for t, f in zip(self.t, self.f) if f >= frac * peak)
        off = next(t for t, f in zip(reversed(self.t), reversed(self.f)) if f >= frac * peak)
        return on, off


class Aero:
    """Zero-lift drag coefficient (reference: body cross-section) vs Mach,
    Reynolds number and thrusting, by component build-up (OpenRocket ch. 3.4)."""

    def __init__(self, v: dict, a: dict, exit_d: float):
        self.d = v['diameter_m']
        self.L = v['length_m']
        self.aref = math.pi * self.d ** 2 / 4
        fins = v['fins']
        nose_l = v['nose_length_m']
        # Wetted areas: cylinder + ogive (~0.75 of its enclosing cylinder).
        self.s_body = math.pi * self.d * (self.L - nose_l) + 0.75 * math.pi * self.d * nose_l
        fin_area = 0.5 * (fins['root_m'] + fins['tip_m']) * fins['span_m']
        self.s_fins = 2 * fins['count'] * fin_area
        self.fin_tc = fins['thickness_m'] / fins['root_m']
        self.fin_front = fins['count'] * fins['thickness_m'] * fins['span_m'] / self.aref
        self.fineness = self.L / self.d
        self.rough = v['surface_roughness_m']
        # Base drag acts on the boattail's aft area (the body's without one).
        self.base_frac = (min(v.get('boattail_aft_diameter_m') or self.d, self.d) / self.d) ** 2
        self.exit_frac = (exit_d / self.d) ** 2
        self.k_prot = a['protuberance_factor']
        self.cd_extra = a['cd_extra']
        self.wave_peak = a['transonic_wave_cd']
        self.m_crit = a['transonic_mach_start']
        self.cd_scale = a.get('cd_scale', 1.0)

    @staticmethod
    def _base(m: float) -> float:
        return 0.12 + 0.13 * m * m if m < 1.0 else 0.25 / m

    def cd(self, mach: float, re: float, thrusting: bool) -> float:
        re = max(re, 1e4)
        cf_t = 1.0 / (1.50 * math.log(re) - 5.6) ** 2
        cf_r = 0.032 * (self.rough / self.L) ** 0.2
        cf = max(cf_t, cf_r)
        cf *= (1 - 0.1 * mach * mach) if mach < 1 else 1 / (1 + 0.15 * mach * mach) ** 0.58
        friction = cf * ((1 + 1 / (2 * self.fineness)) * self.s_body +
                         (1 + 2 * self.fin_tc) * self.s_fins) / self.aref
        base = self._base(mach) * (max(self.base_frac - self.exit_frac, 0.0) if thrusting else self.base_frac)
        m = min(mach, 0.9)
        fin_le = ((1 - m * m) ** -0.417 - 1) * self.fin_front
        fin_te = self._base(mach) * self.fin_front
        x = (mach - self.m_crit) / 0.25
        wave = self.wave_peak * (0.0 if x <= 0 else (min(x, 1.0) ** 2 * (3 - 2 * min(x, 1.0))))
        return self.cd_scale * (friction * self.k_prot + base + fin_le + fin_te + wave + self.cd_extra)


# ------------------------------------------------------------ quaternions

def qmul(a, b):
    aw, ax, ay, az = a
    bw, bx, by, bz = b
    return (aw * bw - ax * bx - ay * by - az * bz,
            aw * bx + ax * bw + ay * bz - az * by,
            aw * by - ax * bz + ay * bw + az * bx,
            aw * bz + ax * by - ay * bx + az * bw)


def qnorm(q):
    n = math.sqrt(sum(c * c for c in q))
    return tuple(c / n for c in q)


def qrot(q, v):
    """Rotate world<-body: R(q) v."""
    w, x, y, z = q
    vx, vy, vz = v
    # t = 2 * cross(q.xyz, v)
    tx = 2 * (y * vz - z * vy)
    ty = 2 * (z * vx - x * vz)
    tz = 2 * (x * vy - y * vx)
    return (vx + w * tx + (y * tz - z * ty),
            vy + w * ty + (z * tx - x * tz),
            vz + w * tz + (x * ty - y * tx))


def qrot_inv(q, v):
    w, x, y, z = q
    return qrot((w, -x, -y, -z), v)


def q_from_axis_angle(axis, ang):
    n = math.sqrt(sum(c * c for c in axis))
    if n < 1e-12 or ang == 0.0:
        return (1.0, 0.0, 0.0, 0.0)
    s = math.sin(ang / 2) / n
    return (math.cos(ang / 2), axis[0] * s, axis[1] * s, axis[2] * s)


def cross(a, b):
    return (a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0])


def dot(a, b):
    return a[0] * b[0] + a[1] * b[1] + a[2] * b[2]


def norm(a):
    return math.sqrt(dot(a, a))


def rail_normal_accel(rail_dir, g, vair, qd, aref, cna, mass):
    """Load (per kg) the rail buttons carry while on the launcher: the weight's component across the
    rail plus the aerodynamic normal force of the cross-flow (the same CNa force as in boost, capped at
    15 deg), as a magnitude. Rail friction is mu times this (Coulomb, vehicle.json launch.rail_friction).
    Mirrored in backend/src/apogee_planner/flight.rs and FirmwareSimulator native_plant.rs."""
    gpar = -g * rail_dir[2]
    load = [0.0 - gpar * rail_dir[0], 0.0 - gpar * rail_dir[1], -g - gpar * rail_dir[2]]
    vmag = norm(vair)
    if vmag > 0.5:
        vh = tuple(c / vmag for c in vair)
        du = dot(rail_dir, vh)
        nperp = [rail_dir[i] - du * vh[i] for i in range(3)]
        sin_a = norm(nperp)
        lim = math.sin(math.radians(15.0))
        ks = min(lim / sin_a, 1.0) if sin_a > lim else 1.0
        fa = [qd * aref * cna * c * ks / mass for c in nperp]
        fa_par = dot(fa, rail_dir)
        for i in range(3):
            load[i] += fa[i] - fa_par * rail_dir[i]
    return norm(load)


def unit(a):
    n = norm(a)
    return (a[0] / n, a[1] / n, a[2] / n) if n > 1e-12 else (0.0, 0.0, 1.0)


def rotate_toward(u, target, ang):
    """u rotated by angle ang (rad) toward target about u x target."""
    ax = cross(u, target)
    if norm(ax) < 1e-9:
        return u
    return qrot(q_from_axis_angle(ax, ang), u)


# ------------------------------------------------------------ flight

PHASES = ['pad', 'rail', 'boost', 'coast', 'drogue', 'main', 'impact', 'ground']


@dataclass
class Truth:
    t: list = field(default_factory=list)           # s since T0 (Launch acceptance)
    pos: list = field(default_factory=list)         # E, N, U (m, from the pad)
    vel: list = field(default_factory=list)
    f_world: list = field(default_factory=list)     # specific force, world frame
    q: list = field(default_factory=list)           # body->world
    w_body: list = field(default_factory=list)      # rad/s
    mach: list = field(default_factory=list)
    qdyn: list = field(default_factory=list)
    thrust: list = field(default_factory=list)
    phase: list = field(default_factory=list)
    events: dict = field(default_factory=dict)
    extra: dict = field(default_factory=dict)


def half_sine(tau: float, dur: float) -> float:
    return math.sin(math.pi * tau / dur) if 0.0 <= tau <= dur else 0.0


def chute_cda_from_rate(mass: float, rate: float, rho: float, g: float) -> float:
    return 2 * mass * g / (rho * rate * rate)


def fly(cfg: dict, rng: random.Random, motor: Motor, record: bool = True,
        stop_at_apogee: bool = False, dt: float = 0.001) -> Truth:
    v, a_cfg, rec, env, lau, att = (cfg['vehicle'], cfg['aero'], cfg['recovery'], cfg['environment'],
                                     cfg['launch'], cfg['attitude'])
    aero = Aero(v, a_cfg, motor.exit_d)
    site_msl, lat = env['site_msl_m'], env['site_lat_deg']
    dT = env['temp_offset_k']
    wind = Wind(env['wind_mps'], env['wind_from_deg'], env['wind_shear_exponent'],
                env['gust_sigma_mps'], env['gust_tau_s'], rng, cap=env.get('wind_profile_cap', 1.4))
    m_dry = v['dry_mass_kg']
    resid = motor.prop_kg * cfg['motor'].get('residual_fraction', 0.0)
    if motor.prop_curve:
        resid = motor.prop_curve[-1]
    m_burn = motor.prop_kg - resid
    L, d = v['length_m'], v['diameter_m']
    aref = aero.aref
    cna, sm = v['cn_alpha_per_rad'], v['static_margin_cal'] * d
    i_factor = v['pitch_inertia_factor']
    t_ign = lau['ignition_s']
    tilt, az = math.radians(lau['tilt_deg']), math.radians(lau['azimuth_deg'])
    rail_dir = (math.sin(tilt) * math.sin(az), math.sin(tilt) * math.cos(az), math.cos(tilt))
    rail_len = lau['rail_length_m']

    # Attitude: body z along the rail, an arbitrary (seeded) roll about it.
    q = q_from_axis_angle(cross((0, 0, 1), rail_dir), tilt) if tilt else (1.0, 0.0, 0.0, 0.0)
    q = qnorm(qmul(q, q_from_axis_angle((0, 0, 1), math.radians(att['pad_roll_deg']))))
    pos = [0.0, 0.0, 0.0]
    vel = [0.0, 0.0, 0.0]
    om = [0.0, 0.0, 0.0]        # world-frame angular velocity perpendicular to the axis
    p_roll = 0.0
    phase = 'pad'
    out = Truth()
    t = 0.0
    s_rail = 0.0
    apogee_t = None
    ev = out.events
    max_h = 0.0
    g_pad = gravity(lat, site_msl)
    ejection = None            # (kind, t_start)
    deploy_t = {}
    chute_cda = {'drogue': None, 'main': None}
    osc_phase = rng.uniform(0, 2 * math.pi)
    osc_axis_phase = rng.uniform(0, 2 * math.pi)
    spin_sign = rng.choice((-1.0, 1.0))
    roll_eq_sign = 1.0 if v.get('roll_direction', 'ccw') == 'ccw' else -1.0
    impact = None
    rest_until = None
    tip = None
    land_crush = rec['landing_crush_m']
    ground_f = None
    m_desc = m_dry + resid
    while True:
        h = pos[2]
        t_air, p_air, rho, a_snd, mu = atmosphere(site_msl + max(h, 0.0), dT)
        g = gravity(lat, site_msl + h)
        gvec = (0.0, 0.0, -g)
        tb = t - t_ign
        thrust = motor.thrust_at(tb, p_air)
        if motor.prop_curve:
            mass = m_dry + motor.prop_left(tb)
        else:
            mass = m_dry + resid + m_burn * (1.0 - motor.burnt_fraction(tb))
        wind.step(dt)
        w = wind.at(h)
        vair = (vel[0] - w[0], vel[1] - w[1], vel[2] - w[2])
        vmag = norm(vair)
        qd = 0.5 * rho * vmag * vmag
        mach = vmag / a_snd
        u = qrot(q, (0.0, 0.0, 1.0))
        acc = [0.0, 0.0, 0.0]
        fx = [0.0, 0.0, 0.0]       # felt by the FC only (rotation about the pivot, ground thump)
        extra_f = (0.0, 0.0, 0.0)
        torque = [0.0, 0.0, 0.0]
        inertia = mass * L * L / 12.0 * i_factor

        if phase == 'pad':
            # Breakaway: thrust beats the weight along the rail plus static friction on the
            # weight's cross-rail component.
            mu_rail = lau.get('rail_friction', 0.0)
            if thrust > mass * g * (rail_dir[2] + mu_rail * math.sqrt(max(0.0, 1.0 - rail_dir[2] ** 2))):
                phase = 'rail'
                ev['liftoff_s'] = t
            acc = [0.0, 0.0, 0.0]
            fw = (0.0, 0.0, g)
        elif phase == 'rail':
            cd = aero.cd(mach, rho * vmag * L / mu, thrust > 0)
            drag = qd * aref * cd
            mu_rail = lau.get('rail_friction', 0.0)
            normal = rail_normal_accel(rail_dir, g, vair, qd, aref, cna, mass)
            along = (thrust - drag) / mass - g * rail_dir[2] - mu_rail * normal
            sp = dot(vel, rail_dir)
            if sp <= 0 and along < 0:
                along = 0.0
            acc = [along * c for c in rail_dir]
            s_rail += sp * dt
            if s_rail >= rail_len:
                phase = 'boost'
                ev['rail_exit_s'] = t
                ev['rail_exit_mps'] = sp
        if phase in ('boost', 'coast'):
            thrusting = thrust > 0.0
            if phase == 'boost' and tb >= motor.burn_s:
                phase = 'coast'
                ev['burnout_s'] = t
            vh = unit(vair) if vmag > 0.5 else u
            re = rho * max(vmag, 0.1) * L / mu
            cd = aero.cd(mach, re, thrusting)
            drag = tuple(-qd * aref * cd * c for c in vh)
            nperp = tuple(u[i] - dot(u, vh) * vh[i] for i in range(3))
            sin_a = norm(nperp)
            lim = math.sin(math.radians(15.0))
            ks = min(1.0, lim / sin_a) if sin_a > lim else 1.0
            normal = tuple(qd * aref * cna * c * ks for c in nperp)
            acc = [(thrust * u[i] + drag[i] + normal[i]) / mass + gvec[i] for i in range(3)]
            k = qd * aref * cna * sm
            ax = cross(u, vh)
            c_damp = 2 * att['pitch_damping_ratio'] * math.sqrt(max(k, 0.0) * inertia)
            torque = [k * ax[i] - c_damp * om[i] for i in range(3)]
            p_eq = roll_eq_sign * math.radians(v['roll_rate_dps_per_mps']) * vmag
            tau_r = v['roll_damping_length_m'] / max(vmag, 5.0)
            p_roll += (p_eq - p_roll) * min(dt / tau_r, 1.0)
            if vel[2] <= 0.0 and apogee_t is None and t > t_ign + 1:
                apogee_t = t
                ev['apogee_s'] = t
                ev['apogee_m'] = h
                if stop_at_apogee:
                    out.extra['max_h'] = max_h
                    return out
        if phase in ('coast', 'drogue') and apogee_t is not None and 'drogue_s' not in ev \
                and t >= apogee_t + rec['drogue_delay_after_apogee_s']:
            ev['drogue_s'] = t
            ev['drogue_alt_m'] = h
            phase = 'drogue'
            ejection = ('drogue', t)
            deploy_t['drogue'] = t
            rho_mid = atmosphere(site_msl + rec['drogue_rate_ref_alt_agl_m'], dT)[2]
            chute_cda['drogue'] = chute_cda_from_rate(m_desc, rec['drogue_descent_mps'], rho_mid, g_pad) \
                - rec['body_cda_descent_m2']
            kick = (rng.gauss(0, 1), rng.gauss(0, 1), rng.gauss(0, 1))
            kick = unit(tuple(kick[i] - dot(kick, u) * u[i] for i in range(3)))
            om = [om[i] + math.radians(rec['ejection_tumble_dps']) * kick[i] for i in range(3)]
        if phase == 'drogue' and 'main_s' not in ev and h <= rec['main_deploy_alt_agl_m'] and \
                vel[2] < 0:
            if 'main_trigger_s' not in ev:
                ev['main_trigger_s'] = t
            if t >= ev['main_trigger_s'] + rec['main_deploy_lag_s']:
                ev['main_s'] = t
                ev['main_alt_m'] = h
                phase = 'main'
                ejection = ('main', t)
                deploy_t['main'] = t
                rho_g = atmosphere(site_msl, dT)[2]
                chute_cda['main'] = chute_cda_from_rate(m_desc, rec['main_descent_mps'], rho_g, g_pad) \
                    - rec['body_cda_descent_m2']
        if phase in ('drogue', 'main'):
            vh = unit(vair) if vmag > 0.05 else (0.0, 0.0, -1.0)
            cda = rec['body_cda_descent_m2']
            for kind in ('drogue', 'main'):
                if kind not in deploy_t:
                    continue
                tau = t - deploy_t[kind] - rec[f'{kind}_line_stretch_s']
                full = chute_cda[kind]
                fill = rec[f'{kind}_fill_s']
                if kind == 'drogue' and 'main' in deploy_t:
                    # The main pulls out of the drogue bay; the drogue stays on the line.
                    full *= rec.get('drogue_cda_under_main_fraction', 1.0)
                if tau <= 0:
                    continue
                if tau < fill:
                    x = tau / fill
                    cda += full * x * x
                else:
                    ck = rec[f'{kind}_opening_overshoot']
                    cda += full * (1 + (ck - 1) * math.exp(-(tau - fill) / 0.35))
            drag = tuple(-0.5 * rho * vmag * cda * c for c in vair)
            acc = [drag[i] / mass + gvec[i] for i in range(3)]
            # Pendulum: the airframe hangs along the line toward the canopy,
            # which itself oscillates about the relative wind.
            kind = 'main' if 'main' in deploy_t else 'drogue'
            line = rec[f'{kind}_line_m']
            w_osc = math.sqrt(g / line)
            amp = math.radians(rec[f'{kind}_swing_deg'])
            tgt = tuple(-c for c in vh)
            perp = unit(cross(tgt, (math.cos(osc_axis_phase), math.sin(osc_axis_phase), 0.0)))
            tgt = rotate_toward(tgt, perp, amp * math.sin(w_osc * t + osc_phase))
            osc_axis_phase += 0.02 * dt * rng.gauss(0, 1) * 10
            ax = cross(u, tgt)
            w_hang = math.sqrt(6 * g / L)
            k = inertia * w_hang * w_hang
            c_damp = 2 * att['hang_damping_ratio'] * math.sqrt(k * inertia)
            torque = [k * ax[i] - c_damp * om[i] for i in range(3)]
            p_eq = spin_sign * math.radians(rec[f'{kind}_spin_dps'])
            p_roll += (p_eq - p_roll) * min(dt / 2.0, 1.0)
        if ejection is not None:
            kind, t0e = ejection
            tau = t - t0e
            dur = rec['ejection_pulse_s']
            dv = rec[f'{kind}_ejection_dv_mps']
            if tau <= dur:
                amp = dv * math.pi / (2 * dur)
                extra_f = tuple(-amp * half_sine(tau, dur) * c for c in u)
            ls = rec[f'{kind}_line_stretch_s']
            sdur = rec['snatch_pulse_s']
            if ls <= tau <= ls + sdur:
                amp = rec[f'{kind}_snatch_dv_mps'] * math.pi / (2 * sdur)
                vh = unit(vair) if vmag > 0.05 else (0.0, 0.0, -1.0)
                extra_f = tuple(extra_f[i] - amp * half_sine(tau - ls, sdur) * vh[i] for i in range(3))
            if tau > ls + sdur:
                ejection = None
            acc = [acc[i] + extra_f[i] for i in range(3)]

        if phase in ('drogue', 'main') and h <= 0.0 and vel[2] < 0:
            phase = 'impact'
            ev['landing_s'] = t
            ev['landing_mps'] = -vel[2]
            impact = t
            tilt_now = math.acos(max(-1.0, min(1.0, u[2])))
            tip_az = math.atan2(u[1], u[0]) if tilt_now > 1e-3 else rng.uniform(0, 2 * math.pi)
            tip = {'theta': max(tilt_now, math.radians(3.0)), 'thd': 0.0, 'az': tip_az, 'done': False}
            om = [0.0, 0.0, 0.0]
        if phase == 'impact':
            # Soil crush: constant deceleration stopping the descent in land_crush m.
            if vel[2] < 0:
                dec = ev['landing_mps'] ** 2 / (2 * land_crush)
                acc = [0.0, 0.0, dec]
                hv = math.hypot(vel[0], vel[1])
                if hv > 0.01:
                    fr = min(hv / dt, 0.6 * g + dec * 0.3)
                    acc[0], acc[1] = -fr * vel[0] / hv, -fr * vel[1] / hv
            else:
                vel = [0.0, 0.0, 0.0]
                acc = [0.0, 0.0, 0.0]
            # Tip-over of a slender rod pivoting on its aft end.
            if not tip['done']:
                r_fc = v['fc_station_from_aft_m']
                thdd = 1.5 * g / L * math.sin(tip['theta'])
                tip['thd'] += thdd * dt
                tip['theta'] += tip['thd'] * dt
                e_th = (math.cos(tip['theta']) * math.cos(tip['az']),
                        math.cos(tip['theta']) * math.sin(tip['az']), -math.sin(tip['theta']))
                tang = r_fc * thdd
                cent = r_fc * tip['thd'] ** 2
                fx = [fx[i] + tang * e_th[i] - cent * u[i] for i in range(3)]
                axis = (-math.sin(tip['az']), math.cos(tip['az']), 0.0)
                om = [tip['thd'] * c for c in axis]
                final = math.radians(rec['rest_tilt_deg'])
                if tip['theta'] >= final:
                    tip['done'] = True
                    ev['tipover_s'] = t
                    ground_f = (t, tip['thd'] * r_fc)
                    om = [0.0, 0.0, 0.0]
            p_roll *= 0.9
            if ground_f is not None:
                tau = t - ground_f[0]
                dur = rec['tipover_thump_s']
                if tau <= dur:
                    amp = ground_f[1] * math.pi / (2 * dur)
                    fx[2] += amp * half_sine(tau, dur)
                elif vel == [0.0, 0.0, 0.0]:
                    phase = 'ground'
                    rest_until = t + cfg['output']['rest_after_landing_s']
        if phase == 'ground':
            acc = [0.0, 0.0, 0.0]
            om = [0.0, 0.0, 0.0]
            p_roll = 0.0
            if t >= rest_until:
                ev['end_s'] = t
                break

        if phase != 'pad':
            fw = tuple(acc[i] - gvec[i] + fx[i] for i in range(3))
        # Integrate attitude (world-frame rate on the left).
        if phase in ('boost', 'coast', 'drogue', 'main'):
            om = [om[i] + torque[i] / inertia * dt for i in range(3)]
            u_now = qrot(q, (0.0, 0.0, 1.0))
            om_par = dot(om, u_now)
            om = [om[i] - om_par * u_now[i] for i in range(3)]
        u_now = qrot(q, (0.0, 0.0, 1.0))
        omega = tuple(om[i] + p_roll * u_now[i] for i in range(3))
        if record:
            out.t.append(t)
            out.pos.append(tuple(pos))
            out.vel.append(tuple(vel))
            out.f_world.append(fw)
            out.q.append(q)
            out.w_body.append(qrot_inv(q, omega))
            out.mach.append(mach)
            out.qdyn.append(qd)
            out.thrust.append(thrust)
            out.phase.append(phase)
        if phase not in ('pad', 'ground'):
            wn = norm(omega)
            if wn > 0:
                q = qnorm(qmul(q_from_axis_angle(omega, wn * dt), q))
            for i in range(3):
                vel[i] += acc[i] * dt
                pos[i] += vel[i] * dt
            if phase == 'impact' and pos[2] < -land_crush:
                pos[2] = -land_crush
        max_h = max(max_h, pos[2])
        if 'max_mach' not in out.extra or mach > out.extra['max_mach'][0]:
            out.extra['max_mach'] = (mach, t)
        sp = norm(vel)
        if 'max_speed' not in out.extra or sp > out.extra['max_speed'][0]:
            out.extra['max_speed'] = (sp, t)
        t = round(t + dt, 6)
        if t > 1200:
            raise RuntimeError('flight did not end within 1200 s')
    out.extra['max_h'] = max_h
    out.extra['mass_liftoff'] = m_dry + motor.prop_kg
    out.extra['mass_burnout'] = m_dry + resid
    out.extra['chute_cda'] = chute_cda
    return out
