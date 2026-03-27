use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::comms::{COMMS_BAUD_RATE, ROCKET_COMMS_PORT, UMBILICAL_COMMS_PORT};

const DEFAULT_CONFIG_PATH: &str = "comms/comms.json";
const LEGACY_CONFIG_PATHS: &[&str] = &[
    "comms/coms.json",
    "comms/radio_links.json",
    "data/radio_links.json",
    "../comms/radio_links.json",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SerialLinkConfig {
    pub port: String,
    #[serde(default = "default_baud_rate")]
    pub baud_rate: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpiLinkConfig {
    pub port: String,
    #[serde(default = "default_spi_speed_hz")]
    pub spi_speed_hz: u32,
    #[serde(default = "default_spi_mode")]
    pub spi_mode: u8,
    #[serde(default = "default_spi_bits_per_word")]
    pub spi_bits_per_word: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CanLinkConfig {
    pub port: String,
    #[serde(default = "default_can_tx_id")]
    pub can_tx_id: u32,
    #[serde(default = "default_can_rx_id")]
    pub can_rx_id: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct I2cLinkConfig {
    #[serde(default = "default_i2c_bus")]
    pub bus: u8,
    #[serde(default = "default_i2c_addr")]
    pub addr: u16,
    #[serde(default = "default_i2c_chunk_delay_ms")]
    pub chunk_delay_ms: u64,
    #[serde(default = "default_i2c_initial_wait_ms")]
    pub initial_wait_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "interface", rename_all = "snake_case")]
pub enum CommsLinkConfig {
    UsbSerial {
        #[serde(flatten)]
        serial: SerialLinkConfig,
    },
    RaspberryPiGpioUart {
        #[serde(flatten)]
        serial: SerialLinkConfig,
    },
    CustomSerial {
        #[serde(flatten)]
        serial: SerialLinkConfig,
    },
    Spi {
        #[serde(flatten)]
        spi: SpiLinkConfig,
    },
    Can {
        #[serde(flatten)]
        can: CanLinkConfig,
    },
    I2c {
        #[serde(flatten)]
        i2c: I2cLinkConfig,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommsLinksConfig {
    #[serde(default = "default_config_version")]
    pub version: u32,
    pub av_bay: CommsLinkConfig,
    pub fill_box: CommsLinkConfig,
}

fn default_config_version() -> u32 {
    1
}

fn default_baud_rate() -> usize {
    COMMS_BAUD_RATE
}

fn default_spi_speed_hz() -> u32 {
    1_000_000
}

fn default_spi_mode() -> u8 {
    0
}

fn default_spi_bits_per_word() -> u8 {
    8
}

fn default_can_tx_id() -> u32 {
    0x120
}

fn default_can_rx_id() -> u32 {
    0x220
}

fn default_i2c_bus() -> u8 {
    1
}

fn default_i2c_addr() -> u16 {
    0x55
}

fn default_i2c_chunk_delay_ms() -> u64 {
    1
}

fn default_i2c_initial_wait_ms() -> u64 {
    10
}

impl Default for CommsLinksConfig {
    fn default() -> Self {
        Self {
            version: default_config_version(),
            av_bay: CommsLinkConfig::UsbSerial {
                serial: SerialLinkConfig {
                    port: ROCKET_COMMS_PORT.to_string(),
                    baud_rate: default_baud_rate(),
                },
            },
            fill_box: CommsLinkConfig::UsbSerial {
                serial: SerialLinkConfig {
                    port: UMBILICAL_COMMS_PORT.to_string(),
                    baud_rate: default_baud_rate(),
                },
            },
        }
    }
}

#[cfg(test)]
impl CommsLinkConfig {
    pub fn port(&self) -> &str {
        match self {
            Self::UsbSerial { serial }
            | Self::RaspberryPiGpioUart { serial }
            | Self::CustomSerial { serial } => &serial.port,
            Self::Spi { spi } => &spi.port,
            Self::Can { can } => &can.port,
            Self::I2c { .. } => "/dev/i2c-*",
        }
    }
}

pub fn config_path() -> PathBuf {
    std::env::var("GS_COMMS_LINK_CONFIG")
        .or_else(|_| std::env::var("GS_RADIO_LINK_CONFIG"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(DEFAULT_CONFIG_PATH))
}

pub fn load_or_default() -> CommsLinksConfig {
    let path = config_path();
    migrate_legacy_config(&path);
    match fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<CommsLinksConfig>(&raw) {
            Ok(cfg) => cfg,
            Err(err) => {
                eprintln!(
                    "WARNING: invalid comms link config at {}: {err}. Falling back to defaults.",
                    path.display()
                );
                CommsLinksConfig::default()
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let cfg = CommsLinksConfig::default();
            if let Err(write_err) = save(&cfg) {
                eprintln!(
                    "WARNING: failed to write default comms link config {}: {write_err}",
                    path.display()
                );
            }
            cfg
        }
        Err(err) => {
            eprintln!(
                "WARNING: failed to read comms link config {}: {err}. Falling back to defaults.",
                path.display()
            );
            CommsLinksConfig::default()
        }
    }
}

pub fn save(cfg: &CommsLinksConfig) -> Result<(), String> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("create config directory {}: {err}", parent.display()))?;
    }
    let raw = serde_json::to_string_pretty(cfg)
        .map_err(|err| format!("serialize comms link config: {err}"))?;
    fs::write(&path, raw)
        .map_err(|err| format!("write comms link config {}: {err}", path.display()))
}

fn migrate_legacy_config(target_path: &PathBuf) {
    if std::env::var_os("GS_COMMS_LINK_CONFIG").is_some()
        || std::env::var_os("GS_RADIO_LINK_CONFIG").is_some()
        || target_path.exists()
    {
        return;
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for legacy_rel_path in LEGACY_CONFIG_PATHS {
        let legacy_path = manifest_dir.join(legacy_rel_path);
        if !legacy_path.exists() {
            continue;
        }
        if let Some(parent) = target_path.parent()
            && let Err(err) = fs::create_dir_all(parent)
        {
            eprintln!(
                "WARNING: failed to create comms config directory {}: {err}",
                parent.display()
            );
            return;
        }
        if let Err(err) = fs::rename(&legacy_path, target_path) {
            if let Err(copy_err) = fs::copy(&legacy_path, target_path) {
                eprintln!(
                    "WARNING: failed to migrate comms link config {} -> {}: {err}; copy fallback failed: {copy_err}",
                    legacy_path.display(),
                    target_path.display()
                );
                return;
            }
        }
        return;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CanLinkConfig, CommsLinkConfig, CommsLinksConfig, I2cLinkConfig, SerialLinkConfig,
        SpiLinkConfig,
    };

    #[test]
    fn default_config_matches_legacy_ports() {
        let cfg = CommsLinksConfig::default();
        assert_eq!(cfg.version, 1);
        assert_eq!(
            cfg.av_bay,
            CommsLinkConfig::UsbSerial {
                serial: SerialLinkConfig {
                    port: "/dev/ttyUSB1".to_string(),
                    baud_rate: 57_600,
                },
            }
        );
        assert_eq!(
            cfg.fill_box,
            CommsLinkConfig::UsbSerial {
                serial: SerialLinkConfig {
                    port: "/dev/ttyUSB2".to_string(),
                    baud_rate: 57_600,
                },
            }
        );
    }

    #[test]
    fn non_serial_variants_are_typed() {
        let spi = CommsLinkConfig::Spi {
            spi: SpiLinkConfig {
                port: "/dev/spidev0.0".to_string(),
                spi_speed_hz: 1_000_000,
                spi_mode: 0,
                spi_bits_per_word: 8,
            },
        };
        let can = CommsLinkConfig::Can {
            can: CanLinkConfig {
                port: "can0".to_string(),
                can_tx_id: 0x121,
                can_rx_id: 0x221,
            },
        };
        let i2c = CommsLinkConfig::I2c {
            i2c: I2cLinkConfig {
                bus: 1,
                addr: 0x55,
                chunk_delay_ms: 1,
                initial_wait_ms: 10,
            },
        };

        assert_eq!(spi.port(), "/dev/spidev0.0");
        assert_eq!(can.port(), "can0");
        assert_eq!(i2c.port(), "/dev/i2c-*");
    }
}
