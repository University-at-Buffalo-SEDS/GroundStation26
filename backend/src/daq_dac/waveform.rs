//! The DAQ DAC waveform specification (DAQ-Board docs/dac-waveforms.md), bit
//! for bit: table generation, volts-to-code, CRC-32 and timer/table-length
//! selection. The GS previews exactly what the DAQ will play, reports the
//! achieved frequency before sending, and checks the table CRC the DAQ
//! reports. Keep every expression in the order the spec writes it: the
//! tables are only identical because both sides round the same operations.
use super::wire::{ChannelStatus, Command, FLAG_RAMP_DOWN, Waveform};
use serde::Serialize;

pub const CLOCK_HZ: u32 = 160_000_000;
pub const UPDATE_MAX_HZ: f64 = 200_000.0;
pub const UPDATE_MIN_HZ: f64 = 0.05;
pub const PERIODIC_LEN_MIN: u16 = 16;
pub const PERIODIC_LEN_MAX: u16 = 256;
pub const NOISE_LEN: u16 = 256;
pub const NOISE_SEED: u32 = 0x2545_F491;
pub const ARB_LEN_MIN: u16 = 16;
pub const ARB_LEN_MAX: u16 = 512;
pub const SHAPE_MAX: i16 = 32767;
pub const FREQ_MIN_HZ: f64 = 0.01;
pub const FREQ_MAX_HZ: f64 = 10_000.0;
pub const NOISE_RATE_MIN_HZ: f64 = 1.0;
pub const SWEEP_MIN_MS: u32 = 100;
pub const SWEEP_MAX_MS: u32 = 600_000;
pub const STEPS_MIN: u16 = 2;
pub const STEPS_MAX: u16 = 256;
pub const DWELL_MIN_MS: u32 = 10;
pub const DWELL_MAX_MS: u32 = 20_000;
pub const DUTY_MIN_Q16: u16 = 656;
pub const DUTY_MAX_Q16: u16 = 64_880;
/// The linear range in codes (0.20 and 3.10 V).
#[cfg(test)]
pub const CODE_LO: u16 = 248;
#[cfg(test)]
pub const CODE_HI: u16 = 3847;
pub const FREQ_TOLERANCE: f64 = 1e-5;
const SPLIT_TRIES: f64 = 64.0;
const CHOOSE_SPLIT_TRIES: f64 = 3.0;
const VREF_V: f64 = 3.3;

const HALF_PI: f64 = 1.5707963267948966;
const C3: f64 = -1.0 / 6.0;
const C5: f64 = 1.0 / 120.0;
const C7: f64 = -1.0 / 5040.0;
const C9: f64 = 1.0 / 362880.0;
const C11: f64 = -1.0 / 39916800.0;
const C13: f64 = 1.0 / 6227020800.0;
const C15: f64 = -1.0 / 1307674368000.0;

/// sin(2 pi x) from basic double arithmetic only (spec "Determinism").
pub fn sin_turns(x: f64) -> f64 {
    let x = x - x.floor();
    let y = 4.0 * x;
    let q = y.floor();
    let mut r = y - q;
    let quadrant = (q as i64) & 3;
    if quadrant & 1 == 1 {
        r = 1.0 - r;
    }
    let t = r * HALF_PI;
    let t2 = t * t;
    let mut s = C15;
    s = C13 + t2 * s;
    s = C11 + t2 * s;
    s = C9 + t2 * s;
    s = C7 + t2 * s;
    s = C5 + t2 * s;
    s = C3 + t2 * s;
    s = 1.0 + t2 * s;
    s *= t;
    if quadrant >= 2 { -s } else { s }
}

pub fn unit(waveform: Waveform, flags: u8, duty_q16: u16, x: f64) -> f64 {
    match waveform {
        Waveform::Sine | Waveform::Chirp => sin_turns(x),
        Waveform::Square => {
            if x < f64::from(duty_q16) / 65536.0 {
                1.0
            } else {
                -1.0
            }
        }
        Waveform::Triangle => {
            if x < 0.5 {
                -1.0 + 4.0 * x
            } else {
                3.0 - 4.0 * x
            }
        }
        Waveform::Sawtooth => {
            if flags & FLAG_RAMP_DOWN != 0 {
                1.0 - 2.0 * x
            } else {
                -1.0 + 2.0 * x
            }
        }
        _ => 0.0,
    }
}

