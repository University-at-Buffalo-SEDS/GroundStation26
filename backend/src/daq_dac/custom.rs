//! Operator-defined DAC shapes: an equation, a list of values or a WAV clip,
//! sampled on the GS into a **normalized** shape (-1..1, stored as Q15). The
//! DAQ applies the same amplitude, offset, phase and frequency controls as
//! for its built-in waves:
//! `output = offset + amplitude * shape(frac(x + phase))` at the selected
//! frequency. Changing amplitude, offset or phase never needs a new upload;
//! frequency only re-times the shape unless the shape itself depends on time
//! (an equation of `t`) or needs a different length. The board receives only
//! the sampled shape.
use super::audio;
use super::expr::{Expr, Vars};
use super::waveform::{self, ARB_LEN_MAX, ARB_LEN_MIN, CLOCK_HZ, SHAPE_MAX};
use base64::Engine;
use serde::{Deserialize, Serialize};

/// Relative frequency error accepted when reusing the length already on the
/// DAQ (avoids a re-upload); a fresh choice targets 10 ppm.
const REUSE_TOLERANCE: f64 = 1e-4;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CustomSource {
    /// `f(x)` over one period, x in [0, 1), returning -1..1. `t = x / f`
    /// (seconds) is also available.
    Expression {
        expression: String,
        /// Shape length; default: the longest even length within 10 ppm.
        #[serde(default)]
        samples: Option<u16>,
    },
    /// The normalized shape values themselves (-1..1).
    Samples { values: Vec<f64> },
    /// A WAV clip: PCM full scale is -1..1 (optionally normalized to its
    /// peak). `playback_rate_hz` picks the clip length (at most 512 samples);
    /// the request's frequency_hz, if given, re-times it afterwards.
    Wav {
        data_base64: String,
        #[serde(default)]
        start_s: f64,
        #[serde(default)]
        normalize: bool,
        #[serde(default)]
        playback_rate_hz: Option<f64>,
        #[serde(default)]
        samples: Option<u16>,
    },
}

impl CustomSource {
    /// The request as kept for status: bulky payloads dropped.
    pub fn slim(&self) -> Self {
        match self {
            Self::Samples { .. } => Self::Samples { values: Vec::new() },
            Self::Wav {
                start_s,
                normalize,
                playback_rate_hz,
                samples,
                ..
            } => Self::Wav {
                data_base64: String::new(),
                start_s: *start_s,
                normalize: *normalize,
                playback_rate_hz: *playback_rate_hz,
                samples: *samples,
            },
            other => other.clone(),
        }
    }
}

/// A sampled normalized shape.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CustomTable {
    #[serde(skip)]
    pub shape: Vec<i16>,
    pub samples: u16,
    pub shape_crc: u32,
    /// The repetition frequency to play it at (WAV: from the playback rate
    /// unless the request overrides it).
    pub frequency_hz: f64,
    /// The shape depends on the frequency (an equation of t, T, f or fs).
    pub depends_on_frequency: bool,
    /// Samples clipped to -1..1.
    pub clipped: usize,
    pub warnings: Vec<String>,
}

fn to_q15(y: f64) -> (i16, bool) {
    let clipped = !(-1.0 - 1e-12..=1.0 + 1e-12).contains(&y);
    let q = (y.clamp(-1.0, 1.0) * f64::from(SHAPE_MAX)).round() as i16;
    (q.max(-SHAPE_MAX), clipped)
}

fn check_len(n: usize) -> Result<u16, String> {
    let n16 = u16::try_from(n).map_err(|_| format!("A shape holds at most {ARB_LEN_MAX} samples."))?;
    if !(ARB_LEN_MIN..=ARB_LEN_MAX).contains(&n16) {
        return Err(format!(
            "A shape holds {ARB_LEN_MIN} to {ARB_LEN_MAX} samples, not {n}."
        ));
    }
    if n16 % 2 != 0 {
        return Err(format!("A shape needs an even number of samples, not {n}."));
    }
    Ok(n16)
}

