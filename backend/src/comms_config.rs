use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::comms::{COMMS_BAUD_RATE, ROCKET_COMMS_PORT};

const DEFAULT_CONFIG_PATH: &str = "comms/comms.json";
const RRLS_CONFIG_PATH: &str = "comms/rrls.json";
const LEGACY_CONFIG_PATHS: &[&str] = &[
    "comms/coms.json",
    "comms/radio_links.json",
    "data/radio_links.json",
    "../comms/radio_links.json",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SerialProtocol {
    PacketFramed,
    RawUart,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SerialLinkConfig {
    pub port: String,
    #[serde(default = "default_baud_rate")]
    pub baud_rate: usize,
    #[serde(default = "default_serial_protocol")]
    pub protocol: SerialProtocol,
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
    /// 1 = legacy slots; 2 = complete packets (requires updated Pico firmware).
    #[serde(default = "default_i2c_protocol_version")]
    pub protocol_version: u8,
    #[serde(default = "default_i2c_bus")]
    pub bus: u8,
    #[serde(default = "default_i2c_addr")]
    pub addr: u16,
    #[serde(default = "default_i2c_chunk_delay_ms")]
    pub chunk_delay_ms: u64,
    #[serde(default = "default_i2c_initial_wait_ms")]
    pub initial_wait_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TcpMode {
    Client,
    Server,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PicoFiTcpConfig {
    /// Find a compatible peer on the Ethernet LAN instead of pinning its IP.
    #[serde(default)]
    pub discovery: bool,
    #[serde(default = "default_discovery_port")]
    pub discovery_port: u16,
    #[serde(default)]
    pub discovery_interface: Option<String>,
    pub mode: TcpMode,
    /// Numeric IP:port, using the Pi's existing Ethernet interface.
    pub address: String,
    /// Optional discovery IP pin; required for manual non-loopback listeners.
    #[serde(default)]
    pub allowed_peer: Option<std::net::IpAddr>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "interface", rename_all = "snake_case")]
pub enum CommsLinkConfig {
    PicoFiTcp {
        #[serde(flatten)]
        tcp: PicoFiTcpConfig,
    },
    #[serde(alias = "usb_serial")]
    Serial {
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

#[derive(Serialize, Deserialize)]
struct RecoveryCommsConfig {
    #[serde(default = "default_config_version")]
    version: u32,
    av_bay: CommsLinkConfig,
}
fn parse_config(raw: &str, recovery: bool) -> Result<CommsLinksConfig, serde_json::Error> {
    if recovery {
        let radio: RecoveryCommsConfig = serde_json::from_str(raw)?;
        let mut config = default_for_role(true);
        config.version = radio.version;
        config.av_bay = radio.av_bay;
        Ok(config)
    } else {
        serde_json::from_str(raw)
    }
}
fn serialize_config(
    config: &CommsLinksConfig,
    recovery: bool,
) -> Result<String, serde_json::Error> {
    if recovery {
        serde_json::to_string_pretty(&RecoveryCommsConfig {
            version: config.version,
            av_bay: config.av_bay.clone(),
        })
    } else {
        serde_json::to_string_pretty(config)
    }
}

fn default_i2c_protocol_version() -> u8 {
    1
}

fn default_config_version() -> u32 {
    1
}

fn default_baud_rate() -> usize {
    COMMS_BAUD_RATE
}

fn default_av_bay_baud_rate() -> usize {
    115_200
}

fn default_serial_protocol() -> SerialProtocol {
    SerialProtocol::PacketFramed
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

fn default_discovery_port() -> u16 {
    49361
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
    0
}

fn default_i2c_initial_wait_ms() -> u64 {
    0
}

impl Default for CommsLinksConfig {
    fn default() -> Self {
        Self {
            version: default_config_version(),
            av_bay: CommsLinkConfig::Serial {
                serial: SerialLinkConfig {
                    port: ROCKET_COMMS_PORT.to_string(),
                    baud_rate: default_av_bay_baud_rate(),
                    protocol: SerialProtocol::RawUart,
                },
            },
            fill_box: CommsLinkConfig::PicoFiTcp {
                tcp: PicoFiTcpConfig {
                    discovery: true,
                    discovery_port: default_discovery_port(),
                    discovery_interface: None,
                    mode: TcpMode::Server,
                    address: "0.0.0.0:0".to_owned(),
                    allowed_peer: None,
                },
            },
        }
    }
}

#[cfg(test)]
impl CommsLinkConfig {
    pub fn port(&self) -> &str {
        match self {
            Self::Serial { serial }
            | Self::RaspberryPiGpioUart { serial }
            | Self::CustomSerial { serial } => &serial.port,
            Self::Spi { spi } => &spi.port,
            Self::Can { can } => &can.port,
            Self::I2c { .. } => "/dev/i2c-*",
            Self::PicoFiTcp { tcp } => &tcp.address,
        }
    }
}

fn selected_config_path(
    rrls: bool,
    rrls_override: Option<PathBuf>,
    common_override: Option<PathBuf>,
) -> PathBuf {
    (if rrls { rrls_override } else { None })
        .or(common_override)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(if rrls {
                RRLS_CONFIG_PATH
            } else {
                DEFAULT_CONFIG_PATH
            })
        })
}
pub fn config_path() -> PathBuf {
    selected_config_path(
        crate::rrls::is_rrls(),
        std::env::var_os("GS_RRLS_COMMS_LINK_CONFIG").map(PathBuf::from),
        std::env::var_os("GS_COMMS_LINK_CONFIG")
            .or_else(|| std::env::var_os("GS_RADIO_LINK_CONFIG"))
            .map(PathBuf::from),
    )
}
fn default_for_role(rrls: bool) -> CommsLinksConfig {
    let mut config = CommsLinksConfig::default();
    if rrls {
        config.fill_box = CommsLinkConfig::Serial {
            serial: SerialLinkConfig {
                port: "unused".into(),
                baud_rate: 115200,
                protocol: SerialProtocol::RawUart,
            },
        };
        if cfg!(target_os = "macos") {
            if let CommsLinkConfig::Serial { serial } = &mut config.av_bay {
                serial.port = "/dev/cu.RRLS-radio".into();
            }
        }
    }
    config
}

pub fn load_or_default() -> CommsLinksConfig {
    let path = config_path();
    migrate_legacy_config(&path);
    let mut config = match fs::read_to_string(&path) {
        Ok(raw) => {
            let raw = migrate_serial_protocol_defaults(raw, &path);
            match parse_config(&raw, crate::rrls::is_rrls()) {
                Ok(cfg) => cfg,
                Err(err) => {
                    eprintln!(
                        "WARNING: invalid comms link config at {}: {err}. Falling back to defaults.",
                        path.display()
                    );
                    default_for_role(crate::rrls::is_rrls())
                }
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let cfg = default_for_role(crate::rrls::is_rrls());
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
            default_for_role(crate::rrls::is_rrls())
        }
    };
    if crate::rrls::is_rrls() {
        if let Ok(port) = std::env::var("GS_RRLS_RADIO_PORT") {
            if !port.trim().is_empty() {
                apply_simulator_serial_override(&mut config.av_bay, port, 115200);
                if let Err(error) = save(&config) {
                    eprintln!("WARNING: cannot save RRLS radio settings: {error}");
                }
            }
        }
    }
    apply_simulator_serial_overrides(&mut config);
    log::info!(
        "{} communications config: {}; radio: {}",
        if crate::rrls::is_rrls() {
            "RRLS"
        } else {
            "GroundStation"
        },
        path.display(),
        crate::comms::link_description(&config.av_bay)
    );
    config
}

fn apply_simulator_serial_overrides(config: &mut CommsLinksConfig) {
    if std::env::var("GS_SIMULATED_SERIAL_PTY").as_deref() == Ok("1") {
        if let Ok(address) = std::env::var("GS_FILL_PICO_TCP_ADDRESS") {
            config.fill_box = CommsLinkConfig::PicoFiTcp {
                tcp: PicoFiTcpConfig {
                    discovery: false,
                    discovery_port: default_discovery_port(),
                    discovery_interface: None,
                    mode: TcpMode::Client,
                    address,
                    allowed_peer: None,
                },
            };
        }
    }
    if std::env::var("GS_SIMULATED_SERIAL_PTY").as_deref() == Ok("1")
        && let Ok(port) = std::env::var("GS_FILL_PICO_DISCOVERY_PORT")
        && let Ok(port) = port.parse::<u16>()
    {
        config.fill_box = CommsLinkConfig::PicoFiTcp {
            tcp: PicoFiTcpConfig {
                discovery: true,
                discovery_port: port,
                discovery_interface: None,
                mode: TcpMode::Server,
                address: "127.0.0.1:0".into(),
                allowed_peer: None,
            },
        };
    }
    for (name, link, fallback_baud_rate) in [
        (
            "GS_AV_BAY_SERIAL_PORT",
            &mut config.av_bay,
            default_av_bay_baud_rate(),
        ),
        ("GS_FILL_SERIAL_PORT", &mut config.fill_box, 115_200),
    ] {
        let Some(port) = std::env::var_os(name) else {
            continue;
        };
        let port = port.to_string_lossy().into_owned();
        apply_simulator_serial_override(link, port, fallback_baud_rate);
    }
}

fn apply_simulator_serial_override(
    link: &mut CommsLinkConfig,
    port: String,
    fallback_baud_rate: usize,
) {
    match link {
        CommsLinkConfig::Serial { serial }
        | CommsLinkConfig::RaspberryPiGpioUart { serial }
        | CommsLinkConfig::CustomSerial { serial } => {
            serial.port = port;
            serial.protocol = SerialProtocol::RawUart;
        }
        _ => {
            *link = CommsLinkConfig::Serial {
                serial: SerialLinkConfig {
                    port,
                    baud_rate: fallback_baud_rate,
                    protocol: SerialProtocol::RawUart,
                },
            };
        }
    }
}

pub fn save(cfg: &CommsLinksConfig) -> Result<(), String> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("create config directory {}: {err}", parent.display()))?;
    }
    let raw = serialize_config(cfg, crate::rrls::is_rrls())
        .map_err(|err| format!("serialize comms link config: {err}"))?;
    fs::write(&path, raw)
        .map_err(|err| format!("write comms link config {}: {err}", path.display()))
}

fn migrate_legacy_config(target_path: &PathBuf) {
    // RRLS must never rename or inherit the primary station's legacy settings.
    if crate::rrls::is_rrls() {
        return;
    }
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
        if let Err(err) = fs::rename(&legacy_path, target_path)
            && let Err(copy_err) = fs::copy(&legacy_path, target_path)
        {
            eprintln!(
                "WARNING: failed to migrate comms link config {} -> {}: {err}; copy fallback failed: {copy_err}",
                legacy_path.display(),
                target_path.display()
            );
            return;
        }
        return;
    }
}

fn migrate_serial_protocol_defaults(raw: String, path: &PathBuf) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return raw;
    };

    let mut changed = false;
    if let Some(obj) = value.as_object_mut() {
        if let Some(av_bay) = obj.get_mut("av_bay") {
            changed |= ensure_serial_protocol(av_bay, SerialProtocol::RawUart);
        }
        if let Some(fill_box) = obj.get_mut("fill_box") {
            changed |= ensure_serial_protocol(fill_box, SerialProtocol::RawUart);
        }
    }

    if !changed {
        return raw;
    }

    match serde_json::to_string_pretty(&value) {
        Ok(updated) => {
            if let Err(err) = fs::write(path, &updated) {
                eprintln!(
                    "WARNING: failed to persist comms protocol migration {}: {err}",
                    path.display()
                );
            }
            updated
        }
        Err(_) => raw,
    }
}