pub fn volts_to_code(volts: f64) -> u16 {
    if !(volts > 0.0) {
        return 0;
    }
    let code = (volts * 4095.0 / VREF_V + 0.5).floor();
    if code >= 4095.0 { 4095 } else { code as u16 }
}

pub fn code_to_volts(code: u16) -> f64 {
    f64::from(code.min(4095)) * VREF_V / 4095.0
}

/// Table-generation inputs, exactly as the DAQ applied them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Params {
    pub waveform: Waveform,
    pub flags: u8,
    pub steps: u16,
    pub phase_q16: u16,
    pub duty_q16: u16,
    pub amplitude_v: f32,
    pub offset_v: f32,
}
impl Params {
    pub fn from_command(c: &Command) -> Self {
        Self {
            waveform: c.waveform,
            flags: c.flags,
            steps: c.steps,
            phase_q16: c.phase_q16,
            duty_q16: c.duty_q16,
            amplitude_v: c.amplitude_v,
            offset_v: c.offset_v,
        }
    }
    pub fn from_status(c: &ChannelStatus) -> Option<Self> {
        Some(Self {
            waveform: Waveform::from_u8(c.waveform)?,
            flags: c.flags,
            steps: c.steps,
            phase_q16: c.phase_q16,
            duty_q16: c.duty_q16,
            amplitude_v: c.amplitude_v,
            offset_v: c.offset_v,
        })
    }
}

/// The n codes of a built-in waveform; None for arbitrary or a bad length.
pub fn fill(p: &Params, n: u16) -> Option<Vec<u16>> {
    if n == 0 {
        return None;
    }
    let a = f64::from(p.amplitude_v);
    let o = f64::from(p.offset_v);
    let n_f = f64::from(n);
    Some(match p.waveform {
        Waveform::Dc => vec![volts_to_code(o); usize::from(n)],
        Waveform::Staircase => {
            if n < 2 {
                return None;
            }
            (0..n)
                .map(|i| volts_to_code(o - a + 2.0 * a * f64::from(i) / f64::from(n - 1)))
                .collect()
        }
        Waveform::Noise => {
            let mut s = NOISE_SEED;
            (0..n)
                .map(|_| {
                    s ^= s << 13;
                    s ^= s >> 17;
                    s ^= s << 5;
                    let u = f64::from(s >> 8) / 8388608.0 - 1.0;
                    volts_to_code(o + a * u)
                })
                .collect()
        }
        Waveform::Arbitrary => return None,
        w => {
            let phase = f64::from(p.phase_q16) / 65536.0;
            (0..n)
                .map(|i| {
                    let mut x = f64::from(i) / n_f + phase;
                    x -= x.floor();
                    volts_to_code(o + a * unit(w, p.flags, p.duty_q16, x))
                })
                .collect()
        }
    })
}

/// Codes for an uploaded normalized shape (Q15), scaled like a built-in wave:
/// `o + a * (s[j] / 32767)`, `j = (i + rot) mod n`,
/// `rot = ((phase_q16 * n + 32768) >> 16) mod n`.
pub fn fill_shape(p: &Params, shape: &[i16]) -> Option<Vec<u16>> {
    let n = u32::try_from(shape.len()).ok().filter(|n| *n > 0)?;
    let a = f64::from(p.amplitude_v);
    let o = f64::from(p.offset_v);
    let rot = ((u32::from(p.phase_q16) * n + 32768) >> 16) % n;
    Some(
        (0..n)
            .map(|i| {
                let u = f64::from(shape[((i + rot) % n) as usize]) / 32767.0;
                volts_to_code(o + a * u)
            })
            .collect(),
    )
}

/// The CRC that identifies an uploaded shape (its int16 samples, LE).
pub fn shape_crc(shape: &[i16]) -> u32 {
    crc32(&shape.iter().map(|s| *s as u16).collect::<Vec<_>>())
}

/// CRC-32/ISO-HDLC of the codes as little-endian u16 bytes.
pub fn crc32(codes: &[u16]) -> u32 {
    let mut crc = !0u32;
    for code in codes {
        for byte in code.to_le_bytes() {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xEDB8_8320 & 0u32.wrapping_sub(crc & 1));
            }
        }
    }
    !crc
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Split {
    pub psc: u16,
    pub arr: u16,
    pub update_hz: f64,
}

