#!/usr/bin/env python3
"""Physics model of the UB SEDS hybrid: N2O run tank -> feed line -> injector ->
paraffin-in-ABS grain -> nozzle (docs/hybrid-motor-model.md).

This file is the reference implementation. The station's apogee planner
(backend/src/apogee_planner/hybrid.rs) and the firmware simulator's plant
(FirmwareSimulator src/bay/hybrid_motor.rs) are ports of it; all three check
themselves against motors/hybrid_vectors.json (hybrid_vectors.py writes it).
Change the physics here first, regenerate the vectors, then port.

Model (each item cites its source in the doc):
  * Tank: equilibrium two-phase N2O (ESDU 91022 saturation properties) with a
    nitrogen pad (ideal gas in the ullage, not dissolved), one lumped
    temperature for liquid, vapour and a calibrated share of the tank wall.
    Draining liquid evaporates to refill the ullage and the latent heat cools
    the tank (Whitmore & Chandler 2010; the Aspire Space run-tank model).
    After the liquid is gone the vapour (and nitrogen) expands adiabatically.
  * Feed line: a lumped loss dP = K rho v^2 / 2 in the line.
  * Injector: Dyer et al. (AIAA 2007-5702) non-homogeneous non-equilibrium
    two-phase model (weighted SPI + choked HEM) in the liquid phase;
    compressible (choked or subsonic) vapour flow after it.
  * Fuel: r = a G_ox^n, paraffin (Stanford N2O lab-scale data) and ABS
    where the port reaches it; circular single port, uniform along the grain.
  * Chamber: quasi-steady mass balance Pc = (mdot_ox + mdot_f) eta_c* c*(O/F, Pc) / A_t,
    with c* from the equilibrium table (hybrid_cstar.py, NASA Glenn data).
  * Nozzle: ideal C_F for the table's throat gamma, exit-area pressure term at
    the ambient pressure (so thrust depends on altitude), Summerfield
    separation (p_wall < 0.4 p_a) for overexpanded flow, a nozzle efficiency
    on the momentum term, optional linear throat erosion.

Everything is SI. Pressures absolute unless named psig.
"""
from __future__ import annotations

import bisect
import json
import math
from pathlib import Path

HERE = Path(__file__).resolve().parent
SPEC_PATH = HERE / 'motors' / 'hybrid_motor.json'
CEA_PATH = HERE / 'motors' / 'sweet_v6_cea.json'
GRAIN_PROFILE_PATH = HERE / 'motors' / 'sweet_v6_grain.json'

PSI = 6894.757293
ATM = 101325.0
G0 = 9.80665
R_N2 = 296.8
R_N2O = 188.91
GAMMA_N2O_VAPOUR = 1.3
GAMMA_N2 = 1.4

# ------------------------------------------------------------------ N2O (ESDU 91022)
TC = 309.57
PC_CRIT = 7.251e6
RHOC = 452.0
T_MIN = 186.0  # reduced temperature 0.6
T_MAX = TC * 0.9999


def _tr(t):
    return min(max(t / TC, 0.6), 0.9999)


def psat(t):
    tr = _tr(t)
    x = 1.0 - tr
    return PC_CRIT * math.exp((-6.71893 * x + 1.35966 * x ** 1.5 - 1.3779 * x ** 2.5 - 4.051 * x ** 5) / tr)


def rho_l(t):
    x = 1.0 - _tr(t)
    return RHOC * math.exp(1.72328 * x ** (1 / 3) - 0.83950 * x ** (2 / 3) + 0.51060 * x - 0.10412 * x ** (4 / 3))


def rho_v(t):
    y = 1.0 / _tr(t) - 1.0
    return RHOC * math.exp(-1.00900 * y ** (1 / 3) - 6.28792 * y ** (2 / 3) + 7.50332 * y - 7.90463 * y ** (4 / 3)
                           + 0.629427 * y ** (5 / 3))


def h_l(t):
    """Saturated liquid enthalpy, J/kg (ESDU 91022 reference state)."""
    x = 1.0 - _tr(t)
    return (-200.0 + 116.043 * x ** (1 / 3) - 917.225 * x ** (2 / 3) + 794.779 * x - 589.587 * x ** (4 / 3)) * 1000.0


def h_v(t):
    x = 1.0 - _tr(t)
    return (-200.0 + 440.055 * x ** (1 / 3) - 459.701 * x ** (2 / 3) + 434.081 * x - 485.338 * x ** (4 / 3)) * 1000.0


def latent(t):
    return h_v(t) - h_l(t)


def cp_l(t):
    x = 1.0 - min(max(t / TC, 0.6), 0.995)
    return 2499.73 * (1.0 + 0.023454 / x - 3.80136 * x + 13.0945 * x * x - 14.5180 * x * x * x)


# Saturated-liquid entropy (J/kg/K, zero at T_MIN): ds = (dh_l - dPsat / rho_l) / T along the
# saturation line, integrated with the trapezoid rule on a 0.25 K grid.
S_GRID_DT = 0.25
_S_T: list[float] = []
_S_L: list[float] = []


