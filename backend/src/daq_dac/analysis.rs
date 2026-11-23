//! Pure signal maths for DAC validation: expected waveforms, offset/gain/noise,
//! amplitude and frequency estimates, and sweep line fits.
use super::model::{PathModel, Tolerances};
use super::wire::Waveform;
use serde::Serialize;
use std::f64::consts::{PI, TAU};

/// Zero-order-hold samples per period when status does not say (v1 tables).
pub const SAMPLES_PER_PERIOD: f64 = 32.0;

/// The waveform the DAQ reports it is driving, in DAC-pin volts.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Wave {
    pub waveform: Waveform,
    pub offset_v: f64,
    pub amplitude_v: f64,
    /// Actual frequency for the periodic waves.
    pub frequency_hz: f64,
    pub steps: u16,
    pub dwell_ms: u32,
    /// Table length playing (0: unknown, taken as 32).
    pub samples: u16,
    /// Wave flags (sawtooth ramp-down).
    pub flags: u8,
    /// Square duty, x 65536 (0: 50 %).
    pub duty_q16: u16,
}
impl Wave {
    pub fn dc(offset_v: f64) -> Self {
        Self {
            offset_v,
            ..Self::default()
        }
    }
    /// Chirp, noise and uploaded tables have no expected-trace model here.
    pub fn modelled(&self) -> bool {
        !matches!(
            self.waveform,
            Waveform::Chirp | Waveform::Noise | Waveform::Arbitrary
        )
    }
    pub fn samples_per_period(&self) -> f64 {
        if self.samples > 1 {
            f64::from(self.samples)
        } else {
            SAMPLES_PER_PERIOD
        }
    }
    fn duty(&self) -> f64 {
        if self.duty_q16 == 0 {
            0.5
        } else {
            f64::from(self.duty_q16) / 65536.0
        }
    }
    /// Shapes whose phase is found by grid search rather than a fundamental fit.
    fn grid_phase(&self) -> bool {
        match self.waveform {
            Waveform::Staircase | Waveform::Sawtooth => true,
            Waveform::Square => (self.duty() - 0.5).abs() > 1e-9,
            _ => false,
        }
    }
    /// Repetition frequency, or None for DC.
    pub fn period_hz(&self) -> Option<f64> {
        match self.waveform {
            Waveform::Dc | Waveform::Chirp | Waveform::Noise | Waveform::Arbitrary => None,
            Waveform::Staircase => {
                let period = f64::from(self.steps.max(2)) * f64::from(self.dwell_ms) / 1000.0;
                (period > 0.0).then(|| 1.0 / period)
            }
            _ => (self.frequency_hz > 0.0).then_some(self.frequency_hz),
        }
    }
    /// Unit shape at fraction `frac` (0..1) of one period, with the
    /// fundamental of square/sine/triangle in phase with sin(2 pi frac).
    fn unit(&self, frac: f64) -> f64 {
        let f = frac.rem_euclid(1.0);
        match self.waveform {
            Waveform::Dc | Waveform::Chirp | Waveform::Noise | Waveform::Arbitrary => 0.0,
            Waveform::Sine => (TAU * f).sin(),
            Waveform::Sawtooth => {
                if self.flags & super::wire::FLAG_RAMP_DOWN != 0 {
                    1.0 - 2.0 * f
                } else {
                    -1.0 + 2.0 * f
                }
            }
            Waveform::Square => {
                if f < self.duty() {
                    1.0
                } else {
                    -1.0
                }
            }
            Waveform::Triangle => {
                if f < 0.25 {
                    4.0 * f
                } else if f < 0.75 {
                    2.0 - 4.0 * f
                } else {
                    4.0 * f - 4.0
                }
            }
            Waveform::Staircase => {
                let n = self.steps.max(2) as f64;
                let i = (f * n).floor().min(n - 1.0);
                -1.0 + 2.0 * i / (n - 1.0)
            }
        }
    }
    /// DAC pin volts at `frac` of a period, with the zero-order hold.
    pub fn pin_at(&self, frac: f64) -> f64 {
        let held = if self.waveform.is_periodic() {
            let n = self.samples_per_period();
            (frac.rem_euclid(1.0) * n).floor() / n
        } else {
            frac
        };
        self.offset_v + self.amplitude_v * self.unit(held)
    }
}

