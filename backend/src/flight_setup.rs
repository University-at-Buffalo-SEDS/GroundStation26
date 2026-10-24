use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) const MAX_SAVED_SETUP_BYTES: usize = 65_536;
const DEFAULT_FLIGHT_SETUP_PATH: &str = "config/flight_setup.json";
const LEGACY_FLIGHT_SETUP_PATH: &str = "data/flight_setup.json";
const FLIGHT_SETUP_WIRE_MAGIC: &[u8; 5] = b"GSFS1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KalmanFilterConstants {
    pub process_position_variance: f32,
    pub process_velocity_variance: f32,
    pub accel_variance: f32,
    pub baro_altitude_variance: f32,
    pub gps_altitude_variance: f32,
    pub gps_velocity_variance: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FlightProfileConfig {
    pub id: String,
    pub label: String,
    pub wind_level: u8,
    pub kalman: KalmanFilterConstants,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FlightSetupConfig {
    #[serde(default = "default_flight_setup_version")]
    pub version: u32,
    pub selected_profile_id: String,
    pub profiles: Vec<FlightProfileConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FlightSetupApplyEnvelope {
    pub version: u32,
    pub selected_profile_id: String,
    pub selected_profile_label: String,
    pub wind_level: u8,
    pub kalman: KalmanFilterConstants,
    pub generated_ms: u64,
}

fn default_flight_setup_version() -> u32 {
    1
}

fn default_profiles() -> Vec<FlightProfileConfig> {
    vec![
        FlightProfileConfig {
            id: "wind_1".to_string(),
            label: "Wind 1".to_string(),
            wind_level: 1,
            kalman: KalmanFilterConstants {
                process_position_variance: 0.08,
                process_velocity_variance: 0.12,
                accel_variance: 0.20,
                baro_altitude_variance: 0.55,
                gps_altitude_variance: 1.10,
                gps_velocity_variance: 0.85,
            },
        },
        FlightProfileConfig {
            id: "wind_2".to_string(),
            label: "Wind 2".to_string(),
            wind_level: 2,
            kalman: KalmanFilterConstants {
                process_position_variance: 0.10,
                process_velocity_variance: 0.15,
                accel_variance: 0.24,
                baro_altitude_variance: 0.65,
                gps_altitude_variance: 1.20,
                gps_velocity_variance: 0.95,
            },
        },
        FlightProfileConfig {
            id: "wind_3".to_string(),
            label: "Wind 3".to_string(),
            wind_level: 3,
            kalman: KalmanFilterConstants {
                process_position_variance: 0.12,
                process_velocity_variance: 0.18,
                accel_variance: 0.30,
                baro_altitude_variance: 0.78,
                gps_altitude_variance: 1.35,
                gps_velocity_variance: 1.10,
            },
        },
        FlightProfileConfig {
            id: "wind_4".to_string(),
            label: "Wind 4".to_string(),
            wind_level: 4,
            kalman: KalmanFilterConstants {
                process_position_variance: 0.16,
                process_velocity_variance: 0.23,
                accel_variance: 0.38,
                baro_altitude_variance: 0.92,
                gps_altitude_variance: 1.55,
                gps_velocity_variance: 1.28,
            },
        },
        FlightProfileConfig {
            id: "wind_5".to_string(),
            label: "Wind 5".to_string(),
            wind_level: 5,
            kalman: KalmanFilterConstants {
                process_position_variance: 0.21,
                process_velocity_variance: 0.30,
                accel_variance: 0.48,
                baro_altitude_variance: 1.08,
                gps_altitude_variance: 1.80,
                gps_velocity_variance: 1.46,
            },
        },
        FlightProfileConfig {
            id: "wind_6".to_string(),
            label: "Wind 6".to_string(),
            wind_level: 6,
            kalman: KalmanFilterConstants {
                process_position_variance: 0.27,
                process_velocity_variance: 0.38,
                accel_variance: 0.60,
                baro_altitude_variance: 1.28,
                gps_altitude_variance: 2.10,
                gps_velocity_variance: 1.68,
            },
        },
    ]
}

impl Default for FlightSetupConfig {
    fn default() -> Self {
        Self {
            version: default_flight_setup_version(),
            selected_profile_id: "wind_3".to_string(),
            profiles: default_profiles(),
        }
    }
}

pub fn config_path() -> PathBuf {
    std::env::var("GS_FLIGHT_SETUP_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(DEFAULT_FLIGHT_SETUP_PATH)
        })
}

fn legacy_config_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(LEGACY_FLIGHT_SETUP_PATH)
}

