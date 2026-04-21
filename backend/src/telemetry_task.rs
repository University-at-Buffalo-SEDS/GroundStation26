use crate::comms::CommsDevice;
use crate::flight_sim;
use crate::gpio_panel::IGNITION_PIN;
use crate::layout;
use crate::loadcell;
#[cfg(feature = "hitl_mode")]
use crate::rocket_commands::FlightComputerCommands;
use crate::rocket_commands::{ActuatorBoardCommands, ValveBoardCommands};
use crate::state::{AppState, launch_countdown_clock};
use crate::telemetry_db::{
    DbQueueItem, DbWrite, LaunchClockKind, RecordingCommand, RecordingMode, RecordingModeWire,
    RecordingStatusMsg, close_and_finalize_sqlite, open_telemetry_db, prune_recent_writes,
    session_db_path,
};
use crate::types::{Board, FlightState, TelemetryCommand, TelemetryRow, u8_to_flight_state};
use crate::web::{FlightStateMsg, emit_warning};
use sedsprintf_rs_2026::config::DataType;
use sedsprintf_rs_2026::packet::Packet;
use sedsprintf_rs_2026::router::Router;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use tokio::sync::mpsc::error::{TryRecvError as MpscTryRecvError, TrySendError};
use tokio::sync::{Notify, broadcast, mpsc};
use tokio::time::{Duration, interval};

pub struct CommsWorkerHandle {
    pub name: &'static str,
    pub comms: Arc<Mutex<Box<dyn CommsDevice>>>,
    pub tx_rx: mpsc::UnboundedReceiver<Vec<u8>>,
}

fn spawn_comms_worker_threads(
    router: Arc<Router>,
    state: Arc<AppState>,
    mut comms_handle: CommsWorkerHandle,
) -> std::io::Result<Vec<thread::JoinHandle<()>>> {
    let worker_name = comms_handle.name;
    let shared_comms = comms_handle.comms.clone();
    let tx_comms = shared_comms.clone();
    let tx_state = state.clone();
    let tx_worker = thread::Builder::new()
        .name(format!("{}_tx_worker", worker_name))
        .spawn(move || {
            let mut comms_shutdown_rx = tx_state.shutdown_subscribe();
            loop {
                match comms_shutdown_rx.try_recv() {
                    Ok(_)
                    | Err(tokio::sync::broadcast::error::TryRecvError::Closed)
                    | Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => break,
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
                }

                match comms_handle.tx_rx.try_recv() {
                    Ok(payload) => {
                        match tx_comms
                            .lock()
                            .expect("failed to get lock")
                            .send_data(&payload)
                        {
                            Ok(()) => {}
                            Err(e) => {
                                eprintln!("{worker_name} tx worker send_data failed: {e}");
                            }
                        }
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return,
                }
            }
        })?;

    let rx_comms = shared_comms;
    let rx_worker = thread::Builder::new()
        .name(format!("{}_rx_worker", worker_name))
        .spawn(move || {
            let mut comms_shutdown_rx = state.shutdown_subscribe();
            loop {
                match comms_shutdown_rx.try_recv() {
                    Ok(_)
                    | Err(tokio::sync::broadcast::error::TryRecvError::Closed)
                    | Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => break,
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
                }

                let tap_state = state.clone();
                let mut packet_tap = |pkt: &Packet| {
                    tap_state.mark_board_seen(pkt.sender(), get_current_timestamp_ms());
                    tap_state.mark_packet_received(get_current_timestamp_ms());
                    let mut rb = tap_state.ring_buffer.lock().unwrap();
                    rb.push(pkt.clone());
                };
                match rx_comms
                    .lock()
                    .expect("failed to get lock")
                    .recv_packet(&router, &mut packet_tap)
                {
                    Ok(_) => {}
                    Err(e) => {
                        log_telemetry_error(
                            &format!("{worker_name} rx worker recv_packet failed"),
                            e,
                        );
                    }
                }

                thread::sleep(Duration::from_millis(2));
            }
        })?;

    Ok(vec![tx_worker, rx_worker])
}

fn spawn_router_worker_thread(
    router: Arc<Router>,
    state: Arc<AppState>,
) -> std::io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("router_worker".to_string())
        .spawn(move || {
            let mut shutdown_rx = state.shutdown_subscribe();
            loop {
                match shutdown_rx.try_recv() {
                    Ok(_)
                    | Err(tokio::sync::broadcast::error::TryRecvError::Closed)
                    | Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => break,
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
                }

                if let Err(e) = router.poll_discovery() {
                    log_telemetry_error("router discovery polling failed", e);
                }
                if timesync_enabled() {
                    let _ = router.poll_timesync();
                }
                if let Err(e) = process_router_queues(&router) {
                    log_telemetry_error("router queue processing failed", e);
                }
                state.mark_discovered_relays_seen();

                thread::sleep(Duration::from_millis(1));
            }
        })
}

const PACKET_WORK_QUEUE_SIZE: usize = 8_192;
const PACKET_ENQUEUE_BURST: usize = 256;
const BACKPRESSURE_LOG_INTERVAL_MS: u64 = 10_000;
const DB_BATCH_MAX_DEFAULT: usize = 256;
const DB_BATCH_WAIT_MS_DEFAULT: u64 = 8;
const ROUTER_QUEUE_BUDGET_MS: u32 = 6;
const ROUTER_TX_BUDGET_MS: u32 = 3;
const ROUTER_RX_BUDGET_MS: u32 = ROUTER_QUEUE_BUDGET_MS - ROUTER_TX_BUDGET_MS;
const LAUNCH_COMMAND_DELAY_MS: u64 = 5_000;
const GPS_SATELLITES_DATA_TYPE: &str = "GPS_SATELLITE_NUMBER";
const VEHICLE_SPEED_DATA_TYPE: &str = "VEHICLE_SPEED";
const GRAVITY_MPS2: f32 = 9.80665;

#[cfg(feature = "hitl_mode")]
fn hitl_flight_command_id(cmd: &TelemetryCommand) -> Option<u8> {
    Some(match cmd {
        TelemetryCommand::DeployParachute => FlightComputerCommands::DeployParachute as u8,
        TelemetryCommand::ExpandParachute => FlightComputerCommands::ExpandParachute as u8,
        TelemetryCommand::ReinitSensors => FlightComputerCommands::ReinitSensors as u8,
        TelemetryCommand::LaunchSignal => FlightComputerCommands::LaunchSignal as u8,
        TelemetryCommand::EvaluationRelax => FlightComputerCommands::EvaluationRelax as u8,
        TelemetryCommand::EvaluationFocus => FlightComputerCommands::EvaluationFocus as u8,
        TelemetryCommand::EvaluationAbort => FlightComputerCommands::EvaluationAbort as u8,
        TelemetryCommand::ReinitBarometer => FlightComputerCommands::ReinitBarometer as u8,
        TelemetryCommand::EnableIMU => FlightComputerCommands::EnableIMU as u8,
        TelemetryCommand::DisableIMU => FlightComputerCommands::DisableIMU as u8,
        TelemetryCommand::MonitorAltitude => FlightComputerCommands::MonitorAltitude as u8,
        TelemetryCommand::RevokeMonitorAltitude => {
            FlightComputerCommands::RevokeMonitorAltitude as u8
        }
        TelemetryCommand::ConsecutiveSamples => FlightComputerCommands::ConsecutiveSamples as u8,
        TelemetryCommand::RevokeConsecutiveSamples => {
            FlightComputerCommands::RevokeConsecutiveSamples as u8
        }
        TelemetryCommand::ResetFailures => FlightComputerCommands::ResetFailures as u8,
        TelemetryCommand::RevokeResetFailures => FlightComputerCommands::RevokeResetFailures as u8,
        TelemetryCommand::ValidateMeasms => FlightComputerCommands::ValidateMeasms as u8,
        TelemetryCommand::RevokeValidateMeasms => {
            FlightComputerCommands::RevokeValidateMeasms as u8
        }
        TelemetryCommand::AbortAfter15 => FlightComputerCommands::AbortAfter15 as u8,
        TelemetryCommand::AbortAfter40 => FlightComputerCommands::AbortAfter40 as u8,
        TelemetryCommand::AbortAfter70 => FlightComputerCommands::AbortAfter70 as u8,
        TelemetryCommand::ReinitAfter12 => FlightComputerCommands::ReinitAfter12 as u8,
        TelemetryCommand::ReinitAfter26 => FlightComputerCommands::ReinitAfter26 as u8,
        TelemetryCommand::ReinitAfter44 => FlightComputerCommands::ReinitAfter44 as u8,
        _ => return None,
    })
}

#[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
const OPERATOR_MODE_FLIGHT_STATE_ORDER: [FlightState; 16] = [
    FlightState::Startup,
    FlightState::Idle,
    FlightState::PreFill,
    FlightState::FillTest,
    FlightState::NitrogenFill,
    FlightState::NitrousFill,
    FlightState::Armed,
    FlightState::Launch,
    FlightState::Ascent,
    FlightState::Coast,
    FlightState::Apogee,
    FlightState::ParachuteDeploy,
    FlightState::Descent,
    FlightState::Landed,
    FlightState::Recovery,
    FlightState::Aborted,
];

#[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
fn operator_mode_adjacent_flight_state(current: FlightState, delta: i32) -> FlightState {
    let idx = OPERATOR_MODE_FLIGHT_STATE_ORDER
        .iter()
        .position(|s| *s == current)
        .unwrap_or(0) as i32;
    let next_idx =
        (idx + delta).clamp(0, (OPERATOR_MODE_FLIGHT_STATE_ORDER.len() - 1) as i32) as usize;
    OPERATOR_MODE_FLIGHT_STATE_ORDER[next_idx]
}

#[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
async fn set_local_flight_state_for_operator_mode(state: &Arc<AppState>, next_state: FlightState) {
    {
        let mut fs = state.state.lock().unwrap();
        *fs = next_state;
    }
    let _ = state.state_tx.send(FlightStateMsg { state: next_state });
    state.broadcast_fill_targets_snapshot();
    let ts_ms = get_current_timestamp_ms() as i64;
    state.update_launch_clock_for_state(next_state, ts_ms);
    let _ = state
        .db_queue_tx
        .send(DbQueueItem::Write(DbWrite::FlightState {
            timestamp_ms: ts_ms,
            state_code: next_state as i64,
        }))
        .await;
}

static DB_BACKPRESSURE_LAST_LOG_MS: AtomicU64 = AtomicU64::new(0);
static DB_BACKPRESSURE_DROPPED: AtomicU64 = AtomicU64::new(0);
static DB_LAST_BUCKET_BY_TYPE: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
static DB_OVERFLOW_LAST_LOG_MS: AtomicU64 = AtomicU64::new(0);
static BATTERY_ESTIMATOR_STATE: OnceLock<Mutex<HashMap<String, BatteryEstimatorState>>> =
    OnceLock::new();
