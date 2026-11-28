//! Fixed explicit-resume deadband; no clocks, hardware or automatic restart.
use crate::{Effects, Sample};

pub const RESUME_DEFICIT_KG: f32 = 0.2;

#[derive(Debug)]
pub enum FillStart {
    Noop,
    Started(Effects),
}

/// Calibrated signed targets retain the existing loadcell direction convention.
/// Compare to the f32 kg boundary so an exactly represented `target - 0.2`
/// remains inclusive instead of losing equality through subtractive cancellation.
pub fn resume_required(now_ms: u64, mass: Option<Sample>, target_kg: f32) -> Result<bool, String> {
    if !target_kg.is_finite() || target_kg == 0.0 {
        return Err("Fill requires a finite, nonzero configured mass target".into());
    }
    let mass = mass
        .filter(|sample| {
            sample.value.is_finite() && sample.at_ms <= now_ms && now_ms - sample.at_ms <= 2000
        })
        .ok_or_else(|| "Fill requires fresh calibrated fill weight before starting".to_string())?;
    Ok(mass.value * target_kg.signum() <= target_kg.abs() - RESUME_DEFICIT_KG)
}
