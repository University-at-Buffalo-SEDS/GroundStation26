//! The station's fit of the hybrid motor model to its own accepted archive
//! fires, plus the learned residual on top of it (docs/hybrid-motor-model.md
//! sections 5 and 6).
//!
//! - **Physics fit:** the same weighted least squares as
//!   `tools/flight_profile/hybrid_calibrate.py` stage 2 (thrust curve, tank
//!   pressure trace and total impulse of every accepted fire, log-normal
//!   priors from the spec), so a new accepted fire refines the model without
//!   anyone running the Python. With only the shipped seed the result equals
//!   the spec's `calibrated` block (a test checks it).
//! - **Residual (grey box):** a Gaussian process on ln(I_measured / I_physics)
//!   over the fire features (fill reading, supercharge, tank temperature,
//!   initial port area). It is off until [`MIN_RESIDUAL_FIRES`] accepted
//!   fires exist; then its mean corrects the planner's impulse and its
//!   variance widens the apogee band. Hyperparameters come from
//!   `<data>/apogee_planner/residual_model.json` when an administrator drops
//!   one there (`tools/flight_profile/hybrid_residual.py` trains it on jupiter),
//!   else from [`ResidualHyper::default`]. The interface takes any model that
//!   maps features to (mean, variance), so a small network exported as JSON
//!   weights can replace the GP later.
use super::hybrid::{self, Params};
use crate::fire_archive::FitInput;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

pub const FIT_PARAMS: [&str; 4] = ["injector_cd", "eta_cstar", "vapour_cd_scale", "wall_coupling"];
const THRUST_SIGMA_N: f64 = 150.0;
const THRUST_SIGMA_REL: f64 = 0.05;
const TANK_SIGMA_PSI: f64 = 10.0;
const THRUST_FROM_S: f64 = 0.65;
const CURVE_STEP_S: f64 = 0.1;
const FUEL_SIGMA_KG: f64 = 0.05;
const ETA_CSTAR_MAX: f64 = 0.95;
const ETA_BARRIER: f64 = 0.01;
const DT: f64 = 0.01;
pub const MIN_RESIDUAL_FIRES: usize = 3;

/// One accepted fire, ready for the model.
#[derive(Clone, Debug, PartialEq)]
pub struct FireData {
    pub id: String,
    pub fill_kg: f64,
    pub tank_c: f64,
    pub supercharge_psig: f64,
    pub grain: String,
    pub impulse_ns: f64,
    pub impulse_sigma_ns: f64,
    pub thrust: Vec<[f64; 2]>,
    pub tank: Vec<[f64; 2]>,
    pub fuel_used_kg: Option<f64>,
}

/// The Python `fire_conditions`: saturated at the ignition tank pressure unless a supercharge is recorded.
pub fn conditions(tank_psi: Option<f64>, supercharge_psi: Option<f64>, ambient_c: Option<f64>) -> (f64, f64) {
    let sc = supercharge_psi.unwrap_or(0.0);
    match tank_psi {
        Some(p) if sc <= 0.0 || ambient_c.is_none() => (hybrid::tsat((p + 14.696) * hybrid::PSI) - 273.15, 0.0),
        _ => (ambient_c.unwrap_or(15.0), sc),
    }
}

pub fn fire_data(inputs: &[FitInput]) -> Vec<FireData> {
    inputs
        .iter()
        .filter(|f| f.thrust.len() >= 5 && f.fill_kg > 0.1 && f.impulse_ns > 0.0)
        .map(|f| {
            let (tank_c, sc) = conditions(f.tank_psi_at_ignition, f.supercharge_psi, f.ambient_c);
            let grain = f.grain.clone().filter(|g| hybrid::spec()["grains"].get(g.as_str()).is_some()).unwrap_or_else(|| hybrid::fire_grain().to_string());
            FireData {
                id: f.id.clone(),
                fill_kg: f.fill_kg,
                tank_c,
                supercharge_psig: sc,
                grain,
                impulse_ns: f.impulse_ns,
                impulse_sigma_ns: if f.impulse_sigma_ns > 0.0 { f.impulse_sigma_ns } else { 0.1 * f.impulse_ns },
                thrust: f.thrust.clone(),
                tank: f.tank.clone(),
                fuel_used_kg: f.fuel_used_kg,
            }
        })
        .collect()
}

