use crate::rocket_commands::{ActuatorBoardCommands, ValveBoardCommands};
use crate::telemetry_task::get_current_timestamp_ms;
use groundstation_shared::{Board, FlightState};
use rand::Rng;
use rand::RngExt;
use sedsprintf_rs_2026::TelemetryResult;
use sedsprintf_rs_2026::config::DataEndpoint;
use sedsprintf_rs_2026::telemetry_packet::TelemetryPacket;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
// ---------------------------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------------------------

/// How often we want to advance to the next flight state (ms).
const FLIGHT_STATE_INTERVAL_MS: i64 = 7_000;
const UMBILICAL_MIN_INTERVAL_MS: i64 = 800;
const UMBILICAL_MAX_INTERVAL_MS: i64 = 2_500;

/// The cycle of dummy flight states we want to walk through.
const FLIGHT_STATES: &[FlightState] = &[
    // FlightState::Startup,
    FlightState::Idle,
    FlightState::PreFill,
    FlightState::NitrogenFill,
    FlightState::FillTest,
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
    // Add/remove as needed
];

// ---------------------------------------------------------------------------------------------
// Internal state for dummy generator
// ---------------------------------------------------------------------------------------------

struct DummyState {
    /// Timestamp of the last time we emitted a flight-state packet.
    last_flightstate_ms: i64,
    /// Index into FLIGHT_STATES.
    idx: usize,
    /// Timestamp of last umbilical status packet.
    last_umbilical_ms: i64,
    /// Next interval (ms) to emit umbilical status.
    next_umbilical_ms: i64,
    /// Umbilical valve states keyed by command id.
    umbilical_states: HashMap<u8, bool>,
}

static DUMMY_STATE: OnceLock<Mutex<DummyState>> = OnceLock::new();
static SENDER_INDEX: OnceLock<Mutex<usize>> = OnceLock::new();

fn dummy_state() -> &'static Mutex<DummyState> {
    DUMMY_STATE.get_or_init(|| {
        Mutex::new(DummyState {
            // Start "now"; first flight-state packet will happen
            // after 5 seconds have passed.
            last_flightstate_ms: get_current_timestamp_ms() as i64,
            idx: 0,
            last_umbilical_ms: get_current_timestamp_ms() as i64,
            next_umbilical_ms: UMBILICAL_MIN_INTERVAL_MS,
            umbilical_states: HashMap::new(),
        })
    })
}

fn next_dummy_sender() -> &'static str {
    if Board::ALL.is_empty() {
        return "TEST";
    }

    let mut idx = SENDER_INDEX.get_or_init(|| Mutex::new(0)).lock().unwrap();
    let sender = Board::ALL[*idx % Board::ALL.len()].sender_id();
    *idx = (*idx + 1) % Board::ALL.len();
    sender
}

fn next_umbilical_status(state: &mut DummyState, rng: &mut impl Rng) -> (u8, bool, &'static str) {
    let valve_cmds = [
        ValveBoardCommands::PilotOpen as u8,
        ValveBoardCommands::NormallyOpenOpen as u8,
        ValveBoardCommands::DumpOpen as u8,
        ValveBoardCommands::PilotClose as u8,
        ValveBoardCommands::NormallyOpenClose as u8,
        ValveBoardCommands::DumpClose as u8,
    ];
    let actuator_cmds = [
        ActuatorBoardCommands::IgniterOn as u8,
        ActuatorBoardCommands::NitrogenOpen as u8,
        ActuatorBoardCommands::NitrousOpen as u8,
        ActuatorBoardCommands::RetractPlumbing as u8,
        ActuatorBoardCommands::IgniterOff as u8,
        ActuatorBoardCommands::NitrogenClose as u8,
        ActuatorBoardCommands::NitrousClose as u8,
    ];

    // Pick a command id, avoiding retracted (one-way) if already on.
    let mut cmd_id = actuator_cmds[rng.random_range(0..actuator_cmds.len())];
    for _ in 0..6 {
        let pick_actuator = rng.random_range(0..2) == 0;
        cmd_id = if pick_actuator {
            actuator_cmds[rng.random_range(0..actuator_cmds.len())]
        } else {
            valve_cmds[rng.random_range(0..valve_cmds.len())]
        };

        if cmd_id == ActuatorBoardCommands::RetractPlumbing as u8 {
            if state
                .umbilical_states
                .get(&cmd_id)
                .copied()
                .unwrap_or(false)
            {
                continue;
            }
        }
        break;
    }

    let current = state
        .umbilical_states
        .get(&cmd_id)
        .copied()
        .unwrap_or(false);
    let next = if cmd_id == ActuatorBoardCommands::RetractPlumbing as u8 {
        true
    } else {
        !current
    };
    state.umbilical_states.insert(cmd_id, next);

    let sender = if valve_cmds.contains(&cmd_id) {
        Board::ValveBoard.sender_id()
    } else {
        Board::ActuatorBoard.sender_id()
    };

    (cmd_id, next, sender)
}

// ---------------------------------------------------------------------------------------------
// Public API: get_dummy_packet
// ---------------------------------------------------------------------------------------------

