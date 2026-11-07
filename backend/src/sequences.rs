use crate::rocket_commands::{ActuatorBoardCommands, ValveBoardCommands};
use crate::state::{AppState, UMBILICAL_PENDING_VALVE_TIMEOUT};
use crate::types::{FlightState, TelemetryCommand};
use crate::web::emit_warning;
use crate::{fill_targets, loadcell};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    pub cmd: String,
    pub enabled: bool,
    pub blink: BlinkMode,
    pub actuated: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActionPolicyMsg {
    #[serde(default)]
    pub local_phase_controls: bool,
    #[serde(default, skip_serializing_if="Option::is_none")]
    pub local_phase_controls_reason: Option<String>,
    pub key_enabled: bool,
    pub software_buttons_enabled: bool,
    pub controls: Vec<ActionControl>,
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
    BlinkMode::None
}

/// Publish explanations from the same server policy that denied the command.
/// Presentation metadata never changes admission or enables a control.
pub(crate) fn explain_disabled_policy(policy: &mut ActionPolicyMsg) {
    for control in &mut policy.controls {
        if control.enabled { control.disabled_reason = None; continue; }
        if control.disabled_reason.is_some() { continue; }
        control.disabled_reason = Some(if matches!(control.cmd.as_str(), "AdvanceFlightState" | "RewindFlightState")
            && policy.local_phase_controls_reason.is_some() {
            policy.local_phase_controls_reason.clone().unwrap()
        } else if !policy.key_enabled { "GroundStation key control is disabled.".into() }
        else if !policy.software_buttons_enabled { "Software controls are disabled by GroundStation.".into() }
        else { "This command is unavailable under the current GroundStation policy.".into() });
    }
}
fn is_recording_command(cmd: &str) -> bool {
    matches!(
        cmd,
        "StartWritingNow" | "StartWritingLastTwoMinutes" | "PauseWritingDb" | "StopWritingDb"
    )
}

pub(crate) fn is_station_utility_command(cmd: &str) -> bool {
    matches!(
        cmd,
        "ToggleAvBayUnderglow" | "ToggleFlightBuzzer" | "ResetTClock"
    )
}

#[cfg_attr(feature = "hitl_mode", allow(dead_code))]
fn default_recording_command_enabled(cmd: &str) -> Option<bool> {
    match cmd {
        "StartWritingNow" | "StartWritingLastTwoMinutes" => Some(true),
        "PauseWritingDb" | "StopWritingDb" => Some(false),
        _ => None,
    }
}

#[cfg_attr(feature = "hitl_mode", allow(dead_code))]
fn default_recording_command_actuated(cmd: &str) -> Option<bool> {
    match cmd {
        "StartWritingNow" | "StartWritingLastTwoMinutes" => Some(true),
        "PauseWritingDb" | "StopWritingDb" => Some(false),
        _ => None,
    }
}

