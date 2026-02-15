use crate::state::AppState;
use crate::telemetry_task::get_current_timestamp_ms;
use crate::web::{emit_warning, emit_warning_db_only};
use groundstation_shared::{Board, FlightState};
use sedsprintf_rs_2026::config::DataType;
use sedsprintf_rs_2026::router::Router;
use sqlx::SqlitePool;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::time::{sleep, Duration};

// Acceleration thresholds (m/s²)
const ACCELERATION_X_MIN_THRESHOLD: f32 = -2.0; // m/s²
const ACCELERATION_X_MAX_THRESHOLD: f32 = 2.0; // m/s²
const ACCELERATION_Y_MIN_THRESHOLD: f32 = -2.0; // m/s²
const ACCELERATION_Y_MAX_THRESHOLD: f32 = 2.0; // m/s²
const ACCELERATION_Z_MIN_THRESHOLD: f32 = -100.0; // m/s²
const ACCELERATION_Z_MAX_THRESHOLD: f32 = 2.0; // m/s²

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
const GPS_LATITUDE_MIN_THRESHOLD: f32 = 25.0; // degrees
const GPS_LATITUDE_MAX_THRESHOLD: f32 = 36.5; // degrees
const GPS_LONGITUDE_MIN_THRESHOLD: f32 = -106.5; // degrees
const GPS_LONGITUDE_MAX_THRESHOLD: f32 = -93.5; // degrees

// Battery voltage thresholds (V)
const BATTERY_VOLTAGE_MIN_THRESHOLD: f32 = 7.0; // V
const BATTERY_VOLTAGE_MAX_THRESHOLD: f32 = 12.6; // V

// Battery current thresholds (A)
const BATTERY_CURRENT_MIN_THRESHOLD: f32 = 0.0; // A
const BATTERY_CURRENT_MAX_THRESHOLD: f32 = 50.0; // A

// Fuel Flow thresholds (L/h)
const FUEL_FLOW_MIN_THRESHOLD: f32 = 0.0; // L/h
const FUEL_FLOW_MAX_THRESHOLD: f32 = 200.0; // L/h
// Fuel Tank Pressure thresholds (psi)
const FUEL_TANK_PRESSURE_MIN_THRESHOLD: f32 = 0.0; // psi
const FUEL_TANK_PRESSURE_MAX_THRESHOLD: f32 = 50.0; // psi

// Kalman Filter thresholds
const KALMAN_X_MIN_THRESHOLD: f32 = -1000.0; // arbitrary units
const KALMAN_X_MAX_THRESHOLD: f32 = 1000.0; // arbitrary units
const KALMAN_Y_MIN_THRESHOLD: f32 = -1000.0; // arbitrary units
const KALMAN_Y_MAX_THRESHOLD: f32 = 1000.0; // arbitrary units
const KALMAN_Z_MIN_THRESHOLD: f32 = -1000.0; // arbitrary units
const KALMAN_Z_MAX_THRESHOLD: f32 = 1000.0; // arbitrary units

