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
const DAQ_CALIBRATION_TYPE: &str = "DAQ_LOADCELL_CALIBRATION";
const DAQ_KG50_CALIBRATION_TYPE: &str = "DAQ_KG50_CALIBRATION";

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
struct PersistentVariables {
    #[serde(default)]
    av_bay_underglow: bool,
    #[serde(default)]
    flight_buzzer: bool,
    #[serde(default)]
    flight_state: u8,
    #[serde(default)]
    daq_log_clock: [u64; 2],
    #[serde(default)]
    daq_log_last_session: u64,
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
        daq_log_clock: [0, 0],
        daq_log_last_session: 0,
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
        DAQ_CALIBRATION_TYPE,
        DAQ_KG50_CALIBRATION_TYPE,
        "DAQ_LOG_CLOCK",
        "DAQ_THERMAL_CALIBRATION",
        "DAQ_FILTER_CALIBRATION",
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
    let calibration = crate::loadcell::load_or_default();
    router.seed_managed_variable(calibration_packet(&calibration)?)?;
    router.seed_managed_variable(kg50_calibration_packet(&calibration)?)?;
    router.seed_managed_variable(cached_daq_log_clock_packet()?)?;
    router.seed_managed_variable(thermal_packet(&calibration)?)?;
    router.seed_managed_variable(filter_packet(&calibration)?)?;
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
    set_daq_calibration(router, &crate::loadcell::load_or_default())?;
    router.set_network_variable(cached_daq_log_clock_packet()?)?;
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

fn filter_packet(cfg: &crate::loadcell::LoadcellCalibrationFile) -> Result<Packet> {
    crate::loadcell::validate_thermal(cfg).map_err(anyhow::Error::msg)?;
    let payload: Vec<u8> = ["KG1000", "KG50"].iter().flat_map(|sensor|
        cfg.noise.get(*sensor).map(|n| n.tau_ms).unwrap_or(0.).to_le_bytes()).collect();
    packet_bytes("DAQ_FILTER_CALIBRATION", "SD_CARD", Arc::from(payload))
}

fn thermal_packet(cfg: &crate::loadcell::LoadcellCalibrationFile) -> Result<Packet> {
    crate::loadcell::validate_thermal(cfg).map_err(anyhow::Error::msg)?;
    let mut payload = Vec::with_capacity(16);
    for sensor in ["KG1000", "KG50"] {
        let t = cfg.thermal.get(sensor).cloned().unwrap_or_default();
        payload.extend_from_slice(&t.reference_c.to_le_bytes());
        payload.extend_from_slice(&t.raw_per_c.to_le_bytes());
    }
    packet_bytes("DAQ_THERMAL_CALIBRATION", "SD_CARD", Arc::from(payload))
}

fn calibration_packet(cfg: &crate::loadcell::LoadcellCalibrationFile) -> Result<Packet> {
    let values = [
        cfg.ch1.m.unwrap_or(1.0),
        cfg.ch1.b.unwrap_or(0.0) - cfg.ch1_zero_raw.map(|z| cfg.ch1.m.unwrap_or(1.0)*z + cfg.ch1.b.unwrap_or(0.0)).unwrap_or(0.0),
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

fn kg50_calibration_packet(cfg: &crate::loadcell::LoadcellCalibrationFile) -> Result<Packet> {
    let values = crate::loadcell::kg50_daq_coefficients(cfg).map_err(anyhow::Error::msg)?;
    let payload: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
    packet_bytes(DAQ_KG50_CALIBRATION_TYPE, "SD_CARD", Arc::from(payload))
}

pub fn set_daq_calibration(
    router: &Router,
    cfg: &crate::loadcell::LoadcellCalibrationFile,
) -> Result<()> {
    let thermal = thermal_packet(cfg)?;
    let filter = filter_packet(cfg)?;
    let kg50 = kg50_calibration_packet(cfg)?;
    router.set_network_variable(calibration_packet(cfg)?)?;
    router.set_network_variable(kg50)?;
    router.set_network_variable(thermal)?;
    router.set_network_variable(filter)?;
    Ok(())
}

fn daq_log_clock_payload(
    session: u64,
    clock: &crate::telemetry_db::LaunchClockMsg,
    wall_now: i64,
    network_now: i64,
) -> Result<Vec<u8>> {
    use crate::telemetry_db::LaunchClockKind;
    let deadline = if clock.kind == LaunchClockKind::Idle {
        0
    } else {
        let anchor = clock
            .anchor_timestamp_ms
            .context("Launch clock has no anchor")?;
        let countdown = if clock.kind == LaunchClockKind::TMinus {
            clock.duration_ms.unwrap_or(0)
        } else {
            0
        };
        let deadline = network_now
            .saturating_add(anchor.saturating_sub(wall_now))
            .saturating_add(countdown)
            .saturating_add(120_000);
        anyhow::ensure!(
            (315_532_800_000..4_354_819_200_000).contains(&deadline),
            "Set GroundStation/network UTC before timestamped DAQ launch logging"
        );
        deadline as u64
    };
    let session = if deadline == 0 { 0 } else { session };
    anyhow::ensure!(
        deadline == 0 || session != 0,
        "DAQ launch session must be nonzero"
    );
    Ok([session.to_le_bytes(), deadline.to_le_bytes()].concat())
}

fn cached_daq_log_clock_packet() -> Result<Packet> {
    let values = store()
        .lock()
        .expect("network-variable store lock poisoned")
        .values
        .daq_log_clock;
    packet_bytes(
        "DAQ_LOG_CLOCK",
        "SD_CARD",
        Arc::from([values[0].to_le_bytes(), values[1].to_le_bytes()].concat()),
    )
}

pub fn next_daq_log_session(anchor_ms: i64) -> u64 {
    let values = store()
        .lock()
        .expect("network-variable store lock poisoned")
        .values;
    let last = values.daq_log_last_session.max(values.daq_log_clock[0]);
    (anchor_ms.max(1) as u64).max(last.saturating_add(1))
}

pub fn set_daq_log_clock(
    router: &Router,
    session: u64,
    clock: &crate::telemetry_db::LaunchClockMsg,
) -> Result<()> {
    let wall_now = get_current_timestamp_ms() as i64;
    let network_now = router
        .network_time()
        .and_then(|t| t.unix_time_ms)
        .map(|v| v as i64)
        .unwrap_or(wall_now);
    let payload = daq_log_clock_payload(session, clock, wall_now, network_now)?;
    // Preserve the absolute deadline, not a fresh countdown on service restart.
    // Drop the disk-cache lock before entering SEDSNet (callbacks may use it).
    {
        let mut guard = store()
            .lock()
            .expect("network-variable store lock poisoned");
        guard.values.daq_log_clock = [
            u64::from_le_bytes(payload[..8].try_into().unwrap()),
            u64::from_le_bytes(payload[8..].try_into().unwrap()),
        ];
        guard.values.daq_log_last_session = guard.values.daq_log_last_session.max(session);
        persist(&guard.path, guard.values)?;
    }
    router.set_network_variable(packet_bytes(
        "DAQ_LOG_CLOCK",
        "SD_CARD",
        Arc::from(payload),
    )?)?;
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
    fn daq_deadline_uses_t_zero_not_launch_button_or_reconnect_time() {
        use crate::telemetry_db::{LaunchClockKind, LaunchClockMsg};
        let now = 1_800_000_000_000;
        let clock = crate::state::launch_countdown_clock(now);
        let decode = |bytes: Vec<u8>| -> (u64, u64) {
            (
                u64::from_le_bytes(bytes[..8].try_into().unwrap()),
                u64::from_le_bytes(bytes[8..].try_into().unwrap()),
            )
        };
        let expected = (17, (now + 130_000) as u64);
        assert_eq!(
            decode(daq_log_clock_payload(17, &clock, now, now).unwrap()),
            expected
        );
        assert_eq!(
            decode(daq_log_clock_payload(17, &clock, now + 50_000, now + 50_000).unwrap()),
            expected
        );
        let pilot = LaunchClockMsg {
            kind: LaunchClockKind::TPlus,
            anchor_timestamp_ms: Some(now + 10_000),
            duration_ms: None,
        };
        assert_eq!(
            decode(daq_log_clock_payload(17, &pilot, now + 10_000, now + 10_000).unwrap()),
            expected
        );
        assert_eq!(
            decode(daq_log_clock_payload(17, &LaunchClockMsg::idle(), now, now).unwrap()),
            (0, 0)
        );
        assert!(daq_log_clock_payload(0, &clock, now, now).is_err());
        assert!(daq_log_clock_payload(17, &crate::state::launch_countdown_clock(0), 0, 0).is_err());
    }

    #[test]
    fn daq_launch_cache_survives_groundstation_restart_without_extending_run() {
        let path = std::env::temp_dir().join(format!("gs-daq-clock-{}.json", std::process::id()));
        let values = PersistentVariables {
            daq_log_clock: [123, 1_800_000_130_000],
            ..Default::default()
        };
        persist(&path, values).unwrap();
        assert_eq!(load(&path).daq_log_clock, values.daq_log_clock);
        persist(&path, PersistentVariables::default()).unwrap();
        assert_eq!(load(&path).daq_log_clock, [0, 0]);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn daq_retained_launch_clock_recovers_after_either_router_restarts() {
        crate::telemetry_schema::initialize().unwrap();
        let ty = crate::telemetry_schema::data_type("DAQ_LOG_CLOCK");
        let ground_link: Arc<Mutex<Option<Arc<Router>>>> = Default::default();
        let daq_link: Arc<Mutex<Option<Arc<Router>>>> = Default::default();
        let observed: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
        let expected: Vec<u8> = [123u64.to_le_bytes(), 1_800_000_130_000u64.to_le_bytes()].concat();
        let make_ground = || {
            let r = Arc::new(Router::new(RouterConfig::new([]).with_sender("GS")));
            r.enable_network_variable(ty, NetworkVariablePermissions::READ_WRITE)
                .unwrap();
            r.seed_managed_variable(
                packet_bytes("DAQ_LOG_CLOCK", "SD_CARD", Arc::from(expected.clone())).unwrap(),
            )
            .unwrap();
            let link = daq_link.clone();
            r.add_side_packet("fill", move |p| {
                let receiver = link.lock().unwrap().clone();
                receiver.map_or(Ok(()), |r| r.rx_from_side(p, 0))
            });
            r
        };
        let make_daq = || {
            let r = Arc::new(Router::new(RouterConfig::new([]).with_sender("DAQ")));
            r.enable_network_variable(ty, NetworkVariablePermissions::READ_ONLY)
                .unwrap();
            let observations = observed.clone();
            r.on_network_variable_update(ty, move |p| {
                observations.lock().unwrap().push(p.payload().to_vec());
                Ok(())
            })
            .unwrap();
            let link = ground_link.clone();
            r.add_side_packet("can", move |p| {
                let receiver = link.lock().unwrap().clone();
                receiver.map_or(Ok(()), |r| r.rx_from_side(p, 0))
            });
            r
        };
        let mut ground = make_ground();
        let mut daq = make_daq();
        for round in 0..3 {
            if round == 1 {
                *ground_link.lock().unwrap() = None;
                ground = make_ground();
            }
            if round == 2 {
                *daq_link.lock().unwrap() = None;
                daq = make_daq();
            }
            *ground_link.lock().unwrap() = Some(ground.clone());
            *daq_link.lock().unwrap() = Some(daq.clone());
            if round == 2 {
                observed.lock().unwrap().clear();
            }
            ground.announce_discovery().unwrap();
            daq.announce_discovery().unwrap();
            daq.request_managed_variable(ty).unwrap();
            for _ in 0..32 {
                ground.process_all_queues().unwrap();
                daq.process_all_queues().unwrap();
            }
            assert_eq!(
                observed.lock().unwrap().last(),
                Some(&expected),
                "restart round {round}"
            );
            // Also prove the new GS instance can deliver a changed clock, not
            // merely that DAQ still remembers the value from before restart.
            let changed: Vec<u8> =
                [124u64.to_le_bytes(), 1_800_000_140_000u64.to_le_bytes()].concat();
            ground
                .set_network_variable(
                    packet_bytes("DAQ_LOG_CLOCK", "SD_CARD", Arc::from(changed.clone())).unwrap(),
                )
                .unwrap();
            for _ in 0..32 {
                ground.process_all_queues().unwrap();
                daq.process_all_queues().unwrap();
            }
            assert_eq!(observed.lock().unwrap().last(), Some(&changed));
            std::thread::sleep(std::time::Duration::from_millis(2));
            ground
                .set_network_variable(
                    packet_bytes("DAQ_LOG_CLOCK", "SD_CARD", Arc::from(expected.clone())).unwrap(),
                )
                .unwrap();
            for _ in 0..32 {
                ground.process_all_queues().unwrap();
                daq.process_all_queues().unwrap();
            }
        }
        // Release simulated cable endpoints; callbacks must not leak routers.
        *ground_link.lock().unwrap() = None;
        *daq_link.lock().unwrap() = None;
    }

    #[test]
    fn legacy_rate_cache_preserves_only_runtime_settings() {
        let values: PersistentVariables = serde_json::from_str(
            r#"{"av_bay_underglow":true,"flight_buzzer":true,"flight_state":6,"rf_telemetry_rate_hz":2,"fc_telemetry_rate_hz":4}"#,
        ).unwrap();
        assert!(values.av_bay_underglow);
        assert!(values.flight_buzzer);
        assert_eq!(values.flight_state, 6);
        let saved = serde_json::to_value(values).unwrap();
        assert!(saved.get("rf_telemetry_rate_hz").is_none());
        assert!(saved.get("fc_telemetry_rate_hz").is_none());
    }

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
                daq_log_clock: [0, 0],
                daq_log_last_session: 0,
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
    fn kg50_calibration_packet_has_separate_type_and_tare() {
        crate::telemetry_schema::initialize().unwrap();
        let mut cfg = crate::loadcell::LoadcellCalibrationFile::default();
        cfg.extra_channels.insert(
            "kg50".into(),
            crate::loadcell::GenericCalibrationChannel {
                linear: crate::loadcell::ChannelLinear {
                    m: Some(2.0),
                    b: Some(1.0),
                },
                zero_raw: Some(3.0),
                ..Default::default()
            },
        );
        let packet = kg50_calibration_packet(&cfg).unwrap();
        assert_eq!(
            packet.data_type(),
            crate::telemetry_schema::data_type("DAQ_KG50_CALIBRATION")
        );
        assert_eq!(
            packet.endpoints(),
            &[crate::telemetry_schema::endpoint("SD_CARD")]
        );
        let decoded: Vec<f32> = packet
            .payload()
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
            .collect();
        assert_eq!(decoded, vec![1.0, 2.0, 0.0, 0.0, 0.0, 0.0, 7.0]);
        assert_eq!(calibration_packet(&cfg).unwrap().payload().len(), 16);
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

#[cfg(test)]
mod thermal_wire_tests {
    use super::*;
    #[test]
    fn thermal_payload_is_fixed_order_and_defaults_disabled() {
        crate::telemetry_schema::initialize().unwrap();
        let mut cfg = crate::loadcell::LoadcellCalibrationFile::default();
        for i in 0..6 {
            crate::loadcell::capture_thermal_zero(&mut cfg, "KG1000", 10. + i as f32 * 0.5, 20. + i as f32 * 2.).unwrap();
        }
        let p = thermal_packet(&cfg).unwrap();
        assert_eq!(p.data_type(), crate::telemetry_schema::data_type("DAQ_THERMAL_CALIBRATION"));
        let values: Vec<f32> = p.payload().chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();
        assert_eq!(values, vec![20., 0.25, 0., 0.]);
    }
}
