//! The motor as a function of the fill (docs/apogee-planner.md section 2).
//!
//! I = c_eff(P̄) · m_fill, c_eff = c_ref · (P̄ / P̄_ref)^n, where m_fill is the GS
//! fill reading at ignition and P̄ the mean liquid-phase tank pressure. The
//! measured curve is scaled by a = (P̄/P̄_ref)^(0.5+n) on the thrust axis and
//! by b = (I/I_ref)/a on the time axis. c_ref and n are fitted from the
//! accepted archive fires (weighted least squares in log space with a prior on
//! n), so the uncertainty shrinks as fires accumulate.
use super::flight::{Config, Motor, THRUST_CSV};
use super::n2o;
use serde::{Deserialize, Serialize};

/// The 2026-11-16 fire the base curve comes from.
pub const REF_FILL_KG: f64 = 5.65;
pub const REF_IMPULSE_NS: f64 = 9192.0;
pub const REF_IMPULSE_SIGMA_NS: f64 = 900.0;
/// Tank pressure at valve open (psig): a self-pressurised blowdown.
pub const REF_TANK_PSIG: f64 = 404.0;
pub const N_PRIOR: f64 = 0.3;
pub const N_PRIOR_SIGMA: f64 = 0.2;
/// Oxidiser-to-fuel ratio for the trajectory's propellant mass (vehicle.json: 6.7 kg for 5.6 kg N2O).
pub const OF_RATIO: f64 = 5.0;
/// Relative impulse uncertainty added per unit |ln(m / m_ref)| (mass extrapolation).
pub const MASS_EXTRAPOLATION_SIGMA: f64 = 0.10;

/// Mean liquid-phase pressure of the reference fire (Pa abs).
pub fn reference_mean_pressure_pa() -> f64 {
    n2o::SELF_PRESSURISED_FACTOR * (REF_TANK_PSIG + n2o::ATM_PSI) * n2o::PA_PER_PSI
}

