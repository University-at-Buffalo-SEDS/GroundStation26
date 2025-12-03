use crate::telemetry_task::get_current_timestamp_ms;
use groundstation_shared::FlightState;
use rand::Rng;
use sedsprintf_rs_2026::config::DataEndpoint;
use sedsprintf_rs_2026::telemetry_packet::TelemetryPacket;
use sedsprintf_rs_2026::TelemetryResult;
use std::sync::{Arc, Mutex, OnceLock};

// ---------------------------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------------------------

/// How often we want to advance to the next flight state (ms).
const FLIGHTSTATE_INTERVAL_MS: i64 = 5_000;

/// The cycle of dummy flight states we want to walk through.
const FLIGHT_STATES: &[FlightState] = &[
    FlightState::Startup,
    FlightState::Idle,
    FlightState::Armed,
    FlightState::Ascent,
    FlightState::Coast,
    FlightState::Descent,
    FlightState::Landed,
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
}

static DUMMY_STATE: OnceLock<Mutex<DummyState>> = OnceLock::new();

fn dummy_state() -> &'static Mutex<DummyState> {
    DUMMY_STATE.get_or_init(|| {
        Mutex::new(DummyState {
            // Start "now"; first flight-state packet will happen
            // after 5 seconds have passed.
            last_flightstate_ms: get_current_timestamp_ms() as i64,
            idx: 0,
        })
    })
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

    let now_ms = get_current_timestamp_ms();

    // Decide whether we should emit a flight-state packet on this call.
    let mut state_guard = dummy_state().lock().expect("dummy_state mutex poisoned");

    let mut emit_flightstate = false;
    if now_ms as i64 - state_guard.last_flightstate_ms >= FLIGHTSTATE_INTERVAL_MS {
        // Advance to next state (wrapping) and mark that we should
        // emit a flight-state packet instead of random telemetry.
        state_guard.last_flightstate_ms = now_ms as i64;
        state_guard.idx = (state_guard.idx + 1) % FLIGHT_STATES.len();
        emit_flightstate = true;
    }

    if emit_flightstate {
        // Grab the state we advanced to.
        let flight_state = &FLIGHT_STATES[state_guard.idx];
        drop(state_guard); // release mutex early

        // Encode FlightState into payload; here as a single u8.
        // If you have a helper like flight_state_to_u8, use that instead.
        let state_code = *flight_state as u8;

        return TelemetryPacket::new(
            FlightState, // <- make sure this matches your DataType variant name
            &[DataEndpoint::GroundStation],
            "TEST",
            now_ms,
            Arc::from([state_code]),
        );
    }

    // Not time for a flight-state packet → generate a random telemetry packet.
    drop(state_guard);

    let mut rng = rand::rng();

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
            // (lat, lon)
            let margin = 0.001;

            // Random offset within ±margin
            let lat = BASE_LAT + rng.random_range(-margin..margin);
            let lon = BASE_LON + rng.random_range(-margin..margin);
            vec![lat, lon]
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
        "TEST", // device ID
        now_ms,
        Arc::from(bytes.as_slice()),
    )
}
