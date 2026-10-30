use crate::state::AppState;
use crate::telemetry_task::{get_current_timestamp_ms, queue_automatic_abort_packet};
use crate::types::{Board, FlightState, canonical_sender_id};
use crate::web::{emit_warning, emit_warning_db_only};
use sedsnet::router::Router;
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
const BATTERY_LOW_VOLTAGE_ALWAYS_WARN_THRESHOLD: f32 = 13.0; // V

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
const BOARD_TIMEOUT_MS: u64 = 12000;
#[cfg(not(feature = "testing"))]
const BOARD_OFFLINE_ABORT_TRIGGER_MS: u64 = 6000;
const SAFETY_WARNING_COOLDOWN_MS_DEFAULT: u64 = 5_000;

// This applies only to board-liveness silence. Shared automatic-abort policy
// still includes Idle for independent faults such as failed ADC zero capture.
fn liveness_abort_allowed_in_state(stage: FlightState) -> bool {
    !matches!(stage, FlightState::Startup | FlightState::Idle)
}

// An explicitly configured MCU simulation clock must compare like domains.
// Physical deployments (no model clock) keep the original wall-age policy.
// Unknown or future model observations remain unknown, never fresh-by-clamping.
fn safety_liveness_age_ms(
    wall_now_ms: u64,
    last_wall_ms: Option<u64>,
    model_now_ms: Option<u64>,
    last_model_ms: Option<u64>,
) -> Option<u64> {
    match model_now_ms {
        Some(now) => last_model_ms.and_then(|last| now.checked_sub(last)),
        None => last_wall_ms.map(|last| wall_now_ms.saturating_sub(last)),
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
enum SafetyAbortReason {
    BoardOffline {
        board: &'static str,
        selected_state: FlightState,
        selected_wall_ms: u64,
        last_observation_wall_ms: u64,
        observation_wall_age_ms: u64,
        decision_clock_basis: &'static str,
        decision_age_ms: u64,
        selected_model_ms: Option<u64>,
        last_observation_model_ms: Option<u64>,
        observation_instant_age_ms: Option<u64>,
        offline_detection_threshold_ms: u64,
        abort_trigger_threshold_ms: u64,
    },
    GenericError {
        sender: String,
        packet_id: u64,
        selected_state: FlightState,
        selected_wall_ms: u64,
    },
}

fn record_first_abort_reason(first: &mut Option<SafetyAbortReason>, reason: SafetyAbortReason) {
    if first.is_some() {
        return;
    }
    // Record the actual first selected cause before claim or transport work.
    // last_observation_wall_ms is ANY board packet evidence, not a heartbeat clock.
    match serde_json::to_string(&reason) {
        Ok(value) => log::error!("automatic_abort_first_cause={value}"),
        Err(error) => {
            log::error!("automatic_abort_first_cause={reason:?} serialization_error={error}")
        }
    }
    *first = Some(reason);
}

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

fn is_fill_system_battery_sender(sender_id: &str) -> bool {
    matches!(
        canonical_sender_id(sender_id),
        sender if sender == Board::GatewayBoard.sender_id()
    )
}

fn is_av_bay_board(board: Board) -> bool {
    matches!(
        board,
        Board::FlightComputer | Board::RFBoard | Board::PowerBoard
    )
}

fn battery_low_voltage_always_warns(sender_id: &str, voltage: f32) -> bool {
    is_fill_system_battery_sender(sender_id) && voltage <= BATTERY_LOW_VOLTAGE_ALWAYS_WARN_THRESHOLD
}

fn battery_voltage_out_of_range(sender_id: &str, voltage: f32) -> bool {
    if !voltage.is_finite() {
        return false;
    }

    let sender_id = canonical_sender_id(sender_id);
    let outside_configured_range = battery_voltage_bounds_by_sender()
        .get(sender_id)
        .copied()
        .is_some_and(|(min_v, max_v)| voltage < min_v || voltage > max_v);

    outside_configured_range || battery_low_voltage_always_warns(sender_id, voltage)
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

#[cfg(feature = "testing")]
const BOARD_TIMEOUT_MS: u64 = 12000;
#[cfg(feature = "testing")]
const BOARD_OFFLINE_ABORT_TRIGGER_MS: u64 = 6000;

pub async fn safety_task(
    state: Arc<AppState>,
    router: Arc<Router>,
    mut shutdown_rx: broadcast::Receiver<()>,
) {
    let mut pending_abort = None;
    let mut first_abort_reason = None;
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
        let model_now_ms = crate::simulation_clock_ms();
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

                let liveness_age_ms = safety_liveness_age_ms(
                    now_ms,
                    status.last_seen_ms,
                    model_now_ms,
                    status.last_seen_model_ms,
                );
                let offline = liveness_age_ms.is_none_or(|age| age > BOARD_TIMEOUT_MS);
                let is_ground_board = matches!(
                    board,
                    Board::ValveBoard
                        | Board::ActuatorBoard
                        | Board::DaqBoard
                        | Board::GatewayBoard
                );
                let in_flight_ignored_board = !on_ground && is_ground_board;

                if offline {
                    if is_av_bay_board(*board) {
                        status.warned = false;
                        continue;
                    }
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
                    if liveness_abort_allowed_in_state(current_state)
                        && abort_eligible
                        && !in_flight_ignored_board
                        && let Some(last_seen_ms) = status.last_seen_ms
                        && let Some(offline_ms) = liveness_age_ms
                    {
                        if offline_ms >= BOARD_OFFLINE_ABORT_TRIGGER_MS && pending_abort.is_none() {
                            let reason = SafetyAbortReason::BoardOffline {
                                board: board.sender_id(),
                                selected_state: current_state,
                                selected_wall_ms: now_ms,
                                last_observation_wall_ms: last_seen_ms,
                                observation_wall_age_ms: now_ms.saturating_sub(last_seen_ms),
                                decision_clock_basis: if model_now_ms.is_some() {
                                    "model"
                                } else {
                                    "wall"
                                },
                                decision_age_ms: offline_ms,
                                selected_model_ms: model_now_ms,
                                last_observation_model_ms: status.last_seen_model_ms,
                                observation_instant_age_ms: status.last_seen_instant.map(|seen| {
                                    seen.elapsed().as_millis().min(u64::MAX as u128) as u64
                                }),
                                offline_detection_threshold_ms: BOARD_TIMEOUT_MS,
                                abort_trigger_threshold_ms: BOARD_OFFLINE_ABORT_TRIGGER_MS,
                            };
                            record_first_abort_reason(&mut first_abort_reason, reason.clone());
                            pending_abort = Some(reason);
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
            if *state.state.lock().unwrap() == FlightState::Startup {
                state.set_local_flight_state(FlightState::Idle);
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
            rb.recent(len)
                .into_iter()
                .map(|received| match received {
                    crate::ring_buffer::ReceivedPacket::Telemetry(packet)
                    | crate::ring_buffer::ReceivedPacket::AppliedStatus { packet, .. } => {
                        packet.clone()
                    }
                })
                .collect::<Vec<_>>()
        };

        if packets.is_empty() {
            continue;
        }

        let mut cycle_warnings: HashSet<&'static str> = HashSet::new();
        for pkt in packets {
            match pkt.data_type() {
                ty if ty == crate::telemetry_schema::data_type("ACCEL_DATA") => {
                    let values = pkt.data_as_f32().unwrap_or_else(|_| vec![0f32; 3]);
                    check_accel_thresholds(&values, &mut cycle_warnings);
                }

                ty if ty == crate::telemetry_schema::data_type("GYRO_DATA") => {
                    let values = pkt.data_as_f32().unwrap_or_else(|_| vec![0f32; 3]);
                    check_gyro_thresholds(&values, &mut cycle_warnings);
                }

                ty if ty == crate::telemetry_schema::data_type("IMU_DATA") => {
                    let values = pkt.data_as_f32().unwrap_or_else(|_| vec![0f32; 6]);
                    if values.len() >= 6 {
                        check_accel_thresholds(&values[..3], &mut cycle_warnings);
                        check_gyro_thresholds(&values[3..6], &mut cycle_warnings);
                    }
                }

                ty if ty == crate::telemetry_schema::data_type("BAROMETER_DATA") => {
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
                ty if ty == crate::telemetry_schema::data_type("GPS_DATA") => {
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

                ty if ty == crate::telemetry_schema::data_type("BATTERY_CURRENT") => {
                    let values = pkt.data_as_f32().unwrap_or_else(|_| vec![0f32; 2]);

                    // Current
                    if let Some(current) = values.get(1)
                        && ((BATTERY_CURRENT_MIN_THRESHOLD > *current)
                            || (*current > BATTERY_CURRENT_MAX_THRESHOLD))
                    {
                        cycle_warnings.insert("Critical: Battery current out of range!");
                    }
                }

                ty if ty == crate::telemetry_schema::data_type("BATTERY_VOLTAGE") => {
                    let values = pkt.data_as_f32().unwrap_or_else(|_| vec![0f32; 2]);
                    // Voltage
                    if let Some(voltage) = values.first() {
                        if is_fill_system_battery_sender(pkt.sender()) {
                            continue;
                        } else if battery_voltage_out_of_range(pkt.sender(), *voltage) {
                            cycle_warnings.insert("Critical: Battery voltage out of range!");
                        }
                    }
                }

                // Fuel tank pressure: [pressure_psi, ...]
                ty if ty == crate::telemetry_schema::data_type("FUEL_TANK_PRESSURE") => {
                    let values = pkt.data_as_f32().unwrap_or_else(|_| vec![0f32; 1]);

                    if let Some(pressure) = values.first()
                        && ((FUEL_TANK_PRESSURE_MIN_THRESHOLD > *pressure)
                            || (*pressure > FUEL_TANK_PRESSURE_MAX_THRESHOLD))
                    {
                        cycle_warnings.insert("Critical: Fuel tank pressure out of range!");
                    }
                }

                // Ascent/descent Kalman filter state packets.
                ty if ty == crate::telemetry_schema::data_type("ASCENT_STATE")
                    || ty == crate::telemetry_schema::data_type("DESCENT_STATE") =>
                {
                    let values = pkt.data_as_f32().unwrap_or_default();

                    if values.iter().any(|value| {
                        !value.is_finite()
                            || *value < KALMAN_STATE_MIN_THRESHOLD
                            || *value > KALMAN_STATE_MAX_THRESHOLD
                    }) {
                        cycle_warnings.insert("Critical: Kalman filter state out of range!");
                    }
                }

                ty if ty == crate::telemetry_schema::data_type("GENERIC_ERROR") => {
                    if !matches!(pending_abort, Some(SafetyAbortReason::GenericError { .. })) {
                        let reason = SafetyAbortReason::GenericError {
                            sender: pkt.sender().to_owned(),
                            packet_id: pkt.packet_id(),
                            selected_state: current_state,
                            selected_wall_ms: now_ms,
                        };
                        record_first_abort_reason(&mut first_abort_reason, reason.clone());
                        // A real fault supersedes a cancellable liveness intent.
                        pending_abort = Some(reason);
                    }
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

        if let Some(reason) = pending_abort.as_ref() {
            match dispatch_safety_abort_reason(&state, &router, reason) {
                SafetyDispatch::Claimed => break,
                SafetyDispatch::IdleLivenessCancelled(stage) => {
                    log::warn!(
                        "automatic_abort_intent_cancelled source=board_offline state={stage:?}"
                    );
                    pending_abort = None;
                    first_abort_reason = None;
                }
                SafetyDispatch::Unavailable => {}
            }
        }
    }
}

/// Claims launch exclusion before dispatch, without holding the GSE mutex over
/// router callbacks. Admission failure keeps the persisted recovery latch.
#[cfg(test)]
pub(crate) fn dispatch_safety_abort(state: &Arc<AppState>, router: &Router) -> bool {
    let Some(claim) = crate::gse::claim_automatic_abort(state) else {
        return false;
    };
    dispatch_claimed_safety_abort(state, router, claim);
    true
}

enum SafetyDispatch {
    Claimed,
    IdleLivenessCancelled(FlightState),
    Unavailable,
}

fn dispatch_safety_abort_reason(
    state: &Arc<AppState>,
    router: &Router,
    reason: &SafetyAbortReason,
) -> SafetyDispatch {
    let claim = if matches!(reason, SafetyAbortReason::BoardOffline { .. }) {
        match crate::gse::claim_liveness_abort(state) {
            crate::gse::LivenessAbortDecision::Claimed(claim) => claim,
            crate::gse::LivenessAbortDecision::IdleOrStartup(stage) => {
                return SafetyDispatch::IdleLivenessCancelled(stage);
            }
            crate::gse::LivenessAbortDecision::Unavailable => return SafetyDispatch::Unavailable,
        }
    } else {
        let Some(claim) = crate::gse::claim_automatic_abort(state) else {
            return SafetyDispatch::Unavailable;
        };
        claim
    };
    // This is a claimed cause, distinct from an earlier selected/cancelled one.
    match serde_json::to_string(reason) {
        Ok(value) => log::error!("automatic_abort_claimed_cause={value}"),
        Err(error) => {
            log::error!("automatic_abort_claimed_cause={reason:?} serialization_error={error}")
        }
    }
    dispatch_claimed_safety_abort(state, router, claim);
    SafetyDispatch::Claimed
}

fn dispatch_claimed_safety_abort(
    state: &Arc<AppState>,
    router: &Router,
    _claim: crate::gse::AutomaticAbortClaim,
) {
    state.set_abort_indicator_latched(true);
    crate::sequences::refresh_action_policy_now(state);
    state.broadcast_action_policy_snapshot();
    match queue_automatic_abort_packet(router, "Safety Task Abort Command Issued") {
        Ok(()) => {
            if let Err(error) = router.process_all_queues_with_timeout(3) {
                log::error!("failed to flush automatic abort: {error:?}");
            }
            gs_debug_println!("Safety task: Abort command accepted for delivery");
        }
        Err(error) => {
            log::error!("failed to queue automatic abort; GSE abort remains latched: {error:?}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn actual_safety_task_preserves_faults_and_cancels_only_idle_liveness() {
        const CHILD: &str = "GS_ACTUAL_LIVENESS_POLICY_CHILD";
        const NAME: &str = "safety_task::tests::actual_safety_task_preserves_faults_and_cancels_only_idle_liveness";
        if std::env::var_os(CHILD).is_none() {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
                .env(CHILD, "1")
                .env("GS_SIM_MODE", "0")
                .env("GS_RRLS_ROLE", "primary")
                .env_remove("GS_SIM_CLOCK_FILE")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        assert!(!crate::flight_sim::sim_mode_enabled());
        assert_eq!(crate::simulation_clock_ms(), None);
        for (stage, generic, transition_idle, expect_abort) in [
            (FlightState::Idle, false, false, false),
            (FlightState::PreFill, false, false, true),
            (FlightState::Idle, true, false, true),
            (FlightState::PreFill, false, true, false),
            (FlightState::PreFill, true, true, true),
        ] {
            actual_safety_scene(stage, generic, transition_idle, expect_abort).await;
        }
    }

    async fn actual_safety_scene(
        stage: FlightState,
        generic: bool,
        transition_idle: bool,
        expect_abort: bool,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let state = crate::state::tests::test_app_state().await;
        let weak = Arc::downgrade(&state);
        let seen = Arc::new(AtomicUsize::new(0));
        let observed = seen.clone();
        let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([
            sedsnet::router::EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint("ABORT"),
                move |packet| {
                    assert!(
                        weak.upgrade().unwrap().gse.try_lock().is_ok(),
                        "I/O callback holds no GSE mutex"
                    );
                    if packet.data_type() == crate::telemetry_schema::data_type("ABORT") {
                        observed.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(())
                },
            ),
        ])));
        let path = crate::gse::armed_fixture_for_test(&state, &router);
        state.set_local_flight_state(stage);
        let now = get_current_timestamp_ms();
        {
            let mut boards = state.board_status.lock().unwrap();
            for board in Board::ALL {
                let status = boards.get_mut(board).unwrap();
                status.last_seen_ms = Some(now);
                status.last_seen_model_ms = None;
                status.last_seen_instant = Some(std::time::Instant::now());
            }
            let ab = boards.get_mut(&Board::ActuatorBoard).unwrap();
            ab.last_seen_ms = Some(now - 13_000);
            ab.last_seen_instant = Some(std::time::Instant::now() - Duration::from_secs(13));
        }
        assert!(state.board_required_for_progression(Board::ActuatorBoard));
        let packet = |kind: &str| {
            sedsnet::packet::Packet::new(
                crate::telemetry_schema::data_type(kind),
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                "PB",
                now,
                Arc::<[u8]>::from(vec![0u8]),
            )
            .unwrap()
        };
        state
            .ring_buffer
            .lock()
            .unwrap()
            .push(packet("FLIGHT_STATE"));
        if generic && !transition_idle {
            state
                .ring_buffer
                .lock()
                .unwrap()
                .push(packet("GENERIC_ERROR"));
        }
        let mut statuses = state.board_status_tx.subscribe();
        let blocker = if transition_idle {
            let blocked_state = state.clone();
            let generic_packet = packet("GENERIC_ERROR");
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let worker = tokio::task::spawn_blocking(move || {
                let mut ring = blocked_state.ring_buffer.lock().unwrap();
                let _ = ready_tx.send(());
                if release_rx
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap_or(false)
                {
                    ring.push(generic_packet);
                }
            });
            ready_rx.await.unwrap();
            Some((release_tx, worker))
        } else {
            None
        };
        let (stop, receiver) = broadcast::channel(1);
        let task = tokio::spawn(safety_task(state.clone(), router, receiver));
        tokio::time::timeout(Duration::from_secs(3), statuses.recv())
            .await
            .unwrap()
            .unwrap();
        if let Some((release, worker)) = blocker {
            // The real task has selected the stale-board intent and broadcast
            // status, but cannot read its ring/claim yet. No runtime hook/bypass.
            state.set_local_flight_state(FlightState::Idle);
            release.send(generic).unwrap();
            worker.await.unwrap();
        }
        if expect_abort {
            tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap();
            assert!(
                seen.load(Ordering::SeqCst) > 0,
                "actual Router receives ABORT"
            );
            assert!(state.abort_indicator_latched());
            assert!(
                crate::gse::claim_automatic_abort(&state).is_none(),
                "claim remains latched after dispatch guard drops"
            );
        } else {
            // A second real iteration proves the first dispatch decision ran.
            tokio::time::timeout(Duration::from_secs(3), statuses.recv())
                .await
                .unwrap()
                .unwrap();
            let _ = stop.send(());
            tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(seen.load(Ordering::SeqCst), 0);
            assert!(!state.abort_indicator_latched());
        }
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            saved["automatic_abort_claimed"], expect_abort,
            "persisted latch matches actual dispatch outcome"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn liveness_idle_exemption_is_only_a_caller_policy() {
        for wire in 0..16 {
            let stage = crate::types::u8_to_flight_state(wire).unwrap();
            assert_eq!(liveness_abort_allowed_in_state(stage), wire > 1);
        }
        // Preserve both existing thresholds; actual selection still requires
        // the outer age >12s gate before the >=6s abort-trigger check.
        assert_eq!(BOARD_TIMEOUT_MS, 12_000);
        assert_eq!(BOARD_OFFLINE_ABORT_TRIGGER_MS, 6_000);
    }

    #[test]
    fn first_abort_reason_retains_its_actual_source_and_observation_age() {
        let first = SafetyAbortReason::BoardOffline {
            board: "AB",
            selected_state: FlightState::PreFill,
            selected_wall_ms: 13_001,
            last_observation_wall_ms: 1_000,
            observation_wall_age_ms: 12_001,
            decision_clock_basis: "wall",
            decision_age_ms: 12_001,
            selected_model_ms: Some(1_300),
            last_observation_model_ms: Some(1_000),
            observation_instant_age_ms: Some(12_001),
            offline_detection_threshold_ms: 12_000,
            abort_trigger_threshold_ms: 6_000,
        };
        let mut selected = None;
        record_first_abort_reason(&mut selected, first.clone());
        record_first_abort_reason(
            &mut selected,
            SafetyAbortReason::GenericError {
                sender: "PB".into(),
                packet_id: 7,
                selected_state: FlightState::Idle,
                selected_wall_ms: 13_002,
            },
        );
        assert_eq!(selected.as_ref(), Some(&first));
        let value = serde_json::to_value(selected.unwrap()).unwrap();
        assert_eq!(value["source"], "board_offline");
        assert_eq!(value["board"], "AB");
        assert_eq!(value["last_observation_wall_ms"], 1_000);
        assert_eq!(value["observation_wall_age_ms"], 12_001);
        assert!(value.get("last_heartbeat_ms").is_none());
    }

    #[test]
    fn slowed_model_cadence_does_not_expire_on_host_wall_time() {
        let age = safety_liveness_age_ms(13_012, Some(1_000), Some(7_364), Some(6_000));
        assert_eq!(age, Some(1_364));
        assert!(!age.is_none_or(|age| age > BOARD_TIMEOUT_MS));
    }

    #[test]
    fn model_timeout_is_stale_even_when_wall_time_is_fresh() {
        let age = safety_liveness_age_ms(1_001, Some(1_000), Some(18_001), Some(6_000));
        assert_eq!(age, Some(12_001));
        assert!(age.is_none_or(|age| age > BOARD_TIMEOUT_MS));
        assert!(age.unwrap() >= BOARD_OFFLINE_ABORT_TRIGGER_MS);
    }

    #[test]
    fn unknown_or_future_model_observations_never_become_fake_fresh() {
        for last in [None, Some(1)] {
            let age = safety_liveness_age_ms(1_000, Some(1_000), Some(0), last);
            assert_eq!(age, None);
            assert!(age.is_none_or(|age| age > BOARD_TIMEOUT_MS));
        }
    }

    #[test]
    fn model_zero_and_exact_timeout_boundary_are_valid() {
        assert_eq!(
            safety_liveness_age_ms(99_000, Some(0), Some(0), Some(0)),
            Some(0)
        );
        let age = safety_liveness_age_ms(99_000, Some(0), Some(12_000), Some(0));
        assert_eq!(age, Some(12_000));
        assert!(!age.is_none_or(|age| age > BOARD_TIMEOUT_MS));
    }

    #[test]
    fn absent_model_clock_preserves_exact_wall_fallback() {
        for (now, last) in [
            (0u64, None),
            (0, Some(0)),
            (1, Some(2)),
            (13_001, Some(1_000)),
        ] {
            let expected = last.map(|last| now.saturating_sub(last));
            assert_eq!(
                safety_liveness_age_ms(now, last, None, Some(u64::MAX)),
                expected
            );
        }
    }

    #[test]
    fn low_voltage_floor_always_warns_for_fill_system_battery() {
        assert!(battery_low_voltage_always_warns("GB", 13.0));
        assert!(battery_low_voltage_always_warns("GW", 13.0));
        assert!(battery_low_voltage_always_warns("GB", 12.99));
    }

    #[test]
    fn low_voltage_floor_does_not_apply_to_non_fill_system_batteries() {
        assert!(!battery_low_voltage_always_warns("PB", 7.4));
        assert!(!battery_low_voltage_always_warns("PB", 13.0));
        assert!(!battery_low_voltage_always_warns("VB", 13.0));
    }

    #[test]
    fn battery_voltage_range_check_includes_non_configurable_fill_system_floor() {
        assert!(battery_voltage_out_of_range("GB", 13.0));
        assert!(battery_voltage_out_of_range("GW", 13.0));
    }
}
