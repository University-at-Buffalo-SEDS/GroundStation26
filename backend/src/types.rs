use serde::{Deserialize, Serialize};

/// Every variant exists in every station mode. Client commands that only
/// exist in HITL (or HITL and test fire) are refused when deserialized in
/// another mode, with the same "unknown variant" error the per-mode builds
/// gave, so the wire contract per mode is unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(remote = "Self")]
#[repr(u8)]
pub enum TelemetryCommand {
    #[serde(skip)]
    OperatorPhase { intent: crate::operator_phase::Intent },
    Postinit,
    Launch,
    Dump,
    Abort,
    NormallyOpen,
    Pilot,
    Igniter,
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
    GroundStationLaunch,
    ToggleButtonInterlock,
    ToggleGroundStationControl,
    ToggleLaunchInterlock,
    TogglePlumbingDelayInterlock,
    ToggleChecklistInterlock,
    ToggleSupplyInterlock,
    ToggleBoardStateProofInterlock,
    TogglePhysicalLaunchMode,
    ToggleAvBayUnderglow,
    ToggleFlightBuzzer,
    ResetLaunchLatch,
    ResetFlightState,
    ResetTClock,
    DeployParachute,
    ExpandParachute,
    EvaluationRelax,
    EvaluationFocus,
    EvaluationAbort,
    ReinitSensors,
    ReinitBarometer,
    EnableIMU,
    DisableIMU,
    AdvanceFlightState,
    RewindFlightState,
    AbortAfter100,
    AbortAfter40,
    AbortAfter250,
    ReinitAfter15,
    ReinitAfter30,
    ReinitAfter50,
    StartFill,
    /// Retained pre-auth Fill admission; never accepted from a client.
    #[serde(skip)]
    FillWithRecording {
        origin: std::sync::Arc<crate::fill_recording::Origin>,
    },
    PauseFill,
    CancelFill,
    ValveSelfTest,
    NitrogenTest,
    Turbocharge,
    /// Integrated ground test only (docs/integrated-ground-test.md). Enter
    /// and LIVE PYRO are deserializable so the refusal names them, but only
    /// the ticketed `/api/igt/confirm` flow sends them.
    IgtEnter,
    IgtLivePyro,
    IgtDryRun,
    IgtExit,
    /// Original physical EDGE identity only; never deserialized from a client.
    #[serde(skip)]
    ColdFlowPanelRelease {
        key: crate::sequence_progress::RunKey,
        generation: u64,
        awaiting_at_edge: bool,
    },
    /// Internal producer intent; never accepted as a client command.
    #[serde(skip)]
    // Production SafetyTask uses direct claimed TX; retain this queued-intent
    // boundary for its real-worker regression without adding production delay.
    #[cfg_attr(not(test), allow(dead_code))]
    AutomaticAbort {
        generation: u64,
    },
    /// Authenticated server admission envelope; never accepted from a client.
    #[serde(skip)]
    QueuedOperator {
        recovery_origin: Option<crate::operator_queue::manual_origin::Origin>,
        generation: u64,
        launch_revision: Option<u64>,
        command: Box<TelemetryCommand>,
        plumbing_origin: Option<crate::actuator_plumbing::MotionOrigin>,
        /// An admitted Launch press: holds station mode changes until the
        /// worker is done with it or the queued command is discarded.
        launch_hold: Option<crate::launch_mode_gate::LaunchHold>,
    },
    #[serde(skip)]
    AutomaticNitrogenClose {
        generation: u64,
    },
    #[serde(skip)]
    AutomaticNitrousClose {
        generation: u64,
    },
}

impl Serialize for TelemetryCommand {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        TelemetryCommand::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for TelemetryCommand {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let command = TelemetryCommand::deserialize(deserializer)?;
        let mode = crate::runtime_mode::current();
        if command.available_in(mode) {
            Ok(command)
        } else {
            Err(serde::de::Error::unknown_variant(
                crate::sequences::command_name(&command),
                client_command_names(mode),
            ))
        }
    }
}