static SPEED_ESTIMATOR_STATE: OnceLock<Mutex<SpeedEstimatorState>> = OnceLock::new();
static BATTERY_LAYOUT_CFG: OnceLock<layout::BatteryLayoutConfig> = OnceLock::new();
static NETWORK_TIME_ROUTER: OnceLock<Arc<Router>> = OnceLock::new();
const BATTERY_VOLTAGE_EMA_ALPHA: f32 = 0.06;
const BATTERY_DROP_RATE_EMA_ALPHA: f32 = 0.10;
const BATTERY_MAX_VOLTAGE_SLEW_V_PER_SEC: f32 = 0.035;
const BATTERY_MIN_VOLTAGE_DEFAULT: f32 = 6.3;
const BATTERY_MAX_VOLTAGE_DEFAULT: f32 = 8.4;

#[derive(Clone)]
struct DbOverflow {
    queue: Arc<Mutex<VecDeque<DbWrite>>>,
    notify: Arc<Notify>,
    running: Arc<AtomicBool>,
    max_entries: usize,
}

fn env_usize(name: &str, default: usize, min: usize, max: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
        .clamp(min, max)
}

fn drop_db_writes_on_backpressure() -> bool {
    static DROP: OnceLock<bool> = OnceLock::new();
    *DROP.get_or_init(|| std::env::var("GS_DB_DROP_ON_BACKPRESSURE").ok().as_deref() == Some("1"))
}

fn db_backpressure_log_interval_ms() -> u64 {
    static INTERVAL: OnceLock<u64> = OnceLock::new();
    *INTERVAL.get_or_init(|| {
        std::env::var("GS_DB_BACKPRESSURE_LOG_INTERVAL_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(60_000)
            .clamp(1_000, 3_600_000)
    })
}

fn db_backpressure_log_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("GS_DB_BACKPRESSURE_LOG").ok().as_deref() != Some("0"))
}

fn db_bucket_ms() -> i64 {
    static BUCKET_MS: OnceLock<i64> = OnceLock::new();
    *BUCKET_MS.get_or_init(|| {
        std::env::var("GS_DB_BUCKET_MS")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(20)
            .clamp(1, 1_000)
    })
}

fn should_persist_telemetry_sample(data_type: &str, ts_ms: i64) -> bool {
    let bucket_ms = db_bucket_ms();
    if bucket_ms <= 1 {
        return true;
    }
    let bucket_id = ts_ms.div_euclid(bucket_ms);
    let map = DB_LAST_BUCKET_BY_TYPE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut by_type = map.lock().unwrap();
    match by_type.get(data_type).copied() {
        Some(prev) if prev == bucket_id => false,
        _ => {
            by_type.insert(data_type.to_string(), bucket_id);
            true
        }
    }
}

#[derive(Default)]
struct BatteryEstimatorState {
    samples: VecDeque<(i64, f32)>,
    ema_voltage: Option<f32>,
    ema_drop_rate_v_per_min: Option<f32>,
    ema_remaining_min: Option<f32>,
    last_ts_ms: Option<i64>,
    last_remaining_ts_ms: Option<i64>,
}

#[derive(Default)]
struct SpeedEstimatorState {
    speed_mps: Option<f32>,
    last_update_ts_ms: Option<i64>,
    accel_mps2: Option<f32>,
    accel_ts_ms: Option<i64>,
    last_baro_alt_sample: Option<(i64, f32)>,
    baro_speed_mps: Option<f32>,
    baro_speed_ts_ms: Option<i64>,
    last_gps_alt_sample: Option<(i64, f32)>,
    gps_speed_mps: Option<f32>,
    gps_speed_ts_ms: Option<i64>,
}

fn battery_layout_cfg() -> &'static layout::BatteryLayoutConfig {
    BATTERY_LAYOUT_CFG.get_or_init(|| match layout::load_layout() {
        Ok(cfg) => cfg.battery,
        Err(err) => {
            eprintln!("WARNING: failed to load battery layout config: {err}");
            layout::BatteryLayoutConfig::default()
        }
    })
}

fn push_battery_sample_and_compute_drop_rate(
    source_id: &str,
    ts_ms: i64,
    voltage: f32,
    window_ms: i64,
) -> (f32, Option<f32>) {
    let by_source = BATTERY_ESTIMATOR_STATE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = by_source.lock().unwrap();
    let state = map.entry(source_id.to_string()).or_default();

    let dt_s = state
        .last_remaining_ts_ms
        .map(|t0| (ts_ms.saturating_sub(t0) as f32 / 1000.0).clamp(0.0, 10.0))
        .unwrap_or(0.0);
    state.last_remaining_ts_ms = Some(ts_ms);
    state.last_ts_ms = Some(ts_ms);

    // Clamp abrupt jumps first, then apply low-alpha EMA for heavy smoothing.
    let slewed = if let Some(prev) = state.ema_voltage {
        let max_step = BATTERY_MAX_VOLTAGE_SLEW_V_PER_SEC * dt_s.max(0.02);
        voltage.clamp(prev - max_step, prev + max_step)
    } else {
        voltage
    };
    let smoothed_voltage = state
        .ema_voltage
        .map(|prev| prev + BATTERY_VOLTAGE_EMA_ALPHA * (slewed - prev))
        .unwrap_or(slewed);
    state.ema_voltage = Some(smoothed_voltage);

    state.samples.push_back((ts_ms, smoothed_voltage));
    while let Some((old_ts, _)) = state.samples.front().copied() {
        if ts_ms.saturating_sub(old_ts) <= window_ms {
            break;
        }
        state.samples.pop_front();
    }

    if state.samples.len() < 2 {
        return (smoothed_voltage, None);
    }

    let t0 = state.samples.front().map(|(t, _)| *t).unwrap_or(ts_ms);
    let n = state.samples.len() as f64;
    let mut sum_x = 0.0f64;
    let mut sum_y = 0.0f64;
    let mut sum_xy = 0.0f64;
    let mut sum_x2 = 0.0f64;

    for (t, v) in state.samples.iter() {
        let x = ((*t - t0) as f64) / 1000.0;
        let y = *v as f64;
        sum_x += x;
        sum_y += y;
        sum_xy += x * y;
        sum_x2 += x * x;
    }

    let denom = n * sum_x2 - (sum_x * sum_x);
    if denom.abs() < f64::EPSILON {
        return (smoothed_voltage, None);
    }

    let slope_v_per_sec = (n * sum_xy - (sum_x * sum_y)) / denom;
    if slope_v_per_sec >= 0.0 {
        let dr = state
            .ema_drop_rate_v_per_min
            .map(|prev| prev + BATTERY_DROP_RATE_EMA_ALPHA * (0.0 - prev))
            .unwrap_or(0.0);
        state.ema_drop_rate_v_per_min = Some(dr);
        return (smoothed_voltage, Some(dr));
    }

    let raw_drop = (-slope_v_per_sec * 60.0) as f32;
    let smoothed_drop = state
        .ema_drop_rate_v_per_min
        .map(|prev| prev + BATTERY_DROP_RATE_EMA_ALPHA * (raw_drop - prev))
        .unwrap_or(raw_drop);
    state.ema_drop_rate_v_per_min = Some(smoothed_drop);
    (smoothed_voltage, Some(smoothed_drop))
}

fn battery_percent(voltage: f32, empty: f32, full: f32, exponent: f32) -> f32 {
    if full <= empty {
        return 0.0;
    }
    let linear = ((voltage - empty) / (full - empty)).clamp(0.0, 1.0);
    let exp = exponent.max(0.1);
    (linear.powf(exp) * 100.0).clamp(0.0, 100.0)
}

fn update_speed_ema(prev: Option<f32>, sample: f32, alpha: f32) -> f32 {
    prev.map(|v| v + alpha * (sample - v)).unwrap_or(sample)
}

fn ingest_altitude_velocity_sample(
    prev_sample: Option<(i64, f32)>,
    prev_speed: Option<f32>,
    ts_ms: i64,
    altitude_m: f32,
    min_dt_ms: i64,
    max_dt_ms: i64,
    alpha: f32,
) -> (Option<(i64, f32)>, Option<f32>, Option<i64>) {
    let next_sample = Some((ts_ms, altitude_m));
    let Some((prev_ts_ms, prev_altitude_m)) = prev_sample else {
        return (next_sample, prev_speed, None);
    };
    let dt_ms = ts_ms.saturating_sub(prev_ts_ms);
    if dt_ms < min_dt_ms || dt_ms > max_dt_ms {
        return (next_sample, prev_speed, None);
    }
    let dt_s = dt_ms as f32 / 1000.0;
    if dt_s <= 0.0 {
        return (next_sample, prev_speed, None);
    }
    let raw_speed_mps = ((altitude_m - prev_altitude_m) / dt_s).clamp(-800.0, 800.0);
    (
        next_sample,
        Some(update_speed_ema(prev_speed, raw_speed_mps, alpha)),
        Some(ts_ms),
    )
}

fn fresh_sensor_value(
    sample: Option<f32>,
    sample_ts_ms: Option<i64>,
    now_ms: i64,
    max_age_ms: i64,
) -> Option<f32> {
    let value = sample?;
    let sample_ts_ms = sample_ts_ms?;
    (now_ms.saturating_sub(sample_ts_ms) <= max_age_ms).then_some(value)
}

