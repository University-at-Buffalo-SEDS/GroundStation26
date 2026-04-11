use serde::{Deserialize, Serialize};

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
    NitrogenClose,
    Nitrous,
    NitrousClose,
    ContinueFillSequence,
    #[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
    AdvanceFlightState,
    #[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
    RewindFlightState,
    #[cfg(feature = "hitl_mode")]
    DeployParachute,
    #[cfg(feature = "hitl_mode")]
    ExpandParachute,
    #[cfg(feature = "hitl_mode")]
    ReinitSensors,
    #[cfg(feature = "hitl_mode")]
    LaunchSignal,
    #[cfg(feature = "hitl_mode")]
    EvaluationRelax,
    #[cfg(feature = "hitl_mode")]
    EvaluationFocus,
    #[cfg(feature = "hitl_mode")]
    EvaluationAbort,
    #[cfg(feature = "hitl_mode")]
    ReinitBarometer,
    #[cfg(feature = "hitl_mode")]
    EnableIMU,
    #[cfg(feature = "hitl_mode")]
    DisableIMU,
    #[cfg(feature = "hitl_mode")]
    MonitorAltitude,
    #[cfg(feature = "hitl_mode")]
    RevokeMonitorAltitude,
    #[cfg(feature = "hitl_mode")]
    ConsecutiveSamples,
    #[cfg(feature = "hitl_mode")]
    RevokeConsecutiveSamples,
    #[cfg(feature = "hitl_mode")]
    ResetFailures,
    #[cfg(feature = "hitl_mode")]
    RevokeResetFailures,
    #[cfg(feature = "hitl_mode")]
    ValidateMeasms,
    #[cfg(feature = "hitl_mode")]
    RevokeValidateMeasms,
    #[cfg(feature = "hitl_mode")]
    AbortAfter15,
    #[cfg(feature = "hitl_mode")]
    AbortAfter40,
    #[cfg(feature = "hitl_mode")]
    AbortAfter70,
    #[cfg(feature = "hitl_mode")]
    ReinitAfter12,
    #[cfg(feature = "hitl_mode")]
    ReinitAfter26,
    #[cfg(feature = "hitl_mode")]
    ReinitAfter44,
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
    pub const ALL: &'static [Board] = &[
        Board::GroundStation,
        Board::FlightComputer,
        Board::RFBoard,
        Board::PowerBoard,
        Board::ValveBoard,
        Board::GatewayBoard,
        Board::ActuatorBoard,
        Board::DaqBoard,
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
        match sender {
            "GS" => Some(Board::GroundStation),
            "FC" => Some(Board::FlightComputer),
            "RF" => Some(Board::RFBoard),
            "PB" => Some(Board::PowerBoard),
            "VB" => Some(Board::ValveBoard),
            "GW" | "GB" => Some(Board::GatewayBoard),
            "AB" => Some(Board::ActuatorBoard),
            "DAQ" | "DAQB" => Some(Board::DaqBoard),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardStatusEntry {
    pub board: Board,
    #[serde(default)]
    pub board_label: String,
    pub sender_id: String,
    pub seen: bool,
    pub last_seen_ms: Option<u64>,
    pub age_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardStatusMsg {
    pub boards: Vec<BoardStatusEntry>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum NetworkTopologyNodeKind {
    Router,
    Endpoint,
    Side,
    Board,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum NetworkTopologyStatus {
    Online,
    Offline,
    Simulated,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct NetworkTopologyNode {
    pub id: String,
    pub label: String,
    pub kind: NetworkTopologyNodeKind,
    pub status: NetworkTopologyStatus,
    pub group: String,
    pub sender_id: Option<String>,
    #[serde(default)]
    pub endpoints: Vec<String>,
    #[serde(default = "default_true")]
    pub show_in_details: bool,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct NetworkTopologyLink {
    pub source: String,
    pub target: String,
    pub label: Option<String>,
    pub status: NetworkTopologyStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Default)]
pub struct NetworkTopologyMsg {
    pub generated_ms: u64,
    #[serde(default)]
    pub simulated: bool,
    #[serde(default)]
    pub nodes: Vec<NetworkTopologyNode>,
    #[serde(default)]
    pub links: Vec<NetworkTopologyLink>,
}

fn default_true() -> bool {
    true
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
    pub data_type: String,
    #[serde(default)]
    pub sender_id: String,
    pub values: Vec<Option<f32>>,
}