fn subsample(curve: &[[f64; 2]], from: f64, step: f64) -> Vec<[f64; 2]> {
    let mut out = Vec::new();
    let mut next = from;
    for p in curve {
        if p[0] + 1e-9 >= next {
            out.push(*p);
            next = p[0] + step - 1e-9;
        }
    }
    out
}

fn with_grain(p: &Params, grain: &str) -> Params {
    let mut q = p.clone();
    if let Ok(g) = hybrid::GrainCfg::from_spec(hybrid::spec(), grain) {
        q.grain = g;
    }
    q
}

/// The fire's residuals (hybrid_calibrate.fire_residuals) and its burn.
pub fn fire_residuals(p: &Params, f: &FireData) -> (Vec<f64>, hybrid::Burn) {
    let p = with_grain(p, &f.grain);
    let b = hybrid::simulate(&p, hybrid::data(), f.fill_kg, f.tank_c, f.supercharge_psig, p.test_ambient_pa, DT);
    let t = b.times();
    let thrust = b.thrust();
    let tank: Vec<f64> = b.samples.iter().map(|s| s.p_tank / hybrid::PSI - 14.696).collect();
    let mut r = Vec::new();
    for [tm, fm] in subsample(&f.thrust, THRUST_FROM_S, CURVE_STEP_S) {
        r.push((hybrid::interp(&t, &thrust, tm) - fm) / (THRUST_SIGMA_N + THRUST_SIGMA_REL * fm));
    }
    for [tm, pm] in subsample(&f.tank, 0.0, CURVE_STEP_S) {
        r.push((hybrid::interp(&t, &tank, tm) - pm) / TANK_SIGMA_PSI);
    }
    r.push((b.summary.impulse_ns - f.impulse_ns) / f.impulse_sigma_ns);
    if let Some(fuel) = f.fuel_used_kg {
        r.push((b.summary.fuel_used_kg - fuel) / FUEL_SIGMA_KG);
    }
    (r, b)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct FireQuality {
    pub fire: String,
    pub grain: String,
    pub impulse_ns: f64,
    pub measured_impulse_ns: f64,
    pub impulse_error_pct: f64,
    pub burn_time_s: f64,
    pub thrust_rms_n: f64,
    pub tank_rms_psi: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct PhysicsFit {
    /// `station` (fitted here), `spec` (the shipped calibration: no fire with a thrust curve).
    pub source: String,
    pub values: Vec<(String, f64)>,
    pub sigma_rel: Vec<(String, f64)>,
    pub reduced_chi2: f64,
    pub fire_ids: Vec<String>,
    pub quality: Vec<FireQuality>,
    /// Relative 1-sigma of the impulse for planning: the fires' own impulse sigma (the model fits each to
    /// about that), pooled; never below 5 %.
    pub impulse_sigma_rel: f64,
}

impl PhysicsFit {
    /// The calibrated parameters for the flight (or a named) grain with this fit's values.
    pub fn params(&self, grain: Option<&str>) -> Params {
        let mut p = hybrid::calibrated(grain);
        for (k, v) in &self.values {
            p.set(k, *v);
        }
        p
    }
}

fn names(fires: &[FireData]) -> Vec<&'static str> {
    let mut n: Vec<&'static str> = FIT_PARAMS.to_vec();
    if fires.iter().any(|f| f.fuel_used_kg.is_some()) {
        n.push("regression_scale");
    }
    n
}

fn solve(a: &[Vec<f64>], b: &[f64]) -> Vec<f64> {
    let n = b.len();
    let mut m: Vec<Vec<f64>> = a.iter().zip(b).map(|(r, bi)| r.iter().copied().chain([*bi]).collect()).collect();
    for c in 0..n {
        let piv = (c..n).max_by(|i, j| m[*i][c].abs().total_cmp(&m[*j][c].abs())).unwrap();
        m.swap(c, piv);
        if m[c][c].abs() < 1e-300 {
            continue;
        }
        for i in c + 1..n {
            let f = m[i][c] / m[c][c];
            for j in c..=n {
                m[i][j] -= f * m[c][j];
            }
        }
    }
    let mut x = vec![0.0; n];
    for i in (0..n).rev() {
        let s: f64 = (i + 1..n).map(|j| m[i][j] * x[j]).sum();
        x[i] = if m[i][i].abs() > 1e-300 { (m[i][n] - s) / m[i][i] } else { 0.0 };
    }
    x
}

/// Fits the station's archive fires (None when no fire has a thrust curve).
pub fn fit(fires: &[FireData]) -> Option<PhysicsFit> {
    if fires.is_empty() {
        return None;
    }
    let keys = names(fires);
    let spec = hybrid::spec();
    let base = Params::from_spec(spec, None, false).ok()?;
    let start = hybrid::calibrated(None);
    let priors: Vec<(f64, f64)> = keys
        .iter()
        .map(|k| {
            let (v, s) = hybrid::prior(spec, k).unwrap_or((1.0, 0.3));
            (v.ln(), s / v)
        })
        .collect();
    let params_for = |th: &[f64]| {
        let mut p = base.clone();
        for (k, x) in keys.iter().zip(th) {
            p.set(k, x.exp());
        }
        p
    };
    let fun = |th: &[f64]| -> Vec<f64> {
        let p = params_for(th);
        let mut r = Vec::new();
        for f in fires {
            r.extend(fire_residuals(&p, f).0);
        }
        for ((x, (mu, s)), _) in th.iter().zip(&priors).zip(&keys) {
            r.push((x - mu) / s);
        }
        let eta = keys.iter().position(|k| *k == "eta_cstar").map(|i| th[i].exp()).unwrap_or(0.0);
        r.push((eta - ETA_CSTAR_MAX).max(0.0) / ETA_BARRIER);
        r
    };
    let mut th: Vec<f64> = keys.iter().map(|k| start.get(k).unwrap_or(1.0).max(1e-9).ln()).collect();
    let mut r = fun(&th);
    let mut cost: f64 = r.iter().map(|x| x * x).sum();
    let mut lam = 1e-2;
    let n = keys.len();
    let mut jtj = vec![vec![0.0; n]; n];
    for _ in 0..25 {
        let h = 0.02;
        let jac: Vec<Vec<f64>> = (0..n)
            .map(|i| {
                let mut t2 = th.clone();
                t2[i] += h;
                fun(&t2).iter().zip(&r).map(|(b, a)| (b - a) / h).collect()
            })
            .collect();
        jtj = (0..n).map(|i| (0..n).map(|j| jac[i].iter().zip(&jac[j]).map(|(a, b)| a * b).sum()).collect()).collect();
        let jtr: Vec<f64> = (0..n).map(|i| jac[i].iter().zip(&r).map(|(a, b)| a * b).sum()).collect();
        let mut improved = false;
        for _ in 0..8 {
            let a: Vec<Vec<f64>> = (0..n).map(|i| (0..n).map(|j| jtj[i][j] + if i == j { lam * jtj[i][i] } else { 0.0 }).collect()).collect();
            let step = solve(&a, &jtr.iter().map(|x| -x).collect::<Vec<_>>());
            let t2: Vec<f64> = th.iter().zip(&step).map(|(a, b)| a + b).collect();
            let r2 = fun(&t2);
            let c2: f64 = r2.iter().map(|x| x * x).sum();
            if c2 < cost {
                improved = cost - c2 > 1e-4 * cost;
                th = t2;
                r = r2;
                cost = c2;
                lam = (lam / 3.0).max(1e-6);
                break;
            }
            lam *= 4.0;
        }
        if !improved {
            break;
        }
    }
    let ndata = r.len() - n - 1;
    let chi2: f64 = r[..ndata].iter().map(|x| x * x).sum();
    let red = chi2 / (ndata.saturating_sub(n)).max(1) as f64;
    let cov: Vec<Vec<f64>> = (0..n).map(|j| solve(&jtj, &(0..n).map(|i| if i == j { 1.0 } else { 0.0 }).collect::<Vec<_>>())).collect();
    let p = params_for(&th);
    let quality = fires
        .iter()
        .map(|f| {
            let (_, b) = fire_residuals(&p, f);
            let t = b.times();
            let thrust = b.thrust();
            let tank = b.tank_psig();
            let pts = subsample(&f.thrust, THRUST_FROM_S, CURVE_STEP_S);
            let rms = (pts.iter().map(|[tm, fm]| (hybrid::interp(&t, &thrust, *tm) - fm).powi(2)).sum::<f64>() / pts.len().max(1) as f64).sqrt();
            let tp = subsample(&f.tank, 0.0, CURVE_STEP_S);
            let trms = (!tp.is_empty()).then(|| (tp.iter().map(|[tm, pm]| (hybrid::interp(&t, &tank, *tm) - pm).powi(2)).sum::<f64>() / tp.len() as f64).sqrt());
            FireQuality {
                fire: f.id.clone(),
                grain: f.grain.clone(),
                impulse_ns: b.summary.impulse_ns,
                measured_impulse_ns: f.impulse_ns,
                impulse_error_pct: 100.0 * (b.summary.impulse_ns / f.impulse_ns - 1.0),
                burn_time_s: b.summary.burn_time_s,
                thrust_rms_n: rms,
                tank_rms_psi: trms,
            }
        })
        .collect();
    let w: f64 = fires.iter().map(|f| (f.impulse_ns / f.impulse_sigma_ns).powi(2)).sum();
    Some(PhysicsFit {
        source: "station".into(),
        values: keys.iter().zip(&th).map(|(k, x)| (k.to_string(), x.exp())).collect(),
        sigma_rel: keys.iter().enumerate().map(|(i, k)| (k.to_string(), (cov[i][i] * red.max(1.0)).max(0.0).sqrt())).collect(),
        reduced_chi2: red,
        fire_ids: fires.iter().map(|f| f.id.clone()).collect(),
        quality,
        impulse_sigma_rel: (1.0 / w.sqrt()).max(0.05),
    })
}

/// The spec's shipped calibration (no usable fire in the archive).
pub fn spec_fit() -> PhysicsFit {
    let c = &hybrid::spec()["calibrated"];
    let values = c["values"].as_object().map(|o| o.iter().filter_map(|(k, v)| Some((k.clone(), v.as_f64()?))).collect()).unwrap_or_default();
    PhysicsFit { source: "spec".into(), values, impulse_sigma_rel: 0.1, reduced_chi2: c["reduced_chi2"].as_f64().unwrap_or(0.0), ..Default::default() }
}

/// The fit for these fires, cached on their contents (a fit costs a few hundred burns).
pub fn cached_fit(inputs: &[FitInput]) -> PhysicsFit {
    static CACHE: Mutex<Option<(String, PhysicsFit)>> = Mutex::new(None);
    let fires = fire_data(inputs);
    let key = format!("{:?}", fires);
    let key = format!("{:x}", key.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3)));
    if let Some((k, f)) = CACHE.lock().unwrap().as_ref() {
        if *k == key {
            return f.clone();
        }
    }
    let f = fit(&fires).unwrap_or_else(spec_fit);
    *CACHE.lock().unwrap() = Some((key, f.clone()));
    f
}

