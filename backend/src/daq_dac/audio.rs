//! WAV import for DAC custom waveforms: parse a RIFF/WAVE file (PCM 8/16/24/32
//! bit or IEEE float 32/64, any channel count, mixed to mono) and fit a clip
//! of it into a DAQ table at a chosen playback rate.

/// Keeps the base64 request under the 2 MB HTTP body limit; a 1 MiB 16-bit
/// mono 44.1 kHz file is about 12 s, far more than one 1024-sample table.
pub const MAX_FILE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Audio {
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub bits: u16,
    /// Mono, full scale -1..1.
    pub samples: Vec<f64>,
}

impl Audio {
    pub fn duration_s(&self) -> f64 {
        self.samples.len() as f64 / f64::from(self.sample_rate_hz)
    }
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

pub fn parse_wav(bytes: &[u8]) -> Result<Audio, String> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err(format!(
            "The WAV file is larger than {} MiB.",
            MAX_FILE_BYTES / (1024 * 1024)
        ));
    }
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("Not a WAV file (no RIFF/WAVE header).".into());
    }
    let mut at = 12;
    let mut format: Option<(u16, u16, u32, u16)> = None;
    let mut data: Option<&[u8]> = None;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32_at(bytes, at + 4) as usize;
        let body_start = at + 8;
        let body_end = body_start.saturating_add(size).min(bytes.len());
        let body = &bytes[body_start..body_end];
        if id == b"fmt " {
            if body.len() < 16 {
                return Err("The WAV fmt chunk is truncated.".into());
            }
            let mut tag = u16_at(body, 0);
            let channels = u16_at(body, 2);
            let rate = u32_at(body, 4);
            let bits = u16_at(body, 14);
            if tag == 0xFFFE {
                if body.len() < 26 {
                    return Err("The WAV extensible fmt chunk is truncated.".into());
                }
                tag = u16_at(body, 24);
            }
            format = Some((tag, channels, rate, bits));
        } else if id == b"data" {
            data = Some(body);
        }
        // Chunks are word aligned.
        at = body_start.saturating_add(size).saturating_add(size & 1);
    }
    let (tag, channels, rate, bits) = format.ok_or("The WAV file has no fmt chunk.")?;
    let data = data.ok_or("The WAV file has no data chunk.")?;
    if channels == 0 || channels > 32 {
        return Err(format!("Unsupported channel count {channels}."));
    }
    if rate == 0 || rate > 768_000 {
        return Err(format!("Unsupported sample rate {rate} Hz."));
    }
    let width = match (tag, bits) {
        (1, 8) => 1,
        (1, 16) => 2,
        (1, 24) => 3,
        (1, 32) | (3, 32) => 4,
        (3, 64) => 8,
        _ => {
            return Err(format!(
                "Unsupported WAV encoding (format {tag}, {bits} bits); use PCM 8/16/24/32-bit or float 32/64-bit."
            ));
        }
    };
    let frame = width * usize::from(channels);
    let frames = data.len() / frame;
    if frames == 0 {
        return Err("The WAV file has no samples.".into());
    }
    let decode = |s: &[u8]| -> f64 {
        match (tag, width) {
            (1, 1) => (f64::from(s[0]) - 128.0) / 128.0,
            (1, 2) => f64::from(i16::from_le_bytes([s[0], s[1]])) / 32768.0,
            (1, 3) => f64::from(i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) / 8_388_608.0,
            (1, 4) => f64::from(i32::from_le_bytes([s[0], s[1], s[2], s[3]])) / 2_147_483_648.0,
            (3, 4) => f64::from(f32::from_le_bytes([s[0], s[1], s[2], s[3]])),
            _ => f64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]),
        }
    };
    let mut samples = Vec::with_capacity(frames);
    for f in 0..frames {
        let base = f * frame;
        let mut sum = 0.0;
        for c in 0..usize::from(channels) {
            let v = decode(&data[base + c * width..base + (c + 1) * width]);
            sum += if v.is_finite() { v } else { 0.0 };
        }
        samples.push(sum / f64::from(channels));
    }
    Ok(Audio {
        sample_rate_hz: rate,
        channels,
        bits,
        samples,
    })
}

