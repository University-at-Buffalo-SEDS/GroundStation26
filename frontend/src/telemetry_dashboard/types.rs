use serde::{Deserialize, Serialize};

pub type FlightState = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardStatusEntry {
    pub board: String,
    #[serde(default)]
    pub board_label: String,
    pub sender_id: String,
    pub seen: bool,
    pub last_seen_ms: Option<u64>,
    pub age_ms: Option<u64>,
}

impl BoardStatusEntry {
    pub fn display_name(&self) -> &str {
        if self.board_label.trim().is_empty() {
            &self.board
        } else {
            &self.board_label
        }
    }

    pub fn from_sender_id(sender_id: &str) -> Option<Self> {
        let (board, label) = match sender_id {
            "GS" => ("GroundStation", "Ground Station"),
            "FC" => ("FlightComputer", "Flight Computer"),
            "RF" => ("RFBoard", "RF Board"),
            "PB" => ("PowerBoard", "Power Board"),
            "VB" => ("ValveBoard", "Valve Board"),
            "GW" => ("GatewayBoard", "Gateway Board"),
            "AB" => ("ActuatorBoard", "Actuator Board"),
            "DAQ" => ("DaqBoard", "DAQ Board"),
            _ => return None,
        };

        Some(Self {
            board: board.to_string(),
            board_label: label.to_string(),
            sender_id: sender_id.to_string(),
            seen: false,
            last_seen_ms: None,
            age_ms: None,
        })
    }
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelemetryRow {
    pub timestamp_ms: i64,
    pub data_type: String,
    #[serde(default)]
    pub sender_id: String,
    pub values: Vec<Option<f32>>,
}