def _build_entropy():
    def ds(t):
        e = 0.01
        dh = (h_l(t + e) - h_l(t - e)) / (2 * e)
        dp = (psat(t + e) - psat(t - e)) / (2 * e)
        return (dh - dp / rho_l(t)) / t
    n = int((309.5 - T_MIN) / S_GRID_DT) + 1
    s = 0.0
    prev = ds(T_MIN)
    _S_T.append(T_MIN)
    _S_L.append(0.0)
    for i in range(1, n):
        t = T_MIN + i * S_GRID_DT
        cur = ds(t)
        s += 0.5 * (prev + cur) * S_GRID_DT
        prev = cur
        _S_T.append(t)
        _S_L.append(s)


_build_entropy()


def s_l(t):
    t = min(max(t, T_MIN), _S_T[-1])
    i = min(int((t - T_MIN) / S_GRID_DT), len(_S_T) - 2)
    k = (t - _S_T[i]) / S_GRID_DT
    return _S_L[i] + (_S_L[i + 1] - _S_L[i]) * k


def s_v(t):
    return s_l(t) + latent(t) / t


def tsat(p):
    """Saturation temperature (K) for p (Pa), Newton on ln psat."""
    if p >= psat(T_MAX):
        return T_MAX
    if p <= psat(T_MIN):
        return T_MIN
    t = 280.0
    for _ in range(30):
        f = math.log(psat(t)) - math.log(p)
        e = 0.01
        d = (math.log(psat(t + e)) - math.log(psat(t - e))) / (2 * e)
        step = f / d
        t = min(max(t - step, T_MIN), T_MAX)
        if abs(step) < 1e-7:
            break
    return t


# ------------------------------------------------------------------ injector

def hem_flux(t1, p1, t2):
    """HEM mass flux (kg/m^2/s) from liquid at t1 (pressure p1 >= psat(t1)) flashing isentropically
    to saturation at t2."""
    rl1 = rho_l(t1)
    h1 = h_l(t1) + max(p1 - psat(t1), 0.0) / rl1
    s1 = s_l(t1)
    sl2, sv2 = s_l(t2), s_v(t2)
    x = min(max((s1 - sl2) / (sv2 - sl2), 0.0), 1.0)
    h2 = h_l(t2) + x * latent(t2)
    rho2 = 1.0 / (x / rho_v(t2) + (1.0 - x) / rho_l(t2))
    dh = h1 - h2
    return rho2 * math.sqrt(2.0 * dh) if dh > 0.0 else 0.0


def hem_choke(t1, p1):
    """(critical flux, downstream saturation temperature at the choke), golden-section search."""
    a, b = max(T_MIN, t1 - 80.0), t1
    g = (math.sqrt(5.0) - 1.0) / 2.0
    c, d = b - g * (b - a), a + g * (b - a)
    fc, fd = hem_flux(t1, p1, c), hem_flux(t1, p1, d)
    for _ in range(40):
        if b - a < 0.01:
            break
        if fc > fd:
            b, d, fd = d, c, fc
            c = b - g * (b - a)
            fc = hem_flux(t1, p1, c)
        else:
            a, c, fc = c, d, fd
            d = a + g * (b - a)
            fd = hem_flux(t1, p1, d)
    tm = 0.5 * (a + b)
    return hem_flux(t1, p1, tm), tm


def dyer_flux(t1, p1, p2, choke):
    """Dyer two-phase injector mass flux per unit CdA (kg/m^2/s). choke = hem_choke(t1, p1)."""
    if p1 <= p2:
        return 0.0
    rl = rho_l(t1)
    spi = math.sqrt(2.0 * rl * (p1 - p2))
    pv = psat(t1)
    if p2 >= pv:
        return spi
    g_crit, t_choke = choke
    if p2 <= psat(t_choke):
        hem = g_crit
    else:
        hem = hem_flux(t1, p1, tsat(p2))
    k = math.sqrt((p1 - p2) / (pv - p2))
    return k / (1.0 + k) * spi + 1.0 / (1.0 + k) * hem


def gas_flux(p1, rho1, gamma, p2):
    """Compressible orifice flux per unit CdA from stagnation (p1, rho1), choked or subsonic."""
    if p1 <= p2 or p1 <= 0.0:
        return 0.0
    crit = (2.0 / (gamma + 1.0)) ** (gamma / (gamma - 1.0))
    r = max(p2 / p1, crit)
    term = 2.0 * gamma / (gamma - 1.0) * (r ** (2.0 / gamma) - r ** ((gamma + 1.0) / gamma))
    return math.sqrt(max(term, 0.0) * p1 * rho1)


# ------------------------------------------------------------------ c* table and nozzle

