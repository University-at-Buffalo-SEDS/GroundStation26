use rand::Rng;
use sedsprintf_rs_2026::config::{DataEndpoint, DataType};
use sedsprintf_rs_2026::telemetry_packet::TelemetryPacket;
use sedsprintf_rs_2026::TelemetryResult;
use std::sync::Arc;
use crate::telemetry_task::get_current_timestamp_ms;

pub fn get_dummy_packet() -> TelemetryResult<TelemetryPacket> {
    use DataType::*;
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
    ];
    let dtype = choices[rng.random_range(0..choices.len())];

    let values: Vec<f32> = match dtype {
        GpsData => {
            // (lat, lon)
            let lat = rng.random_range(-90.0..90.0);
            let lon = rng.random_range(-180.0..180.0);
            vec![lat, lon]
        }
        KalmanFilterData => {
            // Example: filtered accel XYZ
            let ax = rng.random_range(-2.0..2.0);
            let ay = rng.random_range(-2.0..2.0);
            let az = rng.random_range(-2.0..2.0);
            vec![ax, ay, az]
        }
        GyroData => {
            // Gyro [°/s]
            let gx = rng.random_range(-300.0..300.0);
            let gy = rng.random_range(-300.0..300.0);
            let gz = rng.random_range(-300.0..300.0);
            vec![gx, gy, gz]
        }
        AccelData => {
            // Accel [m/s^2]
            let ax = rng.random_range(-10.0..10.0);
            let ay = rng.random_range(-10.0..10.0);
            let az = rng.random_range(-10.0..10.0);
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
            let pressure = rng.random_range(950.0..1050.0);
            let altitude = rng.random_range(0.0..500.0);
            let temp = rng.random_range(-10.0..40.0);
            vec![pressure, altitude, temp]
        }
        _ => {
            // Should never happen
            vec![]
        }
    };

    // Convert Vec<f32> → &[u8] using safe cast
    let mut bytes = Vec::with_capacity(values.len() * 4);
    for v in values {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    TelemetryPacket::new(
        dtype,
        &[DataEndpoint::GroundStation],
        "TEST",  // device ID
        get_current_timestamp_ms(),
        Arc::from(bytes.as_slice()),
    )
}