fn update_vehicle_speed_estimate(
    data_type: &str,
    ts_ms: i64,
    values: &[Option<f32>],
) -> Option<f32> {
    let state_cell =
        SPEED_ESTIMATOR_STATE.get_or_init(|| Mutex::new(SpeedEstimatorState::default()));
    let mut state = state_cell.lock().unwrap();

    match data_type {
        dt if dt == DataType::AccelData.as_str() => {
            if let Some(accel_z_mps2) = values.get(2).copied().flatten()
                && accel_z_mps2.is_finite()
            {
                state.accel_mps2 = Some((accel_z_mps2 - GRAVITY_MPS2).clamp(-200.0, 200.0));
                state.accel_ts_ms = Some(ts_ms);
            }
        }
        dt if dt == DataType::BarometerData.as_str() => {
            if let Some(altitude_m) = values.get(2).copied().flatten()
                && altitude_m.is_finite()
            {
                (
                    state.last_baro_alt_sample,
                    state.baro_speed_mps,
                    state.baro_speed_ts_ms,
                ) = ingest_altitude_velocity_sample(
                    state.last_baro_alt_sample,
                    state.baro_speed_mps,
                    ts_ms,
                    altitude_m,
                    10,
                    2_000,
                    0.22,
                );
            }
        }
        dt if dt == DataType::GpsData.as_str() => {
            if let Some(altitude_m) = values.get(2).copied().flatten()
                && altitude_m.is_finite()
            {
                (
                    state.last_gps_alt_sample,
                    state.gps_speed_mps,
                    state.gps_speed_ts_ms,
                ) = ingest_altitude_velocity_sample(
                    state.last_gps_alt_sample,
                    state.gps_speed_mps,
                    ts_ms,
                    altitude_m,
                    100,
                    10_000,
                    0.15,
                );
            }
        }
        _ => return None,
    }

    let accel_mps2 = fresh_sensor_value(state.accel_mps2, state.accel_ts_ms, ts_ms, 600);
    let baro_speed_mps =
        fresh_sensor_value(state.baro_speed_mps, state.baro_speed_ts_ms, ts_ms, 1_500);
    let gps_speed_mps =
        fresh_sensor_value(state.gps_speed_mps, state.gps_speed_ts_ms, ts_ms, 4_500);

    let dt_s = state
        .last_update_ts_ms
        .map(|last_ts_ms| (ts_ms.saturating_sub(last_ts_ms) as f32 / 1000.0).clamp(0.0, 0.25))
        .unwrap_or(0.0);

    let mut fused_speed_mps = state.speed_mps.unwrap_or_else(|| {
        let mut seed = 0.0;
        let mut weight = 0.0;
        if let Some(v) = baro_speed_mps {
            seed += v * 0.75;
            weight += 0.75;
        }
        if let Some(v) = gps_speed_mps {
            seed += v * 0.25;
            weight += 0.25;
        }
        if weight > 0.0 { seed / weight } else { 0.0 }
    });

    if let Some(a) = accel_mps2
        && dt_s > 0.0
    {
        fused_speed_mps += a * dt_s;
    }

    let mut has_measurement = false;
    if let Some(v_baro) = baro_speed_mps {
        fused_speed_mps += 0.35 * (v_baro - fused_speed_mps);
        has_measurement = true;
    }
    if let Some(v_gps) = gps_speed_mps {
        fused_speed_mps += 0.18 * (v_gps - fused_speed_mps);
        has_measurement = true;
    }

    if !has_measurement && state.speed_mps.is_none() {
        return None;
    }

    if let Some(prev_speed_mps) = state.speed_mps {
        let smooth_alpha = if dt_s <= 0.0 {
            1.0
        } else {
            (dt_s / 0.12).clamp(0.15, 1.0)
        };
        fused_speed_mps = prev_speed_mps + smooth_alpha * (fused_speed_mps - prev_speed_mps);
    }

    if fused_speed_mps.abs() < 0.02 {
        fused_speed_mps = 0.0;
    }

    fused_speed_mps = fused_speed_mps.clamp(-800.0, 800.0);
    state.speed_mps = Some(fused_speed_mps);
    state.last_update_ts_ms = Some(ts_ms);
    Some(fused_speed_mps)
}

fn battery_bounds_for_source(source: &layout::BatterySourceConfig) -> (f32, f32) {
    let mut empty = if source.empty_voltage.is_finite() {
        source.empty_voltage
    } else {
        BATTERY_MIN_VOLTAGE_DEFAULT
    };
    let mut full = if source.full_voltage.is_finite() {
        source.full_voltage
    } else {
        BATTERY_MAX_VOLTAGE_DEFAULT
    };

    if full <= empty {
        empty = BATTERY_MIN_VOLTAGE_DEFAULT;
        full = BATTERY_MAX_VOLTAGE_DEFAULT;
    }
    (empty, full)
}

fn telemetry_values_json(values: &[Option<f32>]) -> Option<String> {
    serde_json::to_string(
        &values
            .iter()
            .map(|v| v.map(|n| n as f64))
            .collect::<Vec<_>>(),
    )
    .ok()
}

#[allow(clippy::too_many_arguments)]
async fn emit_derived_battery_rows(
    state: &Arc<AppState>,
    db_tx: &mpsc::Sender<DbQueueItem>,
    db_overflow: &DbOverflow,
    ts_ms: i64,
    sender_id: &str,
    input_data_type: &str,
    voltage: f32,
    payload_json: &str,
) {
    let cfg = battery_layout_cfg().clone();
    if cfg.sources.is_empty() {
        return;
    }

    let window_ms = (cfg.estimator.window_seconds.max(30) as i64) * 1000;
    let min_drop_rate = cfg.estimator.min_drop_rate_v_per_min.max(0.0001);

    for source in cfg.sources.iter() {
        if source.sender_id != sender_id || source.input_data_type != input_data_type {
            continue;
        }

        let (smoothed_voltage, drop_rate_v_per_min) =
            push_battery_sample_and_compute_drop_rate(&source.id, ts_ms, voltage, window_ms);

        let (empty_v, full_v) = battery_bounds_for_source(source);
        let pct = battery_percent(smoothed_voltage, empty_v, full_v, source.curve_exponent);
        let raw_remaining_min = drop_rate_v_per_min.and_then(|rate| {
            if rate < min_drop_rate {
                return None;
            }
            let remaining_voltage = (smoothed_voltage - empty_v).max(0.0);
            Some(remaining_voltage / rate)
        });
        let remaining_min = smooth_remaining_minutes(&source.id, ts_ms, raw_remaining_min);

        let rows: [(&str, Vec<Option<f32>>); 3] = [
            (&source.percent_data_type, vec![Some(pct)]),
            (&source.drop_rate_data_type, vec![drop_rate_v_per_min]),
            (&source.remaining_minutes_data_type, vec![remaining_min]),
        ];

        for (data_type, values) in rows {
            if should_persist_telemetry_sample(data_type, ts_ms) {
                queue_db_write(
                    state,
                    db_tx,
                    db_overflow,
                    DbWrite::Telemetry {
                        timestamp_ms: ts_ms,
                        data_type: data_type.to_string(),
                        sender_id: sender_id.to_string(),
                        values_json: telemetry_values_json(&values),
                        payload_json: payload_json.to_string(),
                    },
                )
                .await;
            }

            let row = TelemetryRow {
                timestamp_ms: ts_ms,
                data_type: data_type.to_string(),
                sender_id: sender_id.to_string(),
                values,
            };
            state.cache_recent_telemetry(row.clone());
            let _ = state.ws_tx.send(row);
        }
    }
}

struct DerivedLoadcellSample<'a> {
    ts_ms: i64,
    sender_id: &'a str,
    sensor_id: &'a str,
    raw_value: f32,
    payload_json: &'a str,
}

async fn emit_derived_loadcell_rows(
    state: &Arc<AppState>,
    db_tx: &mpsc::Sender<DbQueueItem>,
    db_overflow: &DbOverflow,
    sample: DerivedLoadcellSample<'_>,
) {
    let cfg = state.loadcell_calibration.lock().unwrap().clone();
    let Some(calibrated_value) =
        loadcell::calibrated_sensor_value(&cfg, sample.sensor_id, sample.raw_value)
    else {
        return;
    };
    let rows: Vec<(&str, Vec<Option<f32>>)> = match sample.sensor_id {
        loadcell::RAW_LOADCELL_DATA_TYPE_1000KG => {
            let percent = loadcell::fill_percent(&cfg, calibrated_value);
            {
                let mut latest = state.latest_fill_mass_kg.lock().unwrap();
                *latest = Some(calibrated_value);
            }
            vec![
                (
                    loadcell::DERIVED_WEIGHT_DATA_TYPE,
                    vec![Some(calibrated_value)],
                ),
                (
                    loadcell::DERIVED_FILL_PERCENT_DATA_TYPE,
                    vec![Some(percent)],
                ),
            ]
        }
        loadcell::RAW_LOADCELL_DATA_TYPE_50KG => {
            let percent = loadcell::fill_percent(&cfg, calibrated_value);
            vec![
                (
                    loadcell::DERIVED_50KG_CALIBRATED_DATA_TYPE,
                    vec![Some(calibrated_value)],
                ),
                (
                    loadcell::DERIVED_FILL_PERCENT_DATA_TYPE,
                    vec![Some(percent)],
                ),
            ]
        }
        loadcell::RAW_PRESSURE_TRANSDUCER_DATA_TYPE => vec![(
            loadcell::DERIVED_PRESSURE_TRANSDUCER_CALIBRATED_DATA_TYPE,
            vec![Some(calibrated_value)],
        )],
        _ => Vec::new(),
    };

    for (data_type, values) in rows {
        if should_persist_telemetry_sample(data_type, sample.ts_ms) {
            queue_db_write(
                state,
                db_tx,
                db_overflow,
                DbWrite::Telemetry {
                    timestamp_ms: sample.ts_ms,
                    data_type: data_type.to_string(),
                    sender_id: sample.sender_id.to_string(),
                    values_json: telemetry_values_json(&values),
                    payload_json: sample.payload_json.to_string(),
                },
            )
            .await;
        }

        let row = TelemetryRow {
            timestamp_ms: sample.ts_ms,
            data_type: data_type.to_string(),
            sender_id: sample.sender_id.to_string(),
            values,
        };
        state.cache_recent_telemetry(row.clone());
        let _ = state.ws_tx.send(row);
    }
}

async fn emit_derived_vehicle_speed_row(
    state: &Arc<AppState>,
    db_tx: &mpsc::Sender<DbQueueItem>,
    db_overflow: &DbOverflow,
    ts_ms: i64,
    speed_mps: f32,
    payload_json: &str,
) {
    let values = vec![Some(speed_mps)];
    if should_persist_telemetry_sample(VEHICLE_SPEED_DATA_TYPE, ts_ms) {
        queue_db_write(
            state,
            db_tx,
            db_overflow,
            DbWrite::Telemetry {
                timestamp_ms: ts_ms,
                data_type: VEHICLE_SPEED_DATA_TYPE.to_string(),
                sender_id: String::new(),
                values_json: telemetry_values_json(&values),
                payload_json: payload_json.to_string(),
            },
        )
        .await;
    }

    let row = TelemetryRow {
        timestamp_ms: ts_ms,
        data_type: VEHICLE_SPEED_DATA_TYPE.to_string(),
        sender_id: String::new(),
        values,
    };
    state.cache_recent_telemetry(row.clone());
    let _ = state.ws_tx.send(row);
}

fn normalized_gps_values(
    state: &Arc<AppState>,
    sender_id: &str,
    raw_values: &[Option<f32>],
) -> Vec<Option<f32>> {
    let lat = raw_values.first().copied().flatten();
    let lon = raw_values.get(1).copied().flatten();
    let alt = raw_values.get(2).copied().flatten();

    {
        let mut fixes = state.latest_gps_fix_by_sender.lock().unwrap();
        fixes.insert(sender_id.to_string(), vec![lat, lon, alt]);
    }

    vec![lat, lon, alt]
}

fn f32_values_from_payload_bytes(bytes: &[u8]) -> Option<Vec<Option<f32>>> {
    if bytes.is_empty() || bytes.len() % std::mem::size_of::<f32>() != 0 {
        return None;
    }

    Some(
        bytes
            .chunks_exact(std::mem::size_of::<f32>())
            .map(|chunk| Some(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])))
            .collect(),
    )
}

fn telemetry_f32_values(pkt: &Packet) -> Option<Vec<Option<f32>>> {
    match pkt.data_as_f32() {
        Ok(values) => Some(values.into_iter().map(Some).collect()),
        Err(_) if pkt.data_type() == DataType::GpsData => {
            f32_values_from_payload_bytes(pkt.payload())
        }
        Err(_) => None,
    }
}