/// Client-deserializable variants in declaration order, as serde lists them
/// in an "unknown variant" error for the given mode.
const NORMAL_CLIENT_COMMANDS: &[&str] = &[
    "Postinit", "Launch", "Dump", "Abort", "NormallyOpen", "Pilot", "Igniter",
    "RetractPlumbing", "Nitrogen", "NitrogenClose", "Nitrous", "NitrousClose",
    "StartWritingNow", "StartWritingLastTwoMinutes", "PauseWritingDb", "StopWritingDb",
    "ResetSim", "ContinueFillSequence", "MonitorAltitude", "RevokeMonitorAltitude",
    "ConsecutiveSamples", "RevokeConsecutiveSamples", "ResetFailures", "RevokeResetFailures",
    "ValidateMeasms", "RevokeValidateMeasms", "ToggleAvBayUnderglow", "ToggleFlightBuzzer",
    "ResetTClock", "StartFill", "PauseFill", "CancelFill", "ValveSelfTest", "NitrogenTest",
    "Turbocharge",
];
const IGT_CLIENT_COMMANDS: &[&str] = &[
    "Postinit", "Launch", "Dump", "Abort", "NormallyOpen", "Pilot", "Igniter",
    "RetractPlumbing", "Nitrogen", "NitrogenClose", "Nitrous", "NitrousClose",
    "StartWritingNow", "StartWritingLastTwoMinutes", "PauseWritingDb", "StopWritingDb",
    "ResetSim", "ContinueFillSequence", "MonitorAltitude", "RevokeMonitorAltitude",
    "ConsecutiveSamples", "RevokeConsecutiveSamples", "ResetFailures", "RevokeResetFailures",
    "ValidateMeasms", "RevokeValidateMeasms", "ToggleAvBayUnderglow", "ToggleFlightBuzzer",
    "ResetTClock", "StartFill", "PauseFill", "CancelFill", "ValveSelfTest", "NitrogenTest",
    "Turbocharge", "IgtEnter", "IgtLivePyro", "IgtDryRun", "IgtExit",
];
const TEST_FIRE_CLIENT_COMMANDS: &[&str] = &[
    "Postinit", "Launch", "Dump", "Abort", "NormallyOpen", "Pilot", "Igniter",
    "RetractPlumbing", "Nitrogen", "NitrogenClose", "Nitrous", "NitrousClose",
    "StartWritingNow", "StartWritingLastTwoMinutes", "PauseWritingDb", "StopWritingDb",
    "ResetSim", "ContinueFillSequence", "MonitorAltitude", "RevokeMonitorAltitude",
    "ConsecutiveSamples", "RevokeConsecutiveSamples", "ResetFailures", "RevokeResetFailures",
    "ValidateMeasms", "RevokeValidateMeasms", "GroundStationLaunch", "ToggleAvBayUnderglow",
    "ToggleFlightBuzzer", "ResetTClock", "AdvanceFlightState", "RewindFlightState",
    "StartFill", "PauseFill", "CancelFill", "ValveSelfTest", "NitrogenTest", "Turbocharge",
];
const HITL_CLIENT_COMMANDS: &[&str] = &[
    "Postinit", "Launch", "Dump", "Abort", "NormallyOpen", "Pilot", "Igniter",
    "IgniterSequence", "RetractPlumbing", "Nitrogen", "NitrogenClose", "Nitrous",
    "NitrousClose", "StartWritingNow", "StartWritingLastTwoMinutes", "PauseWritingDb",
    "StopWritingDb", "ResetSim", "ContinueFillSequence", "MonitorAltitude",
    "RevokeMonitorAltitude", "ConsecutiveSamples", "RevokeConsecutiveSamples", "ResetFailures",
    "RevokeResetFailures", "ValidateMeasms", "RevokeValidateMeasms", "GroundStationLaunch",
    "ToggleButtonInterlock", "ToggleGroundStationControl", "ToggleLaunchInterlock",
    "TogglePlumbingDelayInterlock", "ToggleChecklistInterlock", "ToggleSupplyInterlock",
    "ToggleBoardStateProofInterlock", "TogglePhysicalLaunchMode", "ToggleAvBayUnderglow",
    "ToggleFlightBuzzer", "ResetLaunchLatch", "ResetFlightState", "ResetTClock",
    "DeployParachute", "ExpandParachute", "EvaluationRelax", "EvaluationFocus",
    "EvaluationAbort", "ReinitSensors", "ReinitBarometer", "EnableIMU", "DisableIMU",
    "AdvanceFlightState", "RewindFlightState", "AbortAfter100", "AbortAfter40",
    "AbortAfter250", "ReinitAfter15", "ReinitAfter30", "ReinitAfter50", "StartFill",
    "PauseFill", "CancelFill", "ValveSelfTest", "NitrogenTest", "Turbocharge",
];
pub(crate) fn client_command_names(mode: crate::station_mode::Mode) -> &'static [&'static str] {
    use crate::station_mode::Mode;
    match mode {
        Mode::Hitl => HITL_CLIENT_COMMANDS,
        Mode::TestFire => TEST_FIRE_CLIENT_COMMANDS,
        Mode::Normal | Mode::Testing => NORMAL_CLIENT_COMMANDS,
        Mode::IntegratedGroundTest => IGT_CLIENT_COMMANDS,
    }
}

