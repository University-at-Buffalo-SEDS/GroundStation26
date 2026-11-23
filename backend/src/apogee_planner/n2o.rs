//! Saturated nitrous oxide properties (ESDU 91022 correlations, as tabulated in
//! the hybrid-rocket literature) and the run-tank fill limits.
//!
//! Temperatures in °C at the API, kelvin inside; pressures in Pa (absolute)
//! unless a name says psig.

pub const T_CRIT_K: f64 = 309.57;
pub const P_CRIT_PA: f64 = 7.251e6;
pub const RHO_CRIT: f64 = 452.0;
pub const PA_PER_PSI: f64 = 6894.757293;
pub const ATM_PSI: f64 = 14.696;

fn reduced(t_c: f64) -> f64 {
    ((t_c + 273.15) / T_CRIT_K).clamp(0.6, 0.9999)
}

/// Saturation (vapour) pressure, Pa absolute.
pub fn vapour_pressure_pa(t_c: f64) -> f64 {
    let tr = reduced(t_c);
    let tau = 1.0 - tr;
    let b = [-6.71893, 1.35966, -1.3779, -4.051];
    let s = b[0] * tau + b[1] * tau.powf(1.5) + b[2] * tau.powf(2.5) + b[3] * tau.powi(5);
    P_CRIT_PA * (s / tr).exp()
}

/// Saturation pressure in psig at the given ambient pressure (psia).
pub fn vapour_pressure_psig(t_c: f64) -> f64 {
    vapour_pressure_pa(t_c) / PA_PER_PSI - ATM_PSI
}

/// Saturated liquid density, kg/m³.
pub fn liquid_density(t_c: f64) -> f64 {
    let tau = 1.0 - reduced(t_c);
    let b = [1.72328, -0.83950, 0.51060, -0.10412];
    let s = b[0] * tau.powf(1.0 / 3.0) + b[1] * tau.powf(2.0 / 3.0) + b[2] * tau + b[3] * tau.powf(4.0 / 3.0);
    RHO_CRIT * s.exp()
}

/// Saturated vapour density, kg/m³.
pub fn vapour_density(t_c: f64) -> f64 {
    let x = 1.0 / reduced(t_c) - 1.0;
    let b = [-1.00900, -6.28792, 7.50332, -7.90463, 0.629427];
    let s = b[0] * x.powf(1.0 / 3.0)
        + b[1] * x.powf(2.0 / 3.0)
        + b[2] * x
        + b[3] * x.powf(4.0 / 3.0)
        + b[4] * x.powf(5.0 / 3.0);
    RHO_CRIT * s.exp()
}

/// Largest N2O load (liquid + vapour) that leaves `min_ullage` of the tank as
/// vapour space at `t_c`.
pub fn max_fill_kg(volume_l: f64, min_ullage: f64, t_c: f64) -> f64 {
    let v = volume_l / 1000.0;
    let u = min_ullage.clamp(0.0, 0.95);
    liquid_density(t_c) * v * (1.0 - u) + vapour_density(t_c) * v * u
}

/// Ullage (vapour-space fraction) of a tank holding `mass_kg` at `t_c`.
pub fn ullage(volume_l: f64, mass_kg: f64, t_c: f64) -> f64 {
    let v = volume_l / 1000.0;
    let (rl, rv) = (liquid_density(t_c), vapour_density(t_c));
    // m = rl*v*(1-u) + rv*v*u
    ((rl * v - mass_kg) / ((rl - rv) * v)).clamp(0.0, 1.0)
}

/// Temperature (°C) whose vapour pressure is `p_pa`, by bisection.
pub fn saturation_temperature_c(p_pa: f64) -> f64 {
    let (mut lo, mut hi) = (-90.0_f64, 36.3_f64);
    for _ in 0..60 {
        let mid = 0.5 * (lo + hi);
        if vapour_pressure_pa(mid) < p_pa {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

/// Polytropic exponent of the nitrogen pad as the liquid drains.
pub const N2_POLYTROPIC: f64 = 1.2;
/// Mean liquid-phase pressure of a self-pressurised blowdown, as a fraction of
/// the starting vapour pressure (evaporative cooling; 2026-11-16: 404 -> ~330 psi).
pub const SELF_PRESSURISED_FACTOR: f64 = 0.85;

/// Mean tank pressure (Pa abs) over the liquid phase for a tank at `t_c` holding
/// `mass_kg`, supercharged with nitrogen to `supercharge_psig` (below the vapour
/// pressure means no nitrogen).
pub fn mean_liquid_phase_pressure_pa(volume_l: f64, mass_kg: f64, t_c: f64, supercharge_psig: f64) -> f64 {
    let pv = vapour_pressure_pa(t_c);
    let p0 = (supercharge_psig + ATM_PSI) * PA_PER_PSI;
    let base = SELF_PRESSURISED_FACTOR * pv;
    if p0 <= pv {
        return base;
    }
    let pn2 = p0 - pv;
    let u0 = ullage(volume_l, mass_kg, t_c).max(0.02);
    let k = N2_POLYTROPIC;
    // Mean over a linear drain of (u0 / (u0 + (1-u0) x))^k, x in [0, 1].
    let mean = if (1.0 - u0) < 1e-9 {
        1.0
    } else {
        u0.powf(k) * (1.0 - u0.powf(1.0 - k)) / ((1.0 - k) * (1.0 - u0))
    };
    base + pn2 * mean
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn properties_match_published_values() {
        // NIST webbook / ESDU 91022: 20 °C: 5.06 MPa, 786 kg/m³ liquid, 159 kg/m³ vapour; 0 °C: 3.13 MPa.
        assert!((vapour_pressure_pa(20.0) / 1e6 - 5.06).abs() < 0.03, "{}", vapour_pressure_pa(20.0));
        assert!((vapour_pressure_pa(0.0) / 1e6 - 3.13).abs() < 0.04, "{}", vapour_pressure_pa(0.0));
        assert!((liquid_density(20.0) - 786.0).abs() < 5.0);
        assert!((vapour_density(20.0) - 159.0).abs() < 8.0, "{}", vapour_density(20.0));
        // The 2026-11-16 tank started at 404 psig: N2O at about -3 °C.
        let t = saturation_temperature_c((404.0 + ATM_PSI) * PA_PER_PSI);
        assert!((-5.0..-1.0).contains(&t), "{t}");
    }

    #[test]
    fn twelve_litre_tank_at_fifteen_percent_ullage_holds_the_default_8_kg() {
        let m = max_fill_kg(12.0, 0.15, 20.0);
        assert!((m - 8.3).abs() < 0.3, "{m}");
        assert!(max_fill_kg(12.0, 0.15, 30.0) < m, "warmer liquid is less dense");
        assert!((ullage(12.0, m, 20.0) - 0.15).abs() < 1e-6);
    }

    #[test]
    fn supercharge_raises_the_mean_pressure_only_above_vapour_pressure() {
        let pv = vapour_pressure_psig(10.0);
        let self_p = mean_liquid_phase_pressure_pa(12.0, 6.0, 10.0, pv - 50.0);
        assert!((self_p - SELF_PRESSURISED_FACTOR * vapour_pressure_pa(10.0)).abs() < 1.0);
        let sc = mean_liquid_phase_pressure_pa(12.0, 6.0, 10.0, 750.0);
        assert!(sc > self_p);
        // A fuller tank (less ullage) loses its nitrogen pad pressure faster.
        let full = mean_liquid_phase_pressure_pa(12.0, 8.0, 10.0, 750.0);
        assert!(full < sc);
    }
}
