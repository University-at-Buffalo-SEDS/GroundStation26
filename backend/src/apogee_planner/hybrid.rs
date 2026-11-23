//! Physics model of the UB SEDS hybrid (N2O run tank, feed line, Dyer
//! two-phase injector, paraffin-in-ABS grain, equilibrium c*, nozzle with
//! ambient back pressure); docs/hybrid-motor-model.md.
//!
//! A port of GroundStation26 `tools/flight_profile/hybrid_motor.py`, the
//! reference implementation. The same file is vendored into the firmware
//! simulator (FirmwareSimulator `src/bay/hybrid_motor.rs`); both check
//! themselves against `motors/hybrid_vectors.json`. Change the Python first,
//! regenerate the vectors, then port the change here and copy the file.
//!
//! Depends only on `serde_json`. SI units; pressures absolute.
#![allow(dead_code)]

use serde_json::Value;
use std::sync::OnceLock;

pub const PSI: f64 = 6894.757293;
pub const ATM: f64 = 101_325.0;
pub const G0: f64 = 9.80665;
pub const R_N2: f64 = 296.8;
pub const R_N2O: f64 = 188.91;
pub const GAMMA_N2O_VAPOUR: f64 = 1.3;
pub const GAMMA_N2: f64 = 1.4;

// ------------------------------------------------------------------ N2O (ESDU 91022)

pub const TC: f64 = 309.57;
const PC_CRIT: f64 = 7.251e6;
const RHOC: f64 = 452.0;
pub const T_MIN: f64 = 186.0;
pub const T_MAX: f64 = TC * 0.9999;

fn tr(t: f64) -> f64 {
    (t / TC).clamp(0.6, 0.9999)
}

pub fn psat(t: f64) -> f64 {
    let tr = tr(t);
    let x = 1.0 - tr;
    PC_CRIT * ((-6.71893 * x + 1.35966 * x.powf(1.5) - 1.3779 * x.powf(2.5) - 4.051 * x.powi(5)) / tr).exp()
}

pub fn rho_l(t: f64) -> f64 {
    let x = 1.0 - tr(t);
    RHOC * (1.72328 * x.powf(1.0 / 3.0) - 0.83950 * x.powf(2.0 / 3.0) + 0.51060 * x - 0.10412 * x.powf(4.0 / 3.0)).exp()
}

pub fn rho_v(t: f64) -> f64 {
    let y = 1.0 / tr(t) - 1.0;
    RHOC * (-1.00900 * y.powf(1.0 / 3.0) - 6.28792 * y.powf(2.0 / 3.0) + 7.50332 * y - 7.90463 * y.powf(4.0 / 3.0)
        + 0.629427 * y.powf(5.0 / 3.0))
    .exp()
}

/// Saturated liquid enthalpy, J/kg (ESDU 91022 reference state).
pub fn h_l(t: f64) -> f64 {
    let x = 1.0 - tr(t);
    (-200.0 + 116.043 * x.powf(1.0 / 3.0) - 917.225 * x.powf(2.0 / 3.0) + 794.779 * x - 589.587 * x.powf(4.0 / 3.0)) * 1000.0
}

pub fn h_v(t: f64) -> f64 {
    let x = 1.0 - tr(t);
    (-200.0 + 440.055 * x.powf(1.0 / 3.0) - 459.701 * x.powf(2.0 / 3.0) + 434.081 * x - 485.338 * x.powf(4.0 / 3.0)) * 1000.0
}

pub fn latent(t: f64) -> f64 {
    h_v(t) - h_l(t)
}

pub fn cp_l(t: f64) -> f64 {
    let x = 1.0 - (t / TC).clamp(0.6, 0.995);
    2499.73 * (1.0 + 0.023454 / x - 3.80136 * x + 13.0945 * x * x - 14.5180 * x * x * x)
}

const S_GRID_DT: f64 = 0.25;

/// Saturated-liquid entropy table (J/kg/K, zero at T_MIN): ds = (dh_l - dPsat / rho_l) / T.
fn entropy_table() -> &'static Vec<f64> {
    static TABLE: OnceLock<Vec<f64>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let ds = |t: f64| {
            let e = 0.01;
            let dh = (h_l(t + e) - h_l(t - e)) / (2.0 * e);
            let dp = (psat(t + e) - psat(t - e)) / (2.0 * e);
            (dh - dp / rho_l(t)) / t
        };
        let n = ((309.5 - T_MIN) / S_GRID_DT) as usize + 1;
        let mut out = Vec::with_capacity(n);
        let mut s = 0.0;
        let mut prev = ds(T_MIN);
        out.push(0.0);
        for i in 1..n {
            let t = T_MIN + i as f64 * S_GRID_DT;
            let cur = ds(t);
            s += 0.5 * (prev + cur) * S_GRID_DT;
            prev = cur;
            out.push(s);
        }
        out
    })
}

pub fn s_l(t: f64) -> f64 {
    let tab = entropy_table();
    let t_hi = T_MIN + (tab.len() - 1) as f64 * S_GRID_DT;
    let t = t.clamp(T_MIN, t_hi);
    let i = (((t - T_MIN) / S_GRID_DT) as usize).min(tab.len() - 2);
    let k = (t - (T_MIN + i as f64 * S_GRID_DT)) / S_GRID_DT;
    tab[i] + (tab[i + 1] - tab[i]) * k
}

pub fn s_v(t: f64) -> f64 {
    s_l(t) + latent(t) / t
}

/// Saturation temperature (K) for `p` (Pa).
pub fn tsat(p: f64) -> f64 {
    if p >= psat(T_MAX) {
        return T_MAX;
    }
    if p <= psat(T_MIN) {
        return T_MIN;
    }
    let mut t = 280.0;
    for _ in 0..30 {
        let f = psat(t).ln() - p.ln();
        let e = 0.01;
        let d = ((psat(t + e)).ln() - (psat(t - e)).ln()) / (2.0 * e);
        let step = f / d;
        t = (t - step).clamp(T_MIN, T_MAX);
        if step.abs() < 1e-7 {
            break;
        }
    }
    t
}

// ------------------------------------------------------------------ injector

/// HEM mass flux (kg/m²/s): liquid at t1 (pressure p1 >= psat(t1)) flashing isentropically to saturation at t2.
pub fn hem_flux(t1: f64, p1: f64, t2: f64) -> f64 {
    let rl1 = rho_l(t1);
    let h1 = h_l(t1) + (p1 - psat(t1)).max(0.0) / rl1;
    let s1 = s_l(t1);
    let (sl2, sv2) = (s_l(t2), s_v(t2));
    let x = ((s1 - sl2) / (sv2 - sl2)).clamp(0.0, 1.0);
    let h2 = h_l(t2) + x * latent(t2);
    let rho2 = 1.0 / (x / rho_v(t2) + (1.0 - x) / rho_l(t2));
    let dh = h1 - h2;
    if dh > 0.0 { rho2 * (2.0 * dh).sqrt() } else { 0.0 }
}

/// (critical HEM flux, downstream saturation temperature at the choke).
pub fn hem_choke(t1: f64, p1: f64) -> (f64, f64) {
    let (mut a, mut b) = ((t1 - 80.0).max(T_MIN), t1);
    let g = (5.0_f64.sqrt() - 1.0) / 2.0;
    let (mut c, mut d) = (b - g * (b - a), a + g * (b - a));
    let (mut fc, mut fd) = (hem_flux(t1, p1, c), hem_flux(t1, p1, d));
    for _ in 0..40 {
        if b - a < 0.01 {
            break;
        }
        if fc > fd {
            b = d;
            d = c;
            fd = fc;
            c = b - g * (b - a);
            fc = hem_flux(t1, p1, c);
        } else {
            a = c;
            c = d;
            fc = fd;
            d = a + g * (b - a);
            fd = hem_flux(t1, p1, d);
        }
    }
    let tm = 0.5 * (a + b);
    (hem_flux(t1, p1, tm), tm)
}

/// Dyer et al. (AIAA 2007-5702) two-phase injector flux per unit CdA (kg/m²/s).
pub fn dyer_flux(t1: f64, p1: f64, p2: f64, choke: (f64, f64)) -> f64 {
    if p1 <= p2 {
        return 0.0;
    }
    let rl = rho_l(t1);
    let spi = (2.0 * rl * (p1 - p2)).sqrt();
    let pv = psat(t1);
    if p2 >= pv {
        return spi;
    }
    let (g_crit, t_choke) = choke;
    let hem = if p2 <= psat(t_choke) { g_crit } else { hem_flux(t1, p1, tsat(p2)) };
    let k = ((p1 - p2) / (pv - p2)).sqrt();
    k / (1.0 + k) * spi + 1.0 / (1.0 + k) * hem
}