#[cfg(not(feature = "testing"))]
const BOARD_TIMEOUT_MS: u64 = 500;
#[cfg(not(feature = "testing"))]
const BOARD_OFFLINE_ABORT_TRIGGER_MS: u64 = 3000;
const FLIGHT_STATE_DB_RETRIES: usize = 5;
const FLIGHT_STATE_DB_RETRY_DELAY_MS: u64 = 50;

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
    let mut count: u64 = 0;
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
            FlightState::Descent
                | FlightState::Landed
                | FlightState::Recovery
                | FlightState::Aborted
        );
        let now_ms = get_current_timestamp_ms();
        let mut board_warnings = Vec::new();
        let mut board_log_only = Vec::new();
        let all_boards_seen = {
            let mut board_status = state.board_status.lock().unwrap();
            for (board, status) in board_status.iter_mut() {
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
                    if abort_eligible
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
            board_status
                .values()
                .all(|status| status.last_seen_ms.is_some())
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

        if current_state == FlightState::Startup && all_boards_seen {
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
                if let Err(e) =
                    insert_flight_state_with_retry(&state.db, ts_ms, FlightState::Idle as i64).await
                {
                    eprintln!("DB insert into flight_state failed after retry: {e}");
                }
                let _ = state.state_tx.send(crate::web::FlightStateMsg {
                    state: FlightState::Idle,
                });
            }
        }

        // Snapshot current packets from the ring buffer
        let packets = {
            let rb = state.ring_buffer.lock().unwrap();
            let len = rb.len();

            if count >= 20 {
                emit_warning(
                    &state,
                    "Warning: No telemetry packets received for 10 seconds!",
                );
                println!("Safety: No telemetry packets received for 20 iterations!");
                count = 0;
            }

            if len == 0 {
                count += 1;
                Vec::new()
            } else {
                count = 0;
                rb.recent(len).into_iter().cloned().collect::<Vec<_>>()
            }
        };

        if packets.is_empty() {
            continue;
        }

        for pkt in packets {
            match pkt.data_type() {
                DataType::AccelData => {
                    let values = pkt.data_as_f32().unwrap_or_else(|_| vec![0f32; 3]);

                    // X axis
                    if let Some(accel_x) = values.first()
                        && ((ACCELERATION_X_MIN_THRESHOLD > *accel_x)
                            || (*accel_x > ACCELERATION_X_MAX_THRESHOLD))
                    {
                        emit_warning(&state, "Critical: Acceleration X threshold exceeded!");
                    }

                    // Y axis
                    if let Some(accel_y) = values.get(1)
                        && ((ACCELERATION_Y_MIN_THRESHOLD > *accel_y)
                            || (*accel_y > ACCELERATION_Y_MAX_THRESHOLD))
                    {
                        emit_warning(&state, "Critical: Acceleration Y threshold exceeded!");
                    }

                    // Z axis
                    if let Some(accel_z) = values.get(2)
                        && ((ACCELERATION_Z_MIN_THRESHOLD > *accel_z)
                            || (*accel_z > ACCELERATION_Z_MAX_THRESHOLD))
                    {
                        emit_warning(&state, "Critical: Acceleration Z threshold exceeded!");
                    }
                }

                DataType::GyroData => {
                    let values = pkt.data_as_f32().unwrap_or_else(|_| vec![0f32; 3]);

                    // X axis
                    if let Some(gyro_x) = values.first()
                        && ((GYRO_X_MIN_THRESHOLD > *gyro_x) || (*gyro_x > GYRO_X_MAX_THRESHOLD))
                    {
                        emit_warning(&state, "Critical: Gyro X threshold exceeded!");
                    }

                    // Y axis
                    if let Some(gyro_y) = values.get(1)
                        && ((GYRO_Y_MIN_THRESHOLD > *gyro_y) || (*gyro_y > GYRO_Y_MAX_THRESHOLD))
                    {
                        emit_warning(&state, "Critical: Gyro Y threshold exceeded!");
                    }

                    // Z axis
                    if let Some(gyro_z) = values.get(2)
                        && ((GYRO_Z_MIN_THRESHOLD > *gyro_z) || (*gyro_z > GYRO_Z_MAX_THRESHOLD))
                    {
                        emit_warning(&state, "Critical: Gyro Z threshold exceeded!");
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
                        emit_warning(&state, "Critical: Barometer pressure threshold exceeded!");
                    }

                    // Temperature
                    if let Some(temp) = values.get(1)
                        && ((BARO_TEMPERATURE_MIN_THRESHOLD > *temp)
                            || (*temp > BARO_TEMPERATURE_MAX_THRESHOLD))
                    {
                        emit_warning(
                            &state,
                            "Critical: Barometer temperature threshold exceeded!",
                        );
                    }

                    // Altitude
                    if let Some(alt) = values.get(2)
                        && ((BARO_ALTITUDE_MIN_THRESHOLD > *alt)
                            || (*alt > BARO_ALTITUDE_MAX_THRESHOLD))
                    {
                        emit_warning(&state, "Critical: Barometer altitude threshold exceeded!");
                    }
                }

                // GPS: [lat, lon] in "xy"
                DataType::GpsData => {
                    let values = pkt.data_as_f32().unwrap_or_else(|_| vec![0f32; 3]);

                    // Latitude (x)
                    if let Some(lat) = values.first()
                        && ((GPS_LATITUDE_MIN_THRESHOLD > *lat)
                            || (*lat > GPS_LATITUDE_MAX_THRESHOLD))
                    {
                        emit_warning(
                            &state,
                            "Critical: GPS latitude out of bounds (Texas check)!",
                        );
                    }

                    // Longitude (y)
                    if let Some(lon) = values.get(1)
                        && ((GPS_LONGITUDE_MIN_THRESHOLD > *lon)
                            || (*lon > GPS_LONGITUDE_MAX_THRESHOLD))
                    {
                        emit_warning(
                            &state,
                            "Critical: GPS longitude out of bounds (Texas check)!",
                        );
                    }
                }

                DataType::BatteryCurrent => {
                    let values = pkt.data_as_f32().unwrap_or_else(|_| vec![0f32; 2]);

                    // Current
                    if let Some(current) = values.get(1)
                        && ((BATTERY_CURRENT_MIN_THRESHOLD > *current)
                            || (*current > BATTERY_CURRENT_MAX_THRESHOLD))
                    {
                        emit_warning(&state, "Critical: Battery current out of range!");
                    }
                }

                DataType::BatteryVoltage => {
                    let values = pkt.data_as_f32().unwrap_or_else(|_| vec![0f32; 2]);
                    // Voltage
                    if let Some(voltage) = values.first()
                        && ((BATTERY_VOLTAGE_MIN_THRESHOLD > *voltage)
                            || (*voltage > BATTERY_VOLTAGE_MAX_THRESHOLD))
                    {
                        emit_warning(&state, "Critical: Battery voltage out of range!");
                    }
                }

                // Fuel flow: [flow_L_per_hr, ...]
                DataType::FuelFlow => {
                    let values = pkt.data_as_f32().unwrap_or_else(|_| vec![0f32; 1]);

                    if let Some(flow) = values.first()
                        && ((FUEL_FLOW_MIN_THRESHOLD > *flow) || (*flow > FUEL_FLOW_MAX_THRESHOLD))
                    {
                        emit_warning(&state, "Critical: Fuel flow out of range!");
                    }
                }

                // Fuel tank pressure: [pressure_psi, ...]
                DataType::FuelTankPressure => {
                    let values = pkt.data_as_f32().unwrap_or_else(|_| vec![0f32; 1]);

                    if let Some(pressure) = values.first()
                        && ((FUEL_TANK_PRESSURE_MIN_THRESHOLD > *pressure)
                            || (*pressure > FUEL_TANK_PRESSURE_MAX_THRESHOLD))
                    {
                        emit_warning(&state, "Critical: Fuel tank pressure out of range!");
                    }
                }

                // Kalman filter XYZ state
                DataType::KalmanFilterData => {
                    // [x, y, z] in "xyz"
                    let values = pkt.data_as_f32().unwrap_or_else(|_| vec![0f32; 3]);

                    // X
                    if let Some(kx) = values.first()
                        && ((KALMAN_X_MIN_THRESHOLD > *kx) || (*kx > KALMAN_X_MAX_THRESHOLD))
                    {
                        emit_warning(&state, "Critical: Kalman X state out of range!");
                    }

                    // Y
                    if let Some(ky) = values.get(1)
                        && ((KALMAN_Y_MIN_THRESHOLD > *ky) || (*ky > KALMAN_Y_MAX_THRESHOLD))
                    {
                        emit_warning(&state, "Critical: Kalman Y state out of range!");
                    }

                    // Z
                    if let Some(kz) = values.get(2)
                        && ((KALMAN_Z_MIN_THRESHOLD > *kz) || (*kz > KALMAN_Z_MAX_THRESHOLD))
                    {
                        emit_warning(&state, "Critical: Kalman Z state out of range!");
                    }
                }

                DataType::GenericError => {
                    abort = true;
                    emit_warning(&state, "Generic Error received from vehicle!");
                    println!("Safety: Generic Error packet received");
                }

                _ => {}
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
            println!("Safety task: Abort command sent");
            break;
        }
    }
}