impl TelemetryCommand {
    /// Whether the build for `mode` had this variant.
    pub(crate) fn available_in(&self, mode: crate::station_mode::Mode) -> bool {
        use crate::station_mode::Mode;
        match self {
            TelemetryCommand::GroundStationLaunch
            | TelemetryCommand::AdvanceFlightState
            | TelemetryCommand::RewindFlightState => matches!(mode, Mode::Hitl | Mode::TestFire),
            TelemetryCommand::IgtEnter
            | TelemetryCommand::IgtLivePyro
            | TelemetryCommand::IgtDryRun
            | TelemetryCommand::IgtExit => mode == Mode::IntegratedGroundTest,
            TelemetryCommand::OperatorPhase { .. }
            | TelemetryCommand::IgniterSequence
            | TelemetryCommand::ToggleButtonInterlock
            | TelemetryCommand::ToggleGroundStationControl
            | TelemetryCommand::ToggleLaunchInterlock
            | TelemetryCommand::TogglePlumbingDelayInterlock
            | TelemetryCommand::ToggleChecklistInterlock
            | TelemetryCommand::ToggleSupplyInterlock
            | TelemetryCommand::ToggleBoardStateProofInterlock
            | TelemetryCommand::TogglePhysicalLaunchMode
            | TelemetryCommand::ResetLaunchLatch
            | TelemetryCommand::ResetFlightState
            | TelemetryCommand::DeployParachute
            | TelemetryCommand::ExpandParachute
            | TelemetryCommand::EvaluationRelax
            | TelemetryCommand::EvaluationFocus
            | TelemetryCommand::EvaluationAbort
            | TelemetryCommand::ReinitSensors
            | TelemetryCommand::ReinitBarometer
            | TelemetryCommand::EnableIMU
            | TelemetryCommand::DisableIMU
            | TelemetryCommand::AbortAfter100
            | TelemetryCommand::AbortAfter40
            | TelemetryCommand::AbortAfter250
            | TelemetryCommand::ReinitAfter15
            | TelemetryCommand::ReinitAfter30
            | TelemetryCommand::ReinitAfter50
            => mode == Mode::Hitl,
            _ => true,
        }
    }
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
    /// The FC is flying (its Ascent..Reefing). Network value 16; the FC's
    /// detailed phase reaches the GS only through its own state report.
    InFlight = 16,
}

/// The only FLIGHT_STATE values that may ever be on the network (the
/// state-split contract, set (a)). Every GS FLIGHT_STATE write is built from
/// one of these; fill sub-phases and turbocharge stay GS-internal.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq, Hash)]
#[repr(u8)]
pub enum NetworkFlightState {
    Startup = 0,
    Idle = 1,
    Fill = 2,
    Armed = 6,
    Launch = 7,
    Landed = 13,
    Aborted = 15,
    InFlight = 16,
}

