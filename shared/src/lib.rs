use serde::{Deserialize, Serialize};

/// Example packet type after decoding with sedsprintf_rs_2026.
/// Adjust fields to match the real schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[repr(u8)]
pub enum TelemetryCommand {
    Launch,
    Dump,
    Abort,
    NormallyOpen,
    Pilot,
    Igniter,
    RetractPlumbing,
    Nitrogen,
    Nitrous,
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
    GroundStation,
    FlightComputer,
    RFBoard,
    PowerBoard,
    ValveBoard,
    GatewayBoard,
    ActuatorBoard,
    DaqBoard,
}

impl Board {
    pub const ALL: &'static [Board] = &[Board::GroundStation, Board::FlightComputer, Board::RFBoard,
        Board::PowerBoard, Board::ValveBoard, Board::GatewayBoard, Board::ActuatorBoard, Board::DaqBoard
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Board::GroundStation => "Ground Station",
            Board::FlightComputer => "Flight Computer",
            Board::RFBoard => "RF Board",
            Board::PowerBoard => "Power Board",
            Board::ValveBoard => "Valve Board",
            Board::GatewayBoard => "Gateway Board",
            Board::ActuatorBoard => "Actuator Board",
            Board::DaqBoard => "DAQ Board",
        }
    }

    pub fn sender_id(&self) -> &'static str {
        match self {
            Board::GroundStation => "GS",
            Board::FlightComputer => "FC",
            Board::RFBoard => "RF",
            Board::PowerBoard => "PB",
            Board::ValveBoard => "VB",
            Board::GatewayBoard => "GW",
            Board::ActuatorBoard => "AB",
            Board::DaqBoard => "DAQ",
        }
    }

    pub fn from_sender_id(sender: &str) -> Option<Board> {
        Self::ALL
            .iter()
            .copied()
            .find(|board| board.sender_id() == sender)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardStatusEntry {
    pub board: Board,
    pub sender_id: String,
    pub seen: bool,
    pub last_seen_ms: Option<u64>,
    pub age_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardStatusMsg {
    pub boards: Vec<BoardStatusEntry>,
}

impl FlightState {
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
    pub values: Vec<Option<f32>>,
}