fn ensure_serial_protocol(link: &mut serde_json::Value, protocol: SerialProtocol) -> bool {
    let Some(obj) = link.as_object_mut() else {
        return false;
    };
    let Some(interface) = obj.get("interface").and_then(|v| v.as_str()) else {
        return false;
    };
    if !matches!(
        interface,
        "serial" | "usb_serial" | "raspberry_pi_gpio_uart" | "custom_serial"
    ) {
        return false;
    }
    if obj.contains_key("protocol") {
        return false;
    }

    let protocol = match protocol {
        SerialProtocol::PacketFramed => "packet_framed",
        SerialProtocol::RawUart => "raw_uart",
    };
    obj.insert(
        "protocol".to_string(),
        serde_json::Value::String(protocol.to_string()),
    );
    true
}

#[cfg(test)]
mod tests {
    use super::{
        CanLinkConfig, CommsLinkConfig, CommsLinksConfig, I2cLinkConfig, SerialLinkConfig,
        SerialProtocol, SpiLinkConfig, apply_simulator_serial_override, default_for_role,
        selected_config_path,
    };
    use std::path::PathBuf;

    #[test]
    fn recovery_accepts_and_saves_radio_only_config_without_inheriting_fill_transport() {
        let raw = r#"{"version":1,"av_bay":{"interface":"serial","port":"/dev/cu.usbserial-BG00HPF3","baud_rate":115200,"protocol":"raw_uart"}}"#;
        let config = super::parse_config(raw, true).unwrap();
        assert_eq!(config.av_bay.port(), "/dev/cu.usbserial-BG00HPF3");
        let saved: serde_json::Value =
            serde_json::from_str(&super::serialize_config(&config, true).unwrap()).unwrap();
        assert!(saved.get("fill_box").is_none());
        assert_eq!(saved["av_bay"]["baud_rate"], 115200);
        assert!(
            super::parse_config(raw, false).is_err(),
            "the primary station still requires its fill transport"
        );
    }
    #[test]
    fn recovery_config_paths_and_defaults_are_separate() {
        let regular = selected_config_path(false, None, None);
        let recovery = selected_config_path(true, None, None);
        assert_ne!(regular, recovery);
        assert!(regular.ends_with("comms/comms.json"));
        assert!(recovery.ends_with("comms/rrls.json"));
        assert_eq!(
            selected_config_path(
                true,
                Some("recovery.json".into()),
                Some("common.json".into())
            ),
            PathBuf::from("recovery.json")
        );
        assert_eq!(
            selected_config_path(false, Some("recovery.json".into()), None),
            regular
        );
        let recovery = default_for_role(true);
        assert_eq!(recovery.fill_box.port(), "unused");
        assert!(matches!(
            recovery.av_bay,
            CommsLinkConfig::Serial {
                serial: SerialLinkConfig {
                    protocol: SerialProtocol::RawUart,
                    baud_rate: 115200,
                    ..
                }
            }
        ));
    }
    #[test]
    fn default_config_uses_ethernet_for_gateway_and_radio_uart_for_av_bay() {
        let cfg = CommsLinksConfig::default();
        assert_eq!(cfg.version, 1);
        assert_eq!(
            cfg.av_bay,
            CommsLinkConfig::Serial {
                serial: SerialLinkConfig {
                    port: "/dev/ttyAMA0".to_string(),
                    baud_rate: 115_200,
                    protocol: SerialProtocol::RawUart,
                },
            }
        );
        assert_eq!(
            cfg.fill_box,
            CommsLinkConfig::PicoFiTcp {
                tcp: super::PicoFiTcpConfig {
                    discovery: true,
                    discovery_port: super::default_discovery_port(),
                    discovery_interface: None,
                    mode: super::TcpMode::Server,
                    address: "0.0.0.0:0".to_owned(),
                    allowed_peer: None,
                },
            }
        );
    }

