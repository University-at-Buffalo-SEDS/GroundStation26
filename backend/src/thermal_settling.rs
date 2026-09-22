//! Thermal points require stable *uncorrected* load and ADC temperature.
//! This detects observed stability, not physical temperature equilibrium.
use serde::Serialize;
use std::collections::VecDeque;

const BIN_MS: i64 = 10_000;
const BINS: usize = 12;
#[derive(Default)]
struct Bin {
    id: i64,
    n: usize,
    raw: f64,
    temp: f64,
    kg: f64,
    first_ms: i64,
    last_ms: i64,
}
#[derive(Default)]
pub struct Window {
    bins: VecDeque<Bin>,
    last_ms: Option<i64>,
}
#[derive(Clone, Serialize)]
pub struct Settling {
    pub ready: bool,
    pub message: String,
    pub observed_s: usize,
    pub required_s: usize,
    pub load_range_kg: Option<f64>,
    pub load_limit_kg: f64,
    pub temperature_range_c: Option<f64>,
    pub temperature_limit_c: f64,
    pub raw: Option<f64>,
    pub temperature_c: Option<f64>,
}
impl Window {
    pub fn add(&mut self, ms: i64, raw: f64, temp: f64, kg: Option<f64>) {
        let valid = kg.filter(|v| v.is_finite());
        if ms < 0
            || !raw.is_finite()
            || !temp.is_finite()
            || !(-40.0..=125.0).contains(&temp)
            || valid.is_none()
        {
            self.bins.clear();
            self.last_ms = None;
            return;
        }
        if self
            .last_ms
            .is_some_and(|last| ms <= last || ms - last > 2000)
        {
            self.bins.clear();
        }
        self.last_ms = Some(ms);
        let id = ms / BIN_MS;
        if self.bins.back().is_none_or(|b| b.id != id) {
            self.bins.push_back(Bin {
                id,
                first_ms: ms,
                ..Default::default()
            });
        }
        while self.bins.len() > BINS + 1 {
            self.bins.pop_front();
        }
        let b = self.bins.back_mut().unwrap();
        b.last_ms = ms;
        b.n += 1;
        b.raw += raw;
        b.temp += temp;
        b.kg += valid.unwrap();
    }
    pub fn assess(&self, now: i64, sensor: &str) -> Settling {
        let limit = if sensor == "KG50" { 0.005 } else { 0.05 };
        let mut out = Settling {
            ready: false,
            message: String::new(),
            observed_s: 0,
            required_s: 120,
            load_range_kg: None,
            load_limit_kg: limit,
            temperature_range_c: None,
            temperature_limit_c: 0.2,
            raw: None,
            temperature_c: None,
        };
        if self
            .last_ms
            .is_none_or(|last| now < last || now - last > 2000)
        {
            out.message =
                "Waiting for fresh load, ADC temperature, and a valid mass calibration".into();
            return out;
        }
        // Only completed ten-second bins qualify; a burst cannot substitute for a dwell.
        let valid_bin = |b: &Bin| {
            b.n >= 10
                && b.first_ms - b.id * BIN_MS <= 1000
                && (b.id + 1) * BIN_MS - b.last_ms <= 1000
        };
        let completed: Vec<_> = self.bins.iter().filter(|b| b.id < now / BIN_MS).collect();
        out.observed_s = completed
            .iter()
            .rev()
            .take_while(|b| valid_bin(b))
            .count()
            .min(BINS)
            * 10;
        if completed.len() < BINS
            || completed.iter().any(|b| !valid_bin(b))
            || completed.windows(2).any(|b| b[1].id != b[0].id + 1)
        {
            out.message = format!(
                "Collecting settling history: {}/120 seconds",
                out.observed_s
            );
            return out;
        }
        let range = |f: fn(&Bin) -> f64| {
            let lo = self
                .bins
                .iter()
                .map(|b| f(b) / b.n as f64)
                .fold(f64::INFINITY, f64::min);
            let hi = self
                .bins
                .iter()
                .map(|b| f(b) / b.n as f64)
                .fold(f64::NEG_INFINITY, f64::max);
            hi - lo
        };
        let load_range = range(|b| b.kg);
        let temp_range = range(|b| b.temp);
        out.load_range_kg = Some(load_range);
        out.temperature_range_c = Some(temp_range);
        out.ready = load_range <= limit && temp_range <= 0.2;
        out.message = if out.ready {
            "Settled readings available; confirm physically unloaded before capture".into()
        } else {
            format!(
                "Waiting for settling: load range {:.4} kg (limit {:.4}); ADC range {:.3} °C (limit 0.200)",
                load_range, limit, temp_range
            )
        };
        if out.ready {
            out.raw = Some(
                completed.iter().map(|b| b.raw / b.n as f64).sum::<f64>() / completed.len() as f64,
            );
            out.temperature_c = Some(
                completed.iter().map(|b| b.temp / b.n as f64).sum::<f64>() / completed.len() as f64,
            );
        }
        out
    }
}

