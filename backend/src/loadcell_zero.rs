//! Long unloaded acquisition. A bounded queue keeps disk I/O off telemetry processing.
use crate::loadcell::{LoadcellCalibrationFile, NoiseCalibration, ThermalCalibration};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, SyncSender},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
struct Sample {
    timestamp_ms: i64,
    raw: f64,
    temperature: f64,
}
#[derive(Default)]
struct Moments {
    n: u64,
    mean: [f64; 3],
    cov: [[f64; 3]; 3],
    first_ms: i64,
    last_ms: i64,
    min_t: f64,
    max_t: f64,
    previous: Option<Sample>,
    diff_yy: f64,
    diff_xy: f64,
    diff_xx: f64,
    differences: u64,
}
impl Moments {
    fn add(&mut self, s: Sample) -> bool {
        if !s.raw.is_finite()
            || !s.temperature.is_finite()
            || !(-40.0..=125.0).contains(&s.temperature)
            || (self.n > 0 && s.timestamp_ms <= self.last_ms)
        {
            return false;
        }
        if self.n == 0 {
            self.first_ms = s.timestamp_ms;
            self.min_t = s.temperature;
            self.max_t = s.temperature;
        }
        self.min_t = self.min_t.min(s.temperature);
        self.max_t = self.max_t.max(s.temperature);
        if let Some(p) = self
            .previous
            .filter(|p| s.timestamp_ms - p.timestamp_ms <= 2000)
        {
            let dx = s.temperature - p.temperature;
            let dy = s.raw - p.raw;
            self.diff_yy += dy * dy;
            self.diff_xy += dx * dy;
            self.diff_xx += dx * dx;
            self.differences += 1;
        }
        self.previous = Some(s);
        self.last_ms = s.timestamp_ms;
        self.n += 1;
        let values = [
            s.temperature,
            s.raw,
            (s.timestamp_ms - self.first_ms) as f64 / 1000.0,
        ];
        let delta = std::array::from_fn::<_, 3, _>(|i| values[i] - self.mean[i]);
        for (i, value) in delta.iter().enumerate() {
            self.mean[i] += value / self.n as f64;
        }
        for (i, delta_value) in delta.iter().enumerate() {
            for (j, value) in values.iter().enumerate() {
                self.cov[i][j] += delta_value * (value - self.mean[j]);
            }
        }
        true
    }
    fn report(&self) -> Report {
        let slope = if self.cov[0][0] > 1e-12 {
            self.cov[0][1] / self.cov[0][0]
        } else {
            0.0
        };
        let residual = (self.cov[1][1] - slope * self.cov[0][1]).max(0.0);
        let duration = (self.last_ms - self.first_ms).max(0) as f64 / 1000.;
        Report {
            samples: self.n,
            duration_s: duration,
            temperature_min_c: self.min_t,
            temperature_max_c: self.max_t,
            reference_c: self.mean[0],
            zero_raw: self.mean[1],
            raw_per_c: slope,
            residual_sigma_raw: (residual / (self.n.saturating_sub(2).max(1) as f64)).sqrt(),
            adjacent_noise_sigma_raw: ((self.diff_yy - 2. * slope * self.diff_xy
                + slope * slope * self.diff_xx)
                .max(0.)
                / (2. * self.differences.max(1) as f64))
                .sqrt(),
            residual_raw_per_hour: if self.cov[2][2] > 0. {
                3600. * (self.cov[2][1] - slope * self.cov[2][0]) / self.cov[2][2]
            } else {
                0.
            },
            temperature_time_correlation: if self.cov[0][0] * self.cov[2][2] > 0. {
                self.cov[0][2] / (self.cov[0][0] * self.cov[2][2]).sqrt()
            } else {
                0.
            },
            noise_ready: self.n >= 1000 && duration >= 60.,
            thermal_ready: false,
            thermal_quality: Default::default(),
            settled_points: vec![],
            settling_message: String::new(),
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Report {
    pub samples: u64,
    pub duration_s: f64,
    pub temperature_min_c: f64,
    pub temperature_max_c: f64,
    pub reference_c: f64,
    pub zero_raw: f64,
    pub raw_per_c: f64,
    pub residual_sigma_raw: f64,
    pub adjacent_noise_sigma_raw: f64,
    pub residual_raw_per_hour: f64,
    pub temperature_time_correlation: f64,
    pub noise_ready: bool,
    pub thermal_ready: bool,
    #[serde(default)]
    pub thermal_quality: crate::thermal_fit::Quality,
    #[serde(default)]
    pub settled_points: Vec<[f64; 2]>,
    #[serde(default)]
    pub settling_message: String,
}
#[derive(Default)]
struct SettledFit {
    window: crate::thermal_settling::Window,
    points: Vec<[f64; 2]>,
    last_point_ms: Option<i64>,
    last_bucket: Option<i64>,
    message: String,
}
impl SettledFit {
    fn add(&mut self, sample: Sample, sensor: &str, cfg: &LoadcellCalibrationFile) {
        let kg = if sensor == "KG50" && !cfg.extra_channels.contains_key("kg50") {
            None
        } else {
            crate::loadcell::calibrated_weight_kg(cfg, sensor, sample.raw as f32).map(|v| v as f64)
        };
        self.window
            .add(sample.timestamp_ms, sample.raw, sample.temperature, kg);
        let bucket = sample.timestamp_ms / 10_000;
        if self.last_bucket == Some(bucket) {
            return;
        }
        self.last_bucket = Some(bucket);
        let settled = self.window.assess(sample.timestamp_ms, sensor);
        self.message = settled.message;
        if settled.ready
            && self
                .last_point_ms
                .is_none_or(|t| sample.timestamp_ms - t >= 120_000)
        {
            self.points
                .push([settled.temperature_c.unwrap(), settled.raw.unwrap()]);
            self.last_point_ms = Some(sample.timestamp_ms);
        }
    }
    fn report(&self, moments: &Moments) -> Report {
        let mut r = moments.report();
        r.settled_points = self.points.clone();
        r.settling_message = self.message.clone();
        r.thermal_quality = crate::thermal_fit::assess(&self.points);
        r.thermal_ready = r.noise_ready && r.thermal_quality.ready;
        r.raw_per_c = 0.;
        if !r.thermal_ready {
            return r;
        }
        let slope = r.thermal_quality.slope;
        r.reference_c = r.thermal_quality.reference_c;
        r.zero_raw = r.thermal_quality.zero_raw;
        r.raw_per_c = slope;
        r.residual_sigma_raw = ((moments.cov[1][1] - 2. * slope * moments.cov[0][1]
            + slope * slope * moments.cov[0][0])
            .max(0.)
            / moments.n.saturating_sub(2).max(1) as f64)
            .sqrt();
        r.adjacent_noise_sigma_raw = ((moments.diff_yy - 2. * slope * moments.diff_xy
            + slope * slope * moments.diff_xx)
            .max(0.)
            / (2. * moments.differences.max(1) as f64))
            .sqrt();
        r.residual_raw_per_hour = if moments.cov[2][2] > 0. {
            3600. * (moments.cov[2][1] - slope * moments.cov[2][0]) / moments.cov[2][2]
        } else {
            0.
        };
        r
    }
}

#[derive(Clone, Default, Serialize)]
pub struct Status {
    pub session_id: String,
    pub sensor_id: String,
    pub running: bool,
    pub elapsed_s: u64,
    pub requested_duration_s: u64,
    pub rejected_samples: u64,
    pub dropped_samples: u64,
    pub csv_path: String,
    pub report_path: String,
    pub error: Option<String>,
    pub report: Report,
}
/// Original source time is distinct from the receipt timestamp retained in CSV.
/// A capture follows one selected DAQ channel; another channel cannot advance it.
pub struct SourceSample<'a> {
    pub sender: &'a str,
    pub sensor: &'a str,
    pub timestamp_ms: i64,
    pub source_timestamp_ms: Option<i64>,
    pub received_network_ms: i64,
    pub raw: f32,
    pub temperature: Option<f32>,
}
struct Session {
    status: Arc<Mutex<Status>>,
    tx: SyncSender<Sample>,
    stop: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    last_source_ms: Option<i64>,
}
#[derive(Default)]
pub struct Service {
    session: Mutex<Option<Session>>,
    filters: Mutex<BTreeMap<String, Filter>>,
}
impl Service {
    pub fn reset_kg50_input(&self) {
        self.filters.lock().unwrap().remove("KG50");
        let mut slot = self.session.lock().unwrap();
        if slot
            .as_ref()
            .is_some_and(|s| s.status.lock().unwrap().sensor_id == "KG50")
        {
            // Source switching is rejected while capture is running. Retain the
            // CSV on disk but prevent applying an old-source completed report.
            *slot = None;
        }
    }

    pub fn status(&self) -> Status {
        self.session
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| {
                let mut status = s.status.lock().unwrap().clone();
                status.dropped_samples = s.dropped.load(Ordering::Relaxed);
                status
            })
            .unwrap_or_default()
    }
    #[cfg(test)]
    pub fn start(&self, sensor: &str, duration_s: u64, known_zero: bool) -> Result<Status, String> {
        self.start_calibrated(
            sensor,
            duration_s,
            known_zero,
            LoadcellCalibrationFile::default(),
        )
    }
    pub fn start_calibrated(
        &self,
        sensor: &str,
        duration_s: u64,
        known_zero: bool,
        cfg: LoadcellCalibrationFile,
    ) -> Result<Status, String> {
        if !known_zero
            || !matches!(sensor, "KG1000" | "KG50")
            || !(60..=86400).contains(&duration_s)
        {
            return Err("Confirm unloaded KG1000/KG50 and choose 60–86400 seconds".into());
        }
        let dir = std::env::var_os("GS_ZERO_CAPTURE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("calibration/zero_captures")
            });
        self.start_in_dir_calibrated(sensor, duration_s, dir, cfg)
    }
    #[cfg(test)]
    fn start_in_dir(&self, sensor: &str, duration_s: u64, dir: PathBuf) -> Result<Status, String> {
        self.start_in_dir_calibrated(sensor, duration_s, dir, LoadcellCalibrationFile::default())
    }
    fn start_in_dir_calibrated(
        &self,
        sensor: &str,
        duration_s: u64,
        dir: PathBuf,
        cfg: LoadcellCalibrationFile,
    ) -> Result<Status, String> {
        let mut slot = self.session.lock().unwrap();
        if slot
            .as_ref()
            .is_some_and(|s| s.status.lock().unwrap().running)
        {
            return Err("A zero capture is already running".into());
        }
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let id = format!(
            "{}-{}",
            sensor,
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        );
        let path = dir.join(format!("{id}.csv"));
        let report_path = dir.join(format!("{id}.json"));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| e.to_string())?;
        let status = Arc::new(Mutex::new(Status {
            session_id: id,
            sensor_id: sensor.into(),
            running: true,
            requested_duration_s: duration_s,
            csv_path: path.display().to_string(),
            report_path: report_path.display().to_string(),
            ..Default::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::sync_channel::<Sample>(4096);
        let worker_status = status.clone();
        let worker_stop = stop.clone();
        let worker_dropped = dropped.clone();
        let sensor = sensor.to_string();
        std::thread::Builder::new()
            .name("loadcell-zero-capture".into())
            .spawn(move || {
                let mut file = BufWriter::new(file);
                let start = Instant::now();
                let mut moments = Moments::default();
                let mut settled = SettledFit::default();
                let result = (|| -> std::io::Result<()> {
                    writeln!(
                        file,
                        "timestamp_ms,known_load_kg,raw_value,adc_temperature_c"
                    )?;
                    let mut last_flush = Instant::now();
                    while !worker_stop.load(Ordering::Relaxed)
                        && start.elapsed().as_secs() < duration_s
                        && moments.n < 10_000_000
                    {
                        match rx.recv_timeout(Duration::from_millis(200)) {
                            Ok(sample) => {
                                if moments.add(sample) {
                                    settled.add(sample, &sensor, &cfg);
                                    writeln!(
                                        file,
                                        "{},0,{:.12},{:.6}",
                                        sample.timestamp_ms, sample.raw, sample.temperature
                                    )?;
                                } else {
                                    worker_status.lock().unwrap().rejected_samples += 1;
                                }
                            }
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        }
                        if last_flush.elapsed() >= Duration::from_secs(1) {
                            file.flush()?;
                            let mut s = worker_status.lock().unwrap();
                            s.report = settled.report(&moments);
                            s.elapsed_s = start.elapsed().as_secs();
                            last_flush = Instant::now();
                        }
                    }
                    file.flush()?;
                    file.get_ref().sync_all()?;
                    Ok(())
                })();
                let mut s = worker_status.lock().unwrap();
                s.report = settled.report(&moments);
                s.elapsed_s = start.elapsed().as_secs();
                s.dropped_samples =
                    worker_dropped.load(Ordering::Relaxed) + rx.try_iter().count() as u64;
                worker_dropped.store(s.dropped_samples, Ordering::Relaxed);
                if let Err(e) = result {
                    s.error = Some(e.to_string());
                }
                s.running = false;
                let save = File::create(&report_path).and_then(|f| {
                    serde_json::to_writer_pretty(f, &*s).map_err(std::io::Error::other)
                });
                if let Err(e) = save {
                    s.error = Some(format!("Could not save analysis: {e}"));
                }
            })
            .map_err(|e| e.to_string())?;
        let result = status.lock().unwrap().clone();
        *slot = Some(Session {
            status,
            tx,
            stop,
            dropped,
            last_source_ms: None,
        });
        Ok(result)
    }
    pub fn stop(&self, id: &str) -> Result<(), String> {
        let slot = self.session.lock().unwrap();
        let s = slot.as_ref().ok_or("No capture")?;
        if s.status.lock().unwrap().session_id != id {
            return Err("Capture changed; refresh status".into());
        }
        s.stop.store(true, Ordering::Relaxed);
        Ok(())
    }
    pub fn observe_source(&self, sample: SourceSample<'_>) {
        self.observe_inner(sample, true);
    }
    // Existing statistics fixtures deliberately supply synthetic sample times.
    // Production ingress must always supply original source provenance.
    #[cfg(test)]
    pub fn observe(
        &self,
        sender: &str,
        sensor: &str,
        timestamp_ms: i64,
        raw: f32,
        temperature: Option<f32>,
    ) {
        self.observe_inner(
            SourceSample {
                sender,
                sensor,
                timestamp_ms,
                source_timestamp_ms: None,
                received_network_ms: timestamp_ms,
                raw,
                temperature,
            },
            false,
        );
    }
    fn observe_inner(&self, sample: SourceSample<'_>, require_source: bool) {
        if sample.sender != "DAQ" {
            return;
        }
        let mut slot = self.session.lock().unwrap();
        let Some(s) = slot.as_mut() else {
            return;
        };
        let mut status = s.status.lock().unwrap();
        if !status.running || status.sensor_id != sample.sensor {
            return;
        }
        let source_valid = !require_source
            || sample.source_timestamp_ms.is_some_and(|source| {
                source >= 0
                    && s.last_source_ms.is_none_or(|last| source > last)
                    && if source >= 1_577_836_800_000
                        && sample.received_network_ms >= 1_577_836_800_000
                    {
                        sample
                            .received_network_ms
                            .checked_sub(source)
                            .is_some_and(|age| (-100..=2000).contains(&age))
                    } else {
                        true
                    }
            });
        if !source_valid
            || !sample.raw.is_finite()
            || sample.temperature.is_none_or(|value| !value.is_finite())
        {
            status.rejected_samples += 1;
            return;
        }
        if s.tx
            .try_send(Sample {
                timestamp_ms: sample.timestamp_ms,
                raw: sample.raw as f64,
                temperature: sample.temperature.unwrap() as f64,
            })
            .is_err()
        {
            s.dropped.fetch_add(1, Ordering::Relaxed);
        } else if require_source {
            // Only accepted source observations advance this capture. Replays,
            // stale inputs and queue refusals cannot refresh its provenance.
            s.last_source_ms = sample.source_timestamp_ms;
        }
    }
    pub fn apply(
        &self,
        id: &str,
        cfg: &mut LoadcellCalibrationFile,
        thermal: bool,
        tau_ms: f32,
    ) -> Result<(), String> {
        let s = self.status();
        if s.session_id != id || s.running || s.error.is_some() || !s.report.noise_ready {
            return Err("Finish a valid capture with >=1000 samples over >=60 seconds".into());
        }
        if !tau_ms.is_finite() || !(0.0..=2000.0).contains(&tau_ms) {
            return Err("Filter time constant must be 0–2000 ms (0 disables)".into());
        }
        let r = &s.report;
        if thermal {
            if !r.thermal_ready {
                return Err(r.thermal_quality.message.clone());
            }
            // Preserve the existing reference so ordinary mass-fit coordinates do not shift.
            let reference = cfg
                .thermal
                .get(&s.sensor_id)
                .map(|t| t.reference_c)
                .unwrap_or(r.reference_c as f32);
            cfg.thermal.insert(
                s.sensor_id.clone(),
                ThermalCalibration {
                    reference_c: reference,
                    raw_per_c: r.raw_per_c as f32,
                    points: vec![],
                },
            );
        }
        let zero = crate::loadcell::temperature_corrected_raw(
            cfg,
            &s.sensor_id,
            r.zero_raw as f32,
            Some(r.reference_c as f32),
        )
        .ok_or("Invalid zero reference")?;
        crate::loadcell::normalize_calibration(cfg);
        let channel = if s.sensor_id == "KG1000" {
            "ch1"
        } else {
            "kg50"
        };
        crate::loadcell::capture_zero(cfg, channel, zero);
        crate::loadcell::record_capture_temperature(
            cfg,
            channel,
            0.,
            r.zero_raw as f32,
            Some(r.reference_c as f32),
        );
        cfg.noise.insert(
            s.sensor_id.clone(),
            NoiseCalibration {
                sigma_raw: r.adjacent_noise_sigma_raw as f32,
                residual_sigma_raw: r.residual_sigma_raw as f32,
                tau_ms,
                session_id: s.session_id,
            },
        );
        crate::loadcell::validate_thermal(cfg)?;
        Ok(())
    }
    pub fn filter(
        &self,
        cfg: &LoadcellCalibrationFile,
        sender: &str,
        sensor: &str,
        timestamp: i64,
        raw: f32,
    ) -> f32 {
        if sender != "DAQ" {
            return raw;
        }
        let tau = cfg.noise.get(sensor).map(|n| n.tau_ms).unwrap_or(0.);
        let t = cfg.thermal.get(sensor);
        let signature = [
            tau,
            t.map(|t| t.reference_c).unwrap_or(0.),
            t.map(|t| t.raw_per_c).unwrap_or(0.),
        ];
        self.filters
            .lock()
            .unwrap()
            .entry(sensor.into())
            .or_default()
            .add(raw, timestamp, signature)
    }
}
#[derive(Default)]
pub struct Filter {
    value: f32,
    timestamp: Option<i64>,
    signature: [f32; 3],
}
impl Filter {
    pub fn add(&mut self, raw: f32, timestamp: i64, signature: [f32; 3]) -> f32 {
        let tau = signature[0];
        let delta = self
            .timestamp
            .map(|t| timestamp.saturating_sub(t))
            .unwrap_or(0);
        if !raw.is_finite() {
            self.timestamp = None;
            return raw;
        }
        if self.timestamp.is_none()
            || signature != self.signature
            || delta <= 0
            || delta > 2000
            || tau <= 0.
        {
            self.value = raw;
        } else {
            let alpha = delta as f32 / (tau + delta as f32);
            self.value += alpha * (raw - self.value);
        }
        self.timestamp = Some(timestamp);
        self.signature = signature;
        self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn separates_thermal_slope_and_baseline_noise() {
        let mut m = Moments::default();
        for i in 0..10000 {
            let t = 20. + 10. * i as f64 / 9999.;
            let noise = if i % 2 == 0 { 0.001 } else { -0.001 };
            assert!(m.add(Sample {
                timestamp_ms: 1000 + i * 10,
                temperature: t,
                raw: 0.2 + 0.002 * (t - 20.) + noise
            }));
        }
        let r = m.report();
        assert!(r.noise_ready && !r.thermal_ready);
        assert!((r.raw_per_c - 0.002).abs() < 1e-6);
        assert!((r.residual_sigma_raw - 0.001).abs() < 1e-5);
        assert!(r.adjacent_noise_sigma_raw > 0.001 && r.adjacent_noise_sigma_raw < 0.0015);
        assert!(!m.add(Sample {
            timestamp_ms: 1000,
            temperature: 20.,
            raw: 1.
        }));
    }
    #[test]
    fn constant_temperature_does_not_claim_drift_fit() {
        let mut m = Moments::default();
        for i in 0..1200 {
            m.add(Sample {
                timestamp_ms: i * 100,
                temperature: 25.,
                raw: 0.1,
            });
        }
        let r = m.report();
        assert!(r.noise_ready && !r.thermal_ready);
        assert_eq!(r.residual_sigma_raw, 0.);
    }
    #[test]
    fn long_fit_uses_settled_plateaus_and_excludes_temperature_transition() {
        let mut cfg = LoadcellCalibrationFile::default();
        cfg.ch1.m = Some(1.);
        cfg.ch1.b = Some(0.);
        let mut fit = SettledFit::default();
        let mut moments = Moments::default();
        for i in 0..=18000 {
            let temperature = 20. + (i / 1800) as f64 * 0.1;
            // Large lag error during the first minute at the new ADC temperature.
            let raw = 0.2
                + 0.002 * (temperature - 20.)
                + if (1800..2400).contains(&i) { 0.5 } else { 0. };
            let sample = Sample {
                timestamp_ms: i * 100,
                temperature,
                raw,
            };
            moments.add(sample);
            fit.add(sample, "KG1000", &cfg);
        }
        let report = fit.report(&moments);
        assert!(report.thermal_ready);
        assert!((report.raw_per_c - 0.002).abs() < 1e-8);
        assert!(report.settled_points.iter().all(|p| p[1] < 0.3));
    }
    #[test]
    fn smoothing_preserves_dc_steps_and_resets_on_gaps() {
        let mut f = Filter::default();
        let sig = [100., 20., 0.];
        assert_eq!(f.add(0., 0, sig), 0.);
        assert!((f.add(1., 100, sig) - 0.5).abs() < 1e-6);
        for i in 2..100 {
            f.add(1., i * 100, sig);
        }
        assert!((f.value - 1.).abs() < 1e-6);
        assert_eq!(f.add(5., 20000, sig), 5.);
        assert_eq!(f.add(6., 20100, [0., 20., 0.]), 6.);
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    #[test]
    fn records_raw_samples_stops_and_applies_only_completed_capture() {
        let dir = std::env::temp_dir().join(format!(
            "gs-zero-test-{}",
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        let service = Service::default();
        assert!(service.start("KG1000", 60, false).is_err());
        let status = service.start_in_dir("KG1000", 60, dir.clone()).unwrap();
        assert!(service.start_in_dir("KG50", 60, dir.clone()).is_err());
        let mut cfg = LoadcellCalibrationFile::default();
        assert!(
            service
                .apply(&status.session_id, &mut cfg, true, 100.)
                .is_err()
        );
        service.observe("OTHER", "KG1000", 1, 1., Some(20.));
        service.observe("DAQ", "KG1000", 1, 1., None);
        for i in 0..1200 {
            let t = 20. + i as f32 / 100.;
            service.observe(
                "DAQ",
                "KG1000",
                1000 + i * 100,
                0.2 + 0.002 * (t - 20.),
                Some(t),
            );
        }
        let deadline = Instant::now() + Duration::from_secs(4);
        while service.status().report.samples < 1200 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        service.stop(&status.session_id).unwrap();
        while service.status().running && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let done = service.status();
        assert!(!done.running && done.error.is_none());
        assert_eq!(done.report.samples, 1200);
        assert_eq!(done.rejected_samples, 1);
        assert_eq!(done.dropped_samples, 0);
        assert_eq!(
            std::fs::read_to_string(&done.csv_path)
                .unwrap()
                .lines()
                .count(),
            1201
        );
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&done.report_path).unwrap()).unwrap();
        assert_eq!(saved["report"]["samples"], 1200);
        assert!(service.apply("wrong-id", &mut cfg, true, 100.).is_err());
        // Continuous warming is still recorded, but cannot supply a settled fit.
        assert!(!done.report.thermal_ready);
        assert!(
            service
                .apply(&status.session_id, &mut cfg, true, 100.)
                .is_err()
        );
        service
            .apply(&status.session_id, &mut cfg, false, 100.)
            .unwrap();
        assert_eq!(cfg.noise["KG1000"].tau_ms, 100.);
        assert!(cfg.ch1_zero_raw.is_some());
        assert_eq!(cfg.temperature_captures["KG1000"][0].expected, 0.);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod source_provenance_tests {
    use super::*;

    fn input(source: Option<i64>, network: i64, receipt: i64) -> SourceSample<'static> {
        SourceSample {
            sender: "DAQ",
            sensor: "KG1000",
            timestamp_ms: receipt,
            source_timestamp_ms: source,
            received_network_ms: network,
            raw: 0.2,
            temperature: Some(20.),
        }
    }
    fn wait_samples(service: &Service, count: u64) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while service.status().report.samples < count && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(service.status().report.samples, count);
    }
    fn finish(service: &Service, capture: &Status) -> Status {
        service.stop(&capture.session_id).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while service.status().running && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let status = service.status();
        assert!(!status.running);
        status
    }
    fn directory(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "gs-source-capture-{label}-{}-{}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ))
    }

    #[test]
    fn reordered_stale_unknown_and_invalid_inputs_do_not_replace_fresh_constant_observations() {
        let dir = directory("utc");
        let service = Service::default();
        let capture = service.start_in_dir("KG1000", 60, dir.clone()).unwrap();
        let epoch = 1_793_000_000_000;
        service.observe_source(input(Some(epoch), epoch, 1000));
        service.observe_source(input(Some(epoch), epoch + 1, 1001)); // repeated original
        service.observe_source(input(Some(epoch - 1), epoch + 2, 1002)); // reordered
        service.observe_source(input(Some(epoch + 1), epoch + 2002, 1003)); // 2001ms stale
        service.observe_source(input(Some(epoch + 101), epoch, 1004)); // 101ms future
        service.observe_source(input(None, epoch + 3, 1005));
        let mut invalid = input(Some(epoch + 100), epoch + 100, 1006);
        invalid.raw = f32::NAN;
        service.observe_source(invalid); // must not advance the source watermark
        let mut other = input(Some(epoch + 999), epoch + 999, 1007);
        other.sensor = "KG50";
        service.observe_source(other); // another channel must not advance it
        let mut foreign = input(Some(epoch + 999), epoch + 999, 1008);
        foreign.sender = "GB";
        service.observe_source(foreign);
        service.observe_source(input(Some(epoch + 1), epoch + 2001, 1009)); // 2000ms accepted
        service.observe_source(input(Some(epoch + 2), epoch + 2, 1010));
        service.observe_source(input(Some(epoch + 3), epoch - 97, 1011)); // 100ms future accepted
        wait_samples(&service, 4);
        let done = finish(&service, &capture);
        assert_eq!(done.rejected_samples, 6);
        assert_eq!(done.dropped_samples, 0);
        assert!(!done.report.noise_ready && !done.report.thermal_ready);
        let mut cfg = LoadcellCalibrationFile::default();
        let before = serde_json::to_value(&cfg).unwrap();
        assert!(
            service
                .apply(&capture.session_id, &mut cfg, false, 100.)
                .is_err()
        );
        assert_eq!(serde_json::to_value(&cfg).unwrap(), before);
        let csv = std::fs::read_to_string(&done.csv_path).unwrap();
        let receipts: Vec<_> = csv
            .lines()
            .skip(1)
            .map(|line| line.split(',').next().unwrap())
            .collect();
        assert_eq!(
            receipts,
            vec!["1000", "1009", "1010", "1011"],
            "CSV retains original receipt timestamps"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn fresh_constant_samples_with_distinct_source_times_keep_noise_quality_and_saved_conversion() {
        let dir = directory("fresh");
        let service = Service::default();
        let capture = service.start_in_dir("KG1000", 60, dir.clone()).unwrap();
        let epoch = 1_793_000_000_000;
        for index in 0..1200 {
            let time = index * 100;
            service.observe_source(input(Some(epoch + time), epoch + time, 1000 + time));
        }
        wait_samples(&service, 1200);
        let done = finish(&service, &capture);
        assert_eq!((done.rejected_samples, done.dropped_samples), (0, 0));
        assert!(done.report.noise_ready && !done.report.thermal_ready);
        let mut cfg = LoadcellCalibrationFile::default();
        cfg.ch1.m = Some(100.);
        cfg.ch1.b = Some(2.);
        service
            .apply(&capture.session_id, &mut cfg, false, 100.)
            .unwrap();
        assert_eq!(cfg.ch1.m, Some(100.), "tare preserves the saved slope");
        assert!(
            crate::loadcell::calibrated_weight_kg(&cfg, "KG1000", 0.2)
                .unwrap()
                .abs()
                < 0.0001
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn uptime_clock_reset_requires_new_capture_and_channel_watermarks_remain_separate() {
        let dir = directory("uptime");
        let service = Service::default();
        let first = service.start_in_dir("KG1000", 60, dir.clone()).unwrap();
        let network = 1_793_000_000_000;
        service.observe_source(input(Some(100), network, 1000));
        service.observe_source(input(Some(101), network + 1, 1001));
        service.observe_source(input(Some(0), network + 2, 1002));
        service.observe_source(input(Some(101), network + 3, 1003));
        wait_samples(&service, 2);
        let done = finish(&service, &first);
        assert_eq!(done.rejected_samples, 2);
        let restarted = service.start_in_dir("KG1000", 60, dir.clone()).unwrap();
        service.observe_source(input(Some(0), network + 4, 1004));
        wait_samples(&service, 1);
        let done = finish(&service, &restarted);
        assert_eq!(done.rejected_samples, 0);
        let second = service.start_in_dir("KG50", 60, dir.clone()).unwrap();
        let mut selected = input(Some(0), network + 4, 1004);
        selected.sensor = "KG50";
        service.observe_source(selected);
        wait_samples(&service, 1);
        let done = finish(&service, &second);
        assert_eq!(done.rejected_samples, 0);
        // The existing physical-input switch invalidates the previous report;
        // a new capture for another KG50 connector starts its own watermark.
        service.reset_kg50_input();
        assert!(service.status().session_id.is_empty());
        let third = service.start_in_dir("KG50", 60, dir.clone()).unwrap();
        let mut selected = input(Some(0), network + 5, 1005);
        selected.sensor = "KG50";
        service.observe_source(selected);
        wait_samples(&service, 1);
        let done = finish(&service, &third);
        assert_eq!(done.rejected_samples, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