    #[test]
    fn fill_simulator_override_preserves_gateway_uart_baud_rate() {
        let mut fill = CommsLinkConfig::I2c {
            i2c: I2cLinkConfig {
                protocol_version: 1,
                bus: 1,
                addr: 0x55,
                chunk_delay_ms: 0,
                initial_wait_ms: 0,
            },
        };
        apply_simulator_serial_override(&mut fill, "/tmp/fill.pty".to_string(), 115_200);
        assert!(matches!(
            fill,
            CommsLinkConfig::Serial {
                serial: SerialLinkConfig {
                    baud_rate: 115_200,
                    ..
                }
            }
        ));
    }

    #[test]
    fn checked_in_hardware_config_matches_rfboard_uart_contract() {
        let raw = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("comms/comms.json"),
        )
        .expect("checked-in comms config should be readable");
        let cfg: CommsLinksConfig =
            serde_json::from_str(&raw).expect("checked-in comms config should parse");

        match cfg.av_bay {
            CommsLinkConfig::Serial { serial }
            | CommsLinkConfig::RaspberryPiGpioUart { serial }
            | CommsLinkConfig::CustomSerial { serial } => {
                assert_eq!(serial.port, "/dev/ttyAMA0");
                assert_eq!(serial.baud_rate, 115_200);
                assert_eq!(serial.protocol, SerialProtocol::RawUart);
            }
            other => panic!("RF-board av_bay link must be serial raw_uart, got {other:?}"),
        }
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
                protocol_version: 1,
                bus: 1,
                addr: 0x55,
                chunk_delay_ms: 0,
                initial_wait_ms: 0,
            },
        };

        assert_eq!(spi.port(), "/dev/spidev0.0");
        assert_eq!(can.port(), "can0");
        assert_eq!(i2c.port(), "/dev/i2c-*");
    }

    #[test]
    fn legacy_usb_serial_alias_deserializes_as_serial() {
        let raw = r#"{
            "version": 1,
            "av_bay": {
                "interface": "usb_serial",
                "port": "/dev/ttyUSB1",
                "baud_rate": 57600,
                "protocol": "packet_framed"
            },
            "fill_box": {
                "interface": "serial",
                "port": "/dev/ttyUSB2",
                "baud_rate": 115200,
                "protocol": "raw_uart"
            }
        }"#;

        let cfg: CommsLinksConfig = serde_json::from_str(raw).expect("legacy config should parse");
        assert!(matches!(cfg.av_bay, CommsLinkConfig::Serial { .. }));
        assert!(matches!(cfg.fill_box, CommsLinkConfig::Serial { .. }));
    }
}