async fn handle_gps_satellite_count_packet(
    state: &Arc<AppState>,
    db_tx: &mpsc::Sender<DbQueueItem>,
    db_overflow: &DbOverflow,
    pkt: &Packet,
    payload_json: &str,
) -> Option<TelemetryRow> {
    let count = pkt.data_as_u8().ok().and_then(|v| v.first().copied())?;
    let ts_ms = pkt.timestamp() as i64;
    let sender_id = pkt.sender().to_string();

    {
        let mut sats = state.latest_gps_satellites_by_sender.lock().unwrap();
        sats.insert(sender_id.clone(), count);
    }

    let values = vec![Some(count as f32)];
    if should_persist_telemetry_sample(GPS_SATELLITES_DATA_TYPE, ts_ms) {
        queue_db_write(
            state,
            db_tx,
            db_overflow,
            DbWrite::Telemetry {
                timestamp_ms: ts_ms,
                data_type: GPS_SATELLITES_DATA_TYPE.to_string(),
                sender_id: sender_id.clone(),
                values_json: telemetry_values_json(&values),
                payload_json: payload_json.to_string(),
            },
        )
        .await;
    }

    Some(TelemetryRow {
        timestamp_ms: ts_ms,
        data_type: GPS_SATELLITES_DATA_TYPE.to_string(),
        sender_id,
        values,
    })
}

fn smooth_remaining_minutes(source_id: &str, ts_ms: i64, raw: Option<f32>) -> Option<f32> {
    const REMAINING_EMA_ALPHA: f32 = 0.05;
    const REMAINING_MAX_STEP_MIN_PER_SEC: f32 = 0.08;

    let by_source = BATTERY_ESTIMATOR_STATE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = by_source.lock().unwrap();
    let state = map.entry(source_id.to_string()).or_default();
    let Some(raw_val) = raw else {
        state.ema_remaining_min = None;
        return None;
    };

    let dt_s = state
        .last_ts_ms
        .map(|t0| (ts_ms.saturating_sub(t0) as f32 / 1000.0).clamp(0.0, 10.0))
        .unwrap_or(0.0);
    let prev = state.ema_remaining_min.unwrap_or(raw_val);
    let max_step = REMAINING_MAX_STEP_MIN_PER_SEC * dt_s.max(0.02);
    let slewed = raw_val.clamp(prev - max_step, prev + max_step);
    let smoothed = prev + REMAINING_EMA_ALPHA * (slewed - prev);
    state.ema_remaining_min = Some(smoothed.max(0.0));
    state.ema_remaining_min
}

pub fn set_network_time_router(router: Arc<Router>) {
    let _ = NETWORK_TIME_ROUTER.set(router);
}

fn schedule_launch_command_after_delay(state: Arc<AppState>, router: Arc<Router>) {
    tokio::spawn(async move {
        let mut shutdown_rx = state.shutdown_subscribe();
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(LAUNCH_COMMAND_DELAY_MS)) => {}
            recv = shutdown_rx.recv() => {
                match recv {
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) | Err(broadcast::error::RecvError::Closed) => {}
                }
                gs_debug_println!("Delayed launch command canceled by shutdown");
                return;
            }
        }

        let current_state = { *state.state.lock().unwrap() };
        if current_state == FlightState::Aborted {
            gs_debug_println!("Delayed launch command canceled because flight state is Aborted");
            return;
        }

        send_launch_command(&state, &router);
    });
}

fn send_launch_command(state: &AppState, router: &Router) {
    #[cfg(feature = "test_fire_mode")]
    {
        let _ = state;
        if let Err(e) = router.log_queue(
            DataType::ActuatorCommand,
            &[ActuatorBoardCommands::IgniterSequence as u8],
        ) {
            log_telemetry_error("failed to log delayed test-fire Launch command", e);
        } else {
            log_command_queue_success(
                "Delayed test-fire Launch command",
                DataType::ActuatorCommand,
                &[ActuatorBoardCommands::IgniterSequence as u8],
            );
            flush_command_tx(router, "Delayed test-fire Launch command tx");
        }
        gs_debug_println!("Delayed test-fire launch command sent to actuator board");
    }

    #[cfg(not(feature = "test_fire_mode"))]
    {
        if let Err(e) = router.log_queue(
            DataType::FlightCommand,
            &[crate::rocket_commands::FlightCommands::Launch as u8],
        ) {
            log_telemetry_error("failed to log delayed Launch command", e);
        } else {
            log_command_queue_success(
                "Delayed Launch command",
                DataType::FlightCommand,
                &[crate::rocket_commands::FlightCommands::Launch as u8],
            );
            flush_command_tx(router, "Delayed Launch command tx");
        }
        state
            .gpio
            .write_output_pin(IGNITION_PIN, true)
            .expect("failed to set gpio output");
        gs_debug_println!("Delayed launch command sent");
    }
}

