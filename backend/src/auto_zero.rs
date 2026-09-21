//! Bounded, monotonic-clock tare capture. Never blocks the telemetry receiver.
use crate::{loadcell, state::AppState, types::TelemetryCommand};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

const COUNT: usize = 200;
const WINDOW: Duration = Duration::from_secs(5);
const ZERO_KG: f32 = 0.8;

#[derive(Clone, Copy, Serialize, Deserialize)]
pub struct Settings {
    pub enabled: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Clone, Copy)]
struct Sample {
    at: Instant,
    raw: f32,
}
#[derive(Default)]
pub struct Runtime {
    settings: Option<Settings>,
    kg50: VecDeque<Sample>,
    kg1000: VecDeque<Sample>,
    generation: u64,
    pending: bool,
    fill_pending: bool,
}
fn path() -> std::path::PathBuf {
    std::env::var_os("GS_AUTO_ZERO_CONFIG")
        .map(Into::into)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/auto_zero.json")
        })
}
impl Runtime {
    pub fn pending(&self) -> bool {
        self.pending
    }
    pub fn settings(&mut self) -> Settings {
        *self
            .settings
            .get_or_insert_with(|| match std::fs::read(path()) {
                Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                    log::error!("Invalid automatic-zero settings: {e}; automatic zero disabled");
                    Settings { enabled: false }
                }),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
                Err(e) => {
                    log::error!("Reading automatic-zero settings: {e}");
                    Settings { enabled: false }
                }
            })
    }
    pub fn configure(&mut self, settings: Settings) -> Result<(), String> {
        self.configure_at(settings, &path())
    }

    fn configure_at(&mut self, settings: Settings, path: &std::path::Path) -> Result<(), String> {
        if self.pending {
            return Err("Wait for automatic zeroing to finish or cancel the action first".into());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec(&settings).unwrap())
            .map_err(|e| e.to_string())?;
        std::fs::rename(temporary, path).map_err(|e| e.to_string())?;
        self.settings = Some(settings);
        Ok(())
    }
    pub fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.pending = false;
    }
    pub fn cancel_for_command(&mut self, command: &TelemetryCommand) {
        if (self.pending && self.fill_pending && !matches!(command, TelemetryCommand::StartFill))
            || matches!(
                command,
                TelemetryCommand::Abort
                    | TelemetryCommand::CancelFill
                    | TelemetryCommand::PauseFill
            )
        {
            self.cancel();
        }
    }
    #[cfg(test)]
    pub fn testing(enabled: bool) -> Self {
        Self {
            settings: Some(Settings { enabled }),
            ..Self::default()
        }
    }
    pub fn observe(&mut self, sensor: &str, raw: f32) {
        if !raw.is_finite() {
            return;
        }
        let samples = match sensor {
            "KG50" => &mut self.kg50,
            "KG1000" => &mut self.kg1000,
            _ => return,
        };
        samples.push_back(Sample {
            at: Instant::now(),
            raw,
        });
        while samples.len() > COUNT {
            samples.pop_front();
        }
    }
    fn samples(&self, sensor: &str, after: Instant) -> Vec<f32> {
        let samples = if sensor == "KG50" {
            &self.kg50
        } else {
            &self.kg1000
        };
        samples
            .iter()
            .filter(|v| v.at >= after)
            .map(|v| v.raw)
            .collect()
    }
}

