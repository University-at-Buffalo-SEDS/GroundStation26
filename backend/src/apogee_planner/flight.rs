//! Rust port of `tools/flight_profile/flightsim.py` (boost and coast to apogee)
//! plus a terminal-velocity descent for the landing drift.
//!
//! The ascent follows the Python model step for step (1 ms, the same
//! equations, gusts off) so the planner and the mock-flight generator agree;
//! `tests::reference_cases` checks them against fixtures/reference_cases.json.
use serde::{Deserialize, Serialize};

pub const G0: f64 = 9.80665;
const R_AIR: f64 = 287.05287;
const GAMMA: f64 = 1.4;
const R_EARTH: f64 = 6356766.0;

type V3 = [f64; 3];
type Q = [f64; 4];

// ------------------------------------------------------------ configuration

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Fins {
    pub count: f64,
    pub root_m: f64,
    pub tip_m: f64,
    pub span_m: f64,
    pub thickness_m: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Vehicle {
    pub length_m: f64,
    pub diameter_m: f64,
    pub dry_mass_kg: f64,
    pub nose_length_m: f64,
    pub fins: Fins,
    pub surface_roughness_m: f64,
    pub cn_alpha_per_rad: f64,
    pub static_margin_cal: f64,
    pub pitch_inertia_factor: f64,
    pub roll_rate_dps_per_mps: f64,
    pub roll_damping_length_m: f64,
    #[serde(default = "ccw")]
    pub roll_direction: String,
    /// Aft diameter of a boattail: base drag acts on this area, not the body's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boattail_aft_diameter_m: Option<f64>,
}
fn ccw() -> String {
    "ccw".into()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AeroCfg {
    pub protuberance_factor: f64,
    pub cd_extra: f64,
    pub transonic_wave_cd: f64,
    pub transonic_mach_start: f64,
    #[serde(default = "one")]
    pub cd_scale: f64,
}
fn one() -> f64 {
    1.0
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LaunchCfg {
    pub rail_length_m: f64,
    pub tilt_deg: f64,
    pub azimuth_deg: f64,
    #[serde(default)]
    pub rail_friction: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AttitudeCfg {
    pub pad_roll_deg: f64,
    pub pitch_damping_ratio: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RecoveryCfg {
    pub drogue_delay_after_apogee_s: f64,
    pub drogue_descent_mps: f64,
    pub drogue_rate_ref_alt_agl_m: f64,
    pub main_deploy_alt_agl_m: f64,
    pub main_descent_mps: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MotorCfg {
    pub propellant_mass_kg: f64,
    pub residual_fraction: f64,
    pub nozzle_exit_diameter_m: f64,
    pub oscillation_amplitude: f64,
    pub oscillation_hz: f64,
    #[serde(default)]
    pub total_impulse_ns: Option<f64>,
    #[serde(default)]
    pub scale_propellant_with_impulse: bool,
    #[serde(default = "one")]
    pub impulse_scaling_thrust_exponent: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EnvironmentCfg {
    pub site_lat_deg: f64,
    pub site_lon_deg: f64,
    pub site_msl_m: f64,
    pub temp_offset_k: f64,
    pub wind_mps: f64,
    pub wind_from_deg: f64,
    pub wind_shear_exponent: f64,
    #[serde(default = "cap")]
    pub wind_profile_cap: f64,
    #[serde(default)]
    pub gust_sigma_mps: f64,
}
fn cap() -> f64 {
    1.4
}

/// The parts of `tools/flight_profile/vehicle.json` the planner flies.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Config {
    pub vehicle: Vehicle,
    pub motor: MotorCfg,
    pub aero: AeroCfg,
    pub launch: LaunchCfg,
    pub attitude: AttitudeCfg,
    pub recovery: RecoveryCfg,
    pub environment: EnvironmentCfg,
}

/// The committed copy of vehicle.json (tests check it matches tools/).
pub const VEHICLE_JSON: &str = include_str!("model/vehicle.json");
/// The committed copy of the measured thrust curve.
pub const THRUST_CSV: &str = include_str!("model/testfire_20261116.csv");

impl Config {
    pub fn reference() -> Self {
        serde_json::from_str(VEHICLE_JSON).expect("model/vehicle.json parses")
    }
}

// ------------------------------------------------------------ environment

/// Atmosphere: USSA-76 troposphere/tropopause with a uniform temperature offset
/// and a sea-level pressure. Returns (T K, p Pa, rho, a, mu).
pub fn atmosphere(h_msl: f64, temp_offset_k: f64, p_sl: f64) -> (f64, f64, f64, f64, f64) {
    let hg = R_EARTH * h_msl / (R_EARTH + h_msl);
    let t0 = 288.15 + temp_offset_k;
    let (t, p) = if hg < 11000.0 {
        let t = t0 - 0.0065 * hg;
        (t, p_sl * (t / t0).powf(G0 / (R_AIR * 0.0065)))
    } else {
        let t11 = t0 - 0.0065 * 11000.0;
        let p11 = p_sl * (t11 / t0).powf(G0 / (R_AIR * 0.0065));
        (t11, p11 * (-G0 * (hg - 11000.0) / (R_AIR * t11)).exp())
    };
    let rho = p / (R_AIR * t);
    let a = (GAMMA * R_AIR * t).sqrt();
    let mu = 1.458e-6 * t.powf(1.5) / (t + 110.4);
    (t, p, rho, a, mu)
}

pub fn gravity(lat_deg: f64, h_msl: f64) -> f64 {
    let s2 = lat_deg.to_radians().sin().powi(2);
    let g = 9.7803253359 * (1.0 + 0.00193185265241 * s2) / (1.0 - 0.00669437999013 * s2).sqrt();
    g - 3.086e-6 * h_msl
}

/// The atmosphere offsets that reproduce an observed surface temperature and
/// pressure at the site: (temperature offset K, sea-level pressure Pa).
pub fn atmosphere_for_surface(site_msl: f64, temp_c: f64, pressure_pa: Option<f64>) -> (f64, f64) {
    let hg = R_EARTH * site_msl / (R_EARTH + site_msl);
    let dt = (temp_c + 273.15) - (288.15 - 0.0065 * hg);
    let p_sl = match pressure_pa {
        Some(p) if p.is_finite() && p > 10_000.0 => {
            let t0 = 288.15 + dt;
            let t = t0 - 0.0065 * hg;
            p / (t / t0).powf(G0 / (R_AIR * 0.0065))
        }
        _ => 101325.0,
    };
    (dt, p_sl)
}

/// Mean wind (no gusts): the Python power law from 10 m, or, with levels,
/// the power law up to 10 m and linear interpolation (by components) through
/// the levels above.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct WindProfile {
    pub surface_mps: f64,
    pub from_deg: f64,
    pub shear_exp: f64,
    pub cap: f64,
    /// (height AGL m, east m/s, north m/s), ascending.
    pub levels: Vec<(f64, f64, f64)>,
}

pub fn wind_vector(speed: f64, from_deg: f64) -> (f64, f64) {
    let to = (from_deg + 180.0).to_radians();
    (speed * to.sin(), speed * to.cos())
}

impl WindProfile {
    pub fn power_law(surface_mps: f64, from_deg: f64, shear_exp: f64, cap: f64) -> Self {
        Self { surface_mps, from_deg, shear_exp, cap, levels: Vec::new() }
    }
    pub fn at(&self, h_agl: f64) -> (f64, f64) {
        let (e, n) = wind_vector(self.surface_mps, self.from_deg);
        let prof = |h: f64| ((h.max(1.0) / 10.0).powf(self.shear_exp)).min(self.cap);
        if self.levels.is_empty() || h_agl <= 10.0 {
            let k = prof(h_agl);
            return (e * k, n * k);
        }
        let mut pts = vec![(10.0, e, n)];
        pts.extend(self.levels.iter().copied().filter(|l| l.0 > 10.0));
        if h_agl >= pts[pts.len() - 1].0 {
            let l = pts[pts.len() - 1];
            return (l.1, l.2);
        }
        for w in pts.windows(2) {
            let (a, b) = (w[0], w[1]);
            if h_agl <= b.0 {
                let x = (h_agl - a.0) / (b.0 - a.0);
                return (a.1 + (b.1 - a.1) * x, a.2 + (b.2 - a.2) * x);
            }
        }
        (e, n)
    }
    pub fn scaled(&self, k: f64) -> Self {
        let mut w = self.clone();
        w.surface_mps *= k;
        for l in &mut w.levels {
            l.1 *= k;
            l.2 *= k;
        }
        w
    }
}

#[derive(Clone, Debug)]
pub struct Environment {
    pub site_msl_m: f64,
    pub site_lat_deg: f64,
    pub temp_offset_k: f64,
    pub p_sl_pa: f64,
    pub wind: WindProfile,
}

impl Environment {
    /// vehicle.json's own environment (the reference cases).
    #[cfg(test)]
    pub fn from_config(cfg: &Config) -> Self {
        let e = &cfg.environment;
        Self {
            site_msl_m: e.site_msl_m,
            site_lat_deg: e.site_lat_deg,
            temp_offset_k: e.temp_offset_k,
            p_sl_pa: 101325.0,
            wind: WindProfile::power_law(e.wind_mps, e.wind_from_deg, e.wind_shear_exponent, e.wind_profile_cap),
        }
    }
}

// ------------------------------------------------------------ motor

#[derive(Clone, Debug, Serialize)]
pub struct Motor {
    pub t: Vec<f64>,
    pub f: Vec<f64>,
    pub prop_kg: f64,
    pub exit_d: f64,
    pub osc_amp: f64,
    pub osc_hz: f64,
    pub impulse: f64,
    #[serde(skip)]
    cum: Vec<f64>,
    /// Physics motor (hybrid.rs): propellant left on the same time base (replaces the impulse-fraction mass
    /// model and the residual fraction) and the nozzle exit area with the ambient pressure the curve was
    /// computed at, so thrust gains (pa_ref - p) x exit area with altitude. Empty / 0 = the measured curve.
    #[serde(skip)]
    pub prop_curve: Vec<f64>,
    #[serde(skip)]
    pub exit_area_m2: f64,
    #[serde(skip)]
    pub pa_ref: f64,
}

pub fn parse_curve(csv: &str) -> (Vec<f64>, Vec<f64>) {
    let (mut t, mut f) = (Vec::new(), Vec::new());
    for line in csv.lines() {
        let line = line.trim();
        if !line.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            continue;
        }
        let mut it = line.split(',');
        if let (Some(a), Some(b)) = (it.next(), it.next()) {
            if let (Ok(a), Ok(b)) = (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
                t.push(a);
                f.push(b.max(0.0));
            }
        }
    }
    (t, f)
}

impl Motor {
    pub fn new(t: Vec<f64>, f: Vec<f64>, prop_kg: f64, exit_d: f64, osc_amp: f64, osc_hz: f64) -> Self {
        let mut m = Self { t, f, prop_kg, exit_d, osc_amp, osc_hz, impulse: 0.0, cum: Vec::new(), prop_curve: Vec::new(), exit_area_m2: 0.0, pa_ref: 0.0 };
        m.integrate();
        m
    }
    /// The motor exactly as `flightsim.Motor.load` builds it from `cfg.motor`.
    pub fn from_config(cfg: &MotorCfg, csv: &str) -> Self {
        let (t, f) = parse_curve(csv);
        let mut m = Self::new(t, f, cfg.propellant_mass_kg, cfg.nozzle_exit_diameter_m, cfg.oscillation_amplitude, cfg.oscillation_hz);
        if let Some(target) = cfg.total_impulse_ns.filter(|x| *x > 0.0) {
            let k = target / m.impulse;
            let e = cfg.impulse_scaling_thrust_exponent;
            m = m.scaled(k.powf(e), k.powf(1.0 - e));
            if cfg.scale_propellant_with_impulse {
                m.prop_kg *= k;
            }
        }
        m
    }
    /// Thrust axis times `a`, time axis times `b`.
    pub fn scaled(&self, a: f64, b: f64) -> Self {
        Self::new(
            self.t.iter().map(|x| x * b).collect(),
            self.f.iter().map(|x| x * a).collect(),
            self.prop_kg,
            self.exit_d,
            self.osc_amp,
            self.osc_hz,
        )
    }
    fn integrate(&mut self) {
        self.cum = vec![0.0];
        for i in 1..self.t.len() {
            let c = self.cum[i - 1] + 0.5 * (self.f[i] + self.f[i - 1]) * (self.t[i] - self.t[i - 1]);
            self.cum.push(c);
        }
        self.impulse = *self.cum.last().unwrap_or(&0.0);
    }
    pub fn burn_s(&self) -> f64 {
        *self.t.last().unwrap_or(&0.0)
    }
    fn index(&self, tb: f64) -> usize {
        // bisect_right(t, tb) - 1
        self.t.partition_point(|x| *x <= tb).saturating_sub(1)
    }
    pub fn thrust(&self, tb: f64) -> f64 {
        if tb <= 0.0 || tb >= self.burn_s() {
            return 0.0;
        }
        let i = self.index(tb);
        let k = (tb - self.t[i]) / (self.t[i + 1] - self.t[i]);
        let base = self.f[i] + (self.f[i + 1] - self.f[i]) * k;
        if self.osc_amp != 0.0 {
            base * (1.0 + self.osc_amp * (2.0 * std::f64::consts::PI * self.osc_hz * tb).sin())
        } else {
            base
        }
    }
    /// Thrust at ambient pressure `p_air` (the altitude term applies to a physics motor only).
    pub fn thrust_at(&self, tb: f64, p_air: f64) -> f64 {
        let f = self.thrust(tb);
        if f > 0.0 && self.exit_area_m2 > 0.0 { (f + (self.pa_ref - p_air) * self.exit_area_m2).max(0.0) } else { f }
    }
    /// Propellant left (physics motor).
    pub fn prop_left(&self, tb: f64) -> f64 {
        let n = self.prop_curve.len();
        if tb <= 0.0 {
            return self.prop_curve[0];
        }
        if tb >= self.burn_s() {
            return self.prop_curve[n - 1];
        }
        let i = self.index(tb);
        let k = (tb - self.t[i]) / (self.t[i + 1] - self.t[i]);
        self.prop_curve[i] + (self.prop_curve[i + 1] - self.prop_curve[i]) * k
    }
    pub fn burnt_fraction(&self, tb: f64) -> f64 {
        if tb <= 0.0 {
            return 0.0;
        }
        if tb >= self.burn_s() {
            return 1.0;
        }
        let i = self.index(tb);
        let k = (tb - self.t[i]) / (self.t[i + 1] - self.t[i]);
        let part = self.cum[i] + (self.f[i] + 0.5 * (self.f[i + 1] - self.f[i]) * k) * (tb - self.t[i]);
        part / self.impulse
    }
}

// ------------------------------------------------------------ aero

struct Aero {
    l: f64,
    aref: f64,
    s_body: f64,
    s_fins: f64,
    fin_tc: f64,
    fin_front: f64,
    fineness: f64,
    rough: f64,
    base_frac: f64,
    exit_frac: f64,
    k_prot: f64,
    cd_extra: f64,
    wave_peak: f64,
    m_crit: f64,
    cd_scale: f64,
}

impl Aero {
    fn new(v: &Vehicle, a: &AeroCfg, exit_d: f64) -> Self {
        let d = v.diameter_m;
        let l = v.length_m;
        let aref = std::f64::consts::PI * d * d / 4.0;
        let nose = v.nose_length_m;
        let s_body = std::f64::consts::PI * d * (l - nose) + 0.75 * std::f64::consts::PI * d * nose;
        let fin_area = 0.5 * (v.fins.root_m + v.fins.tip_m) * v.fins.span_m;
        Self {
            l,
            aref,
            s_body,
            s_fins: 2.0 * v.fins.count * fin_area,
            fin_tc: v.fins.thickness_m / v.fins.root_m,
            fin_front: v.fins.count * v.fins.thickness_m * v.fins.span_m / aref,
            fineness: l / d,
            rough: v.surface_roughness_m,
            base_frac: (v.boattail_aft_diameter_m.unwrap_or(d).min(d) / d).powi(2),
            exit_frac: (exit_d / d).powi(2),
            k_prot: a.protuberance_factor,
            cd_extra: a.cd_extra,
            wave_peak: a.transonic_wave_cd,
            m_crit: a.transonic_mach_start,
            cd_scale: a.cd_scale,
        }
    }
    fn base(m: f64) -> f64 {
        if m < 1.0 { 0.12 + 0.13 * m * m } else { 0.25 / m }
    }
    fn cd(&self, mach: f64, re: f64, thrusting: bool) -> f64 {
        let re = re.max(1e4);
        let cf_t = 1.0 / (1.50 * re.ln() - 5.6).powi(2);
        let cf_r = 0.032 * (self.rough / self.l).powf(0.2);
        let mut cf = cf_t.max(cf_r);
        cf *= if mach < 1.0 { 1.0 - 0.1 * mach * mach } else { 1.0 / (1.0 + 0.15 * mach * mach).powf(0.58) };
        let friction = cf * ((1.0 + 1.0 / (2.0 * self.fineness)) * self.s_body + (1.0 + 2.0 * self.fin_tc) * self.s_fins) / self.aref;
        let base = Self::base(mach) * if thrusting { (self.base_frac - self.exit_frac).max(0.0) } else { self.base_frac };
        let m = mach.min(0.9);
        let fin_le = ((1.0 - m * m).powf(-0.417) - 1.0) * self.fin_front;
        let fin_te = Self::base(mach) * self.fin_front;
        let x = (mach - self.m_crit) / 0.25;
        let xc = x.min(1.0);
        let wave = self.wave_peak * if x <= 0.0 { 0.0 } else { xc * xc * (3.0 - 2.0 * xc) };
        self.cd_scale * (friction * self.k_prot + base + fin_le + fin_te + wave + self.cd_extra)
    }
}

// ------------------------------------------------------------ vectors

fn qmul(a: Q, b: Q) -> Q {
    [
        a[0] * b[0] - a[1] * b[1] - a[2] * b[2] - a[3] * b[3],
        a[0] * b[1] + a[1] * b[0] + a[2] * b[3] - a[3] * b[2],
        a[0] * b[2] - a[1] * b[3] + a[2] * b[0] + a[3] * b[1],
        a[0] * b[3] + a[1] * b[2] - a[2] * b[1] + a[3] * b[0],
    ]
}
fn qnorm(q: Q) -> Q {
    let n = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt();
    [q[0] / n, q[1] / n, q[2] / n, q[3] / n]
}
fn qrot(q: Q, v: V3) -> V3 {
    let [w, x, y, z] = q;
    let tx = 2.0 * (y * v[2] - z * v[1]);
    let ty = 2.0 * (z * v[0] - x * v[2]);
    let tz = 2.0 * (x * v[1] - y * v[0]);
    [
        v[0] + w * tx + (y * tz - z * ty),
        v[1] + w * ty + (z * tx - x * tz),
        v[2] + w * tz + (x * ty - y * tx),
    ]
}
fn q_axis_angle(axis: V3, ang: f64) -> Q {
    let n = norm(axis);
    if n < 1e-12 || ang == 0.0 {
        return [1.0, 0.0, 0.0, 0.0];
    }
    let s = (ang / 2.0).sin() / n;
    [(ang / 2.0).cos(), axis[0] * s, axis[1] * s, axis[2] * s]
}
fn cross(a: V3, b: V3) -> V3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}
fn dot(a: V3, b: V3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn norm(a: V3) -> f64 {
    dot(a, a).sqrt()
}
fn unit(a: V3) -> V3 {
    let n = norm(a);
    if n > 1e-12 { [a[0] / n, a[1] / n, a[2] / n] } else { [0.0, 0.0, 1.0] }
}

// ------------------------------------------------------------ flight

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct FlightResult {
    pub apogee_m: f64,
    /// From ignition.
    pub apogee_s: f64,
    pub liftoff_s: f64,
    pub rail_exit_mps: f64,
    pub burnout_s: f64,
    pub max_mach: f64,
    pub max_speed_mps: f64,
    /// Horizontal position at apogee (east, north m).
    pub apogee_en_m: (f64, f64),
    pub drift_m: f64,
    pub drift_bearing_deg: f64,
    pub descent_s: f64,
}

#[derive(PartialEq, Clone, Copy)]
enum Phase {
    Pad,
    Rail,
    Boost,
    Coast,
}

/// Flies to apogee (the Python ascent, gusts off), then the descent drift.
pub fn fly(cfg: &Config, env: &Environment, motor: &Motor, dt: f64) -> FlightResult {
    let v = &cfg.vehicle;
    let aero = Aero::new(v, &cfg.aero, motor.exit_d);
    let (site, lat, dtk, psl) = (env.site_msl_m, env.site_lat_deg, env.temp_offset_k, env.p_sl_pa);
    let m_dry = v.dry_mass_kg;
    let resid = if motor.prop_curve.is_empty() { motor.prop_kg * cfg.motor.residual_fraction } else { motor.prop_curve[motor.prop_curve.len() - 1] };
    let m_burn = motor.prop_kg - resid;
    let l = v.length_m;
    let aref = aero.aref;
    let (cna, sm) = (v.cn_alpha_per_rad, v.static_margin_cal * v.diameter_m);
    let lau = &cfg.launch;
    let (tilt, az) = (lau.tilt_deg.to_radians(), lau.azimuth_deg.to_radians());
    let rail_dir = [tilt.sin() * az.sin(), tilt.sin() * az.cos(), tilt.cos()];
    let mut q: Q = if tilt != 0.0 { q_axis_angle(cross([0.0, 0.0, 1.0], rail_dir), tilt) } else { [1.0, 0.0, 0.0, 0.0] };
    q = qnorm(qmul(q, q_axis_angle([0.0, 0.0, 1.0], cfg.attitude.pad_roll_deg.to_radians())));
    let roll_sign = if v.roll_direction == "ccw" { 1.0 } else { -1.0 };
    let mut pos: V3 = [0.0; 3];
    let mut vel: V3 = [0.0; 3];
    let mut om: V3 = [0.0; 3];
    let mut p_roll = 0.0;
    let mut phase = Phase::Pad;
    let mut s_rail = 0.0;
    let mut t = 0.0_f64;
    let mut res = FlightResult::default();
    let mut step = 0u64;
    loop {
        let h = pos[2];
        let (_, p_air, rho, a_snd, mu) = atmosphere(site + h.max(0.0), dtk, psl);
        let g = gravity(lat, site + h);
        let gvec = [0.0, 0.0, -g];
        let tb = t;
        let thrust = motor.thrust_at(tb, p_air);
        let mass = if motor.prop_curve.is_empty() { m_dry + resid + m_burn * (1.0 - motor.burnt_fraction(tb)) } else { m_dry + motor.prop_left(tb) };
        let (we, wn) = env.wind.at(h);
        let vair = [vel[0] - we, vel[1] - wn, vel[2]];
        let vmag = norm(vair);
        let qd = 0.5 * rho * vmag * vmag;
        let mach = vmag / a_snd;
        let u = qrot(q, [0.0, 0.0, 1.0]);
        let mut acc = [0.0; 3];
        let mut torque = [0.0; 3];
        let inertia = mass * l * l / 12.0 * v.pitch_inertia_factor;
        let fric = 1.0 + lau.rail_friction;
        if phase == Phase::Pad {
            if thrust > mass * g * rail_dir[2] * fric {
                phase = Phase::Rail;
                res.liftoff_s = t;
            }
        } else if phase == Phase::Rail {
            let cd = aero.cd(mach, rho * vmag * l / mu, thrust > 0.0);
            let drag = qd * aref * cd;
            let mut along = (thrust - drag) / mass - g * rail_dir[2] * fric;
            let sp = dot(vel, rail_dir);
            if sp <= 0.0 && along < 0.0 {
                along = 0.0;
            }
            acc = [along * rail_dir[0], along * rail_dir[1], along * rail_dir[2]];
            s_rail += sp * dt;
            if s_rail >= lau.rail_length_m {
                phase = Phase::Boost;
                res.rail_exit_mps = sp;
            }
        }
        if matches!(phase, Phase::Boost | Phase::Coast) {
            let thrusting = thrust > 0.0;
            if phase == Phase::Boost && tb >= motor.burn_s() {
                phase = Phase::Coast;
                res.burnout_s = t;
            }
            let vh = if vmag > 0.5 { unit(vair) } else { u };
            let re = rho * vmag.max(0.1) * l / mu;
            let cd = aero.cd(mach, re, thrusting);
            let drag = [-qd * aref * cd * vh[0], -qd * aref * cd * vh[1], -qd * aref * cd * vh[2]];
            let du = dot(u, vh);
            let nperp = [u[0] - du * vh[0], u[1] - du * vh[1], u[2] - du * vh[2]];
            let sin_a = norm(nperp);
            let lim = 15.0_f64.to_radians().sin();
            let ks = if sin_a > lim { (lim / sin_a).min(1.0) } else { 1.0 };
            for i in 0..3 {
                let normal = qd * aref * cna * nperp[i] * ks;
                acc[i] = (thrust * u[i] + drag[i] + normal) / mass + gvec[i];
            }
            let k = qd * aref * cna * sm;
            let ax = cross(u, vh);
            let c_damp = 2.0 * cfg.attitude.pitch_damping_ratio * (k.max(0.0) * inertia).sqrt();
            for i in 0..3 {
                torque[i] = k * ax[i] - c_damp * om[i];
            }
            let p_eq = roll_sign * v.roll_rate_dps_per_mps.to_radians() * vmag;
            let tau_r = v.roll_damping_length_m / vmag.max(5.0);
            p_roll += (p_eq - p_roll) * (dt / tau_r).min(1.0);
            if vel[2] <= 0.0 && t > 1.0 {
                res.apogee_s = t;
                res.apogee_m = h;
                res.apogee_en_m = (pos[0], pos[1]);
                break;
            }
        }
        if matches!(phase, Phase::Boost | Phase::Coast) {
            for i in 0..3 {
                om[i] += torque[i] / inertia * dt;
            }
            let un = qrot(q, [0.0, 0.0, 1.0]);
            let par = dot(om, un);
            for i in 0..3 {
                om[i] -= par * un[i];
            }
        }
        let un = qrot(q, [0.0, 0.0, 1.0]);
        let omega = [om[0] + p_roll * un[0], om[1] + p_roll * un[1], om[2] + p_roll * un[2]];
        if phase != Phase::Pad {
            let wn = norm(omega);
            if wn > 0.0 {
                q = qnorm(qmul(q_axis_angle(omega, wn * dt), q));
            }
            for i in 0..3 {
                vel[i] += acc[i] * dt;
                pos[i] += vel[i] * dt;
            }
        }
        res.max_mach = res.max_mach.max(mach);
        res.max_speed_mps = res.max_speed_mps.max(norm(vel));
        step += 1;
        t = (step as f64) * dt;
        if t > 400.0 || (phase == Phase::Pad && t > motor.burn_s() + 1.0) {
            // Never lifted off (thrust below weight) or a runaway.
            res.apogee_s = t;
            break;
        }
    }
    descent(cfg, env, &mut res);
    res
}

/// Terminal-velocity descent from apogee under the drogue then the main,
/// drifting with the wind; sets the landing drift.
fn descent(cfg: &Config, env: &Environment, res: &mut FlightResult) {
    let r = &cfg.recovery;
    let (site, dtk, psl) = (env.site_msl_m, env.temp_offset_k, env.p_sl_pa);
    let rho_ref = atmosphere(site + r.drogue_rate_ref_alt_agl_m, dtk, psl).2;
    let rho_g = atmosphere(site, dtk, psl).2;
    let (mut e, mut n) = res.apogee_en_m;
    let mut h = res.apogee_m;
    let mut t = 0.0;
    // Coasting through the drogue delay drifts with the wind too.
    let (we, wn) = env.wind.at(h);
    e += we * r.drogue_delay_after_apogee_s;
    n += wn * r.drogue_delay_after_apogee_s;
    t += r.drogue_delay_after_apogee_s;
    let dt = 0.25;
    while h > 0.0 && t < 3600.0 {
        let rho = atmosphere(site + h, dtk, psl).2;
        let rate = if h > r.main_deploy_alt_agl_m {
            r.drogue_descent_mps * (rho_ref / rho).sqrt()
        } else {
            r.main_descent_mps * (rho_g / rho).sqrt()
        };
        let (we, wn) = env.wind.at(h);
        e += we * dt;
        n += wn * dt;
        h -= rate * dt;
        t += dt;
    }
    res.descent_s = t;
    res.drift_m = (e * e + n * n).sqrt();
    res.drift_bearing_deg = e.atan2(n).to_degrees().rem_euclid(360.0);
}