fn check_frequency(frequency_hz: Option<f64>) -> Result<f64, String> {
    let f = frequency_hz.ok_or("A custom waveform needs frequency_hz (periods per second).")?;
    if !f.is_finite() || f <= 0.0 {
        return Err("Frequency must be a positive number of hertz.".into());
    }
    if !(waveform::FREQ_MIN_HZ..=waveform::FREQ_MAX_HZ).contains(&f) {
        return Err(format!(
            "A custom waveform repeats at {} Hz to {} Hz.",
            waveform::FREQ_MIN_HZ,
            waveform::FREQ_MAX_HZ
        ));
    }
    Ok(f)
}

fn check_rate(n: u16, f: f64) -> Result<(), String> {
    let max_f = waveform::UPDATE_MAX_HZ / f64::from(n);
    if f > max_f {
        return Err(format!(
            "{n} samples at {f} Hz need {:.0} S/s; the DAC updates at most {:.0} S/s, so use at most {max_f:.4} Hz or fewer samples.",
            f * f64::from(n),
            waveform::UPDATE_MAX_HZ
        ));
    }
    Ok(())
}

/// Can a shape of length n play at f within the reuse tolerance?
fn length_fits(n: u16, f: f64) -> bool {
    let rate = f * f64::from(n);
    rate <= waveform::UPDATE_MAX_HZ
        && waveform::split(CLOCK_HZ, rate)
            .is_some_and(|s| (s.update_hz / f64::from(n) - f).abs() <= f * REUSE_TOLERANCE)
}

/// The shape length for frequency f: `prefer` (the length already uploaded)
/// when it still fits, else the longest even length within 10 ppm.
pub fn choose_len(f: f64, prefer: Option<u16>) -> Result<u16, String> {
    if let Some(n) = prefer.filter(|n| check_len(usize::from(*n)).is_ok() && length_fits(*n, f)) {
        return Ok(n);
    }
    let top = ((waveform::UPDATE_MAX_HZ / f).floor().min(f64::from(ARB_LEN_MAX)) as u16) & !1;
    if top < ARB_LEN_MIN {
        return Err(format!(
            "{f} Hz needs fewer than {ARB_LEN_MIN} samples per period at the DAC's 200 kS/s limit."
        ));
    }
    waveform::choose(CLOCK_HZ, f, ARB_LEN_MIN, top)
        .map(|r| r.samples)
        .ok_or_else(|| "No shape length reaches this frequency.".into())
}

fn finish(
    shape: Vec<i16>,
    frequency_hz: f64,
    depends_on_frequency: bool,
    clipped: usize,
    mut warnings: Vec<String>,
) -> Result<CustomTable, String> {
    let n = check_len(shape.len())?;
    check_rate(n, frequency_hz)?;
    if clipped > 0 {
        warnings.push(format!(
            "{clipped} of {n} shape samples were outside the normalized -1..1 range and were clipped."
        ));
    }
    Ok(CustomTable {
        shape_crc: waveform::shape_crc(&shape),
        samples: n,
        shape,
        frequency_hz,
        depends_on_frequency,
        clipped,
        warnings,
    })
}