pub async fn telemetry_task(
    state: Arc<AppState>,
    router: Arc<sedsprintf_rs_2026::router::Router>,
    comms: Vec<CommsWorkerHandle>,
    mut rx: mpsc::Receiver<TelemetryCommand>,
    mut db_rx: mpsc::Receiver<DbQueueItem>,
    mut shutdown_rx: broadcast::Receiver<()>,
) {
    let mut handle_interval = interval(Duration::from_millis(1));
    let mut heartbeat_interval = interval(Duration::from_millis(500));
    handle_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    heartbeat_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut heartbeat_failed = false;
    let mut last_backpressure_log_ms: u64 = 0;
    let packet_work_queue_size = env_usize(
        "GS_PACKET_WORK_QUEUE_SIZE",
        PACKET_WORK_QUEUE_SIZE,
        1024,
        262_144,
    );
    let packet_enqueue_burst = env_usize("GS_PACKET_ENQUEUE_BURST", PACKET_ENQUEUE_BURST, 32, 4096);
    let (packet_tx, mut packet_rx) = mpsc::channel::<Packet>(packet_work_queue_size);
    let db_overflow = DbOverflow {
        queue: Arc::new(Mutex::new(VecDeque::new())),
        notify: Arc::new(Notify::new()),
        running: Arc::new(AtomicBool::new(true)),
        max_entries: env_usize("GS_DB_OVERFLOW_MAX", 250_000, 1024, 5_000_000),
    };

    let db_worker = {
        let state = state.clone();
        let db_batch_max = env_usize("GS_DB_BATCH_MAX", DB_BATCH_MAX_DEFAULT, 1, 4096);
        let db_batch_wait_ms = std::env::var("GS_DB_BATCH_WAIT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DB_BATCH_WAIT_MS_DEFAULT)
            .clamp(1, 250);
        tokio::spawn(async move {
            let mut db_shutdown_rx = state.shutdown_subscribe();
            struct ActiveRecording {
                pool: sqlx::SqlitePool,
                path: String,
            }

            let placeholder_path = state.placeholder_db_path.clone();
            let recordings_dir = std::path::Path::new(&placeholder_path)
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .to_path_buf();
            let mut recent_writes: VecDeque<DbWrite> = VecDeque::new();
            let mut active_recording: Option<ActiveRecording> = None;
            let mut mode = RecordingMode::Idle;
            let mut last_recording_end_ms: Option<i64> = None;

            let mut pending: Vec<DbWrite> = Vec::with_capacity(db_batch_max);
            let mut shutting_down = false;
            loop {
                let first = if shutting_down {
                    match db_rx.try_recv() {
                        Ok(item) => item,
                        Err(MpscTryRecvError::Empty | MpscTryRecvError::Disconnected) => break,
                    }
                } else {
                    tokio::select! {
                        recv = db_rx.recv() => {
                            let Some(item) = recv else { break; };
                            item
                        }
                        recv = db_shutdown_rx.recv() => {
                            match recv {
                                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) | Err(broadcast::error::RecvError::Closed) => {
                                    shutting_down = true;
                                    continue;
                                }
                            }
                        }
                    }
                };

                let mut deferred_control: Option<RecordingCommand> = None;
                match first {
                    DbQueueItem::Write(write) => {
                        let newest_ts = write.timestamp_ms();
                        recent_writes.push_back(write.clone());
                        prune_recent_writes(&mut recent_writes, newest_ts);
                        if matches!(mode, RecordingMode::Recording) {
                            pending.push(write);
                        }
                    }
                    DbQueueItem::Control(cmd) => deferred_control = Some(cmd),
                }

                let deadline =
                    tokio::time::Instant::now() + Duration::from_millis(db_batch_wait_ms);
                while pending.len() < db_batch_max && deferred_control.is_none() {
                    match db_rx.try_recv() {
                        Ok(DbQueueItem::Write(write)) => {
                            let newest_ts = write.timestamp_ms();
                            recent_writes.push_back(write.clone());
                            prune_recent_writes(&mut recent_writes, newest_ts);
                            if matches!(mode, RecordingMode::Recording) {
                                pending.push(write);
                            }
                        }
                        Ok(DbQueueItem::Control(cmd)) => {
                            deferred_control = Some(cmd);
                        }
                        Err(MpscTryRecvError::Disconnected) => break,
                        Err(MpscTryRecvError::Empty) => {
                            let now = tokio::time::Instant::now();
                            if now >= deadline {
                                break;
                            }
                            let remaining = deadline.saturating_duration_since(now);
                            match tokio::time::timeout(remaining, db_rx.recv()).await {
                                Ok(Some(DbQueueItem::Write(write))) => {
                                    let newest_ts = write.timestamp_ms();
                                    recent_writes.push_back(write.clone());
                                    prune_recent_writes(&mut recent_writes, newest_ts);
                                    if matches!(mode, RecordingMode::Recording) {
                                        pending.push(write);
                                    }
                                }
                                Ok(Some(DbQueueItem::Control(cmd))) => {
                                    deferred_control = Some(cmd);
                                }
                                Ok(None) => break,
                                Err(_) => break,
                            }
                        }
                    }
                }

                flush_recording_batch(active_recording.as_ref().map(|a| &a.pool), &mut pending)
                    .await;

                if let Some(cmd) = deferred_control {
                    match cmd {
                        RecordingCommand::StartNow | RecordingCommand::StartWithRecent => {
                            if let Some(active) = active_recording.take() {
                                close_and_finalize_sqlite(active.pool, &active.path).await;
                                last_recording_end_ms = Some(get_current_timestamp_ms() as i64);
                            }

                            let started_at_ms = get_current_timestamp_ms() as i64;
                            let target_path = session_db_path(&recordings_dir, started_at_ms);
                            match open_telemetry_db(&target_path).await {
                                Ok((pool, path)) => {
                                    state.replace_telemetry_db(pool.clone(), path.clone());
                                    state.set_recording_status(RecordingStatusMsg {
                                        mode: RecordingModeWire::Recording,
                                        db_path: Some(path.clone()),
                                    });
                                    active_recording = Some(ActiveRecording { pool, path });
                                    mode = RecordingMode::Recording;

                                    if matches!(cmd, RecordingCommand::StartWithRecent)
                                        && let Some(active) = &active_recording
                                    {
                                        let cutoff = started_at_ms.saturating_sub(120_000);
                                        let min_ts = last_recording_end_ms
                                            .map(|prev| prev.max(cutoff))
                                            .unwrap_or(cutoff);
                                        let backfill: Vec<DbWrite> = recent_writes
                                            .iter()
                                            .filter(|write| write.timestamp_ms() > min_ts)
                                            .cloned()
                                            .collect();
                                        if let Err(err) =
                                            insert_db_batch_with_retry(&active.pool, &backfill)
                                                .await
                                        {
                                            eprintln!("Failed to backfill recent writes: {err}");
                                        }
                                    }
                                }
                                Err(err) => {
                                    emit_warning(
                                        &state,
                                        format!("Failed to open recording DB session: {err}"),
                                    );
                                }
                            }
                        }
                        RecordingCommand::Pause | RecordingCommand::Stop => {
                            if let Some(active) = active_recording.take() {
                                close_and_finalize_sqlite(active.pool, &active.path).await;
                                last_recording_end_ms = Some(get_current_timestamp_ms() as i64);
                            }
                            mode = if matches!(cmd, RecordingCommand::Pause) {
                                RecordingMode::Paused
                            } else {
                                RecordingMode::Idle
                            };
                            rotate_placeholder_db(&state, &placeholder_path, mode).await;
                            state.set_recording_status(RecordingStatusMsg {
                                mode: RecordingModeWire::from(mode),
                                db_path: Some(state.telemetry_db_path()),
                            });
                        }
                    }
                }
            }

            if let Some(active) = active_recording.take() {
                close_and_finalize_sqlite(active.pool, &active.path).await;
            }
        })
    };

    let db_overflow_worker = {
        let db_tx = state.db_queue_tx.clone();
        let db_overflow = db_overflow.clone();
        tokio::spawn(async move {
            while db_overflow.running.load(Ordering::Relaxed) {
                db_overflow.notify.notified().await;
                loop {
                    let next = {
                        let mut q = db_overflow.queue.lock().unwrap();
                        q.pop_front()
                    };
                    let Some(write) = next else {
                        break;
                    };
                    if db_tx.send(DbQueueItem::Write(write)).await.is_err() {
                        return;
                    }
                }
            }
        })
    };

    let packet_worker = {
        let state = state.clone();
        let db_tx = state.db_queue_tx.clone();
        let db_overflow = db_overflow.clone();
        tokio::spawn(async move {
            while let Some(pkt) = packet_rx.recv().await {
                if let Some(row) = handle_packet(&state, &db_tx, &db_overflow, pkt).await {
                    state.cache_recent_telemetry(row.clone());
                    let _ = state.ws_tx.send(row);
                }
            }
        })
    };

    let router_worker = match spawn_router_worker_thread(router.clone(), state.clone()) {
        Ok(worker) => Some(worker),
        Err(err) => {
            eprintln!("Failed to spawn router worker thread: {err}");
            None
        }
    };

    let comms_workers: Vec<_> = comms
        .into_iter()
        .flat_map(|comms_handle| {
            let router = router.clone();
            let state = state.clone();
            match spawn_comms_worker_threads(router, state, comms_handle) {
                Ok(handles) => handles,
                Err(err) => {
                    eprintln!("Failed to spawn comms worker thread: {err}");
                    Vec::new()
                }
            }
        })
        .collect();

    loop {
        tokio::select! {
            biased;

                Some(cmd) = rx.recv() => {
                    if !state.is_command_allowed(&cmd) {
                        emit_warning(
                            &state,
                            format!("Command {cmd:?} blocked by sequence/key interlock"),
                        );
                        continue;
                    }
                    state.record_command_accepted(&cmd, get_current_timestamp_ms());
                    if flight_sim::handle_command(&cmd) {
                        continue;
                    }
                    match cmd {
                        TelemetryCommand::Launch => {
                                if matches!(
                                    state.launch_clock_snapshot().kind,
                                    LaunchClockKind::TMinus | LaunchClockKind::TPlus
                                ) {
                                    gs_debug_println!("Launch command ignored because launch clock is already running");
                                    continue;
                                }
                                if state.recording_status_snapshot().mode != RecordingModeWire::Recording {
                                    let _ = state.db_queue_tx.send(DbQueueItem::Control(RecordingCommand::StartNow)).await;
                                    gs_debug_println!("Launch auto-started DB recording");
                                }
                                let now_ms = get_current_timestamp_ms() as i64;
                                state.set_launch_clock(launch_countdown_clock(now_ms));
                                schedule_launch_command_after_delay(state.clone(), router.clone());
                                gs_debug_println!("Launch command scheduled after {LAUNCH_COMMAND_DELAY_MS} ms");
                            }
                        TelemetryCommand::Dump => {
                                let key = ValveBoardCommands::DumpOpen as u8;
                                let is_on = state.get_umbilical_valve_state(key).unwrap_or(false);
                                let cmd = if is_on {
                                    ValveBoardCommands::DumpClose
                                } else {
                                    ValveBoardCommands::DumpOpen
                                };
                                if let Err(e) = router.log_queue(
                                    DataType::ValveCommand,
                                    &[cmd as u8],
                                ) {
                                    log_telemetry_error("failed to log Dump command", e);
                                } else {
                                    log_command_queue_success("Dump command", DataType::ValveCommand, &[cmd as u8]);
                                    flush_command_tx(&router, "Dump command tx");
                                }
                                {
                                    let gpio = &state.gpio;
                                    gpio.write_output_pin(IGNITION_PIN, false).expect("failed to set gpio output");
                                }
                                gs_debug_println!("Dump command sent {:?}", cmd);
                            }
                        TelemetryCommand::Abort => {
                                if let Err(e) = router.log(
                                    DataType::Abort,
                                    "Manual Abort Command Issued".as_ref(),
                                ) {
                                    log_telemetry_error("failed to log Abort command", e);
                                }
                                gs_debug_println!("Abort command sent");
                            }
                        TelemetryCommand::Igniter => {
                                let key = ActuatorBoardCommands::IgniterOn as u8;
                                let is_on = state.get_umbilical_valve_state(key).unwrap_or(false);
                                let cmd = if is_on {
                                    ActuatorBoardCommands::IgniterOff
                                } else {
                                    ActuatorBoardCommands::IgniterOn
                                };
                                if let Err(e) = router.log_queue(
                                    DataType::ActuatorCommand,
                                    &[cmd as u8],
                                ) {
                                    log_telemetry_error("failed to log Igniter command", e);
                                } else {
                                    log_command_queue_success("Igniter command", DataType::ActuatorCommand, &[cmd as u8]);
                                    flush_command_tx(&router, "Igniter command tx");
                                }
                                gs_debug_println!("Igniter command sent {:?}", cmd);
                            }
                        TelemetryCommand::Pilot => {
                                let key = ValveBoardCommands::PilotOpen as u8;
                                let is_on = state.get_umbilical_valve_state(key).unwrap_or(false);
                                let cmd = if is_on {
                                    ValveBoardCommands::PilotClose
                                } else {
                                    ValveBoardCommands::PilotOpen
                                };
                                if let Err(e) = router.log_queue(
                                    DataType::ValveCommand,
                                    &[cmd as u8],
                                ) {
                                    log_telemetry_error("failed to log Pilot command", e);
                                } else {
                                    log_command_queue_success("Pilot command", DataType::ValveCommand, &[cmd as u8]);
                                    flush_command_tx(&router, "Pilot command tx");
                                }
                                gs_debug_println!("Pilot command sent {:?}", cmd);
                            }
                        TelemetryCommand::NormallyOpen => {
                                let key = ValveBoardCommands::NormallyOpenOpen as u8;
                                let is_on = state.get_umbilical_valve_state(key).unwrap_or(false);
                                let cmd = if is_on {
                                    ValveBoardCommands::NormallyOpenClose
                                } else {
                                    ValveBoardCommands::NormallyOpenOpen
                                };
                                if let Err(e) = router.log_queue(
                                    DataType::ValveCommand,
                                    &[cmd as u8],
                                ) {
                                    log_telemetry_error("failed to log NormallyOpen command", e);
                                } else {
                                    log_command_queue_success("NormallyOpen command", DataType::ValveCommand, &[cmd as u8]);
                                    flush_command_tx(&router, "NormallyOpen command tx");
                                }
                                gs_debug_println!("Tanks command sent {:?}", cmd);
                            }
                        TelemetryCommand::Nitrogen => {
                                let cmd_id = ActuatorBoardCommands::NitrogenOpen as u8;
                                let is_on = state.get_umbilical_valve_state(cmd_id).unwrap_or(false);
                                let cmd = if is_on {
                                    ActuatorBoardCommands::NitrogenClose
                                } else {
                                    ActuatorBoardCommands::NitrogenOpen
                                };
                                if let Err(e) = router.log_queue(
                                    DataType::ActuatorCommand,
                                    &[cmd as u8],
                                ) {
                                    log_telemetry_error("failed to log Nitrogen command", e);
                                } else {
                                    log_command_queue_success("Nitrogen command", DataType::ActuatorCommand, &[cmd as u8]);
                                    flush_command_tx(&router, "Nitrogen command tx");
                                }
                                gs_debug_println!("Nitrogen command sent {:?}", cmd);
                            }
                        TelemetryCommand::NitrogenClose => {
                                if let Err(e) = router.log_queue(
                                    DataType::ActuatorCommand,
                                    &[ActuatorBoardCommands::NitrogenClose as u8],
                                ) {
                                    log_telemetry_error("failed to log NitrogenClose command", e);
                                } else {
                                    log_command_queue_success(
                                        "NitrogenClose command",
                                        DataType::ActuatorCommand,
                                        &[ActuatorBoardCommands::NitrogenClose as u8],
                                    );
                                    flush_command_tx(&router, "NitrogenClose command tx");
                                }
                                gs_debug_println!("Nitrogen explicit close command sent");
                            }
                        TelemetryCommand::RetractPlumbing => {
                                if let Err(e) = router.log_queue(
                                    DataType::ActuatorCommand,
                                    &[ActuatorBoardCommands::RetractPlumbing as u8],
                                ) {
                                    log_telemetry_error("failed to log RetractPlumbing command", e);
                                } else {
                                    log_command_queue_success(
                                        "RetractPlumbing command",
                                        DataType::ActuatorCommand,
                                        &[ActuatorBoardCommands::RetractPlumbing as u8],
                                    );
                                    flush_command_tx(&router, "RetractPlumbing command tx");
                                }
                                gs_debug_println!("RetractPlumbing command sent");
                        }
                        TelemetryCommand::Nitrous => {
                                let cmd_id = ActuatorBoardCommands::NitrousOpen as u8;
                                let is_on = state.get_umbilical_valve_state(cmd_id).unwrap_or(false);
                                let cmd = if is_on {
                                    ActuatorBoardCommands::NitrousClose
                                } else {
                                    ActuatorBoardCommands::NitrousOpen
                                };
                                if let Err(e) = router.log_queue(
                                    DataType::ActuatorCommand,
                                    &[cmd as u8],
                                ) {
                                    log_telemetry_error("failed to log Nitrous command", e);
                                } else {
                                    log_command_queue_success("Nitrous command", DataType::ActuatorCommand, &[cmd as u8]);
                                    flush_command_tx(&router, "Nitrous command tx");
                                }
                                gs_debug_println!("Nitrous command sent: {:?}", cmd);
                        }
                        TelemetryCommand::NitrousClose => {
                                if let Err(e) = router.log_queue(
                                    DataType::ActuatorCommand,
                                    &[ActuatorBoardCommands::NitrousClose as u8],
                                ) {
                                    log_telemetry_error("failed to log NitrousClose command", e);
                                } else {
                                    log_command_queue_success(
                                        "NitrousClose command",
                                        DataType::ActuatorCommand,
                                        &[ActuatorBoardCommands::NitrousClose as u8],
                                    );
                                    flush_command_tx(&router, "NitrousClose command tx");
                                }
                                gs_debug_println!("Nitrous explicit close command sent");
                        }
                        TelemetryCommand::StartWritingNow => {
                                let _ = state.db_queue_tx.send(DbQueueItem::Control(RecordingCommand::StartNow)).await;
                                gs_debug_println!("DB recording started without backfill");
                        }
                        TelemetryCommand::StartWritingLastTwoMinutes => {
                                let _ = state.db_queue_tx.send(DbQueueItem::Control(RecordingCommand::StartWithRecent)).await;
                                gs_debug_println!("DB recording started with recent backfill");
                        }
                        TelemetryCommand::PauseWritingDb => {
                                let _ = state.db_queue_tx.send(DbQueueItem::Control(RecordingCommand::Pause)).await;
                                gs_debug_println!("DB recording paused");
                        }
                        TelemetryCommand::StopWritingDb => {
                                let _ = state.db_queue_tx.send(DbQueueItem::Control(RecordingCommand::Stop)).await;
                                gs_debug_println!("DB recording stopped");
                        }
                        TelemetryCommand::ContinueFillSequence => {
                                state.request_fill_sequence_continue();
                                gs_debug_println!("ContinueFillSequence command accepted");
                        }
                        #[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
                        TelemetryCommand::AdvanceFlightState => {
                                let current = *state.state.lock().unwrap();
                                let next = operator_mode_adjacent_flight_state(current, 1);
                                set_local_flight_state_for_operator_mode(&state, next).await;
                                gs_debug_println!("Operator-mode flight state advanced: {:?} -> {:?}", current, next);
                        }
                        #[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
                        TelemetryCommand::RewindFlightState => {
                                let current = *state.state.lock().unwrap();
                                let next = operator_mode_adjacent_flight_state(current, -1);
                                set_local_flight_state_for_operator_mode(&state, next).await;
                                gs_debug_println!("Operator-mode flight state rewound: {:?} -> {:?}", current, next);
                        }
                        #[cfg(feature = "hitl_mode")]
                        TelemetryCommand::DeployParachute
                        | TelemetryCommand::ExpandParachute
                        | TelemetryCommand::ReinitSensors
                        | TelemetryCommand::LaunchSignal
                        | TelemetryCommand::EvaluationRelax
                        | TelemetryCommand::EvaluationFocus
                        | TelemetryCommand::EvaluationAbort
                        | TelemetryCommand::ReinitBarometer
                        | TelemetryCommand::EnableIMU
                        | TelemetryCommand::DisableIMU
                        | TelemetryCommand::MonitorAltitude
                        | TelemetryCommand::RevokeMonitorAltitude
                        | TelemetryCommand::ConsecutiveSamples
                        | TelemetryCommand::RevokeConsecutiveSamples
                        | TelemetryCommand::ResetFailures
                        | TelemetryCommand::RevokeResetFailures
                        | TelemetryCommand::ValidateMeasms
                        | TelemetryCommand::RevokeValidateMeasms
                        | TelemetryCommand::AbortAfter15
                        | TelemetryCommand::AbortAfter40
                        | TelemetryCommand::AbortAfter70
                        | TelemetryCommand::ReinitAfter12
                        | TelemetryCommand::ReinitAfter26
                        | TelemetryCommand::ReinitAfter44 => {
                                if let Some(cmd_id) = hitl_flight_command_id(&cmd) {
                                    if let Err(e) = router.log_queue(DataType::FlightCommand, &[cmd_id]) {
                                        log_telemetry_error("failed to log HITL flight command", e);
                                    }
                                    gs_debug_println!("HITL flight command sent: {:?} ({cmd_id})", cmd);
                                }
                        }
                    }
                }
                _ = heartbeat_interval.tick() => {
                    if router.log_queue::<u8>(DataType::Heartbeat, &[]).is_ok() {
                        state.mark_board_seen(
                            Board::GroundStation.sender_id(),
                            get_current_timestamp_ms(),
                        );
                        heartbeat_failed = false;
                    } else if !heartbeat_failed {
                            emit_warning(
                                &state,
                                "Warning: Ground Station heartbeat send failed",
                            );
                            heartbeat_failed = true;

                    }
                }
                _ = handle_interval.tick() => {
                    for _ in 0..packet_enqueue_burst {
                        match packet_tx.try_reserve() {
                            Ok(permit) => {
                                let pkt = {
                                    let mut rb = state.ring_buffer.lock().unwrap();
                                    rb.pop_oldest()
                                };
                                let Some(pkt) = pkt else {
                                    break;
                                };
                                permit.send(pkt);
                            }
                            Err(TrySendError::Full(_)) => {
                                let now_ms = get_current_timestamp_ms();
                                if now_ms.saturating_sub(last_backpressure_log_ms)
                                    >= BACKPRESSURE_LOG_INTERVAL_MS
                                {
                                    eprintln!(
                                        "Telemetry ingest backpressured: processing queue is full"
                                    );
                                    last_backpressure_log_ms = now_ms;
                                }
                                break;
                            }
                            Err(TrySendError::Closed(_)) => {
                                emit_warning(
                                    &state,
                                    "Warning: telemetry processing worker stopped unexpectedly",
                                );
                                break;
                            }
                        }
                    }
                }
                recv = shutdown_rx.recv() => {
                    match recv {
                        Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) | Err(broadcast::error::RecvError::Closed) => {
                            break;
                        }
                    }
                }
        }
    }

    let worker_shutdown_timeout = Duration::from_secs(10);

    for worker in comms_workers {
        match tokio::time::timeout(
            worker_shutdown_timeout,
            tokio::task::spawn_blocking(move || worker.join()),
        )
        .await
        {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(_))) => eprintln!("Comms worker panicked during shutdown"),
            Ok(Err(e)) => eprintln!("Comms worker join task failed: {e}"),
            Err(_) => eprintln!(
                "Comms worker did not shut down within {:?}",
                worker_shutdown_timeout
            ),
        }
    }

    if let Some(worker) = router_worker {
        match tokio::time::timeout(
            worker_shutdown_timeout,
            tokio::task::spawn_blocking(move || worker.join()),
        )
        .await
        {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(_))) => eprintln!("Router worker panicked during shutdown"),
            Ok(Err(e)) => eprintln!("Router worker join task failed: {e}"),
            Err(_) => eprintln!(
                "Router worker did not shut down within {:?}",
                worker_shutdown_timeout
            ),
        }
    }

    // Stop intake first, then wait for packet worker to drain packet queue.
    drop(packet_tx);
    match tokio::time::timeout(worker_shutdown_timeout, packet_worker).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("Packet worker ended with error: {e}"),
        Err(_) => eprintln!(
            "Packet worker did not shut down within {:?}",
            worker_shutdown_timeout
        ),
    }

    db_overflow.running.store(false, Ordering::Relaxed);
    db_overflow.notify.notify_waiters();
    match tokio::time::timeout(worker_shutdown_timeout, db_overflow_worker).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("DB overflow worker ended with error: {e}"),
        Err(_) => eprintln!(
            "DB overflow worker did not shut down within {:?}",
            worker_shutdown_timeout
        ),
    }

    match tokio::time::timeout(worker_shutdown_timeout, db_worker).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("DB worker ended with error: {e}"),
        Err(_) => eprintln!(
            "DB worker did not shut down within {:?}",
            worker_shutdown_timeout
        ),
    }
}