/// Mean liquid-phase pressure (Pa abs) of a fire from what the archive knows:
/// its tank pressure at ignition (taken as the start of the liquid phase), or,
/// failing that, the vapour pressure at the ambient temperature.
pub fn mean_pressure_for_fire(tank_psig: Option<f64>, supercharge_psig: Option<f64>, ambient_c: Option<f64>, fill_kg: f64) -> f64 {
    match (tank_psig, ambient_c) {
        (Some(p), _) => {
            let t = n2o::saturation_temperature_c((p + n2o::ATM_PSI) * n2o::PA_PER_PSI);
            let sc = supercharge_psig.unwrap_or(p).max(p);
            n2o::mean_liquid_phase_pressure_pa(12.0, fill_kg, t, sc)
        }
        (None, Some(t)) => n2o::mean_liquid_phase_pressure_pa(12.0, fill_kg, t, supercharge_psig.unwrap_or(0.0)),
        _ => reference_mean_pressure_pa(),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FitPoint {
    pub id: String,
    pub label: String,
    pub fill_kg: f64,
    pub mean_pressure_pa: f64,
    pub impulse_ns: f64,
    pub impulse_sigma_ns: f64,
}

impl FitPoint {
    /// The 2026-11-16 fire, used when the archive is empty.
    pub fn reference() -> Self {
        Self {
            id: "2026-11-16_static_fire".into(),
            label: "2026-11-16 static fire".into(),
            fill_kg: REF_FILL_KG,
            mean_pressure_pa: reference_mean_pressure_pa(),
            impulse_ns: REF_IMPULSE_NS,
            impulse_sigma_ns: REF_IMPULSE_SIGMA_NS,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FitFire {
    pub id: String,
    pub label: String,
    pub fill_kg: f64,
    pub impulse_ns: f64,
    /// Share of the fit's information (0..1).
    pub weight: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MotorFit {
    /// Impulse per kg of fill reading at the reference pressure.
    pub c_ref_ns_per_kg: f64,
    pub n: f64,
    /// Relative 1σ of c_ref.
    pub sigma_c_rel: f64,
    pub sigma_n: f64,
    /// Covariance of (ln c_ref, n).
    pub cov: [[f64; 2]; 2],
    pub p_ref_pa: f64,
    pub p_ref_psi: f64,
    pub fires: Vec<FitFire>,
}

impl MotorFit {
    /// Weighted least squares of ln(I/m) = ln c + n ln(P̄/P̄_ref), with the
    /// prior on n as a pseudo-observation. One fire fits c only.
    pub fn fit(points: &[FitPoint]) -> Self {
        let points: Vec<&FitPoint> = points
            .iter()
            .filter(|p| p.fill_kg > 0.1 && p.impulse_ns > 0.0 && p.impulse_sigma_ns > 0.0 && p.mean_pressure_pa > 0.0)
            .collect();
        let p_ref = reference_mean_pressure_pa();
        let wn = 1.0 / (N_PRIOR_SIGMA * N_PRIOR_SIGMA);
        // Normal matrix for parameters (ln c, n).
        let (mut a00, mut a01, mut a11, mut b0, mut b1) = (0.0, 0.0, wn, 0.0, wn * N_PRIOR);
        let mut weights = Vec::new();
        for p in &points {
            let y = (p.impulse_ns / p.fill_kg).ln();
            let x = (p.mean_pressure_pa / p_ref).ln();
            let s = p.impulse_sigma_ns / p.impulse_ns;
            let w = 1.0 / (s * s);
            a00 += w;
            a01 += w * x;
            a11 += w * x * x;
            b0 += w * y;
            b1 += w * x * y;
            weights.push(w);
        }
        if points.is_empty() {
            return Self::fit(&[FitPoint::reference()]);
        }
        let det = a00 * a11 - a01 * a01;
        let ln_c = (a11 * b0 - a01 * b1) / det;
        let n = (a00 * b1 - a01 * b0) / det;
        let cov = [[a11 / det, -a01 / det], [-a01 / det, a00 / det]];
        let total: f64 = weights.iter().sum();
        Self {
            c_ref_ns_per_kg: ln_c.exp(),
            n,
            sigma_c_rel: cov[0][0].sqrt(),
            sigma_n: cov[1][1].sqrt(),
            cov,
            p_ref_pa: p_ref,
            p_ref_psi: p_ref / n2o::PA_PER_PSI - n2o::ATM_PSI,
            fires: points
                .iter()
                .zip(&weights)
                .map(|(p, w)| FitFire {
                    id: p.id.clone(),
                    label: p.label.clone(),
                    fill_kg: p.fill_kg,
                    impulse_ns: p.impulse_ns,
                    weight: w / total,
                })
                .collect(),
        }
    }

    /// Impulse and its relative 1σ for a fill and mean pressure.
    pub fn impulse(&self, fill_kg: f64, mean_pressure_pa: f64) -> (f64, f64) {
        let x = (mean_pressure_pa / self.p_ref_pa).ln();
        let imp = self.c_ref_ns_per_kg * (mean_pressure_pa / self.p_ref_pa).powf(self.n) * fill_kg;
        let var_fit = self.cov[0][0] + 2.0 * x * self.cov[0][1] + x * x * self.cov[1][1];
        let m_ref = self.weighted_fill().unwrap_or(REF_FILL_KG);
        let ext = MASS_EXTRAPOLATION_SIGMA * (fill_kg.max(0.01) / m_ref).ln().abs();
        (imp, (var_fit + ext * ext).sqrt())
    }

    fn weighted_fill(&self) -> Option<f64> {
        if self.fires.is_empty() {
            return None;
        }
        Some(self.fires.iter().map(|f| f.weight * f.fill_kg.ln()).sum::<f64>().exp())
    }
}

/// Motor for a fill: the measured curve scaled to the fitted impulse.
#[derive(Clone, Debug)]
pub struct FillMotor {
    pub motor: Motor,
    pub impulse_ns: f64,
    pub sigma_rel: f64,
    #[cfg_attr(not(test), allow(dead_code))]
    pub mean_pressure_pa: f64,
}

pub fn motor_for_fill(cfg: &Config, fit: &MotorFit, fill_kg: f64, tank_c: f64, supercharge_psig: f64, tank_volume_l: f64, impulse_factor: f64) -> FillMotor {
    let base = Motor::from_config(&super::flight::MotorCfg { total_impulse_ns: None, ..cfg.motor.clone() }, THRUST_CSV);
    let pbar = n2o::mean_liquid_phase_pressure_pa(tank_volume_l, fill_kg, tank_c, supercharge_psig);
    let (imp, sigma) = fit.impulse(fill_kg, pbar);
    let imp = imp * impulse_factor;
    let a = (pbar / fit.p_ref_pa).powf(0.5 + fit.n);
    let b = (imp / base.impulse) / a;
    let mut motor = base.scaled(a, b);
    motor.prop_kg = fill_kg * (1.0 + 1.0 / OF_RATIO);
    FillMotor { motor, impulse_ns: imp, sigma_rel: sigma, mean_pressure_pa: pbar }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_fire_fits_c_only_and_reproduces_it() {
        let fit = MotorFit::fit(&[FitPoint::reference()]);
        assert!((fit.c_ref_ns_per_kg - REF_IMPULSE_NS / REF_FILL_KG).abs() < 1e-6);
        assert!((fit.n - N_PRIOR).abs() < 1e-9, "n stays at the prior");
        assert!((fit.sigma_c_rel - REF_IMPULSE_SIGMA_NS / REF_IMPULSE_NS).abs() < 1e-9);
        let (i, s) = fit.impulse(REF_FILL_KG, reference_mean_pressure_pa());
        assert!((i - REF_IMPULSE_NS).abs() < 1e-6);
        assert!((s - fit.sigma_c_rel).abs() < 1e-9);
        // Extrapolating in mass widens the band.
        assert!(fit.impulse(8.0, reference_mean_pressure_pa()).1 > s);
    }

    #[test]
    fn uncertainty_shrinks_and_n_moves_as_fires_accumulate() {
        let p_ref = reference_mean_pressure_pa();
        // Synthetic truth: c = 1700, n = 0.5.
        let fire = |id: &str, m: f64, pk: f64| {
            let p = p_ref * pk;
            FitPoint { id: id.into(), label: id.into(), fill_kg: m, mean_pressure_pa: p, impulse_ns: 1700.0 * pk.powf(0.5) * m, impulse_sigma_ns: 0.05 * 1700.0 * m }
        };
        let one = MotorFit::fit(&[fire("a", 5.0, 1.0)]);
        let many = MotorFit::fit(&[fire("a", 5.0, 1.0), fire("b", 6.0, 1.4), fire("c", 7.0, 1.8), fire("d", 6.5, 1.2)]);
        assert!(many.sigma_c_rel < one.sigma_c_rel);
        assert!(many.sigma_n < N_PRIOR_SIGMA);
        assert!((many.n - 0.5).abs() < 0.05, "{}", many.n);
        assert!((many.c_ref_ns_per_kg / 1700.0 - 1.0).abs() < 0.02, "{}", many.c_ref_ns_per_kg);
        let w: f64 = many.fires.iter().map(|f| f.weight).sum();
        assert!((w - 1.0).abs() < 1e-9);
    }

    #[test]
    fn reference_fill_rebuilds_the_measured_motor() {
        let cfg = Config::reference();
        let fit = MotorFit::fit(&[FitPoint::reference()]);
        let t = n2o::saturation_temperature_c((REF_TANK_PSIG + n2o::ATM_PSI) * n2o::PA_PER_PSI);
        let m = motor_for_fill(&cfg, &fit, REF_FILL_KG, t, 0.0, 12.0, 1.0);
        assert!((m.motor.impulse - REF_IMPULSE_NS).abs() < 1.0, "{}", m.motor.impulse);
        let base = Motor::from_config(&cfg.motor, THRUST_CSV);
        assert!((m.motor.burn_s() - base.burn_s()).abs() < 1e-3, "same pressure, same curve: {} vs {} (pbar {} ref {})", m.motor.burn_s(), base.burn_s(), m.mean_pressure_pa, fit.p_ref_pa);
        // More fill burns longer at the same pressure; a supercharge burns harder.
        let big = motor_for_fill(&cfg, &fit, 7.5, t, 0.0, 12.0, 1.0);
        assert!(big.motor.burn_s() > base.burn_s());
        let hard = motor_for_fill(&cfg, &fit, REF_FILL_KG, t, 750.0, 12.0, 1.0);
        assert!(hard.motor.impulse > m.motor.impulse);
        assert!(hard.motor.f.iter().cloned().fold(0.0, f64::max) > base.f.iter().cloned().fold(0.0, f64::max));
    }
}