/// Fundamental amplitude of the unit shape relative to its peak.
pub fn fundamental_coefficient(waveform: Waveform) -> f64 {
    match waveform {
        Waveform::Sine => 1.0,
        Waveform::Square => 4.0 / PI,
        Waveform::Triangle => 8.0 / (PI * PI),
        _ => 0.0,
    }
}
/// The zero-order hold scales the fundamental by sinc(pi / N) and delays it by
/// half a hold sample.
pub fn zoh_gain(samples_per_period: f64) -> f64 {
    let x = PI / samples_per_period;
    x.sin() / x
}

pub fn mean(xs: &[f64]) -> Option<f64> {
    (!xs.is_empty()).then(|| xs.iter().sum::<f64>() / xs.len() as f64)
}
/// Population standard deviation.
pub fn std_dev(xs: &[f64]) -> Option<f64> {
    let m = mean(xs)?;
    Some((xs.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / xs.len() as f64).sqrt())
}

/// Least-squares y = slope x + intercept.
pub fn linear_fit(xs: &[f64], ys: &[f64]) -> Option<(f64, f64)> {
    if xs.len() != ys.len() || xs.len() < 2 {
        return None;
    }
    let mx = mean(xs)?;
    let my = mean(ys)?;
    let sxx: f64 = xs.iter().map(|x| (x - mx) * (x - mx)).sum();
    let sxy: f64 = xs.iter().zip(ys).map(|(x, y)| (x - mx) * (y - my)).sum();
    if sxx <= 1e-18 * xs.len() as f64 {
        return None;
    }
    let slope = sxy / sxx;
    Some((slope, my - slope * mx))
}

/// Least squares y = c + a cos(wt) + b sin(wt) at a known frequency.
/// Returns (c, amplitude, phase) with y ~ c + amplitude sin(wt + phase).
pub fn fit_fundamental(t_s: &[f64], y: &[f64], frequency_hz: f64) -> Option<(f64, f64, f64)> {
    if t_s.len() != y.len() || t_s.len() < 4 || frequency_hz <= 0.0 {
        return None;
    }
    let w = TAU * frequency_hz;
    // Normal equations for basis [1, cos, sin].
    let mut m = [[0.0f64; 3]; 3];
    let mut r = [0.0f64; 3];
    for (t, v) in t_s.iter().zip(y) {
        let basis = [1.0, (w * t).cos(), (w * t).sin()];
        for i in 0..3 {
            r[i] += basis[i] * v;
            for j in 0..3 {
                m[i][j] += basis[i] * basis[j];
            }
        }
    }
    let [c, a, b] = solve3(m, r)?;
    // a cos + b sin = R sin(wt + phase) with R cos(phase) = b, R sin(phase) = a.
    Some((c, a.hypot(b), a.atan2(b)))
}

fn solve3(mut m: [[f64; 3]; 3], mut r: [f64; 3]) -> Option<[f64; 3]> {
    for col in 0..3 {
        let pivot = (col..3).max_by(|a, b| m[*a][col].abs().total_cmp(&m[*b][col].abs()))?;
        if m[pivot][col].abs() < 1e-12 {
            return None;
        }
        m.swap(col, pivot);
        r.swap(col, pivot);
        for row in 0..3 {
            if row != col {
                let f = m[row][col] / m[col][col];
                for k in 0..3 {
                    m[row][k] -= f * m[col][k];
                }
                r[row] -= f * r[col];
            }
        }
    }
    Some([r[0] / m[0][0], r[1] / m[1][1], r[2] / m[2][2]])
}