impl NetworkFlightState {
    pub const ALL: [NetworkFlightState; 8] = [
        Self::Startup, Self::Idle, Self::Fill, Self::Armed,
        Self::Launch, Self::Landed, Self::Aborted, Self::InFlight,
    ];
    /// A set-(a) wire byte; retired and unknown values are None.
    pub fn from_u8(value: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|state| *state as u8 == value)
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Startup => "Startup",
            Self::Idle => "Idle",
            Self::Fill => "Fill",
            Self::Armed => "Armed",
            Self::Launch => "Launch",
            Self::Landed => "Landed",
            Self::Aborted => "Aborted",
            Self::InFlight => "In flight",
        }
    }
    /// The GS-internal state a received network value means when the GS has
    /// no finer detail of its own: Fill starts at PreFill.
    pub fn local(self) -> FlightState {
        match self {
            Self::Startup => FlightState::Startup,
            Self::Idle => FlightState::Idle,
            Self::Fill => FlightState::PreFill,
            Self::Armed => FlightState::Armed,
            Self::Launch => FlightState::Launch,
            Self::Landed => FlightState::Landed,
            Self::Aborted => FlightState::Aborted,
            Self::InFlight => FlightState::InFlight,
        }
    }
}

impl FlightState {
    /// What this GS-internal (or FC-detailed) state is on the network.
    pub fn network(self) -> NetworkFlightState {
        use FlightState as F;
        match self {
            F::Startup => NetworkFlightState::Startup,
            F::Idle => NetworkFlightState::Idle,
            F::PreFill | F::FillTest | F::NitrogenFill | F::NitrousFill => NetworkFlightState::Fill,
            F::Armed => NetworkFlightState::Armed,
            F::Launch => NetworkFlightState::Launch,
            F::Ascent | F::Coast | F::Apogee | F::Descent | F::Reefing | F::InFlight => {
                NetworkFlightState::InFlight
            }
            F::Landed | F::Recovery => NetworkFlightState::Landed,
            F::Aborted => NetworkFlightState::Aborted,
        }
    }
    /// The GS-only fill sub-phase, None outside Fill.
    pub fn fill_phase(self) -> Option<FlightState> {
        matches!(self, FlightState::PreFill | FlightState::FillTest
            | FlightState::NitrogenFill | FlightState::NitrousFill).then_some(self)
    }
    /// Ascent..Reefing, or InFlight when the FC's detailed phase is unknown.
    pub fn is_in_flight(self) -> bool {
        self.network() == NetworkFlightState::InFlight
    }
}

/// A received FLIGHT_STATE byte under the state-split contract. A retired
/// value from old firmware (3-5, 8-12, 14) maps to the nearest network value
/// and is logged once per value as a contract violation; unknown values are
/// None.
pub fn network_flight_state_from_wire(value: u8, sender: &str) -> Option<NetworkFlightState> {
    if let Some(state) = NetworkFlightState::from_u8(value) {
        return Some(state);
    }
    let mapped = match value {
        3..=5 => NetworkFlightState::Fill,
        8..=12 => NetworkFlightState::InFlight,
        14 => NetworkFlightState::Landed,
        _ => return None,
    };
    note_retired_flight_state(value, sender, mapped);
    Some(mapped)
}

static RETIRED_FLIGHT_STATES_SEEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Logs one contract violation per retired value; returns whether this call
/// was the first for `value`.
fn note_retired_flight_state(value: u8, sender: &str, mapped: NetworkFlightState) -> bool {
    let bit = 1u32 << value.min(31);
    let first = RETIRED_FLIGHT_STATES_SEEN.fetch_or(bit, std::sync::atomic::Ordering::Relaxed) & bit == 0;
    if first {
        log::warn!(
            "FLIGHT_STATE contract violation: {sender} sent retired value {value}; treated as {} ({}). Update that board's firmware.",
            mapped as u8,
            mapped.label()
        );
    }
    first
}

#[cfg(test)]
pub(crate) fn retired_flight_states_seen_for_test() -> u32 {
    RETIRED_FLIGHT_STATES_SEEN.load(std::sync::atomic::Ordering::Relaxed)
}