// ------------------------------------------------------------------ residual

/// Hyperparameters of the residual GP (squared-exponential, one length scale per feature).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ResidualHyper {
    pub kind: String,
    /// Feature order: fill reading kg, supercharge psig, tank C, initial port area cm^2.
    pub length_scales: [f64; 4],
    pub signal_sigma: f64,
    /// Added to each fire's own (impulse sigma / impulse)^2.
    pub noise_floor: f64,
    pub source: String,
}

impl Default for ResidualHyper {
    fn default() -> Self {
        Self {
            kind: "gp_se_ard".into(),
            length_scales: [2.0, 250.0, 10.0, 8.0],
            signal_sigma: 0.08,
            noise_floor: 0.02,
            source: "default (docs/hybrid-motor-model.md section 6)".into(),
        }
    }
}

pub fn features(fill_kg: f64, supercharge_psig: f64, tank_c: f64, port_area_m2: f64) -> [f64; 4] {
    [fill_kg, supercharge_psig, tank_c, port_area_m2 * 1e4]
}

/// A model of ln(I_measured / I_physics) from fire features: mean and variance.
pub trait ResidualModel {
    fn predict(&self, x: &[f64; 4]) -> (f64, f64);
}

#[derive(Clone, Debug)]
pub struct Gp {
    hyper: ResidualHyper,
    x: Vec<[f64; 4]>,
    alpha: Vec<f64>,
    chol: Vec<Vec<f64>>,
}