fn split_tries(clock_hz: u32, rate_hz: f64, tries: f64) -> Option<Split> {
    if clock_hz == 0 || !(UPDATE_MIN_HZ..=UPDATE_MAX_HZ).contains(&rate_hz) {
        return None;
    }
    let clock = f64::from(clock_hz);
    let total = clock / rate_hz;
    if !(total >= 2.0) {
        return None;
    }
    let first = ((total - 1.0) / 65536.0).floor();
    let mut best: Option<(Split, f64)> = None;
    let mut p = first;
    while p < first + tries && p <= 65535.0 {
        let div = p + 1.0;
        let a = (total / div + 0.5).floor().clamp(2.0, 65536.0);
        let achieved = clock / (div * a);
        let err = (achieved - rate_hz).abs();
        if best.is_none_or(|(_, e)| err < e) {
            best = Some((
                Split {
                    psc: p as u16,
                    arr: (a - 1.0) as u16,
                    update_hz: achieved,
                },
                err,
            ));
        }
        p += 1.0;
    }
    best.map(|(s, _)| s)
}

/// A lone timer split (staircase, noise, chirp, uploaded table).
pub fn split(clock_hz: u32, rate_hz: f64) -> Option<Split> {
    split_tries(clock_hz, rate_hz, SPLIT_TRIES)
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Rate {
    pub samples: u16,
    pub psc: u16,
    pub arr: u16,
    pub update_hz: f64,
    pub frequency_hz: f64,
}

/// Longest even table length within 10 ppm, else least error.
pub fn choose(clock_hz: u32, frequency_hz: f64, n_min: u16, n_max: u16) -> Option<Rate> {
    if !(frequency_hz > 0.0) || n_min < 2 || n_max < n_min {
        return None;
    }
    let mut best: Option<(Rate, f64)> = None;
    let mut n = n_max & !1;
    while n >= n_min && n >= 2 {
        let rate = frequency_hz * f64::from(n);
        if (UPDATE_MIN_HZ..=UPDATE_MAX_HZ).contains(&rate)
            && let Some(s) = split_tries(clock_hz, rate, CHOOSE_SPLIT_TRIES)
        {
            let f = s.update_hz / f64::from(n);
            let err = (f - frequency_hz).abs();
            if best.is_none_or(|(_, e)| err < e) {
                best = Some((
                    Rate {
                        samples: n,
                        psc: s.psc,
                        arr: s.arr,
                        update_hz: s.update_hz,
                        frequency_hz: f,
                    },
                    err,
                ));
            }
            if err <= frequency_hz * FREQ_TOLERANCE {
                break;
            }
        }
        n -= 2;
    }
    best.map(|(r, _)| r)
}

pub fn chirp_len(start_hz: f64, stop_hz: f64) -> u16 {
    let top = start_hz.max(stop_hz);
    if !(top > 0.0) {
        return PERIODIC_LEN_MIN;
    }
    let fit = (UPDATE_MAX_HZ / top).floor();
    let n = if fit >= f64::from(PERIODIC_LEN_MAX) {
        u32::from(PERIODIC_LEN_MAX)
    } else {
        fit as u32
    } & !1;
    n.max(u32::from(PERIODIC_LEN_MIN)) as u16
}

/// Instantaneous chirp frequency `elapsed_ms` into a repeating sweep (the
/// DAQ retunes to this every 50 ms; status reports the live value).
#[cfg(test)]
pub fn chirp_hz(start_hz: f64, stop_hz: f64, sweep_ms: u32, log: bool, elapsed_ms: u64) -> f64 {
    if sweep_ms == 0 {
        return start_hz;
    }
    let frac = (elapsed_ms % u64::from(sweep_ms)) as f64 / f64::from(sweep_ms);
    if log && start_hz > 0.0 && stop_hz > 0.0 {
        start_hz * (stop_hz / start_hz).powf(frac)
    } else {
        start_hz + (stop_hz - start_hz) * frac
    }
}

/// What the DAQ will play for a command: the table, its CRC and the timing.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Plan {
    pub samples: u16,
    pub psc: u16,
    pub arr: u16,
    pub update_hz: f64,
    /// Period rate; noise: sample rate; 0 for DC and staircase.
    pub frequency_hz: f64,
    /// Staircase only.
    pub dwell_ms: u32,
    pub table_crc: u32,
    #[serde(skip)]
    pub table: Vec<u16>,
}