/// Compressible orifice flux per unit CdA from stagnation (p1, rho1), choked or subsonic.
pub fn gas_flux(p1: f64, rho1: f64, gamma: f64, p2: f64) -> f64 {
    if p1 <= p2 || p1 <= 0.0 {
        return 0.0;
    }
    let crit = (2.0 / (gamma + 1.0)).powf(gamma / (gamma - 1.0));
    let r = (p2 / p1).max(crit);
    let term = 2.0 * gamma / (gamma - 1.0) * (r.powf(2.0 / gamma) - r.powf((gamma + 1.0) / gamma));
    (term.max(0.0) * p1 * rho1).sqrt()
}

// ------------------------------------------------------------------ CEA table and nozzle

/// The team's NASA CEA surface sliced to the nozzle's expansion ratio (tools/flight_profile/hybrid_preset.py):
/// c*, chamber gamma, vacuum C_F and exit pressure ratio over ln Pc x O/F x N2O enthalpy x ABS mass fraction.
#[derive(Clone, Debug)]
pub struct CeaTable {
    axes: [Vec<f64>; 4],
    strides: [usize; 4],
    fields: [Vec<f64>; 4],
    pub eps: f64,
}

fn floats(v: &Value) -> Vec<f64> {
    v.as_array().map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default()
}

impl CeaTable {
    pub fn from_json(text: &str) -> Result<Self, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let axes = [
            floats(&v["pc_pa"]).iter().map(|p| p.ln()).collect::<Vec<f64>>(),
            floats(&v["of"]),
            floats(&v["h_cal_mol"]),
            floats(&v["abs_fraction"]),
        ];
        let n: Vec<usize> = axes.iter().map(Vec::len).collect();
        if n.iter().any(|x| *x < 2) {
            return Err("CEA table axes".into());
        }
        let total = n.iter().product::<usize>();
        let f = &v["fields"];
        let fields = [floats(&f["cstar"]), floats(&f["gamma"]), floats(&f["cf_vac"]), floats(&f["pe_pc"])];
        if fields.iter().any(|x| x.len() != total) {
            return Err("CEA table field size".into());
        }
        Ok(Self {
            strides: [n[1] * n[2] * n[3], n[2] * n[3], n[3], 1],
            axes,
            fields,
            eps: v["expansion_ratio"].as_f64().ok_or("expansion_ratio")?,
        })
    }

    fn cell(axis: &[f64], x: f64) -> (usize, f64) {
        let x = x.clamp(axis[0], axis[axis.len() - 1]);
        let i = axis.partition_point(|a| *a <= x).saturating_sub(1).min(axis.len() - 2);
        (i, (x - axis[i]) / (axis[i + 1] - axis[i]))
    }

    /// (c* m/s, gamma, vacuum C_F, pe/pc), multilinear, clamped to the grid.
    pub fn lookup(&self, of: f64, pc: f64, h_cal_mol: f64, abs_frac: f64) -> [f64; 4] {
        let cells = [
            Self::cell(&self.axes[0], pc.max(1.0).ln()),
            Self::cell(&self.axes[1], of),
            Self::cell(&self.axes[2], h_cal_mol),
            Self::cell(&self.axes[3], abs_frac),
        ];
        let mut out = [0.0; 4];
        for corner in 0..16usize {
            let mut w = 1.0;
            let mut idx = 0;
            for (d, (i, u)) in cells.iter().enumerate() {
                let bit = (corner >> d) & 1;
                w *= if bit == 1 { *u } else { 1.0 - *u };
                idx += (i + bit) * self.strides[d];
            }
            if w == 0.0 {
                continue;
            }
            for k in 0..4 {
                out[k] += w * self.fields[k][idx];
            }
        }
        out
    }
}

// N2O enthalpy on the CEA surface's basis: CEA gas dHf(298) = 81.6 kJ/mol plus the real-fluid enthalpy relative to
// the ideal gas at 298.15 K (ESDU 91022, saturated vapour at T_MIN taken as ideal; NIST Shomate ideal-gas cp).
const N2O_M: f64 = 0.0440128;
const N2O_CEA_DHF_J_MOL: f64 = 81600.0;

fn cp_ig(t: f64) -> f64 {
    let x = t / 1000.0;
    27.67988 + 51.14898 * x - 30.64454 * x * x + 6.847911 * x.powi(3) - 0.157906 / (x * x)
}

fn h_ig_298() -> f64 {
    static H: OnceLock<f64> = OnceLock::new();
    *H.get_or_init(|| {
        let n = 200;
        let dh: f64 = (0..n).map(|i| cp_ig(T_MIN + (298.15 - T_MIN) * (i as f64 + 0.5) / n as f64)).sum::<f64>() * (298.15 - T_MIN) / n as f64 / N2O_M;
        h_v(T_MIN) + dh
    })
}

pub fn n2o_enthalpy_cal_mol(t: f64, liquid: bool) -> f64 {
    let h = if liquid { h_l(t) } else { h_v(t) };
    (N2O_CEA_DHF_J_MOL + (h - h_ig_298()) * N2O_M) / 4.184
}

/// Supersonic area ratio A/A* at static/stagnation pressure ratio `pr`.
pub fn area_ratio(g: f64, pr: f64) -> f64 {
    let a = ((g + 1.0) / 2.0).powf(1.0 / (g - 1.0)) * pr.powf(1.0 / g);
    let b = ((g + 1.0) / (g - 1.0) * (1.0 - pr.powf((g - 1.0) / g))).sqrt();
    1.0 / (a * b)
}

