use crate::flight_sim::{_next_state_aware_packet, sim_mode_enabled};
use crate::telemetry_task::get_current_timestamp_ms;
use crate::types::Board;
use rand::RngExt;
use sedsnet::TelemetryResult;
use sedsnet::packet::Packet;
use std::sync::Arc;

const BASE_LAT: f32 = 31.7619;
const BASE_LON: f32 = -106.485;

fn random_sender() -> &'static str {
    let mut rng = rand::rng();
    let idx = rng.random_range(0..Board::ALL.len());
    Board::ALL[idx].sender_id()
}

fn random_packet() -> TelemetryResult<Packet> {
    let now_ms = get_current_timestamp_ms();
    let mut sender = random_sender();
    let mut rng = rand::rng();

    let choices = [
        crate::telemetry_schema::data_type("GPS_DATA"),
        crate::telemetry_schema::data_type("ASCENT_STATE"),
        crate::telemetry_schema::data_type("DESCENT_STATE"),
        crate::telemetry_schema::data_type("IMU_DATA"),
        crate::telemetry_schema::data_type("BATTERY_VOLTAGE"),
        crate::telemetry_schema::data_type("BATTERY_CURRENT"),
        crate::telemetry_schema::data_type("BAROMETER_DATA"),
        crate::telemetry_schema::data_type("FUEL_FLOW"),
        crate::telemetry_schema::data_type("FUEL_TANK_PRESSURE"),
    ];

    let dtype = choices[rng.random_range(0..choices.len())];

    let values: Vec<f32> = match dtype {
        ty if ty == crate::telemetry_schema::data_type("GPS_DATA") => {
            let margin = 0.001;
            let lat = BASE_LAT + rng.random_range(-margin..margin);
            let lon = BASE_LON + rng.random_range(-margin..margin);
            let alt_m = rng.random_range(0.0..200.0);
            vec![lat, lon, alt_m]
        }
        ty if ty == crate::telemetry_schema::data_type("ASCENT_STATE") => {
            let q0 = rng.random_range(0.95..1.0);
            let q1 = rng.random_range(-0.05..0.05);
            let q2 = rng.random_range(-0.05..0.05);
            let q3 = rng.random_range(-0.05..0.05);
            let altitude_m = rng.random_range(0.0..200.0);
            let velocity_mps = rng.random_range(-20.0..120.0);
            vec![q0, q1, q2, q3, altitude_m, velocity_mps]
        }
        ty if ty == crate::telemetry_schema::data_type("DESCENT_STATE") => {
            let margin = 0.001;
            let lat = BASE_LAT + rng.random_range(-margin..margin);
            let lon = BASE_LON + rng.random_range(-margin..margin);
            let altitude_m = rng.random_range(0.0..200.0);
            let velocity_mps = rng.random_range(-80.0..10.0);
            vec![lat, lon, altitude_m, velocity_mps]
        }
        ty if ty == crate::telemetry_schema::data_type("IMU_DATA") => {
            let ax = rng.random_range(-2.0..2.0);
            let ay = rng.random_range(-2.0..2.0);
            let az = rng.random_range(8.0..11.0);
            let gx = rng.random_range(-5.0..5.0);
            let gy = rng.random_range(-5.0..5.0);
            let gz = rng.random_range(-180.0..180.0);
            vec![ax, ay, az, gx, gy, gz]
        }
        ty if ty == crate::telemetry_schema::data_type("BATTERY_VOLTAGE") => {
            let sources = [
                (Board::PowerBoard.sender_id(), 6.3, 8.4),
                (Board::ValveBoard.sender_id(), 6.3, 8.4),
                (Board::GatewayBoard.sender_id(), 13.3, 15.5),
            ];
            let (source, low, high) = sources[rng.random_range(0..sources.len())];
            sender = source;
            vec![rng.random_range(low..high)]
        }
        ty if ty == crate::telemetry_schema::data_type("BATTERY_CURRENT") => {
            let sources = [
                (Board::PowerBoard.sender_id(), 0.5, 8.0),
                (Board::ValveBoard.sender_id(), 0.1, 2.0),
                (Board::GatewayBoard.sender_id(), 0.2, 2.5),
            ];
            let (source, low, high) = sources[rng.random_range(0..sources.len())];
            sender = source;
            vec![rng.random_range(low..high)]
        }
        ty if ty == crate::telemetry_schema::data_type("BAROMETER_DATA") => {
            let pressure_pa = rng.random_range(98_000.0..102_000.0);
            let temp_c = rng.random_range(10.0..35.0);
            let altitude_m = rng.random_range(0.0..200.0);
            vec![pressure_pa, temp_c, altitude_m]
        }
        ty if ty == crate::telemetry_schema::data_type("FUEL_FLOW") => {
            vec![rng.random_range(0.0..20.0)]
        }
        ty if ty == crate::telemetry_schema::data_type("FUEL_TANK_PRESSURE") => {
            vec![rng.random_range(0.0..120.0)]
        }
        _ => vec![0.0],
    };

    let mut bytes = Vec::with_capacity(values.len() * 4);
    for v in values {
        bytes.extend_from_slice(&v.to_le_bytes());
    }

    Packet::new(
        dtype,
        &[crate::telemetry_schema::endpoint("GROUND_STATION")],
        sender,
        now_ms,
        Arc::from(bytes.as_slice()),
    )
}

pub fn get_dummy_packet() -> TelemetryResult<Option<Packet>> {
    if sim_mode_enabled() {
        _next_state_aware_packet()
    } else {
        random_packet().map(Some)
    }
}