impl Gp {
    fn k(&self, a: &[f64; 4], b: &[f64; 4]) -> f64 {
        let d: f64 = (0..4).map(|i| ((a[i] - b[i]) / self.hyper.length_scales[i]).powi(2)).sum();
        self.hyper.signal_sigma.powi(2) * (-0.5 * d).exp()
    }

    /// Fits to (features, y, noise variance) when there are enough fires.
    pub fn fit(hyper: ResidualHyper, points: &[([f64; 4], f64, f64)]) -> Option<Self> {
        if points.len() < MIN_RESIDUAL_FIRES {
            return None;
        }
        let mut gp = Self { hyper, x: points.iter().map(|p| p.0).collect(), alpha: Vec::new(), chol: Vec::new() };
        let n = points.len();
        let mut k: Vec<Vec<f64>> = (0..n).map(|i| (0..n).map(|j| gp.k(&gp.x[i], &gp.x[j])).collect()).collect();
        for (i, p) in points.iter().enumerate() {
            k[i][i] += p.2 + gp.hyper.noise_floor.powi(2);
        }
        // Cholesky
        let mut l = vec![vec![0.0; n]; n];
        for i in 0..n {
            for j in 0..=i {
                let s: f64 = (0..j).map(|m| l[i][m] * l[j][m]).sum();
                if i == j {
                    let d = k[i][i] - s;
                    if d <= 0.0 {
                        return None;
                    }
                    l[i][i] = d.sqrt();
                } else {
                    l[i][j] = (k[i][j] - s) / l[j][j];
                }
            }
        }
        let y: Vec<f64> = points.iter().map(|p| p.1).collect();
        let z = forward(&l, &y);
        gp.alpha = backward(&l, &z);
        gp.chol = l;
        Some(gp)
    }
}

