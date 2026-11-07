use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_FILL_TARGETS_PATH: &str = "config/fill_targets.json";
const LEGACY_FILL_TARGETS_PATH: &str = "data/fill_targets.json";

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FillSource {
    #[default]
    Kg50,
    Kg1000Absolute,
}
impl FillSource {
    pub fn sensor(self) -> &'static str {
        match self {
            Self::Kg50 => "KG50",
            Self::Kg1000Absolute => "KG1000",
        }
    }
    pub fn mass(self, calibrated: f32) -> f32 {
        match self {
            Self::Kg50 => calibrated,
            Self::Kg1000Absolute => calibrated.abs(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FluidFillTarget {
    pub target_mass_kg: f32,
    pub target_pressure_psi: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FillTargetsConfig {
    #[serde(default)]
    pub fill_source: FillSource,
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
            fill_source: FillSource::default(),
            version: default_fill_targets_version(),
            nitrogen: FluidFillTarget {
                target_mass_kg: 0.5,
                target_pressure_psi: 500.0,
            },
            nitrous: FluidFillTarget {
                target_mass_kg: 8.0,
                target_pressure_psi: 750.0,
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
    cfg.nitrogen.target_mass_kg = normalize_mass_target(cfg.nitrogen.target_mass_kg);
    cfg.nitrous.target_mass_kg = normalize_mass_target(cfg.nitrous.target_mass_kg);
    cfg.nitrogen.target_pressure_psi = cfg.nitrogen.target_pressure_psi.max(0.0);
    cfg.nitrous.target_pressure_psi = cfg.nitrous.target_pressure_psi.max(0.0);
    cfg
}

fn normalize_mass_target(value: f32) -> f32 {
    if !value.is_finite() {
        return 0.01;
    }
    if value.abs() < 0.01 {
        if value.is_sign_negative() {
            -0.01
        } else {
            0.01
        }
    } else {
        value
    }
}

/// Canonical saved form shared by the API and paired-settings adapter.
pub(crate) fn canonical_settings(cfg: FillTargetsConfig) -> FillTargetsConfig {
    normalize(cfg)
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
    save_to_path(cfg, &config_path())
}

fn save_to_path(cfg: &FillTargetsConfig, path: &Path) -> Result<(), String> {
    let cfg = normalize(cfg.clone());
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let formatted = serde_json::to_string_pretty(&cfg).map_err(|err| err.to_string())?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, formatted).map_err(|err| err.to_string())?;
    fs::rename(temporary, path).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mass_and_pressure_targets_round_trip_on_disk() {
        let dir = std::env::temp_dir().join(format!(
            "seds-fill-target-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("fill_targets.json");
        let mut cfg = FillTargetsConfig::default();
        cfg.nitrogen.target_mass_kg = 4.2;
        cfg.nitrogen.target_pressure_psi = 550.;
        cfg.nitrous.target_mass_kg = 12.3;
        cfg.nitrous.target_pressure_psi = 650.;
        cfg.fill_source = FillSource::Kg1000Absolute;
        save_to_path(&cfg, &path).unwrap();
        let saved = fs::read(&path).unwrap();
        assert_eq!(load_from_path(&path).unwrap(), cfg);
        assert_eq!(
            fs::read(&path).unwrap(),
            saved,
            "loading preserves configured targets without migration"
        );
        assert!(!path.with_extension("json.tmp").exists());
        fs::remove_file(&path).unwrap();
        fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn fill_sources_keep_kg50_signed_and_map_only_kg1000_to_absolute() {
        assert_eq!(FillSource::Kg50.mass(-12.), -12.);
        assert_eq!(FillSource::Kg1000Absolute.mass(-12.), 12.);
        let old = r#"{"version":1,"nitrogen":{"target_mass_kg":1,"target_pressure_psi":2},"nitrous":{"target_mass_kg":3,"target_pressure_psi":4}}"#;
        assert_eq!(
            serde_json::from_str::<FillTargetsConfig>(old)
                .unwrap()
                .fill_source,
            FillSource::Kg50
        );
    }

    #[test]
    fn fresh_defaults_match_shipped_seed_and_keep_hardware_limits_explicit() {
        let defaults = FillTargetsConfig::default();
        let seed: FillTargetsConfig =
            serde_json::from_str(include_str!("../config/fill_targets.json")).unwrap();
        assert_eq!(seed, defaults);
        assert_eq!(
            (
                defaults.nitrogen.target_mass_kg,
                defaults.nitrogen.target_pressure_psi
            ),
            (0.5, 500.0)
        );
        assert_eq!(
            (
                defaults.nitrous.target_mass_kg,
                defaults.nitrous.target_pressure_psi
            ),
            (8.0, 750.0)
        );
        let sequence: gse_sequence::Config =
            serde_json::from_str(include_str!("../config/gse_sequence.json")).unwrap();
        assert_eq!(sequence.nitrogen_target_psi, 500.0);
        assert!(sequence.nitrogen_steps.is_empty());
        assert!(sequence.pressure_ceiling_psi.is_none());
        assert!(sequence.turbocharge_max_added_mass_kg.is_none());
        assert!(sequence.cold_flow_target_psi.is_none());
        assert_eq!(
            gse_sequence::Engine::default().nitrous_target_psi,
            defaults.nitrous.target_pressure_psi
        );
    }

    #[test]
    fn fresh_targets_require_explicit_turbo_limits_without_retargeting_nitrogen() {
        use gse_sequence::turbocharge::{Context, Controller};
        use gse_sequence::{Inputs, Sample};
        let targets = FillTargetsConfig::default();
        let mut config = gse_sequence::Config::default();
        let input = Inputs {
            now_ms: 100,
            pressure: Some(Sample {
                value: 100.0,
                at_ms: 100,
            }),
            valves: [None; 5],
            prelaunch: true,
            interlock: true,
        };
        let mass = Some(Sample {
            value: targets.nitrous.target_mass_kg,
            at_ms: 100,
        });
        let context = Context {
            reset_generation: 1,
            qualification_generation: 1,
            configuration: [0; 32],
        };
        let start = |config: &gse_sequence::Config| {
            Controller::start(
                config,
                targets.nitrous.target_mass_kg,
                targets.nitrous.target_pressure_psi,
                input,
                mass,
                context,
            )
        };
        assert!(
            start(&config).is_err(),
            "fluid targets are not hardware limits"
        );
        config.turbocharge_max_added_mass_kg = Some(0.3); // Explicit test-only allowance.
        assert!(
            start(&config).is_err(),
            "an allowance does not supply a hardware ceiling"
        );
        config.pressure_ceiling_psi = Some(750.0);
        assert!(
            start(&config).is_err(),
            "target must remain below the explicit ceiling"
        );
        config.pressure_ceiling_psi = Some(1000.0); // Explicit test-only ceiling.
        let (controller, effects) = start(&config).unwrap();
        let observation = controller.progress_observation();
        assert_eq!(observation.pressure_target_psi, 750.0);
        assert_eq!(observation.liquid_target_mass_kg, 8.0);
        assert_eq!(targets.nitrogen.target_pressure_psi, 500.0);
        assert!(effects.valves.iter().all(|(_, open)| !open));
        assert!(
            !effects.valves.is_empty(),
            "initial plan closes outputs before nitrogen admission"
        );
    }

    #[test]
    fn defaults_are_nonzero() {
        let cfg = FillTargetsConfig::default();
        assert!(cfg.nitrogen.target_mass_kg.abs() > 0.0);
        assert!(cfg.nitrous.target_mass_kg.abs() > 0.0);
    }

    #[test]
    fn default_config_path_uses_config_directory() {
        assert!(config_path().ends_with(DEFAULT_FILL_TARGETS_PATH));
    }
}
