use crate::flight_sim::next_state_aware_packet;
use crate::telemetry_task::get_current_timestamp_ms;
use groundstation_shared::Board;
use rand::RngExt;
use sedsprintf_rs_2026::config::{DataEndpoint, DataType};
use sedsprintf_rs_2026::telemetry_packet::TelemetryPacket;
use sedsprintf_rs_2026::TelemetryResult;
use std::sync::Arc;

// Switch between legacy random telemetry and realistic state-aware flight sim.
// `true`  => command-driven fill states + manual launch + realistic 10k ft profile.
// `false` => random standalone telemetry values.
const USE_STATE_AWARE_SIM: bool = true;

const BASE_LAT: f32 = 31.7619;
const BASE_LON: f32 = -106.4850;

fn random_sender() -> &'static str {
    let mut rng = rand::rng();
    let idx = rng.random_range(0..Board::ALL.len());
    Board::ALL[idx].sender_id()
}

fn random_packet() -> TelemetryResult<TelemetryPacket> {
    let now_ms = get_current_timestamp_ms();
    let sender = random_sender();
    let mut rng = rand::rng();

    let choices = [
        DataType::GpsData,
        DataType::KalmanFilterData,
        DataType::GyroData,
        DataType::AccelData,
        DataType::BatteryVoltage,
        DataType::BatteryCurrent,
        DataType::BarometerData,
        DataType::FuelFlow,
        DataType::FuelTankPressure,
    ];

    let dtype = choices[rng.random_range(0..choices.len())];

    let values: Vec<f32> = match dtype {
        DataType::GpsData => {
            let margin = 0.001;
            let lat = BASE_LAT + rng.random_range(-margin..margin);
            let lon = BASE_LON + rng.random_range(-margin..margin);
            let alt_m = rng.random_range(0.0..200.0);
            vec![lat, lon, alt_m]
        }
        DataType::KalmanFilterData => {
            let x = rng.random_range(-20.0..20.0);
            let y = rng.random_range(-20.0..20.0);
            let z = rng.random_range(-20.0..20.0);
            vec![x, y, z]
        }
        DataType::GyroData => {
            let gx = rng.random_range(-5.0..5.0);
            let gy = rng.random_range(-5.0..5.0);
            let gz = rng.random_range(-180.0..180.0);
            vec![gx, gy, gz]
        }
        DataType::AccelData => {
            let ax = rng.random_range(-2.0..2.0);
            let ay = rng.random_range(-2.0..2.0);
            let az = rng.random_range(8.0..11.0);
            vec![ax, ay, az]
        }
        DataType::BatteryVoltage => vec![rng.random_range(11.0..12.6)],
        DataType::BatteryCurrent => vec![rng.random_range(0.0..18.0)],
        DataType::BarometerData => {
            let pressure_pa = rng.random_range(98_000.0..102_000.0);
            let temp_c = rng.random_range(10.0..35.0);
            let altitude_m = rng.random_range(0.0..200.0);
            vec![pressure_pa, temp_c, altitude_m]
        }
        DataType::FuelFlow => vec![rng.random_range(0.0..20.0)],
        DataType::FuelTankPressure => vec![rng.random_range(0.0..120.0)],
        _ => vec![0.0],
    };

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

pub fn get_dummy_packet() -> TelemetryResult<TelemetryPacket> {
    if USE_STATE_AWARE_SIM {
        next_state_aware_packet()
    } else {
        random_packet()
    }
}