/// SEDSnet compact-bundle node table. Every SEDS build (GS, RRLS and all
/// boards) must list the same names in the same order; append only.
pub const SEDS_NODE_TABLE: [&str; 9] = ["GS", "RRLS", "RF", "FC", "PB", "AB", "VB", "GB", "DAQ"];

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
    /// RF/PB optional router services that failed to start and are being
    /// retried (BOARD_HEALTH word 6 bits 16..23); empty when all started.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub service_faults: Vec<String>,
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
    /// DAQ live input faults (DAQ_INPUT_HEALTH). Missing on older firmware/other boards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daq_input: Option<DaqInputHealthStatus>,
    /// DAQ DAC signal generator: any output driven, from DAQ_DAC_STATUS.
    #[serde(default)]
    pub dac_active: Option<bool>,
    #[serde(default)]
    pub dac_status_received_age_ms: Option<u64>,
    /// RF radio TX queue timing block. Missing on older RF firmware/other boards.
    #[serde(default)]
    pub rf_tx_timing: Option<RfTxTiming>,
    /// Board's own network-time quality (SEDSnet 4.4). Missing on older firmware.
    #[serde(default)]
    pub time_sync: Option<BoardTimeSync>,
}

/// DAQ input-health block: each monitored input fault and the readings behind
/// it. Voltages are millivolts; `None` means no reading in the last interval.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Default, Hash)]
pub struct DaqInputHealthStatus {
    pub received_age_ms: u64,
    pub faults: Vec<DaqInputFaultStatus>,
    /// The selected KG50 input is the P6 differential (leg checks apply).
    pub kg50_p6: bool,
    /// Station setting: a chamber pressure transducer is connected. When false
    /// the chamber check is not listed and the chamber reads "not installed".
    pub chamber_pt_installed: bool,
    pub vmain_mv: Option<u16>,
    /// Lowest VMAIN since the DAQ's previous report (about 1 s).
    pub vmain_min_mv: Option<u16>,
    pub chamber_mv: Option<u16>,
    pub chamber_min_mv: Option<u16>,
    pub p6_imbalance_max_mv: Option<u16>,
    pub p6_common_mv: Option<u16>,
    /// The DAQ's configured supply undervoltage and chamber no-signal levels.
    pub supply_low_mv: u16,
    pub chamber_floor_mv: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Default, Hash)]
pub struct DaqInputFaultStatus {
    pub key: String,
    pub label: String,
    /// Debounced and present now.
    pub active: bool,
    /// Seen, even briefly, since the DAQ's previous report.
    pub latched: bool,
    /// Episodes since the DAQ booted (saturates at 65535).
    pub episodes: u16,
    pub advice: String,
}

/// A board's network-time quality from its BOARD_HEALTH sync byte: "synced
/// +-X ms" or "unsynced".
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq, Default, Hash)]
pub struct BoardTimeSync {
    /// Locked (or holding over within its bound): its timestamps may be trusted.
    pub synced: bool,
    /// Error bound of the board's network time, microseconds; `None` when unknown.
    pub error_bound_us: Option<u64>,
    /// Where its time comes from: stratum 1 = the GS itself, 2 = RF
    /// re-serving the GS over CAN; `None` when the board does not report it.
    #[serde(default)]
    pub stratum: Option<u8>,
}

/// RF BOARD_HEALTH words 16..21 ("RFT1" tag). Timing and high-water cover the
/// interval since the previous report; stale drops are cumulative.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Default, Hash)]
pub struct RfTxTiming {
    /// Canonical ACK frames whose UART DMA completed in the interval.
    pub ack_frames: u32,
    /// Mean ACK enqueue-to-DMA-complete time, ms (0 without samples).
    pub ack_mean_ms: u32,
    /// Maximum ACK enqueue-to-DMA-complete time, ms.
    pub ack_max_ms: u32,
    /// Highest owned radio TX queue depth, descriptors.
    pub queue_high_water: u32,
    /// Ordinary frames dropped for exceeding the queue age limit since boot.
    pub stale_drops: u32,
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
    /// Latest packet's board network time minus station receive time. A large
    /// value means the board's clock is not synced to the station.
    #[serde(default)]
    pub clock_offset_ms: Option<i64>,
}

