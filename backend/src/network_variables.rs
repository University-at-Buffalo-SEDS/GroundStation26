use crate::telemetry_task::get_current_timestamp_ms;
use crate::types::Board;
use anyhow::{Context, Result};
use sedsnet::packet::Packet;
use sedsnet::router::{NetworkVariablePermissions, Router};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

const DEFAULT_CACHE_PATH: &str = "backend/data/network_variables.json";
const UNDERGLOW_TYPE: &str = "AV_BAY_UNDERGLOW";
const FLIGHT_BUZZER_TYPE: &str = "FLIGHT_BUZZER";
const FLIGHT_STATE_TYPE: &str = "FLIGHT_STATE";
const RF_RATE_TYPE: &str = "RF_TELEMETRY_RATE_HZ";
const FC_RATE_TYPE: &str = "FC_TELEMETRY_RATE_HZ";
const DAQ_CALIBRATION_TYPE: &str = "DAQ_LOADCELL_CALIBRATION";

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
struct PersistentVariables {
    #[serde(default)]
    av_bay_underglow: bool,
    #[serde(default)]
    flight_buzzer: bool,
    #[serde(default)]
    flight_state: u8,
    #[serde(default = "default_rf_rate_hz")]
    rf_telemetry_rate_hz: f32,
    #[serde(default = "default_fc_rate_hz")]
    fc_telemetry_rate_hz: f32,
}

const fn default_rf_rate_hz() -> f32 {
    1.0
}
const fn default_fc_rate_hz() -> f32 {
    1.0
}

struct VariableStore {
    path: PathBuf,
    values: PersistentVariables,
}

static STORE: OnceLock<Mutex<VariableStore>> = OnceLock::new();
static NETWORK_VARIABLE_NONCE: AtomicU16 = AtomicU16::new(1);

fn cache_path() -> PathBuf {
    std::env::var_os("GS_NETWORK_VARIABLE_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CACHE_PATH))
}

fn load(path: &Path) -> PersistentVariables {
    if let Some(values) = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .filter(|values: &PersistentVariables| values.flight_state <= 15)
    {
        return values;
    }
    PersistentVariables {
        av_bay_underglow: std::env::var("GS_AV_BAY_UNDERGLOW_DEFAULT")
            .ok()
            .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "on")),
        flight_buzzer: std::env::var("GS_FLIGHT_BUZZER_DEFAULT")
            .ok()
            .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "on")),
        flight_state: std::env::var("GS_FLIGHT_STATE_DEFAULT")
            .ok()
            .and_then(|value| value.parse::<u8>().ok())
            .filter(|value| *value <= 15)
            .unwrap_or(0),
        rf_telemetry_rate_hz: default_rf_rate_hz(),
        fc_telemetry_rate_hz: default_fc_rate_hz(),
    }
}

fn store() -> &'static Mutex<VariableStore> {
    STORE.get_or_init(|| {
        let path = cache_path();
        Mutex::new(VariableStore {
            values: load(&path),
            path,
        })
    })
}

