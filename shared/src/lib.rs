use serde::{Deserialize, Serialize};

/// Example packet type after decoding with sedsprintf_rs_2026.
/// Adjust fields to match the real schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[repr(u8)]
pub enum TelemetryCommand {
    Arm,
    Disarm,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelemetryRow {
    pub timestamp_ms: i64,
    pub data_type: String, // "GYRO_DATA", "ACCEL_DATA", etc.
    pub v0: Option<f32>,   // meaning depends on data_type
    pub v1: Option<f32>,
    pub v2: Option<f32>,
}