class CeaTable:
    """The team's NASA CEA surface sliced to the nozzle's expansion ratio (hybrid_preset.py):
    c*, chamber gamma, vacuum C_F and exit pressure ratio over ln Pc x O/F x N2O enthalpy x ABS mass fraction."""

    FIELDS = ('cstar', 'gamma', 'cf_vac', 'pe_pc')

    def __init__(self, data: dict):
        self.axes = [[math.log(p) for p in data['pc_pa']], list(data['of']), list(data['h_cal_mol']),
                     list(data['abs_fraction'])]
        self.eps = data['expansion_ratio']
        self.f = [data['fields'][k] for k in self.FIELDS]
        n = self.axes
        self.strides = [len(n[1]) * len(n[2]) * len(n[3]), len(n[2]) * len(n[3]), len(n[3]), 1]

    @staticmethod
    def _cell(axis, x):
        x = min(max(x, axis[0]), axis[-1])
        i = min(max(bisect.bisect_right(axis, x) - 1, 0), len(axis) - 2)
        return i, (x - axis[i]) / (axis[i + 1] - axis[i])

    def lookup(self, of: float, pc: float, h_cal_mol: float, abs_frac: float) -> tuple[float, float, float, float]:
        """(c* m/s, gamma, vacuum C_F, pe/pc), multilinear, clamped to the grid."""
        cells = [self._cell(self.axes[0], math.log(max(pc, 1.0))), self._cell(self.axes[1], of),
                 self._cell(self.axes[2], h_cal_mol), self._cell(self.axes[3], abs_frac)]
        out = [0.0, 0.0, 0.0, 0.0]
        for corner in range(16):
            w = 1.0
            idx = 0
            for d in range(4):
                bit = (corner >> d) & 1
                i, u = cells[d]
                w *= u if bit else 1.0 - u
                idx += (i + bit) * self.strides[d]
            if w == 0.0:
                continue
            for k in range(4):
                out[k] += w * self.f[k][idx]
        return out[0], out[1], out[2], out[3]


# N2O enthalpy on the CEA surface's basis: CEA gas dHf(298) = 81.6 kJ/mol (the surface's reference) plus the
# real-fluid enthalpy relative to the ideal gas at 298.15 K, from ESDU 91022 (saturated vapour at T_MIN taken as
# ideal) and the NIST Shomate ideal-gas heat capacity of N2O (Chase 1998).
N2O_M = 0.0440128
N2O_CEA_DHF_J_MOL = 81600.0


def _cp_ig(t):
    x = t / 1000.0
    return 27.67988 + 51.14898 * x - 30.64454 * x * x + 6.847911 * x ** 3 - 0.157906 / (x * x)


def _ideal_gas_298_esdu():
    n = 200
    dh = sum(_cp_ig(T_MIN + (298.15 - T_MIN) * (i + 0.5) / n) for i in range(n)) * (298.15 - T_MIN) / n / N2O_M
    return h_v(T_MIN) + dh


H_IG_298 = _ideal_gas_298_esdu()


def n2o_enthalpy_cal_mol(t, liquid=True):
    h = h_l(t) if liquid else h_v(t)
    return (N2O_CEA_DHF_J_MOL + (h - H_IG_298) * N2O_M) / 4.184


def area_ratio(gamma, pr):
    """Supersonic area ratio A/A* at static/stagnation pressure ratio pr."""
    g = gamma
    a = ((g + 1.0) / 2.0) ** (1.0 / (g - 1.0)) * pr ** (1.0 / g)
    b = math.sqrt((g + 1.0) / (g - 1.0) * (1.0 - pr ** ((g - 1.0) / g)))
    return 1.0 / (a * b)


def exit_pressure_ratio(gamma, eps):
    """pe/pc for supersonic flow at area ratio eps (bisection in ln pr)."""
    if eps <= 1.0:
        return (2.0 / (gamma + 1.0)) ** (gamma / (gamma - 1.0))
    lo, hi = math.log(1e-6), math.log((2.0 / (gamma + 1.0)) ** (gamma / (gamma - 1.0)))
    for _ in range(60):
        mid = 0.5 * (lo + hi)
        if area_ratio(gamma, math.exp(mid)) > eps:
            lo = mid
        else:
            hi = mid
    return math.exp(0.5 * (lo + hi))


def thrust_coefficient(gamma, eps, pc, pa, eta_nozzle, sep_ratio=0.4):
    """(C_F, effective exit area ratio). Momentum term times eta_nozzle plus the exit pressure term;
    Summerfield separation: the flow leaves the wall where p = sep_ratio * pa."""
    if pc <= pa:
        return 0.0, eps
    g = gamma
    pr = exit_pressure_ratio(g, eps)
    e = eps
    if pr * pc < sep_ratio * pa:
        pr = sep_ratio * pa / pc
        crit = (2.0 / (g + 1.0)) ** (g / (g - 1.0))
        if pr >= crit:
            # not even the throat flows supersonically to the separation pressure: expand to ambient
            pr = min(pa / pc, 1.0)
            e = 1.0
        else:
            e = area_ratio(g, pr)
    k = 2.0 * g * g / (g - 1.0) * (2.0 / (g + 1.0)) ** ((g + 1.0) / (g - 1.0))
    mom = math.sqrt(k * max(1.0 - pr ** ((g - 1.0) / g), 0.0))
    if e == 1.0 and pa / pc > (2.0 / (g + 1.0)) ** (g / (g - 1.0)):
        return eta_nozzle * mom, e
    return eta_nozzle * mom + (pr - pa / pc) * e, e


# ------------------------------------------------------------------ spec



