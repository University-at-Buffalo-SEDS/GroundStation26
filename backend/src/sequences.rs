use crate::layout;
use crate::rocket_commands::{ActuatorBoardCommands, ValveBoardCommands};
use crate::state::AppState;
use crate::types::{FlightState, TelemetryCommand};
use crate::web::emit_warning;
use crate::{fill_targets, loadcell};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

pub const KEY_ENABLE_PIN: u8 = 25;
pub const SOFTWARE_DISABLE_PIN: u8 = 8;
const DEFAULT_NITROGEN_PRESSURE_TARGET_PSI: f32 = 120.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NitrogenAutocloseMode {
    Pressure,
    Weight,
    Both,
    Either,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BlinkMode {
    None,
    Slow,
    Fast,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActionControl {
    pub cmd: String,
    pub enabled: bool,
    pub blink: BlinkMode,
    pub actuated: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActionPolicyMsg {
    pub key_enabled: bool,
    pub software_buttons_enabled: bool,
    pub controls: Vec<ActionControl>,
}

fn backend_illuminated_commands() -> HashSet<String> {
    layout::load_layout()
        .map(|layout| {
            layout
                .actions_tab
                .actions
                .into_iter()
                .filter(|action| action.illuminated)
                .map(|action| action.cmd)
                .collect()
        })
        .unwrap_or_default()
}

fn backend_blink_for(cmd: &str, enabled: bool, recommended: Option<&BlinkMode>) -> BlinkMode {
    if !enabled {
        return BlinkMode::None;
    }
    if let Some(blink) = recommended {
        return blink.clone();
    }
    if is_recording_command(cmd) {
        return BlinkMode::None;
    }
    if backend_illuminated_commands().contains(cmd) {
        return BlinkMode::Slow;
    }
    BlinkMode::None
}

fn is_recording_command(cmd: &str) -> bool {
    matches!(
        cmd,
        "StartWritingNow" | "StartWritingLastTwoMinutes" | "PauseWritingDb" | "StopWritingDb"
    )
}

fn default_recording_command_actuated(cmd: &str) -> Option<bool> {
    match cmd {
        "StartWritingNow" | "StartWritingLastTwoMinutes" => Some(false),
        "PauseWritingDb" => Some(false),
        "StopWritingDb" => Some(true),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistentNotification {
    pub id: u64,
    pub timestamp_ms: i64,
    pub message: String,
    pub persistent: bool,
    #[serde(default)]
    pub action_label: Option<String>,
    #[serde(default)]
    pub action_cmd: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SequenceStep {
    SetupValves,
    NitrogenFill,
    CloseNitrogen,
    NitrogenLeakCheck,
    DumpNitrogen,
    CloseDump,
    AwaitFillTestDecision,
    RecoverNitrogenClose,
    RecoverNitrogenVent,
    RecoverNitrogenCloseDump,
    OpenNitrous,
    NitrousSoak,
    CloseNitrous,
    RecoverNitrousClose,
    RecoverNitrousVent,
    RecoverNitrousCloseDump,
    CloseNormallyOpen,
    RetractFillLines,
    ArmedReady,
}

#[derive(Clone, Debug)]
struct SequenceConfig {
    leak_check_duration: Duration,
    nitrous_soak_duration: Duration,
    nitrous_level_duration: Duration,
    nitrogen_pressure_target_psi: f32,
    nitrogen_target_mass_kg: Option<f32>,
    nitrogen_autoclose_mode: NitrogenAutocloseMode,
    nitrous_pressure_min_psi: f32,
    nitrous_rise_epsilon_psi: f32,
    dump_pressure_max_psi: f32,
    max_leak_drop_psi: f32,
    max_leak_mass_delta_kg: f32,
    allowed_hold_drop_psi_per_min: f32,
    normally_open_hold_drop_psi_per_min: f32,
    allowed_hold_mass_drop_kg_per_min: f32,
    nitrous_weight_rise_epsilon_kg: f32,
    empty_mass_noise_allowance_kg: f32,
    calibration_pressure_min_psi: f32,
    calibration_pressure_max_psi: f32,
    calibration_mass_min_kg: f32,
    calibration_mass_max_kg: f32,
    pending_fast_window: Duration,
    key_required: bool,
    key_enable_pin: u8,
    software_disable_pin: u8,
}

impl SequenceConfig {
    fn from_env() -> Self {
        let fill_cfg = fill_targets::load_or_default();
        let leak_check_duration = std::env::var("GS_SEQUENCE_LEAK_CHECK_SEC")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(60));

        let pressure_min_psi = std::env::var("GS_SEQUENCE_PRESSURE_MIN_PSI")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(10.0);

        let nitrogen_pressure_target_psi =
            std::env::var("GS_SEQUENCE_NITROGEN_PRESSURE_TARGET_PSI")
                .ok()
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(fill_cfg.nitrogen.target_pressure_psi)
                .max(
                    DEFAULT_NITROGEN_PRESSURE_TARGET_PSI.min(fill_cfg.nitrogen.target_pressure_psi),
                );

        let nitrous_pressure_min_psi = std::env::var("GS_SEQUENCE_NITROUS_PRESSURE_MIN_PSI")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(fill_cfg.nitrous.target_pressure_psi.max(pressure_min_psi));

        let nitrogen_target_mass_kg = std::env::var("GS_SEQUENCE_NITROGEN_TARGET_MASS_KG")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|v| *v > 0.0)
            .or(Some(fill_cfg.nitrogen.target_mass_kg.max(0.01)));

        let nitrogen_autoclose_mode = std::env::var("GS_SEQUENCE_NITROGEN_AUTOCLOSE_MODE")
            .ok()
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .and_then(|value| match value.as_str() {
                "pressure" => Some(NitrogenAutocloseMode::Pressure),
                "weight" => Some(NitrogenAutocloseMode::Weight),
                "both" => Some(NitrogenAutocloseMode::Both),
                "either" | "any" => Some(NitrogenAutocloseMode::Either),
                _ => None,
            })
            .unwrap_or_else(|| {
                if nitrogen_target_mass_kg.is_some() {
                    NitrogenAutocloseMode::Either
                } else {
                    NitrogenAutocloseMode::Pressure
                }
            });

        let dump_pressure_max_psi = std::env::var("GS_SEQUENCE_DUMP_PRESSURE_MAX_PSI")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(5.0);

        let nitrous_soak_duration = std::env::var("GS_SEQUENCE_NITROUS_SOAK_SEC")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(30));

        let nitrous_level_duration = std::env::var("GS_SEQUENCE_NITROUS_LEVEL_SEC")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(3));

        let nitrous_rise_epsilon_psi = std::env::var("GS_SEQUENCE_NITROUS_RISE_EPSILON_PSI")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(0.15);

        let max_leak_drop_psi = std::env::var("GS_SEQUENCE_MAX_LEAK_DROP_PSI")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(1.0);

        let max_leak_mass_delta_kg = std::env::var("GS_SEQUENCE_MAX_LEAK_MASS_DELTA_KG")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(0.15);

        let allowed_hold_drop_psi_per_min =
            std::env::var("GS_SEQUENCE_ALLOWED_HOLD_DROP_PSI_PER_MIN")
                .ok()
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(0.2);

        let normally_open_hold_drop_psi_per_min =
            std::env::var("GS_SEQUENCE_NO_HOLD_DROP_PSI_PER_MIN")
                .ok()
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(20.0);

        let allowed_hold_mass_drop_kg_per_min =
            std::env::var("GS_SEQUENCE_ALLOWED_HOLD_MASS_DROP_KG_PER_MIN")
                .ok()
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(0.03);

        let nitrous_weight_rise_epsilon_kg =
            std::env::var("GS_SEQUENCE_NITROUS_WEIGHT_RISE_EPSILON_KG")
                .ok()
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(0.03);

        let empty_mass_noise_allowance_kg = std::env::var("GS_SEQUENCE_EMPTY_MASS_NOISE_KG")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(3.0)
            .max(0.0);

        let calibration_pressure_min_psi =
            std::env::var("GS_SEQUENCE_CALIBRATION_PRESSURE_MIN_PSI")
                .ok()
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(0.0);

        let calibration_pressure_max_psi =
            std::env::var("GS_SEQUENCE_CALIBRATION_PRESSURE_MAX_PSI")
                .ok()
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(3000.0)
                .max(calibration_pressure_min_psi);

        let calibration_mass_min_kg = std::env::var("GS_SEQUENCE_CALIBRATION_MASS_MIN_KG")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(-empty_mass_noise_allowance_kg);

        let calibration_mass_max_kg = std::env::var("GS_SEQUENCE_CALIBRATION_MASS_MAX_KG")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or_else(|| {
                fill_cfg
                    .nitrous
                    .target_mass_kg
                    .max(fill_cfg.nitrogen.target_mass_kg)
                    .max(loadcell::DEFAULT_FULL_MASS_KG)
                    + empty_mass_noise_allowance_kg
            })
            .max(calibration_mass_min_kg);

        let pending_fast_window = std::env::var("GS_SEQUENCE_PENDING_FAST_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_millis(4_000));

        let key_required = if cfg!(feature = "raspberry_pi") {
            std::env::var("GS_KEY_REQUIRED")
                .ok()
                .as_deref()
                .unwrap_or("1")
                != "0"
        } else {
            std::env::var("GS_KEY_REQUIRED")
                .ok()
                .as_deref()
                .unwrap_or("0")
                != "0"
        };

        let key_enable_pin = std::env::var("GS_KEY_ENABLE_PIN")
            .ok()
            .and_then(|v| v.parse::<u8>().ok())
            .unwrap_or(KEY_ENABLE_PIN);

        let software_disable_pin = std::env::var("GS_SOFTWARE_DISABLE_PIN")
            .ok()
            .and_then(|v| v.parse::<u8>().ok())
            .unwrap_or(SOFTWARE_DISABLE_PIN);

        Self {
            leak_check_duration,
            nitrous_soak_duration,
            nitrous_level_duration,
            nitrogen_pressure_target_psi,
            nitrogen_target_mass_kg,
            nitrogen_autoclose_mode,
            nitrous_pressure_min_psi,
            nitrous_rise_epsilon_psi,
            dump_pressure_max_psi,
            max_leak_drop_psi,
            max_leak_mass_delta_kg,
            allowed_hold_drop_psi_per_min,
            normally_open_hold_drop_psi_per_min,
            allowed_hold_mass_drop_kg_per_min,
            nitrous_weight_rise_epsilon_kg,
            empty_mass_noise_allowance_kg,
            calibration_pressure_min_psi,
            calibration_pressure_max_psi,
            calibration_mass_min_kg,
            calibration_mass_max_kg,
            pending_fast_window,
            key_required,
            key_enable_pin,
            software_disable_pin,
        }
    }
}

#[derive(Clone, Debug)]
struct SequenceRuntime {
    step: SequenceStep,
    next_step_after_dump: Option<SequenceStep>,
    step_started_at: Option<Instant>,
    pressure_at_close_psi: Option<f32>,
    mass_at_close_kg: Option<f32>,
    notified_leak_pass: bool,
    notified_armed: bool,
    warned_rapid_drop: bool,
    warned_mass_shift: bool,
    leak_fail_notification_id: Option<u64>,
    notified_close_nitrous: bool,
    notified_close_normally_open: bool,
    nitrous_level_since: Option<Instant>,
    last_nitrous_pressure_psi: Option<f32>,
    last_nitrous_mass_kg: Option<f32>,
    notified_retract_fill_lines: bool,
    auto_close_nitrogen_sent: bool,
    auto_close_nitrous_sent: bool,
    notified_reopen_normally_open: bool,
    notified_nitrogen_dump_recovery: bool,
    notified_nitrogen_recovery_vent: bool,
    notified_nitrogen_recovery_close_dump: bool,
    notified_nitrous_dump_recovery: bool,
    notified_nitrous_recovery_vent: bool,
    notified_nitrous_recovery_close_dump: bool,
    calibration_ready: bool,
    calibration_block_notification_id: Option<u64>,
    calibration_block_message: Option<String>,
}

impl Default for SequenceRuntime {
    fn default() -> Self {
        Self {
            step: SequenceStep::SetupValves,
            next_step_after_dump: None,
            step_started_at: None,
            pressure_at_close_psi: None,
            mass_at_close_kg: None,
            notified_leak_pass: false,
            notified_armed: false,
            warned_rapid_drop: false,
            warned_mass_shift: false,
            leak_fail_notification_id: None,
            notified_close_nitrous: false,
            notified_close_normally_open: false,
            nitrous_level_since: None,
            last_nitrous_pressure_psi: None,
            last_nitrous_mass_kg: None,
            notified_retract_fill_lines: false,
            auto_close_nitrogen_sent: false,
            auto_close_nitrous_sent: false,
            notified_reopen_normally_open: false,
            notified_nitrogen_dump_recovery: false,
            notified_nitrogen_recovery_vent: false,
            notified_nitrogen_recovery_close_dump: false,
            notified_nitrous_dump_recovery: false,
            notified_nitrous_recovery_vent: false,
            notified_nitrous_recovery_close_dump: false,
            calibration_ready: false,
            calibration_block_notification_id: None,
            calibration_block_message: None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ValveSnapshot {
    normally_open: Option<bool>,
    dump_open: Option<bool>,
    nitrogen_open: Option<bool>,
    nitrous_open: Option<bool>,
    pilot_open: Option<bool>,
    igniter_on: Option<bool>,
    retract: Option<bool>,
}

impl ValveSnapshot {
    fn read(state: &AppState) -> Self {
        let valve = |cmd| state.get_umbilical_valve_state(cmd);
        Self {
            pilot_open: valve(ValveBoardCommands::PilotOpen as u8),
            normally_open: valve(ValveBoardCommands::NormallyOpenOpen as u8),
            dump_open: valve(ValveBoardCommands::DumpOpen as u8),
            igniter_on: valve(ActuatorBoardCommands::IgniterOn as u8),
            nitrogen_open: valve(ActuatorBoardCommands::NitrogenOpen as u8),
            nitrous_open: valve(ActuatorBoardCommands::NitrousOpen as u8),
            retract: valve(ActuatorBoardCommands::RetractPlumbing as u8),
        }
    }

    fn actuated_for_cmd(&self, cmd: &str) -> Option<bool> {
        match cmd {
            "Dump" => self.dump_open,
            "NormallyOpen" => self.normally_open,
            "Nitrogen" => self.nitrogen_open,
            "Nitrous" => self.nitrous_open,
            "ContinueFillSequence" => None,
            "Pilot" => self.pilot_open,
            "Igniter" => self.igniter_on,
            "RetractPlumbing" => self.retract,
            _ => None,
        }
    }
}

fn is_fill_state(state: FlightState) -> bool {
    matches!(
        state,
        FlightState::PreFill
            | FlightState::FillTest
            | FlightState::NitrogenFill
            | FlightState::NitrousFill
    )
}

pub fn command_name(cmd: &TelemetryCommand) -> &'static str {
    match cmd {
        TelemetryCommand::Dump => "Dump",
        TelemetryCommand::Abort => "Abort",
        TelemetryCommand::NormallyOpen => "NormallyOpen",
        TelemetryCommand::Pilot => "Pilot",
        TelemetryCommand::Igniter => "Igniter",
        TelemetryCommand::RetractPlumbing => "RetractPlumbing",
        TelemetryCommand::Nitrogen | TelemetryCommand::NitrogenClose => "Nitrogen",
        TelemetryCommand::Nitrous | TelemetryCommand::NitrousClose => "Nitrous",
        TelemetryCommand::StartWritingNow => "StartWritingNow",
        TelemetryCommand::StartWritingLastTwoMinutes => "StartWritingLastTwoMinutes",
        TelemetryCommand::PauseWritingDb => "PauseWritingDb",
        TelemetryCommand::StopWritingDb => "StopWritingDb",
        TelemetryCommand::ResetSim => "ResetSim",
        TelemetryCommand::ContinueFillSequence => "ContinueFillSequence",
        TelemetryCommand::PostinitSignal => "PostinitSignal",
        TelemetryCommand::LaunchSignal => "LaunchSignal",
        TelemetryCommand::RollbackSignal => "RollbackSignal",
        TelemetryCommand::MonitorAltitude => "MonitorAltitude",
        TelemetryCommand::RevokeMonitorAltitude => "RevokeMonitorAltitude",
        TelemetryCommand::ConsecutiveSamples => "ConsecutiveSamples",
        TelemetryCommand::RevokeConsecutiveSamples => "RevokeConsecutiveSamples",
        TelemetryCommand::ResetFailures => "ResetFailures",
        TelemetryCommand::RevokeResetFailures => "RevokeResetFailures",
        TelemetryCommand::ValidateMeasms => "ValidateMeasms",
        TelemetryCommand::RevokeValidateMeasms => "RevokeValidateMeasms",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::DeployParachute => "DeployParachute",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::ExpandParachute => "ExpandParachute",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::EvaluationRelax => "EvaluationRelax",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::EvaluationFocus => "EvaluationFocus",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::EvaluationAbort => "EvaluationAbort",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::ReinitSensors => "ReinitSensors",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::ReinitBarometer => "ReinitBarometer",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::ReinitIMU => "ReinitIMU",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::DisableIMU => "DisableIMU",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::AdvanceFlightState => "AdvanceFlightState",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::RewindFlightState => "RewindFlightState",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::AbortAfter40 => "AbortAfter40",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::AbortAfter100 => "AbortAfter100",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::AbortAfter250 => "AbortAfter250",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::ReinitAfter15 => "ReinitAfter15",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::ReinitAfter30 => "ReinitAfter30",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::ReinitAfter50 => "ReinitAfter50",
    }
}

#[cfg(all(not(feature = "hitl_mode"), not(feature = "test_fire_mode")))]
pub fn all_command_names() -> Vec<&'static str> {
    let mut names = vec![
        "Dump",
        "Abort",
        "NormallyOpen",
        "Pilot",
        "Igniter",
        "RetractPlumbing",
        "Nitrogen",
        "Nitrous",
        "StartWritingNow",
        "StartWritingLastTwoMinutes",
        "PauseWritingDb",
        "StopWritingDb",
        "ContinueFillSequence",
        "Postinit",
        "Launch",
        "Rollback",
        "MonitorAltitude",
        "RevokeMonitorAltitude",
        "ConsecutiveSamples",
        "RevokeConsecutiveSamples",
        "ResetFailures",
        "RevokeResetFailures",
        "ValidateMeasms",
        "RevokeValidateMeasms",
    ];
    if crate::flight_sim::sim_mode_enabled() {
        names.push("ResetSim");
    }
    names
}

#[cfg(feature = "hitl_mode")]
pub fn all_command_names() -> Vec<&'static str> {
    vec![
        "Dump",
        "Abort",
        "NormallyOpen",
        "Pilot",
        "Igniter",
        "RetractPlumbing",
        "Nitrogen",
        "Nitrous",
        "StartWritingNow",
        "StartWritingLastTwoMinutes",
        "PauseWritingDb",
        "StopWritingDb",
        "ContinueFillSequence",
        "Postinit",
        "Launch",
        "Rollback",
        "MonitorAltitude",
        "RevokeMonitorAltitude",
        "ConsecutiveSamples",
        "RevokeConsecutiveSamples",
        "ResetFailures",
        "RevokeResetFailures",
        "ValidateMeasms",
        "RevokeValidateMeasms",
        "DeployParachute",
        "ExpandParachute",
        "EvaluationRelax",
        "EvaluationFocus",
        "EvaluationAbort",
        "ReinitSensors",
        "ReinitBarometer",
        "ReinitIMU",
        "DisableIMU",
        "AdvanceFlightState",
        "RewindFlightState",
        "AbortAfter40",
        "AbortAfter100",
        "AbortAfter250",
        "ReinitAfter15",
        "ReinitAfter30",
        "ReinitAfter50",
    ]
}

#[cfg(all(not(feature = "hitl_mode"), feature = "test_fire_mode"))]
pub fn all_command_names() -> Vec<&'static str> {
    vec![
        "Dump",
        "Abort",
        "NormallyOpen",
        "Pilot",
        "Igniter",
        "RetractPlumbing",
        "Nitrogen",
        "Nitrous",
        "StartWritingNow",
        "StartWritingLastTwoMinutes",
        "PauseWritingDb",
        "StopWritingDb",
        "ContinueFillSequence",
        "Postinit",
        "Launch",
        "Rollback",
        "MonitorAltitude",
        "RevokeMonitorAltitude",
        "ConsecutiveSamples",
        "RevokeConsecutiveSamples",
        "ResetFailures",
        "RevokeResetFailures",
        "ValidateMeasms",
        "RevokeValidateMeasms",
    ]
}

pub fn default_action_policy() -> ActionPolicyMsg {
    let controls = all_command_names()
        .into_iter()
        .map(|cmd| {
            let enabled = matches!(
                cmd,
                "Abort"
                    | "ResetSim"
                    | "StartWritingNow"
                    | "StartWritingLastTwoMinutes"
                    | "PauseWritingDb"
                    | "StopWritingDb"
            );
            ActionControl {
                cmd: cmd.to_string(),
                enabled,
                blink: backend_blink_for(cmd, enabled, None),
                actuated: default_recording_command_actuated(cmd),
            }
        })
        .collect();
    ActionPolicyMsg {
        key_enabled: true,
        software_buttons_enabled: true,
        controls,
    }
}

fn policy_with_overrides(
    key_enabled: bool,
    software_buttons_enabled: bool,
    valves: ValveSnapshot,
    recommended: HashMap<&'static str, BlinkMode>,
) -> ActionPolicyMsg {
    let controls = all_command_names()
        .into_iter()
        .map(|cmd| {
            // Keep controls pressable while key is enabled; blink indicates recommendation.
            let enabled = if matches!(
                cmd,
                "Abort"
                    | "StartWritingNow"
                    | "StartWritingLastTwoMinutes"
                    | "PauseWritingDb"
                    | "StopWritingDb"
            ) {
                true
            } else if cmd == "ContinueFillSequence" {
                false
            } else if cmd == "RetractPlumbing" && valves.retract == Some(true) {
                // Fill lines are one-way: once retracted, do not re-enable.
                false
            } else {
                key_enabled
            };
            ActionControl {
                cmd: cmd.to_string(),
                enabled,
                blink: backend_blink_for(cmd, enabled, recommended.get(cmd)),
                actuated: if is_recording_command(cmd) {
                    Some(true)
                } else {
                    valves.actuated_for_cmd(cmd)
                },
            }
        })
        .collect();

    ActionPolicyMsg {
        key_enabled,
        software_buttons_enabled,
        controls,
    }
}

fn pending_mode(
    state: &AppState,
    cmd: &'static str,
    now_ms: u64,
    cfg: &SequenceConfig,
) -> BlinkMode {
    if let Some(last_ms) = state.last_command_timestamp_ms(cmd)
        && now_ms.saturating_sub(last_ms) <= cfg.pending_fast_window.as_millis() as u64
    {
        return BlinkMode::Fast;
    }
    BlinkMode::Slow
}

fn set_control_enabled(policy: &mut ActionPolicyMsg, cmd: &str, enabled: bool) {
    if let Some(control) = policy
        .controls
        .iter_mut()
        .find(|control| control.cmd == cmd)
    {
        control.enabled = enabled;
        if !enabled {
            control.blink = BlinkMode::None;
        }
    }
}

fn sequence_expects_normally_open(step: SequenceStep) -> bool {
    matches!(
        step,
        SequenceStep::NitrogenFill
            | SequenceStep::CloseNitrogen
            | SequenceStep::NitrogenLeakCheck
            | SequenceStep::DumpNitrogen
            | SequenceStep::CloseDump
            | SequenceStep::AwaitFillTestDecision
            | SequenceStep::RecoverNitrogenClose
            | SequenceStep::RecoverNitrogenVent
            | SequenceStep::RecoverNitrogenCloseDump
            | SequenceStep::OpenNitrous
            | SequenceStep::NitrousSoak
            | SequenceStep::CloseNitrous
            | SequenceStep::RecoverNitrousClose
            | SequenceStep::RecoverNitrousVent
            | SequenceStep::RecoverNitrousCloseDump
    )
}

fn sequence_blocks_until_normally_open(step: SequenceStep) -> bool {
    matches!(
        step,
        SequenceStep::NitrogenFill
            | SequenceStep::CloseNitrogen
            | SequenceStep::NitrogenLeakCheck
            | SequenceStep::DumpNitrogen
            | SequenceStep::CloseDump
            | SequenceStep::AwaitFillTestDecision
            | SequenceStep::OpenNitrous
            | SequenceStep::NitrousSoak
            | SequenceStep::CloseNitrous
    )
}

fn dump_open_fails_nitrogen_step(step: SequenceStep) -> bool {
    matches!(
        step,
        SequenceStep::NitrogenFill | SequenceStep::CloseNitrogen | SequenceStep::NitrogenLeakCheck
    )
}

fn dump_open_fails_nitrous_step(step: SequenceStep) -> bool {
    matches!(
        step,
        SequenceStep::OpenNitrous | SequenceStep::CloseNitrous | SequenceStep::NitrousSoak
    )
}

fn mass_is_vented(current_mass_kg: Option<f32>, cfg: &SequenceConfig) -> bool {
    current_mass_kg
        .map(|m| m <= cfg.empty_mass_noise_allowance_kg)
        .unwrap_or(true)
}

fn tank_is_vented(
    pressure_psi: Option<f32>,
    current_mass_kg: Option<f32>,
    cfg: &SequenceConfig,
) -> bool {
    pressure_psi.is_some_and(|p| p <= cfg.dump_pressure_max_psi)
        && mass_is_vented(current_mass_kg, cfg)
}

fn calibrated_value_in_range(
    label: &str,
    value: Option<f32>,
    min: f32,
    max: f32,
) -> Result<(), String> {
    let value = value.ok_or_else(|| format!("{label} has no calibrated telemetry yet"))?;
    if !value.is_finite() {
        return Err(format!("{label} calibrated value is not finite"));
    }
    if value < min || value > max {
        return Err(format!(
            "{label} calibrated value {value:.2} is outside {min:.2}..{max:.2}"
        ));
    }
    Ok(())
}

fn fill_sequence_calibration_issue(
    _state: &AppState,
    cfg: &SequenceConfig,
    pressure_psi: Option<f32>,
    current_mass_kg: Option<f32>,
) -> Option<String> {
    if crate::flight_sim::sim_mode_enabled() {
        return None;
    }

    let mut issues = Vec::new();

    if let Err(issue) = calibrated_value_in_range(
        "Fill mass",
        current_mass_kg,
        cfg.calibration_mass_min_kg,
        cfg.calibration_mass_max_kg,
    ) {
        issues.push(issue);
    }
    if let Err(issue) = calibrated_value_in_range(
        "Tank pressure",
        pressure_psi,
        cfg.calibration_pressure_min_psi,
        cfg.calibration_pressure_max_psi,
    ) {
        issues.push(issue);
    }

    if issues.is_empty() {
        None
    } else {
        Some(format!(
            "Check sequence sensor calibration before starting fill sequence: {}.",
            issues.join("; ")
        ))
    }
}

fn nitrous_fill_status(state: &AppState, current_mass_kg: f32) -> (f32, f32) {
    let fill_target_mass_kg = fill_targets::load_or_default().nitrous.target_mass_kg;
    let loadcell_cfg = state.loadcell_calibration.lock().unwrap().clone();
    let target_mass_kg = loadcell_cfg
        .full_mass_kg
        .unwrap_or(fill_target_mass_kg)
        .max(0.0001);
    let percent = loadcell::fill_percent(&loadcell_cfg, current_mass_kg);
    (target_mass_kg, percent)
}

fn update_sequence_runtime(
    state: &AppState,
    runtime: &mut SequenceRuntime,
    cfg: &SequenceConfig,
    valves: ValveSnapshot,
    pressure_psi: Option<f32>,
    current_mass_kg: Option<f32>,
    now: Instant,
) {
    let at_or_above = |p: Option<f32>, threshold: f32| p.is_some_and(|x| x >= threshold);
    let dismiss_leak_fail_notification = |state: &AppState, runtime: &mut SequenceRuntime| {
        if let Some(id) = runtime.leak_fail_notification_id.take() {
            let _ = state.dismiss_notification(id);
        }
    };

    if valves.dump_open == Some(true) && dump_open_fails_nitrogen_step(runtime.step) {
        dismiss_leak_fail_notification(state, runtime);
        runtime.step = SequenceStep::RecoverNitrogenClose;
        runtime.step_started_at = None;
        runtime.pressure_at_close_psi = None;
        runtime.mass_at_close_kg = None;
        runtime.next_step_after_dump = None;
        runtime.nitrous_level_since = None;
        runtime.auto_close_nitrogen_sent = false;
        runtime.notified_nitrogen_dump_recovery = false;
        runtime.notified_nitrogen_recovery_vent = false;
        runtime.notified_nitrogen_recovery_close_dump = false;
    }

    if valves.dump_open == Some(true) && dump_open_fails_nitrous_step(runtime.step) {
        dismiss_leak_fail_notification(state, runtime);
        runtime.step = SequenceStep::RecoverNitrousClose;
        runtime.step_started_at = None;
        runtime.nitrous_level_since = None;
        runtime.last_nitrous_pressure_psi = None;
        runtime.last_nitrous_mass_kg = None;
        runtime.auto_close_nitrous_sent = false;
        runtime.notified_nitrous_dump_recovery = false;
        runtime.notified_nitrous_recovery_vent = false;
        runtime.notified_nitrous_recovery_close_dump = false;
    }

    if sequence_blocks_until_normally_open(runtime.step) && valves.normally_open == Some(false) {
        if !runtime.notified_reopen_normally_open {
            state.add_notification(
                "Normally open valve is closed early. Open N/O before continuing the fill sequence.",
            );
            runtime.notified_reopen_normally_open = true;
        }
        return;
    }
    if valves.normally_open == Some(true) {
        runtime.notified_reopen_normally_open = false;
    }

    if matches!(runtime.step, SequenceStep::SetupValves) {
        if let Some(issue) =
            fill_sequence_calibration_issue(state, cfg, pressure_psi, current_mass_kg)
        {
            runtime.calibration_ready = false;
            if runtime.calibration_block_message.as_deref() != Some(issue.as_str()) {
                if let Some(id) = runtime.calibration_block_notification_id.take() {
                    let _ = state.dismiss_notification(id);
                }
                let id = state.add_notification(issue.clone());
                runtime.calibration_block_notification_id = Some(id);
                runtime.calibration_block_message = Some(issue);
            }
            return;
        }

        runtime.calibration_ready = true;
        if let Some(id) = runtime.calibration_block_notification_id.take() {
            let _ = state.dismiss_notification(id);
        }
        runtime.calibration_block_message = None;
    }

    match runtime.step {
        SequenceStep::SetupValves => {
            if valves.normally_open == Some(true) && valves.dump_open == Some(false) {
                runtime.step = SequenceStep::NitrogenFill;
            }
        }
        SequenceStep::NitrogenFill => {
            if valves.nitrogen_open != Some(true) {
                runtime.auto_close_nitrogen_sent = false;
                return;
            }

            let pressure_ready = at_or_above(pressure_psi, cfg.nitrogen_pressure_target_psi);
            let weight_ready = cfg
                .nitrogen_target_mass_kg
                .is_some_and(|target_mass_kg| current_mass_kg.is_some_and(|m| m >= target_mass_kg));
            let should_close = match cfg.nitrogen_autoclose_mode {
                NitrogenAutocloseMode::Pressure => pressure_ready,
                NitrogenAutocloseMode::Weight => weight_ready,
                NitrogenAutocloseMode::Both => pressure_ready && weight_ready,
                NitrogenAutocloseMode::Either => pressure_ready || weight_ready,
            };

            if should_close && !runtime.auto_close_nitrogen_sent {
                match state.cmd_tx.try_send(TelemetryCommand::NitrogenClose) {
                    Ok(_) => {
                        runtime.auto_close_nitrogen_sent = true;
                        match cfg.nitrogen_autoclose_mode {
                            NitrogenAutocloseMode::Pressure => {
                                state.add_notification(format!(
                                    "Nitrogen pressure target reached ({:.1} psi). Auto-closing nitrogen valve.",
                                    cfg.nitrogen_pressure_target_psi
                                ));
                            }
                            NitrogenAutocloseMode::Weight => {
                                let target_mass_kg =
                                    cfg.nitrogen_target_mass_kg.unwrap_or_default();
                                state.add_notification(format!(
                                    "Nitrogen weight target reached ({target_mass_kg:.2} kg). Auto-closing nitrogen valve."
                                ));
                            }
                            NitrogenAutocloseMode::Both => {
                                let target_mass_kg =
                                    cfg.nitrogen_target_mass_kg.unwrap_or_default();
                                state.add_notification(format!(
                                    "Nitrogen targets reached ({target_mass_kg:.2} kg, {:.1} psi). Auto-closing nitrogen valve.",
                                    cfg.nitrogen_pressure_target_psi
                                ));
                            }
                            NitrogenAutocloseMode::Either => {
                                let target_mass_kg =
                                    cfg.nitrogen_target_mass_kg.unwrap_or_default();
                                state.add_notification(format!(
                                    "Nitrogen fill target reached by weight or pressure ({target_mass_kg:.2} kg / {:.1} psi). Auto-closing nitrogen valve.",
                                    cfg.nitrogen_pressure_target_psi
                                ));
                            }
                        }
                        runtime.step = SequenceStep::CloseNitrogen;
                    }
                    Err(err) => {
                        emit_warning(
                            state,
                            format!("Auto-close nitrogen command failed at fill target: {err}"),
                        );
                    }
                }
            }
        }
        SequenceStep::CloseNitrogen => {
            if valves.nitrogen_open == Some(false) {
                runtime.auto_close_nitrogen_sent = false;
                runtime.pressure_at_close_psi = pressure_psi;
                runtime.mass_at_close_kg = current_mass_kg;
                runtime.step_started_at = Some(now);
                runtime.warned_rapid_drop = false;
                runtime.warned_mass_shift = false;
                if valves.normally_open == Some(true) {
                    state.add_temporary_notification(format!(
                        "Nitrogen fill test started. N/O is open, so a controlled pressure bleed is expected while loadcell hold is monitored for {}s.",
                        cfg.leak_check_duration.as_secs()
                    ));
                } else {
                    state.add_temporary_notification(format!(
                        "Nitrogen fill test started. Monitoring pressure and loadcell hold for {}s.",
                        cfg.leak_check_duration.as_secs()
                    ));
                }
                runtime.step = SequenceStep::NitrogenLeakCheck;
            }
        }
        SequenceStep::NitrogenLeakCheck => {
            let Some(started) = runtime.step_started_at else {
                runtime.step_started_at = Some(now);
                return;
            };
            let baseline = runtime.pressure_at_close_psi.unwrap_or(0.0);
            let current = pressure_psi.unwrap_or(0.0);
            let drop_psi = (baseline - current).max(0.0);
            let mass_baseline = runtime.mass_at_close_kg.unwrap_or(0.0);
            let mass_shift_kg = current_mass_kg
                .map(|m| (m - mass_baseline).abs())
                .unwrap_or(0.0);
            let elapsed_min = now.saturating_duration_since(started).as_secs_f32() / 60.0;
            let allowed_pressure_drop = cfg.max_leak_drop_psi
                + elapsed_min
                    * if valves.normally_open == Some(true) {
                        cfg.normally_open_hold_drop_psi_per_min
                    } else {
                        cfg.allowed_hold_drop_psi_per_min
                    };
            let allowed_mass_shift =
                cfg.max_leak_mass_delta_kg + elapsed_min * cfg.allowed_hold_mass_drop_kg_per_min;
            if !runtime.warned_rapid_drop && drop_psi > allowed_pressure_drop {
                runtime.warned_rapid_drop = true;
                emit_warning(
                    state,
                    format!(
                        "Pressure drop exceeded fill-test allowance: -{drop_psi:.2} psi from hold baseline"
                    ),
                );
            }
            if !runtime.warned_mass_shift && mass_shift_kg > cfg.max_leak_mass_delta_kg {
                runtime.warned_mass_shift = true;
                emit_warning(
                    state,
                    format!(
                        "Unexpected loadcell change detected during fill test: {mass_shift_kg:.2} kg from hold baseline"
                    ),
                );
            }
            if now.saturating_duration_since(started) < cfg.leak_check_duration {
                return;
            }

            let pressure_ok = current >= baseline - allowed_pressure_drop;
            let mass_ok = current_mass_kg.is_none() || mass_shift_kg <= allowed_mass_shift;

            if pressure_ok && mass_ok {
                dismiss_leak_fail_notification(state, runtime);
                if !runtime.notified_leak_pass {
                    if valves.normally_open == Some(true) {
                        state.add_notification(
                            "Nitrogen hold check passed. N/O bleed stayed within allowance and loadcell is stable.",
                        );
                    } else {
                        state.add_notification(
                            "Nitrogen hold check passed. Pressure and loadcell are stable.",
                        );
                    }
                    runtime.notified_leak_pass = true;
                }
                runtime.next_step_after_dump = Some(SequenceStep::OpenNitrous);
                runtime.step = SequenceStep::DumpNitrogen;
                runtime.step_started_at = None;
            } else {
                runtime.leak_fail_notification_id = Some(state.add_notification_action(
                    "Nitrogen hold check failed: pressure exceeded allowance or loadcell drifted. Dumping and awaiting operator decision.",
                    true,
                    Some("Continue anyway".to_string()),
                    Some("ContinueFillSequence".to_string()),
                ));
                runtime.next_step_after_dump = Some(SequenceStep::AwaitFillTestDecision);
                runtime.step = SequenceStep::DumpNitrogen;
                runtime.step_started_at = None;
            }
        }
        SequenceStep::DumpNitrogen => {
            if valves.dump_open == Some(true) && tank_is_vented(pressure_psi, current_mass_kg, cfg)
            {
                runtime.step = SequenceStep::CloseDump;
            }
        }
        SequenceStep::CloseDump => {
            if valves.dump_open == Some(false) {
                runtime.step = runtime
                    .next_step_after_dump
                    .take()
                    .unwrap_or(SequenceStep::OpenNitrous);
            }
        }
        SequenceStep::AwaitFillTestDecision => {
            if state.consume_fill_sequence_continue_requests() {
                dismiss_leak_fail_notification(state, runtime);
                state.add_notification(
                    "Operator override accepted. Continuing fill sequence to nitrous fill.",
                );
                runtime.step = SequenceStep::OpenNitrous;
                return;
            }

            if valves.nitrogen_open == Some(true) {
                dismiss_leak_fail_notification(state, runtime);
                runtime.auto_close_nitrogen_sent = false;
                runtime.step = SequenceStep::NitrogenFill;
            }
        }
        SequenceStep::RecoverNitrogenClose => {
            if !runtime.notified_nitrogen_dump_recovery {
                state.add_notification(
                    "Dump opened during nitrogen fill. Treating nitrogen fill as failed: close Nitrogen, keep Dump open while pressure and loadcell vent, then close Dump and refill nitrogen.",
                );
                runtime.notified_nitrogen_dump_recovery = true;
            }
            if valves.dump_open != Some(true) {
                return;
            }
            if valves.nitrogen_open == Some(false) {
                runtime.notified_nitrogen_recovery_vent = false;
                runtime.step = SequenceStep::RecoverNitrogenVent;
            }
        }
        SequenceStep::RecoverNitrogenVent => {
            if valves.dump_open != Some(true) {
                if !runtime.notified_nitrogen_recovery_vent {
                    state.add_notification(
                        "Nitrogen recovery is not vented yet. Reopen Dump and wait for pressure and loadcell to vent.",
                    );
                    runtime.notified_nitrogen_recovery_vent = true;
                }
                return;
            }
            if tank_is_vented(pressure_psi, current_mass_kg, cfg) {
                runtime.notified_nitrogen_recovery_close_dump = false;
                runtime.step = SequenceStep::RecoverNitrogenCloseDump;
            }
        }
        SequenceStep::RecoverNitrogenCloseDump => {
            if !runtime.notified_nitrogen_recovery_close_dump {
                state.add_notification(format!(
                    "Nitrogen recovery vented below {:.1} psi and {:.1} kg. Close Dump, then refill nitrogen.",
                    cfg.dump_pressure_max_psi, cfg.empty_mass_noise_allowance_kg
                ));
                runtime.notified_nitrogen_recovery_close_dump = true;
            }
            if valves.dump_open == Some(false) {
                runtime.auto_close_nitrogen_sent = false;
                runtime.notified_leak_pass = false;
                runtime.warned_rapid_drop = false;
                runtime.warned_mass_shift = false;
                runtime.step = SequenceStep::NitrogenFill;
            }
        }
        SequenceStep::OpenNitrous => {
            dismiss_leak_fail_notification(state, runtime);
            if valves.nitrous_open != Some(true) {
                runtime.nitrous_level_since = None;
                runtime.last_nitrous_pressure_psi = None;
                runtime.last_nitrous_mass_kg = None;
                runtime.auto_close_nitrous_sent = false;
                return;
            }

            let nitrous_full_by_loadcell = current_mass_kg.map(|m| {
                let (target_mass_kg, fill_percent) = nitrous_fill_status(state, m);
                (
                    target_mass_kg,
                    fill_percent,
                    m + cfg.nitrous_weight_rise_epsilon_kg >= target_mass_kg
                        || fill_percent >= 99.5,
                )
            });
            if let Some((target_mass_kg, fill_percent, true)) = nitrous_full_by_loadcell {
                if !runtime.auto_close_nitrous_sent {
                    match state.cmd_tx.try_send(TelemetryCommand::NitrousClose) {
                        Ok(_) => {
                            runtime.auto_close_nitrous_sent = true;
                            state.add_notification(format!(
                                "Nitrous loadcell target reached ({target_mass_kg:.2} kg, {fill_percent:.1}%). Auto-closing nitrous valve."
                            ));
                        }
                        Err(err) => {
                            emit_warning(
                                state,
                                format!(
                                    "Auto-close nitrous command failed at loadcell target: {err}"
                                ),
                            );
                        }
                    }
                }
                runtime.notified_close_nitrous = false;
                runtime.step = SequenceStep::CloseNitrous;
                return;
            }

            let Some(current_pressure) = pressure_psi else {
                return;
            };

            if !at_or_above(Some(current_pressure), cfg.nitrous_pressure_min_psi) {
                runtime.nitrous_level_since = None;
                runtime.last_nitrous_pressure_psi = Some(current_pressure);
                runtime.last_nitrous_mass_kg = current_mass_kg;
                return;
            }

            let rising = runtime
                .last_nitrous_pressure_psi
                .is_some_and(|prev| current_pressure > prev + cfg.nitrous_rise_epsilon_psi);
            let weight_rising = match (runtime.last_nitrous_mass_kg, current_mass_kg) {
                (Some(prev), Some(cur)) => cur > prev + cfg.nitrous_weight_rise_epsilon_kg,
                _ => false,
            };

            runtime.last_nitrous_pressure_psi = Some(current_pressure);
            runtime.last_nitrous_mass_kg = current_mass_kg;

            if rising || weight_rising {
                runtime.nitrous_level_since = None;
                return;
            }

            let leveled_since = runtime.nitrous_level_since.get_or_insert(now);
            if now.saturating_duration_since(*leveled_since) >= cfg.nitrous_level_duration {
                runtime.notified_close_nitrous = false;
                if !runtime.auto_close_nitrous_sent {
                    match state.cmd_tx.try_send(TelemetryCommand::NitrousClose) {
                        Ok(_) => {
                            runtime.auto_close_nitrous_sent = true;
                            state.add_notification(
                                "Nitrous fill has leveled by pressure and loadcell. Auto-closing nitrous valve.",
                            );
                        }
                        Err(err) => {
                            emit_warning(
                                state,
                                format!(
                                    "Auto-close nitrous command failed after fill leveled: {err}"
                                ),
                            );
                        }
                    }
                }
                runtime.step = SequenceStep::CloseNitrous;
            }
        }
        SequenceStep::CloseNitrous => {
            if !runtime.notified_close_nitrous {
                if runtime.auto_close_nitrous_sent {
                    state.add_notification("Waiting for nitrous valve to report closed.");
                } else {
                    state.add_notification("Close Nitrous valve to start the settle timer.");
                }
                runtime.notified_close_nitrous = true;
            }
            if valves.nitrous_open == Some(false) {
                runtime.auto_close_nitrous_sent = false;
                runtime.step_started_at = Some(now);
                state.add_temporary_notification(format!(
                    "Nitrous settle started. Waiting {}s before fill line removal.",
                    cfg.nitrous_soak_duration.as_secs()
                ));
                runtime.step = SequenceStep::NitrousSoak;
            }
        }
        SequenceStep::NitrousSoak => {
            let Some(started) = runtime.step_started_at else {
                runtime.step_started_at = Some(now);
                return;
            };
            if now.saturating_duration_since(started) >= cfg.nitrous_soak_duration {
                runtime.notified_close_normally_open = false;
                runtime.step = SequenceStep::CloseNormallyOpen;
            }
        }
        SequenceStep::RecoverNitrousClose => {
            if !runtime.notified_nitrous_dump_recovery {
                state.add_notification(
                    "Dump opened during nitrous fill. Treating nitrous fill as failed: close Nitrous, keep Dump open while pressure and loadcell vent, then close Dump and refill nitrous only.",
                );
                runtime.notified_nitrous_dump_recovery = true;
            }
            if valves.dump_open != Some(true) {
                return;
            }
            if valves.nitrous_open == Some(false) {
                runtime.notified_nitrous_recovery_vent = false;
                runtime.step = SequenceStep::RecoverNitrousVent;
            }
        }
        SequenceStep::RecoverNitrousVent => {
            if valves.dump_open != Some(true) {
                if !runtime.notified_nitrous_recovery_vent {
                    state.add_notification(
                        "Nitrous recovery is not vented yet. Reopen Dump and wait for pressure and loadcell to vent.",
                    );
                    runtime.notified_nitrous_recovery_vent = true;
                }
                return;
            }
            if tank_is_vented(pressure_psi, current_mass_kg, cfg) {
                runtime.notified_nitrous_recovery_close_dump = false;
                runtime.step = SequenceStep::RecoverNitrousCloseDump;
            }
        }
        SequenceStep::RecoverNitrousCloseDump => {
            if !runtime.notified_nitrous_recovery_close_dump {
                state.add_notification(format!(
                    "Nitrous recovery vented below {:.1} psi and {:.1} kg. Close Dump, then refill nitrous.",
                    cfg.dump_pressure_max_psi, cfg.empty_mass_noise_allowance_kg
                ));
                runtime.notified_nitrous_recovery_close_dump = true;
            }
            if valves.dump_open == Some(false) {
                runtime.nitrous_level_since = None;
                runtime.last_nitrous_pressure_psi = None;
                runtime.last_nitrous_mass_kg = None;
                runtime.auto_close_nitrous_sent = false;
                runtime.notified_close_nitrous = false;
                runtime.step = SequenceStep::OpenNitrous;
            }
        }
        SequenceStep::CloseNormallyOpen => {
            if !runtime.notified_close_normally_open {
                state.add_notification(
                    "Nitrous settle complete. Close normally open valve before retracting fill lines.",
                );
                runtime.notified_close_normally_open = true;
            }
            if valves.normally_open == Some(false) {
                runtime.notified_retract_fill_lines = false;
                runtime.step = SequenceStep::RetractFillLines;
            }
        }
        SequenceStep::RetractFillLines => {
            if !runtime.notified_retract_fill_lines {
                state.add_notification("Normally open valve closed. Retract fill lines.");
                runtime.notified_retract_fill_lines = true;
            }
            if valves.retract == Some(true) {
                runtime.step = SequenceStep::ArmedReady;
            }
        }
        SequenceStep::ArmedReady => {
            if !runtime.notified_armed {
                state.add_notification(
                    "Fill sequence complete: nitrous closed, normally open closed, fill lines removed. Launch state is ready.",
                );
                runtime.notified_armed = true;
            }
        }
    }
}

fn maybe_drive_local_prelaunch_state(
    state: &AppState,
    runtime: &SequenceRuntime,
    valves: ValveSnapshot,
    current_state: FlightState,
) -> FlightState {
    if (current_state as u8) > (FlightState::Armed as u8) {
        return current_state;
    }

    if current_state == FlightState::Startup && state.all_required_boards_seen() {
        state.set_local_flight_state(FlightState::Idle);
        return FlightState::Idle;
    }

    let desired_state = match runtime.step {
        SequenceStep::SetupValves => {
            if current_state == FlightState::Idle
                && runtime.calibration_ready
                && valves.normally_open == Some(true)
                && valves.dump_open == Some(false)
            {
                Some(FlightState::PreFill)
            } else {
                None
            }
        }
        SequenceStep::NitrogenFill | SequenceStep::CloseNitrogen => Some(FlightState::NitrogenFill),
        SequenceStep::NitrogenLeakCheck
        | SequenceStep::DumpNitrogen
        | SequenceStep::CloseDump
        | SequenceStep::RecoverNitrogenClose
        | SequenceStep::RecoverNitrogenVent
        | SequenceStep::RecoverNitrogenCloseDump => Some(FlightState::FillTest),
        SequenceStep::AwaitFillTestDecision => Some(FlightState::FillTest),
        SequenceStep::OpenNitrous
        | SequenceStep::CloseNitrous
        | SequenceStep::NitrousSoak
        | SequenceStep::RecoverNitrousClose
        | SequenceStep::RecoverNitrousVent
        | SequenceStep::RecoverNitrousCloseDump
        | SequenceStep::CloseNormallyOpen
        | SequenceStep::RetractFillLines => Some(FlightState::NitrousFill),
        SequenceStep::ArmedReady => Some(FlightState::Armed),
    };

    if let Some(next_state) = desired_state
        && current_state != next_state
    {
        state.set_local_flight_state(next_state);
        return next_state;
    }

    current_state
}

fn hitl_action_policy(valves: ValveSnapshot) -> ActionPolicyMsg {
    let controls = all_command_names()
        .into_iter()
        .map(|cmd| {
            let enabled = cmd != "RetractPlumbing";
            let actuated = if is_recording_command(cmd) {
                Some(true)
            } else {
                valves.actuated_for_cmd(cmd)
            };
            ActionControl {
                cmd: cmd.to_string(),
                enabled,
                blink: BlinkMode::None,
                actuated,
            }
        })
        .collect();

    ActionPolicyMsg {
        key_enabled: true,
        software_buttons_enabled: true,
        controls,
    }
}

#[derive(Clone, Copy)]
struct PolicyInputs {
    flight_state: FlightState,
    key_enabled: bool,
    software_buttons_enabled: bool,
    valves: ValveSnapshot,
    now_ms: u64,
}

fn build_policy(
    state: &AppState,
    cfg: &SequenceConfig,
    runtime: &SequenceRuntime,
    inputs: PolicyInputs,
) -> ActionPolicyMsg {
    if !inputs.key_enabled {
        let mut policy = policy_with_overrides(
            false,
            inputs.software_buttons_enabled,
            inputs.valves,
            HashMap::new(),
        );
        set_control_enabled(&mut policy, "Abort", true);
        return policy;
    }

    if inputs.flight_state == FlightState::Armed {
        let mut enabled = HashMap::new();
        enabled.insert("Launch", BlinkMode::Slow);
        enabled.insert("Dump", BlinkMode::None);
        return policy_with_overrides(
            true,
            inputs.software_buttons_enabled,
            inputs.valves,
            enabled,
        );
    }

    if !is_fill_state(inputs.flight_state) {
        // Idle/other non-fill states: keep controls available with no highlight.
        // Launch is kept disabled outside the armed state.
        // RetractPlumbing is one-way: once actuated, keep it disabled.
        let mut enabled: HashMap<&'static str, BlinkMode> = HashMap::new();
        for cmd in all_command_names() {
            if cmd == "RetractPlumbing" && inputs.valves.retract == Some(true) {
                continue;
            }
            enabled.insert(cmd, BlinkMode::None);
        }
        // In Idle, make the first fill-transition action the only illuminated action.
        // All other controls remain available (dimmed client-side when not blinking).
        if inputs.flight_state == FlightState::Idle && inputs.valves.normally_open != Some(true) {
            enabled.insert(
                "NormallyOpen",
                pending_mode(state, "NormallyOpen", inputs.now_ms, cfg),
            );
        }
        if inputs.flight_state == FlightState::Idle && inputs.valves.dump_open != Some(false) {
            enabled.insert("Dump", pending_mode(state, "Dump", inputs.now_ms, cfg));
        }
        let mut policy = policy_with_overrides(
            true,
            inputs.software_buttons_enabled,
            inputs.valves,
            enabled,
        );
        if inputs.flight_state == FlightState::Idle && !runtime.calibration_ready {
            for cmd in [
                "NormallyOpen",
                "Nitrogen",
                "Nitrous",
                "Pilot",
                "Igniter",
                "RetractPlumbing",
            ] {
                set_control_enabled(&mut policy, cmd, false);
            }
        }
        set_control_enabled(&mut policy, "Launch", false);
        return policy;
    }

    let mut recommended: HashMap<&'static str, BlinkMode> = HashMap::new();

    if sequence_expects_normally_open(runtime.step) && inputs.valves.normally_open != Some(true) {
        recommended.insert(
            "NormallyOpen",
            pending_mode(state, "NormallyOpen", inputs.now_ms, cfg),
        );
    }

    match runtime.step {
        SequenceStep::SetupValves => {
            if inputs.valves.normally_open != Some(true) {
                recommended.insert(
                    "NormallyOpen",
                    pending_mode(state, "NormallyOpen", inputs.now_ms, cfg),
                );
            }
            if inputs.valves.dump_open != Some(false) {
                recommended.insert("Dump", pending_mode(state, "Dump", inputs.now_ms, cfg));
            }
        }
        SequenceStep::NitrogenFill => {
            if inputs.valves.nitrogen_open != Some(true) {
                recommended.insert(
                    "Nitrogen",
                    pending_mode(state, "Nitrogen", inputs.now_ms, cfg),
                );
            }
        }
        SequenceStep::CloseNitrogen => {
            if inputs.valves.nitrogen_open != Some(false) {
                recommended.insert(
                    "Nitrogen",
                    pending_mode(state, "Nitrogen", inputs.now_ms, cfg),
                );
            }
        }
        SequenceStep::NitrogenLeakCheck => {}
        SequenceStep::RecoverNitrogenClose => {
            if inputs.valves.nitrogen_open != Some(false) {
                recommended.insert(
                    "Nitrogen",
                    pending_mode(state, "Nitrogen", inputs.now_ms, cfg),
                );
            } else if inputs.valves.dump_open != Some(true) {
                recommended.insert("Dump", pending_mode(state, "Dump", inputs.now_ms, cfg));
            }
        }
        SequenceStep::RecoverNitrogenVent => {
            if inputs.valves.dump_open != Some(true) {
                recommended.insert("Dump", pending_mode(state, "Dump", inputs.now_ms, cfg));
            }
        }
        SequenceStep::RecoverNitrogenCloseDump => {
            if inputs.valves.dump_open != Some(false) {
                recommended.insert("Dump", pending_mode(state, "Dump", inputs.now_ms, cfg));
            }
        }
        SequenceStep::DumpNitrogen => {
            if inputs.valves.dump_open != Some(true) {
                recommended.insert("Dump", pending_mode(state, "Dump", inputs.now_ms, cfg));
            }
        }
        SequenceStep::CloseDump => {
            if inputs.valves.dump_open != Some(false) {
                recommended.insert("Dump", pending_mode(state, "Dump", inputs.now_ms, cfg));
            }
        }
        SequenceStep::AwaitFillTestDecision => {
            if inputs.valves.nitrogen_open != Some(true) {
                recommended.insert(
                    "Nitrogen",
                    pending_mode(state, "Nitrogen", inputs.now_ms, cfg),
                );
            }
        }
        SequenceStep::OpenNitrous => {
            if inputs.valves.nitrous_open != Some(true) {
                recommended.insert(
                    "Nitrous",
                    pending_mode(state, "Nitrous", inputs.now_ms, cfg),
                );
            }
        }
        SequenceStep::CloseNitrous => {
            if inputs.valves.nitrous_open != Some(false) {
                recommended.insert(
                    "Nitrous",
                    pending_mode(state, "Nitrous", inputs.now_ms, cfg),
                );
            }
        }
        SequenceStep::NitrousSoak => {}
        SequenceStep::RecoverNitrousClose => {
            if inputs.valves.nitrous_open != Some(false) {
                recommended.insert(
                    "Nitrous",
                    pending_mode(state, "Nitrous", inputs.now_ms, cfg),
                );
            } else if inputs.valves.dump_open != Some(true) {
                recommended.insert("Dump", pending_mode(state, "Dump", inputs.now_ms, cfg));
            }
        }
        SequenceStep::RecoverNitrousVent => {
            if inputs.valves.dump_open != Some(true) {
                recommended.insert("Dump", pending_mode(state, "Dump", inputs.now_ms, cfg));
            }
        }
        SequenceStep::RecoverNitrousCloseDump => {
            if inputs.valves.dump_open != Some(false) {
                recommended.insert("Dump", pending_mode(state, "Dump", inputs.now_ms, cfg));
            }
        }
        SequenceStep::CloseNormallyOpen => {
            if inputs.valves.normally_open != Some(false) {
                recommended.insert(
                    "NormallyOpen",
                    pending_mode(state, "NormallyOpen", inputs.now_ms, cfg),
                );
            }
        }
        SequenceStep::RetractFillLines => {
            if inputs.valves.retract != Some(true) {
                recommended.insert(
                    "RetractPlumbing",
                    pending_mode(state, "RetractPlumbing", inputs.now_ms, cfg),
                );
            }
        }
        SequenceStep::ArmedReady => {
            recommended.insert("Launch", BlinkMode::Slow);
        }
    }

    let mut policy = policy_with_overrides(
        true,
        inputs.software_buttons_enabled,
        inputs.valves,
        recommended,
    );
    set_control_enabled(&mut policy, "Launch", false);
    policy
}

fn read_key_enabled(state: &AppState, cfg: &SequenceConfig) -> bool {
    if crate::flight_sim::sim_mode_enabled() {
        return true;
    }
    if cfg!(feature = "testing") {
        return true;
    }
    if cfg!(feature = "hitl_mode") || cfg!(feature = "test_fire_mode") {
        return true;
    }
    if !cfg.key_required {
        return true;
    }
    state
        .gpio
        .read_input_pin(cfg.key_enable_pin)
        .unwrap_or(false)
}

fn read_software_buttons_enabled(state: &AppState, cfg: &SequenceConfig) -> bool {
    if crate::flight_sim::sim_mode_enabled() {
        return true;
    }
    if cfg!(feature = "testing") {
        return true;
    }
    if cfg!(feature = "hitl_mode") || cfg!(feature = "test_fire_mode") {
        return true;
    }
    state
        .gpio
        .read_input_pin(cfg.software_disable_pin)
        .map(|is_high| !is_high)
        .unwrap_or(true)
}

pub fn start_sequence_task(
    state: Arc<AppState>,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    let cfg = SequenceConfig::from_env();
    if cfg!(feature = "hitl_mode") {
        return tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(200));
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        let valves = ValveSnapshot::read(&state);
                        state.set_action_policy(hitl_action_policy(valves));
                    }
                    recv = shutdown_rx.recv() => {
                        match recv {
                            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) | Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }
        });
    }

    if cfg.key_required
        && !cfg!(feature = "hitl_mode")
        && !cfg!(feature = "test_fire_mode")
        && let Err(err) = state.gpio.setup_input_pin(cfg.key_enable_pin)
    {
        eprintln!(
            "Sequence key GPIO setup failed (pin {}): {}",
            cfg.key_enable_pin, err
        );
    }

    if !cfg!(feature = "testing")
        && !cfg!(feature = "hitl_mode")
        && !cfg!(feature = "test_fire_mode")
        && let Err(err) = state.gpio.setup_input_pin(cfg.software_disable_pin)
    {
        eprintln!(
            "Software disable GPIO setup failed (pin {}): {}",
            cfg.software_disable_pin, err
        );
    }

    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(200));
        let mut runtime = SequenceRuntime::default();

        loop {
            tokio::select! {
                _ = tick.tick() => {}
                recv = shutdown_rx.recv() => {
                    match recv {
                        Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) | Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }

            let mut flight_state = *state.state.lock().unwrap();
            let valves = ValveSnapshot::read(&state);
            let pressure_psi = *state.latest_fuel_tank_pressure.lock().unwrap();
            let current_mass_kg = *state.latest_fill_mass_kg.lock().unwrap();
            let now = Instant::now();
            let now_ms = crate::telemetry_task::get_current_timestamp_ms();
            let key_enabled = read_key_enabled(&state, &cfg);
            let software_buttons_enabled = read_software_buttons_enabled(&state, &cfg);

            update_sequence_runtime(
                &state,
                &mut runtime,
                &cfg,
                valves,
                pressure_psi,
                current_mass_kg,
                now,
            );
            flight_state =
                maybe_drive_local_prelaunch_state(&state, &runtime, valves, flight_state);
            let policy = build_policy(
                &state,
                &cfg,
                &runtime,
                PolicyInputs {
                    flight_state,
                    key_enabled,
                    software_buttons_enabled,
                    valves,
                    now_ms,
                },
            );
            state.set_action_policy(policy);
        }
    })
}
