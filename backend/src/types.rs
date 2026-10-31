use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[repr(u8)]
pub enum TelemetryCommand {
    Postinit,
    Launch,
    Dump,
    Abort,
    NormallyOpen,
    Pilot,
    Igniter,
    #[cfg(feature = "hitl_mode")]
    IgniterSequence,
    RetractPlumbing,
    Nitrogen,
    NitrogenClose,
    Nitrous,
    NitrousClose,
    StartWritingNow,
    StartWritingLastTwoMinutes,
    PauseWritingDb,
    StopWritingDb,
    ResetSim,
    ContinueFillSequence,
    MonitorAltitude,
    RevokeMonitorAltitude,
    ConsecutiveSamples,
    RevokeConsecutiveSamples,
    ResetFailures,
    RevokeResetFailures,
    ValidateMeasms,
    RevokeValidateMeasms,
    #[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
    GroundStationLaunch,
    #[cfg(feature = "hitl_mode")]
    ToggleButtonInterlock,
    #[cfg(feature = "hitl_mode")]
    ToggleGroundStationControl,
    #[cfg(feature = "hitl_mode")]
    ToggleLaunchInterlock,
    #[cfg(feature = "hitl_mode")]
    TogglePhysicalLaunchMode,
    ToggleAvBayUnderglow,
    ToggleFlightBuzzer,
    #[cfg(feature = "hitl_mode")]
    ResetLaunchLatch,
    #[cfg(feature = "hitl_mode")]
    ResetFlightState,
    ResetTClock,
    #[cfg(feature = "hitl_mode")]
    DeployParachute,
    #[cfg(feature = "hitl_mode")]
    ExpandParachute,
    #[cfg(feature = "hitl_mode")]
    EvaluationRelax,
    #[cfg(feature = "hitl_mode")]
    EvaluationFocus,
    #[cfg(feature = "hitl_mode")]
    EvaluationAbort,
    #[cfg(feature = "hitl_mode")]
    ReinitSensors,
    #[cfg(feature = "hitl_mode")]
    ReinitBarometer,
    #[cfg(feature = "hitl_mode")]
    EnableIMU,
    #[cfg(feature = "hitl_mode")]
    DisableIMU,
    #[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
    AdvanceFlightState,
    #[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
    RewindFlightState,
    #[cfg(feature = "hitl_mode")]
    AbortAfter100,
    #[cfg(feature = "hitl_mode")]
    AbortAfter40,
    #[cfg(feature = "hitl_mode")]
    AbortAfter250,
    #[cfg(feature = "hitl_mode")]
    ReinitAfter15,
    #[cfg(feature = "hitl_mode")]
    ReinitAfter30,
    #[cfg(feature = "hitl_mode")]
    ReinitAfter50,
    StartFill,
    PauseFill,
    CancelFill,
    ValveSelfTest,
    NitrogenTest,
    /// Internal producer intent; never accepted as a client command.
    #[serde(skip)]
    AutomaticAbort,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[repr(u8)]
pub enum FlightState {
    Startup = 0,
    Idle = 1,
    PreFill = 2,
    FillTest = 3,
    NitrogenFill = 4,
    NitrousFill = 5,
    Armed = 6,
    Launch = 7,
    Ascent = 8,
    Coast = 9,
    Apogee = 10,
    #[serde(alias = "ParachuteDeploy")]
    Descent = 11,
    Reefing = 12,
    Landed = 13,
    Recovery = 14,
    Aborted = 15,
}

#[allow(clippy::enum_variant_names)]
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
            Board::GatewayBoard => "GB",
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

pub fn canonical_sender_id(sender: &str) -> &str {
    match sender {
        "GW" => Board::GatewayBoard.sender_id(),
        "DAQB" => Board::DaqBoard.sender_id(),
        _ => sender,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardStatusEntry {
    pub board: Board,
    #[serde(default)]
    pub board_label: String,
    pub sender_id: String,
    pub seen: bool,
    #[serde(default)]
    pub packet_count: u64,
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
    /// Last evidence from this sender, not a cached topology refresh.
    #[serde(default)]
    pub last_seen_ms: Option<u64>,
    /// Monotonic age at snapshot time; independent of wall-clock corrections.
    #[serde(default)]
    pub age_ms: Option<u64>,
    #[serde(default)]
    pub endpoints: Vec<String>,
    #[serde(default = "default_true")]
    pub show_in_details: bool,
    pub detail: Option<String>,
    #[serde(default)]
    pub stats: Option<NetworkTopologyStats>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq, Default)]
pub struct NetworkTopologyStats {
    #[serde(default)]
    pub packets_sent: u64,
    #[serde(default)]
    pub packets_received: u64,
    #[serde(default)]
    pub bytes_sent: u64,
    #[serde(default)]
    pub bytes_received: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct NetworkTopologyLink {
    pub source: String,
    pub target: String,
    pub label: Option<String>,
    pub status: NetworkTopologyStatus,
}

/// Application-level DAQ SD journal acknowledgement, independently aged.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Default, Hash)]
pub struct DaqLaunchLogStatus {
    pub session: u64,
    pub deadline_utc_ms: u64,
    pub revision: u64,
    pub flags: u32,
    pub received_age_ms: u64,
    pub matches_expected: Option<bool>,
    pub expected_session: Option<u64>,
    pub expected_deadline_utc_ms: Option<u64>,
    pub expected_revision: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Default, Hash)]
pub struct BoardDiagnostics {
    /// Internal MCU die temperature, not ambient. Absent on older firmware.
    #[serde(default)]
    pub die_temperature_centi_c: Option<i32>,
    #[serde(default)]
    pub die_temperature_sample_age_ms: u32,
    pub version: u32,
    pub received_age_ms: u64,
    pub uptime_ticks: u64,
    pub uptime_frequency_hz: u32,
    pub reset_flags: u32,
    pub watchdog_missing_mask: u32,
    pub heap_live_bytes: Option<u32>,
    pub heap_peak_bytes: Option<u32>,
    pub heap_region_bytes: Option<u32>,
    pub allocation_failures: Option<u32>,
    pub admission_refusals: u32,
    pub can_rx_drops: Option<u32>,
    pub can_tx_rejected: Option<u32>,
    pub uart_rx_drops: Option<u32>,
    pub tx_queue_high_water: Option<u32>,
    pub sd_accepted_records: Option<u32>,
    pub sd_processed_records: Option<u32>,
    pub sd_rejected_records: Option<u32>,
    pub sd_flushed_records: Option<u32>,
    pub sd_write_errors: Option<u32>,
    pub sd_max_write_ms: Option<u32>,
    pub adc_overruns: Option<u32>,
    pub diagnostic_send_failures: u32,
    /// DAQ-only SD companion report. Missing on older firmware/other boards.
    #[serde(default)]
    pub sd_status_received_age_ms: Option<u64>,
    #[serde(default)]
    pub sd_flags: Option<u32>,
    #[serde(default)]
    pub sd_last_write_age_ms: Option<u32>,
    #[serde(default)]
    pub sd_last_flush_age_ms: Option<u32>,
    #[serde(default)]
    pub sd_init_stage: Option<u32>,
    #[serde(default)]
    pub sd_retry_count: Option<u32>,
    #[serde(default)]
    pub sd_retry_requested_token: Option<u32>,
    #[serde(default)]
    pub sd_retry_completed_token: Option<u32>,
    #[serde(default)]
    pub sd_init_failures: Option<u32>,
    #[serde(default)]
    pub launch_log: Option<DaqLaunchLogStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Default, Hash)]
pub struct NetworkBoardHealthEntry {
    /// Host CPU/package sensor; unsupported platforms report unavailable.
    #[serde(default)]
    pub cpu_temperature_centi_c: Option<i32>,
    #[serde(default)]
    pub cpu_temperature_age_ms: u64,
    #[serde(default)]
    pub diagnostics: Option<BoardDiagnostics>,
    pub sender_id: String,
    pub healthy: bool,
    pub age_ms: u64,
    pub last_seen_ms: u64,
    pub disconnects: u64,
    pub reconnects: u64,
    pub rx_observations: u64,
    pub observed_ms: u64,
    pub offline_ms: u64,
    pub longest_outage_ms: u64,
    pub current_outage_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Default)]
