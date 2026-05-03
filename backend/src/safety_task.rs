use crate::state::AppState;
use crate::telemetry_task::get_current_timestamp_ms;
use crate::types::{Board, FlightState};
use crate::web::{emit_warning, emit_warning_db_only};
use sedsprintf_rs_2026::config::DataType;
use sedsprintf_rs_2026::router::Router;
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use tokio::sync::broadcast;
use tokio::time::{Duration, sleep};

// Acceleration thresholds (m/s²)
const ACCELERATION_X_MIN_THRESHOLD: f32 = -2.0; // m/s²
const ACCELERATION_X_MAX_THRESHOLD: f32 = 2.0; // m/s²
const ACCELERATION_Y_MIN_THRESHOLD: f32 = -2.0; // m/s²
const ACCELERATION_Y_MAX_THRESHOLD: f32 = 2.0; // m/s²
const ACCELERATION_Z_MIN_THRESHOLD: f32 = -2.0; // m/s²
const ACCELERATION_Z_MAX_THRESHOLD: f32 = 100.0; // m/s²

// Gyroscope thresholds (deg/s)
const GYRO_X_MAX_THRESHOLD: f32 = 5.0; // deg/s
const GYRO_Y_MAX_THRESHOLD: f32 = 5.0; // deg/s
const GYRO_Z_MAX_THRESHOLD: f32 = 360.0; // deg/s
const GYRO_X_MIN_THRESHOLD: f32 = -5.0;
const GYRO_Y_MIN_THRESHOLD: f32 = -5.0;
const GYRO_Z_MIN_THRESHOLD: f32 = -360.0;

// Barometric pressure thresholds (Pa)
const BARO_PRESSURE_MIN_THRESHOLD: f32 = 30000.0; // Pa
const BARO_PRESSURE_MAX_THRESHOLD: f32 = 110000.0; // Pa
const BARO_TEMPERATURE_MIN_THRESHOLD: f32 = 0.0; // °C
const BARO_TEMPERATURE_MAX_THRESHOLD: f32 = 85.0; // °
const BARO_ALTITUDE_MIN_THRESHOLD: f32 = -5.0; // m
const BARO_ALTITUDE_MAX_THRESHOLD: f32 = 11000.0; // m

// GPS thresholds
// Should be in texas
#[cfg(not(feature = "hitl_mode"))]
const GPS_LATITUDE_MIN_THRESHOLD: f32 = 25.0; // degrees
#[cfg(not(feature = "hitl_mode"))]
const GPS_LATITUDE_MAX_THRESHOLD: f32 = 36.5; // degrees
#[cfg(not(feature = "hitl_mode"))]
const GPS_LONGITUDE_MIN_THRESHOLD: f32 = -106.5; // degrees
#[cfg(not(feature = "hitl_mode"))]
const GPS_LONGITUDE_MAX_THRESHOLD: f32 = -93.5; // degrees

// Default battery voltage thresholds (V), used as fallback sender ranges.
const BATTERY_VOLTAGE_AV_BAY_MIN_THRESHOLD: f32 = 6.3; // V
const BATTERY_VOLTAGE_AV_BAY_MAX_THRESHOLD: f32 = 8.0; // V
const BATTERY_VOLTAGE_VALVE_BOARD_MIN_THRESHOLD: f32 = 12.3; // V
const BATTERY_VOLTAGE_VALVE_BOARD_MAX_THRESHOLD: f32 = 16.5; // V
const BATTERY_VOLTAGE_GROUND_STATION_MIN_THRESHOLD: f32 = 12.3; // V
const BATTERY_VOLTAGE_GROUND_STATION_MAX_THRESHOLD: f32 = 16.5; // V

// Battery current thresholds (A)
const BATTERY_CURRENT_MIN_THRESHOLD: f32 = 0.0; // A
const BATTERY_CURRENT_MAX_THRESHOLD: f32 = 50.0; // A

// Fuel Tank Pressure thresholds (psi)
const FUEL_TANK_PRESSURE_MIN_THRESHOLD: f32 = 0.0; // psi
const FUEL_TANK_PRESSURE_MAX_THRESHOLD: f32 = 3000.0; // psi

// Kalman Filter thresholds
const KALMAN_STATE_MIN_THRESHOLD: f32 = -5000.0; // arbitrary units
const KALMAN_STATE_MAX_THRESHOLD: f32 = 5000.0; // arbitrary units

