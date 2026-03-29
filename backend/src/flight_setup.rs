use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

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
    let raw = fs::read_to_string(path).map_err(|err| err.to_string())?;
    let cfg = serde_json::from_str::<FlightSetupConfig>(&raw).map_err(|err| err.to_string())?;
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

pub fn load_or_default() -> FlightSetupConfig {
    let path = config_path();
    match load_from_path(&path) {
        Ok(cfg) => cfg,
        Err(_) if !path.exists() => {
            let legacy_path = legacy_config_path();
            match load_from_path(&legacy_path) {
                Ok(cfg) => {
                    if let Err(err) = save(&cfg) {
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
                    let _ = save(&cfg);
                    cfg
                }
                Err(err) => {
                    eprintln!(
                        "WARNING: invalid legacy flight setup config at {}: {err}. Falling back to defaults.",
                        legacy_path.display()
                    );
                    let cfg = FlightSetupConfig::default();
                    let _ = save(&cfg);
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
            let _ = save(&cfg);
            cfg
        }
    }
}

pub fn save(cfg: &FlightSetupConfig) -> Result<(), String> {
    let cfg = normalize(cfg.clone());
    let path = config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let formatted = serde_json::to_string_pretty(&cfg).map_err(|err| err.to_string())?;
    fs::write(path, formatted).map_err(|err| err.to_string())
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