fn umbilical_state_key(cmd_id: u8, on: bool) -> Option<(u8, bool)> {
    use ActuatorBoardCommands as A;
    use ValveBoardCommands as V;

    match cmd_id {
        x if x == V::PilotOpen as u8 => Some((V::PilotOpen as u8, on)),
        x if x == V::PilotClose as u8 => Some((V::PilotOpen as u8, false)),
        x if x == V::NormallyOpenOpen as u8 => Some((V::NormallyOpenOpen as u8, on)),
        x if x == V::NormallyOpenClose as u8 => Some((V::NormallyOpenOpen as u8, false)),
        x if x == V::DumpOpen as u8 => Some((V::DumpOpen as u8, on)),
        x if x == V::DumpClose as u8 => Some((V::DumpOpen as u8, false)),
        x if x == A::IgniterOn as u8 => Some((A::IgniterOn as u8, on)),
        x if x == A::IgniterOff as u8 => Some((A::IgniterOn as u8, false)),
        x if x == A::NitrogenOpen as u8 => Some((A::NitrogenOpen as u8, on)),
        x if x == A::NitrogenClose as u8 => Some((A::NitrogenOpen as u8, false)),
        x if x == A::NitrousOpen as u8 => Some((A::NitrousOpen as u8, on)),
        x if x == A::NitrousClose as u8 => Some((A::NitrousOpen as u8, false)),
        x if x == A::RetractPlumbing as u8 => Some((A::RetractPlumbing as u8, on)),
        _ => None,
    }
}

const VALVE_STATE_DATA_TYPE: &str = "VALVE_STATE";

fn bool_to_f32(value: Option<bool>) -> Option<f32> {
    value.map(|v| if v { 1.0 } else { 0.0 })
}

fn valve_state_values(state: &AppState) -> [Option<f32>; 8] {
    use ActuatorBoardCommands as A;
    use ValveBoardCommands as V;

    [
        bool_to_f32(state.get_umbilical_valve_state(V::PilotOpen as u8)),
        bool_to_f32(state.get_umbilical_valve_state(V::NormallyOpenOpen as u8)),
        bool_to_f32(state.get_umbilical_valve_state(V::DumpOpen as u8)),
        bool_to_f32(state.get_umbilical_valve_state(A::IgniterOn as u8)),
        bool_to_f32(state.get_umbilical_valve_state(A::NitrogenOpen as u8)),
        bool_to_f32(state.get_umbilical_valve_state(A::NitrousOpen as u8)),
        bool_to_f32(state.get_umbilical_valve_state(A::RetractPlumbing as u8)),
        None,
    ]
}

const DB_RETRIES: usize = 5;
const DB_RETRY_DELAY_MS: u64 = 50;