#[cfg(not(feature = "testing"))]
const BOARD_TIMEOUT_MS: u64 = 500;
#[cfg(not(feature = "testing"))]
const BOARD_OFFLINE_ABORT_TRIGGER_MS: u64 = 3000;
const FLIGHT_STATE_DB_RETRIES: usize = 5;
const FLIGHT_STATE_DB_RETRY_DELAY_MS: u64 = 50;
const SAFETY_WARNING_COOLDOWN_MS_DEFAULT: u64 = 5_000;

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

fn battery_voltage_bounds_by_sender() -> &'static HashMap<String, (f32, f32)> {
    static BOUNDS: OnceLock<HashMap<String, (f32, f32)>> = OnceLock::new();
    BOUNDS.get_or_init(|| {
        let mut out = HashMap::from([
            (
                Board::PowerBoard.sender_id().to_string(),
                (
                    BATTERY_VOLTAGE_AV_BAY_MIN_THRESHOLD,
                    BATTERY_VOLTAGE_AV_BAY_MAX_THRESHOLD,
                ),
            ),
            (
                Board::GatewayBoard.sender_id().to_string(),
                (
                    BATTERY_VOLTAGE_GROUND_STATION_MIN_THRESHOLD,
                    BATTERY_VOLTAGE_GROUND_STATION_MAX_THRESHOLD,
                ),
            ),
            (
                Board::ValveBoard.sender_id().to_string(),
                (
                    BATTERY_VOLTAGE_VALVE_BOARD_MIN_THRESHOLD,
                    BATTERY_VOLTAGE_VALVE_BOARD_MAX_THRESHOLD,
                ),
            ),
        ]);

        if let Ok(layout) = crate::layout::load_layout() {
            for source in &layout.battery.sources {
                if !source.empty_voltage.is_finite() || !source.full_voltage.is_finite() {
                    continue;
                }
                let (lo, hi) = if source.empty_voltage <= source.full_voltage {
                    (source.empty_voltage, source.full_voltage)
                } else {
                    (source.full_voltage, source.empty_voltage)
                };
                out.insert(source.sender_id.clone(), (lo, hi));
            }
        } else {
            eprintln!(
                "WARNING: safety_task failed to load layout battery bounds; using sender defaults"
            );
        }

        out
    })
}

fn check_accel_thresholds(values: &[f32], warnings: &mut HashSet<&'static str>) {
    if let Some(accel_x) = values.first()
        && ((ACCELERATION_X_MIN_THRESHOLD > *accel_x) || (*accel_x > ACCELERATION_X_MAX_THRESHOLD))
    {
        warnings.insert("Critical: Acceleration X threshold exceeded!");
    }

    if let Some(accel_y) = values.get(1)
        && ((ACCELERATION_Y_MIN_THRESHOLD > *accel_y) || (*accel_y > ACCELERATION_Y_MAX_THRESHOLD))
    {
        warnings.insert("Critical: Acceleration Y threshold exceeded!");
    }

    if let Some(accel_z) = values.get(2)
        && ((ACCELERATION_Z_MIN_THRESHOLD > *accel_z) || (*accel_z > ACCELERATION_Z_MAX_THRESHOLD))
    {
        warnings.insert("Critical: Acceleration Z threshold exceeded!");
    }
}

fn check_gyro_thresholds(values: &[f32], warnings: &mut HashSet<&'static str>) {
    if let Some(gyro_x) = values.first()
        && ((GYRO_X_MIN_THRESHOLD > *gyro_x) || (*gyro_x > GYRO_X_MAX_THRESHOLD))
    {
        warnings.insert("Critical: Gyro X threshold exceeded!");
    }

    if let Some(gyro_y) = values.get(1)
        && ((GYRO_Y_MIN_THRESHOLD > *gyro_y) || (*gyro_y > GYRO_Y_MAX_THRESHOLD))
    {
        warnings.insert("Critical: Gyro Y threshold exceeded!");
    }

    if let Some(gyro_z) = values.get(2)
        && ((GYRO_Z_MIN_THRESHOLD > *gyro_z) || (*gyro_z > GYRO_Z_MAX_THRESHOLD))
    {
        warnings.insert("Critical: Gyro Z threshold exceeded!");
    }
}