/// Build the shape. `frequency_hz` is the repetition rate (WAV: optional
/// override); `prefer_len` is the length already on the DAQ, reused when it
/// still fits so a frequency change does not force a re-upload.
pub fn build(
    source: &CustomSource,
    frequency_hz: Option<f64>,
    prefer_len: Option<u16>,
) -> Result<CustomTable, String> {
    match source {
        CustomSource::Expression {
            expression,
            samples,
        } => {
            let expr = Expr::parse(expression).map_err(|e| e.to_string())?;
            let f = check_frequency(frequency_hz)?;
            let n = match samples {
                Some(n) => check_len(usize::from(*n))?,
                None => choose_len(f, prefer_len)?,
            };
            check_rate(n, f)?;
            let len = f64::from(n);
            let period = 1.0 / f;
            let mut shape = Vec::with_capacity(usize::from(n));
            let mut clipped = 0;
            for i in 0..n {
                let x = f64::from(i) / len;
                let vars = Vars {
                    t: x * period,
                    n: f64::from(i),
                    len,
                    x,
                    period,
                    freq: f,
                    rate: len * f,
                };
                let y = expr.eval(&vars);
                if !y.is_finite() {
                    return Err(format!(
                        "The expression is not a finite number at sample {i} (x = {x:.6}).",
                    ));
                }
                let (q, c) = to_q15(y);
                clipped += usize::from(c);
                shape.push(q);
            }
            let mut warnings = Vec::new();
            if expr.is_constant() {
                warnings.push("The expression does not depend on x, t or n: the output is a constant level.".into());
            }
            finish(shape, f, expr.uses_time(), clipped, warnings)
        }
        CustomSource::Samples { values } => {
            check_len(values.len())?;
            let f = check_frequency(frequency_hz)?;
            let mut clipped = 0;
            let mut shape = Vec::with_capacity(values.len());
            for (i, y) in values.iter().enumerate() {
                if !y.is_finite() {
                    return Err(format!("Value {i} is not a finite number."));
                }
                let (q, c) = to_q15(*y);
                clipped += usize::from(c);
                shape.push(q);
            }
            finish(shape, f, false, clipped, Vec::new())
        }
        CustomSource::Wav {
            data_base64,
            start_s,
            normalize,
            playback_rate_hz,
            samples,
        } => {
            if data_base64.len() > audio::MAX_FILE_BYTES / 3 * 4 + 8 {
                return Err(format!(
                    "The WAV file is larger than {} MiB.",
                    audio::MAX_FILE_BYTES / (1024 * 1024)
                ));
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data_base64.trim())
                .map_err(|e| format!("The WAV data is not valid base64: {e}"))?;
            let clip = audio::parse_wav(&bytes)?;
            if !start_s.is_finite() || *start_s < 0.0 || *start_s >= clip.duration_s() {
                return Err(format!(
                    "Start {start_s} s is outside the {:.3} s file.",
                    clip.duration_s()
                ));
            }
            let rate = playback_rate_hz.unwrap_or_else(|| {
                f64::from(clip.sample_rate_hz).min(waveform::UPDATE_MAX_HZ)
            });
            if !rate.is_finite() || !(1.0..=waveform::UPDATE_MAX_HZ).contains(&rate) {
                return Err(format!(
                    "Playback rate must be 1 to {:.0} S/s.",
                    waveform::UPDATE_MAX_HZ
                ));
            }
            let available = ((clip.duration_s() - start_s) * rate).floor() as usize;
            let cap = samples.map_or(usize::from(ARB_LEN_MAX), usize::from).min(usize::from(ARB_LEN_MAX));
            let n = available.min(cap) & !1;
            check_len(n).map_err(|_| {
                format!(
                    "Only {available} samples of the clip remain at {rate:.0} S/s; a shape needs {ARB_LEN_MIN} or more."
                )
            })?;
            let mut x = audio::resample(&clip, *start_s, rate, n);
            let mut warnings = Vec::new();
            if *normalize {
                let peak = x.iter().fold(0.0f64, |m, v| m.max(v.abs()));
                if peak > 0.0 {
                    x.iter_mut().for_each(|v| *v /= peak);
                }
            }
            if available > n {
                warnings.push(format!(
                    "The shape holds {n} samples: {:.1} ms of the {:.3} s file from {start_s} s at {rate:.0} S/s. It loops.",
                    n as f64 / rate * 1000.0,
                    clip.duration_s()
                ));
            }
            if f64::from(clip.sample_rate_hz) > rate * 1.000_001 {
                warnings.push(format!(
                    "Resampled from {} to {rate:.0} S/s with an averaging filter; content above {:.0} Hz is lost.",
                    clip.sample_rate_hz,
                    rate / 2.0
                ));
            }
            let mut clipped = 0;
            let shape = x
                .iter()
                .map(|v| {
                    let (q, c) = to_q15(*v);
                    clipped += usize::from(c);
                    q
                })
                .collect::<Vec<_>>();
            let f = match frequency_hz {
                Some(f) => check_frequency(Some(f))?,
                None => rate / n as f64,
            };
            finish(shape, f, false, clipped, warnings)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::wire::{Command, Waveform};
    use super::*;

    fn expr(e: &str) -> CustomSource {
        CustomSource::Expression {
            expression: e.into(),
            samples: None,
        }
    }
    fn play(t: &CustomTable, amp: f32, off: f32, f: f32) -> waveform::Plan {
        let c = Command {
            waveform: Waveform::Arbitrary,
            amplitude_v: amp,
            offset_v: off,
            frequency_hz: f,
            table_len: t.samples,
            table_crc: t.shape_crc,
            ..Command::default()
        };
        waveform::plan_shape(&c, &t.shape).unwrap()
    }

    #[test]
    fn equation_is_a_normalized_shape_over_one_period() {
        let t = build(&expr("0.8*sin(2*pi*x) + 0.2*sign(sin(2*pi*10*x))"), Some(5.0), None).unwrap();
        // 5 Hz: the longest even length within 10 ppm is the 512 maximum.
        assert_eq!(t.samples, 512);
        assert_eq!(t.clipped, 0);
        assert!(!t.depends_on_frequency);
        assert_eq!(t.shape[0], 0);
        // A quarter period: 0.8 + 0.2 = 1.0.
        assert_eq!(t.shape[128], 32767);
        assert_eq!(t.shape_crc, waveform::shape_crc(&t.shape));
    }

    /// Rylan's requirement: the same equation at two frequencies and
    /// amplitudes gives correctly scaled and timed output from one shape.
    #[test]
    fn one_shape_at_two_frequencies_and_amplitudes() {
        let src = expr("sin(2*pi*x)");
        let a = build(&src, Some(10.0), None).unwrap();
        // A frequency change reuses the uploaded length: identical shape and CRC, no upload.
        let b = build(&src, Some(250.0), Some(a.samples)).unwrap();
        assert_eq!((a.samples, a.shape_crc), (b.samples, b.shape_crc));
        let slow = play(&a, 1.0, 1.6, 10.0);
        let fast = play(&b, 0.25, 2.0, 250.0);
        assert_eq!((slow.frequency_hz, fast.frequency_hz), (10.0, 250.0));
        assert_eq!(slow.update_hz, 10.0 * 512.0);
        assert_eq!(fast.update_hz, 250.0 * 512.0);
        let pin = |c: u16| waveform::code_to_volts(c);
        // Peaks at a quarter period: 1.6 + 1.0 and 2.0 + 0.25 (one code of tolerance).
        assert!((pin(slow.table[128]) - 2.6).abs() < 1e-3);
        assert!((pin(slow.table[384]) - 0.6).abs() < 1e-3);
        assert!((pin(fast.table[128]) - 2.25).abs() < 1e-3);
        assert!((pin(fast.table[384]) - 1.75).abs() < 1e-3);
        // Every sample is offset + amplitude x shape.
        for i in 0..512 {
            let s = f64::from(a.shape[i]) / 32767.0;
            assert_eq!(fast.table[i], waveform::volts_to_code(f64::from(2.0f32) + 0.25 * s));
        }
        // Beyond 200 kS/s / 512 the length must change: a new shape is sampled.
        let c = build(&src, Some(1000.0), Some(a.samples)).unwrap();
        assert!(c.samples <= 200 && c.shape_crc != a.shape_crc);
        // An equation of t changes with the frequency, so it is re-sampled.
        let t1 = build(&expr("sin(2*pi*10*t)"), Some(10.0), None).unwrap();
        let t2 = build(&expr("sin(2*pi*10*t)"), Some(5.0), Some(t1.samples)).unwrap();
        assert!(t1.depends_on_frequency && t1.shape_crc != t2.shape_crc);
        // At 10 Hz, t = x / 10 gives the same shape as sin(2 pi x).
        assert_eq!(t1.shape, a.shape);
    }

    #[test]
    fn clipping_lengths_and_rates() {
        let t = build(&expr("1.5*x"), Some(10.0), None).unwrap();
        // 1.5x passes 1 at x = 2/3.
        assert_eq!(t.clipped, 512 - 342);
        assert!(t.warnings.iter().any(|w| w.contains("normalized -1..1")));
        assert_eq!(*t.shape.last().unwrap(), 32767);
        let neg = build(&expr("-2"), Some(10.0), None).unwrap();
        assert!(neg.shape.iter().all(|s| *s == -32767));
        for n in [63u16, 8, 1024] {
            let s = CustomSource::Expression { expression: "x".into(), samples: Some(n) };
            assert!(build(&s, Some(1.0), None).is_err(), "{n}");
        }
        let s = CustomSource::Expression { expression: "x".into(), samples: Some(512) };
        assert!(build(&s, Some(1000.0), None).unwrap_err().contains("at most 390.6250 Hz"));
        let t = build(&expr("x"), Some(10_000.0), None).unwrap();
        assert!(t.samples <= 20);
        assert!(build(&expr("x"), None, None).is_err());
        assert!(build(&expr("x"), Some(20_000.0), None).is_err());
        assert!(build(&expr("1/(x-0.5)"), Some(1.0), None).unwrap_err().contains("sample 256"));
        assert!(build(&expr("sin(("), Some(1.0), None).is_err());
        assert!(build(&expr("0.5"), Some(1.0), None).unwrap().warnings.iter().any(|w| w.contains("constant")));
        let samples = CustomSource::Samples { values: (0..32).map(|i| -1.0 + 2.0 * f64::from(i) / 31.0).collect() };
        let t = build(&samples, Some(2.0), None).unwrap();
        assert_eq!((t.shape[0], t.shape[31]), (-32767, 32767));
        assert!(build(&CustomSource::Samples { values: vec![f64::NAN; 32] }, Some(2.0), None).is_err());
        // A stale preferred length that no longer fits is replaced.
        assert_eq!(choose_len(1000.0, Some(512)).unwrap(), 200);
        assert_eq!(choose_len(10.0, Some(300)).unwrap(), 300);
    }

    #[test]
    fn wav_is_a_normalized_shape_too() {
        // 0.5 s of a 100 Hz sine at 8 kS/s, amplitude 0.5 of full scale.
        let frames: Vec<Vec<f64>> = (0..4000)
            .map(|i| vec![0.5 * (2.0 * std::f64::consts::PI * 100.0 * f64::from(i) / 8000.0).sin()])
            .collect();
        let wav = audio::wav_bytes(8000, 1, 16, false, &frames);
        let data = base64::engine::general_purpose::STANDARD.encode(&wav);
        let src = CustomSource::Wav { data_base64: data.clone(), start_s: 0.0, normalize: false, playback_rate_hz: None, samples: None };
        let t = build(&src, None, None).unwrap();
        // 8 kS/s playback: 512 samples = 64 ms of the 0.5 s file, looping at 15.625 Hz.
        assert_eq!(t.samples, 512);
        assert_eq!(t.frequency_hz, 15.625);
        assert!(t.warnings.iter().any(|w| w.contains("64.0 ms")));
        assert_eq!(t.clipped, 0);
        let peak = t.shape.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!((f64::from(peak) / 32767.0 - 0.5).abs() < 0.01);
        // Normalized: the peak fills the shape; amplitude then scales it on the DAQ.
        let norm = CustomSource::Wav { data_base64: data.clone(), start_s: 0.0, normalize: true, playback_rate_hz: Some(2000.0), samples: Some(200) };
        let n = build(&norm, None, None).unwrap();
        assert_eq!((n.samples, n.frequency_hz), (200, 10.0));
        assert_eq!(n.shape.iter().map(|s| s.unsigned_abs()).max().unwrap(), 32767);
        assert!(n.warnings.iter().any(|w| w.contains("averaging filter")));
        // Re-timing the same clip keeps the shape (no re-upload).
        let faster = build(&norm, Some(40.0), None).unwrap();
        assert_eq!((faster.shape_crc, faster.frequency_hz), (n.shape_crc, 40.0));
        let p = play(&faster, 1.0, 1.6, 40.0);
        assert_eq!(p.update_hz, 8000.0);
        // Errors.
        let late = CustomSource::Wav { data_base64: data.clone(), start_s: 0.499, normalize: false, playback_rate_hz: None, samples: None };
        assert!(build(&late, None, None).unwrap_err().contains("Only"));
        let fast = CustomSource::Wav { data_base64: data.clone(), start_s: 0.0, normalize: false, playback_rate_hz: Some(1e6), samples: None };
        assert!(build(&fast, None, None).is_err());
        let junk = CustomSource::Wav { data_base64: "!!!".into(), start_s: 0.0, normalize: false, playback_rate_hz: None, samples: None };
        assert!(build(&junk, None, None).unwrap_err().contains("base64"));
        assert_eq!(norm.slim(), CustomSource::Wav { data_base64: String::new(), start_s: 0.0, normalize: true, playback_rate_hz: Some(2000.0), samples: Some(200) });
    }
}
