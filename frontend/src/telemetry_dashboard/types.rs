use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "PascalCase")]
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

impl FlightState {
    pub fn as_str(&self) -> &'static str {
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

impl fmt::Display for FlightState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelemetryRow {
    pub timestamp_ms: i64,
    pub data_type: String,
    pub v0: Option<f32>,
    pub v1: Option<f32>,
    pub v2: Option<f32>,
    pub v3: Option<f32>,
    pub v4: Option<f32>,
    pub v5: Option<f32>,
    pub v6: Option<f32>,
    pub v7: Option<f32>,
}