async fn insert_flight_state_with_retry(
    db: &SqlitePool,
    timestamp_ms: i64,
    state_code: i64,
) -> Result<(), sqlx::Error> {
    let mut delay = FLIGHT_STATE_DB_RETRY_DELAY_MS;
    let mut last_err: Option<sqlx::Error> = None;

    for _ in 0..=FLIGHT_STATE_DB_RETRIES {
        match sqlx::query("INSERT INTO flight_state (timestamp_ms, f_state) VALUES (?, ?)")
            .bind(timestamp_ms)
            .bind(state_code)
            .execute(db)
            .await
        {
            Ok(_) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                sleep(Duration::from_millis(delay)).await;
                delay = (delay * 2).min(1000);
            }
        }
    }

    Err(last_err.unwrap())
}
#[cfg(feature = "testing")]
const BOARD_TIMEOUT_MS: u64 = 3000;
#[cfg(feature = "testing")]
const BOARD_OFFLINE_ABORT_TRIGGER_MS: u64 = 6000;

pub async fn safety_task(
    state: Arc<AppState>,
    router: Arc<Router>,
    mut shutdown_rx: broadcast::Receiver<()>,
) {
    let mut abort = false;
    let mut last_no_packet_warning_ms: u64 = 0;
    let warning_cooldown_ms = env_u64(
        "GS_SAFETY_WARNING_COOLDOWN_MS",
        SAFETY_WARNING_COOLDOWN_MS_DEFAULT,
    );
    let mut last_warning_emit_ms: HashMap<&'static str, u64> = HashMap::new();
    loop {
        tokio::select! {
            _ = sleep(Duration::from_millis(500)) => {}
            recv = shutdown_rx.recv() => {
                match recv {
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) | Err(broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
        }

        let current_state = { *state.state.lock().unwrap() };
        let on_ground = matches!(
            current_state,
            FlightState::Startup
                | FlightState::Idle
                | FlightState::PreFill
                | FlightState::FillTest
                | FlightState::NitrogenFill
                | FlightState::NitrousFill
                | FlightState::Armed
        );
        let suppress_disconnect_warnings = matches!(
            current_state,
            FlightState::Launch
                | FlightState::Ascent
                | FlightState::Coast
                | FlightState::Apogee
                | FlightState::Descent
                | FlightState::Reefing
                | FlightState::Landed
                | FlightState::Recovery
                | FlightState::Aborted
        );
        let now_ms = get_current_timestamp_ms();
        let sim_mode = crate::flight_sim::sim_mode_enabled();
        let mut board_warnings = Vec::new();
        let mut board_log_only = Vec::new();
        let all_boards_seen = {
            let mut board_status = state.board_status.lock().unwrap();
            for (board, status) in board_status.iter_mut() {
                if !state.board_required_for_progression(*board) {
                    status.warned = false;
                    continue;
                }
                if sim_mode {
                    status.last_seen_ms = Some(now_ms);
                    status.warned = false;
                    continue;
                }

                let offline = match status.last_seen_ms {
                    Some(last_seen_ms) => now_ms.saturating_sub(last_seen_ms) > BOARD_TIMEOUT_MS,
                    None => true,
                };
                let is_ground_board = matches!(
                    board,
                    Board::ValveBoard
                        | Board::ActuatorBoard
                        | Board::DaqBoard
                        | Board::GatewayBoard
                );
                let in_flight_ignored_board = !on_ground && is_ground_board;

                if offline {
                    if !status.warned && !suppress_disconnect_warnings {
                        let msg = format!(
                            "Warning: No messages from {} in >{}ms",
                            board.as_str(),
                            BOARD_TIMEOUT_MS
                        );
                        if current_state != FlightState::Startup {
                            if on_ground {
                                board_warnings.push(msg);
                            } else {
                                board_log_only.push(msg);
                            }
                        }
                        status.warned = true;
                    }

                    let abort_eligible = *board != Board::ValveBoard
                        && !suppress_disconnect_warnings
                        && (on_ground || !is_ground_board);
                    if current_state != FlightState::Startup
                        && abort_eligible
                        && !in_flight_ignored_board
                        && let Some(last_seen_ms) = status.last_seen_ms
                    {
                        let offline_ms = now_ms.saturating_sub(last_seen_ms);
                        if offline_ms >= BOARD_OFFLINE_ABORT_TRIGGER_MS && !abort {
                            abort = true;
                        }
                    }
                } else {
                    status.warned = false;
                }
            }
            Board::ALL.iter().all(|board| {
                if !state.board_required_for_progression(*board) {
                    return true;
                }
                board_status
                    .get(board)
                    .and_then(|s| s.last_seen_ms)
                    .is_some()
            })
        };

        for warning in board_warnings {
            emit_warning(&state, warning);
        }
        for warning in board_log_only {
            emit_warning_db_only(&state, warning);
        }

        let _ = state
            .board_status_tx
            .send(state.board_status_snapshot(now_ms));

        if current_state == FlightState::Startup
            && all_boards_seen
            && !cfg!(feature = "hitl_mode")
            && !cfg!(feature = "test_fire_mode")
        {
            let should_advance = {
                let mut fs = state.state.lock().unwrap();
                if *fs == FlightState::Startup {
                    *fs = FlightState::Idle;
                    true
                } else {
                    false
                }
            };

            if should_advance {
                let ts_ms = get_current_timestamp_ms() as i64;
                state.update_launch_clock_for_state(FlightState::Idle, ts_ms);
                if let Err(e) = insert_flight_state_with_retry(
                    &state.telemetry_db_pool(),
                    ts_ms,
                    FlightState::Idle as i64,
                )
                .await
                {
                    eprintln!("DB insert into flight_state failed after retry: {e}");
                }
                let _ = state.state_tx.send(crate::web::FlightStateMsg {
                    state: FlightState::Idle,
                });
                state.broadcast_fill_targets_snapshot();
            }
        }

        let last_packet_ms = state.last_packet_received_ms();
        if current_state == FlightState::Startup {
            last_no_packet_warning_ms = 0;
        } else if last_packet_ms == 0 || now_ms.saturating_sub(last_packet_ms) > 10_000 {
            if last_no_packet_warning_ms == 0
                || now_ms.saturating_sub(last_no_packet_warning_ms) >= 10_000
            {
                emit_warning(
                    &state,
                    "Warning: No telemetry packets received for 10 seconds!",
                );
                gs_debug_println!("Safety: No telemetry packets received for >=10 seconds!");
                last_no_packet_warning_ms = now_ms;
            }
        } else {
            last_no_packet_warning_ms = 0;
        }

        // Snapshot current packets from the ring buffer for value-range checks.
        let packets = {
            let rb = state.ring_buffer.lock().unwrap();
            let len = rb.len();
            rb.recent(len).into_iter().cloned().collect::<Vec<_>>()
        };

        if packets.is_empty() {
            continue;
        }

        let mut cycle_warnings: HashSet<&'static str> = HashSet::new();

        for pkt in packets {
            match pkt.data_type() {
                DataType::AccelData => {
                    let values = pkt.data_as_f32().unwrap_or_else(|_| vec![0f32; 3]);
                    check_accel_thresholds(&values, &mut cycle_warnings);
                }

                DataType::GyroData => {
                    let values = pkt.data_as_f32().unwrap_or_else(|_| vec![0f32; 3]);
                    check_gyro_thresholds(&values, &mut cycle_warnings);
                }

                DataType::IMUData => {
                    let values = pkt.data_as_f32().unwrap_or_else(|_| vec![0f32; 6]);
                    if values.len() >= 6 {
                        check_accel_thresholds(&values[..3], &mut cycle_warnings);
                        check_gyro_thresholds(&values[3..6], &mut cycle_warnings);
                    }
                }

                DataType::BarometerData => {
                    // [pressure_Pa, temperature_C, altitude_m]
                    let values = pkt.data_as_f32().unwrap_or_else(|_| vec![0f32; 3]);

                    // Pressure
                    if let Some(pressure) = values.first()
                        && ((BARO_PRESSURE_MIN_THRESHOLD > *pressure)
                            || (*pressure > BARO_PRESSURE_MAX_THRESHOLD))
                    {
                        cycle_warnings.insert("Critical: Barometer pressure threshold exceeded!");
                    }

                    // Temperature
                    if let Some(temp) = values.get(1)
                        && ((BARO_TEMPERATURE_MIN_THRESHOLD > *temp)
                            || (*temp > BARO_TEMPERATURE_MAX_THRESHOLD))
                    {
                        cycle_warnings
                            .insert("Critical: Barometer temperature threshold exceeded!");
                    }

                    // Altitude
                    if let Some(alt) = values.get(2)
                        && ((BARO_ALTITUDE_MIN_THRESHOLD > *alt)
                            || (*alt > BARO_ALTITUDE_MAX_THRESHOLD))
                    {
                        cycle_warnings.insert("Critical: Barometer altitude threshold exceeded!");
                    }
                }

                // GPS: [lat, lon] in "xy"
                DataType::GpsData => {
                    #[cfg(not(feature = "hitl_mode"))]
                    {
                        let values = pkt.data_as_f32().unwrap_or_else(|_| vec![0f32; 3]);

                        // Latitude (x)
                        if let Some(lat) = values.first()
                            && ((GPS_LATITUDE_MIN_THRESHOLD > *lat)
                                || (*lat > GPS_LATITUDE_MAX_THRESHOLD))
                        {
                            cycle_warnings
                                .insert("Critical: GPS latitude out of bounds (Texas check)!");
                        }

                        // Longitude (y)
                        if let Some(lon) = values.get(1)
                            && ((GPS_LONGITUDE_MIN_THRESHOLD > *lon)
                                || (*lon > GPS_LONGITUDE_MAX_THRESHOLD))
                        {
                            cycle_warnings
                                .insert("Critical: GPS longitude out of bounds (Texas check)!");
                        }
                    }
                }

                DataType::BatteryCurrent => {
                    let values = pkt.data_as_f32().unwrap_or_else(|_| vec![0f32; 2]);

                    // Current
                    if let Some(current) = values.get(1)
                        && ((BATTERY_CURRENT_MIN_THRESHOLD > *current)
                            || (*current > BATTERY_CURRENT_MAX_THRESHOLD))
                    {
                        cycle_warnings.insert("Critical: Battery current out of range!");
                    }
                }

                DataType::BatteryVoltage => {
                    let values = pkt.data_as_f32().unwrap_or_else(|_| vec![0f32; 2]);
                    // Voltage
                    if let Some(voltage) = values.first()
                        && let Some((min_v, max_v)) = battery_voltage_bounds_by_sender()
                            .get(pkt.sender())
                            .copied()
                        && ((*voltage < min_v) || (*voltage > max_v))
                    {
                        cycle_warnings.insert("Critical: Battery voltage out of range!");
                    }
                }

                // Fuel tank pressure: [pressure_psi, ...]
                DataType::FuelTankPressure => {
                    let values = pkt.data_as_f32().unwrap_or_else(|_| vec![0f32; 1]);

                    if let Some(pressure) = values.first()
                        && ((FUEL_TANK_PRESSURE_MIN_THRESHOLD > *pressure)
                            || (*pressure > FUEL_TANK_PRESSURE_MAX_THRESHOLD))
                    {
                        cycle_warnings.insert("Critical: Fuel tank pressure out of range!");
                    }
                }

                // Ascent/descent Kalman filter state packets.
                DataType::AscentState | DataType::DescentState => {
                    let values = pkt.data_as_f32().unwrap_or_default();

                    if values.iter().any(|value| {
                        !value.is_finite()
                            || *value < KALMAN_STATE_MIN_THRESHOLD
                            || *value > KALMAN_STATE_MAX_THRESHOLD
                    }) {
                        cycle_warnings.insert("Critical: Kalman filter state out of range!");
                    }
                }

                DataType::GenericError => {
                    abort = true;
                    cycle_warnings.insert("Generic Error received from vehicle!");
                    gs_debug_println!("Safety: Generic Error packet received");
                }

                _ => {}
            }
        }

        if !cycle_warnings.is_empty() {
            let mut emitted = cycle_warnings.into_iter().collect::<Vec<_>>();
            emitted.sort_unstable();
            for msg in emitted {
                let last_ms = last_warning_emit_ms.get(msg).copied().unwrap_or(0);
                if now_ms.saturating_sub(last_ms) >= warning_cooldown_ms {
                    emit_warning(&state, msg);
                    last_warning_emit_ms.insert(msg, now_ms);
                }
            }
        }

        if abort {
            router
                .log::<u8>(
                    DataType::Abort,
                    "Safety Task Abort Command Issued".as_bytes(),
                )
                .unwrap_or_else(|e| {
                    eprintln!("failed to log Abort command: {:?}", e);
                });
            gs_debug_println!("Safety task: Abort command sent");
            break;
        }
    }
}