def nozzle_cf(cea_cf_vac, cea_pe_pc, gamma, eps, pc, pa, eta_nozzle, sep_ratio=0.4):
    """C_F at ambient pa: the CEA vacuum C_F and exit pressure (momentum term x eta_nozzle, plus the exit
    pressure term) while the nozzle flows full; the ideal-gamma Summerfield model when it separates."""
    if pc <= pa:
        return 0.0
    if cea_pe_pc * pc >= sep_ratio * pa:
        mom = cea_cf_vac - cea_pe_pc * eps
        return eta_nozzle * mom + (cea_pe_pc - pa / pc) * eps
    return thrust_coefficient(gamma, eps, pc, pa, eta_nozzle, sep_ratio)[0]


# ------------------------------------------------------------------ grain geometry

class Grain:
    """Port area, burning perimeter and ABS share of the burned material vs web burned (w), tabulated from the
    port outline (circular, or an N-point star) offset by w and clipped by the grain OD (docs section 3.2).

    The port at web w is every point within w of the initial port (the Minkowski sum with a disk of radius w):
    star tips round off, the points burn out into a near-circular port, and the last fuel is the sliver between
    the outline and the OD. The region stays star-shaped about the axis, so its boundary is r(theta, w) and
    A(w) = 1/2 int min(r, R_out)^2 dtheta; the burning perimeter is dA/dw (exact for offset fronts; the part on
    the OD drops out). The ABS share of each burned shell weights the radial ABS profile by the shell's area."""

    N_W = 160
    N_THETA = 240

    def __init__(self, g: dict, abs_profile: dict | None):
        self.shape = g['port_shape']
        self.L = g['grain_length_m']
        self.r_out = g['grain_od_m'] / 2.0
        self.rho_p = g['paraffin_rho']
        self.rho_a = g['abs_rho']
        self.prof_edges = abs_profile['r_edges_m'] if abs_profile else None
        self.prof_share = abs_profile['abs_share'] if abs_profile else None
        self.abs_uniform = g.get('abs_share_uniform', 0.0)
        if self.shape == 'circular':
            self.n = 0
            self.r0 = g['port_d_m'] / 2.0
            self.r_min = self.r0
        else:
            self.n = int(g['star_points'])
            self.rt = g['star_tip_radius_m']
            self.rr = g['star_root_radius_m']
            self.fil = g.get('star_tip_fillet_m', 0.0)
            self.r_min = self.rr
            # core star: tips pulled in by the fillet so the offset by `fil` restores the tip radius
            half = math.pi / self.n
            self.core = [(self.rt - self.fil, 0.0), (self.rr, half)]
            if self.fil > 0.0:
                # pull the core root in so the filleted outline still has its root at rr
                lo, hi = 0.2 * self.rr, self.rr
                for _ in range(40):
                    mid = 0.5 * (lo + hi)
                    self.core = [(self.rt - self.fil, 0.0), (mid, half)]
                    if self.boundary_r(half, 0.0) > self.rr:
                        hi = mid
                    else:
                        lo = mid
                self.core = [(self.rt - self.fil, 0.0), (0.5 * (lo + hi), half)]
        self.w_max = self.r_out - self.r_min
        self._tabulate()

    def abs_share_at(self, r):
        if self.prof_edges is None:
            return self.abs_uniform
        e = self.prof_edges
        if r <= e[0]:
            return self.prof_share[0]
        if r >= e[-1]:
            return self.prof_share[-1]
        i = min(bisect.bisect_right(e, r) - 1, len(self.prof_share) - 1)
        return self.prof_share[i]

    def _dist_core(self, x, y):
        """Distance from (x, y) to the core star polygon region (0 inside), using the sector symmetry."""
        th = math.atan2(y, x)
        half = math.pi / self.n
        th = math.fmod(th, 2 * half)
        if th < 0:
            th += 2 * half
        if th > half:
            th = 2 * half - th
        r = math.hypot(x, y)
        px, py = r * math.cos(th), r * math.sin(th)
        (rt, _), (rr, _) = self.core
        ax, ay = rt, 0.0
        bx, by = rr * math.cos(half), rr * math.sin(half)
        # inside test: the region in this half-sector is the triangle origin-A-B (star-shaped about the origin)
        cross = (bx - ax) * (py - ay) - (by - ay) * (px - ax)
        if cross >= 0.0 and th <= half:
            return 0.0
        ex, ey = bx - ax, by - ay
        t = min(max(((px - ax) * ex + (py - ay) * ey) / (ex * ex + ey * ey), 0.0), 1.0)
        return math.hypot(px - (ax + t * ex), py - (ay + t * ey))

    def boundary_r(self, theta, w):
        """Port boundary radius at angle theta after burning w (unclipped)."""
        if self.n == 0:
            return self.r0 + w
        reach = w + self.fil
        lo, hi = 0.0, self.rt + reach + 1e-6
        for _ in range(40):
            mid = 0.5 * (lo + hi)
            if self._dist_core(mid * math.cos(theta), mid * math.sin(theta)) <= reach:
                lo = mid
            else:
                hi = mid
        return 0.5 * (lo + hi)

    def _tabulate(self):
        n_w = self.N_W
        self.w = [self.w_max * i / n_w for i in range(n_w + 1)]
        if self.n == 0:
            thetas = [0.0]
            span = 2.0 * math.pi
        else:
            span = 2.0 * math.pi
            half = math.pi / self.n
            thetas = [half * (j + 0.5) / self.N_THETA for j in range(self.N_THETA)]
        radii = [[min(self.boundary_r(th, w), self.r_out) for th in thetas] for w in self.w]
        dth = span / len(thetas)
        self.area = [0.5 * sum(r * r for r in row) * dth for row in radii]
        # ABS share of the material burned between w[i] and w[i+1] (area-weighted over the shell)
        self.abs_shell = []
        for i in range(n_w):
            num = den = 0.0
            for a, b in zip(radii[i], radii[i + 1]):
                if b <= a:
                    continue
                k = 6
                for s in range(k):
                    r = a + (b - a) * (s + 0.5) / k
                    da = r * (b - a) / k
                    num += self.abs_share_at(r) * da
                    den += da
            self.abs_shell.append(num / den if den > 0 else 0.0)
        # perimeter = dA/dw at the nodes (central differences, one-sided at the ends)
        self.perim = []
        for i in range(n_w + 1):
            a, b = max(i - 1, 0), min(i + 1, n_w)
            self.perim.append((self.area[b] - self.area[a]) / (self.w[b] - self.w[a]))
        self.port_area0 = self.area[0]
        # fuel mass per shell and in total
        self.shell_mass = [self.L * (self.area[i + 1] - self.area[i]) * (self.rho_p * (1 - f) + self.rho_a * f)
                           for i, f in enumerate(self.abs_shell)]
        self.abs_mass = sum(self.L * (self.area[i + 1] - self.area[i]) * self.rho_a * f
                            for i, f in enumerate(self.abs_shell))
        self.mass = sum(self.shell_mass)

    def at(self, w):
        """(port area, burning perimeter, ABS share of the surface) at web w."""
        if w >= self.w_max:
            return math.pi * self.r_out ** 2, 0.0, 0.0
        x = w / self.w_max * self.N_W
        i = min(int(x), self.N_W - 1)
        u = x - i
        a = self.area[i] + (self.area[i + 1] - self.area[i]) * u
        p = self.perim[i] + (self.perim[i + 1] - self.perim[i]) * u
        return a, max(p, 0.0), self.abs_shell[i]

    def mass_burned(self, w):
        if w >= self.w_max:
            return self.mass
        x = w / self.w_max * self.N_W
        i = min(int(x), self.N_W - 1)
        return sum(self.shell_mass[:i]) + self.shell_mass[i] * (x - i)