pub fn from_rows(
    rows: &[crate::types::TelemetryRow],
    cfg: &crate::loadcell::LoadcellCalibrationFile,
    sensor: &str,
    now: i64,
) -> Settling {
    let mut ordered: Vec<_> = rows
        .iter()
        .filter(|r| {
            r.sender_id == "DAQ"
                && r.timestamp_ms <= now
                && now - r.timestamp_ms <= 132_000
                && (r.data_type == sensor || r.data_type == "DAQ_ADC_TEMPERATURE")
        })
        .collect();
    ordered.sort_by_key(|r| r.timestamp_ms);
    let mut temperature = None;
    let mut window = Window::default();
    for r in ordered {
        let value = r
            .values
            .first()
            .copied()
            .flatten()
            .filter(|v| v.is_finite());
        if r.data_type == "DAQ_ADC_TEMPERATURE" {
            temperature = value.map(|v| (r.timestamp_ms, v));
            continue;
        }
        let temp = temperature
            .filter(|(ms, _)| r.timestamp_ms - ms <= 2000)
            .map(|(_, v)| v);
        let kg = value.and_then(|v| {
            // KG50's legacy raw fallback is not a calibrated mass.
            if sensor == "KG50" && !cfg.extra_channels.contains_key("kg50") {
                return None;
            }
            crate::loadcell::calibrated_weight_kg(cfg, sensor, v)
        });
        window.add(
            r.timestamp_ms,
            value.unwrap_or(f32::NAN) as f64,
            temp.unwrap_or(f32::NAN) as f64,
            kg.map(|v| v as f64),
        );
    }
    window.assess(now, sensor)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn adc_stable_but_load_still_creeping_must_wait() {
        let mut w = Window::default();
        for i in 0..=1200 {
            w.add(i * 100, i as f64 * 0.001, 30., Some(i as f64 * 0.001));
        }
        assert!(!w.assess(120000, "KG1000").ready);
        for i in 1201..=2500 {
            w.add(i * 100, 1.2, 30., Some(1.2));
        }
        assert!(w.assess(250000, "KG1000").ready);
    }
    #[test]
    fn stable_load_with_warming_adc_and_stale_data_do_not_qualify() {
        let mut w = Window::default();
        for i in 0..=1200 {
            w.add(i * 100, 1., 20. + i as f64 / 1000., Some(1.));
        }
        assert!(!w.assess(120000, "KG1000").ready);
        let mut w = Window::default();
        for i in 0..=1200 {
            w.add(i * 100, 1., 30., Some(1.));
        }
        assert!(w.assess(120000, "KG1000").ready);
        assert!(!w.assess(123000, "KG1000").ready);
        w.add(130000, 1., 30., Some(1.));
        assert!(!w.assess(130000, "KG1000").ready);
    }
    #[test]
    fn missing_calibration_and_short_bursts_cannot_qualify() {
        let mut w = Window::default();
        for i in 0..=1200 {
            w.add(i * 100, 1., 30., None);
        }
        assert!(!w.assess(120000, "KG1000").ready);
        for i in 0..1000 {
            w.add(130000 + i, 1., 30., Some(1.));
        }
        assert!(!w.assess(131000, "KG1000").ready);
    }
}
