use serde::{Deserialize, Serialize};

/// Example packet type after decoding with sedsprintf_rs_2026.
/// Adjust fields to match the real schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[repr(u8)]
pub enum TelemetryCommand {
    Arm,
    Disarm,
    Abort,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelemetryRow {
    pub timestamp_ms: i64,
    pub data_type: String, // "GYRO_DATA", "ACCEL_DATA", etc.
    pub v0: Option<f32>,   // meaning depends on data_type
    pub v1: Option<f32>,
    pub v2: Option<f32>,
    pub v3: Option<f32>,
    pub v4: Option<f32>,
    pub v5: Option<f32>,
    pub v6: Option<f32>,
    pub v7: Option<f32>,
    
}
