//! Realistic SD files for the transfer tests: a port of the FC launch-file
//! shape of `tools/gen_sd.py` (firmware float text, field order, record mix)
//! with synthetic signals, so a test can make a few hundred KB in memory.

/// `sd_format_float`: binary64 normalisation of a binary32, 9 digits.
pub(crate) fn fmt_float(value: f64) -> String {
    let value = value as f32;
    let bits = value.to_bits();
    let sign = if bits >> 31 != 0 { "-" } else { "" };
    let mag = bits & 0x7fff_ffff;
    if mag >= 0x7f80_0000 {
        return format!("{sign}{}", if mag == 0x7f80_0000 { "inf" } else { "nan" });
    }
    if mag == 0 {
        return format!("{sign}0");
    }
    let mut n = f64::from(value).abs();
    let mut e = 0i32;
    while n >= 10.0 {
        n /= 10.0;
        e += 1;
    }
    while n < 1.0 {
        n *= 10.0;
        e -= 1;
    }
    let mut d = (n * 100_000_000.0 + 0.5) as u64;
    if d == 1_000_000_000 {
        d = 100_000_000;
        e += 1;
    }
    format!("{sign}{}.{:08}e{}{:02}", d / 100_000_000, d % 100_000_000, if e < 0 { '-' } else { '+' }, e.abs())
}

fn vec(values: &[f64]) -> String {
    values.iter().map(|v| fmt_float(*v)).collect::<Vec<_>>().join(" ")
}

pub(crate) const CSV_HEADER: &str = "timestamp_ms,monotonic_ms,sensor,value,raw_adc_code,raw_value,calibrated_value,time_source,adc_temperature_c,adc_temperature_code,uptime_ticks,uptime_frequency_hz,record_sequence\r\n";
const FC_BLOCK: &str = "# seds-sd,format=fc,version=2,board=FC,firmware=0ad1ecd12345,tick_hz=1000,schema=e59ae8a7\r\n\
# seds-sd-columns,u64:ms,u64:ms,label,vec,vec,vec,vec,clock,f32:degC,i32:code,u64:tick,u32:Hz,u64:seq\r\n\
# seds-sd-records,accelerometer_sample:v=m/s2*3:a=i16*3:r=i16*3:c=m/s2*3,gyroscope_sample:v=deg/s*3:a=i16*3:r=i16*3:c=deg/s*3,barometer_sample:v=Pa;degC;m:a=u24*2;nan:r=u24*2;nan:c=Pa;degC;m\r\n";

/// Deterministic xorshift noise (no `rand` state shared between tests).
pub(crate) struct Noise(u64);

impl Noise {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }
    pub(crate) fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    /// Roughly normal, mean 0, deviation `sd`.
    pub(crate) fn gauss(&mut self, sd: f64) -> f64 {
        let sum: f64 = (0..4).map(|_| (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64).sum();
        (sum - 2.0) * sd * 1.73
    }
    pub(crate) fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }
}

fn fc_record(out: &mut String, utc: u64, up: u64, sensor: &str, value: &str, raw: Option<&str>, seq: u64) {
    let v = value.replace('"', "\"\"");
    let raw = raw.unwrap_or("");
    let cal = if raw.is_empty() { "" } else { value };
    out.push_str(&format!("{utc},{up},{sensor},\"{v}\",\"{raw}\",\"{raw}\",\"{cal}\",network,,,{up},1000,{seq}\r\n"));
}

/// An FC launch file of `ms` milliseconds: the pad phase (accel and gyro at
/// 100 Hz, IMU 20 Hz), then ascent and descent rates, barometer at 25 Hz,
/// DKF in descent, a few event rows. About 15 KB per second of pad.
pub(crate) fn fc_launch_csv(ms: u64, seed: u64) -> Vec<u8> {
    let mut rnd = Noise::new(seed);
    let mut out = String::from(CSV_HEADER);
    out.push_str(FC_BLOCK);
    let (utc0, up0, mut seq) = (1_794_877_191_000u64, 734_000u64, 48_000u64);
    let acc_lsb = 24.0 * 9.80665 / 32768.0;
    let gyr_lsb = 2000.0 / 32768.0;
    let mut last_imu = 0u64;
    for t in 0..ms {
        let pad = t < ms / 4;
        let ascent = !pad && t < ms / 2;
        let phase = t as f64 / 1000.0;
        if (pad && t % 10 == 0) || (ascent && t % 40 == 0) {
            let a = [0.3 * phase.sin() + rnd.gauss(0.05), rnd.gauss(0.05), 9.8 + if ascent { 40.0 } else { 0.0 } + rnd.gauss(0.05)];
            let g = [rnd.gauss(0.5), rnd.gauss(0.5), 2.0 * phase.cos() + rnd.gauss(0.5)];
            let a_raw: Vec<f64> = a.iter().map(|v| (v / acc_lsb).round()).collect();
            let g_raw: Vec<f64> = g.iter().map(|v| (v / gyr_lsb).round()).collect();
            let av: Vec<f64> = a_raw.iter().map(|r| r * acc_lsb).collect();
            let gv: Vec<f64> = g_raw.iter().map(|r| r * gyr_lsb).collect();
            fc_record(&mut out, utc0 + t, up0 + t, "accelerometer_sample", &vec(&av), Some(&vec(&a_raw)), seq);
            fc_record(&mut out, utc0 + t + 1, up0 + t + 1, "gyroscope_sample", &vec(&gv), Some(&vec(&g_raw)), seq + 1);
            seq += 2;
            if t - last_imu >= 50 || t == 0 {
                last_imu = t;
                let both: Vec<f64> = av.iter().chain(&gv).copied().collect();
                fc_record(&mut out, utc0 + t + 1, up0 + t + 1, "IMU", &vec(&both), None, seq);
                seq += 1;
            }
        }
        if t % 40 == 3 {
            let alt = if pad { 0.0 } else { 3000.0 * (phase / 10.0).min(1.0) };
            let p = 91_000.0 - alt * 11.0 + rnd.gauss(1.2);
            let temp = 30.0 + rnd.gauss(0.01);
            let praw = (8.3e6 + (p - 90_000.0) * 21.7 + rnd.gauss(3.0)).trunc();
            let traw = (5.1e6 + temp * 1000.0 + rnd.gauss(2.0)).trunc();
            let raw = vec(&[praw, traw, f64::NAN]);
            fc_record(&mut out, utc0 + t, up0 + t, "barometer_sample", &vec(&[p, temp, alt]), Some(&raw), seq);
            fc_record(&mut out, utc0 + t, up0 + t, "BARO", &vec(&[alt, p, temp]), None, seq + 1);
            seq += 2;
            if !pad && !ascent {
                let dkf = [rnd.gauss(1e-5), rnd.gauss(1e-5), alt + rnd.gauss(0.5), -20.0 + rnd.gauss(0.5)];
                fc_record(&mut out, utc0 + t + 1, up0 + t + 1, "DKF", &vec(&dkf), None, seq);
                seq += 1;
            }
        }
        if t % 5_000 == 0 {
            fc_record(&mut out, utc0 + t, up0 + t, "Ordered", &format!("EV new flight state: {}", t / 5_000 % 11), None, seq);
            seq += 1;
        }
        if t == ms / 4 + 10 {
            fc_record(&mut out, utc0 + t, up0 + t, "pyro_gpio_high", "he said \"go\", then, ok", None, seq);
            seq += 1;
        }
    }
    out.into_bytes()
}