/// Trim at most 5% on each tail. Require the remaining 90% to be within a
/// 0.8 kg-wide band; do not mistake sustained motion for isolated outliers.
fn stable(
    raw: &[f32],
    cfg: &loadcell::LoadcellCalibrationFile,
    sensor: &str,
) -> Result<(f32, bool), String> {
    if raw.len() != COUNT {
        return Err("Need 200 fresh loadcell samples".into());
    }
    if sensor == "KG50" && !cfg.extra_channels.contains_key("kg50") {
        return Err("Save a 50 kg loadcell calibration before automatic zeroing".into());
    }
    let mut values: Vec<(f32, f32)> = raw
        .iter()
        .map(|raw| {
            loadcell::calibrated_weight_kg(cfg, sensor, *raw)
                .filter(|kg| kg.is_finite())
                .map(|kg| (kg, *raw))
                .ok_or_else(|| "Loadcell calibration is invalid".to_string())
        })
        .collect::<Result<_, _>>()?;
    values.sort_by(|a, b| a.0.total_cmp(&b.0));
    let kept = &values[COUNT / 20..COUNT - COUNT / 20];
    if kept.last().unwrap().0 - kept[0].0 > ZERO_KG {
        return Err("Loadcell is moving or unstable; no automatic zero was applied".into());
    }
    let mean_kg = kept.iter().map(|v| v.0 as f64).sum::<f64>() / kept.len() as f64;
    if mean_kg.abs() > ZERO_KG as f64 {
        return Err(
            "Load exceeds the ±0.8 kg automatic-zero limit; inspect the load and tare manually"
                .into(),
        );
    }
    Ok((
        (kept.iter().map(|v| v.1 as f64).sum::<f64>() / kept.len() as f64) as f32,
        kept.iter().all(|v| v.0.abs() <= ZERO_KG),
    ))
}

/// Returns true when Start Fill was intercepted. Resume never erases fill mass.
pub fn before_fill(state: &Arc<AppState>, cmd: &TelemetryCommand) -> bool {
    if !matches!(cmd, TelemetryCommand::StartFill) || crate::gse::active(state) {
        return false;
    }
    begin(
        state,
        state.fill_targets_snapshot().fill_source.sensor(),
        true,
    )
}

/// Starts concurrently with launch dispatch, never adding to countdown time.
pub fn launch(state: &Arc<AppState>) {
    begin(state, "KG1000", false);
}