/// The DAQ's `daq_dac_plan` for an already-clamped command. An arbitrary
/// command carries no table here (pass the uploaded codes to `plan_table`).
pub fn plan(command: &Command) -> Option<Plan> {
    let w = command.waveform;
    let params = Params::from_command(command);
    let f = f64::from(command.frequency_hz);
    let (n, timing) = match w {
        Waveform::Dc => (1, None),
        Waveform::Staircase => {
            if !(STEPS_MIN..=STEPS_MAX).contains(&command.steps) || command.dwell_ms == 0 {
                return None;
            }
            let s = split(CLOCK_HZ, 1000.0 / f64::from(command.dwell_ms))?;
            (command.steps, Some(s))
        }
        Waveform::Noise => (NOISE_LEN, Some(split(CLOCK_HZ, f)?)),
        Waveform::Chirp => {
            let n = chirp_len(f, f64::from(command.frequency_stop_hz));
            (n, Some(split(CLOCK_HZ, f * f64::from(n))?))
        }
        Waveform::Arbitrary => return None,
        _ => {
            let r = choose(CLOCK_HZ, f, PERIODIC_LEN_MIN, PERIODIC_LEN_MAX)?;
            (
                r.samples,
                Some(Split {
                    psc: r.psc,
                    arr: r.arr,
                    update_hz: r.update_hz,
                }),
            )
        }
    };
    let table = fill(&params, n)?;
    Some(finish(w, n, timing, crc32(&table), table))
}

/// What the DAQ plays for an (already clamped) arbitrary command whose
/// uploaded shape is `shape`.
pub fn plan_shape(command: &Command, shape: &[i16]) -> Option<Plan> {
    let n = u16::try_from(shape.len()).ok()?;
    if !(ARB_LEN_MIN..=ARB_LEN_MAX).contains(&n) || n % 2 != 0 {
        return None;
    }
    let s = split(CLOCK_HZ, f64::from(command.frequency_hz) * f64::from(n))?;
    let table = fill_shape(&Params::from_command(command), shape)?;
    Some(finish(Waveform::Arbitrary, n, Some(s), crc32(&table), table))
}

fn finish(w: Waveform, n: u16, timing: Option<Split>, crc: u32, table: Vec<u16>) -> Plan {
    let mut plan = Plan {
        samples: n,
        psc: 0,
        arr: 0,
        update_hz: 0.0,
        frequency_hz: 0.0,
        dwell_ms: 0,
        table_crc: crc,
        table,
    };
    if let Some(s) = timing {
        plan.psc = s.psc;
        plan.arr = s.arr;
        // The DAQ reports these as f32.
        plan.update_hz = f64::from(s.update_hz as f32);
        match w {
            Waveform::Staircase => plan.dwell_ms = (1000.0 / s.update_hz + 0.5) as u32,
            Waveform::Noise => plan.frequency_hz = f64::from(s.update_hz as f32),
            _ => plan.frequency_hz = f64::from((s.update_hz / f64::from(n)) as f32),
        }
    }
    plan
}

/// The CRC a channel's status should report for its applied parameters, or
/// None when it cannot be predicted (arbitrary: compare with the upload).
pub fn expected_status_crc(ch: &ChannelStatus) -> Option<u32> {
    let p = Params::from_status(ch)?;
    if p.waveform == Waveform::Arbitrary || ch.samples == 0 {
        return None;
    }
    fill(&p, ch.samples).map(|t| crc32(&t))
}