async fn insert_db_batch_once(
    db: &sqlx::SqlitePool,
    writes: &[DbWrite],
) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    for write in writes {
        match write {
            DbWrite::FlightState {
                timestamp_ms,
                state_code,
            } => {
                sqlx::query("INSERT INTO flight_state (timestamp_ms, f_state) VALUES (?, ?)")
                    .bind(*timestamp_ms)
                    .bind(*state_code)
                    .execute(&mut *tx)
                    .await?;
            }
            DbWrite::Telemetry {
                timestamp_ms,
                data_type,
                sender_id,
                values_json,
                payload_json,
            } => {
                sqlx::query(
                    "INSERT INTO telemetry (timestamp_ms, data_type, sender_id, values_json, payload_json) VALUES (?, ?, ?, ?, ?)",
                )
                    .bind(*timestamp_ms)
                    .bind(data_type.as_str())
                    .bind(sender_id.as_str())
                    .bind(values_json.as_deref())
                    .bind(payload_json.as_str())
                    .execute(&mut *tx)
                    .await?;
            }
            DbWrite::Alert {
                timestamp_ms,
                severity,
                message,
            } => {
                sqlx::query(
                    "INSERT INTO alerts (timestamp_ms, severity, message) VALUES (?, ?, ?)",
                )
                .bind(*timestamp_ms)
                .bind(severity.as_str())
                .bind(message.as_str())
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    tx.commit().await
}

async fn insert_db_batch_with_retry(
    db: &sqlx::SqlitePool,
    writes: &[DbWrite],
) -> Result<(), sqlx::Error> {
    let mut delay = DB_RETRY_DELAY_MS;
    let mut last_err: Option<sqlx::Error> = None;

    for _ in 0..=DB_RETRIES {
        let result = insert_db_batch_once(db, writes).await;
        match result {
            Ok(_) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(delay)).await;
                delay = (delay * 2).min(1000);
            }
        }
    }

    Err(last_err.unwrap())
}

async fn flush_recording_batch(active: Option<&sqlx::SqlitePool>, batch: &mut Vec<DbWrite>) {
    if batch.is_empty() {
        return;
    }
    if let Some(active) = active
        && let Err(e) = insert_db_batch_with_retry(active, batch).await
    {
        eprintln!("DB insert failed after retry: {e}");
    }
    batch.clear();
}

async fn rotate_placeholder_db(state: &Arc<AppState>, placeholder_path: &str, mode: RecordingMode) {
    let old_pool = state.telemetry_db_pool();
    let old_path = state.telemetry_db_path();
    if old_path != placeholder_path {
        close_and_finalize_sqlite(old_pool, &old_path).await;
    }
    let _ = std::fs::remove_file(placeholder_path);
    match open_telemetry_db(std::path::Path::new(placeholder_path)).await {
        Ok((db, db_path)) => {
            state.replace_telemetry_db(db, db_path.clone());
            state.set_recording_status(RecordingStatusMsg {
                mode: RecordingModeWire::from(mode),
                db_path: Some(db_path),
            });
        }
        Err(err) => {
            emit_warning(
                state,
                format!("Failed to reopen placeholder telemetry DB: {err}"),
            );
        }
    }
}

async fn queue_db_write(
    state: &AppState,
    db_tx: &mpsc::Sender<DbQueueItem>,
    db_overflow: &DbOverflow,
    write: DbWrite,
) {
    if drop_db_writes_on_backpressure() {
        match db_tx.try_send(DbQueueItem::Write(write)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                DB_BACKPRESSURE_DROPPED.fetch_add(1, Ordering::Relaxed);
                if !db_backpressure_log_enabled() {
                    return;
                }
                let now_ms = get_current_timestamp_ms();
                let prev = DB_BACKPRESSURE_LAST_LOG_MS.load(Ordering::Relaxed);
                if now_ms.saturating_sub(prev) >= db_backpressure_log_interval_ms() {
                    DB_BACKPRESSURE_LAST_LOG_MS.store(now_ms, Ordering::Relaxed);
                    let dropped = DB_BACKPRESSURE_DROPPED.swap(0, Ordering::Relaxed);
                    eprintln!(
                        "Telemetry DB backpressured: dropped {} DB rows (realtime ingest preserved)",
                        dropped
                    );
                }
            }
            Err(TrySendError::Closed(_)) => {
                emit_warning(state, "Warning: telemetry DB worker stopped unexpectedly");
            }
        }
        return;
    }

    match db_tx.try_send(DbQueueItem::Write(write)) {
        Ok(()) => {}
        Err(TrySendError::Full(DbQueueItem::Write(write))) => {
            let mut write_opt = Some(write);
            let mut queued_len = 0usize;
            let mut pushed = false;
            {
                let mut q = db_overflow.queue.lock().unwrap();
                if q.len() < db_overflow.max_entries {
                    q.push_back(write_opt.take().unwrap());
                    queued_len = q.len();
                    pushed = true;
                }
            }
            if pushed {
                db_overflow.notify.notify_one();
                let now_ms = get_current_timestamp_ms();
                let prev = DB_OVERFLOW_LAST_LOG_MS.load(Ordering::Relaxed);
                if now_ms.saturating_sub(prev) >= 60_000 {
                    DB_OVERFLOW_LAST_LOG_MS.store(now_ms, Ordering::Relaxed);
                    eprintln!(
                        "Telemetry DB overflow queue buffered {} pending rows (no drop mode)",
                        queued_len
                    );
                }
            } else if db_tx
                .send(DbQueueItem::Write(write_opt.take().unwrap()))
                .await
                .is_err()
            {
                emit_warning(state, "Warning: telemetry DB worker stopped unexpectedly");
            }
        }
        Err(TrySendError::Full(DbQueueItem::Control(_))) => {
            emit_warning(
                state,
                "Warning: telemetry DB worker control queue backpressured",
            );
        }
        Err(TrySendError::Closed(_)) => {
            emit_warning(state, "Warning: telemetry DB worker stopped unexpectedly");
        }
    }
}

async fn handle_packet(
    state: &Arc<AppState>,
    db_tx: &mpsc::Sender<DbQueueItem>,
    db_overflow: &DbOverflow,
    pkt: Packet,
) -> Option<TelemetryRow> {
    state.mark_board_seen(pkt.sender(), get_current_timestamp_ms());

    if pkt.data_type() == DataType::Warning {
        if let Ok(msg) = pkt.data_as_string() {
            emit_warning(state, msg.to_string());
        } else {
            emit_warning(state, "Warning packet with invalid UTF-8 payload");
        }
        return None;
    }

    if pkt.data_type() == DataType::FlightState {
        if !cfg!(feature = "testing") && !state.all_required_boards_seen() {
            return None;
        }
        let pkt_data = match pkt.data_as_u8() {
            Ok(data) => *data.first().expect("index 0 does not exist"),
            Err(_) => return None,
        };
        let new_flight_state = match u8_to_flight_state(pkt_data) {
            Some(flight_state) => flight_state,
            None => return None,
        };
        {
            let mut fs = state.state.lock().unwrap();
            *fs = new_flight_state;
        }
        let ts_ms = pkt.timestamp() as i64;
        state.update_launch_clock_for_state(new_flight_state, ts_ms);
        queue_db_write(
            state,
            db_tx,
            db_overflow,
            DbWrite::FlightState {
                timestamp_ms: ts_ms,
                state_code: pkt_data as i64,
            },
        )
        .await;

        let _ = state.state_tx.send(FlightStateMsg {
            state: new_flight_state,
        });
        state.broadcast_fill_targets_snapshot();
        return None;
    }

    if pkt.data_type() == DataType::UmbilicalStatus {
        if let Ok(data) = pkt.data_as_u8()
            && data.len() == 2
        {
            let cmd_id = data[0];
            let on = data[1] != 0;
            if let Some((key_cmd_id, key_on)) = umbilical_state_key(cmd_id, on) {
                state.set_umbilical_valve_state(key_cmd_id, key_on);

                let ts_ms = pkt.timestamp() as i64;
                let values = valve_state_values(state);
                let values_vec: Vec<Option<f32>> = values.into_iter().collect();
                let values_json = serde_json::to_string(
                    &values_vec
                        .iter()
                        .map(|v| v.map(|n| n as f64))
                        .collect::<Vec<_>>(),
                )
                .ok();
                let payload_json = payload_json_from_pkt(&pkt);

                queue_db_write(
                    state,
                    db_tx,
                    db_overflow,
                    DbWrite::Telemetry {
                        timestamp_ms: ts_ms,
                        data_type: VALVE_STATE_DATA_TYPE.to_string(),
                        sender_id: Board::GroundStation.sender_id().to_string(),
                        values_json,
                        payload_json,
                    },
                )
                .await;

                let row = TelemetryRow {
                    timestamp_ms: ts_ms,
                    data_type: VALVE_STATE_DATA_TYPE.to_string(),
                    sender_id: Board::GroundStation.sender_id().to_string(),
                    values: values_vec,
                };
                return Some(row);
            }
        }
        return None;
    }

    let data_type_str = pkt.data_type().as_str().to_string();
    let ts_ms = if pkt.data_type() == DataType::GpsData {
        get_current_timestamp_ms() as i64
    } else {
        pkt.timestamp() as i64
    };

    let payload_json = payload_json_from_pkt(&pkt);

    if pkt.data_type() == DataType::GpsSatelliteNumber {
        return handle_gps_satellite_count_packet(state, db_tx, db_overflow, &pkt, &payload_json)
            .await;
    }

    if let Some(mut values_vec) = telemetry_f32_values(&pkt) {
        if pkt.data_type() == DataType::GpsData {
            values_vec = normalized_gps_values(state, pkt.sender(), &values_vec);
        }
        if pkt.data_type() == DataType::FuelTankPressure {
            let latest = values_vec.first().copied().flatten();
            let mut pressure = state.latest_fuel_tank_pressure.lock().unwrap();
            *pressure = latest;
        }
        let values_json = serde_json::to_string(
            &values_vec
                .iter()
                .map(|v| v.map(|n| n as f64))
                .collect::<Vec<_>>(),
        )
        .ok();

        if should_persist_telemetry_sample(&data_type_str, ts_ms) {
            queue_db_write(
                state,
                db_tx,
                db_overflow,
                DbWrite::Telemetry {
                    timestamp_ms: ts_ms,
                    data_type: data_type_str.clone(),
                    sender_id: pkt.sender().to_string(),
                    values_json,
                    payload_json: payload_json.clone(),
                },
            )
            .await;
        }

        if let Some(voltage) = values_vec.first().copied().flatten() {
            let derived_ts_ms = get_current_timestamp_ms() as i64;
            emit_derived_battery_rows(
                state,
                db_tx,
                db_overflow,
                derived_ts_ms,
                pkt.sender(),
                &data_type_str,
                voltage,
                &payload_json,
            )
            .await;

            if matches!(
                data_type_str.as_str(),
                loadcell::RAW_LOADCELL_DATA_TYPE_1000KG
                    | loadcell::RAW_LOADCELL_DATA_TYPE_50KG
                    | loadcell::RAW_PRESSURE_TRANSDUCER_DATA_TYPE
            ) {
                emit_derived_loadcell_rows(
                    state,
                    db_tx,
                    db_overflow,
                    DerivedLoadcellSample {
                        ts_ms: derived_ts_ms,
                        sender_id: pkt.sender(),
                        sensor_id: &data_type_str,
                        raw_value: voltage,
                        payload_json: &payload_json,
                    },
                )
                .await;
            }
        }

        if let Some(speed_mps) = update_vehicle_speed_estimate(&data_type_str, ts_ms, &values_vec) {
            emit_derived_vehicle_speed_row(
                state,
                db_tx,
                db_overflow,
                ts_ms,
                speed_mps,
                &payload_json,
            )
            .await;
        }

        let row = TelemetryRow {
            timestamp_ms: ts_ms,
            data_type: data_type_str,
            sender_id: pkt.sender().to_string(),
            values: values_vec,
        };

        Some(row)
    } else {
        if should_persist_telemetry_sample(&data_type_str, ts_ms) {
            queue_db_write(
                state,
                db_tx,
                db_overflow,
                DbWrite::Telemetry {
                    timestamp_ms: ts_ms,
                    data_type: data_type_str,
                    sender_id: pkt.sender().to_string(),
                    values_json: None,
                    payload_json,
                },
            )
            .await;
        }
        None
    }
}