fn default_command_actuated(cmd: &str) -> Option<bool> {
    match cmd {
        "ResetSim" => Some(true),
        "ToggleAvBayUnderglow" => Some(crate::network_variables::underglow_enabled()),
        "ToggleFlightBuzzer" => Some(crate::network_variables::flight_buzzer_enabled()),
        _ => default_recording_command_actuated(cmd),
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
pub(crate) enum SequenceStep {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SequencePolicyState {
    pub generation: u64,
    pub step: SequenceStep,
    pub calibration_ready: bool,
}

impl Default for SequencePolicyState {
    fn default() -> Self {
        Self {
            generation: 0,
            step: SequenceStep::SetupValves,
            calibration_ready: false,
        }
    }
}

#[derive(Clone, Debug)]
struct SequenceConfig {
    leak_check_duration: Duration,
    nitrous_soak_duration: Duration,
    nitrous_level_duration: Duration,
    manual_close_target_tolerance_kg: f32,
    nitrogen_pressure_target_psi: f32,
    nitrogen_target_mass_kg: Option<f32>,
    nitrogen_autoclose_mode: NitrogenAutocloseMode,
    nitrous_pressure_min_psi: f32,
    nitrous_rise_epsilon_psi: f32,
    dump_pressure_max_psi: f32,
    max_leak_drop_psi: f32,
    max_leak_mass_delta_kg: f32,
    allowed_hold_drop_psi_per_min: f32,
    allowed_hold_mass_drop_kg_per_min: f32,
    nitrous_weight_rise_epsilon_kg: f32,
    empty_mass_noise_allowance_kg: f32,
    calibration_pressure_min_psi: f32,
    calibration_pressure_max_psi: f32,
    calibration_mass_min_kg: f32,
    calibration_mass_max_kg: f32,
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
            .unwrap_or_else(|| Duration::from_secs(30));

        let pressure_min_psi = std::env::var("GS_SEQUENCE_PRESSURE_MIN_PSI")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(10.0);

        let manual_close_target_tolerance_kg =
            std::env::var("GS_SEQUENCE_MANUAL_CLOSE_TARGET_TOLERANCE_KG")
                .ok()
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(5.0)
                .max(0.0);

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
            .filter(|v| v.abs() >= 0.01)
            .or(Some(fill_cfg.nitrogen.target_mass_kg));

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
            manual_close_target_tolerance_kg,
            nitrogen_pressure_target_psi,
            nitrogen_target_mass_kg,
            nitrogen_autoclose_mode,
            nitrous_pressure_min_psi,
            nitrous_rise_epsilon_psi,
            dump_pressure_max_psi,
            max_leak_drop_psi,
            max_leak_mass_delta_kg,
            allowed_hold_drop_psi_per_min,
            allowed_hold_mass_drop_kg_per_min,
            nitrous_weight_rise_epsilon_kg,
            empty_mass_noise_allowance_kg,
            calibration_pressure_min_psi,
            calibration_pressure_max_psi,
            calibration_mass_min_kg,
            calibration_mass_max_kg,
            key_required,
            key_enable_pin,
            software_disable_pin,
        }
    }
}

#[derive(Clone, Debug)]
struct SequenceRuntime {
    generation: u64,
    step: SequenceStep,
    next_step_after_dump: Option<SequenceStep>,
    step_started_at: Option<Instant>,
    pressure_at_close_psi: Option<f32>,
    mass_at_close_kg: Option<f32>,
    notified_leak_pass: bool,
    notified_armed: bool,
    warned_rapid_drop: bool,
    warned_mass_shift: bool,
    rapid_drop_notification_id: Option<u64>,
    mass_shift_notification_id: Option<u64>,
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
            generation: 0,
            step: SequenceStep::SetupValves,
            next_step_after_dump: None,
            step_started_at: None,
            pressure_at_close_psi: None,
            mass_at_close_kg: None,
            notified_leak_pass: false,
            notified_armed: false,
            warned_rapid_drop: false,
            warned_mass_shift: false,
            rapid_drop_notification_id: None,
            mass_shift_notification_id: None,
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

impl SequenceRuntime {
    fn for_generation(generation: u64) -> Self {
        Self {
            generation,
            ..Self::default()
        }
    }

    fn bind_generation(&mut self, generation: u64) -> bool {
        if self.generation == generation {
            return false;
        }
        *self = Self::for_generation(generation);
        true
    }

    fn policy_state(&self) -> SequencePolicyState {
        SequencePolicyState {
            generation: self.generation,
            step: self.step,
            calibration_ready: self.calibration_ready,
        }
    }

    fn from_policy_state(policy: SequencePolicyState) -> Self {
        Self {
            generation: policy.generation,
            step: policy.step,
            calibration_ready: policy.calibration_ready,
            ..Self::default()
        }
    }
}

/// Own all state publication from a legacy tick, not only its queued closes.
/// Recovery quiesces this existing owner count before changing the epoch.
fn begin_sequence_tick(
    state: &AppState,
    runtime: &mut SequenceRuntime,
    last_flight_state: &mut Option<FlightState>,
) -> Option<crate::automatic_flight_state::Observation> {
    let claim = crate::automatic_flight_state::Observation::begin(state)?;
    let generation = claim.generation();
    if runtime.bind_generation(generation) {
        *last_flight_state = None;
    }
    Some(claim)
}

#[cfg(test)]
mod generation_lifecycle_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn gse_owned_policy_refresh_never_reenters_dispatch() {
        let _owned_entry: fn(&Arc<AppState>) = refresh_action_policy_under_dispatch;
        if std::env::var("GS_SEQUENCE_GATE_CHILD").as_deref() == Ok("1") {
            let state = crate::state::tests::test_app_state().await;
            assert!(state.topology_router.get().is_none());
            assert!(crate::gse::handle_command(
                &state,
                &TelemetryCommand::CancelFill
            ));
            assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 0);
            return;
        }
        // A regression must fail without leaving the parent suite's global
        // mutex locked. This child runs the actual serialized GSE command.
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "sequences::generation_lifecycle_tests::gse_owned_policy_refresh_never_reenters_dispatch", "--nocapture", "--test-threads=1"])
            .env("GS_SEQUENCE_GATE_CHILD", "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let output = child.wait_with_output().unwrap();
                panic!(
                    "GSE refresh re-entered the dispatch gate: {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn fill_decision(
        state: &AppState,
        generation: u64,
        branch: usize,
    ) -> (
        SequenceRuntime,
        SequenceConfig,
        ValveSnapshot,
        Option<f32>,
        Option<f32>,
    ) {
        let mut cfg = SequenceConfig::from_env();
        cfg.nitrogen_autoclose_mode = NitrogenAutocloseMode::Pressure;
        cfg.nitrogen_pressure_target_psi = 100.0;
        cfg.nitrous_level_duration = Duration::ZERO;
        let mut runtime = SequenceRuntime::for_generation(generation);
        runtime.step = if branch == 0 {
            SequenceStep::NitrogenFill
        } else {
            SequenceStep::OpenNitrous
        };
        let valves = ValveSnapshot {
            normally_open: Some(branch != 0),
            dump_open: Some(false),
            nitrogen_open: Some(branch == 0),
            nitrous_open: Some(branch != 0),
            ..ValveSnapshot::default()
        };
        let mass = if branch == 1 {
            let (target, _) = nitrous_fill_status(state, 0.0);
            Some(target + if target >= 0.0 { 1.0 } else { -1.0 })
        } else {
            None
        };
        let pressure = if branch == 0 {
            101.0
        } else {
            cfg.nitrous_pressure_min_psi + 1.0
        };
        (runtime, cfg, valves, Some(pressure), mass)
    }

    async fn assert_old_decision_rejected(branch: usize) {
        crate::telemetry_schema::initialize().unwrap();
        let mut state = crate::state::tests::test_app_state().await;
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        Arc::get_mut(&mut state).unwrap().cmd_tx = tx;
        state.set_local_flight_state(FlightState::NitrogenFill);
        let before = crate::gse::automatic_generation(&state);
        // These decisions originated before the reset; no outputs or transport
        // are present in this fixture. Advance the actual owner generation.
        state
            .automatic_generation
            .store(before + 1, Ordering::Release);
        let (mut runtime, cfg, valves, pressure, mass) = fill_decision(&state, before, branch);
        update_sequence_runtime(
            &state,
            &mut runtime,
            &cfg,
            valves,
            pressure,
            mass,
            Instant::now(),
        );
        let queued = rx.try_recv().expect("actual automatic close was queued");
        let origin = queued
            .automatic_generation()
            .expect("automatic origin retained");
        assert!(
            crate::telemetry_task::command_at_dispatch(&state, queued).is_none(),
            "pre-reset branch {branch} acquired the post-reset dispatch generation"
        );
        assert_eq!(
            origin, before,
            "no new origin may be minted from old observations"
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn old_nitrogen_pressure_decision_cannot_acquire_new_epoch() {
        assert_old_decision_rejected(0).await;
    }
    #[tokio::test]
    async fn old_nitrous_weight_decision_cannot_acquire_new_epoch() {
        assert_old_decision_rejected(1).await;
    }
    #[tokio::test]
    async fn old_nitrous_plateau_decision_cannot_acquire_new_epoch() {
        assert_old_decision_rejected(2).await;
    }

    #[tokio::test]
    async fn fresh_fill_decisions_remain_dispatchable_in_all_close_branches() {
        crate::telemetry_schema::initialize().unwrap();
        let mut state = crate::state::tests::test_app_state().await;
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        Arc::get_mut(&mut state).unwrap().cmd_tx = tx;
        state.set_local_flight_state(FlightState::NitrogenFill);
        let generation = crate::gse::automatic_generation(&state);
        for branch in 0..3 {
            let (mut runtime, cfg, valves, pressure, mass) =
                fill_decision(&state, generation, branch);
            update_sequence_runtime(
                &state,
                &mut runtime,
                &cfg,
                valves,
                pressure,
                mass,
                Instant::now(),
            );
            let queued = rx.try_recv().expect("fresh automatic close was queued");
            assert_eq!(queued.automatic_generation(), Some(generation));
            let expected = if branch == 0 {
                TelemetryCommand::NitrogenClose
            } else {
                TelemetryCommand::NitrousClose
            };
            let actual = crate::telemetry_task::command_at_dispatch(&state, queued)
                .expect("fresh decision allowed");
            assert_eq!(
                std::mem::discriminant(&actual),
                std::mem::discriminant(&expected)
            );
            assert!(rx.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn closed_admission_blocks_actual_sequence_tick_without_state_publication() {
        let state = crate::state::tests::test_app_state().await;
        let generation = crate::gse::automatic_generation(&state);
        let mut runtime = SequenceRuntime::for_generation(generation);
        runtime.step = SequenceStep::NitrogenLeakCheck;
        let mut previous = Some(FlightState::NitrogenFill);
        let before = state.local_flight_state_snapshot();
        crate::gse::reset_generation::close_operator_admission(&state, generation, 83).unwrap();
        assert!(begin_sequence_tick(&state, &mut runtime, &mut previous).is_none());
        assert_eq!(runtime.generation, generation);
        assert!(matches!(runtime.step, SequenceStep::NitrogenLeakCheck));
        assert_eq!(previous, Some(FlightState::NitrogenFill));
        assert_eq!(state.local_flight_state_snapshot(), before);
        assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 0);
        state.manual_abort_intent.request();
        assert!(state.manual_abort_intent.pending_epoch().is_some());
        // Model only the completed journal/epoch/reopen boundary here. Actual
        // durable-prefix ownership is covered by the clock-worker regressions.
        state
            .automatic_generation
            .store(generation + 1, Ordering::Release);
        assert!(begin_sequence_tick(&state, &mut runtime, &mut previous).is_none());
        state.operator_admission_barrier.store(0, Ordering::Release);
        let claim = begin_sequence_tick(&state, &mut runtime, &mut previous).unwrap();
        assert_eq!(runtime.generation, generation + 1);
        assert!(matches!(runtime.step, SequenceStep::SetupValves));
        assert!(previous.is_none());
        assert!(state.manual_abort_intent.pending_epoch().is_some());
        drop(claim);
    }

    #[tokio::test]
    async fn sequence_tick_claim_quiesces_before_verified_epoch_change() {
        let state = crate::state::tests::test_app_state().await;
        let generation = crate::gse::automatic_generation(&state);
        let mut runtime = SequenceRuntime::for_generation(generation);
        runtime.step = SequenceStep::NitrogenLeakCheck;
        let mut previous = Some(FlightState::NitrogenFill);
        let claim = begin_sequence_tick(&state, &mut runtime, &mut previous).unwrap();
        assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 1);
        assert!(
            crate::gse::reset_generation::invalidate_capture_epoch(&state, generation).is_err()
        );
        drop(claim);
        let next =
            crate::gse::reset_generation::invalidate_capture_epoch(&state, generation).unwrap();
        let claim = begin_sequence_tick(&state, &mut runtime, &mut previous).unwrap();
        assert_eq!(runtime.generation, next);
        assert!(matches!(runtime.step, SequenceStep::SetupValves));
        assert!(previous.is_none());
        drop(claim);
        assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 0);
    }

    #[cfg(not(any(feature = "hitl_mode", feature = "test_fire_mode")))]
    #[tokio::test]
    async fn policy_refresh_cannot_republish_pre_reset_armed_stage() {
        if crate::operator_queue::isolated_case(module_path!(), "policy_refresh_cannot_republish_pre_reset_armed_stage") { return; }
        // This positive publication uses the same initialized managed Store,
        // public SDK and network clock as the actual host, not an empty state.
        let (state, router) = crate::automatic_flight_state::tests::ready().await;
        let generation = crate::gse::automatic_generation(&state);
        let mut old = SequenceRuntime::for_generation(generation);
        old.step = SequenceStep::ArmedReady;
        state.set_sequence_policy_state(old.policy_state());
        state
            .automatic_generation
            .store(generation + 1, Ordering::Release);
        state.set_local_flight_state(FlightState::Idle);
        assert_eq!(crate::network_variables::flight_state(), FlightState::Idle as u8);
        let before_automatic = crate::network_variables::automatic_idle_test_snapshot();
        refresh_action_policy_now(&state);
        assert_eq!(
            state.local_flight_state_snapshot(),
            FlightState::Idle,
            "old policy must not republish Armed after recovery"
        );
        let snapshot = state.sequence_policy_state_snapshot();
        assert_eq!(snapshot.generation, generation + 1);
        assert!(matches!(snapshot.step, SequenceStep::SetupValves));
        // Current-generation readiness still drives the existing state path.
        let mut fresh = SequenceRuntime::for_generation(generation + 1);
        fresh.step = SequenceStep::ArmedReady;
        state.set_sequence_policy_state(fresh.policy_state());
        refresh_action_policy_now(&state);
        assert_eq!(state.local_flight_state_snapshot(), FlightState::Armed);
        let packet = router.get_cached_network_variable(
            crate::telemetry_schema::data_type("FLIGHT_STATE")
        ).unwrap().expect("fresh phase admitted by actual managed SDK");
        assert_eq!(packet.payload(), &[FlightState::Armed as u8]);
        assert_eq!(crate::network_variables::flight_state(), FlightState::Idle as u8);
        assert!(crate::network_variables::automatic_idle_test_snapshot()
            .durable_unchanged(&before_automatic));
    }

    #[cfg(not(any(feature = "hitl_mode", feature = "test_fire_mode")))]
    #[tokio::test]
    async fn policy_refresh_without_initialized_transport_cannot_publish_armed_stage() {
        if crate::operator_queue::isolated_case(module_path!(), "policy_refresh_without_initialized_transport_cannot_publish_armed_stage") { return; }
        let state = crate::state::tests::test_app_state().await;
        state.set_local_flight_state(FlightState::Idle);
        let mut fresh = SequenceRuntime::for_generation(crate::gse::automatic_generation(&state));
        fresh.step = SequenceStep::ArmedReady;
        state.set_sequence_policy_state(fresh.policy_state());
        assert!(state.topology_router.get().is_none());
        refresh_action_policy_now(&state);
        assert_eq!(state.local_flight_state_snapshot(), FlightState::Idle);
    }

    #[test]
    fn new_generation_discards_previous_sequence_observations_and_timers() {
        let mut runtime = SequenceRuntime::for_generation(7);
        runtime.step = SequenceStep::NitrogenLeakCheck;
        runtime.step_started_at = Some(Instant::now());
        runtime.pressure_at_close_psi = Some(120.0);
        runtime.mass_at_close_kg = Some(10.0);
        runtime.nitrous_level_since = Some(Instant::now());
        runtime.calibration_ready = true;
        runtime.auto_close_nitrogen_sent = true;
        runtime.auto_close_nitrous_sent = true;
        assert!(!runtime.bind_generation(7));
        assert!(matches!(runtime.step, SequenceStep::NitrogenLeakCheck));
        assert!(runtime.bind_generation(8));
        assert_eq!(runtime.generation, 8);
        assert!(matches!(runtime.step, SequenceStep::SetupValves));
        assert!(runtime.step_started_at.is_none() && runtime.pressure_at_close_psi.is_none());
        assert!(runtime.mass_at_close_kg.is_none() && runtime.nitrous_level_since.is_none());
        assert!(
            !runtime.calibration_ready
                && !runtime.auto_close_nitrogen_sent
                && !runtime.auto_close_nitrous_sent
        );
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct ValveSnapshot {
    normally_open: Option<bool>,
    pending_normally_open: Option<bool>,
    dump_open: Option<bool>,
    pending_dump_open: Option<bool>,
    nitrogen_open: Option<bool>,
    pending_nitrogen_open: Option<bool>,
    nitrous_open: Option<bool>,
    pending_nitrous_open: Option<bool>,
    pilot_open: Option<bool>,
    pending_pilot_open: Option<bool>,
    igniter_on: Option<bool>,
    pending_igniter_on: Option<bool>,
    retract: Option<bool>,
    pending_retract: Option<bool>,
}


impl ValveSnapshot {
    fn read(state: &AppState) -> Self {
        let valve = |cmd| state.get_umbilical_valve_state(cmd);
        let pending = |cmd| state.get_pending_umbilical_valve_state(cmd);
        Self {
            pilot_open: valve(ValveBoardCommands::PilotOpen as u8),
            pending_pilot_open: pending(ValveBoardCommands::PilotOpen as u8),
            normally_open: valve(ValveBoardCommands::NormallyOpenOpen as u8),
            pending_normally_open: pending(ValveBoardCommands::NormallyOpenOpen as u8),
            dump_open: valve(ValveBoardCommands::DumpOpen as u8),
            pending_dump_open: pending(ValveBoardCommands::DumpOpen as u8),
            igniter_on: valve(ActuatorBoardCommands::IgniterOn as u8),
            pending_igniter_on: pending(ActuatorBoardCommands::IgniterOn as u8),
            nitrogen_open: valve(ActuatorBoardCommands::NitrogenOpen as u8),
            pending_nitrogen_open: pending(ActuatorBoardCommands::NitrogenOpen as u8),
            nitrous_open: valve(ActuatorBoardCommands::NitrousOpen as u8),
            pending_nitrous_open: pending(ActuatorBoardCommands::NitrousOpen as u8),
            retract: valve(ActuatorBoardCommands::RetractPlumbing as u8),
            pending_retract: pending(ActuatorBoardCommands::RetractPlumbing as u8),
        }
    }

    fn actuated_for_cmd(&self, cmd: &str) -> Option<bool> {
        let (confirmed, pending) = match cmd {
            "Dump" => (self.dump_open, self.pending_dump_open),
            "NormallyOpen" => (self.normally_open, self.pending_normally_open),
            "Nitrogen" => (self.nitrogen_open, self.pending_nitrogen_open),
            "Nitrous" => (self.nitrous_open, self.pending_nitrous_open),
            "Pilot" => (self.pilot_open, self.pending_pilot_open),
            "Igniter" => (self.igniter_on, self.pending_igniter_on),
            "RetractPlumbing" => (self.retract, self.pending_retract),
            _ => return None,
        };
        if pending.is_some() { None } else { confirmed }
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
        TelemetryCommand::OperatorPhase{intent} => intent.name(),
        TelemetryCommand::ColdFlowPanelRelease { .. } => "ColdFlowPanelRelease",
        TelemetryCommand::QueuedOperator { command, .. } => command_name(command),
        TelemetryCommand::Postinit => "Postinit",
        TelemetryCommand::Launch => "Launch",
        TelemetryCommand::Dump => "Dump",
        TelemetryCommand::Abort => "Abort",
        TelemetryCommand::AutomaticAbort { .. } => "AutomaticAbort",
        TelemetryCommand::AutomaticNitrogenClose { .. } => "AutomaticNitrogenClose",
        TelemetryCommand::AutomaticNitrousClose { .. } => "AutomaticNitrousClose",
        TelemetryCommand::NormallyOpen => "NormallyOpen",
        TelemetryCommand::Pilot => "Pilot",
        TelemetryCommand::Igniter => "Igniter",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::IgniterSequence => "IgniterSequence",
        TelemetryCommand::RetractPlumbing => "RetractPlumbing",
        TelemetryCommand::Nitrogen | TelemetryCommand::NitrogenClose => "Nitrogen",
        TelemetryCommand::Nitrous | TelemetryCommand::NitrousClose => "Nitrous",
        TelemetryCommand::StartWritingNow => "StartWritingNow",
        TelemetryCommand::StartWritingLastTwoMinutes => "StartWritingLastTwoMinutes",
        TelemetryCommand::PauseWritingDb => "PauseWritingDb",
        TelemetryCommand::StopWritingDb => "StopWritingDb",
        TelemetryCommand::ResetSim => "ResetSim",
        TelemetryCommand::ContinueFillSequence => "ContinueFillSequence",
        TelemetryCommand::StartFill | TelemetryCommand::FillWithRecording { .. } => "StartFill",
        TelemetryCommand::PauseFill => "PauseFill",
        TelemetryCommand::CancelFill => "CancelFill",
        TelemetryCommand::ValveSelfTest => "ValveSelfTest",
        TelemetryCommand::NitrogenTest => "NitrogenTest",
        TelemetryCommand::Turbocharge => "Turbocharge",
        TelemetryCommand::MonitorAltitude => "MonitorAltitude",
        TelemetryCommand::RevokeMonitorAltitude => "RevokeMonitorAltitude",
        TelemetryCommand::ConsecutiveSamples => "ConsecutiveSamples",
        TelemetryCommand::RevokeConsecutiveSamples => "RevokeConsecutiveSamples",
        TelemetryCommand::ResetFailures => "ResetFailures",
        TelemetryCommand::RevokeResetFailures => "RevokeResetFailures",
        TelemetryCommand::ValidateMeasms => "ValidateMeasms",
        TelemetryCommand::RevokeValidateMeasms => "RevokeValidateMeasms",
        #[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
        TelemetryCommand::GroundStationLaunch => "GroundStationLaunch",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::ToggleButtonInterlock => "ToggleButtonInterlock",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::ToggleGroundStationControl => "ToggleGroundStationControl",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::ToggleLaunchInterlock => "ToggleLaunchInterlock",
        #[cfg(feature = "hitl_mode")]
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::TogglePlumbingDelayInterlock => "TogglePlumbingDelayInterlock",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::ToggleChecklistInterlock => "ToggleChecklistInterlock",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::ToggleSupplyInterlock => "ToggleSupplyInterlock",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::ToggleBoardStateProofInterlock => "ToggleBoardStateProofInterlock",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::TogglePhysicalLaunchMode => "TogglePhysicalLaunchMode",
        TelemetryCommand::ToggleAvBayUnderglow => "ToggleAvBayUnderglow",
        TelemetryCommand::ToggleFlightBuzzer => "ToggleFlightBuzzer",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::ResetLaunchLatch => "ResetLaunchLatch",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::ResetFlightState => "ResetFlightState",
        TelemetryCommand::ResetTClock => "ResetTClock",
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
        TelemetryCommand::EnableIMU => "EnableIMU",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::DisableIMU => "DisableIMU",
        #[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
        TelemetryCommand::AdvanceFlightState => "AdvanceFlightState",
        #[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
        TelemetryCommand::RewindFlightState => "RewindFlightState",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::AbortAfter100 => "AbortAfter100",
        #[cfg(feature = "hitl_mode")]
        TelemetryCommand::AbortAfter40 => "AbortAfter40",
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
        "ToggleAvBayUnderglow",
        "ToggleFlightBuzzer",
        "ResetTClock",
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
        "IgniterSequence",
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
        "MonitorAltitude",
        "RevokeMonitorAltitude",
        "ConsecutiveSamples",
        "RevokeConsecutiveSamples",
        "ResetFailures",
        "RevokeResetFailures",
        "ValidateMeasms",
        "RevokeValidateMeasms",
        "GroundStationLaunch",
        "ToggleButtonInterlock",
        "ToggleGroundStationControl",
        "ToggleLaunchInterlock",
        "TogglePlumbingDelayInterlock",
        "ToggleChecklistInterlock",
        "ToggleSupplyInterlock",
        "ToggleBoardStateProofInterlock",
        "TogglePhysicalLaunchMode",
        "ToggleAvBayUnderglow",
        "ToggleFlightBuzzer",
        "ResetLaunchLatch",
        "ResetFlightState",
        "ResetTClock",
        "DeployParachute",
        "ExpandParachute",
        "EvaluationRelax",
        "EvaluationFocus",
        "EvaluationAbort",
        "ReinitSensors",
        "ReinitBarometer",
        "EnableIMU",
        "DisableIMU",
        "AdvanceFlightState",
        "RewindFlightState",
        "AbortAfter100",
        "AbortAfter40",
        "AbortAfter250",
        "ReinitAfter15",
        "ReinitAfter30",
        "ReinitAfter50",
    ]
}

#[cfg(all(not(feature = "hitl_mode"), feature = "test_fire_mode"))]
pub fn all_command_names() -> Vec<&'static str> {
    vec![
        "ToggleAvBayUnderglow",
        "ToggleFlightBuzzer",
        "ResetTClock",
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
        "GroundStationLaunch",
        "MonitorAltitude",
        "RevokeMonitorAltitude",
        "ConsecutiveSamples",
        "RevokeConsecutiveSamples",
        "ResetFailures",
        "RevokeResetFailures",
        "ValidateMeasms",
        "RevokeValidateMeasms",
        "AdvanceFlightState",
        "RewindFlightState",
    ]
}

/// Controls owned by the Valve master while its actual launch scheduler runs.
/// A retained flight phase/recovery record is not evidence of that ownership.
pub(crate) fn is_fill_system_control(name: &str) -> bool {
    matches!(name,
        "Dump" | "NormallyOpen" | "Pilot" | "Igniter" | "IgniterSequence"
        | "RetractPlumbing" | "Nitrogen" | "NitrogenClose" | "Nitrous" | "NitrousClose"
        | "StartFill" | "ContinueFillSequence" | "NitrogenTest" | "ValveSelfTest"
        | "Turbocharge")
}

pub fn default_action_policy() -> ActionPolicyMsg {
    #[cfg(feature = "hitl_mode")]
    {
        hitl_action_policy(ValveSnapshot {
            normally_open: None,
            dump_open: None,
            nitrogen_open: None,
            nitrous_open: None,
            pilot_open: None,
            igniter_on: None,
            retract: None,
            ..ValveSnapshot::default()
        })
    }
    #[cfg(not(feature = "hitl_mode"))]
    {
        let controls = all_command_names()
            .into_iter()
            .map(|cmd| {
                let enabled = is_station_utility_command(cmd)
                    || default_recording_command_enabled(cmd)
                        .unwrap_or(matches!(cmd, "Abort" | "Postinit" | "ResetSim"));
                ActionControl { disabled_reason: None,
                    cmd: cmd.to_string(),
                    enabled,
                    blink: backend_blink_for(cmd, enabled, None),
                    actuated: default_command_actuated(cmd),
                }
            })
            .collect();
        ActionPolicyMsg {
            local_phase_controls: false,
            local_phase_controls_reason: None,
            key_enabled: true,
            software_buttons_enabled: true,
            controls,
        }
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
                    | "Postinit"
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
            ActionControl { disabled_reason: None,
                cmd: cmd.to_string(),
                enabled,
                blink: backend_blink_for(cmd, enabled, recommended.get(cmd)),
                actuated: default_command_actuated(cmd).or_else(|| valves.actuated_for_cmd(cmd)),
            }
        })
        .collect();

    ActionPolicyMsg {
        local_phase_controls: false,
        local_phase_controls_reason: None,
        key_enabled,
        software_buttons_enabled,
        controls,
    }
}

fn command_prompt_blink(
    _state: &AppState,
    _cfg: &SequenceConfig,
    valves: ValveSnapshot,
    cmd: &'static str,
    desired: bool,
    _now_ms: u64,
) -> Option<BlinkMode> {
    if valves.actuated_for_cmd(cmd) == Some(desired) {
        return None;
    }
    Some(BlinkMode::Slow)
}

fn vent_closed_for_launch(valves: ValveSnapshot) -> bool {
    valves.actuated_for_cmd("NormallyOpen") == Some(false)
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

fn enable_launch_when_key_pressed(policy: &mut ActionPolicyMsg, inputs: PolicyInputs) {
    if inputs.key_enabled {
        set_control_enabled(policy, "Launch", true);
        set_control_enabled(policy, "GroundStationLaunch", true);
    }
}

fn sequence_expects_normally_open(step: SequenceStep) -> bool {
    matches!(
        step,
        SequenceStep::DumpNitrogen
            | SequenceStep::CloseDump
            | SequenceStep::AwaitFillTestDecision
            | SequenceStep::RecoverNitrogenClose
            | SequenceStep::RecoverNitrogenVent
            | SequenceStep::RecoverNitrogenCloseDump
            | SequenceStep::OpenNitrous
            | SequenceStep::CloseNitrous
            | SequenceStep::RecoverNitrousClose
            | SequenceStep::RecoverNitrousVent
            | SequenceStep::RecoverNitrousCloseDump
    )
}

fn sequence_expects_normally_closed(step: SequenceStep) -> bool {
    if cfg!(feature = "test_fire_mode") {
        return false;
    }
    matches!(
        step,
        SequenceStep::SetupValves
            | SequenceStep::NitrogenFill
            | SequenceStep::CloseNitrogen
            | SequenceStep::NitrogenLeakCheck
            | SequenceStep::NitrousSoak
            | SequenceStep::CloseNormallyOpen
    )
}

fn sequence_blocks_until_normally_open(step: SequenceStep) -> bool {
    matches!(
        step,
        SequenceStep::DumpNitrogen
            | SequenceStep::CloseDump
            | SequenceStep::AwaitFillTestDecision
            | SequenceStep::OpenNitrous
            | SequenceStep::CloseNitrous
    )
}

fn sequence_blocks_until_normally_closed(step: SequenceStep) -> bool {
    if cfg!(feature = "test_fire_mode") {
        return false;
    }
    matches!(
        step,
        SequenceStep::SetupValves
            | SequenceStep::NitrogenFill
            | SequenceStep::CloseNitrogen
            | SequenceStep::NitrogenLeakCheck
            | SequenceStep::NitrousSoak
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

fn first_fill_step_after_setup() -> SequenceStep {
    if cfg!(feature = "test_fire_mode") {
        SequenceStep::OpenNitrous
    } else {
        SequenceStep::NitrogenFill
    }
}

fn step_after_nitrous_valve_closed() -> SequenceStep {
    SequenceStep::CloseNormallyOpen
}

fn nitrous_close_accepted_for_vent_prompt(valves: ValveSnapshot) -> bool {
    valves.actuated_for_cmd("Nitrous") == Some(false)
}

fn setup_valves_ready(valves: ValveSnapshot) -> bool {
    valves.dump_open == Some(false)
        && (cfg!(feature = "test_fire_mode") || valves.normally_open == Some(false))
}

fn mass_is_vented(current_mass_kg: Option<f32>, cfg: &SequenceConfig) -> bool {
    if cfg!(feature = "test_fire_mode") {
        return true;
    }
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

    match current_mass_kg {
        None => issues.push("fill mass has no calibrated telemetry yet"),
        Some(value) if !value.is_finite() => issues.push("fill mass calibrated value is invalid"),
        Some(value)
            if value < cfg.calibration_mass_min_kg || value > cfg.calibration_mass_max_kg =>
        {
            issues.push("fill mass is outside the configured calibration range")
        }
        Some(_) => {}
    }
    match pressure_psi {
        None => issues.push("tank pressure has no calibrated telemetry yet"),
        Some(value) if !value.is_finite() => {
            issues.push("tank pressure calibrated value is invalid")
        }
        Some(value)
            if value < cfg.calibration_pressure_min_psi
                || value > cfg.calibration_pressure_max_psi =>
        {
            issues.push("tank pressure is outside the configured calibration range")
        }
        Some(_) => {}
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

fn nitrous_fill_status(_state: &AppState, current_mass_kg: f32) -> (f32, f32) {
    let fill_target_mass_kg = fill_targets::load_or_default().nitrous.target_mass_kg;
    let target_mass_kg = normalized_mass_target(fill_target_mass_kg, 0.0001);
    let percent = loadcell::fill_percent(target_mass_kg, current_mass_kg);
    (target_mass_kg, percent)
}

fn normalized_mass_target(target_mass_kg: f32, epsilon_kg: f32) -> f32 {
    if target_mass_kg.abs() < epsilon_kg {
        if target_mass_kg.is_sign_negative() {
            -epsilon_kg
        } else {
            epsilon_kg
        }
    } else {
        target_mass_kg
    }
}

fn mass_target_reached(current_mass_kg: f32, target_mass_kg: f32, epsilon_kg: f32) -> bool {
    if target_mass_kg >= 0.0 {
        current_mass_kg + epsilon_kg >= target_mass_kg
    } else {
        current_mass_kg - epsilon_kg <= target_mass_kg
    }
}

/// The one-button GSE fill uses the same thresholds and signed mass target as
/// the main fill: immediate target cutoff, or a pressure/weight plateau.
#[cfg_attr(not(test), derive(Default))]
pub(crate) struct FillCutoff {
    #[cfg(test)]
    cfg: SequenceConfig,
    previous: Option<(f32, f32)>,
    level_since: Option<Instant>,
}
#[cfg(test)]
impl Default for FillCutoff {
    fn default() -> Self {
        Self {
            #[cfg(test)]
            cfg: SequenceConfig::from_env(),
            previous: None,
            level_since: None,
        }
    }
}
impl FillCutoff {
    #[cfg(test)]
    pub fn target_met(&self, mass: f32, target: f32) -> bool {
        mass.is_finite()
            && target.is_finite()
            && target != 0.0
            && (mass_target_reached(mass, target, self.cfg.nitrous_weight_rise_epsilon_kg)
                || loadcell::fill_percent(target, mass) >= 99.5)
    }
    pub fn reset(&mut self) {
        self.previous = None;
        self.level_since = None;
    }
    #[cfg(test)]
    pub fn reached(&mut self, now: Instant, mass: f32, pressure: f32, target: f32) -> bool {
        if !mass.is_finite() || !pressure.is_finite() || !target.is_finite() || target == 0.0 {
            self.reset();
            return false;
        }
        if self.target_met(mass, target) {
            return true;
        }
        let mut previous_mass = self.previous.map(|(m, _)| m);
        let mut previous_pressure = self.previous.map(|(_, p)| p);
        let reached = nitrous_plateau(
            &self.cfg,
            now,
            pressure,
            Some(mass),
            &mut previous_pressure,
            &mut previous_mass,
            &mut self.level_since,
        );
        self.previous = Some((mass, pressure));
        reached
    }
}

fn nitrous_plateau(
    cfg: &SequenceConfig,
    now: Instant,
    pressure: f32,
    mass: Option<f32>,
    previous_pressure: &mut Option<f32>,
    previous_mass: &mut Option<f32>,
    level_since: &mut Option<Instant>,
) -> bool {
    let rising = previous_pressure.is_some_and(|p| pressure > p + cfg.nitrous_rise_epsilon_psi);
    let weight_rising = match (*previous_mass, mass) {
        (Some(prev), Some(cur)) => cur > prev + cfg.nitrous_weight_rise_epsilon_kg,
        _ => false,
    };
    *previous_pressure = Some(pressure);
    *previous_mass = mass;
    if pressure < cfg.nitrous_pressure_min_psi || rising || weight_rising {
        *level_since = None;
        return false;
    }
    now.saturating_duration_since(*level_since.get_or_insert(now)) >= cfg.nitrous_level_duration
}

fn mass_target_within_tolerance(
    current_mass_kg: f32,
    target_mass_kg: f32,
    tolerance_kg: f32,
) -> bool {
    (current_mass_kg - target_mass_kg).abs() <= tolerance_kg.max(0.0)
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
    let dismiss_fill_test_warning_notifications =
        |state: &AppState, runtime: &mut SequenceRuntime| {
            if let Some(id) = runtime.rapid_drop_notification_id.take() {
                let _ = state.dismiss_notification(id);
            }
            if let Some(id) = runtime.mass_shift_notification_id.take() {
                let _ = state.dismiss_notification(id);
            }
        };

    if valves.dump_open == Some(true) && dump_open_fails_nitrogen_step(runtime.step) {
        dismiss_leak_fail_notification(state, runtime);
        dismiss_fill_test_warning_notifications(state, runtime);
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
        dismiss_fill_test_warning_notifications(state, runtime);
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
                "Vent valve is closed early. Open the vent valve before continuing the fill sequence.",
            );
            runtime.notified_reopen_normally_open = true;
        }
        return;
    }
    if sequence_blocks_until_normally_closed(runtime.step) && valves.normally_open == Some(true) {
        if !runtime.notified_reopen_normally_open {
            state.add_notification(
                "Vent valve must be closed for this step. Close the vent valve before continuing the fill sequence.",
            );
            runtime.notified_reopen_normally_open = true;
        }
        return;
    }
    if (sequence_blocks_until_normally_open(runtime.step) && valves.normally_open == Some(true))
        || (sequence_blocks_until_normally_closed(runtime.step)
            && valves.normally_open == Some(false))
    {
        runtime.notified_reopen_normally_open = false;
    }

    if matches!(runtime.step, SequenceStep::SetupValves) {
        if let Some(issue) =
            fill_sequence_calibration_issue(state, cfg, pressure_psi, current_mass_kg)
        {
            runtime.calibration_ready = false;
            if runtime.calibration_block_notification_id.is_none() {
                let id = state.add_notification(issue.clone());
                runtime.calibration_block_notification_id = Some(id);
            }
            runtime.calibration_block_message = Some(issue);
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
            if setup_valves_ready(valves) {
                runtime.step = first_fill_step_after_setup();
            }
        }
        SequenceStep::NitrogenFill => {
            if valves.nitrogen_open != Some(true) {
                let manually_closed_near_target = valves.nitrogen_open == Some(false)
                    && cfg.nitrogen_target_mass_kg.is_some_and(|target_mass_kg| {
                        current_mass_kg.is_some_and(|m| {
                            mass_target_within_tolerance(
                                m,
                                target_mass_kg,
                                cfg.manual_close_target_tolerance_kg,
                            )
                        })
                    });
                if manually_closed_near_target {
                    runtime.auto_close_nitrogen_sent = false;
                    state.add_notification(format!(
                        "Nitrogen valve was manually closed within {:.1} kg of target. Continuing fill sequence.",
                        cfg.manual_close_target_tolerance_kg
                    ));
                    runtime.step = SequenceStep::CloseNitrogen;
                    return;
                }
                runtime.auto_close_nitrogen_sent = false;
                return;
            }

            let pressure_ready = at_or_above(pressure_psi, cfg.nitrogen_pressure_target_psi);
            let weight_ready = cfg.nitrogen_target_mass_kg.is_some_and(|target_mass_kg| {
                current_mass_kg.is_some_and(|m| {
                    mass_target_reached(m, target_mass_kg, cfg.nitrous_weight_rise_epsilon_kg)
                })
            });
            let should_close = match cfg.nitrogen_autoclose_mode {
                NitrogenAutocloseMode::Pressure => pressure_ready,
                NitrogenAutocloseMode::Weight => weight_ready,
                NitrogenAutocloseMode::Both => pressure_ready && weight_ready,
                NitrogenAutocloseMode::Either => pressure_ready || weight_ready,
            };

            if should_close && !runtime.auto_close_nitrogen_sent {
                match state
                    .cmd_tx
                    .try_send(TelemetryCommand::AutomaticNitrogenClose {
                        generation: runtime.generation,
                    }) {
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
                dismiss_fill_test_warning_notifications(state, runtime);
                state.add_temporary_notification(format!(
                    "Nitrogen fill test started with vent valve closed. Monitoring pressure and loadcell hold for {}s with only small noise tolerance.",
                    cfg.leak_check_duration.as_secs()
                ));
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
            let allowed_pressure_drop =
                cfg.max_leak_drop_psi + elapsed_min * cfg.allowed_hold_drop_psi_per_min;
            let allowed_mass_shift =
                cfg.max_leak_mass_delta_kg + elapsed_min * cfg.allowed_hold_mass_drop_kg_per_min;
            let pressure_warning_active = drop_psi > allowed_pressure_drop;
            if pressure_warning_active && !runtime.warned_rapid_drop {
                runtime.warned_rapid_drop = true;
                if runtime.rapid_drop_notification_id.is_none() {
                    runtime.rapid_drop_notification_id = Some(state.add_notification(
                        "Pressure drop exceeded the fill-test allowance. Investigate before continuing.",
                    ));
                }
            } else if !pressure_warning_active {
                runtime.warned_rapid_drop = false;
                if let Some(id) = runtime.rapid_drop_notification_id.take() {
                    let _ = state.dismiss_notification(id);
                }
            }
            let mass_warning_active =
                !cfg!(feature = "test_fire_mode") && mass_shift_kg > cfg.max_leak_mass_delta_kg;
            if mass_warning_active && !runtime.warned_mass_shift {
                runtime.warned_mass_shift = true;
                if runtime.mass_shift_notification_id.is_none() {
                    runtime.mass_shift_notification_id = Some(state.add_notification(
                        "Unexpected loadcell change detected during fill test. Investigate before continuing.",
                    ));
                }
            } else if !mass_warning_active {
                runtime.warned_mass_shift = false;
                if let Some(id) = runtime.mass_shift_notification_id.take() {
                    let _ = state.dismiss_notification(id);
                }
            }
            if now.saturating_duration_since(started) < cfg.leak_check_duration {
                return;
            }

            let pressure_ok = current >= baseline - allowed_pressure_drop;
            let mass_ok = cfg!(feature = "test_fire_mode")
                || current_mass_kg.is_none()
                || mass_shift_kg <= allowed_mass_shift;

            if pressure_ok && mass_ok {
                dismiss_leak_fail_notification(state, runtime);
                dismiss_fill_test_warning_notifications(state, runtime);
                if !runtime.notified_leak_pass {
                    state.add_notification(
                        "Nitrogen hold check passed. Pressure drop stayed within the closed-vent allowance and loadcell is stable.",
                    );
                    runtime.notified_leak_pass = true;
                }
                runtime.next_step_after_dump = Some(SequenceStep::OpenNitrous);
                runtime.step = SequenceStep::DumpNitrogen;
                runtime.step_started_at = None;
            } else {
                dismiss_fill_test_warning_notifications(state, runtime);
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
                dismiss_fill_test_warning_notifications(state, runtime);
                state.add_notification(
                    "Operator override accepted. Continuing fill sequence to nitrous fill.",
                );
                runtime.step = SequenceStep::OpenNitrous;
                return;
            }

            if valves.nitrogen_open == Some(true) {
                dismiss_leak_fail_notification(state, runtime);
                dismiss_fill_test_warning_notifications(state, runtime);
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
                dismiss_fill_test_warning_notifications(state, runtime);
                runtime.step = SequenceStep::NitrogenFill;
            }
        }
        SequenceStep::OpenNitrous => {
            dismiss_leak_fail_notification(state, runtime);
            if valves.nitrous_open != Some(true) {
                let manually_closed_near_target = valves.nitrous_open == Some(false)
                    && current_mass_kg.is_some_and(|m| {
                        let (target_mass_kg, _) = nitrous_fill_status(state, m);
                        mass_target_within_tolerance(
                            m,
                            target_mass_kg,
                            cfg.manual_close_target_tolerance_kg,
                        )
                    });
                if manually_closed_near_target {
                    runtime.nitrous_level_since = None;
                    runtime.last_nitrous_pressure_psi = None;
                    runtime.last_nitrous_mass_kg = None;
                    runtime.auto_close_nitrous_sent = false;
                    runtime.notified_close_nitrous = false;
                    state.add_notification(format!(
                        "Nitrous valve was manually closed within {:.1} kg of target. Continuing fill sequence.",
                        cfg.manual_close_target_tolerance_kg
                    ));
                    runtime.step = SequenceStep::CloseNitrous;
                    return;
                }
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
                    mass_target_reached(m, target_mass_kg, cfg.nitrous_weight_rise_epsilon_kg)
                        || fill_percent >= 99.5,
                )
            });
            if let Some((target_mass_kg, fill_percent, true)) = nitrous_full_by_loadcell {
                if !runtime.auto_close_nitrous_sent {
                    match state
                        .cmd_tx
                        .try_send(TelemetryCommand::AutomaticNitrousClose {
                            generation: runtime.generation,
                        }) {
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

            if nitrous_plateau(
                cfg,
                now,
                current_pressure,
                current_mass_kg,
                &mut runtime.last_nitrous_pressure_psi,
                &mut runtime.last_nitrous_mass_kg,
                &mut runtime.nitrous_level_since,
            ) {
                runtime.notified_close_nitrous = false;
                if !runtime.auto_close_nitrous_sent {
                    match state
                        .cmd_tx
                        .try_send(TelemetryCommand::AutomaticNitrousClose {
                            generation: runtime.generation,
                        }) {
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
                    state.add_notification("Close Nitrous valve before closing the vent valve.");
                }
                runtime.notified_close_nitrous = true;
            }
            if nitrous_close_accepted_for_vent_prompt(valves) {
                runtime.notified_close_normally_open = false;
                runtime.step = step_after_nitrous_valve_closed();
                if runtime.step == SequenceStep::NitrousSoak {
                    runtime.step_started_at = Some(now);
                    runtime.auto_close_nitrous_sent = false;
                    state.add_temporary_notification(format!(
                        "Nitrous settle started. Waiting {}s before fill line removal.",
                        cfg.nitrous_soak_duration.as_secs()
                    ));
                } else {
                    runtime.step_started_at = None;
                }
            }
        }
        SequenceStep::NitrousSoak => {
            let Some(started) = runtime.step_started_at else {
                runtime.step_started_at = Some(now);
                return;
            };
            if now.saturating_duration_since(started) >= cfg.nitrous_soak_duration {
                runtime.notified_retract_fill_lines = false;
                runtime.step = SequenceStep::RetractFillLines;
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
                    "Nitrous fill complete. Close vent valve to start the 30 second settle.",
                );
                runtime.notified_close_normally_open = true;
            }
            if valves.normally_open == Some(false) && valves.nitrous_open == Some(false) {
                runtime.auto_close_nitrous_sent = false;
                runtime.step_started_at = Some(now);
                state.add_temporary_notification(format!(
                    "Vent valve closed. Nitrous settle started. Waiting {}s before fill line removal.",
                    cfg.nitrous_soak_duration.as_secs()
                ));
                runtime.step = SequenceStep::NitrousSoak;
            }
        }
        SequenceStep::RetractFillLines => {
            if !runtime.notified_retract_fill_lines {
                if cfg!(feature = "test_fire_mode") {
                    state.add_notification("Retract fill lines.");
                } else {
                    state.add_notification("Vent valve closed. Retract fill lines.");
                }
                runtime.notified_retract_fill_lines = true;
            }
            if valves.retract == Some(true) {
                runtime.step = SequenceStep::ArmedReady;
            }
        }
        SequenceStep::ArmedReady => {
            if !runtime.notified_armed {
                if cfg!(feature = "test_fire_mode") {
                    state.add_notification(
                        "Fill sequence complete: nitrous closed and fill lines removed. Launch state is ready.",
                    );
                } else {
                    state.add_notification(
                        "Fill sequence complete: nitrous closed, vent valve closed, fill lines removed. Launch state is ready.",
                    );
                }
                runtime.notified_armed = true;
            }
        }
    }
}

fn selected_prelaunch_state(
    state: &AppState,
    runtime: &SequenceRuntime,
    valves: ValveSnapshot,
    current_state: FlightState,
) -> FlightState {
    if cfg!(feature = "hitl_mode") {
        return current_state;
    }

    if cfg!(feature = "test_fire_mode")
        && matches!(current_state, FlightState::Startup | FlightState::Idle)
    {
        return current_state;
    }

    if (current_state as u8) > (FlightState::Armed as u8) {
        return current_state;
    }

    if current_state == FlightState::Startup && state.all_required_boards_seen() {
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
        return next_state;
    }

    current_state
}

fn drive_observed_prelaunch_state(
    state: &Arc<AppState>,
    runtime: &SequenceRuntime,
    valves: ValveSnapshot,
    current_state: FlightState,
    captured: Option<crate::automatic_flight_state::Captured>,
) -> FlightState {
    let next = selected_prelaunch_state(state, runtime, valves, current_state);
    if let Some(original) = captured
        && original.expected == current_state
        && let Some(proposal) = original.proposal(next,
            crate::automatic_flight_state::Cause::LegacySequence { next_policy: runtime.policy_state() }) {
        crate::automatic_flight_state::publish(state, proposal);
    }
    // Never report an unadmitted proposal as the current actual phase.
    state.local_flight_state_snapshot()
}
#[cfg(test)]
fn maybe_drive_local_prelaunch_state(
    state: &Arc<AppState>, runtime: &SequenceRuntime, valves: ValveSnapshot, current_state: FlightState,
) -> FlightState {
    // Match main's initialized desired Store for this legacy direct-helper
    // fixture. The actual production driver still uses the original entry owner.
    let _ = crate::network_variables::flight_state();
    let observation = crate::automatic_flight_state::Observation::begin(state);
    drive_observed_prelaunch_state(state, runtime, valves, current_state,
        observation.as_ref().and_then(|o| o.captured()))
}

fn hitl_action_policy(valves: ValveSnapshot) -> ActionPolicyMsg {
    let controls = all_command_names()
        .into_iter()
        .map(|cmd| {
            let actuated = default_command_actuated(cmd).or_else(|| valves.actuated_for_cmd(cmd));
            ActionControl { disabled_reason: None,
                cmd: cmd.to_string(),
                enabled: true,
                blink: BlinkMode::None,
                actuated,
            }
        })
        .collect();

    ActionPolicyMsg {
        local_phase_controls: false,
        local_phase_controls_reason: None,
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
        if !vent_closed_for_launch(inputs.valves) {
            if let Some(blink) = command_prompt_blink(
                state,
                cfg,
                inputs.valves,
                "NormallyOpen",
                false,
                inputs.now_ms,
            ) {
                enabled.insert("NormallyOpen", blink);
            }
        } else if !state.launch_indicator_latched() {
            enabled.insert("Launch", BlinkMode::Slow);
            enabled.insert("GroundStationLaunch", BlinkMode::Slow);
        } else {
            enabled.insert("Launch", BlinkMode::None);
            enabled.insert("GroundStationLaunch", BlinkMode::None);
        }
        enabled.insert("Dump", BlinkMode::None);
        let mut policy = policy_with_overrides(
            true,
            inputs.software_buttons_enabled,
            inputs.valves,
            enabled,
        );
        enable_launch_when_key_pressed(&mut policy, inputs);
        return policy;
    }

    if !is_fill_state(inputs.flight_state) {
        // Idle/other non-fill states: keep controls available with no highlight.
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
        if !cfg!(feature = "test_fire_mode")
            && inputs.flight_state == FlightState::Idle
            && let Some(blink) = command_prompt_blink(
                state,
                cfg,
                inputs.valves,
                "NormallyOpen",
                true,
                inputs.now_ms,
            )
        {
            enabled.insert("NormallyOpen", blink);
        }
        if !cfg!(feature = "test_fire_mode")
            && inputs.flight_state == FlightState::Idle
            && let Some(blink) =
                command_prompt_blink(state, cfg, inputs.valves, "Dump", false, inputs.now_ms)
        {
            enabled.insert("Dump", blink);
        }
        let mut policy = policy_with_overrides(
            true,
            inputs.software_buttons_enabled,
            inputs.valves,
            enabled,
        );
        if !cfg!(feature = "test_fire_mode") && inputs.flight_state != FlightState::Idle {
            set_control_enabled(&mut policy, "Launch", false);
            set_control_enabled(&mut policy, "GroundStationLaunch", false);
        }
        enable_launch_when_key_pressed(&mut policy, inputs);
        return policy;
    }

    let mut recommended: HashMap<&'static str, BlinkMode> = HashMap::new();

    if sequence_expects_normally_open(runtime.step)
        && let Some(blink) = command_prompt_blink(
            state,
            cfg,
            inputs.valves,
            "NormallyOpen",
            true,
            inputs.now_ms,
        )
    {
        recommended.insert("NormallyOpen", blink);
    }
    if sequence_expects_normally_closed(runtime.step)
        && let Some(blink) = command_prompt_blink(
            state,
            cfg,
            inputs.valves,
            "NormallyOpen",
            false,
            inputs.now_ms,
        )
    {
        recommended.insert("NormallyOpen", blink);
    }

    match runtime.step {
        SequenceStep::SetupValves => {
            if !cfg!(feature = "test_fire_mode")
                && let Some(blink) = command_prompt_blink(
                    state,
                    cfg,
                    inputs.valves,
                    "NormallyOpen",
                    false,
                    inputs.now_ms,
                ) {
                    recommended.insert("NormallyOpen", blink);
                }
            if let Some(blink) =
                command_prompt_blink(state, cfg, inputs.valves, "Dump", false, inputs.now_ms)
            {
                recommended.insert("Dump", blink);
            }
        }
        SequenceStep::NitrogenFill => {
            if let Some(blink) =
                command_prompt_blink(state, cfg, inputs.valves, "Nitrogen", true, inputs.now_ms)
            {
                recommended.insert("Nitrogen", blink);
            }
        }
        SequenceStep::CloseNitrogen => {
            if let Some(blink) =
                command_prompt_blink(state, cfg, inputs.valves, "Nitrogen", false, inputs.now_ms)
            {
                recommended.insert("Nitrogen", blink);
            }
        }
        SequenceStep::NitrogenLeakCheck => {}
        SequenceStep::RecoverNitrogenClose => {
            if let Some(blink) =
                command_prompt_blink(state, cfg, inputs.valves, "Nitrogen", false, inputs.now_ms)
            {
                recommended.insert("Nitrogen", blink);
            } else if let Some(blink) =
                command_prompt_blink(state, cfg, inputs.valves, "Dump", true, inputs.now_ms)
            {
                recommended.insert("Dump", blink);
            }
        }
        SequenceStep::RecoverNitrogenVent => {
            if let Some(blink) =
                command_prompt_blink(state, cfg, inputs.valves, "Dump", true, inputs.now_ms)
            {
                recommended.insert("Dump", blink);
            }
        }
        SequenceStep::RecoverNitrogenCloseDump => {
            if let Some(blink) =
                command_prompt_blink(state, cfg, inputs.valves, "Dump", false, inputs.now_ms)
            {
                recommended.insert("Dump", blink);
            }
        }
        SequenceStep::DumpNitrogen => {
            if let Some(blink) =
                command_prompt_blink(state, cfg, inputs.valves, "Dump", true, inputs.now_ms)
            {
                recommended.insert("Dump", blink);
            }
        }
        SequenceStep::CloseDump => {
            if let Some(blink) =
                command_prompt_blink(state, cfg, inputs.valves, "Dump", false, inputs.now_ms)
            {
                recommended.insert("Dump", blink);
            }
        }
        SequenceStep::AwaitFillTestDecision => {
            if let Some(blink) =
                command_prompt_blink(state, cfg, inputs.valves, "Nitrogen", true, inputs.now_ms)
            {
                recommended.insert("Nitrogen", blink);
            }
        }
        SequenceStep::OpenNitrous => {
            if let Some(blink) =
                command_prompt_blink(state, cfg, inputs.valves, "Nitrous", true, inputs.now_ms)
            {
                recommended.insert("Nitrous", blink);
            }
        }
        SequenceStep::CloseNitrous => {
            if let Some(blink) =
                command_prompt_blink(state, cfg, inputs.valves, "Nitrous", false, inputs.now_ms)
            {
                recommended.insert("Nitrous", blink);
            }
        }
        SequenceStep::NitrousSoak => {}
        SequenceStep::RecoverNitrousClose => {
            if let Some(blink) =
                command_prompt_blink(state, cfg, inputs.valves, "Nitrous", false, inputs.now_ms)
            {
                recommended.insert("Nitrous", blink);
            } else if let Some(blink) =
                command_prompt_blink(state, cfg, inputs.valves, "Dump", true, inputs.now_ms)
            {
                recommended.insert("Dump", blink);
            }
        }
        SequenceStep::RecoverNitrousVent => {
            if let Some(blink) =
                command_prompt_blink(state, cfg, inputs.valves, "Dump", true, inputs.now_ms)
            {
                recommended.insert("Dump", blink);
            }
        }
        SequenceStep::RecoverNitrousCloseDump => {
            if let Some(blink) =
                command_prompt_blink(state, cfg, inputs.valves, "Dump", false, inputs.now_ms)
            {
                recommended.insert("Dump", blink);
            }
        }
        SequenceStep::CloseNormallyOpen => {
            if let Some(blink) = command_prompt_blink(
                state,
                cfg,
                inputs.valves,
                "NormallyOpen",
                false,
                inputs.now_ms,
            ) {
                recommended.insert("NormallyOpen", blink);
            }
        }
        SequenceStep::RetractFillLines => {
            if let Some(blink) = command_prompt_blink(
                state,
                cfg,
                inputs.valves,
                "RetractPlumbing",
                true,
                inputs.now_ms,
            ) {
                recommended.insert("RetractPlumbing", blink);
            }
        }
        SequenceStep::ArmedReady => {
            if !state.launch_indicator_latched() {
                recommended.insert("Launch", BlinkMode::Slow);
                recommended.insert("GroundStationLaunch", BlinkMode::Slow);
            }
        }
    }

    let mut policy = policy_with_overrides(
        true,
        inputs.software_buttons_enabled,
        inputs.valves,
        recommended,
    );
    set_control_enabled(&mut policy, "Launch", false);
    set_control_enabled(&mut policy, "GroundStationLaunch", false);
    enable_launch_when_key_pressed(&mut policy, inputs);
    policy
}

fn read_key_enabled(state: &AppState, cfg: &SequenceConfig) -> bool {
    if crate::flight_sim::sim_mode_enabled() {
        return true;
    }
    if cfg!(feature = "testing") {
        return true;
    }
    if cfg!(feature = "raspberry_pi")
        || cfg!(feature = "hitl_mode")
        || cfg!(feature = "test_fire_mode")
    {
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
    if cfg!(feature = "raspberry_pi") {
        return state
            .gpio
            .read_input_pin(crate::gpio_panel::ALL_BUTTONS_ENABLE_PIN)
            .unwrap_or(false);
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
    crate::gse::initialize(&state);
    let cfg = SequenceConfig::from_env();
    if cfg!(feature = "hitl_mode") {
        return tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(200));
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        #[cfg(all(test, feature = "test_fire_mode"))]
                        crate::recovery_pending_probe::sequence_tick(&state);
                        state.expire_pending_umbilical_valve_states(UMBILICAL_PENDING_VALVE_TIMEOUT);
                        #[cfg(feature = "hitl_mode")]
                        crate::gse::tick(&state, state.hitl_button_interlock_satisfied());
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
        && !cfg!(feature = "raspberry_pi")
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
        let mut last_flight_state: Option<FlightState> = None;

        loop {
            tokio::select! {
                _ = tick.tick() => {}
                recv = shutdown_rx.recv() => {
                    match recv {
                        Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) | Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }

            // Acquire before observations/GSE waits. Recovery cannot advance
            // the epoch while this tick can still publish managed state.
            let Some(sequence_observation) =
                begin_sequence_tick(&state, &mut runtime, &mut last_flight_state)
            else {
                continue;
            };
            #[cfg(all(test, feature = "test_fire_mode"))]
            crate::recovery_pending_probe::sequence_tick(&state);
            let generation = runtime.generation;
            state.expire_pending_umbilical_valve_states(UMBILICAL_PENDING_VALVE_TIMEOUT);
            let mut flight_state = *state.state.lock().unwrap();
            #[cfg(all(test, not(any(feature = "hitl_mode", feature = "test_fire_mode", feature = "firmware_sim"))))]
            crate::normal_publication_probe::point(&state, crate::normal_publication_probe::Site::CountedTick, flight_state as u8).await;
            let valves = ValveSnapshot::read(&state);
            let pressure_psi = *state.latest_fuel_tank_pressure.lock().unwrap();
            let current_mass_kg = *state.latest_fill_mass_kg.lock().unwrap();
            let now = Instant::now();
            let now_ms = crate::telemetry_task::get_current_timestamp_ms();
            let key_enabled = read_key_enabled(&state, &cfg);
            let software_buttons_enabled = read_software_buttons_enabled(&state, &cfg);
            let sequence_active = !cfg!(feature = "test_fire_mode")
                || !matches!(flight_state, FlightState::Startup | FlightState::Idle);

            if cfg!(feature = "test_fire_mode") {
                // Readiness is published only by the original dequeued event
                // owner. The live policy timer must not manufacture intent.
                if flight_state == FlightState::Idle {
                    runtime = SequenceRuntime::for_generation(generation);
                    sequence_observation.update_policy(&state, flight_state, runtime.policy_state());
                } else if last_flight_state != Some(FlightState::PreFill)
                    && flight_state == FlightState::PreFill
                {
                    runtime = SequenceRuntime::for_generation(generation);
                    sequence_observation.update_policy(&state, flight_state, runtime.policy_state());
                    state.add_temporary_notification(
                        "Test-fire sequence started. Use the sequencing LEDs to move valves into the required starting positions.",
                    );
                }
            }

            crate::gse::tick(&state, key_enabled && software_buttons_enabled);
            if sequence_active && !crate::gse::claimed(&state) {
                update_sequence_runtime(
                    &state,
                    &mut runtime,
                    &cfg,
                    valves,
                    pressure_psi,
                    current_mass_kg,
                    now,
                );
            }
            if !crate::gse::claimed(&state) {
                flight_state =
                    drive_observed_prelaunch_state(&state, &runtime, valves, flight_state, sequence_observation.captured());
            }
            sequence_observation.update_policy(&state, flight_state, runtime.policy_state());
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
            last_flight_state = Some(flight_state);
        }
    })
}

pub fn refresh_action_policy_now(state: &Arc<AppState>) {
    let Some(observation) = crate::automatic_flight_state::Observation::begin(state) else { return; };
    refresh_action_policy_at_generation(state, observation.generation(), Some(&observation));
}

/// Only GSE's existing serialized command owner calls this entry point.
/// Its dispatch gate already excludes reset epoch changes; reacquiring that
/// non-reentrant gate here would deadlock the command owner.
pub(crate) fn refresh_action_policy_under_dispatch(_state: &Arc<AppState>) {
    // The actual public command wrapper carries the original observation and
    // refreshes after dropping Dispatch. No config/provider/publisher reentry.
}
pub(crate) fn refresh_action_policy_with_observation(
    state: &Arc<AppState>, observation: &crate::automatic_flight_state::Observation,
) {
    refresh_action_policy_at_generation(state, observation.generation(), Some(observation));
}
pub(crate) fn refresh_action_policy_projection(state: &Arc<AppState>) {
    // This is UI projection only; it cannot select/admit another phase.
    refresh_action_policy_at_generation(state, crate::gse::automatic_generation(state), None);
}

fn refresh_action_policy_at_generation(
    state: &Arc<AppState>, generation: u64,
    observation: Option<&crate::automatic_flight_state::Observation>,
) {
    let cfg = SequenceConfig::from_env();
    if cfg!(feature = "hitl_mode") {
        let valves = ValveSnapshot::read(state);
        state.set_action_policy(hitl_action_policy(valves));
        return;
    }

    let mut runtime = SequenceRuntime::from_policy_state(state.sequence_policy_state_snapshot());
    runtime.bind_generation(generation);
    let mut flight_state = *state.state.lock().unwrap();
    let valves = ValveSnapshot::read(state);
    let now_ms = crate::telemetry_task::get_current_timestamp_ms();
    let key_enabled = read_key_enabled(state, &cfg);
    let software_buttons_enabled = read_software_buttons_enabled(state, &cfg);

    // TF readiness state is owned solely by a fresh dequeued packet's original
    // generation/Stop/SDK admission. Policy refresh never mints that authority.
    if !crate::gse::claimed(state) && let Some(observation) = observation {
        flight_state = drive_observed_prelaunch_state(state, &runtime, valves, flight_state, observation.captured());
    }
    if let Some(observation) = observation {
        observation.update_policy(state, flight_state, runtime.policy_state());
    }
    let policy = build_policy(
        state,
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

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn captured_normal_startup_reset_policy_republishes_idle_but_hitl_preserves_startup() {
        if crate::operator_queue::isolated_case(module_path!(), "captured_normal_startup_reset_policy_republishes_idle_but_hitl_preserves_startup") { return; }
        crate::telemetry_schema::initialize().unwrap();
        let state = crate::state::tests::test_app_state().await;
        for board in crate::types::Board::ALL {
            state.mark_board_seen(board.sender_id(), 1);
        }
        assert!(state.all_required_boards_seen());
        let router = std::sync::Arc::new(sedsnet::router::Router::new(Default::default()));
        crate::telemetry_task::refresh_host_network_time(
            &router,
            crate::telemetry_task::get_current_timestamp_ms(),
        );
        let ty = crate::telemetry_schema::data_type("FLIGHT_STATE");
        router
            .enable_network_variable(ty, sedsnet::router::NetworkVariablePermissions::READ_WRITE)
            .unwrap();
        state
            .topology_router
            .set(router.clone())
            .unwrap_or_else(|_| panic!("empty router slot"));
        assert_eq!(
            state.local_flight_state_snapshot(),
            crate::types::FlightState::Startup
        );
        let next = super::maybe_drive_local_prelaunch_state(
            &state,
            &super::SequenceRuntime::default(),
            super::ValveSnapshot::default(),
            crate::types::FlightState::Startup,
        );
        if cfg!(feature = "hitl_mode") || cfg!(feature = "test_fire_mode") {
            assert_eq!(next, crate::types::FlightState::Startup);
            assert_eq!(
                state.local_flight_state_snapshot(),
                crate::types::FlightState::Startup
            );
            assert!(router.get_cached_network_variable(ty).unwrap().is_none());
            println!("POLICY_PROOF operator-mode keepsStartup0; no automaticIdle1 publication");
        } else {
            assert_eq!(next, crate::types::FlightState::Idle);
            assert_eq!(
                state.local_flight_state_snapshot(),
                crate::types::FlightState::Idle
            );
            assert_eq!(
                router
                    .get_cached_network_variable(ty)
                    .unwrap()
                    .unwrap()
                    .payload(),
                &[1]
            );
            println!(
                "POLICY_PROOF normal ready Startup0->Idle1 invokes real AppState+managedpublisher; no output side or hardware"
            );
        }
    }
    #[test]
    fn one_button_fill_uses_main_target_and_plateau_rules() {
        let mut monitor = super::FillCutoff::default();
        monitor.cfg.nitrous_weight_rise_epsilon_kg = 0.03;
        monitor.cfg.nitrous_pressure_min_psi = 100.0;
        monitor.cfg.nitrous_rise_epsilon_psi = 1.0;
        monitor.cfg.nitrous_level_duration = std::time::Duration::from_secs(3);
        let now = std::time::Instant::now();
        assert!(monitor.reached(now, 10.0, 90.0, 10.0));
        assert!(monitor.reached(now, -10.0, 90.0, -10.0));
        monitor.reset();
        assert!(!monitor.reached(now, 5.0, 110.0, 10.0));
        assert!(!monitor.reached(now + std::time::Duration::from_secs(2), 5.0, 110.0, 10.0));
        assert!(monitor.reached(now + std::time::Duration::from_secs(3), 5.0, 110.0, 10.0));
        assert!(!monitor.reached(now + std::time::Duration::from_secs(4), 6.0, 110.0, 10.0));
        assert!(!monitor.reached(now + std::time::Duration::from_secs(5), 6.0, 90.0, 10.0));
        assert!(!monitor.reached(now, f32::NAN, 110.0, 10.0));
    }
    use super::*;

    #[cfg(feature = "test_fire_mode")]
    #[test]
    fn test_fire_sequence_starts_with_nitrous_fill_after_setup() {
        assert_eq!(first_fill_step_after_setup(), SequenceStep::OpenNitrous);
    }

    #[cfg(feature = "test_fire_mode")]
    #[test]
    fn test_fire_setup_does_not_require_closing_vent_valve() {
        assert!(!sequence_expects_normally_closed(SequenceStep::SetupValves));
        assert!(!sequence_blocks_until_normally_closed(
            SequenceStep::SetupValves
        ));
        assert!(!sequence_expects_normally_closed(SequenceStep::NitrousSoak));
        assert!(!sequence_blocks_until_normally_closed(
            SequenceStep::NitrousSoak
        ));
        assert!(setup_valves_ready(ValveSnapshot {
            normally_open: Some(true),
            dump_open: Some(false),
            ..ValveSnapshot::default()
        }));
    }

    #[cfg(feature = "test_fire_mode")]
    #[test]
    fn test_fire_prompts_vent_close_after_nitrous_closes() {
        assert_eq!(
            step_after_nitrous_valve_closed(),
            SequenceStep::CloseNormallyOpen
        );
    }

    #[test]
    fn armed_launch_requires_closed_vent_valve() {
        assert!(!vent_closed_for_launch(ValveSnapshot {
            normally_open: Some(true),
            ..ValveSnapshot::default()
        }));
        assert!(!vent_closed_for_launch(ValveSnapshot::default()));
        assert!(vent_closed_for_launch(ValveSnapshot {
            normally_open: Some(false),
            ..ValveSnapshot::default()
        }));
        assert!(!vent_closed_for_launch(ValveSnapshot {
            normally_open: Some(true),
            pending_normally_open: Some(false),
            ..ValveSnapshot::default()
        }));
    }

    #[cfg(not(feature = "test_fire_mode"))]
    #[test]
    fn normal_sequence_starts_with_nitrogen_fill_after_setup() {
        assert_eq!(first_fill_step_after_setup(), SequenceStep::NitrogenFill);
    }

    #[test]
    fn sequence_prompts_vent_close_after_nitrous_closes() {
        assert_eq!(
            step_after_nitrous_valve_closed(),
            SequenceStep::CloseNormallyOpen
        );
    }

    #[test]
    fn nitrous_pending_close_is_not_confirmed_closed() {
        assert!(!nitrous_close_accepted_for_vent_prompt(ValveSnapshot {
            nitrous_open: Some(true),
            pending_nitrous_open: Some(false),
            ..ValveSnapshot::default()
        }));
    }

    #[test]
    fn key_pressed_keeps_launch_controls_enabled() {
        let inputs = PolicyInputs {
            flight_state: FlightState::Coast,
            key_enabled: true,
            software_buttons_enabled: true,
            valves: ValveSnapshot::default(),
            now_ms: 0,
        };
        let mut policy = policy_with_overrides(true, true, inputs.valves, HashMap::new());
        set_control_enabled(&mut policy, "Launch", false);
        set_control_enabled(&mut policy, "GroundStationLaunch", false);

        enable_launch_when_key_pressed(&mut policy, inputs);

        let enabled = |cmd: &str| {
            policy
                .controls
                .iter()
                .find(|control| control.cmd == cmd)
                .map(|control| control.enabled)
        };
        assert_eq!(enabled("Launch"), Some(true));
        #[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
        assert_eq!(enabled("GroundStationLaunch"), Some(true));
    }

    #[cfg(feature = "hitl_mode")]
    #[test]
    fn hitl_policy_enables_manual_igniter_sequence() {
        let policy = default_action_policy();
        let enabled = |cmd: &str| {
            policy
                .controls
                .iter()
                .find(|control| control.cmd == cmd)
                .map(|control| control.enabled)
        };

        assert_eq!(enabled("GroundStationLaunch"), Some(true));
        assert_eq!(enabled("IgniterSequence"), Some(true));
    }
}

#[cfg(all(test, not(any(feature = "hitl_mode", feature = "test_fire_mode"))))]
mod automatic_phase_tests;

#[cfg(test)]
mod disabled_reason_tests {
    use super::*;
    #[test]
    fn disabled_reasons_are_presentation_only_and_preserve_precise_server_causes() {
        let mut policy: ActionPolicyMsg = serde_json::from_value(serde_json::json!({
            "key_enabled":true,"software_buttons_enabled":true,
            "controls":[{"cmd":"Launch","enabled":false,"blink":"none","actuated":null,
                "disabled_reason":"VB reports Abort; review recovery."},
                {"cmd":"Nitrogen","enabled":true,"blink":"none","actuated":null,
                "disabled_reason":"old blocker"},
                {"cmd":"StartFill","enabled":false,"blink":"none","actuated":null}]
        })).unwrap();
        explain_disabled_policy(&mut policy);
        assert_eq!(policy.controls[0].disabled_reason.as_deref(),Some("VB reports Abort; review recovery."));
        assert!(policy.controls[1].enabled);
        assert_eq!(policy.controls[1].disabled_reason,None);
        assert!(!policy.controls[2].enabled);
        assert!(policy.controls[2].disabled_reason.as_deref().unwrap().contains("GroundStation policy"));
    }
}