fn begin(state: &Arc<AppState>, sensor: &'static str, fill: bool) -> bool {
    let cfg = state.loadcell_calibration.lock().unwrap().clone();
    let start = Instant::now();
    let token = {
        let mut rt = state.auto_zero.lock().unwrap();
        if !rt.settings().enabled {
            return false;
        }
        if rt.pending {
            return true;
        }
        if fill {
            let recent = rt.samples(sensor, start - WINDOW);
            // Require a live stream, not merely a recently stopped sensor.
            let history = if sensor == "KG50" {
                &rt.kg50
            } else {
                &rt.kg1000
            };
            let fresh = history
                .back()
                .is_some_and(|v| start.duration_since(v.at) < Duration::from_millis(250));
            if fresh && matches!(stable(&recent, &cfg, sensor), Ok((_, true))) {
                return false;
            }
        }
        rt.cancel();
        rt.pending = true;
        rt.fill_pending = fill;
        rt.generation
    };
    state.add_notification_with_persistence(
        format!("Collecting 200 fresh {sensor} samples for automatic zero"),
        false,
    );
    let state = state.clone();
    tokio::spawn(async move {
        let result = loop {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let raw = {
                let rt = state.auto_zero.lock().unwrap();
                if rt.generation != token {
                    return;
                }
                rt.samples(sensor, start)
            };
            // Never admit ignition/countdown samples after the idle window.
            if start.elapsed() >= WINDOW {
                break Err(
                    "200 stable samples were not captured within the first five seconds"
                        .to_string(),
                );
            }
            if raw.len() == COUNT {
                break stable(&raw, &cfg, sensor).map(|v| v.0);
            }
        };
        let result = result.and_then(|raw| {
            let runtime = state.auto_zero.lock().unwrap();
            if runtime.generation != token {
                return Err("Automatic zero cancelled".into());
            }
            let mut current = state.loadcell_calibration.lock().unwrap();
            if serde_json::to_value(&*current).ok() != serde_json::to_value(&cfg).ok() {
                return Err("Calibration changed during automatic zero; retry".into());
            }
            let mut updated = current.clone();
            loadcell::capture_zero(
                &mut updated,
                if sensor == "KG50" { "kg50" } else { "ch1" },
                raw,
            );
            loadcell::save(&updated)?;
            *current = updated;
            Ok(())
        });
        {
            let mut rt = state.auto_zero.lock().unwrap();
            if rt.generation != token {
                return;
            }
            rt.pending = false;
        }
        match result {
            Ok(()) => {
                state.broadcast_fill_targets_snapshot();
                let cfg = state.loadcell_calibration.lock().unwrap().clone();
                if let Some(router) = state.topology_router.get() {
                    if let Err(e) = crate::network_variables::set_daq_calibration(router, &cfg) {
                        state.add_notification(format!(
                            "Zero saved locally, but DAQ calibration sync failed: {e}"
                        ));
                    }
                }
                state.add_notification_with_persistence(
                    format!("{sensor} automatic zero saved"),
                    false,
                );
                if fill && state.is_command_allowed(&TelemetryCommand::StartFill) {
                    // Refresh the fill controller with the newly zeroed mass before start.
                    state.gse.lock().unwrap().observe_mass(Some(0.0));
                    *state.latest_fill_mass_kg.lock().unwrap() = Some(0.0);
                    crate::gse::handle_command(&state, &TelemetryCommand::StartFill);
                }
            }
            Err(e) => {
                state.add_notification(format!(
                    "Automatic zero failed: {e}. {}",
                    if fill {
                        "Fill was not started."
                    } else {
                        "Launch abort requested."
                    }
                ));
                if !fill {
                    let _ = state.cmd_tx.send(TelemetryCommand::Abort).await;
                }
            }
        }
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> loadcell::LoadcellCalibrationFile {
        let mut cfg = loadcell::LoadcellCalibrationFile::default();
        cfg.ch1.m = Some(1.0);
        cfg.ch1.b = Some(0.0);
        cfg.ch1_zero_raw = None;
        cfg
    }
    #[test]
    fn zero_band_ignores_isolated_outliers() {
        let mut raw = vec![0.2; COUNT];
        raw[0] = 900.;
        raw[1] = -900.;
        let (mean, zero) = stable(&raw, &config(), "KG1000").unwrap();
        assert!(zero);
        assert!((mean - 0.2).abs() < 0.001);
    }
    #[test]
    fn movement_load_and_missing_samples_are_rejected() {
        assert!(stable(&vec![2.; COUNT], &config(), "KG1000").is_err());
        assert!(stable(&vec![0.; COUNT - 1], &config(), "KG1000").is_err());
        let raw: Vec<_> = (0..COUNT)
            .map(|i| if i % 2 == 0 { -0.7 } else { 0.7 })
            .collect();
        assert!(stable(&raw, &config(), "KG1000").is_err());
    }
    #[test]
    fn history_is_bounded_and_capture_excludes_old_samples() {
        let mut rt = Runtime::default();
        for _ in 0..500 {
            rt.observe("KG1000", 0.0);
        }
        assert_eq!(rt.kg1000.len(), COUNT);
        assert!(rt.samples("KG1000", Instant::now()).is_empty());
        rt.pending = true;
        let old = rt.generation;
        rt.cancel();
        assert!(!rt.pending);
        assert_ne!(old, rt.generation);
    }
    #[test]
    fn manual_actions_cancel_a_pending_fill_but_duplicate_start_does_not() {
        let mut rt = Runtime::testing(true);
        rt.pending = true;
        rt.fill_pending = true;
        rt.cancel_for_command(&TelemetryCommand::StartFill);
        assert!(rt.pending);
        rt.cancel_for_command(&TelemetryCommand::Dump);
        assert!(!rt.pending);
        rt.pending = true;
        rt.fill_pending = false;
        rt.cancel_for_command(&TelemetryCommand::Abort);
        assert!(!rt.pending);
    }
    #[test]
    fn zero_band_boundaries_and_sustained_outliers() {
        assert!(stable(&vec![0.8; COUNT], &config(), "KG1000").unwrap().1);
        assert!(stable(&vec![-0.8; COUNT], &config(), "KG1000").unwrap().1);
        let mut values = vec![0.0; COUNT];
        values[..20].fill(2.0);
        assert!(stable(&values, &config(), "KG1000").is_err());
        assert!(stable(&values, &config(), "KG50").is_err());
    }

    #[test]
    fn toggle_is_written_to_disk_and_pending_capture_cannot_change_it() {
        let dir = std::env::temp_dir().join(format!(
            "seds-auto-zero-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("auto_zero.json");
        let mut rt = Runtime::testing(true);
        rt.configure_at(Settings { enabled: false }, &path).unwrap();
        let loaded: Settings = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(!loaded.enabled);
        rt.pending = true;
        assert!(rt.configure_at(Settings { enabled: true }, &path).is_err());
        let loaded: Settings = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(!loaded.enabled);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }
}
