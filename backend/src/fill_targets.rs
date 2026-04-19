use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_FILL_TARGETS_PATH: &str = "config/fill_targets.json";
const LEGACY_FILL_TARGETS_PATH: &str = "data/fill_targets.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FluidFillTarget {
    pub target_mass_kg: f32,
    pub target_pressure_psi: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FillTargetsConfig {
    #[serde(default = "default_fill_targets_version")]
    pub version: u32,
    pub nitrogen: FluidFillTarget,
    pub nitrous: FluidFillTarget,
}

fn default_fill_targets_version() -> u32 {
    1
}

impl Default for FillTargetsConfig {
    fn default() -> Self {
        Self {
            version: default_fill_targets_version(),
            nitrogen: FluidFillTarget {
                target_mass_kg: 10.0,
                target_pressure_psi: 120.0,
            },
            nitrous: FluidFillTarget {
                target_mass_kg: 10.0,
                target_pressure_psi: 745.0,
            },
        }
    }
}

pub fn config_path() -> PathBuf {
    std::env::var("GS_FILL_TARGETS_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(DEFAULT_FILL_TARGETS_PATH)
        })
}

fn legacy_config_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(LEGACY_FILL_TARGETS_PATH)
}

fn load_from_path(path: &Path) -> Result<FillTargetsConfig, String> {
    let raw = fs::read_to_string(path).map_err(|err| err.to_string())?;
    let cfg = serde_json::from_str::<FillTargetsConfig>(&raw).map_err(|err| err.to_string())?;
    Ok(normalize(cfg))
}

fn normalize(mut cfg: FillTargetsConfig) -> FillTargetsConfig {
    cfg.nitrogen.target_mass_kg = cfg.nitrogen.target_mass_kg.max(0.01);
    cfg.nitrous.target_mass_kg = cfg.nitrous.target_mass_kg.max(0.01);
    cfg.nitrogen.target_pressure_psi = cfg.nitrogen.target_pressure_psi.max(0.0);
    cfg.nitrous.target_pressure_psi = cfg.nitrous.target_pressure_psi.max(0.0);
    cfg
}

pub fn load_or_default() -> FillTargetsConfig {
    let path = config_path();
    match load_from_path(&path) {
        Ok(cfg) => cfg,
        Err(_) if !path.exists() => {
            let legacy_path = legacy_config_path();
            match load_from_path(&legacy_path) {
                Ok(cfg) => {
                    if let Err(err) = save(&cfg) {
                        eprintln!(
                            "WARNING: loaded legacy fill targets config from {} but failed to migrate it to {}: {err}",
                            legacy_path.display(),
                            path.display()
                        );
                    }
                    cfg
                }
                Err(_) if !legacy_path.exists() => {
                    let cfg = FillTargetsConfig::default();
                    let _ = save(&cfg);
                    cfg
                }
                Err(err) => {
                    eprintln!(
                        "WARNING: invalid legacy fill targets config at {}: {err}. Falling back to defaults.",
                        legacy_path.display()
                    );
                    let cfg = FillTargetsConfig::default();
                    let _ = save(&cfg);
                    cfg
                }
            }
        }
        Err(err) => {
            eprintln!(
                "WARNING: invalid fill targets config at {}: {err}. Falling back to defaults.",
                path.display()
            );
            let cfg = FillTargetsConfig::default();
            let _ = save(&cfg);
            cfg
        }
    }
}

pub fn save(cfg: &FillTargetsConfig) -> Result<(), String> {
    let cfg = normalize(cfg.clone());
    let path = config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let formatted = serde_json::to_string_pretty(&cfg).map_err(|err| err.to_string())?;
    fs::write(path, formatted).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_positive() {
        let cfg = FillTargetsConfig::default();
        assert!(cfg.nitrogen.target_mass_kg > 0.0);
        assert!(cfg.nitrous.target_mass_kg > 0.0);
    }

    #[test]
    fn default_config_path_uses_config_directory() {
        assert!(config_path().ends_with(DEFAULT_FILL_TARGETS_PATH));
    }
}