# ------------------------------------------------------------------ spec

class ModelData:
    """The tables a burn needs: the CEA surface slice and the radial ABS profile."""

    def __init__(self, cea: CeaTable, abs_profile: dict | None):
        self.cea = cea
        self.abs_profile = abs_profile
        self._grains: dict = {}

    def grain(self, g: dict) -> Grain:
        key = json.dumps(g, sort_keys=True)
        if key not in self._grains:
            self._grains[key] = Grain(g, self.abs_profile if g.get('abs_profile') == 'gyroid_print' else None)
        return self._grains[key]


def load_spec(path: Path = SPEC_PATH) -> dict:
    return json.loads(path.read_text())


def load_data(cea_path: Path = CEA_PATH, grain_path: Path = GRAIN_PROFILE_PATH) -> ModelData:
    return ModelData(CeaTable(json.loads(cea_path.read_text())), json.loads(grain_path.read_text()))


def value(node):
    """A spec entry is a number or {"value": x, ...}."""
    return node['value'] if isinstance(node, dict) else node


def grain_config(spec: dict, name: str | None = None) -> dict:
    """A grain block from spec['grains'] (default: the flight grain) as plain values."""
    name = name or spec['flight_grain']
    return {k: value(v) for k, v in spec['grains'][name].items() if not k.startswith('_')}


def params(spec: dict, overrides: dict | None = None, grain: str | None = None, calibrated: bool = True) -> dict:
    """Flat SI parameters: geometry, then model priors, then the calibrated values; 'grain' is the grain block."""
    p = {k: value(v) for k, v in spec['geometry'].items() if not k.startswith('_')}
    for k, v in spec.get('model', {}).items():
        if not k.startswith('_'):
            p[k] = value(v)
    if calibrated:
        for k, v in spec.get('calibrated', {}).get('values', {}).items():
            p[k] = v
    p['grain'] = grain_config(spec, grain)
    if overrides:
        p.update(overrides)
    return p


# ------------------------------------------------------------------ motor

