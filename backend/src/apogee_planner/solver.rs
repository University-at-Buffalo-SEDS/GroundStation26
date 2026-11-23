//! Fill mass and supercharge pressure for a target apogee
//! (docs/apogee-planner.md section 4). Policy: fill first, then pressure.
use super::flight::{fly, Config, Environment, FlightResult};
use super::motor::{motor_for_fill, physics_motor_for_fill, MotorFit, PhysicsMotor};
use super::n2o;
use super::weather::Snapshot;
use serde::{Deserialize, Serialize};

/// IREC/SA Cup minimum rail exit speed.
pub const MIN_RAIL_EXIT_MPS: f64 = 30.5;
pub const FILL_STEP_KG: f64 = 0.05;
pub const PRESSURE_STEP_PSI: f64 = 5.0;
/// Warm-case margin above the tank temperature for the ullage rule.
pub const WARM_MARGIN_C: f64 = 5.0;

#[derive(Clone, Debug)]
pub struct Input {
    pub cfg: Config,
    pub weather: Snapshot,
    pub fit: MotorFit,
    /// The physics motor; None flies the impulse-scaling model (the fallback).
    pub physics: Option<PhysicsMotor>,
    pub target_m: f64,
    /// Tank (N2O) temperature, °C: the override or the surface temperature.
    pub tank_c: f64,
    pub tank_volume_l: f64,
    pub min_ullage: f64,
    pub min_fill_kg: f64,
    /// The lowest hardware pressure limit and where each limit came from.
    pub max_supercharge_psi: f64,
    pub limit_sources: Vec<String>,
    pub nominal_supercharge_psi: f64,
    pub launch_ms: i64,
    pub dt: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Limits {
    pub max_fill_kg: f64,
    pub min_fill_kg: f64,
    pub max_supercharge_psi: f64,
    pub vapour_pressure_psi: f64,
    pub limit_sources: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Solution {
    /// "ok", "unreachable_high" or "unreachable_low".
    pub status: String,
    pub target_apogee_m: f64,
    pub fill_mass_kg: f64,
    pub supercharge_psi: f64,
    pub apogee_m: f64,
    pub apogee_band_m: [f64; 2],
    pub apogee_sigma_m: f64,
    pub max_achievable_apogee_m: f64,
    pub min_achievable_apogee_m: f64,
    pub rail_exit_mps: f64,
    pub max_mach: f64,
    pub time_to_apogee_s: f64,
    pub burnout_s: f64,
    pub drift_m: f64,
    pub drift_bearing_deg: f64,
    pub impulse_ns: f64,
    pub impulse_sigma_ns: f64,
    pub fill_band_kg: [f64; 2],
    pub limits: Limits,
    pub warnings: Vec<String>,
    pub flights: u32,
}

struct Ctx<'a> {
    input: &'a Input,
    env: Environment,
    flights: u32,
}

impl Ctx<'_> {
    fn fly(&mut self, fill: f64, psi: f64, impulse_factor: f64, env: Option<&Environment>, tank_c: Option<f64>) -> (FlightResult, f64, f64) {
        let i = self.input;
        let tc = tank_c.unwrap_or(i.tank_c);
        let m = match &i.physics {
            Some(pm) => physics_motor_for_fill(&i.cfg, pm, fill, tc, psi, i.tank_volume_l, impulse_factor),
            None => motor_for_fill(&i.cfg, &i.fit, fill, tc, psi, i.tank_volume_l, impulse_factor),
        };
        self.flights += 1;
        let r = fly(&i.cfg, env.unwrap_or(&self.env), &m.motor, i.dt);
        (r, m.impulse_ns, m.sigma_rel)
    }
    fn apogee(&mut self, fill: f64, psi: f64) -> f64 {
        self.fly(fill, psi, 1.0, None, None).0.apogee_m
    }
    /// x in [lo, hi] with apogee(x) = target, apogee increasing in x.
    fn bisect(&mut self, lo: f64, hi: f64, target: f64, f: &dyn Fn(&mut Self, f64) -> f64, tol: f64) -> f64 {
        let (mut lo, mut hi) = (lo, hi);
        for _ in 0..40 {
            if hi - lo <= tol {
                break;
            }
            let mid = 0.5 * (lo + hi);
            if f(self, mid) < target {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        0.5 * (lo + hi)
    }
}

fn round_to(x: f64, step: f64) -> f64 {
    (x / step).round() * step
}

pub fn limits(input: &Input) -> Limits {
    let warm = input.tank_c + WARM_MARGIN_C;
    Limits {
        max_fill_kg: (n2o::max_fill_kg(input.tank_volume_l, input.min_ullage, warm) / FILL_STEP_KG).floor() * FILL_STEP_KG,
        min_fill_kg: input.min_fill_kg,
        max_supercharge_psi: input.max_supercharge_psi,
        vapour_pressure_psi: n2o::vapour_pressure_psig(input.tank_c),
        limit_sources: input.limit_sources.clone(),
    }
}

pub fn solve(input: &Input) -> Solution {
    let env = input.weather.environment(input.cfg.environment.wind_shear_exponent, input.cfg.environment.wind_profile_cap);
    let mut cx = Ctx { input, env, flights: 0 };
    let lim = limits(input);
    let mut warnings = Vec::new();
    let pv = lim.vapour_pressure_psi;
    let p_low = round_to(pv.max(0.0), PRESSURE_STEP_PSI);
    let max_sc = lim.max_supercharge_psi;
    let mut nominal = input.nominal_supercharge_psi.min(max_sc);
    if input.nominal_supercharge_psi > max_sc {
        warnings.push(format!("Nominal supercharge {:.0} psi is above the {:.0} psi limit: using the limit.", input.nominal_supercharge_psi, max_sc));
    }
    if nominal < pv {
        warnings.push(format!(
            "Supercharge target {nominal:.0} psi is below the N2O vapour pressure ({pv:.0} psi at {:.1} °C): no nitrogen will be added.",
            input.tank_c
        ));
        nominal = nominal.max(p_low.min(max_sc));
    }
    if max_sc < pv {
        warnings.push(format!("The {max_sc:.0} psi pressure limit is below the vapour pressure ({pv:.0} psi): the tank exceeds it unsupercharged."));
    }
    let warm_pv = n2o::vapour_pressure_psig(input.tank_c + WARM_MARGIN_C);
    if warm_pv > max_sc {
        warnings.push(format!("If the tank warms {WARM_MARGIN_C:.0} °C, vapour pressure alone ({warm_pv:.0} psi) exceeds the {max_sc:.0} psi limit."));
    }
    let (min_fill, max_fill) = (lim.min_fill_kg, lim.max_fill_kg);
    let mut status = "ok";
    let (fill, psi);
    if max_fill < min_fill {
        warnings.push(format!("The max fill ({max_fill:.2} kg at {:.0} °C with {:.0} % ullage) is below the minimum fill.", input.tank_c + WARM_MARGIN_C, input.min_ullage * 100.0));
        status = "unreachable_high";
        fill = max_fill.max(0.0);
        psi = nominal;
    } else {
        let a_max = cx.apogee(max_fill, nominal);
        let a_min = cx.apogee(min_fill, nominal);
        let t = input.target_m;
        if t >= a_min && t <= a_max {
            fill = cx.bisect(min_fill, max_fill, t, &|c, m| c.apogee(m, nominal), 0.005);
            psi = nominal;
        } else if t > a_max {
            let a_top = cx.apogee(max_fill, max_sc);
            if t <= a_top && max_sc > nominal {
                fill = max_fill;
                psi = cx.bisect(nominal, max_sc, t, &|c, p| c.apogee(max_fill, p), 1.0);
                warnings.push(format!("The target needs more than the max fill: supercharge raised above the {nominal:.0} psi nominal."));
            } else {
                status = "unreachable_high";
                fill = max_fill;
                psi = max_sc.max(nominal);
            }
        } else {
            let a_bot = cx.apogee(min_fill, p_low.min(nominal));
            if t >= a_bot && nominal > p_low {
                fill = min_fill;
                psi = cx.bisect(p_low, nominal, t, &|c, p| c.apogee(min_fill, p), 1.0);
                warnings.push(format!("The target is below the minimum-fill apogee: supercharge lowered below the {nominal:.0} psi nominal."));
            } else {
                status = "unreachable_low";
                fill = min_fill;
                psi = p_low.min(nominal);
            }
        }
    }
    let fill = round_to(fill, FILL_STEP_KG).clamp(min_fill.min(max_fill), max_fill.max(0.0));
    let psi = round_to(psi, PRESSURE_STEP_PSI).min(max_sc);
    let (r, impulse, sigma_rel) = cx.fly(fill, psi, 1.0, None, None);
    // Reach: max fill at max pressure, min fill unsupercharged.
    let max_reach = if status == "unreachable_high" && (fill - max_fill).abs() < 1e-9 && (psi - max_sc).abs() < 1e-9 { r.apogee_m } else { cx.apogee(max_fill, max_sc.max(nominal)) };
    let min_reach = cx.apogee(min_fill, p_low.min(max_sc));
    // Band: one-at-a-time ±1σ.
    let (sig_t, sig_w) = input.weather.sigmas(input.launch_ms);
    let mut var = 0.0;
    let hi = cx.fly(fill, psi, 1.0 + sigma_rel, None, None).0.apogee_m;
    let lo = cx.fly(fill, psi, (1.0 - sigma_rel).max(0.05), None, None).0.apogee_m;
    var += ((hi - lo) / 2.0).powi(2);
    let ws = input.weather.wind_mps.max(0.5);
    let env_hi = cx.env.wind.scaled((ws + sig_w) / ws);
    let env_lo = cx.env.wind.scaled(((ws - sig_w) / ws).max(0.0));
    let mut e1 = cx.env.clone();
    e1.wind = env_hi;
    let mut e2 = cx.env.clone();
    e2.wind = env_lo;
    let w1 = cx.fly(fill, psi, 1.0, Some(&e1), None).0.apogee_m;
    let w2 = cx.fly(fill, psi, 1.0, Some(&e2), None).0.apogee_m;
    var += ((w1 - w2) / 2.0).powi(2);
    let mut warm = input.weather.clone();
    warm.surface_temp_c += sig_t;
    let mut cold = input.weather.clone();
    cold.surface_temp_c -= sig_t;
    let (shear, capv) = (input.cfg.environment.wind_shear_exponent, input.cfg.environment.wind_profile_cap);
    let ew = warm.environment(shear, capv);
    let ec = cold.environment(shear, capv);
    let t1 = cx.fly(fill, psi, 1.0, Some(&ew), Some(input.tank_c + sig_t)).0.apogee_m;
    let t2 = cx.fly(fill, psi, 1.0, Some(&ec), Some(input.tank_c - sig_t)).0.apogee_m;
    var += ((t1 - t2) / 2.0).powi(2);
    let sigma = var.sqrt();
    if r.rail_exit_mps < MIN_RAIL_EXIT_MPS {
        warnings.push(format!("Rail exit {:.1} m/s is below the {MIN_RAIL_EXIT_MPS} m/s minimum.", r.rail_exit_mps));
    }
    if r.liftoff_s <= 0.0 {
        warnings.push("The motor does not lift the vehicle off the rail.".into());
    }
    match status {
        "unreachable_high" => warnings.push(format!(
            "Target {:.0} m is not reachable: the most this vehicle can reach is about {:.0} m (max fill {max_fill:.2} kg at {:.0} psi).",
            input.target_m, max_reach, max_sc.max(nominal)
        )),
        "unreachable_low" => warnings.push(format!(
            "Target {:.0} m is below the minimum-fill apogee of about {:.0} m ({min_fill:.2} kg).",
            input.target_m, min_reach
        )),
        _ => {}
    }
    if (r.apogee_m - input.target_m).abs() > sigma.max(30.0) && status == "ok" {
        warnings.push(format!("Rounded plan predicts {:.0} m for a {:.0} m target.", r.apogee_m, input.target_m));
    }
    Solution {
        status: status.into(),
        target_apogee_m: input.target_m,
        fill_mass_kg: fill,
        supercharge_psi: psi,
        apogee_m: r.apogee_m,
        apogee_band_m: [r.apogee_m - sigma, r.apogee_m + sigma],
        apogee_sigma_m: sigma,
        max_achievable_apogee_m: max_reach,
        min_achievable_apogee_m: min_reach,
        rail_exit_mps: r.rail_exit_mps,
        max_mach: r.max_mach,
        time_to_apogee_s: r.apogee_s - r.liftoff_s,
        burnout_s: r.burnout_s,
        drift_m: r.drift_m,
        drift_bearing_deg: r.drift_bearing_deg,
        impulse_ns: impulse,
        impulse_sigma_ns: impulse * sigma_rel,
        fill_band_kg: [
            round_to(fill / (1.0 + sigma_rel), 0.01).max(0.0),
            round_to(fill / (1.0 - sigma_rel).max(0.05), 0.01),
        ],
        limits: lim,
        warnings,
        flights: cx.flights,
    }
}