/// Generate a dummy packet:
///
/// - Normally: a random packet from `choices` (GPS / gyro / etc.).
/// - Once every 5 seconds: on the first call after the interval has elapsed,
///   return a *flight-state* packet with the next FlightState (wrapping).
pub fn get_dummy_packet() -> TelemetryResult<TelemetryPacket> {
    use crate::DataType::*;

    let sender = next_dummy_sender();

    let now_ms = get_current_timestamp_ms();

    // Decide whether we should emit a flight-state packet on this call.
    {
        let mut state_guard = dummy_state().lock().expect("dummy_state mutex poisoned");

        let mut emit_flightstate = false;
        if now_ms as i64 - state_guard.last_flightstate_ms >= FLIGHT_STATE_INTERVAL_MS {
            // Advance to next state (wrapping) and mark that we should
            // emit a flight-state packet instead of random telemetry.
            state_guard.last_flightstate_ms = now_ms as i64;
            state_guard.idx = (state_guard.idx + 1) % FLIGHT_STATES.len();
            emit_flightstate = true;
        }

        if emit_flightstate {
            // Grab the state we advanced to.
            let flight_state = &FLIGHT_STATES[state_guard.idx];

            // Encode FlightState into payload; here as a single u8.
            // If you have a helper like flight_state_to_u8, use that instead.
            let state_code = *flight_state as u8;

            return TelemetryPacket::new(
                FlightState, // <- make sure this matches the DataType variant name
                &[DataEndpoint::GroundStation],
                sender,
                now_ms,
                Arc::from([state_code]),
            );
        }
    }

    // Maybe emit umbilical status
    let mut rng = rand::rng();
    {
        let mut state_guard = dummy_state().lock().expect("dummy_state mutex poisoned");
        let since = now_ms as i64 - state_guard.last_umbilical_ms;
        if since >= state_guard.next_umbilical_ms {
            let (cmd_id, on, sender_id) = next_umbilical_status(&mut state_guard, &mut rng);
            state_guard.last_umbilical_ms = now_ms as i64;
            state_guard.next_umbilical_ms =
                rng.random_range(UMBILICAL_MIN_INTERVAL_MS..=UMBILICAL_MAX_INTERVAL_MS);
            drop(state_guard);

            return TelemetryPacket::new(
                UmbilicalStatus,
                &[DataEndpoint::GroundStation],
                sender_id,
                now_ms,
                Arc::from([cmd_id, if on { 1 } else { 0 }]),
            );
        }
    }

    // Not time for a flight-state packet → generate a random telemetry packet.

    // Choose one of the data-carrying types (NOT TelemetryError / GenericError / MessageData)
    let choices = [
        GpsData,
        KalmanFilterData,
        GyroData,
        AccelData,
        BatteryVoltage,
        BatteryCurrent,
        BarometerData,
        FuelFlow,
        FuelTankPressure,
    ];

    let dtype = choices[rng.random_range(0..choices.len())];
    const BASE_LAT: f32 = 31.7619;
    const BASE_LON: f32 = -106.485;
    let values: Vec<f32> = match dtype {
        GpsData => {
            // (lat, lon, alt)
            let margin = 0.001;

            // Random offset within ±margin
            let lat = BASE_LAT + rng.random_range(-margin..margin);
            let lon = BASE_LON + rng.random_range(-margin..margin);
            let alt = rng.random_range(0.0..3000.0);
            vec![lat, lon, alt]
        }
        KalmanFilterData => {
            // Example: filtered accel XYZ
            let ax = rng.random_range(-20.0..20.0);
            let ay = rng.random_range(-20.0..20.0);
            let az = rng.random_range(-20.0..20.0);
            vec![ax, ay, az]
        }
        GyroData => {
            // Gyro [°/s]
            let gx = rng.random_range(-5.0..5.0);
            let gy = rng.random_range(-5.0..5.0);
            let gz = rng.random_range(-360.0..360.0);
            vec![gx, gy, gz]
        }
        AccelData => {
            // Accel [m/s^2]
            let ax = rng.random_range(-2.0..2.0);
            let ay = rng.random_range(-2.0..2.0);
            let az = rng.random_range(-30.0..2.0);
            vec![ax, ay, az]
        }
        BatteryVoltage => {
            // Voltage 7V–12.6V
            let v = rng.random_range(7.0..12.6);
            vec![v]
        }
        BatteryCurrent => {
            // Current 0–40A
            let a = rng.random_range(0.0..40.0);
            vec![a]
        }
        BarometerData => {
            // Pressure (hPa), Altitude (m), Temperature (C)
            let pressure = rng.random_range(30000.0..110000.0);
            let altitude = rng.random_range(-3.4..11000.0);
            let temp = rng.random_range(5.0..40.0);
            vec![pressure, temp, altitude]
        }
        FuelFlow => {
            // Fuel flow rate (L/h)
            let flow = rng.random_range(0.0..100.0);
            vec![flow]
        }
        FuelTankPressure => {
            // Fuel tank pressure (psi)
            let pressure = rng.random_range(0.0..30.0);
            vec![pressure]
        }
        _ => {
            // Should never happen given the choices[] list
            vec![]
        }
    };

    // Convert Vec<f32> → &[u8] using little-endian encoding
    let mut bytes = Vec::with_capacity(values.len() * 4);
    for v in values {
        bytes.extend_from_slice(&v.to_le_bytes());
    }

    TelemetryPacket::new(
        dtype,
        &[DataEndpoint::GroundStation],
        sender,
        now_ms,
        Arc::from(bytes.as_slice()),
    )
}