/// `count` samples of `audio` from `start_s` at `rate_hz`: box-averaged when
/// decimating (a simple anti-alias filter), linearly interpolated otherwise.
pub fn resample(audio: &Audio, start_s: f64, rate_hz: f64, count: usize) -> Vec<f64> {
    let rin = f64::from(audio.sample_rate_hz);
    let src = &audio.samples;
    let last = src.len().saturating_sub(1);
    let at = |i: usize| src[i.min(last)];
    (0..count)
        .map(|k| {
            let t0 = start_s + k as f64 / rate_hz;
            let t1 = start_s + (k + 1) as f64 / rate_hz;
            if rin > rate_hz {
                let a = (t0 * rin).floor().max(0.0) as usize;
                let b = ((t1 * rin).floor() as usize).max(a + 1).min(src.len());
                if a >= b {
                    return at(a);
                }
                src[a..b].iter().sum::<f64>() / (b - a) as f64
            } else {
                let p = (t0 * rin).max(0.0);
                let i = p.floor() as usize;
                let frac = p - p.floor();
                at(i) * (1.0 - frac) + at(i + 1) * frac
            }
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn wav_bytes(rate: u32, channels: u16, bits: u16, float: bool, frames: &[Vec<f64>]) -> Vec<u8> {
    let width = usize::from(bits / 8);
    let mut data = Vec::new();
    for frame in frames {
        for &v in frame {
            match (float, bits) {
                (true, 32) => data.extend((v as f32).to_le_bytes()),
                (true, _) => data.extend(v.to_le_bytes()),
                (false, 8) => data.push(((v * 127.0).round() + 128.0) as u8),
                (false, 16) => data.extend(((v * 32767.0).round() as i16).to_le_bytes()),
                (false, 24) => data.extend(&((v * 8_388_607.0).round() as i32).to_le_bytes()[..3]),
                (false, _) => data.extend(((v * 2_147_483_647.0).round() as i32).to_le_bytes()),
            }
        }
    }
    let mut b = Vec::new();
    b.extend(b"RIFF");
    b.extend((36 + data.len() as u32 + 12).to_le_bytes());
    b.extend(b"WAVE");
    // An unrelated chunk with odd size, to exercise padding.
    b.extend(b"LIST");
    b.extend(3u32.to_le_bytes());
    b.extend([1, 2, 3, 0]);
    b.extend(b"fmt ");
    b.extend(16u32.to_le_bytes());
    b.extend((if float { 3u16 } else { 1u16 }).to_le_bytes());
    b.extend(channels.to_le_bytes());
    b.extend(rate.to_le_bytes());
    b.extend((rate * u32::from(channels) * width as u32).to_le_bytes());
    b.extend((channels * bits / 8).to_le_bytes());
    b.extend(bits.to_le_bytes());
    b.extend(b"data");
    b.extend((data.len() as u32).to_le_bytes());
    b.extend(data);
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_supported_encoding_decodes_to_full_scale() {
        let frames: Vec<Vec<f64>> = vec![vec![0.0, 0.5], vec![0.5, -0.5], vec![-1.0, 1.0]];
        for (bits, float, tol) in [
            (8, false, 1.0 / 127.0),
            (16, false, 1e-4),
            (24, false, 1e-6),
            (32, false, 1e-8),
            (32, true, 1e-7),
            (64, true, 0.0),
        ] {
            let a = parse_wav(&wav_bytes(8000, 2, bits, float, &frames)).unwrap();
            assert_eq!((a.sample_rate_hz, a.channels, a.bits), (8000, 2, bits));
            // Stereo is mixed down to mono.
            let want = [0.25, 0.0, 0.0];
            for (got, want) in a.samples.iter().zip(want) {
                assert!((got - want).abs() <= tol + 1e-12, "{bits} {float}: {got} vs {want}");
            }
        }
    }

    #[test]
    fn malformed_and_unsupported_files_are_rejected() {
        let good = wav_bytes(8000, 1, 16, false, &[vec![0.1], vec![0.2]]);
        assert!(parse_wav(&good).is_ok());
        assert!(parse_wav(b"not a wav file at all").is_err());
        assert!(parse_wav(&good[..20]).is_err());
        let mut no_data = good.clone();
        let pos = no_data.windows(4).position(|w| w == b"data").unwrap();
        no_data[pos..pos + 4].copy_from_slice(b"junk");
        assert!(parse_wav(&no_data).unwrap_err().contains("no data"));
        let mut adpcm = good.clone();
        let fmt = adpcm.windows(4).position(|w| w == b"fmt ").unwrap();
        adpcm[fmt + 8] = 2;
        assert!(parse_wav(&adpcm).unwrap_err().contains("Unsupported WAV encoding"));
        let mut zero_rate = good.clone();
        zero_rate[fmt + 12..fmt + 16].copy_from_slice(&0u32.to_le_bytes());
        assert!(parse_wav(&zero_rate).is_err());
        assert!(parse_wav(&vec![0u8; MAX_FILE_BYTES + 1]).is_err());
        // A size field pointing past the end is clamped, not trusted.
        let mut huge = good.clone();
        let data = huge.windows(4).position(|w| w == b"data").unwrap();
        huge[data + 4..data + 8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse_wav(&huge).unwrap().samples.len(), 2);
    }

    #[test]
    fn resampling_decimates_with_averaging_and_interpolates_up() {
        let audio = Audio {
            sample_rate_hz: 4,
            channels: 1,
            bits: 16,
            samples: vec![0.0, 1.0, 0.0, 1.0, 0.4, 0.4, 0.4, 0.4],
        };
        // 4 -> 2 S/s: pairs average.
        assert_eq!(resample(&audio, 0.0, 2.0, 4), vec![0.5, 0.5, 0.4, 0.4]);
        // 4 -> 8 S/s: midpoints interpolate.
        let up = resample(&audio, 0.0, 8.0, 4);
        assert_eq!(up, vec![0.0, 0.5, 1.0, 0.5]);
        // Start offset.
        assert_eq!(resample(&audio, 1.0, 4.0, 2), vec![0.4, 0.4]);
        assert!((audio.duration_s() - 2.0).abs() < 1e-12);
    }
}