pub struct NetworkTopologyMsg {
    #[serde(default)]
    pub health: Vec<NetworkBoardHealthEntry>,
    pub generated_ms: u64,
    #[serde(default)]
    pub simulated: bool,
    #[serde(default)]
    pub nodes: Vec<NetworkTopologyNode>,
    #[serde(default)]
    pub links: Vec<NetworkTopologyLink>,
    #[serde(default)]
    pub traffic: Option<NetworkTrafficSnapshot>,
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
        11 => Some(FlightState::Descent),
        12 => Some(FlightState::Reefing),
        13 => Some(FlightState::Landed),
        14 => Some(FlightState::Recovery),
        15 => Some(FlightState::Aborted),
        _ => None,
    }
}

/// Application phase names corresponding to the FC's global-state wire bytes.
pub(crate) const FLIGHT_PHASE_VERSION: u8 = 1;

pub(crate) fn canonical_phase_key(phase: &str, version: u8) -> &str {
    if phase.eq_ignore_ascii_case("ParachuteDeploy") {
        "Descent"
    } else if version == 0 && phase.eq_ignore_ascii_case("Descent") {
        "Reefing"
    } else {
        phase
    }
}

/// Preserve the wire meaning of phase-indexed settings written by older GS
/// builds. Validate the complete replacement before changing the caller's map.
pub(crate) fn normalize_phase_map<T: Clone>(
    phases: &mut std::collections::BTreeMap<String, T>,
    version: u8,
) -> Result<(), &'static str> {
    if version > FLIGHT_PHASE_VERSION {
        return Err("Unsupported flight phase version");
    }
    let mut next = std::collections::BTreeMap::new();
    for (phase, value) in phases.iter() {
        if next
            .insert(canonical_phase_key(phase, version).to_owned(), value.clone())
            .is_some()
        {
            return Err("Conflicting legacy and current flight phase names");
        }
    }
    *phases = next;
    Ok(())
}

