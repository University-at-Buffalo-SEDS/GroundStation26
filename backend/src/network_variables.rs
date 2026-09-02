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

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
struct PersistentVariables {
    #[serde(default)]
    av_bay_underglow: bool,
    #[serde(default)]
    flight_buzzer: bool,
    #[serde(default)]
    flight_state: u8,
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
    Ok(Packet::new(
        crate::telemetry_schema::data_type(data_type),
        &[crate::telemetry_schema::endpoint(endpoint)],
        Board::GroundStation.sender_id(),
        get_current_timestamp_ms(),
        Arc::from([value]),
    )?
    .with_nonce(NETWORK_VARIABLE_NONCE.fetch_add(1, Ordering::Relaxed)))
}

pub fn initialize(router: &Router) -> Result<()> {
    for data_type in [UNDERGLOW_TYPE, FLIGHT_BUZZER_TYPE, FLIGHT_STATE_TYPE] {
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
        "HEART_BEAT",
        u8::from(underglow_enabled()),
    )?)?;
    router.seed_managed_variable(packet(
        FLIGHT_BUZZER_TYPE,
        "HEART_BEAT",
        u8::from(flight_buzzer_enabled()),
    )?)?;
    router.seed_managed_variable(packet(FLIGHT_STATE_TYPE, "FLIGHT_STATE", flight_state())?)?;
    Ok(())
}

pub fn publish_current(router: &Router) -> Result<()> {
    router.set_network_variable(packet(
        UNDERGLOW_TYPE,
        "HEART_BEAT",
        u8::from(underglow_enabled()),
    )?)?;
    router.set_network_variable(packet(
        FLIGHT_BUZZER_TYPE,
        "HEART_BEAT",
        u8::from(flight_buzzer_enabled()),
    )?)?;
    router.set_network_variable(packet(FLIGHT_STATE_TYPE, "FLIGHT_STATE", flight_state())?)?;
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
    router.set_network_variable(packet(UNDERGLOW_TYPE, "HEART_BEAT", u8::from(enabled))?)?;
    Ok(())
}

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
    router.set_network_variable(packet(FLIGHT_BUZZER_TYPE, "HEART_BEAT", u8::from(enabled))?)?;
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
            },
        )
        .unwrap();
        assert!(load(&path).av_bay_underglow);
        assert!(load(&path).flight_buzzer);
        assert_eq!(load(&path).flight_state, 6);
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
            .set_network_variable(packet(UNDERGLOW_TYPE, "HEART_BEAT", 1).unwrap())
            .unwrap();
        source.process_all_queues().unwrap();
        peer.process_all_queues().unwrap();
        assert_eq!(*observed.lock().unwrap(), vec![vec![1]]);
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
