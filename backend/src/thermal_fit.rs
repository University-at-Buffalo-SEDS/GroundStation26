//! Empirical fit on settled period means, never on individual ADC samples.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Quality {
    pub ready: bool,
    pub message: String,
    pub span_c: f64,
    pub slope: f64,
    pub slope_margin: Option<f64>,
    pub reference_c: f64,
    pub zero_raw: f64,
}

pub fn assess(points: &[[f64; 2]]) -> Quality {
    let mut q = Quality::default();
    if points.iter().any(|p| p.iter().any(|v| !v.is_finite())) {
        q.message = "Invalid settled temperature point".into();
        return q;
    }
    if points.is_empty() {
        q.message = "Waiting for settled temperature periods".into();
        return q;
    }
    let n = points.len() as f64;
    q.reference_c = points.iter().map(|p| p[0]).sum::<f64>() / n;
    q.zero_raw = points.iter().map(|p| p[1]).sum::<f64>() / n;
    let lo = points.iter().map(|p| p[0]).fold(f64::INFINITY, f64::min);
    let hi = points
        .iter()
        .map(|p| p[0])
        .fold(f64::NEG_INFINITY, f64::max);
    q.span_c = hi - lo;
    if points.len() < 6 || q.span_c < 0.25 {
        q.message = format!(
            "Collect at least 6 settled periods spanning 0.25 °C (now {} periods, {:.3} °C)",
            points.len(),
            q.span_c
        );
        return q;
    }
    // Require coverage between the extremes, not just repeated samples at two temperatures.
    if !points
        .iter()
        .any(|p| p[0] > lo + q.span_c * 0.25 && p[0] < hi - q.span_c * 0.25)
    {
        q.message =
            "Collect settled periods at intermediate temperatures as well as both extremes".into();
        return q;
    }
    let xx = points
        .iter()
        .map(|p| (p[0] - q.reference_c).powi(2))
        .sum::<f64>();
    q.slope = points
        .iter()
        .map(|p| (p[0] - q.reference_c) * (p[1] - q.zero_raw))
        .sum::<f64>()
        / xx;
    let residual = points
        .iter()
        .map(|p| (p[1] - q.zero_raw - q.slope * (p[0] - q.reference_c)).powi(2))
        .sum::<f64>();
    // Conservative t multiplier for >=4 degrees of freedom. This is a repeatability
    // estimate; autocorrelation and systematic drift are not covered by this margin.
    let margin = 2.776 * (residual / (n - 2.) / xx).sqrt();
    q.slope_margin = Some(margin);
    q.ready = q.slope.is_finite()
        && margin.is_finite()
        && q.slope.abs() > 1e-12
        && margin <= q.slope.abs() * 0.5;
    q.message = if q.ready {
        "Fit ready: repeat on cooling to check that the correction reverses. Use near the measured temperature range.".into()
    } else {
        "Temperature effect is not resolved consistently; collect more settled periods or a wider temperature range".into()
    };
    q
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn small_repeated_span_qualifies_but_noise_and_insufficient_coverage_do_not() {
        let points: Vec<_> = (0..8)
            .map(|i| {
                let t = 30. + i as f64 * 0.05;
                [t, 0.2 + 0.002 * (t - 30.)]
            })
            .collect();
        let q = assess(&points);
        assert!(q.ready, "{}", q.message);
        assert!((q.slope - 0.002).abs() < 1e-12);
        assert!(!assess(&points[..5]).ready);
        assert!(!assess(&[[30., 0.2]; 8]).ready);
        let noisy: Vec<_> = points
            .iter()
            .enumerate()
            .map(|(i, p)| [p[0], p[1] + if i % 2 == 0 { 0.01 } else { -0.01 }])
            .collect();
        assert!(!assess(&noisy).ready);
        assert!(
            !assess(&[
                [30., 0.2],
                [30., 0.2],
                [30., 0.2],
                [31., 0.3],
                [31., 0.3],
                [31., 0.3]
            ])
            .ready
        );
        assert!(
            !assess(&[
                [30., 0.2],
                [30.1, 0.2],
                [30.2, 0.2],
                [30.3, 0.2],
                [30.4, 0.2],
                [30.5, 0.2]
            ])
            .ready
        );
        assert!(!assess(&[[f64::NAN, 0.]]).ready);
    }
}