class HybridMotor:
    """Stepped state of one burn. Build it at valve open; call step() each dt."""

    def __init__(self, p: dict, data: ModelData, fill_kg: float, tank_c: float, supercharge_psig: float = 0.0,
                 ambient_pa: float = ATM):
        self.p = p
        self.data = data
        self.grain = data.grain(p['grain'])
        self.V = p['tank_volume_l'] / 1000.0
        self.m = fill_kg * p['fill_scale']  # the GS fill reading times the calibrated reading-to-mass factor
        self.T = tank_c + 273.15
        self.t = 0.0
        vl = self.liquid_volume()
        vu = max(self.V - vl, 0.02 * self.V)
        p_sc = supercharge_psig * PSI + ambient_pa
        self.mn = max(p_sc - psat(self.T), 0.0) * vu / (R_N2 * self.T)
        self.a_inj = p['injector_holes'] * math.pi / 4.0 * p['injector_hole_d_m'] ** 2
        self.w = 0.0
        self.dt_throat = p['throat_d_m']
        self.eps = (p['exit_d_m'] / p['throat_d_m']) ** 2
        self.fuel_burned = 0.0
        self.abs_burned = 0.0
        self.fuel0 = self.grain.mass
        self.ox_out = 0.0
        self.pc = ambient_pa
        self.done = False

    # tank -------------------------------------------------------------
    def liquid_volume(self):
        rl, rv = rho_l(self.T), rho_v(self.T)
        return min(max((self.m - rv * self.V) / (rl - rv), 0.0), 0.995 * self.V)

    def ullage(self):
        return max(self.V - self.liquid_volume(), 0.005 * self.V)

    def n2o_pressure(self):
        if self.m <= 0.0:
            return 0.0
        if self.liquid_volume() > 0.0:
            return psat(self.T)
        return psat(self.T) * min(self.m / self.V / rho_v(self.T), 1.0)

    def tank_pressure(self):
        return self.n2o_pressure() + self.mn * R_N2 * self.T / self.ullage()

    def heat_capacity_liquid(self):
        return self.liquid_volume() * rho_l(self.T) * cp_l(self.T) + self.p['tank_wall_kg'] * self.p['wall_cp'] * self.p['wall_coupling']

    # flows -------------------------------------------------------------
    def ox_flow(self, pc, liquid, choke):
        """(N2O, N2) flow (kg/s) into a chamber at pc. Liquid: the run valve (an orifice of Cd_v A_v in series)
        then the Dyer injector; after the liquid, compressible flow through the injector."""
        cda = self.p['injector_cd'] * self.a_inj
        pt = self.tank_pressure()
        if liquid:
            rl = rho_l(self.T)
            cva = self.p['valve_cd'] * self.p['valve_area_m2']
            mdot = 0.0
            for i in range(4):
                p1 = pt - (mdot / cva) ** 2 / (2.0 * rl)
                if p1 <= pc:
                    return 0.0, 0.0
                new = cda * dyer_flux(self.T, p1, pc, choke)
                mdot = 0.5 * (mdot + new) if i else new
            return mdot, 0.0
        vu = self.ullage()
        mg = self.m + self.mn
        if mg <= 0.0:
            return 0.0, 0.0
        rho = mg / vu
        xn = self.mn / mg
        gamma = (1.0 - xn) * GAMMA_N2O_VAPOUR + xn * GAMMA_N2
        # vapour_cd_scale: the tail carries entrained liquid and foam (two-phase), not dry vapour; calibrated
        mdot = cda * self.p['vapour_cd_scale'] * gas_flux(pt, rho, gamma, pc)
        return mdot * (1.0 - xn), mdot * xn

    def fuel_rates(self, mdot_ox):
        """(fuel kg/s, ABS mass fraction of it, web regression rate m/s). The surface is paraffin with the
        ABS share f of the gyroid matrix; the front recedes at the area-weighted rate of the two laws and
        consumes both materials."""
        p = self.p
        area, perim, f = self.grain.at(self.w)
        if perim <= 0.0 or mdot_ox <= 0.0:
            return 0.0, 0.0, 0.0
        gox = mdot_ox / area
        s = p['regression_scale']
        r_par = s * p['paraffin_a'] * gox ** p['paraffin_n']
        r_abs = s * p['abs_a'] * gox ** p['abs_n']
        rdot = (1.0 - f) * r_par + f * r_abs
        g = self.grain
        dens = g.rho_p * (1.0 - f) + g.rho_a * f
        mdot = dens * perim * g.L * rdot
        return mdot, (g.rho_a * f / dens if dens > 0 else 0.0), rdot

    def chamber(self, mdot_ox, fr, mdot_n2, pc, h_ox):
        """(required chamber pressure, O/F, CEA row)."""
        mf, wabs, _ = fr
        if mdot_ox <= 0.0:
            return 0.0, 0.0, None
        of = mdot_ox / mf if mf > 1e-9 else 1e3
        row = self.data.cea.lookup(of, pc, h_ox, wabs)
        cs = row[0]
        tot = mdot_ox + mf + mdot_n2
        cs *= (mdot_ox + mf) / tot if tot > 0 else 1.0  # inert nitrogen dilutes the products
        at = math.pi / 4.0 * self.dt_throat ** 2
        return tot * self.p['eta_cstar'] * cs / at, of, row

    # step --------------------------------------------------------------
    def step(self, dt, ambient_pa):
        p = self.p
        out = {'t': self.t, 'p_tank': self.tank_pressure(), 'T_tank': self.T, 'm_n2o': self.m, 'm_n2': self.mn}
        if self.done:
            out.update(thrust=0.0, pc=ambient_pa, mdot_ox=0.0, mdot_f=0.0, of=0.0, liquid=False, abs_frac=0.0)
            self.t += dt
            return out
        liquid = self.liquid_volume() > 0.0
        pt = out['p_tank']
        lit = self.t >= p['ignition_delay_s']
        choke = hem_choke(self.T, pt) if liquid else None
        h_ox = n2o_enthalpy_cal_mol(self.T, liquid)

        def residual(pc):
            n2o, n2 = self.ox_flow(pc, liquid, choke)
            fr = self.fuel_rates(n2o) if lit else (0.0, 0.0, 0.0)
            need, of, row = self.chamber(n2o, fr, n2, pc, h_ox) if lit else (ambient_pa, 0.0, None)
            return need - pc, (n2o, n2, fr, of, row)

        lo, hi = ambient_pa, max(pt, ambient_pa)
        f_lo, _ = residual(lo)
        if f_lo <= 0.0 or not lit:
            pc = ambient_pa
        else:
            f_hi, _ = residual(hi)
            if f_hi >= 0.0:
                pc = hi
            else:
                side = 0
                pc = lo
                for _ in range(60):
                    pc = (lo * f_hi - hi * f_lo) / (f_hi - f_lo)
                    f, _ = residual(pc)
                    if abs(f) < 1.0 or hi - lo < 1.0:
                        break
                    if f > 0.0:
                        lo, f_lo = pc, f
                        if side == 1:
                            f_hi *= 0.5
                        side = 1
                    else:
                        hi, f_hi = pc, f
                        if side == -1:
                            f_lo *= 0.5
                        side = -1
        _, (n2o, n2, fr, of, row) = residual(pc)
        n2o = min(n2o, self.m / dt)
        n2 = min(n2, self.mn / dt)
        thrust = 0.0
        if lit and pc > ambient_pa and row is not None:
            cf = nozzle_cf(row[2], row[3], row[1], self.eps, pc, ambient_pa, p['eta_nozzle'])
            thrust = max(cf * pc * math.pi / 4.0 * self.dt_throat ** 2, 0.0)
        out.update(thrust=thrust, pc=pc, mdot_ox=n2o, mdot_n2=n2, mdot_f=fr[0], of=of, liquid=liquid,
                   abs_frac=fr[1], web=self.w, port_area=self.grain.at(self.w)[0])
        if liquid:
            vap_before = self.m - self.liquid_volume() * rho_l(self.T)
            self.m -= n2o * dt
            vap_after = self.m - self.liquid_volume() * rho_l(self.T)
            self.T -= (vap_after - vap_before) * latent(self.T) / max(self.heat_capacity_liquid(), 1.0)
        else:
            cv_n2o = R_N2O / (GAMMA_N2O_VAPOUR - 1.0)
            cv_n2 = R_N2 / (GAMMA_N2 - 1.0)
            c = self.m * cv_n2o + self.mn * cv_n2 + p['tank_wall_kg'] * p['wall_cp'] * p['wall_coupling']
            self.T += -(R_N2O * n2o + R_N2 * n2) * self.T * dt / max(c, 1e-6)
            self.m -= n2o * dt
            self.mn -= n2 * dt
        self.T = min(max(self.T, T_MIN), T_MAX)
        self.m = max(self.m, 0.0)
        self.mn = max(self.mn, 0.0)
        self.ox_out += n2o * dt
        if lit:
            self.w = min(self.w + fr[2] * dt, self.grain.w_max)
            self.fuel_burned += fr[0] * dt
            self.abs_burned += fr[0] * fr[1] * dt
            self.dt_throat += p['throat_erosion_m_per_s'] * dt
        if self.tank_pressure() - ambient_pa < p['end_dp_pa'] or self.t > p['max_burn_s']:
            self.done = True
        self.t += dt
        return out


