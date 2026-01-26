use serde::{Deserialize, Serialize};

/// Example packet type after decoding with sedsprintf_rs_2026.
/// Adjust fields to match the real schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[repr(u8)]
pub enum TelemetryCommand {
    Arm,
    Disarm,
    Abort,
    Tanks,
    Pilot,
    Igniter,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[repr(u8)]
pub enum FlightState {
    Startup,
    Idle,
    PreFill,
    FillTest,
    NitrogenFill,
    NitrousFill,
    Armed,
    Launch,
    Ascent,
    Coast,
    Apogee,
    ParachuteDeploy,
    Descent,
    Landed,
    Recovery,
    Aborted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum Board {
    Rocket,
    Umbilical,
}

impl Board {
    pub const ALL: &'static [Board] = &[Board::Rocket, Board::Umbilical];

    pub fn as_str(&self) -> &'static str {
        match self {
            Board::Rocket => "Rocket",
            Board::Umbilical => "Umbilical",
        }
    }

    pub fn sender_id(&self) -> &'static str {
        match self {
            Board::Rocket => "ROCKET",
            Board::Umbilical => "UMBILICAL",
        }
    }

    pub fn from_sender_id(sender: &str) -> Option<Board> {
        Self::ALL.iter().copied().find(|board| board.sender_id() == sender)
    }
}

impl FlightState{
    pub fn to_string(&self) -> &'static str {
        match self {
            FlightState::Startup => "Startup",
            FlightState::Idle => "Idle",
            FlightState::PreFill => "PreFill",
            FlightState::FillTest => "FillTest",
            FlightState::NitrogenFill => "NitrogenFill",
            FlightState::NitrousFill => "NitrousFill",
            FlightState::Armed => "Armed",
            FlightState::Launch => "Launch",
            FlightState::Ascent => "Ascent",
            FlightState::Coast => "Coast",
            FlightState::Apogee => "Apogee",
            FlightState::ParachuteDeploy => "ParachuteDeploy",
            FlightState::Descent => "Descent",
            FlightState::Landed => "Landed",
            FlightState::Recovery => "Recovery",
            FlightState::Aborted => "Aborted",
        }
    }
}
pub fn u8_to_flight_state(value: u8) -> Option<FlightState> {
    match value {
        0 => Some(FlightState::Startup),
        1 => Some(FlightState::Idle),
        2 => Some(FlightState::PreFill),
        3 => Some(FlightState::FillTest),
        4 => Some(FlightState::NitrogenFill),
        5 => Some(FlightState::NitrousFill),
        6 => Some(FlightState::Armed),
        7 => Some(FlightState::Launch),
        8 => Some(FlightState::Ascent),
        9 => Some(FlightState::Coast),
        10 => Some(FlightState::Apogee),
        11 => Some(FlightState::ParachuteDeploy),
        12 => Some(FlightState::Descent),
        13 => Some(FlightState::Landed),
        14 => Some(FlightState::Recovery),
        15 => Some(FlightState::Aborted),
        _ => None,
    }
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