pub fn get_current_timestamp_ms() -> u64 {
    NETWORK_TIME_ROUTER
        .get()
        .and_then(|router| router.network_time_ms())
        .unwrap_or_else(get_system_timestamp_ms)
}

fn get_system_timestamp_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now();
    let duration_since_epoch = now.duration_since(UNIX_EPOCH).unwrap();
    duration_since_epoch.as_millis() as u64
}

fn log_telemetry_error(context: &str, err: sedsprintf_rs_2026::TelemetryError) {
    eprintln!("{context}: {:?}", err);
}

fn log_command_queue_success(context: &str, ty: DataType, payload: &[u8]) {
    eprintln!(
        "{context}: queued ty={ty:?} payload={}",
        payload
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
}

fn process_router_queues(router: &Router) -> Result<(), sedsprintf_rs_2026::TelemetryError> {
    if let Err(err) = router.process_tx_queue_with_timeout(ROUTER_TX_BUDGET_MS) {
        log_telemetry_error("router tx queue processing failed", err);
        return Ok(());
    }
    if ROUTER_RX_BUDGET_MS > 0 {
        if let Err(err) = router.process_rx_queue_with_timeout(ROUTER_RX_BUDGET_MS) {
            log_telemetry_error("router rx queue processing failed", err);
            return Ok(());
        }
    }
    Ok(())
}

fn flush_command_tx(router: &Router, context: &str) {
    if let Err(err) = router.process_tx_queue_with_timeout(ROUTER_TX_BUDGET_MS) {
        log_telemetry_error(context, err);
    }
}

fn payload_json_from_pkt(pkt: &Packet) -> String {
    let bytes = pkt.payload();
    serde_json::to_string(&bytes).unwrap_or_else(|_| "[]".to_string())
}

pub fn timesync_enabled() -> bool {
    if cfg!(feature = "testing") {
        return std::env::var("GROUNDSTATION_TIMESYNC").ok().as_deref() == Some("1");
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthManager;
    use crate::fill_targets;
    use crate::gpio::GpioPins;
    use crate::loadcell;
    use crate::ring_buffer::RingBuffer;
    use crate::sequences::default_action_policy;
    use crate::telemetry_db::{LaunchClockMsg, RecordingModeWire};
    use crate::types::Board;
    use sqlx::SqlitePool;
    use std::collections::{HashMap, VecDeque};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};
    use std::sync::{Mutex, OnceLock};
    use tokio::sync::{Notify, broadcast, mpsc};

    fn timesync_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn timesync_defaults_on_without_testing_feature() {
        let _guard = timesync_env_lock().lock().unwrap();
        unsafe {
            std::env::remove_var("GROUNDSTATION_TIMESYNC");
        }
        assert!(timesync_enabled());
    }

    #[test]
    fn timesync_env_can_still_enable_it() {
        let _guard = timesync_env_lock().lock().unwrap();
        unsafe {
            std::env::set_var("GROUNDSTATION_TIMESYNC", "1");
        }
        assert!(timesync_enabled());
        unsafe {
            std::env::remove_var("GROUNDSTATION_TIMESYNC");
        }
    }

    fn test_db_overflow() -> DbOverflow {
        DbOverflow {
            queue: Arc::new(Mutex::new(VecDeque::new())),
            notify: Arc::new(Notify::new()),
            running: Arc::new(AtomicBool::new(true)),
            max_entries: 16,
        }
    }

    async fn test_app_state(db_tx: mpsc::Sender<DbQueueItem>) -> Arc<AppState> {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("failed to open in-memory telemetry db");
        let auth_db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("failed to open in-memory auth db");
        let (cmd_tx, _cmd_rx) = mpsc::channel(4);
        let (ws_tx, _ws_rx) = broadcast::channel(16);
        let (state_tx, _state_rx) = broadcast::channel(4);
        let (board_status_tx, _board_status_rx) = broadcast::channel(4);
        let (shutdown_tx, _shutdown_rx) = broadcast::channel(4);
        let (notifications_tx, _notifications_rx) = broadcast::channel(4);
        let (action_policy_tx, _action_policy_rx) = broadcast::channel(4);
        let (fill_targets_tx, _fill_targets_rx) = broadcast::channel(4);
        let (launch_clock_tx, _launch_clock_rx) = broadcast::channel(4);
        let (recording_status_tx, _recording_status_rx) = broadcast::channel(4);

        let mut board_status = HashMap::new();
        for board in Board::ALL {
            board_status.insert(
                *board,
                crate::state::BoardStatus {
                    last_seen_ms: None,
                    ema_gap_ms: None,
                    warned: false,
                },
            );
        }

        Arc::new(AppState {
            ring_buffer: Arc::new(Mutex::new(RingBuffer::new(128))),
            cmd_tx,
            ws_tx,
            warnings_tx: broadcast::channel(4).0,
            errors_tx: broadcast::channel(4).0,
            db: Arc::new(Mutex::new(db)),
            db_path: Arc::new(Mutex::new("sqlite::memory:".to_string())),
            placeholder_db_path: "sqlite::memory:".to_string(),
            db_queue_tx: db_tx,
            auth_db,
            state: Arc::new(Mutex::new(FlightState::Startup)),
            state_tx,
            gpio: GpioPins::new(),
            board_status: Arc::new(Mutex::new(board_status)),
            board_status_tx,
            last_packet_rx_ms: Arc::new(AtomicU64::new(0)),
            umbilical_valve_states: Arc::new(Mutex::new(HashMap::new())),
            latest_fuel_tank_pressure: Arc::new(Mutex::new(None)),
            latest_fill_mass_kg: Arc::new(Mutex::new(None)),
            loadcell_calibration: Arc::new(Mutex::new(loadcell::load_or_default())),
            shutdown_tx,
            pending_db_writes: Arc::new(AtomicUsize::new(0)),
            db_write_notify: Arc::new(Notify::new()),
            notifications: Arc::new(Mutex::new(Vec::new())),
            notifications_tx,
            next_notification_id: Arc::new(AtomicU64::new(0)),
            action_policy: Arc::new(Mutex::new(default_action_policy())),
            action_policy_tx,
            fill_targets: Arc::new(Mutex::new(fill_targets::load_or_default())),
            fill_targets_tx,
            launch_clock: Arc::new(Mutex::new(LaunchClockMsg::idle())),
            launch_clock_tx,
            recording_status: Arc::new(Mutex::new(RecordingStatusMsg {
                mode: RecordingModeWire::Idle,
                db_path: None,
            })),
            recording_status_tx,
            last_command_ms: Arc::new(Mutex::new(HashMap::new())),
            fill_sequence_continue_requests: Arc::new(AtomicU64::new(0)),
            recent_telemetry_cache: Arc::new(Mutex::new(VecDeque::new())),
            latest_gps_fix_by_sender: Arc::new(Mutex::new(HashMap::new())),
            latest_gps_satellites_by_sender: Arc::new(Mutex::new(HashMap::new())),
            recent_alerts_cache: Arc::new(Mutex::new(VecDeque::new())),
            av_bay_comms_connected: Arc::new(AtomicBool::new(false)),
            fill_comms_connected: Arc::new(AtomicBool::new(false)),
            topology_router: Arc::new(OnceLock::new()),
            auth: Arc::new(AuthManager::new(PathBuf::from(
                "/tmp/groundstation-test-users.json",
            ))),
        })
    }

    fn f32_payload(values: &[f32]) -> Arc<[u8]> {
        values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect::<Vec<_>>()
            .into()
    }

    #[tokio::test]
    async fn rf_gps_packets_become_graphable_telemetry_rows() {
        let (db_tx, mut db_rx) = mpsc::channel(8);
        let state = test_app_state(db_tx.clone()).await;
        let db_overflow = test_db_overflow();
        let gps_values = [31.7619_f32, -106.485_f32, 1412.5_f32];
        let gps_pkt = Packet::new(
            DataType::GpsData,
            &[
                sedsprintf_rs_2026::config::DataEndpoint::GroundStation,
                sedsprintf_rs_2026::config::DataEndpoint::SdCard,
                sedsprintf_rs_2026::config::DataEndpoint::FlightController,
            ],
            Board::RFBoard.sender_id(),
            123_456,
            f32_payload(&gps_values),
        )
        .expect("failed to build RF GPS_DATA packet");

        let row = handle_packet(&state, &db_tx, &db_overflow, gps_pkt)
            .await
            .expect("RF GPS_DATA packet should produce telemetry row");

        assert_eq!(row.data_type, DataType::GpsData.as_str());
        assert_eq!(row.sender_id, Board::RFBoard.sender_id());
        assert_eq!(row.values.len(), 3);
        for (actual, expected) in row.values.iter().zip(gps_values) {
            assert_eq!(actual.unwrap(), expected);
        }
        assert_eq!(
            state
                .latest_gps_fix_by_sender
                .lock()
                .unwrap()
                .get(Board::RFBoard.sender_id())
                .cloned(),
            Some(row.values.clone())
        );

        let write = db_rx.recv().await.expect("GPS_DATA should queue DB write");
        match write {
            DbQueueItem::Write(DbWrite::Telemetry {
                data_type,
                sender_id,
                values_json,
                ..
            }) => {
                assert_eq!(data_type, DataType::GpsData.as_str());
                assert_eq!(sender_id, Board::RFBoard.sender_id());
                assert_eq!(
                    serde_json::from_str::<Vec<Option<f32>>>(&values_json.unwrap()).unwrap(),
                    row.values
                );
            }
            other => panic!("unexpected DB item for GPS_DATA: {other:?}"),
        }

        let sat_pkt = Packet::new(
            DataType::GpsSatelliteNumber,
            &[sedsprintf_rs_2026::config::DataEndpoint::GroundStation],
            Board::RFBoard.sender_id(),
            123_789,
            Arc::from([14_u8]),
        )
        .expect("failed to build RF GPS_SATELLITE_NUMBER packet");

        let sat_row = handle_packet(&state, &db_tx, &db_overflow, sat_pkt)
            .await
            .expect("RF GPS_SATELLITE_NUMBER packet should produce telemetry row");

        assert_eq!(sat_row.data_type, GPS_SATELLITES_DATA_TYPE);
        assert_eq!(sat_row.sender_id, Board::RFBoard.sender_id());
        assert_eq!(sat_row.values, vec![Some(14.0)]);
        assert_eq!(
            state
                .latest_gps_satellites_by_sender
                .lock()
                .unwrap()
                .get(Board::RFBoard.sender_id())
                .copied(),
            Some(14)
        );
    }
}