def simulate(p: dict, data: ModelData, fill_kg: float, tank_c: float, supercharge_psig: float = 0.0,
             ambient_pa: float = ATM, dt: float = 0.01) -> dict:
    """Run a burn to the end. Returns the time series (lists) and a summary."""
    m = HybridMotor(p, data, fill_kg, tank_c, supercharge_psig, ambient_pa)
    keys = ['t', 'thrust', 'pc', 'p_tank', 'T_tank', 'mdot_ox', 'mdot_f', 'of', 'm_n2o', 'abs_frac', 'web']
    series = {k: [] for k in keys}
    series['prop_left'] = []
    series['cg_from_aft'] = []
    liquid_end = None
    while not m.done:
        o = m.step(dt, ambient_pa)
        for k in keys:
            series[k].append(o.get(k, 0.0))
        series['prop_left'].append(m.m + m.mn + (m.fuel0 - m.fuel_burned))
        series['cg_from_aft'].append(prop_cg(m))
        if liquid_end is None and not o['liquid'] and m.t > p['ignition_delay_s']:
            liquid_end = o['t']
    for k in keys:
        series[k].append(0.0 if k in ('thrust', 'mdot_ox', 'mdot_f') else series[k][-1])
    series['t'][-1] = m.t
    series['prop_left'].append(m.m + m.mn + m.fuel0 - m.fuel_burned)
    series['cg_from_aft'].append(prop_cg(m))
    s = series
    imp = sum(0.5 * (a + b) * (t1 - t0) for a, b, t0, t1 in zip(s['thrust'], s['thrust'][1:], s['t'], s['t'][1:]))
    peak = max(s['thrust'])
    on = next((t for t, f in zip(s['t'], s['thrust']) if f >= 0.05 * peak), 0.0)
    off = next((t for t, f in zip(reversed(s['t']), reversed(s['thrust'])) if f >= 0.05 * peak), 0.0)
    summary = {
        'impulse_ns': imp,
        'peak_thrust_n': peak,
        'burn_time_s': off - on,
        'end_s': m.t,
        'liquid_end_s': liquid_end if liquid_end is not None else m.t,
        'ox_used_kg': m.ox_out,
        'ox_left_kg': m.m,
        'fuel_used_kg': m.fuel_burned,
        'abs_used_kg': m.abs_burned,
        'fuel_initial_kg': m.fuel0,
        'mean_of': m.ox_out / m.fuel_burned if m.fuel_burned > 0 else 0.0,
        'web_burned_m': m.w,
        'peak_pc_pa': max(s['pc']),
        'isp_s': imp / ((m.ox_out + m.fuel_burned) * G0) if m.ox_out > 0 else 0.0,
    }
    return {'series': series, 'summary': summary}