/// The DAQ's float clamp of amplitude/offset, so a command the GS sends is
/// already exactly what the DAQ applies (no 1-ulp re-clamp on the DAQ).
pub fn clamp_pin(offset_v: f32, amplitude_v: f32, has_amplitude: bool) -> (f32, f32) {
    let lo = 0.20f64 as f32;
    let hi = 3.10f64 as f32;
    let o = offset_v.clamp(lo, hi);
    if !has_amplitude {
        return (o, 0.0);
    }
    let headroom = (o - lo).min(hi - o);
    (o, amplitude_v.clamp(0.0, headroom))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(w: Waveform, amp: f32, off: f32, f: f32) -> Command {
        Command {
            waveform: w,
            amplitude_v: amp,
            offset_v: off,
            frequency_hz: f,
            duty_q16: 32768,
            enable: true,
            lease_ms: 3000,
            sequence: 1,
            ..Command::default()
        }
    }

    /// DAQ-Board tests/test_dac_waveforms.py GOLDEN, computed by the firmware C.
    #[test]
    fn golden_vectors_match_the_firmware() {
        #[rustfmt::skip]
        let golden: [(&str, Waveform, u8, f32, f32, f32, u16, u16, u16, u32, f32, u32, u16, u16, u16, u32); 10] = [
            ("sine10", Waveform::Sine, 0, 1.0, 1.6, 10.0, 0, 32768, 0, 0, 0.0, 0, 256, 0, 62499, 0x81CD_D413),
            ("sine123p90", Waveform::Sine, 0, 1.0, 1.6, 123.4, 16384, 32768, 0, 0, 0.0, 0, 236, 0, 5493, 0xAF1D_0167),
            ("square50d25", Waveform::Square, 0, 1.0, 1.6, 50.0, 0, 16384, 0, 0, 0.0, 0, 256, 0, 12499, 0x4C82_9B77),
            ("tri1", Waveform::Triangle, 0, 1.0, 1.6, 1.0, 0, 32768, 0, 0, 0.0, 0, 256, 9, 62499, 0x8BF0_BFDD),
            ("saw7777", Waveform::Sawtooth, 0, 1.0, 1.6, 7777.0, 0, 32768, 0, 0, 0.0, 0, 18, 0, 1142, 0xF74D_8A08),
            ("sawdown2p5", Waveform::Sawtooth, 1, 1.0, 1.6, 2.5, 0, 32768, 0, 0, 0.0, 0, 256, 3, 62499, 0xA444_C715),
            ("stair11", Waveform::Staircase, 0, 1.0, 1.6, 0.0, 0, 32768, 11, 500, 0.0, 0, 11, 1249, 63999, 0x4367_5E77),
            ("noise1k", Waveform::Noise, 0, 0.5, 1.6, 1000.0, 0, 32768, 0, 0, 0.0, 0, 256, 3, 39999, 0x806C_5080),
            ("chirp10to1k", Waveform::Chirp, 0, 1.0, 1.6, 10.0, 0, 32768, 0, 0, 1000.0, 10000, 200, 1, 39999, 0x70F8_CF62),
            ("dc1p25", Waveform::Dc, 0, 0.0, 1.25, 0.0, 0, 32768, 0, 0, 0.0, 0, 1, 0, 0, 0x2F22_AB05),
        ];
        for (name, w, flags, amp, off, f, phase, duty, steps, dwell, fstop, sweep, n, psc, arr, crc) in golden {
            let mut c = command(w, amp, off, f);
            c.flags = flags;
            c.phase_q16 = phase;
            c.duty_q16 = duty;
            c.steps = steps;
            c.dwell_ms = dwell;
            c.frequency_stop_hz = fstop;
            c.sweep_ms = sweep;
            let p = plan(&c).unwrap();
            assert_eq!((p.samples, p.psc, p.arr, p.table_crc), (n, psc, arr, crc), "{name}");
        }
    }

    /// DAQ-Board test_golden_shape_vector.
    #[test]
    fn golden_shape_vector_matches_the_firmware() {
        let shape: Vec<i16> = (0..64)
            .map(|i| {
                let x = f64::from(i) / 64.0;
                let y = if x < 0.5 { -1.0 + 4.0 * x } else { 3.0 - 4.0 * x };
                (32767.0 * y + 0.5).floor() as i16
            })
            .collect();
        assert_eq!(shape_crc(&shape), 0x7911_9F9E);
        let mut c = command(Waveform::Arbitrary, 1.0, 1.6, 100.0);
        c.phase_q16 = 16384;
        let p = plan_shape(&c, &shape).unwrap();
        assert_eq!((p.samples, p.psc, p.arr, p.table_crc), (64, 0, 24999, 0xA7B9_3781));
        // Same shape at another frequency and amplitude: retimed and rescaled.
        let mut c2 = c;
        c2.frequency_hz = 1000.0;
        c2.amplitude_v = 0.5;
        c2.offset_v = 2.0;
        c2.phase_q16 = 0;
        let q = plan_shape(&c2, &shape).unwrap();
        assert_eq!((q.frequency_hz, q.update_hz), (1000.0, 64_000.0));
        for (i, code) in q.table.iter().enumerate() {
            assert_eq!(*code, volts_to_code(2.0 + 0.5 * (f64::from(shape[i]) / 32767.0)));
        }
        // Quarter-turn phase rotates by n/4.
        c2.phase_q16 = 16384;
        let r = plan_shape(&c2, &shape).unwrap();
        assert!((0..64).all(|i| r.table[i] == q.table[(i + 16) % 64]));
    }

    #[test]
    fn sine_and_codes() {
        for i in -2000..=2000 {
            let x = f64::from(i) / 791.9;
            assert!((sin_turns(x) - (std::f64::consts::TAU * x).sin()).abs() < 1e-11);
        }
        assert_eq!(volts_to_code(f64::from(0.2f64 as f32)), 248);
        assert_eq!(volts_to_code(0.2), CODE_LO);
        assert_eq!(volts_to_code(3.1), CODE_HI);
        assert_eq!((volts_to_code(-1.0), volts_to_code(f64::NAN), volts_to_code(9.0)), (0, 0, 4095));
        assert_eq!(volts_to_code(1.65), 2048);
        assert_eq!(volts_to_code(f64::from(1.65f32)), 2047);
        // The firmware's CRC check value.
        let bytes: Vec<u16> = vec![u16::from_le_bytes([b'1', b'2']), u16::from_le_bytes([b'3', b'4'])];
        assert_eq!(crc32(&bytes), 0x9BE3_E0A3);
    }

    #[test]
    fn rates_stay_within_limits_and_report_achieved_frequency() {
        let mut f = 0.01;
        let mut worst: f64 = 0.0;
        while f <= 10_000.0 {
            let r = choose(CLOCK_HZ, f, 16, 256).unwrap();
            assert!(r.samples % 2 == 0 && (16..=256).contains(&r.samples));
            assert!(r.update_hz <= UPDATE_MAX_HZ);
            let e = (r.frequency_hz - f).abs() / f;
            if f <= 500.0 {
                assert!(e <= 1e-5, "{f}");
            }
            worst = worst.max(e);
            f *= 1.0137;
        }
        assert!(worst < 5e-4);
        assert!(choose(CLOCK_HZ, 20_000.0, 16, 256).is_none());
        assert_eq!(chirp_len(10.0, 1000.0), 200);
        assert_eq!(chirp_len(10_000.0, 1.0), 20);
        assert!((chirp_hz(10.0, 1000.0, 10_000, true, 5000) - 100.0).abs() < 1e-9);
        assert_eq!(chirp_hz(100.0, 1100.0, 10_000, false, 15_000), 600.0);
        let mut c = command(Waveform::Arbitrary, 1.0, 1.6, 390.625);
        let p = plan_shape(&c, &[0; 512]).unwrap();
        assert_eq!((p.update_hz, p.frequency_hz), (200_000.0, 390.625));
        c.frequency_hz = 391.0;
        assert!(plan_shape(&c, &[0; 512]).is_none());
        assert!(plan_shape(&c, &[0; 15]).is_none());
        assert!(plan_shape(&c, &[0; 514]).is_none());
    }

    #[test]
    fn status_crc_prediction() {
        let c = command(Waveform::Square, 1.0, 1.6, 50.0);
        let p = plan(&c).unwrap();
        let ch = ChannelStatus {
            active: true,
            waveform: Waveform::Square as u8,
            amplitude_v: 1.0,
            offset_v: 1.6,
            duty_q16: 32768,
            samples: p.samples,
            table_crc: p.table_crc,
            ..ChannelStatus::default()
        };
        assert_eq!(expected_status_crc(&ch), Some(p.table_crc));
        let off_by_ulp = ChannelStatus {
            amplitude_v: f32::from_bits(1.0f32.to_bits() - 1),
            ..ch
        };
        // A different applied amplitude still predicts a self-consistent CRC.
        assert!(expected_status_crc(&off_by_ulp).is_some());
        assert_eq!(clamp_pin(3.0, 1.0, true), (3.0, (3.10f64 as f32) - 3.0));
        assert_eq!(clamp_pin(9.0, 1.0, false).1, 0.0);
    }
}
