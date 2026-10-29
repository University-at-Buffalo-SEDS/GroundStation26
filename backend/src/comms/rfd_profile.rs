//! Explicit matched application/modem profile. Loading never contacts a modem.
use crate::comms_config::{CommsLinkConfig, SerialProtocol};
use serde::Deserialize;
use std::sync::OnceLock;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Profile {
    version: u64,
    name: String,
    modem: Modem,
    pub transport: Transport,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Modem {
    serial_baud: u64,
    air_bps: u64,
    max_data_bytes: u64,
    max_retries: u64,
    broadcast_retries: u64,
    serial_break_ms: u64,
    tx_encapsulation: u64,
    rx_encapsulation: u64,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Transport {
    pub usable_air_bps: u64,
    pub frame_overhead_bytes: u64,
    pub packet_burst_ms: u64,
    pub packet_gap_ms: u64,
    idle_margin_ms: u64,
    pub rf_share_percent: u64,
    pub host_default_share_percent: u64,
}
static ACTIVE: OnceLock<Option<Profile>> = OnceLock::new();

impl Profile {
    pub(crate) fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
        let profile: Self = serde_json::from_slice(bytes)?;
        profile.validate()?;
        Ok(profile)
    }
    fn validate(&self) -> anyhow::Result<()> {
        let m = &self.modem;
        let t = &self.transport;
        anyhow::ensure!(
            self.version == 1 && !self.name.is_empty() && self.name.len() <= 80,
            "unsupported or unnamed RFD profile"
        );
        anyhow::ensure!(m.serial_baud == 115200, "profile UART baud must be 115200");
        anyhow::ensure!(
            m.air_bps == 64000,
            "this raw ASYNC profile supports 64000 air bps"
        );
        anyhow::ensure!(
            (1028..=4068).contains(&m.max_data_bytes),
            "invalid modem packet capacity"
        );
        anyhow::ensure!(
            (1..=100).contains(&m.max_retries) && m.broadcast_retries <= 100,
            "invalid modem retry limits"
        );
        anyhow::ensure!(
            (1..=500).contains(&m.serial_break_ms),
            "raw ASYNC requires a finite serial break"
        );
        anyhow::ensure!(
            m.tx_encapsulation == 0 && m.rx_encapsulation == 0,
            "this transport implements raw ASYNC encapsulation only"
        );
        anyhow::ensure!(
            (1200..=m.air_bps).contains(&t.usable_air_bps)
                && t.frame_overhead_bytes <= 256
                && (1..=500).contains(&t.packet_burst_ms)
                && t.packet_gap_ms <= 1000
                && (1..=100).contains(&t.rf_share_percent)
                && (1..=100).contains(&t.host_default_share_percent),
            "invalid RFD application capacity"
        );
        anyhow::ensure!(
            t.rf_share_percent + (2 * t.host_default_share_percent).max(50) <= 100,
            "RF and host shares exceed the shared link, including the 15/35 relay split"
        );
        anyhow::ensure!(
            (5..=100).contains(&t.idle_margin_ms)
                && t.packet_gap_ms >= m.serial_break_ms + t.idle_margin_ms,
            "UART idle gap must exceed modem serial break with a guard margin"
        );
        let largest_burst = (m.serial_baud * t.packet_burst_ms).div_ceil(10_000) + 1028;
        anyhow::ensure!(
            largest_burst <= m.max_data_bytes,
            "application burst could cross MAX_DATA inside an owned UART frame"
        );
        Ok(())
    }
}

pub(crate) fn active() -> Option<&'static Profile> {
    ACTIVE.get().and_then(Option::as_ref)
}

pub(crate) fn capacity(uart_baud: u32) -> super::rfd_capacity::Capacity {
    if let Some(p) = active() {
        super::rfd_capacity::Capacity::from_defaults(
            uart_baud,
            super::rfd_capacity::Defaults {
                air_bps: p.transport.usable_air_bps,
                overhead_bytes: p.transport.frame_overhead_bytes,
                packet_burst_ms: p.transport.packet_burst_ms,
                packet_gap_ms: p.transport.packet_gap_ms,
                host_share_percent: p.transport.host_default_share_percent,
            },
        )
    } else {
        super::rfd_capacity::Capacity::from_env(uart_baud)
    }
}