def prop_cg(m: HybridMotor) -> float:
    """Propellant CG (m from the nozzle exit): liquid settled aft in the tank, vapour/N2 above it, fuel at the grain."""
    p = m.p
    a_tank = math.pi / 4.0 * p['tank_id_m'] ** 2
    l_tank = m.V / a_tank
    vl = m.liquid_volume()
    hl = vl / a_tank
    ml = vl * rho_l(m.T)
    mg = max(m.m - ml + m.mn, 0.0)
    x0 = p['tank_aft_from_nozzle_exit_m']
    xl = x0 + hl / 2.0
    xg = x0 + hl + (l_tank - hl) / 2.0
    mf = m.fuel0 - m.fuel_burned
    xf = p['grain_aft_from_nozzle_exit_m'] + m.grain.L / 2.0
    tot = ml + mg + mf
    return (ml * xl + mg * xg + mf * xf) / tot if tot > 0 else xf


def curve_for_flight(res: dict, dt_out: float = 0.05) -> tuple[list[float], list[float], list[float]]:
    """(t, thrust, propellant left) resampled for the flight model."""
    s = res['series']
    t, f, pl = s['t'], s['thrust'], s['prop_left']
    out_t, out_f, out_m = [], [], []
    x = 0.0
    j = 0
    while x <= t[-1] + 1e-9:
        while j < len(t) - 2 and t[j + 1] < x:
            j += 1
        k = 0.0 if t[j + 1] == t[j] else min(max((x - t[j]) / (t[j + 1] - t[j]), 0.0), 1.0)
        out_t.append(round(x, 6))
        out_f.append(f[j] + (f[j + 1] - f[j]) * k)
        out_m.append(pl[j] + (pl[j + 1] - pl[j]) * k)
        x += dt_out
    return out_t, out_f, out_m


def main(argv):
    import argparse
    ap = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    ap.add_argument('--fill', type=float, default=5.65)
    ap.add_argument('--tank-c', type=float, default=None, help='default: saturated at --tank-psig')
    ap.add_argument('--tank-psig', type=float, default=404.1)
    ap.add_argument('--supercharge-psig', type=float, default=0.0)
    ap.add_argument('--ambient-pa', type=float, default=None)
    ap.add_argument('--csv', action='store_true', help='print the time series')
    ap.add_argument('--grain', default=None, help='a grain from the spec (default: the flight grain)')
    a = ap.parse_args(argv)
    ap_grain = a.grain
    spec = load_spec()
    p = params(spec, grain=ap_grain)
    tc = a.tank_c if a.tank_c is not None else tsat((a.tank_psig + 14.696) * PSI) - 273.15
    amb = a.ambient_pa if a.ambient_pa is not None else p['test_ambient_pa']
    res = simulate(p, load_data(), a.fill, tc, a.supercharge_psig, amb)
    if a.csv:
        s = res['series']
        print('t_s,thrust_n,pc_psia,tank_psia,tank_c,mdot_ox,mdot_f,of')
        for i in range(len(s['t'])):
            print(f"{s['t'][i]:.3f},{s['thrust'][i]:.1f},{s['pc'][i] / PSI:.1f},{s['p_tank'][i] / PSI:.1f},"
                  f"{s['T_tank'][i] - 273.15:.2f},{s['mdot_ox'][i]:.3f},{s['mdot_f'][i]:.4f},{s['of'][i]:.2f}")
    print(json.dumps({k: round(v, 4) for k, v in res['summary'].items()}, indent=1))
    return 0


if __name__ == '__main__':
    import sys
    sys.exit(main(sys.argv[1:]))