fn forward(l: &[Vec<f64>], b: &[f64]) -> Vec<f64> {
    let n = b.len();
    let mut z = vec![0.0; n];
    for i in 0..n {
        z[i] = (b[i] - (0..i).map(|j| l[i][j] * z[j]).sum::<f64>()) / l[i][i];
    }
    z
}

fn backward(l: &[Vec<f64>], z: &[f64]) -> Vec<f64> {
    let n = z.len();
    let mut x = vec![0.0; n];
    for i in (0..n).rev() {
        x[i] = (z[i] - (i + 1..n).map(|j| l[j][i] * x[j]).sum::<f64>()) / l[i][i];
    }
    x
}

impl ResidualModel for Gp {
    fn predict(&self, x: &[f64; 4]) -> (f64, f64) {
        let ks: Vec<f64> = self.x.iter().map(|xi| self.k(x, xi)).collect();
        let mean = ks.iter().zip(&self.alpha).map(|(a, b)| a * b).sum();
        let v = forward(&self.chol, &ks);
        let var = (self.hyper.signal_sigma.powi(2) - v.iter().map(|a| a * a).sum::<f64>()).max(0.0);
        (mean, var)
    }
}

/// The residual GP over these fires under `fit` (None below [`MIN_RESIDUAL_FIRES`]).
pub fn residual(fit: &PhysicsFit, fires: &[FireData], hyper: ResidualHyper) -> Option<Gp> {
    if fires.len() < MIN_RESIDUAL_FIRES {
        return None;
    }
    let p = fit.params(None);
    let points: Vec<([f64; 4], f64, f64)> = fires
        .iter()
        .map(|f| {
            let (_, b) = fire_residuals(&p, f);
            let area0 = hybrid::data().grain(&with_grain(&p, &f.grain).grain).at(0.0).0;
            (features(f.fill_kg, f.supercharge_psig, f.tank_c, area0), (f.impulse_ns / b.summary.impulse_ns).ln(), (f.impulse_sigma_ns / f.impulse_ns).powi(2))
        })
        .collect();
    Gp::fit(hyper, &points)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_inputs() -> Vec<FitInput> {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("hybrid-fit-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        crate::fire_archive::ensure_seeded(&dir).unwrap();
        let f = crate::fire_archive::fit_inputs_in(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        f
    }

    #[test]
    fn the_station_fit_of_the_seed_matches_the_python_calibration() {
        let inputs = seed_inputs();
        assert_eq!(inputs.len(), 1, "the 2026-11-16 seed is the accepted fire");
        assert!(inputs[0].tank.len() > 50, "the seed ships its tank trace");
        let fires = fire_data(&inputs);
        let f = fit(&fires).unwrap();
        let spec = &hybrid::spec()["calibrated"]["values"];
        for (k, v) in &f.values {
            let want = spec[k].as_f64().unwrap();
            assert!((v / want - 1.0).abs() < 0.05, "{k}: station {v} vs hybrid_calibrate.py {want}");
        }
        let q = &f.quality[0];
        assert!(q.impulse_error_pct.abs() < 8.0, "{q:?}");
        assert!(q.thrust_rms_n < 320.0 && q.tank_rms_psi.unwrap() < 10.0, "{q:?}");
    }

    #[test]
    fn the_residual_is_off_below_three_fires_and_learns_a_bias_above() {
        let fit = spec_fit();
        let inputs = seed_inputs();
        let one = fire_data(&inputs);
        assert!(residual(&fit, &one, ResidualHyper::default()).is_none());
        // Synthetic: three more fires at other fills that all came in 10 % above the physics.
        let p = fit.params(Some(hybrid::fire_grain()));
        let mut fires = one.clone();
        for (i, fill) in [4.5, 6.5, 7.5].iter().enumerate() {
            let b = hybrid::simulate(&p, hybrid::data(), *fill, 0.0, 0.0, p.test_ambient_pa, 0.01);
            let mut f = one[0].clone();
            f.id = format!("synthetic-{i}");
            f.fill_kg = *fill;
            f.tank_c = 0.0;
            f.impulse_ns = 1.10 * b.summary.impulse_ns;
            f.impulse_sigma_ns = 0.03 * f.impulse_ns;
            fires.push(f);
        }
        let gp = residual(&fit, &fires, ResidualHyper::default()).unwrap();
        let area0 = hybrid::data().grain(&p.grain).at(0.0).0;
        let (m, v) = gp.predict(&features(6.0, 0.0, 0.0, area0));
        assert!(m > 0.03 && m < 0.12, "learns most of the +10 %: {m}");
        assert!(v < ResidualHyper::default().signal_sigma.powi(2), "and is surer near the data: {v}");
        let (_, far) = gp.predict(&features(6.0, 900.0, 30.0, area0));
        assert!(far > v, "less sure far from the fires");
    }
}