pub(crate) fn initialize(link: &CommsLinkConfig) -> anyhow::Result<()> {
    let serial = match link {
        CommsLinkConfig::Serial { serial }
        | CommsLinkConfig::CustomSerial { serial }
        | CommsLinkConfig::RaspberryPiGpioUart { serial } => Some(serial),
        _ => None,
    };
    let selected = std::env::var_os("GS_RADIO_PROFILE")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            serial.and_then(|s| s.rfd_profile.as_ref()).map(|path| {
                if path.is_absolute() {
                    path.clone()
                } else {
                    crate::comms_config::config_path()
                        .parent()
                        .unwrap_or_else(|| std::path::Path::new("."))
                        .join(path)
                }
            })
        });
    let raw_radio = serial.is_some_and(|s| s.protocol == SerialProtocol::RawUart);
    let profile = if selected.is_some() || raw_radio {
        let bytes = if let Some(path) = selected {
            std::fs::read(path)?
        } else {
            include_bytes!("../../comms/rfd-profile.json").to_vec()
        };
        let profile = Profile::parse(&bytes)?;
        let serial =
            serial.ok_or_else(|| anyhow::anyhow!("RFD profile requires a serial radio link"))?;
        anyhow::ensure!(
            serial.baud_rate as u64 == profile.modem.serial_baud
                && serial.protocol == SerialProtocol::RawUart,
            "radio link does not match selected RFD profile"
        );
        for (key, expected) in [
            (
                "GS_RADIO_AIR_BIT_RATE_BPS",
                profile.transport.usable_air_bps,
            ),
            (
                "GS_RADIO_AIR_FRAME_OVERHEAD_BYTES",
                profile.transport.frame_overhead_bytes,
            ),
            (
                "GS_RADIO_PACKET_BURST_MS",
                profile.transport.packet_burst_ms,
            ),
            ("GS_RADIO_PACKET_GAP_MS", profile.transport.packet_gap_ms),
            (
                "GS_RADIO_AIR_SHARE_PERCENT",
                profile.transport.host_default_share_percent,
            ),
        ] {
            if let Some(value) = std::env::var_os(key) {
                anyhow::ensure!(
                    value.to_str().and_then(|v| v.parse::<u64>().ok()) == Some(expected),
                    "{key} conflicts with the selected RFD profile"
                );
            }
        }
        log::info!(
            "RFD profile selected: {} (modem settings require separate matched configuration)",
            profile.name
        );
        Some(profile)
    } else {
        None
    };
    ACTIVE
        .set(profile)
        .map_err(|_| anyhow::anyhow!("RFD profile already initialized"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn example() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../config/radio/rfd-async-64k-low-latency.json"
        ))
        .unwrap()
    }
    #[test]
    fn shared_profile_preserves_raw_frame_boundaries_and_bounded_capacity() {
        let p = Profile::parse(&serde_json::to_vec(&example()).unwrap()).unwrap();
        assert_eq!(p.transport.usable_air_bps, 32000);
        assert_eq!(p.transport.packet_gap_ms, 15);
    }
    #[test]
    fn shipping_profile_retains_existing_modem_idle_boundary() {
        let p = Profile::parse(include_bytes!("../../comms/rfd-profile.json")).unwrap();
        assert_eq!(p.modem.serial_break_ms, 50);
        assert_eq!(p.transport.packet_gap_ms, 55);
        assert_eq!(p.transport.usable_air_bps, 32000);
        assert_eq!(p.transport.host_default_share_percent, 25);
    }
    #[test]
    fn mismatched_or_unbounded_profiles_are_rejected() {
        for (section, key, value) in [
            ("modem", "serial_break_ms", 0),
            ("modem", "serial_break_ms", 50),
            ("modem", "max_data_bytes", 1028),
            ("modem", "max_retries", 0),
            ("modem", "tx_encapsulation", 2),
            ("transport", "usable_air_bps", 64001),
            ("transport", "rf_share_percent", 80),
            ("transport", "host_default_share_percent", 40),
            ("transport", "packet_burst_ms", 500),
            ("transport", "packet_gap_ms", 10),
            ("transport", "rf_share_percent", 0),
        ] {
            let mut v = example();
            v[section][key] = value.into();
            assert!(
                Profile::parse(&serde_json::to_vec(&v).unwrap()).is_err(),
                "{section}.{key}"
            );
        }
    }
}