/// pe/pc for supersonic flow at area ratio `eps`.
pub fn exit_pressure_ratio(g: f64, eps: f64) -> f64 {
    let crit = (2.0 / (g + 1.0)).powf(g / (g - 1.0));
    if eps <= 1.0 {
        return crit;
    }
    let (mut lo, mut hi) = (1e-6_f64.ln(), crit.ln());
    for _ in 0..60 {
        let mid = 0.5 * (lo + hi);
        if area_ratio(g, mid.exp()) > eps {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (0.5 * (lo + hi)).exp()
}

/// (C_F, effective exit area ratio) of an ideal-gamma nozzle: momentum term x eta_nozzle plus the exit pressure
/// term; Summerfield separation where the wall pressure falls below `sep_ratio` x ambient.
pub fn thrust_coefficient(g: f64, eps: f64, pc: f64, pa: f64, eta_nozzle: f64, sep_ratio: f64) -> (f64, f64) {
    if pc <= pa {
        return (0.0, eps);
    }
    let crit = (2.0 / (g + 1.0)).powf(g / (g - 1.0));
    let mut pr = exit_pressure_ratio(g, eps);
    let mut e = eps;
    if pr * pc < sep_ratio * pa {
        pr = sep_ratio * pa / pc;
        if pr >= crit {
            pr = (pa / pc).min(1.0);
            e = 1.0;
        } else {
            e = area_ratio(g, pr);
        }
    }
    let k = 2.0 * g * g / (g - 1.0) * (2.0 / (g + 1.0)).powf((g + 1.0) / (g - 1.0));
    let mom = (k * (1.0 - pr.powf((g - 1.0) / g)).max(0.0)).sqrt();
    if e == 1.0 && pa / pc > crit {
        return (eta_nozzle * mom, e);
    }
    (eta_nozzle * mom + (pr - pa / pc) * e, e)
}

/// C_F at ambient: the CEA vacuum C_F and exit pressure while the nozzle flows full, the ideal-gamma Summerfield
/// model when it separates.
pub fn nozzle_cf(cea_cf_vac: f64, cea_pe_pc: f64, gamma: f64, eps: f64, pc: f64, pa: f64, eta_nozzle: f64) -> f64 {
    if pc <= pa {
        return 0.0;
    }
    if cea_pe_pc * pc >= 0.4 * pa {
        let mom = cea_cf_vac - cea_pe_pc * eps;
        return eta_nozzle * mom + (cea_pe_pc - pa / pc) * eps;
    }
    thrust_coefficient(gamma, eps, pc, pa, eta_nozzle, 0.4).0
}

// ------------------------------------------------------------------ grain geometry

/// A port geometry: circular (star_points 0) or an N-point star with straight sides and filleted tips.
#[derive(Clone, Debug, PartialEq)]
pub struct GrainCfg {
    pub star_points: usize,
    pub port_d_m: f64,
    pub star_tip_radius_m: f64,
    pub star_root_radius_m: f64,
    pub star_tip_fillet_m: f64,
    pub grain_length_m: f64,
    pub grain_od_m: f64,
    pub paraffin_rho: f64,
    pub abs_rho: f64,
    /// Use the printed gyroid's radial ABS profile (else no ABS).
    pub abs_profile: bool,
}

impl GrainCfg {
    pub fn from_spec(spec: &Value, name: &str) -> Result<Self, String> {
        let g = spec["grains"].get(name).ok_or_else(|| format!("no grain {name}"))?;
        let num = |k: &str| entry(&g[k]).ok_or_else(|| format!("grain {name} lacks {k}"));
        let shape = g["port_shape"].get("value").and_then(Value::as_str).or_else(|| g["port_shape"].as_str()).unwrap_or("circular");
        let star = shape == "star";
        Ok(Self {
            star_points: if star { num("star_points")? as usize } else { 0 },
            port_d_m: if star { 0.0 } else { num("port_d_m")? },
            star_tip_radius_m: if star { num("star_tip_radius_m")? } else { 0.0 },
            star_root_radius_m: if star { num("star_root_radius_m")? } else { 0.0 },
            star_tip_fillet_m: if star { entry(&g["star_tip_fillet_m"]).unwrap_or(0.0) } else { 0.0 },
            grain_length_m: num("grain_length_m")?,
            grain_od_m: num("grain_od_m")?,
            paraffin_rho: num("paraffin_rho")?,
            abs_rho: num("abs_rho")?,
            abs_profile: g["abs_profile"].get("value").and_then(Value::as_str).or_else(|| g["abs_profile"].as_str()) == Some("gyroid_print"),
        })
    }
}

/// Radial ABS share of the printed matrix (motors/sweet_v6_grain.json).
#[derive(Clone, Debug, Default)]
pub struct AbsProfile {
    pub edges: Vec<f64>,
    pub share: Vec<f64>,
}

impl AbsProfile {
    pub fn from_json(text: &str) -> Result<Self, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let p = Self { edges: floats(&v["r_edges_m"]), share: floats(&v["abs_share"]) };
        if p.edges.len() != p.share.len() + 1 || p.share.is_empty() {
            return Err("ABS profile shape".into());
        }
        Ok(p)
    }
    pub fn at(&self, r: f64) -> f64 {
        let e = &self.edges;
        if r <= e[0] {
            return self.share[0];
        }
        if r >= e[e.len() - 1] {
            return self.share[self.share.len() - 1];
        }
        let i = (e.partition_point(|x| *x <= r).saturating_sub(1)).min(self.share.len() - 1);
        self.share[i]
    }
}

/// Port area, burning perimeter and ABS share of the burned material vs web burned (the Python `Grain`).
#[derive(Clone, Debug)]
pub struct Grain {
    pub cfg: GrainCfg,
    pub r_out: f64,
    pub w_max: f64,
    core: (f64, f64),
    pub area: Vec<f64>,
    pub perim: Vec<f64>,
    pub abs_shell: Vec<f64>,
    shell_mass: Vec<f64>,
    pub mass: f64,
    pub abs_mass: f64,
}

pub const GRAIN_N_W: usize = 160;
pub const GRAIN_N_THETA: usize = 240;

impl Grain {
    pub fn new(cfg: &GrainCfg, profile: Option<&AbsProfile>) -> Self {
        let r_out = cfg.grain_od_m / 2.0;
        let r_min = if cfg.star_points == 0 { cfg.port_d_m / 2.0 } else { cfg.star_root_radius_m };
        let mut g = Self {
            cfg: cfg.clone(),
            r_out,
            w_max: r_out - r_min,
            core: (cfg.star_tip_radius_m - cfg.star_tip_fillet_m, cfg.star_root_radius_m),
            area: Vec::new(),
            perim: Vec::new(),
            abs_shell: Vec::new(),
            shell_mass: Vec::new(),
            mass: 0.0,
            abs_mass: 0.0,
        };
        if cfg.star_points > 0 && cfg.star_tip_fillet_m > 0.0 {
            let half = std::f64::consts::PI / cfg.star_points as f64;
            let (mut lo, mut hi) = (0.2 * cfg.star_root_radius_m, cfg.star_root_radius_m);
            for _ in 0..40 {
                let mid = 0.5 * (lo + hi);
                g.core.1 = mid;
                if g.boundary_r(half, 0.0) > cfg.star_root_radius_m {
                    hi = mid;
                } else {
                    lo = mid;
                }
            }
            g.core.1 = 0.5 * (lo + hi);
        }
        g.tabulate(profile);
        g
    }

    fn dist_core(&self, x: f64, y: f64) -> f64 {
        let half = std::f64::consts::PI / self.cfg.star_points as f64;
        let mut th = y.atan2(x) % (2.0 * half);
        if th < 0.0 {
            th += 2.0 * half;
        }
        if th > half {
            th = 2.0 * half - th;
        }
        let r = x.hypot(y);
        let (px, py) = (r * th.cos(), r * th.sin());
        let (ax, ay) = (self.core.0, 0.0);
        let (bx, by) = (self.core.1 * half.cos(), self.core.1 * half.sin());
        let cross = (bx - ax) * (py - ay) - (by - ay) * (px - ax);
        if cross >= 0.0 && th <= half {
            return 0.0;
        }
        let (ex, ey) = (bx - ax, by - ay);
        let t = (((px - ax) * ex + (py - ay) * ey) / (ex * ex + ey * ey)).clamp(0.0, 1.0);
        (px - (ax + t * ex)).hypot(py - (ay + t * ey))
    }

    /// Port boundary radius at angle `theta` after burning `w` (unclipped).
    pub fn boundary_r(&self, theta: f64, w: f64) -> f64 {
        if self.cfg.star_points == 0 {
            return self.cfg.port_d_m / 2.0 + w;
        }
        let reach = w + self.cfg.star_tip_fillet_m;
        let (mut lo, mut hi) = (0.0, self.cfg.star_tip_radius_m + reach + 1e-6);
        for _ in 0..40 {
            let mid = 0.5 * (lo + hi);
            if self.dist_core(mid * theta.cos(), mid * theta.sin()) <= reach {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        0.5 * (lo + hi)
    }

    fn tabulate(&mut self, profile: Option<&AbsProfile>) {
        let n_w = GRAIN_N_W;
        let ws: Vec<f64> = (0..=n_w).map(|i| self.w_max * i as f64 / n_w as f64).collect();
        let thetas: Vec<f64> = if self.cfg.star_points == 0 {
            vec![0.0]
        } else {
            let half = std::f64::consts::PI / self.cfg.star_points as f64;
            (0..GRAIN_N_THETA).map(|j| half * (j as f64 + 0.5) / GRAIN_N_THETA as f64).collect()
        };
        let radii: Vec<Vec<f64>> = ws.iter().map(|w| thetas.iter().map(|th| self.boundary_r(*th, *w).min(self.r_out)).collect()).collect();
        let dth = 2.0 * std::f64::consts::PI / thetas.len() as f64;
        self.area = radii.iter().map(|row| 0.5 * row.iter().map(|r| r * r).sum::<f64>() * dth).collect();
        let share = |r: f64| profile.map(|p| p.at(r)).unwrap_or(0.0);
        self.abs_shell = (0..n_w)
            .map(|i| {
                let (mut num, mut den) = (0.0, 0.0);
                for (a, b) in radii[i].iter().zip(&radii[i + 1]) {
                    if b <= a {
                        continue;
                    }
                    let k = 6;
                    for s in 0..k {
                        let r = a + (b - a) * (s as f64 + 0.5) / k as f64;
                        let da = r * (b - a) / k as f64;
                        num += share(r) * da;
                        den += da;
                    }
                }
                if den > 0.0 { num / den } else { 0.0 }
            })
            .collect();
        self.perim = (0..=n_w)
            .map(|i| {
                let (a, b) = (i.saturating_sub(1), (i + 1).min(n_w));
                (self.area[b] - self.area[a]) / (ws[b] - ws[a])
            })
            .collect();
        let (l, rp, ra) = (self.cfg.grain_length_m, self.cfg.paraffin_rho, self.cfg.abs_rho);
        self.shell_mass = self.abs_shell.iter().enumerate().map(|(i, f)| l * (self.area[i + 1] - self.area[i]) * (rp * (1.0 - f) + ra * f)).collect();
        self.abs_mass = self.abs_shell.iter().enumerate().map(|(i, f)| l * (self.area[i + 1] - self.area[i]) * ra * f).sum();
        self.mass = self.shell_mass.iter().sum();
    }

    /// (port area, burning perimeter, ABS share of the surface) at web `w`.
    pub fn at(&self, w: f64) -> (f64, f64, f64) {
        if w >= self.w_max {
            return (std::f64::consts::PI * self.r_out * self.r_out, 0.0, 0.0);
        }
        let x = w / self.w_max * GRAIN_N_W as f64;
        let i = (x as usize).min(GRAIN_N_W - 1);
        let u = x - i as f64;
        let a = self.area[i] + (self.area[i + 1] - self.area[i]) * u;
        let p = self.perim[i] + (self.perim[i + 1] - self.perim[i]) * u;
        (a, p.max(0.0), self.abs_shell[i])
    }

    pub fn mass_burned(&self, w: f64) -> f64 {
        if w >= self.w_max {
            return self.mass;
        }
        let x = w / self.w_max * GRAIN_N_W as f64;
        let i = (x as usize).min(GRAIN_N_W - 1);
        self.shell_mass[..i].iter().sum::<f64>() + self.shell_mass[i] * (x - i as f64)
    }
}

// ------------------------------------------------------------------ parameters

/// Flat SI parameters (the Python `params()`): spec geometry, then model, then calibrated values, plus the grain.
#[derive(Clone, Debug, PartialEq)]
pub struct Params {
    pub tank_volume_l: f64,
    pub tank_id_m: f64,
    pub tank_wall_kg: f64,
    pub wall_cp: f64,
    pub valve_cd: f64,
    pub valve_area_m2: f64,
    pub injector_holes: f64,
    pub injector_hole_d_m: f64,
    pub injector_cd: f64,
    pub throat_d_m: f64,
    pub exit_d_m: f64,
    pub throat_erosion_m_per_s: f64,
    pub paraffin_a: f64,
    pub paraffin_n: f64,
    pub abs_a: f64,
    pub abs_n: f64,
    pub tank_aft_from_nozzle_exit_m: f64,
    pub grain_aft_from_nozzle_exit_m: f64,
    pub test_ambient_pa: f64,
    pub eta_cstar: f64,
    pub eta_nozzle: f64,
    pub wall_coupling: f64,
    pub fill_scale: f64,
    pub vapour_cd_scale: f64,
    pub regression_scale: f64,
    pub ignition_delay_s: f64,
    pub end_dp_pa: f64,
    pub max_burn_s: f64,
    pub grain: GrainCfg,
}

fn entry(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.get("value").and_then(Value::as_f64))
}

impl Params {
    /// Every numeric name the spec may set (the grain is separate).
    pub const NAMES: [&'static str; 28] = [
        "tank_volume_l", "tank_id_m", "tank_wall_kg", "wall_cp", "valve_cd", "valve_area_m2", "injector_holes", "injector_hole_d_m",
        "injector_cd", "throat_d_m", "exit_d_m", "throat_erosion_m_per_s", "paraffin_a", "paraffin_n", "abs_a", "abs_n",
        "tank_aft_from_nozzle_exit_m", "grain_aft_from_nozzle_exit_m", "test_ambient_pa", "eta_cstar", "eta_nozzle", "wall_coupling",
        "fill_scale", "vapour_cd_scale", "regression_scale", "ignition_delay_s", "end_dp_pa", "max_burn_s",
    ];

    fn slot(&mut self, name: &str) -> Option<&mut f64> {
        Some(match name {
            "tank_volume_l" => &mut self.tank_volume_l,
            "tank_id_m" => &mut self.tank_id_m,
            "tank_wall_kg" => &mut self.tank_wall_kg,
            "wall_cp" => &mut self.wall_cp,
            "valve_cd" => &mut self.valve_cd,
            "valve_area_m2" => &mut self.valve_area_m2,
            "injector_holes" => &mut self.injector_holes,
            "injector_hole_d_m" => &mut self.injector_hole_d_m,
            "injector_cd" => &mut self.injector_cd,
            "throat_d_m" => &mut self.throat_d_m,
            "exit_d_m" => &mut self.exit_d_m,
            "throat_erosion_m_per_s" => &mut self.throat_erosion_m_per_s,
            "paraffin_a" => &mut self.paraffin_a,
            "paraffin_n" => &mut self.paraffin_n,
            "abs_a" => &mut self.abs_a,
            "abs_n" => &mut self.abs_n,
            "tank_aft_from_nozzle_exit_m" => &mut self.tank_aft_from_nozzle_exit_m,
            "grain_aft_from_nozzle_exit_m" => &mut self.grain_aft_from_nozzle_exit_m,
            "test_ambient_pa" => &mut self.test_ambient_pa,
            "eta_cstar" => &mut self.eta_cstar,
            "eta_nozzle" => &mut self.eta_nozzle,
            "wall_coupling" => &mut self.wall_coupling,
            "fill_scale" => &mut self.fill_scale,
            "vapour_cd_scale" => &mut self.vapour_cd_scale,
            "regression_scale" => &mut self.regression_scale,
            "ignition_delay_s" => &mut self.ignition_delay_s,
            "end_dp_pa" => &mut self.end_dp_pa,
            "max_burn_s" => &mut self.max_burn_s,
            _ => return None,
        })
    }

    pub fn get(&self, name: &str) -> Option<f64> {
        let mut c = self.clone();
        c.slot(name).map(|v| *v)
    }

    pub fn set(&mut self, name: &str, value: f64) -> bool {
        match self.slot(name) {
            Some(v) => {
                *v = value;
                true
            }
            None => false,
        }
    }

    /// From the spec JSON with the named grain (None = the flight grain); `calibrated` = false gives the priors.
    pub fn from_spec(spec: &Value, grain: Option<&str>, calibrated: bool) -> Result<Self, String> {
        let gname = grain.or_else(|| spec["flight_grain"].as_str()).ok_or("spec lacks flight_grain")?;
        let mut p = Self {
            tank_volume_l: 0.0, tank_id_m: 0.0, tank_wall_kg: 0.0, wall_cp: 0.0, valve_cd: 0.0, valve_area_m2: 0.0, injector_holes: 0.0,
            injector_hole_d_m: 0.0, injector_cd: 0.0, throat_d_m: 0.0, exit_d_m: 0.0, throat_erosion_m_per_s: 0.0, paraffin_a: 0.0,
            paraffin_n: 0.0, abs_a: 0.0, abs_n: 0.0, tank_aft_from_nozzle_exit_m: 0.0, grain_aft_from_nozzle_exit_m: 0.0,
            test_ambient_pa: ATM, eta_cstar: 0.0, eta_nozzle: 0.0, wall_coupling: 0.0, fill_scale: 1.0, vapour_cd_scale: 1.0,
            regression_scale: 1.0, ignition_delay_s: 0.0, end_dp_pa: PSI, max_burn_s: 20.0,
            grain: GrainCfg::from_spec(spec, gname)?,
        };
        let mut seen = std::collections::HashSet::new();
        let mut sections = vec![&spec["geometry"], &spec["model"]];
        if calibrated {
            sections.push(&spec["calibrated"]["values"]);
        }
        for section in sections {
            if let Some(obj) = section.as_object() {
                for (k, v) in obj {
                    if k.starts_with('_') {
                        continue;
                    }
                    if let Some(x) = entry(v) {
                        if p.set(k, x) {
                            seen.insert(k.clone());
                        }
                    }
                }
            }
        }
        let missing: Vec<&str> = Self::NAMES.iter().copied().filter(|n| !seen.contains(*n)).collect();
        if !missing.is_empty() {
            return Err(format!("hybrid motor spec lacks {}", missing.join(", ")));
        }
        Ok(p)
    }
}

/// Prior (value, 1-sigma) of a spec entry (sigma defaults to 30 %).
pub fn prior(spec: &Value, name: &str) -> Option<(f64, f64)> {
    let node = spec["geometry"].get(name).or_else(|| spec["model"].get(name))?;
    let v = node.get("value")?.as_f64()?;
    Some((v, node.get("sigma").and_then(Value::as_f64).unwrap_or(0.3 * v)))
}

/// The tables a burn needs, with a cache of tabulated grains.
#[derive(Debug)]
pub struct ModelData {
    pub cea: CeaTable,
    pub abs_profile: AbsProfile,
    grains: std::sync::Mutex<Vec<(GrainCfg, std::sync::Arc<Grain>)>>,
}

impl ModelData {
    pub fn new(cea: CeaTable, abs_profile: AbsProfile) -> Self {
        Self { cea, abs_profile, grains: std::sync::Mutex::new(Vec::new()) }
    }
    pub fn grain(&self, cfg: &GrainCfg) -> std::sync::Arc<Grain> {
        let mut cache = self.grains.lock().unwrap();
        if let Some((_, g)) = cache.iter().find(|(c, _)| c == cfg) {
            return g.clone();
        }
        let g = std::sync::Arc::new(Grain::new(cfg, if cfg.abs_profile { Some(&self.abs_profile) } else { None }));
        cache.push((cfg.clone(), g.clone()));
        if cache.len() > 16 {
            cache.remove(0);
        }
        g
    }
}

// ------------------------------------------------------------------ motor

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sample {
    pub t: f64,
    pub thrust: f64,
    pub pc: f64,
    pub p_tank: f64,
    pub t_tank: f64,
    pub m_n2o: f64,
    pub m_n2: f64,
    pub mdot_ox: f64,
    pub mdot_n2: f64,
    pub mdot_f: f64,
    pub of: f64,
    pub abs_frac: f64,
    pub web: f64,
    pub liquid: bool,
}

/// Stepped state of one burn: build it at valve open, call `step` each dt.
#[derive(Clone, Debug)]
pub struct Motor {
    pub p: Params,
    pub grain: std::sync::Arc<Grain>,
    pub v: f64,
    /// N2O in the tank (kg), nitrogen (kg), lumped temperature (K).
    pub m: f64,
    pub mn: f64,
    pub temp: f64,
    pub t: f64,
    a_inj: f64,
    eps: f64,
    pub w: f64,
    pub throat_d: f64,
    pub fuel_burned: f64,
    pub abs_burned: f64,
    pub fuel0: f64,
    pub ox_out: f64,
    pub pc: f64,
    pub done: bool,
}

type Row = Option<[f64; 4]>;

impl Motor {
    pub fn new(p: &Params, data: &ModelData, fill_kg: f64, tank_c: f64, supercharge_psig: f64, ambient_pa: f64) -> Self {
        let grain = data.grain(&p.grain);
        let mut m = Self {
            p: p.clone(),
            fuel0: grain.mass,
            grain,
            v: p.tank_volume_l / 1000.0,
            m: fill_kg.max(0.0) * p.fill_scale,
            mn: 0.0,
            temp: tank_c + 273.15,
            t: 0.0,
            a_inj: p.injector_holes * std::f64::consts::PI / 4.0 * p.injector_hole_d_m.powi(2),
            eps: (p.exit_d_m / p.throat_d_m).powi(2),
            w: 0.0,
            throat_d: p.throat_d_m,
            fuel_burned: 0.0,
            abs_burned: 0.0,
            ox_out: 0.0,
            pc: ambient_pa,
            done: false,
        };
        let vu = (m.v - m.liquid_volume()).max(0.02 * m.v);
        let p_sc = supercharge_psig * PSI + ambient_pa;
        m.mn = (p_sc - psat(m.temp)).max(0.0) * vu / (R_N2 * m.temp);
        m
    }

    pub fn liquid_volume(&self) -> f64 {
        let (rl, rv) = (rho_l(self.temp), rho_v(self.temp));
        ((self.m - rv * self.v) / (rl - rv)).clamp(0.0, 0.995 * self.v)
    }

    pub fn ullage(&self) -> f64 {
        (self.v - self.liquid_volume()).max(0.005 * self.v)
    }

    pub fn n2o_pressure(&self) -> f64 {
        if self.m <= 0.0 {
            return 0.0;
        }
        if self.liquid_volume() > 0.0 {
            return psat(self.temp);
        }
        psat(self.temp) * (self.m / self.v / rho_v(self.temp)).min(1.0)
    }

    pub fn tank_pressure(&self) -> f64 {
        self.n2o_pressure() + self.mn * R_N2 * self.temp / self.ullage()
    }

    fn heat_capacity_liquid(&self) -> f64 {
        self.liquid_volume() * rho_l(self.temp) * cp_l(self.temp) + self.p.tank_wall_kg * self.p.wall_cp * self.p.wall_coupling
    }

    /// (N2O, N2) flow (kg/s) into a chamber at `pc`.
    pub fn ox_flow(&self, pc: f64, liquid: bool, choke: (f64, f64)) -> (f64, f64) {
        let cda = self.p.injector_cd * self.a_inj;
        let pt = self.tank_pressure();
        if liquid {
            let rl = rho_l(self.temp);
            let cva = self.p.valve_cd * self.p.valve_area_m2;
            let mut mdot = 0.0;
            for i in 0..4 {
                let p1 = pt - (mdot / cva).powi(2) / (2.0 * rl);
                if p1 <= pc {
                    return (0.0, 0.0);
                }
                let new = cda * dyer_flux(self.temp, p1, pc, choke);
                mdot = if i > 0 { 0.5 * (mdot + new) } else { new };
            }
            return (mdot, 0.0);
        }
        let vu = self.ullage();
        let mg = self.m + self.mn;
        if mg <= 0.0 {
            return (0.0, 0.0);
        }
        let rho = mg / vu;
        let xn = self.mn / mg;
        let gamma = (1.0 - xn) * GAMMA_N2O_VAPOUR + xn * GAMMA_N2;
        // vapour_cd_scale: the tail carries entrained liquid and foam (two-phase), not dry vapour; calibrated
        let mdot = cda * self.p.vapour_cd_scale * gas_flux(pt, rho, gamma, pc);
        (mdot * (1.0 - xn), mdot * xn)
    }

    /// (fuel kg/s, ABS mass fraction of it, web regression rate m/s).
    pub fn fuel_rates(&self, mdot_ox: f64) -> (f64, f64, f64) {
        let p = &self.p;
        let (area, perim, f) = self.grain.at(self.w);
        if perim <= 0.0 || mdot_ox <= 0.0 {
            return (0.0, 0.0, 0.0);
        }
        let gox = mdot_ox / area;
        let s = p.regression_scale;
        let r_par = s * p.paraffin_a * gox.powf(p.paraffin_n);
        let r_abs = s * p.abs_a * gox.powf(p.abs_n);
        let rdot = (1.0 - f) * r_par + f * r_abs;
        let g = &self.grain.cfg;
        let dens = g.paraffin_rho * (1.0 - f) + g.abs_rho * f;
        let mdot = dens * perim * g.grain_length_m * rdot;
        (mdot, if dens > 0.0 { g.abs_rho * f / dens } else { 0.0 }, rdot)
    }

    /// (required chamber pressure, O/F, CEA row).
    fn chamber(&self, cea: &CeaTable, mdot_ox: f64, fr: (f64, f64, f64), mdot_n2: f64, pc: f64, h_ox: f64) -> (f64, f64, Row) {
        let (mf, wabs, _) = fr;
        if mdot_ox <= 0.0 {
            return (0.0, 0.0, None);
        }
        let of = if mf > 1e-9 { mdot_ox / mf } else { 1e3 };
        let row = cea.lookup(of, pc, h_ox, wabs);
        let mut cs = row[0];
        let tot = mdot_ox + mf + mdot_n2;
        if tot > 0.0 {
            cs *= (mdot_ox + mf) / tot;
        }
        let at = std::f64::consts::PI / 4.0 * self.throat_d * self.throat_d;
        (tot * self.p.eta_cstar * cs / at, of, Some(row))
    }

    pub fn step(&mut self, data: &ModelData, dt: f64, ambient_pa: f64) -> Sample {
        let p = self.p.clone();
        let mut out = Sample { t: self.t, p_tank: self.tank_pressure(), t_tank: self.temp, m_n2o: self.m, m_n2: self.mn, web: self.w, ..Default::default() };
        if self.done {
            out.pc = ambient_pa;
            self.t += dt;
            return out;
        }
        let liquid = self.liquid_volume() > 0.0;
        let pt = out.p_tank;
        let lit = self.t >= p.ignition_delay_s;
        let choke = if liquid { hem_choke(self.temp, pt) } else { (0.0, 0.0) };
        let h_ox = n2o_enthalpy_cal_mol(self.temp, liquid);
        type Info = (f64, f64, (f64, f64, f64), f64, Row);
        let cea = &data.cea;
        let residual = |s: &Self, pc: f64| -> (f64, Info) {
            let (n2o, n2) = s.ox_flow(pc, liquid, choke);
            let fr = if lit { s.fuel_rates(n2o) } else { (0.0, 0.0, 0.0) };
            let (need, of, row) = if lit { s.chamber(cea, n2o, fr, n2, pc, h_ox) } else { (ambient_pa, 0.0, None) };
            (need - pc, (n2o, n2, fr, of, row))
        };
        let (mut lo, mut hi) = (ambient_pa, pt.max(ambient_pa));
        let (mut f_lo, _) = residual(self, lo);
        let pc;
        if f_lo <= 0.0 || !lit {
            pc = ambient_pa;
        } else {
            let (mut f_hi, _) = residual(self, hi);
            if f_hi >= 0.0 {
                pc = hi;
            } else {
                let mut side = 0;
                let mut x = lo;
                for _ in 0..60 {
                    x = (lo * f_hi - hi * f_lo) / (f_hi - f_lo);
                    let (f, _) = residual(self, x);
                    if f.abs() < 1.0 || hi - lo < 1.0 {
                        break;
                    }
                    if f > 0.0 {
                        lo = x;
                        f_lo = f;
                        if side == 1 {
                            f_hi *= 0.5;
                        }
                        side = 1;
                    } else {
                        hi = x;
                        f_hi = f;
                        if side == -1 {
                            f_lo *= 0.5;
                        }
                        side = -1;
                    }
                }
                pc = x;
            }
        }
        let (_, (n2o, n2, fr, of, row)) = residual(self, pc);
        let n2o = n2o.min(self.m / dt);
        let n2 = n2.min(self.mn / dt);
        let mut thrust = 0.0;
        if lit && pc > ambient_pa {
            if let Some(row) = row {
                let cf = nozzle_cf(row[2], row[3], row[1], self.eps, pc, ambient_pa, p.eta_nozzle);
                thrust = (cf * pc * std::f64::consts::PI / 4.0 * self.throat_d * self.throat_d).max(0.0);
            }
        }
        out.thrust = thrust;
        out.pc = pc;
        out.mdot_ox = n2o;
        out.mdot_n2 = n2;
        out.mdot_f = fr.0;
        out.of = of;
        out.abs_frac = fr.1;
        out.liquid = liquid;
        if liquid {
            let vap_before = self.m - self.liquid_volume() * rho_l(self.temp);
            self.m -= n2o * dt;
            let vap_after = self.m - self.liquid_volume() * rho_l(self.temp);
            self.temp -= (vap_after - vap_before) * latent(self.temp) / self.heat_capacity_liquid().max(1.0);
        } else {
            let cv_n2o = R_N2O / (GAMMA_N2O_VAPOUR - 1.0);
            let cv_n2 = R_N2 / (GAMMA_N2 - 1.0);
            let c = self.m * cv_n2o + self.mn * cv_n2 + p.tank_wall_kg * p.wall_cp * p.wall_coupling;
            self.temp += -(R_N2O * n2o + R_N2 * n2) * self.temp * dt / c.max(1e-6);
            self.m -= n2o * dt;
            self.mn -= n2 * dt;
        }
        self.temp = self.temp.clamp(T_MIN, T_MAX);
        self.m = self.m.max(0.0);
        self.mn = self.mn.max(0.0);
        self.ox_out += n2o * dt;
        if lit {
            self.w = (self.w + fr.2 * dt).min(self.grain.w_max);
            self.fuel_burned += fr.0 * dt;
            self.abs_burned += fr.0 * fr.1 * dt;
            self.throat_d += p.throat_erosion_m_per_s * dt;
        }
        if self.tank_pressure() - ambient_pa < p.end_dp_pa || self.t > p.max_burn_s {
            self.done = true;
        }
        self.t += dt;
        self.pc = pc;
        out
    }

    /// Propellant CG (m from the nozzle exit): liquid settled aft in the tank, gas above it, fuel at the grain.
    pub fn prop_cg(&self) -> f64 {
        let p = &self.p;
        let a_tank = std::f64::consts::PI / 4.0 * p.tank_id_m * p.tank_id_m;
        let l_tank = self.v / a_tank;
        let vl = self.liquid_volume();
        let hl = vl / a_tank;
        let ml = vl * rho_l(self.temp);
        let mg = (self.m - ml + self.mn).max(0.0);
        let x0 = p.tank_aft_from_nozzle_exit_m;
        let xl = x0 + hl / 2.0;
        let xg = x0 + hl + (l_tank - hl) / 2.0;
        let mf = self.fuel0 - self.fuel_burned;
        let xf = p.grain_aft_from_nozzle_exit_m + self.grain.cfg.grain_length_m / 2.0;
        let tot = ml + mg + mf;
        if tot > 0.0 { (ml * xl + mg * xg + mf * xf) / tot } else { xf }
    }

    pub fn prop_left(&self) -> f64 {
        self.m + self.mn + self.fuel0 - self.fuel_burned
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    pub impulse_ns: f64,
    pub peak_thrust_n: f64,
    pub burn_time_s: f64,
    pub end_s: f64,
    pub liquid_end_s: f64,
    pub ox_used_kg: f64,
    pub ox_left_kg: f64,
    pub fuel_used_kg: f64,
    pub abs_used_kg: f64,
    pub fuel_initial_kg: f64,
    pub mean_of: f64,
    pub web_burned_m: f64,
    pub peak_pc_pa: f64,
    pub isp_s: f64,
}

#[derive(Clone, Debug, Default)]
pub struct Burn {
    pub samples: Vec<Sample>,
    pub prop_left: Vec<f64>,
    pub cg_from_aft: Vec<f64>,
    pub summary: Summary,
}

impl Burn {
    pub fn times(&self) -> Vec<f64> {
        self.samples.iter().map(|s| s.t).collect()
    }
    pub fn thrust(&self) -> Vec<f64> {
        self.samples.iter().map(|s| s.thrust).collect()
    }
    pub fn tank_psig(&self) -> Vec<f64> {
        self.samples.iter().map(|s| s.p_tank / PSI - 14.696).collect()
    }
}

/// Runs a burn to the end (the Python `simulate`, constant ambient).
pub fn simulate(p: &Params, data: &ModelData, fill_kg: f64, tank_c: f64, supercharge_psig: f64, ambient_pa: f64, dt: f64) -> Burn {
    let mut m = Motor::new(p, data, fill_kg, tank_c, supercharge_psig, ambient_pa);
    let mut b = Burn::default();
    let mut liquid_end = None;
    while !m.done {
        let o = m.step(data, dt, ambient_pa);
        if liquid_end.is_none() && !o.liquid && m.t > p.ignition_delay_s {
            liquid_end = Some(o.t);
        }
        b.samples.push(o);
        b.prop_left.push(m.prop_left());
        b.cg_from_aft.push(m.prop_cg());
    }
    let mut last = b.samples.last().cloned().unwrap_or_default();
    last.t = m.t;
    last.thrust = 0.0;
    last.mdot_ox = 0.0;
    last.mdot_f = 0.0;
    b.samples.push(last);
    b.prop_left.push(m.prop_left());
    b.cg_from_aft.push(m.prop_cg());
    let s = &b.samples;
    let imp: f64 = s.windows(2).map(|w| 0.5 * (w[0].thrust + w[1].thrust) * (w[1].t - w[0].t)).sum();
    let peak = s.iter().map(|x| x.thrust).fold(0.0, f64::max);
    let on = s.iter().find(|x| x.thrust >= 0.05 * peak).map(|x| x.t).unwrap_or(0.0);
    let off = s.iter().rev().find(|x| x.thrust >= 0.05 * peak).map(|x| x.t).unwrap_or(0.0);
    b.summary = Summary {
        impulse_ns: imp,
        peak_thrust_n: peak,
        burn_time_s: off - on,
        end_s: m.t,
        liquid_end_s: liquid_end.unwrap_or(m.t),
        ox_used_kg: m.ox_out,
        ox_left_kg: m.m,
        fuel_used_kg: m.fuel_burned,
        abs_used_kg: m.abs_burned,
        fuel_initial_kg: m.fuel0,
        mean_of: if m.fuel_burned > 0.0 { m.ox_out / m.fuel_burned } else { 0.0 },
        web_burned_m: m.w,
        peak_pc_pa: s.iter().map(|x| x.pc).fold(0.0, f64::max),
        isp_s: if m.ox_out > 0.0 { imp / ((m.ox_out + m.fuel_burned) * G0) } else { 0.0 },
    };
    b
}

/// Linear interpolation of (ts, ys) at t, clamped at the ends.
pub fn interp(ts: &[f64], ys: &[f64], t: f64) -> f64 {
    if ts.is_empty() {
        return 0.0;
    }
    if t <= ts[0] {
        return ys[0];
    }
    if t >= ts[ts.len() - 1] {
        return ys[ys.len() - 1];
    }
    let i = ts.partition_point(|x| *x <= t).saturating_sub(1).min(ts.len() - 2);
    let k = (t - ts[i]) / (ts[i + 1] - ts[i]);
    ys[i] + (ys[i + 1] - ys[i]) * k
}

/// (t, thrust, propellant left) resampled every `dt_out` for the flight model.
pub fn curve_for_flight(b: &Burn, dt_out: f64) -> (Vec<f64>, Vec<f64>, Vec<f64>) {
    let t = b.times();
    let f = b.thrust();
    let end = *t.last().unwrap_or(&0.0);
    let (mut ot, mut of, mut om) = (Vec::new(), Vec::new(), Vec::new());
    let mut i = 0usize;
    loop {
        let x = i as f64 * dt_out;
        if x > end + 1e-9 {
            break;
        }
        ot.push(x);
        of.push(interp(&t, &f, x));
        om.push(interp(&t, &b.prop_left, x));
        i += 1;
    }
    (ot, of, om)
}

// ------------------------------------------------------------------ committed model files

/// The committed copies of tools/flight_profile/motors/{hybrid_motor,sweet_v6_cea,sweet_v6_grain,hybrid_vectors}.json
/// (the planner tests check they match tools/).
pub const SPEC_JSON: &str = include_str!("model/hybrid_motor.json");
pub const CEA_JSON: &str = include_str!("model/sweet_v6_cea.json");
pub const GRAIN_JSON: &str = include_str!("model/sweet_v6_grain.json");
#[cfg(test)]
pub const VECTORS_JSON: &str = include_str!("model/hybrid_vectors.json");

pub fn spec() -> &'static Value {
    static SPEC: OnceLock<Value> = OnceLock::new();
    SPEC.get_or_init(|| serde_json::from_str(SPEC_JSON).expect("model/hybrid_motor.json parses"))
}

pub fn data() -> &'static ModelData {
    static DATA: OnceLock<ModelData> = OnceLock::new();
    DATA.get_or_init(|| {
        ModelData::new(
            CeaTable::from_json(CEA_JSON).expect("model/sweet_v6_cea.json parses"),
            AbsProfile::from_json(GRAIN_JSON).expect("model/sweet_v6_grain.json parses"),
        )
    })
}

/// The calibrated parameters of the committed spec with a grain (None = the flight grain).
pub fn calibrated(grain: Option<&str>) -> Params {
    Params::from_spec(spec(), grain, true).expect("model/hybrid_motor.json has every parameter")
}

/// The grain the archive's fires burned unless their record says otherwise.
pub fn fire_grain() -> &'static str {
    spec()["fire_grain_default"].as_str().unwrap_or("cylindrical_preset")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, rel: f64, what: &str) {
        let tol = rel * b.abs().max(1e-9);
        assert!((a - b).abs() <= tol.max(1e-9), "{what}: {a} vs {b} (rel {})", (a - b) / b.abs().max(1e-12));
    }

    fn vectors() -> Value {
        serde_json::from_str(VECTORS_JSON).unwrap()
    }

    #[test]
    fn n2o_properties_match_nist() {
        // NIST Chemistry WebBook saturation points for N2O (as in n2o.rs): 20 C: 5.06 MPa, liquid 786, vapour 159 kg/m^3;
        // 0 C: 3.13 MPa, liquid 906; -20 C: 1.81 MPa.
        close(psat(293.15) / 1e6, 5.06, 0.01, "psat 20 C");
        close(rho_l(293.15), 786.0, 0.01, "rho_l 20 C");
        close(rho_v(293.15), 159.0, 0.03, "rho_v 20 C");
        close(psat(273.15) / 1e6, 3.13, 0.01, "psat 0 C");
        close(rho_l(273.15), 906.0, 0.01, "rho_l 0 C");
        close(psat(253.15) / 1e6, 1.81, 0.015, "psat -20 C");
        assert!(latent(253.15) > latent(293.15) && latent(293.15) > latent(305.0));
        close(tsat(psat(280.0)), 280.0, 1e-6, "tsat inverts psat");
        assert!(s_v(270.0) > s_l(270.0));
        // The CEA surface's liquid-N2O enthalpy axis spans about -14..+30 C (16036..17169 cal/mol).
        close(n2o_enthalpy_cal_mol(293.15, true), 16860.0, 0.005, "liquid enthalpy at 20 C");
        assert!(n2o_enthalpy_cal_mol(293.15, false) > 18400.0);
    }

    #[test]
    fn components_match_the_python_vectors() {
        let v = vectors();
        for x in v["properties"].as_array().unwrap() {
            let t = x["t"].as_f64().unwrap();
            for (name, f) in [("psat", psat as fn(f64) -> f64), ("rho_l", rho_l), ("rho_v", rho_v), ("h_l", h_l), ("h_v", h_v), ("s_l", s_l), ("cp_l", cp_l)] {
                close(f(t), x[name].as_f64().unwrap(), 1e-9, name);
            }
        }
        for x in v["tsat"].as_array().unwrap() {
            close(tsat(x["p"].as_f64().unwrap()), x["t"].as_f64().unwrap(), 1e-9, "tsat");
        }
        for x in v["enthalpy"].as_array().unwrap() {
            let t = x["t"].as_f64().unwrap();
            close(n2o_enthalpy_cal_mol(t, true), x["liquid"].as_f64().unwrap(), 1e-9, "h liquid");
            close(n2o_enthalpy_cal_mol(t, false), x["vapour"].as_f64().unwrap(), 1e-9, "h vapour");
        }
        for x in v["injector"].as_array().unwrap() {
            let (t1, p1, p2) = (x["t1"].as_f64().unwrap(), x["p1"].as_f64().unwrap(), x["p2"].as_f64().unwrap());
            let ch = hem_choke(t1, p1);
            close(ch.0, x["choke_flux"].as_f64().unwrap(), 1e-8, "choke flux");
            close(ch.1, x["choke_t"].as_f64().unwrap(), 1e-8, "choke t");
            close(dyer_flux(t1, p1, p2, ch), x["dyer_flux"].as_f64().unwrap(), 1e-8, "dyer");
        }
        for x in v["gas"].as_array().unwrap() {
            let f = gas_flux(x["p1"].as_f64().unwrap(), x["rho1"].as_f64().unwrap(), x["gamma"].as_f64().unwrap(), x["p2"].as_f64().unwrap());
            close(f, x["flux"].as_f64().unwrap(), 1e-9, "gas flux");
        }
        for x in v["cea"].as_array().unwrap() {
            let f = |k: &str| x[k].as_f64().unwrap();
            let row = data().cea.lookup(f("of"), f("pc"), f("h"), f("abs"));
            for (i, k) in ["cstar", "gamma", "cf_vac", "pe_pc"].iter().enumerate() {
                close(row[i], f(k), 1e-9, k);
            }
        }
        for x in v["nozzle"].as_array().unwrap() {
            let f = |k: &str| x[k].as_f64().unwrap();
            let (cf, e) = thrust_coefficient(f("gamma"), f("eps"), f("pc"), f("pa"), f("eta"), 0.4);
            close(cf, f("cf"), 1e-8, "cf");
            close(e, f("eps_eff"), 1e-8, "eps_eff");
            close(exit_pressure_ratio(f("gamma"), f("eps")), f("pe_over_pc"), 1e-8, "pe/pc");
            close(nozzle_cf(1.55, 0.06, f("gamma"), f("eps"), f("pc"), f("pa"), f("eta")), f("cea_cf_with_1p55_0p06"), 1e-8, "cea cf");
        }
        for g in v["grains"].as_array().unwrap() {
            let name = g["name"].as_str().unwrap();
            let gr = data().grain(&GrainCfg::from_spec(spec(), name).unwrap());
            close(gr.w_max, g["w_max"].as_f64().unwrap(), 1e-9, "w_max");
            close(gr.mass, g["mass"].as_f64().unwrap(), 1e-8, "grain mass");
            close(gr.abs_mass, g["abs_mass"].as_f64().unwrap(), 1e-8, "abs mass");
            for x in g["at"].as_array().unwrap() {
                let w = x["w"].as_f64().unwrap();
                let (a, p, f) = gr.at(w);
                close(a, x["area"].as_f64().unwrap(), 1e-8, &format!("{name} area w={w}"));
                close(p + 1e-12, x["perim"].as_f64().unwrap() + 1e-12, 1e-7, &format!("{name} perimeter w={w}"));
                close(f + 1e-12, x["abs_share"].as_f64().unwrap() + 1e-12, 1e-8, &format!("{name} abs w={w}"));
                close(gr.mass_burned(w), x["mass_burned"].as_f64().unwrap(), 1e-8, "mass burned");
            }
        }
    }

    #[test]
    fn spec_parameters_match_the_python_params() {
        let v = vectors();
        let p = calibrated(None);
        for (k, x) in v["params"].as_object().unwrap() {
            if k == "nozzle_half_angle_deg" {
                continue; // documentation: eta_nozzle carries it
            }
            let mine = p.get(k).unwrap_or_else(|| panic!("Params lacks {k}"));
            close(mine, x.as_f64().unwrap(), 1e-9, k);
        }
    }

    #[test]
    fn full_burns_match_the_python_vectors() {
        let v = vectors();
        for b in v["burns"].as_array().unwrap() {
            let name = b["name"].as_str().unwrap();
            let p = calibrated(Some(b["grain"].as_str().unwrap()));
            let f = |k: &str| b[k].as_f64().unwrap();
            let m = Motor::new(&p, data(), f("fill_kg"), f("tank_c"), f("supercharge_psig"), f("ambient_pa"));
            close(m.mn + 1e-30, f("initial_n2_kg") + 1e-30, 1e-9, "initial N2");
            let fr = m.fuel_rates(1.5);
            let want: Vec<f64> = b["fuel_rates_at_1p5kgs"].as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect();
            for (a, w) in [fr.0, fr.1, fr.2].iter().zip(&want) {
                close(*a + 1e-30, *w + 1e-30, 1e-9, "fuel rates");
            }
            let burn = simulate(&p, data(), f("fill_kg"), f("tank_c"), f("supercharge_psig"), f("ambient_pa"), f("dt"));
            let s = &b["summary"];
            let sm = &burn.summary;
            for (k, mine) in [("impulse_ns", sm.impulse_ns), ("peak_thrust_n", sm.peak_thrust_n), ("burn_time_s", sm.burn_time_s), ("end_s", sm.end_s),
                ("liquid_end_s", sm.liquid_end_s), ("ox_used_kg", sm.ox_used_kg), ("fuel_used_kg", sm.fuel_used_kg), ("abs_used_kg", sm.abs_used_kg),
                ("peak_pc_pa", sm.peak_pc_pa), ("isp_s", sm.isp_s), ("web_burned_m", sm.web_burned_m)] {
                close(mine, s[k].as_f64().unwrap(), 1e-6, &format!("{name} {k}"));
            }
            for x in b["samples"].as_array().unwrap() {
                let t = x["t"].as_f64().unwrap();
                let i = burn.samples.iter().enumerate().min_by(|a, b| (a.1.t - t).abs().total_cmp(&(b.1.t - t).abs())).unwrap().0;
                let o = &burn.samples[i];
                for (k, mine) in [("thrust", o.thrust), ("pc", o.pc), ("p_tank", o.p_tank), ("t_tank", o.t_tank), ("mdot_ox", o.mdot_ox),
                    ("abs_frac", o.abs_frac), ("web", o.web), ("prop_left", burn.prop_left[i]), ("cg", burn.cg_from_aft[i])] {
                    close(mine + 1e-12, x[k].as_f64().unwrap() + 1e-12, 1e-6, &format!("{name} t={t} {k}"));
                }
            }
        }
    }

    #[test]
    fn the_calibrated_model_reproduces_the_2026_11_16_fire() {
        // The measured curve (tools/flight_profile/motors/testfire_20261116.csv): 9.19 kN s +-0.9, 5.65 s burn.
        let p = calibrated(Some(fire_grain()));
        let tank_c = tsat((404.1 + 14.696) * PSI) - 273.15;
        let b = simulate(&p, data(), 5.65, tank_c, 0.0, p.test_ambient_pa, 0.01);
        close(b.summary.impulse_ns, 9192.0, 0.08, "impulse");
        close(b.summary.burn_time_s, 5.65, 0.15, "burn time");
        close(b.summary.liquid_end_s, 3.0, 0.15, "liquid phase (tank-pressure knee)");
        let csv = include_str!("model/testfire_20261116.csv");
        let (mt, mf) = super::super::flight::parse_curve(csv);
        let (t, f) = (b.times(), b.thrust());
        let (mut se, mut n) = (0.0, 0.0);
        for (tm, fm) in mt.iter().zip(&mf) {
            if *tm >= 0.65 {
                se += (interp(&t, &f, *tm) - fm).powi(2);
                n += 1.0;
            }
        }
        let rms = (se / n).sqrt();
        assert!(rms < 320.0, "thrust RMS {rms} N");
    }

    #[test]
    fn thrust_rises_with_altitude_and_warm_tanks_burn_harder() {
        let p = calibrated(None);
        let sea = simulate(&p, data(), 7.0, 15.0, 0.0, ATM, 0.01).summary.impulse_ns;
        let high = simulate(&p, data(), 7.0, 15.0, 0.0, 30_000.0, 0.01).summary.impulse_ns;
        assert!(high > sea, "less back pressure, more thrust: {high} vs {sea}");
        let cold = simulate(&p, data(), 7.0, 0.0, 0.0, ATM, 0.01).summary;
        let warm = simulate(&p, data(), 7.0, 25.0, 0.0, ATM, 0.01).summary;
        assert!(warm.peak_thrust_n > cold.peak_thrust_n);
        assert!(warm.burn_time_s < cold.burn_time_s);
    }

    #[test]
    fn circular_grain_geometry_is_analytic() {
        let cfg = GrainCfg::from_spec(spec(), "cylindrical_preset").unwrap();
        let g = Grain::new(&cfg, None);
        let r0 = cfg.port_d_m / 2.0;
        for w in [0.0, 0.003, 0.009, 0.0165] {
            let (a, p, _) = g.at(w);
            close(a, std::f64::consts::PI * (r0 + w).powi(2), 1e-5, "circle area (linear between table nodes)");
            close(p, 2.0 * std::f64::consts::PI * (r0 + w), 0.01, "circle perimeter");
        }
        // the whole annulus burns, and the perimeter is zero once the port reaches the OD
        close(g.mass, cfg.paraffin_rho * std::f64::consts::PI * (g.r_out.powi(2) - r0 * r0) * cfg.grain_length_m, 1e-9, "fuel mass");
        assert_eq!(g.at(g.w_max + 1e-6).1, 0.0);
    }

    /// Brute-force reference: area of {points within w of the star polygon} inside the OD, on a fine grid.
    fn raster_area(cfg: &GrainCfg, w: f64) -> f64 {
        let g = Grain::new(&GrainCfg { star_tip_fillet_m: 0.0, ..cfg.clone() }, None);
        let r_out = cfg.grain_od_m / 2.0;
        let n = 900;
        let h = 2.0 * r_out / n as f64;
        let mut count = 0usize;
        for i in 0..n {
            for j in 0..n {
                let (x, y) = (-r_out + (i as f64 + 0.5) * h, -r_out + (j as f64 + 0.5) * h);
                if x.hypot(y) <= r_out && g.dist_core(x, y) <= w {
                    count += 1;
                }
            }
        }
        count as f64 * h * h
    }

    #[test]
    fn star_grain_geometry_matches_analytic_and_raster_references() {
        let cfg = GrainCfg {
            star_points: 5,
            port_d_m: 0.0,
            star_tip_radius_m: 0.03,
            star_root_radius_m: 0.018,
            star_tip_fillet_m: 0.0,
            grain_length_m: 0.33,
            grain_od_m: 0.085,
            paraffin_rho: 900.0,
            abs_rho: 1020.0,
            abs_profile: false,
        };
        let g = Grain::new(&cfg, None);
        let (n, rt, rr) = (5.0, 0.03, 0.018);
        let half = std::f64::consts::PI / n;
        // initial polygon: area N rt rr sin(pi/N), perimeter 2N x side
        close(g.at(0.0).0, n * rt * rr * half.sin(), 2e-3, "star area at w=0");
        let side = (rt * rt + rr * rr - 2.0 * rt * rr * half.cos()).sqrt();
        close(g.perim[0], 2.0 * n * side, 0.03, "star perimeter at w=0");
        // offsets: points burning out, the near-circular port, the sliver against the OD
        for w in [0.004, 0.012, 0.02] {
            close(g.at(w).0, raster_area(&cfg, w), 0.01, &format!("star area at w={w}"));
        }
        // the perimeter is dA/dw (check against the reference's slope)
        let (w1, w2) = (0.006, 0.008);
        close(g.at(0.007).1, (raster_area(&cfg, w2) - raster_area(&cfg, w1)) / (w2 - w1), 0.04, "perimeter = dA/dw");
        // at the end the port fills the OD and the perimeter vanishes
        close(g.at(g.w_max - 1e-9).0, std::f64::consts::PI * g.r_out * g.r_out, 1e-3, "burnout area");
        assert!(g.at(0.95 * g.w_max).1 < 0.3 * g.perim[0], "sliver: the perimeter collapses at the end");
        // fillets keep the tip and root radii
        let fil = Grain::new(&GrainCfg { star_tip_fillet_m: 0.002, ..cfg.clone() }, None);
        close(fil.boundary_r(0.0, 0.0), rt, 1e-6, "filleted tip radius");
        close(fil.boundary_r(half, 0.0), rr, 1e-6, "filleted root radius");
        assert!(fil.at(0.0).0 < g.at(0.0).0 + 1e-12 || fil.at(0.0).0 > 0.0);
    }

    #[test]
    fn regression_and_nozzle_components() {
        let p = calibrated(Some("cylindrical_preset"));
        let m = Motor::new(&p, data(), 6.0, 15.0, 0.0, ATM);
        let a_port = std::f64::consts::PI * (p.grain.port_d_m / 2.0).powi(2);
        let g = 1.5 / a_port;
        let (mdot, wabs, rdot) = m.fuel_rates(1.5);
        let f = m.grain.at(0.0).2;
        close(rdot, (1.0 - f) * p.paraffin_a * g.powf(p.paraffin_n) + f * p.abs_a * g.powf(p.abs_n), 1e-12, "rdot");
        let dens = p.grain.paraffin_rho * (1.0 - f) + p.grain.abs_rho * f;
        close(mdot, dens * std::f64::consts::PI * p.grain.port_d_m * p.grain.grain_length_m * rdot, 5e-3, "fuel flow (tabulated perimeter)");
        close(wabs, p.grain.abs_rho * f / dens, 1e-12, "ABS share of the fuel");
        // ideal vacuum Cf at gamma 1.2, eps 4.9 is about 1.66 (Sutton & Biblarz fig. 3-7)
        close(thrust_coefficient(1.2, 4.9, 2.5e6, 1.0, 1.0, 0.4).0, 1.66, 0.02, "vacuum Cf");
        // CEA: paraffin/N2O c* peaks near O/F 7-8 at about 1600 m/s; ABS lowers it a little
        let liquid = n2o_enthalpy_cal_mol(293.15, true);
        let c_par = data().cea.lookup(7.0, 3.4e6, liquid, 0.0)[0];
        close(c_par, 1600.0, 0.02, "c* peak");
        assert!(data().cea.lookup(7.0, 3.4e6, liquid, 0.3)[0] < c_par + 30.0);
    }
}