/// The station's own time: network-time quality (it normally leads, so this
/// is "reference"; an RRLS following the GS reports its sync) and the
/// absolute-UTC source ("UTC via GPS +-X ms", "via NTP", "unsynced").
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Default)]
pub struct StationTimeStatus {
    /// 0 unsynced, 1 acquiring, 2 locked, 3 holdover.
    pub sync_state: u8,
    pub is_reference: bool,
    pub stratum: u8,
    /// Network-time error bound, microseconds (`None` unknown).
    pub error_bound_us: Option<u64>,
    /// "gps", "ntp", "held" or "unsynced".
    pub utc_source: String,
    /// Absolute UTC error bound, microseconds (`None` unknown).
    pub utc_bound_us: Option<u64>,
    /// GPS reporting delay in use, microseconds, and whether it was learned
    /// against NTP (else the default).
    #[serde(default)]
    pub gps_delay_us: i64,
    #[serde(default)]
    pub gps_delay_learned: bool,
    /// Time-base epoch, step policy and the last step (SEDSnet 6).
    #[serde(default)]
    pub time_base: NetworkTimeBase,
}

/// The station's network time base: SEDSnet starts a new epoch on every step
/// of the network clock (an operator or host UTC correction over 1 s while
/// steps are allowed) and followers adopt it within an exchange.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Default)]
pub struct NetworkTimeBase {
    /// Epoch of the station's network clock.
    pub epoch: u16,
    /// Newest epoch heard on the network; above `epoch` the station is
    /// re-anchoring to it.
    pub newest_seen_epoch: u16,
    /// The station holds its clock until its source adopts its epoch.
    pub holding: bool,
    /// A local time source may step the network clock now (Startup/Idle or
    /// no board locked); otherwise every correction slews at 20 ppm.
    pub steps_allowed: bool,
    /// Steps and epoch changes since the station started.
    pub steps: u32,
    /// The most recent step or epoch change.
    pub last_step: Option<NetworkTimeStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Default)]
pub struct NetworkTimeStep {
    /// New reading minus old reading, ms (0 for an epoch rename).
    pub delta_ms: i64,
    /// "local_source", "explicit", "utc_anchor", "source_epoch", "resync",
    /// "diverged", "renamed", "joined" or "raised".
    pub reason: String,
    pub from_epoch: u16,
    pub to_epoch: u16,
    /// Age of the step, ms.
    pub age_ms: u64,
}