fn persist(path: &Path, values: PersistentVariables) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "create network-variable cache directory {}",
                parent.display()
            )
        })?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(&values)?)
        .with_context(|| format!("write network-variable cache {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("replace network-variable cache {}", path.display()))?;
    Ok(())
}

fn packet(data_type: &str, endpoint: &str, value: u8) -> Result<Packet> {
    packet_bytes(data_type, endpoint, Arc::from([value]))
}

fn packet_bytes(data_type: &str, endpoint: &str, payload: Arc<[u8]>) -> Result<Packet> {
    Ok(Packet::new(
        crate::telemetry_schema::data_type(data_type),
        &[crate::telemetry_schema::endpoint(endpoint)],
        Board::GroundStation.sender_id(),
        get_current_timestamp_ms(),
        payload,
    )?
    .with_nonce(NETWORK_VARIABLE_NONCE.fetch_add(1, Ordering::Relaxed)))
}

pub fn initialize(router: &Router) -> Result<()> {
    for data_type in [
        UNDERGLOW_TYPE,
        FLIGHT_BUZZER_TYPE,
        FLIGHT_STATE_TYPE,
        RF_RATE_TYPE,
        FC_RATE_TYPE,
        DAQ_CALIBRATION_TYPE,
    ] {
        router.enable_network_variable(
            crate::telemetry_schema::data_type(data_type),
            NetworkVariablePermissions::READ_WRITE,
        )?;
    }
    router.on_network_variable_update(
        crate::telemetry_schema::data_type(FLIGHT_STATE_TYPE),
        |packet| {
            let state =
                packet
                    .payload()
                    .first()
                    .copied()
                    .ok_or(sedsnet::TelemetryError::HandlerError(
                        "empty flight-state network variable",
                    ))?;
            if state > 15 {
                return Err(sedsnet::TelemetryError::HandlerError(
                    "out-of-range flight-state network variable",
                ));
            }
            let mut guard = store()
                .lock()
                .expect("network-variable store lock poisoned");
            guard.values.flight_state = state;
            persist(&guard.path, guard.values).map_err(|err| {
                log::warn!("failed to persist inbound flight-state network variable: {err}");
                sedsnet::TelemetryError::HandlerError(
                    "persist inbound flight-state network variable",
                )
            })
        },
    )?;
    {
        let guard = store()
            .lock()
            .expect("network-variable store lock poisoned");
        if !guard.path.exists() {
            persist(&guard.path, guard.values)?;
        }
    }
    router.seed_managed_variable(packet(
        UNDERGLOW_TYPE,
        "AV_BAY_UNDERGLOW_OWNER",
        u8::from(underglow_enabled()),
    )?)?;
    router.seed_managed_variable(packet(
        FLIGHT_BUZZER_TYPE,
        "FLIGHT_CONTROLLER",
        u8::from(flight_buzzer_enabled()),
    )?)?;
    router.seed_managed_variable(packet(FLIGHT_STATE_TYPE, "FLIGHT_STATE", flight_state())?)?;
    router.seed_managed_variable(rate_packet(RF_RATE_TYPE, rf_telemetry_rate_hz())?)?;
    router.seed_managed_variable(rate_packet(FC_RATE_TYPE, fc_telemetry_rate_hz())?)?;
    router.seed_managed_variable(calibration_packet(&crate::loadcell::load_or_default())?)?;
    Ok(())
}

pub fn publish_current(router: &Router) -> Result<()> {
    router.set_network_variable(packet(
        UNDERGLOW_TYPE,
        "AV_BAY_UNDERGLOW_OWNER",
        u8::from(underglow_enabled()),
    )?)?;
    router.set_network_variable(packet(
        FLIGHT_BUZZER_TYPE,
        "FLIGHT_CONTROLLER",
        u8::from(flight_buzzer_enabled()),
    )?)?;
    router.set_network_variable(packet(FLIGHT_STATE_TYPE, "FLIGHT_STATE", flight_state())?)?;
    router.set_network_variable(rate_packet(RF_RATE_TYPE, rf_telemetry_rate_hz())?)?;
    router.set_network_variable(rate_packet(FC_RATE_TYPE, fc_telemetry_rate_hz())?)?;
    router.set_network_variable(calibration_packet(&crate::loadcell::load_or_default())?)?;
    Ok(())
}

pub fn toggle_underglow(router: &Router) -> Result<bool> {
    let enabled = !underglow_enabled();
    set_underglow(router, enabled)?;
    Ok(enabled)
}

pub fn set_underglow(router: &Router, enabled: bool) -> Result<()> {
    {
        let mut guard = store()
            .lock()
            .expect("network-variable store lock poisoned");
        guard.values.av_bay_underglow = enabled;
        persist(&guard.path, guard.values)?;
    }
    router.set_network_variable(packet(
        UNDERGLOW_TYPE,
        "AV_BAY_UNDERGLOW_OWNER",
        u8::from(enabled),
    )?)?;
    Ok(())
}

#[allow(dead_code)] // Used by the optional direct-command compatibility path.
pub fn toggle_flight_buzzer(router: &Router) -> Result<bool> {
    let enabled = !flight_buzzer_enabled();
    set_flight_buzzer(router, enabled)?;
    Ok(enabled)
}

pub fn set_flight_buzzer(router: &Router, enabled: bool) -> Result<()> {
    {
        let mut guard = store()
            .lock()
            .expect("network-variable store lock poisoned");
        guard.values.flight_buzzer = enabled;
        persist(&guard.path, guard.values)?;
    }
    router.set_network_variable(packet(
        FLIGHT_BUZZER_TYPE,
        "FLIGHT_CONTROLLER",
        u8::from(enabled),
    )?)?;
    Ok(())
}

pub fn set_flight_state(router: &Router, state: u8) -> Result<()> {
    anyhow::ensure!(state <= 15, "invalid flight-state value {state}");
    {
        let mut guard = store()
            .lock()
            .expect("network-variable store lock poisoned");
        guard.values.flight_state = state;
        persist(&guard.path, guard.values)?;
    }
    router.set_network_variable(packet(FLIGHT_STATE_TYPE, "FLIGHT_STATE", state)?)?;
    Ok(())
}

fn rate_packet(data_type: &str, rate_hz: f32) -> Result<Packet> {
    anyhow::ensure!(
        rate_hz.is_finite() && (0.1..=20.0).contains(&rate_hz),
        "telemetry rate must be between 0.1 and 20 Hz"
    );
    packet_bytes(
        data_type,
        "GROUND_STATION",
        Arc::from(rate_hz.to_le_bytes()),
    )
}

fn calibration_packet(cfg: &crate::loadcell::LoadcellCalibrationFile) -> Result<Packet> {
    let values = [
        cfg.ch1.m.unwrap_or(1.0),
        cfg.ch1.b.unwrap_or(0.0),
        cfg.iadc.m.unwrap_or(1.0),
        cfg.iadc.b.unwrap_or(0.0),
    ];
    anyhow::ensure!(
        values.iter().all(|value| value.is_finite()),
        "DAQ calibration contains a non-finite coefficient"
    );
    let mut payload = Vec::with_capacity(16);
    for value in values {
        payload.extend_from_slice(&value.to_le_bytes());
    }
    packet_bytes(DAQ_CALIBRATION_TYPE, "SD_CARD", Arc::from(payload))
}

pub fn set_daq_calibration(
    router: &Router,
    cfg: &crate::loadcell::LoadcellCalibrationFile,
) -> Result<()> {
    router.set_network_variable(calibration_packet(cfg)?)?;
    Ok(())
}

pub fn set_rf_telemetry_rate_hz(router: &Router, rate_hz: f32) -> Result<()> {
    let update = rate_packet(RF_RATE_TYPE, rate_hz)?;
    {
        let mut guard = store()
            .lock()
            .expect("network-variable store lock poisoned");
        guard.values.rf_telemetry_rate_hz = rate_hz;
        persist(&guard.path, guard.values)?;
    }
    router.set_network_variable(update)?;
    Ok(())
}

pub fn set_fc_telemetry_rate_hz(router: &Router, rate_hz: f32) -> Result<()> {
    let update = rate_packet(FC_RATE_TYPE, rate_hz)?;
    {
        let mut guard = store()
            .lock()
            .expect("network-variable store lock poisoned");
        guard.values.fc_telemetry_rate_hz = rate_hz;
        persist(&guard.path, guard.values)?;
    }
    router.set_network_variable(update)?;
    Ok(())
}

#[cfg(test)]
fn toggle_persisted(store: &mut VariableStore) -> Result<bool> {
    store.values.av_bay_underglow = !store.values.av_bay_underglow;
    persist(&store.path, store.values)?;
    Ok(store.values.av_bay_underglow)
}

#[cfg(test)]
fn set_persisted_flight_state(store: &mut VariableStore, state: u8) -> Result<()> {
    anyhow::ensure!(state <= 15, "invalid flight-state value {state}");
    store.values.flight_state = state;
    persist(&store.path, store.values)
}

pub fn underglow_enabled() -> bool {
    store()
        .lock()
        .expect("network-variable store lock poisoned")
        .values
        .av_bay_underglow
}

pub fn flight_buzzer_enabled() -> bool {
    store()
        .lock()
        .expect("network-variable store lock poisoned")
        .values
        .flight_buzzer
}

pub fn flight_state() -> u8 {
    store()
        .lock()
        .expect("network-variable store lock poisoned")
        .values
        .flight_state
}

pub fn rf_telemetry_rate_hz() -> f32 {
    store()
        .lock()
        .expect("network-variable store lock poisoned")
        .values
        .rf_telemetry_rate_hz
}

pub fn fc_telemetry_rate_hz() -> f32 {
    store()
        .lock()
        .expect("network-variable store lock poisoned")
        .values
        .fc_telemetry_rate_hz
}

#[cfg(test)]
mod tests {
    use super::*;
    use sedsnet::router::{EndpointHandler, RouterConfig};

    #[test]
    fn missing_cache_defaults_to_off() {
        let path = std::env::temp_dir().join(format!(
            "gs26-missing-network-vars-{}.json",
            std::process::id()
        ));
        assert!(!load(&path).av_bay_underglow);
    }

    #[test]
    fn persisted_cache_round_trips() {
        let path =
            std::env::temp_dir().join(format!("gs26-network-vars-{}.json", std::process::id()));
        persist(
            &path,
            PersistentVariables {
                av_bay_underglow: true,
                flight_buzzer: true,
                flight_state: 6,
                rf_telemetry_rate_hz: 2.0,
                fc_telemetry_rate_hz: 4.0,
            },
        )
        .unwrap();
        assert!(load(&path).av_bay_underglow);
        assert!(load(&path).flight_buzzer);
        assert_eq!(load(&path).flight_state, 6);
        assert_eq!(load(&path).rf_telemetry_rate_hz, 2.0);
        assert_eq!(load(&path).fc_telemetry_rate_hz, 4.0);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn persisted_toggle_survives_two_process_style_reloads() {
        let path = std::env::temp_dir().join(format!(
            "gs26-network-vars-toggle-{}.json",
            std::process::id()
        ));
        persist(
            &path,
            PersistentVariables {
                av_bay_underglow: true,
                ..Default::default()
            },
        )
        .unwrap();

        let mut first_restart = VariableStore {
            path: path.clone(),
            values: load(&path),
        };
        assert!(!toggle_persisted(&mut first_restart).unwrap());
        assert!(!load(&path).av_bay_underglow);

        let mut second_restart = VariableStore {
            path: path.clone(),
            values: load(&path),
        };
        assert!(toggle_persisted(&mut second_restart).unwrap());
        assert!(load(&path).av_bay_underglow);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn flight_state_changes_survive_process_style_reloads() {
        let path = std::env::temp_dir().join(format!(
            "gs26-flight-state-restarts-{}.json",
            std::process::id()
        ));
        persist(&path, PersistentVariables::default()).unwrap();

        for expected in [1, 0, 1] {
            let mut restarted = VariableStore {
                path: path.clone(),
                values: load(&path),
            };
            set_persisted_flight_state(&mut restarted, expected).unwrap();
            assert_eq!(load(&path).flight_state, expected);
        }
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn corrupt_out_of_range_flight_state_fails_safe() {
        let path = std::env::temp_dir().join(format!(
            "gs26-invalid-flight-state-{}.json",
            std::process::id()
        ));
        fs::write(&path, br#"{"flight_state":255}"#).unwrap();
        assert_eq!(load(&path).flight_state, 0);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn network_variable_reaches_a_read_only_avionics_peer() {
        crate::telemetry_schema::initialize().unwrap();
        let source = Arc::new(Router::new(
            RouterConfig::new([]).with_sender(Board::GroundStation.sender_id()),
        ));
        let peer = Arc::new(Router::new(RouterConfig::new([]).with_sender("RF")));
        let ty = crate::telemetry_schema::data_type(UNDERGLOW_TYPE);
        source
            .enable_network_variable(ty, NetworkVariablePermissions::READ_WRITE)
            .unwrap();
        peer.enable_network_variable(ty, NetworkVariablePermissions::READ_ONLY)
            .unwrap();

        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_callback = observed.clone();
        peer.on_network_variable_update(ty, move |packet| {
            observed_callback
                .lock()
                .unwrap()
                .push(packet.payload().to_vec());
            Ok(())
        })
        .unwrap();

        let peer_rx = peer.clone();
        source.add_side_packet("to-rf", move |packet| peer_rx.rx_from_side(packet, 0));
        let source_rx = source.clone();
        peer.add_side_packet("to-gs", move |packet| source_rx.rx_from_side(packet, 0));

        source
            .set_network_variable(packet(UNDERGLOW_TYPE, "AV_BAY_UNDERGLOW_OWNER", 1).unwrap())
            .unwrap();
        source.process_all_queues().unwrap();
        peer.process_all_queues().unwrap();
        assert_eq!(*observed.lock().unwrap(), vec![vec![1]]);
    }

    #[test]
    fn flight_buzzer_targets_only_the_discovered_flight_controller() {
        crate::telemetry_schema::initialize().unwrap();
        let packet = packet(FLIGHT_BUZZER_TYPE, "FLIGHT_CONTROLLER", 1).unwrap();
        assert_eq!(
            packet.endpoints(),
            &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")]
        );
        assert_eq!(packet.payload(), &[1]);
    }

    #[test]
    fn telemetry_rate_packets_are_bounded_and_little_endian() {
        crate::telemetry_schema::initialize().unwrap();
        for rate in [0.1_f32, 1.0, 20.0] {
            let packet = rate_packet(RF_RATE_TYPE, rate).unwrap();
            assert_eq!(packet.payload(), rate.to_le_bytes());
        }
        assert!(rate_packet(RF_RATE_TYPE, 0.09).is_err());
        assert!(rate_packet(RF_RATE_TYPE, 20.01).is_err());
        assert!(rate_packet(RF_RATE_TYPE, f32::NAN).is_err());
    }

    #[test]
    fn daq_calibration_packet_contains_all_linear_coefficients() {
        crate::telemetry_schema::initialize().unwrap();
        let mut cfg = crate::loadcell::LoadcellCalibrationFile::default();
        cfg.ch1.m = Some(2.0);
        cfg.ch1.b = Some(-3.0);
        cfg.iadc.m = Some(4.5);
        cfg.iadc.b = Some(6.0);
        let packet = calibration_packet(&cfg).unwrap();
        let decoded: Vec<f32> = packet
            .payload()
            .chunks_exact(4)
            .map(|bytes| f32::from_le_bytes(bytes.try_into().unwrap()))
            .collect();
        assert_eq!(decoded, vec![2.0, -3.0, 4.5, 6.0]);
    }

    #[test]
    fn flight_state_network_variable_reaches_a_read_only_avionics_peer() {
        crate::telemetry_schema::initialize().unwrap();
        let source = Arc::new(Router::new(
            RouterConfig::new([]).with_sender(Board::GroundStation.sender_id()),
        ));
        let peer = Arc::new(Router::new(
            RouterConfig::new([EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint("FLIGHT_STATE"),
                |_packet| Ok(()),
            )])
            .with_sender("FC"),
        ));
        let ty = crate::telemetry_schema::data_type(FLIGHT_STATE_TYPE);
        source
            .enable_network_variable(ty, NetworkVariablePermissions::READ_WRITE)
            .unwrap();
        peer.enable_network_variable(ty, NetworkVariablePermissions::READ_ONLY)
            .unwrap();

        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_callback = observed.clone();
        peer.on_network_variable_update(ty, move |packet| {
            observed_callback.lock().unwrap().push(packet.payload()[0]);
            Ok(())
        })
        .unwrap();
        let peer_rx = peer.clone();
        source.add_side_packet("to-fc", move |packet| peer_rx.rx_from_side(packet, 0));
        let source_rx = source.clone();
        peer.add_side_packet("to-gs", move |packet| source_rx.rx_from_side(packet, 0));

        for state in [1, 0, 1] {
            std::thread::sleep(std::time::Duration::from_millis(2));
            source
                .set_network_variable(packet(FLIGHT_STATE_TYPE, "FLIGHT_STATE", state).unwrap())
                .unwrap();
            source.process_all_queues().unwrap();
            peer.process_all_queues().unwrap();
        }
        assert_eq!(*observed.lock().unwrap(), vec![1, 0, 1]);
    }
}