#[cfg(test)]
mod flight_state_wire_tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn every_wire_state_matches_the_captured_fc_producer_contract() {
        let contract: serde_json::Value = serde_json::from_str(include_str!(
            "test-fixtures/fc-global-state-contract.json"
        ))
        .unwrap();
        for (wire, expected) in contract["global_states"].as_array().unwrap().iter().enumerate() {
            let state = u8_to_flight_state(wire as u8).unwrap();
            assert_eq!(state as usize, wire);
            assert_eq!(serde_json::to_value(state).unwrap(), *expected);
        }
        assert_eq!(u8_to_flight_state(16), None);
        assert_eq!(u8_to_flight_state(255), None);
        let legacy: FlightState = serde_json::from_str("\"ParachuteDeploy\"").unwrap();
        assert_eq!(legacy, FlightState::Descent);
        assert_eq!(legacy as u8, 11);
    }

    #[test]
    fn legacy_phase_settings_preserve_numeric_meaning_without_repeated_migration() {
        let mut phases = BTreeMap::from([
            ("ParachuteDeploy".into(), 11), ("Descent".into(), 12),
        ]);
        normalize_phase_map(&mut phases, 0).unwrap();
        assert_eq!(phases, BTreeMap::from([("Descent".into(), 11), ("Reefing".into(), 12)]));
        let migrated = phases.clone();
        normalize_phase_map(&mut phases, FLIGHT_PHASE_VERSION).unwrap();
        assert_eq!(phases, migrated);
        let mut conflict = BTreeMap::from([("Descent".into(), 1), ("Reefing".into(), 2)]);
        let original = conflict.clone();
        assert!(normalize_phase_map(&mut conflict, 0).is_err());
        assert_eq!(conflict, original);
        assert!(normalize_phase_map(&mut conflict, 2).is_err());
        assert_eq!(conflict, original);
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

/// Ground Station router counters, sampled before WebSocket batching.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Default)]
pub struct NetworkTrafficSnapshot {
    pub interval_ms: u64,
    pub sides: Vec<NetworkTrafficSide>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Default)]
pub struct NetworkTrafficSide {
    pub side_id: usize,
    pub name: String,
    pub ingress_enabled: bool,
    pub egress_enabled: bool,
    pub totals: NetworkTopologyStats,
    /// Absent until two samples exist, or if the side/counters reset.
    pub delta: Option<NetworkTopologyStats>,
}