/// Frequency from rising crossings of the mean, with hysteresis of a fifth of
/// the signal's standard deviation and linear interpolation of crossing times.
pub fn zero_crossing_frequency(t_s: &[f64], y: &[f64]) -> Option<f64> {
    let m = mean(y)?;
    let hysteresis = 0.2 * std_dev(y)?;
    if hysteresis <= 0.0 {
        return None;
    }
    let mut armed = false;
    let mut rises = Vec::new();
    for i in 1..y.len() {
        let (a, b) = (y[i - 1] - m, y[i] - m);
        if b < -hysteresis {
            armed = true;
        }
        if armed && a < 0.0 && b >= 0.0 {
            let t = t_s[i - 1] + (t_s[i] - t_s[i - 1]) * (-a) / (b - a);
            rises.push(t);
            armed = false;
        }
    }
    if rises.len() < 2 {
        return None;
    }
    let span = rises.last()? - rises.first()?;
    (span > 0.0).then(|| (rises.len() - 1) as f64 / span)
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Analysis {
    pub samples: usize,
    pub offset_error: Option<f64>,
    pub gain_error_pct: Option<f64>,
    pub noise_rms: Option<f64>,
    pub measured_amplitude: Option<f64>,
    pub expected_amplitude: Option<f64>,
    pub measured_frequency_hz: Option<f64>,
    pub expected_frequency_hz: Option<f64>,
}

/// Phase (fraction of a period at t = 0) of a staircase that best matches the
/// measured samples, by grid search of the de-meaned squared error.
fn staircase_phase(t_s: &[f64], y: &[f64], wave: &Wave, model: PathModel, f: f64) -> f64 {
    let grid = if wave.waveform == Waveform::Staircase {
        (u32::from(wave.steps.max(2)) * 16).min(1024)
    } else {
        256
    };
    let mut best = (f64::INFINITY, 0.0);
    for g in 0..grid {
        let phase = f64::from(g) / f64::from(grid);
        let residuals: Vec<f64> = t_s
            .iter()
            .zip(y)
            .map(|(t, v)| v - model.reported(wave.pin_at(t * f + phase)))
            .collect();
        let sse = std_dev(&residuals).unwrap_or(f64::INFINITY);
        if sse < best.0 {
            best = (sse, phase);
        }
    }
    best.1
}

/// Expected value per sample (same units as `y`, phase-aligned for periodic
/// waves) and the analysis of measured against expected. `t_s` is seconds on
/// any common time base.
pub fn analyse(t_s: &[f64], y: &[f64], wave: &Wave, model: PathModel) -> (Vec<f64>, Analysis) {
    let n = t_s.len().min(y.len());
    let (t_s, y) = (&t_s[..n], &y[..n]);
    let mut out = Analysis {
        samples: n,
        ..Default::default()
    };
    let Some(f) = wave.period_hz() else {
        let expected = model.reported(wave.offset_v);
        let exp = vec![expected; n];
        if let Some(m) = mean(y) {
            out.offset_error = Some(m - expected);
            out.noise_rms = std_dev(y);
        }
        return (exp, out);
    };
    let expected_amplitude = (model.slope * wave.amplitude_v).abs();
    out.expected_amplitude = Some(expected_amplitude);
    out.expected_frequency_hz = Some(f);
    if n < 4 {
        let exp = t_s
            .iter()
            .map(|t| model.reported(wave.pin_at(t * f)))
            .collect();
        return (exp, out);
    }
    // Phase as a fraction of a period at t = 0.
    let t0 = t_s[0];
    let rel: Vec<f64> = t_s.iter().map(|t| t - t0).collect();
    let phase = if wave.grid_phase() {
        staircase_phase(&rel, y, wave, model, f)
    } else {
        match fit_fundamental(&rel, y, f) {
            Some((_, amplitude, measured_phase)) => {
                out.measured_amplitude =
                    Some(amplitude / (fundamental_coefficient(wave.waveform) * zoh_gain(wave.samples_per_period())));
                // A negative path slope inverts the wave.
                let phase = if model.slope < 0.0 {
                    measured_phase + PI
                } else {
                    measured_phase
                };
                // The held wave's fundamental lags its shape by half a hold sample.
                phase / TAU + 0.5 / wave.samples_per_period()
            }
            None => 0.0,
        }
    };
    let expected: Vec<f64> = rel
        .iter()
        .map(|t| model.reported(wave.pin_at(t * f + phase)))
        .collect();
    let residual: Vec<f64> = y.iter().zip(&expected).map(|(m, e)| m - e).collect();
    out.offset_error = mean(&residual);
    let line = linear_fit(&expected, y);
    if wave.grid_phase() {
        if let Some((slope, _)) = line {
            out.measured_amplitude = Some(slope.abs() * expected_amplitude);
        }
    }
    if let Some(measured) = out.measured_amplitude
        && expected_amplitude > 0.0
    {
        out.gain_error_pct = Some((measured / expected_amplitude - 1.0) * 100.0);
    }
    out.noise_rms = match line {
        Some((slope, intercept)) => {
            let fit: Vec<f64> = expected
                .iter()
                .zip(y)
                .map(|(e, m)| m - (slope * e + intercept))
                .collect();
            std_dev(&fit)
        }
        None => std_dev(&residual),
    };
    // Frequency needs at least two full periods in the window.
    let span = rel.last().copied().unwrap_or(0.0);
    if span * f >= 2.0 {
        out.measured_frequency_hz = zero_crossing_frequency(&rel, y);
    }
    (expected, out)
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SweepPoint {
    pub dac_pin_v: f64,
    pub expected: f64,
    pub measured_mean: f64,
    pub measured_std: f64,
    pub readback_v: Option<f64>,
    pub samples: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub value: f64,
    pub limit: f64,
    pub pass: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SweepResult {
    pub offset_error: f64,
    pub gain_error_pct: f64,
    pub max_nonlinearity_pct_fs: f64,
    pub noise_rms: f64,
    pub checks: Vec<Check>,
    pub pass: bool,
}

/// Fit measured = slope x expected + intercept over the points. Offset is the
/// intercept (measured at expected 0), gain error is slope - 1, nonlinearity
/// is the largest residual from the line as a percentage of `full_scale`, and
/// noise is the RMS of the per-point standard deviations.
pub fn evaluate_sweep(
    points: &[SweepPoint],
    full_scale: f64,
    tol: &Tolerances,
) -> Option<SweepResult> {
    let xs: Vec<f64> = points.iter().map(|p| p.expected).collect();
    let ys: Vec<f64> = points.iter().map(|p| p.measured_mean).collect();
    let (slope, intercept) = linear_fit(&xs, &ys)?;
    let max_residual = xs
        .iter()
        .zip(&ys)
        .map(|(x, y)| (y - (slope * x + intercept)).abs())
        .fold(0.0, f64::max);
    let nonlinearity = if full_scale > 0.0 {
        max_residual / full_scale * 100.0
    } else {
        0.0
    };
    let noise = (points
        .iter()
        .map(|p| p.measured_std * p.measured_std)
        .sum::<f64>()
        / points.len() as f64)
        .sqrt();
    let gain = (slope - 1.0) * 100.0;
    let check = |name, value: f64, limit: f64| Check {
        name,
        value,
        limit,
        pass: value.abs() <= limit,
    };
    let checks = vec![
        check("offset", intercept, tol.offset),
        check("gain", gain, tol.gain_pct),
        check("nonlinearity", nonlinearity, tol.nonlinearity_pct_fs),
        check("noise", noise, tol.noise_rms),
    ];
    let pass = checks.iter().all(|c| c.pass);
    Some(SweepResult {
        offset_error: intercept,
        gain_error_pct: gain,
        max_nonlinearity_pct_fs: nonlinearity,
        noise_rms: noise,
        checks,
        pass,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIRECT: PathModel = PathModel {
        intercept: 0.0,
        slope: 2.0,
    };

    /// Deterministic pseudo-noise, uniform in [-a, a].
    fn noise(i: usize, a: f64) -> f64 {
        let x = ((i as u64)
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407)
            >> 33) as f64
            / (1u64 << 31) as f64;
        (x * 2.0 - 1.0) * a
    }

    fn sample(
        wave: &Wave,
        model: PathModel,
        rate_hz: f64,
        seconds: f64,
        phase: f64,
        gain: f64,
        offset: f64,
        noise_a: f64,
    ) -> (Vec<f64>, Vec<f64>) {
        let f = wave.period_hz().unwrap_or(0.0);
        let n = (rate_hz * seconds) as usize;
        let t: Vec<f64> = (0..n).map(|i| 100.0 + i as f64 / rate_hz).collect();
        let y = t
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let ideal = model.reported(wave.pin_at(t * f + phase));
                ideal * gain + offset + noise(i, noise_a)
            })
            .collect();
        (t, y)
    }

    #[test]
    fn dc_offset_and_noise() {
        let wave = Wave::dc(1.0);
        let (t, y) = sample(&wave, DIRECT, 4.0, 30.0, 0.0, 1.0, 0.003, 0.002);
        let (expected, a) = analyse(&t, &y, &wave, DIRECT);
        assert!(expected.iter().all(|e| (*e - 2.0).abs() < 1e-12));
        assert_eq!(a.samples, 120);
        assert!((a.offset_error.unwrap() - 0.003).abs() < 5e-4);
        // Uniform noise of +-a has RMS a / sqrt(3).
        assert!((a.noise_rms.unwrap() - 0.002 / 3f64.sqrt()).abs() < 3e-4);
        assert_eq!(a.gain_error_pct, None);
        assert_eq!(a.measured_amplitude, None);
        assert_eq!(a.measured_frequency_hz, None);
    }

    #[test]
    fn sine_amplitude_frequency_gain_and_phase_alignment() {
        let wave = Wave {
            waveform: Waveform::Sine,
            offset_v: 1.0,
            amplitude_v: 0.5,
            frequency_hz: 0.25,
            steps: 0,
            dwell_ms: 0,
            ..Default::default()
        };
        // 40 Hz stream, true gain 0.99, offset +0.01, unknown phase.
        let (t, y) = sample(&wave, DIRECT, 40.0, 30.0, 0.37, 0.99, 0.01, 0.0005);
        let (expected, a) = analyse(&t, &y, &wave, DIRECT);
        assert!((a.expected_amplitude.unwrap() - 1.0).abs() < 1e-12);
        assert!(
            (a.measured_amplitude.unwrap() - 0.99).abs() < 0.003,
            "{a:?}"
        );
        assert!((a.gain_error_pct.unwrap() + 1.0).abs() < 0.3, "{a:?}");
        assert!(
            (a.measured_frequency_hz.unwrap() - 0.25).abs() < 0.005,
            "{a:?}"
        );
        assert_eq!(a.expected_frequency_hz, Some(0.25));
        // Expected is phase aligned: measured - expected is small everywhere.
        let worst = y
            .iter()
            .zip(&expected)
            .map(|(m, e)| (m - e).abs())
            .fold(0.0, f64::max);
        assert!(worst < 0.1, "worst residual {worst}");
        assert!(a.noise_rms.unwrap() < 0.002, "{a:?}");
    }

    #[test]
    fn square_and_triangle_peak_amplitude_from_the_fundamental() {
        for waveform in [Waveform::Square, Waveform::Triangle] {
            let wave = Wave {
                waveform,
                offset_v: 1.5,
                amplitude_v: 0.4,
                frequency_hz: 0.5,
                steps: 0,
                dwell_ms: 0,
                ..Default::default()
            };
            let (t, y) = sample(&wave, DIRECT, 40.0, 20.0, 0.1, 1.0, 0.0, 0.0);
            let (_, a) = analyse(&t, &y, &wave, DIRECT);
            assert!(
                (a.measured_amplitude.unwrap() - 0.8).abs() < 0.01,
                "{waveform:?} {a:?}"
            );
            assert!(
                (a.measured_frequency_hz.unwrap() - 0.5).abs() < 0.01,
                "{waveform:?} {a:?}"
            );
            assert!(a.offset_error.unwrap().abs() < 0.02, "{waveform:?} {a:?}");
        }
    }

    #[test]
    fn inverted_load_cell_path_keeps_alignment() {
        let model = PathModel {
            intercept: 0.218,
            slope: -1.009,
        };
        let wave = Wave {
            waveform: Waveform::Sine,
            offset_v: 0.15,
            amplitude_v: 0.05,
            frequency_hz: 1.0,
            steps: 0,
            dwell_ms: 0,
            ..Default::default()
        };
        let (t, y) = sample(&wave, model, 40.0, 10.0, 0.6, 1.0, 0.0, 0.0);
        let (expected, a) = analyse(&t, &y, &wave, model);
        assert!(a.offset_error.unwrap().abs() < 2e-3, "{a:?}");
        let worst = y
            .iter()
            .zip(&expected)
            .map(|(m, e)| (m - e).abs())
            .fold(0.0, f64::max);
        assert!(worst < 0.02, "{worst}");
    }

    #[test]
    fn staircase_alignment_gain_and_frequency() {
        let wave = Wave {
            waveform: Waveform::Staircase,
            offset_v: 1.0,
            amplitude_v: 0.5,
            frequency_hz: 0.0,
            steps: 5,
            dwell_ms: 1000,
            ..Default::default()
        };
        assert!((wave.period_hz().unwrap() - 0.2).abs() < 1e-12);
        let (t, y) = sample(&wave, DIRECT, 10.0, 30.0, 0.43, 1.02, 0.0, 0.0);
        let (_, a) = analyse(&t, &y, &wave, DIRECT);
        assert!((a.gain_error_pct.unwrap() - 2.0).abs() < 0.5, "{a:?}");
        assert!(
            (a.measured_frequency_hz.unwrap() - 0.2).abs() < 0.01,
            "{a:?}"
        );
    }

    #[test]
    fn frequency_needs_two_periods_and_zero_crossings_count_rises() {
        let t: Vec<f64> = (0..400).map(|i| i as f64 * 0.025).collect();
        let y: Vec<f64> = t.iter().map(|t| (TAU * 1.3 * t).sin()).collect();
        assert!((zero_crossing_frequency(&t, &y).unwrap() - 1.3).abs() < 0.01);
        assert_eq!(zero_crossing_frequency(&t, &vec![1.0; 400]), None);
        let wave = Wave {
            waveform: Waveform::Sine,
            offset_v: 1.0,
            amplitude_v: 0.5,
            frequency_hz: 0.05,
            steps: 0,
            dwell_ms: 0,
            ..Default::default()
        };
        let (t, y) = sample(&wave, DIRECT, 4.0, 30.0, 0.0, 1.0, 0.0, 0.0);
        assert_eq!(analyse(&t, &y, &wave, DIRECT).1.measured_frequency_hz, None);
    }

    fn sweep_points(gain: f64, offset: f64, bow: f64, std: f64) -> Vec<SweepPoint> {
        (0..11)
            .map(|i| {
                let pin = 0.2 + 0.1 * i as f64;
                let expected = 2.0 * pin;
                let x = (expected - 0.4) / 2.0 - 0.5; // -0.5..0.5 across the sweep
                SweepPoint {
                    dac_pin_v: pin,
                    expected,
                    measured_mean: expected * gain + offset + bow * (0.25 - x * x),
                    measured_std: std,
                    readback_v: Some(pin),
                    samples: 10,
                }
            })
            .collect()
    }

    #[test]
    fn sweep_fit_reports_offset_gain_linearity_and_noise() {
        let tol = Tolerances {
            offset: 0.01,
            gain_pct: 1.0,
            nonlinearity_pct_fs: 0.25,
            noise_rms: 0.002,
        };
        let r = evaluate_sweep(&sweep_points(1.004, 0.002, 0.0, 0.001), 2.4, &tol).unwrap();
        assert!((r.offset_error - 0.002).abs() < 1e-9);
        assert!((r.gain_error_pct - 0.4).abs() < 1e-6);
        assert!(r.max_nonlinearity_pct_fs < 1e-6);
        assert!((r.noise_rms - 0.001).abs() < 1e-12);
        assert!(r.pass && r.checks.iter().all(|c| c.pass));
        assert_eq!(r.checks.len(), 4);

        // Each limit fails on its own.
        let gain = evaluate_sweep(&sweep_points(1.02, 0.0, 0.0, 0.001), 2.4, &tol).unwrap();
        assert!(!gain.pass && failing(&gain) == ["gain"]);
        let offset = evaluate_sweep(&sweep_points(1.0, -0.02, 0.0, 0.001), 2.4, &tol).unwrap();
        assert!(!offset.pass && failing(&offset) == ["offset"]);
        let bowed = evaluate_sweep(&sweep_points(1.0, 0.0, 0.05, 0.001), 2.4, &tol).unwrap();
        assert!(bowed.max_nonlinearity_pct_fs > 0.25, "{bowed:?}");
        assert!(failing(&bowed).contains(&"nonlinearity"));
        let noisy = evaluate_sweep(&sweep_points(1.0, 0.0, 0.0, 0.01), 2.4, &tol).unwrap();
        assert!(!noisy.pass && failing(&noisy) == ["noise"]);
        // A single point cannot be fitted.
        assert!(evaluate_sweep(&sweep_points(1.0, 0.0, 0.0, 0.0)[..1], 2.4, &tol).is_none());
    }

    fn failing(r: &SweepResult) -> Vec<&'static str> {
        r.checks
            .iter()
            .filter(|c| !c.pass)
            .map(|c| c.name)
            .collect()
    }

    #[test]
    fn linear_fit_recovers_line_and_rejects_degenerate_input() {
        let (m, b) = linear_fit(&[0.0, 1.0, 2.0], &[1.0, 3.0, 5.0]).unwrap();
        assert!((m - 2.0).abs() < 1e-12 && (b - 1.0).abs() < 1e-12);
        assert!(linear_fit(&[1.0, 1.0], &[1.0, 2.0]).is_none());
        assert!(linear_fit(&[1.0], &[1.0]).is_none());
    }
}