fn load_from_path(path: &Path) -> Result<FlightSetupConfig, String> {
    use std::io::Read;
    let file = fs::File::open(path).map_err(|e| e.to_string())?;
    if file.metadata().map_err(|e| e.to_string())?.len() > MAX_SAVED_SETUP_BYTES as u64 {
        return Err("Saved flight setup exceeds64KiB".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_SAVED_SETUP_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > MAX_SAVED_SETUP_BYTES {
        return Err("Saved flight setup exceeds64KiB".into());
    }
    let cfg = serde_json::from_slice::<FlightSetupConfig>(&bytes).map_err(|err| err.to_string())?;
    Ok(normalize(cfg))
}

fn normalize(mut cfg: FlightSetupConfig) -> FlightSetupConfig {
    cfg.profiles.sort_by_key(|profile| profile.wind_level);
    cfg.profiles.dedup_by(|a, b| a.id == b.id);
    if cfg.profiles.is_empty() {
        cfg.profiles = default_profiles();
    }
    if !cfg
        .profiles
        .iter()
        .any(|profile| profile.id == cfg.selected_profile_id)
    {
        cfg.selected_profile_id = cfg
            .profiles
            .first()
            .map(|profile| profile.id.clone())
            .unwrap_or_else(|| "wind_1".to_string());
    }
    cfg
}

/// Canonical saved form shared by the API and paired-settings adapter.
pub(crate) fn canonical_settings(cfg: FlightSetupConfig) -> FlightSetupConfig {
    normalize(cfg)
}

pub fn load_or_default() -> FlightSetupConfig {
    let _guard = SAVED_SETTINGS.lock().unwrap();
    load_or_default_unlocked()
}
fn load_or_default_unlocked() -> FlightSetupConfig {
    let path = config_path();
    match load_from_path(&path) {
        Ok(cfg) => cfg,
        Err(_) if !path.exists() => {
            let legacy_path = legacy_config_path();
            match load_from_path(&legacy_path) {
                Ok(cfg) => {
                    if let Err(err) = save_unlocked(&cfg) {
                        eprintln!(
                            "WARNING: loaded legacy flight setup config from {} but failed to migrate it to {}: {err}",
                            legacy_path.display(),
                            path.display()
                        );
                    }
                    cfg
                }
                Err(_) if !legacy_path.exists() => {
                    let cfg = FlightSetupConfig::default();
                    let _ = save_unlocked(&cfg);
                    cfg
                }
                Err(err) => {
                    eprintln!(
                        "WARNING: invalid legacy flight setup config at {}: {err}. Falling back to defaults.",
                        legacy_path.display()
                    );
                    let cfg = FlightSetupConfig::default();
                    let _ = save_unlocked(&cfg);
                    cfg
                }
            }
        }
        Err(err) => {
            eprintln!(
                "WARNING: invalid flight setup config at {}: {err}. Falling back to defaults.",
                path.display()
            );
            let cfg = FlightSetupConfig::default();
            cfg
        }
    }
}

static SAVED_SETTINGS: std::sync::Mutex<()> = std::sync::Mutex::new(());
pub(crate) fn snapshot_saved() -> Result<FlightSetupConfig, String> {
    let _guard = SAVED_SETTINGS.lock().unwrap();
    read_saved_unlocked()
}
fn read_saved_unlocked() -> Result<FlightSetupConfig, String> {
    let path = config_path();
    if path.exists() {
        load_from_path(&path)
    } else if legacy_config_path().exists() {
        load_from_path(&legacy_config_path())
    } else {
        Ok(FlightSetupConfig::default())
    }
}
pub(crate) fn compare_and_save(
    expected: &FlightSetupConfig,
    next: &FlightSetupConfig,
) -> Result<(), String> {
    let _guard = SAVED_SETTINGS.lock().unwrap();
    if read_saved_unlocked()? != *expected {
        return Err("Saved flight settings changed locally".into());
    }
    save_unlocked(next)
}
pub fn save(cfg: &FlightSetupConfig) -> Result<(), String> {
    let _guard = SAVED_SETTINGS.lock().unwrap();
    save_unlocked(cfg)
}
fn save_unlocked(cfg: &FlightSetupConfig) -> Result<(), String> {
    save_to_path(cfg, &config_path())
}
fn save_to_path(cfg: &FlightSetupConfig, path: &Path) -> Result<(), String> {
    let cfg = normalize(cfg.clone());
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let formatted = serde_json::to_string_pretty(&cfg).map_err(|err| err.to_string())?;
    if formatted.len() > MAX_SAVED_SETUP_BYTES {
        return Err("Saved flight setup exceeds64KiB".into());
    }
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, formatted).map_err(|err| err.to_string())?;
    fs::rename(temporary, path).map_err(|err| err.to_string())
}

pub fn selected_profile(cfg: &FlightSetupConfig) -> Option<&FlightProfileConfig> {
    cfg.profiles
        .iter()
        .find(|profile| profile.id == cfg.selected_profile_id)
}

pub fn build_apply_payload(cfg: &FlightSetupConfig, generated_ms: u64) -> Result<Vec<u8>, String> {
    let profile =
        selected_profile(cfg).ok_or_else(|| "selected flight profile missing".to_string())?;
    let envelope = FlightSetupApplyEnvelope {
        version: cfg.version,
        selected_profile_id: profile.id.clone(),
        selected_profile_label: profile.label.clone(),
        wind_level: profile.wind_level,
        kalman: profile.kalman.clone(),
        generated_ms,
    };
    let mut payload = FLIGHT_SETUP_WIRE_MAGIC.to_vec();
    payload.extend(serde_json::to_vec(&envelope).map_err(|err| err.to_string())?);
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_setup_has_selected_profile() {
        let cfg = FlightSetupConfig::default();
        assert!(selected_profile(&cfg).is_some());
        assert_eq!(cfg.profiles.len(), 6);
    }

    #[test]
    fn apply_payload_has_magic_prefix() {
        let cfg = FlightSetupConfig::default();
        let payload = build_apply_payload(&cfg, 123).expect("payload");
        assert!(payload.starts_with(FLIGHT_SETUP_WIRE_MAGIC));
    }

    #[test]
    fn default_config_path_uses_config_directory() {
        assert!(config_path().ends_with(DEFAULT_FLIGHT_SETUP_PATH));
    }
}

#[cfg(test)]
mod saved_mission_tests {
    use super::*;
    #[test]
    fn failed_atomic_save_preserves_prior_file_and_bounded_reads_reject_oversize() {
        let dir = std::env::temp_dir().join(format!("gs-saved-flight-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        let path = dir.join("saved.json");
        let original = FlightSetupConfig::default();
        save_to_path(&original, &path).unwrap();
        let bytes = fs::read(&path).unwrap();
        fs::create_dir(path.with_extension("json.tmp")).unwrap();
        let mut changed = original.clone();
        changed.selected_profile_id = "wind_2".into();
        assert!(save_to_path(&changed, &path).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(load_from_path(&path).unwrap(), original);
        let mut padded = bytes;
        padded.resize(MAX_SAVED_SETUP_BYTES, b' ');
        fs::write(&path, &padded).unwrap();
        assert!(load_from_path(&path).is_ok());
        padded.push(b' ');
        fs::write(&path, &padded).unwrap();
        assert!(load_from_path(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), padded);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn maximum_saved_setup_read_and_snapshot_cost_is_measured_with_bounded_bytes() {
        let mut cfg = FlightSetupConfig::default();
        let template = cfg.profiles[0].clone();
        cfg.profiles.clear();
        for n in 1..=200 {
            let mut profile = template.clone();
            profile.id = format!("{:080}", n);
            profile.label = "x".repeat(120);
            profile.wind_level = n as u8;
            cfg.profiles.push(profile);
            cfg.selected_profile_id = cfg.profiles[0].id.clone();
            if serde_json::to_vec_pretty(&cfg).unwrap().len() > MAX_SAVED_SETUP_BYTES {
                cfg.profiles.pop();
                break;
            }
        }
        let bytes = serde_json::to_vec_pretty(&cfg).unwrap();
        assert!(bytes.len() <= MAX_SAVED_SETUP_BYTES);
        assert!(bytes.len() > MAX_SAVED_SETUP_BYTES - 1024);
        let dir = std::env::temp_dir().join(format!("gs-flight-profile-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("saved.json");
        fs::write(&path, &bytes).unwrap();
        let start = std::time::Instant::now();
        for _ in 0..128 {
            std::hint::black_box(load_from_path(&path).unwrap());
        }
        eprintln!(
            "max_saved_flight: bytes={} profiles={} reads=128 elapsed_us={}",
            bytes.len(),
            cfg.profiles.len(),
            start.elapsed().as_micros()
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
#[cfg(test)]
mod saved_file_transaction_test {
    use super::*;
    #[test]
    #[ignore = "requires explicit isolated fake flight config path"]
    fn public_loader_waits_for_same_saved_file_transaction_and_cannot_migrate_stale_data() {
        let dir = PathBuf::from(
            std::env::var("GS_MISSION_ADAPTER_TEST_DIR")
                .expect("isolated fake settings directory required"),
        );
        assert!(config_path().starts_with(&dir));
        let original = FlightSetupConfig::default();
        save(&original).unwrap();
        let guard = SAVED_SETTINGS.lock().unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            result_tx.send(load_or_default()).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(
            result_rx
                .recv_timeout(std::time::Duration::from_millis(25))
                .is_err(),
            "public loader bypassed shared transaction lock"
        );
        let mut imported = original.clone();
        imported.selected_profile_id = "wind_1".into();
        save_unlocked(&imported).unwrap();
        drop(guard);
        assert_eq!(
            result_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
            imported
        );
        reader.join().unwrap();
        assert_eq!(snapshot_saved().unwrap(), imported);
    }
}