impl NetworkTimeBase {
    pub fn from_status(status: &sedsnet::timesync::TimeSyncStatus) -> Self {
        Self {
            epoch: status.epoch,
            newest_seen_epoch: status.newest_seen_epoch,
            holding: status.holding,
            steps_allowed: status.steps_allowed,
            steps: status.steps,
            last_step: status.last_step.map(|step| NetworkTimeStep {
                delta_ms: step.delta_ns / 1_000_000,
                reason: step.reason.name().to_string(),
                from_epoch: step.from_epoch,
                to_epoch: step.to_epoch,
                age_ms: step.age_ms,
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Default)]
pub struct NetworkTopologyMsg {
    /// Station time quality; missing from older backends.
    #[serde(default)]
    pub station_time: Option<StationTimeStatus>,
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
        16 => Some(FlightState::InFlight),
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
            .insert(
                canonical_phase_key(phase, version).to_owned(),
                value.clone(),
            )
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
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("test-fixtures/fc-global-state-contract.json"))
                .unwrap();
        for (wire, expected) in contract["global_states"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            let state = u8_to_flight_state(wire as u8).unwrap();
            assert_eq!(state as usize, wire);
            assert_eq!(serde_json::to_value(state).unwrap(), *expected);
        }
        assert_eq!(u8_to_flight_state(16), Some(FlightState::InFlight));
        assert_eq!(u8_to_flight_state(17), None);
        assert_eq!(u8_to_flight_state(255), None);
        let legacy: FlightState = serde_json::from_str("\"ParachuteDeploy\"").unwrap();
        assert_eq!(legacy, FlightState::Descent);
        assert_eq!(legacy as u8, 11);
    }

    /// The state-split contract: only set (a) is ever built for the wire.
    #[test]
    fn every_local_state_maps_to_a_set_a_network_value() {
        let set_a = [0u8, 1, 2, 6, 7, 13, 15, 16];
        let network: Vec<u8> = NetworkFlightState::ALL.iter().map(|s| *s as u8).collect();
        assert_eq!(network, set_a);
        for code in 0..=16u8 {
            let local = u8_to_flight_state(code).unwrap();
            assert!(set_a.contains(&(local.network() as u8)), "{local:?}");
        }
        use FlightState as F;
        for fill in [F::PreFill, F::FillTest, F::NitrogenFill, F::NitrousFill] {
            assert_eq!(fill.network(), NetworkFlightState::Fill);
            assert_eq!(fill.fill_phase(), Some(fill));
        }
        for flying in [F::Ascent, F::Coast, F::Apogee, F::Descent, F::Reefing, F::InFlight] {
            assert_eq!(flying.network(), NetworkFlightState::InFlight);
            assert!(flying.is_in_flight());
        }
        assert_eq!(F::Recovery.network(), NetworkFlightState::Landed);
        assert_eq!(F::Idle.fill_phase(), None);
        for state in NetworkFlightState::ALL {
            assert_eq!(state.local().network(), state);
        }
    }

    #[test]
    fn retired_wire_values_map_to_the_nearest_network_value_and_log_once() {
        let sender = "FC";
        assert_eq!(network_flight_state_from_wire(16, sender), Some(NetworkFlightState::InFlight));
        assert_eq!(network_flight_state_from_wire(2, sender), Some(NetworkFlightState::Fill));
        for (value, expected) in [(3, NetworkFlightState::Fill), (4, NetworkFlightState::Fill),
            (5, NetworkFlightState::Fill), (8, NetworkFlightState::InFlight), (9, NetworkFlightState::InFlight),
            (10, NetworkFlightState::InFlight), (11, NetworkFlightState::InFlight),
            (12, NetworkFlightState::InFlight), (14, NetworkFlightState::Landed)] {
            assert_eq!(network_flight_state_from_wire(value, sender), Some(expected), "{value}");
            assert_ne!(retired_flight_states_seen_for_test() & (1 << value), 0, "{value} logged");
            assert!(!note_retired_flight_state(value, sender, expected), "{value} is logged only once");
        }
        assert_eq!(network_flight_state_from_wire(17, sender), None);
        assert_eq!(network_flight_state_from_wire(255, sender), None);
    }

    #[test]
    fn legacy_phase_settings_preserve_numeric_meaning_without_repeated_migration() {
        let mut phases = BTreeMap::from([("ParachuteDeploy".into(), 11), ("Descent".into(), 12)]);
        normalize_phase_map(&mut phases, 0).unwrap();
        assert_eq!(
            phases,
            BTreeMap::from([("Descent".into(), 11), ("Reefing".into(), 12)])
        );
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

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TelemetryRow {
    pub timestamp_ms: i64,
    pub data_type: String,
    #[serde(default)]
    pub sender_id: String,
    pub values: Vec<Option<f32>>,
}

impl Serialize for TelemetryRow {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let source = crate::telemetry_provenance::source(self);
        let mut row =
            serializer.serialize_struct("TelemetryRow", if source.is_some() { 5 } else { 4 })?;
        row.serialize_field("timestamp_ms", &self.timestamp_ms)?;
        row.serialize_field("data_type", &self.data_type)?;
        row.serialize_field("sender_id", &self.sender_id)?;
        row.serialize_field("values", &self.values)?;
        if let Some(source) = source {
            row.serialize_field("source_timestamp_ms", &source)?;
        }
        row.end()
    }
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

impl TelemetryCommand {
    pub(crate) fn automatic_generation(&self) -> Option<u64> {
        match self {
            Self::AutomaticAbort { generation }
            | Self::AutomaticNitrogenClose { generation }
            | Self::AutomaticNitrousClose { generation } => Some(*generation),
            _ => None,
        }
    }
}
