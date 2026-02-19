use crate::rocket_commands::{ActuatorBoardCommands, ValveBoardCommands};
use crate::state::AppState;
use groundstation_shared::{FlightState, TelemetryCommand};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const KEY_ENABLE_PIN: u8 = 25;

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
    pub controls: Vec<ActionControl>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistentNotification {
    pub id: u64,
    pub timestamp_ms: i64,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SequenceStep {
    SetupValves,
    NitrogenFill,
    CloseNitrogen,
    NitrogenLeakCheck,
    DumpNitrogen,
    CloseDump,
    OpenNitrous,
    ArmedReady,
}

#[derive(Clone, Debug)]
struct SequenceConfig {
    leak_check_duration: Duration,
    pressure_min_psi: f32,
    max_leak_drop_psi: f32,
    pending_fast_window: Duration,
    key_required: bool,
    key_enable_pin: u8,
}

impl SequenceConfig {
    fn from_env() -> Self {
        let leak_check_duration = std::env::var("GS_SEQUENCE_LEAK_CHECK_SEC")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(60));

        let pressure_min_psi = std::env::var("GS_SEQUENCE_PRESSURE_MIN_PSI")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(10.0);

        let max_leak_drop_psi = std::env::var("GS_SEQUENCE_MAX_LEAK_DROP_PSI")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(1.0);

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

        Self {
            leak_check_duration,
            pressure_min_psi,
            max_leak_drop_psi,
            pending_fast_window,
            key_required,
            key_enable_pin,
        }
    }
}

#[derive(Clone, Debug)]
struct SequenceRuntime {
    step: SequenceStep,
    step_started_at: Option<Instant>,
    pressure_at_close_psi: Option<f32>,
    notified_leak_pass: bool,
    notified_armed: bool,
}

impl Default for SequenceRuntime {
    fn default() -> Self {
        Self {
            step: SequenceStep::SetupValves,
            step_started_at: None,
            pressure_at_close_psi: None,
            notified_leak_pass: false,
            notified_armed: false,
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
        TelemetryCommand::Launch => "Launch",
        TelemetryCommand::Dump => "Dump",
        TelemetryCommand::Abort => "Abort",
        TelemetryCommand::NormallyOpen => "NormallyOpen",
        TelemetryCommand::Pilot => "Pilot",
        TelemetryCommand::Igniter => "Igniter",
        TelemetryCommand::RetractPlumbing => "RetractPlumbing",
        TelemetryCommand::Nitrogen => "Nitrogen",
        TelemetryCommand::Nitrous => "Nitrous",
    }
}

pub fn all_command_names() -> [&'static str; 9] {
    [
        "Launch",
        "Dump",
        "Abort",
        "NormallyOpen",
        "Pilot",
        "Igniter",
        "RetractPlumbing",
        "Nitrogen",
        "Nitrous",
    ]
}

pub fn default_action_policy() -> ActionPolicyMsg {
    let controls = all_command_names()
        .into_iter()
        .map(|cmd| ActionControl {
            cmd: cmd.to_string(),
            enabled: cmd == "Abort",
            blink: BlinkMode::None,
            actuated: None,
        })
        .collect();
    ActionPolicyMsg {
        key_enabled: true,
        controls,
    }
}

fn policy_with_overrides(
    key_enabled: bool,
    valves: ValveSnapshot,
    enabled: HashMap<&'static str, BlinkMode>,
) -> ActionPolicyMsg {
    let controls = all_command_names()
        .into_iter()
        .map(|cmd| ActionControl {
            cmd: cmd.to_string(),
            enabled: cmd == "Abort" || enabled.contains_key(cmd),
            blink: enabled.get(cmd).cloned().unwrap_or(BlinkMode::None),
            actuated: valves.actuated_for_cmd(cmd),
        })
        .collect();

    ActionPolicyMsg {
        key_enabled,
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

fn update_sequence_runtime(
    state: &AppState,
    runtime: &mut SequenceRuntime,
    cfg: &SequenceConfig,
    valves: ValveSnapshot,
    pressure_psi: Option<f32>,
    now: Instant,
) {
    let at_or_above = |p: Option<f32>, threshold: f32| p.is_some_and(|x| x >= threshold);

    match runtime.step {
        SequenceStep::SetupValves => {
            if valves.normally_open == Some(false) && valves.dump_open == Some(false) {
                runtime.step = SequenceStep::NitrogenFill;
            }
        }
        SequenceStep::NitrogenFill => {
            if valves.nitrogen_open == Some(true) && at_or_above(pressure_psi, cfg.pressure_min_psi)
            {
                runtime.step = SequenceStep::CloseNitrogen;
            }
        }
        SequenceStep::CloseNitrogen => {
            if valves.nitrogen_open == Some(false) {
                runtime.pressure_at_close_psi = pressure_psi;
                runtime.step_started_at = Some(now);
                runtime.step = SequenceStep::NitrogenLeakCheck;
            }
        }
        SequenceStep::NitrogenLeakCheck => {
            let Some(started) = runtime.step_started_at else {
                runtime.step_started_at = Some(now);
                return;
            };
            if now.saturating_duration_since(started) < cfg.leak_check_duration {
                return;
            }

            let baseline = runtime.pressure_at_close_psi.unwrap_or(0.0);
            let current = pressure_psi.unwrap_or(0.0);
            let pressure_ok = current >= baseline - cfg.max_leak_drop_psi;

            if pressure_ok {
                if !runtime.notified_leak_pass {
                    state.add_notification(
                        "Nitrogen hold check passed. Good to proceed to nitrous fill.",
                    );
                    runtime.notified_leak_pass = true;
                }
                runtime.step = SequenceStep::DumpNitrogen;
                runtime.step_started_at = None;
            } else {
                state.add_notification(
                    "Nitrogen hold check failed: pressure dropped. Refill required.",
                );
                runtime.step = SequenceStep::NitrogenFill;
                runtime.step_started_at = None;
            }
        }
        SequenceStep::DumpNitrogen => {
            if valves.dump_open == Some(true) {
                runtime.step = SequenceStep::CloseDump;
            }
        }
        SequenceStep::CloseDump => {
            if valves.dump_open == Some(false) {
                runtime.step = SequenceStep::OpenNitrous;
            }
        }
        SequenceStep::OpenNitrous => {
            if valves.nitrous_open == Some(true) {
                runtime.step = SequenceStep::ArmedReady;
            }
        }
        SequenceStep::ArmedReady => {
            if !runtime.notified_armed {
                state.add_notification(
                    "Nitrous fill complete. Key is accepted; launch can proceed when enabled.",
                );
                runtime.notified_armed = true;
            }
        }
    }
}

fn build_policy(
    state: &AppState,
    cfg: &SequenceConfig,
    runtime: &SequenceRuntime,
    flight_state: FlightState,
    key_enabled: bool,
    valves: ValveSnapshot,
    now_ms: u64,
) -> ActionPolicyMsg {
    if !key_enabled {
        return policy_with_overrides(false, valves, HashMap::new());
    }

    if flight_state == FlightState::Armed {
        let mut enabled = HashMap::new();
        enabled.insert("Launch", BlinkMode::Slow);
        enabled.insert("Dump", BlinkMode::None);
        return policy_with_overrides(true, valves, enabled);
    }

    if !is_fill_state(flight_state) {
        return policy_with_overrides(true, valves, HashMap::new());
    }

    let mut enabled: HashMap<&'static str, BlinkMode> = HashMap::new();

    match runtime.step {
        SequenceStep::SetupValves => {
            if valves.normally_open != Some(false) {
                enabled.insert(
                    "NormallyOpen",
                    pending_mode(state, "NormallyOpen", now_ms, cfg),
                );
            }
            if valves.dump_open != Some(false) {
                enabled.insert("Dump", pending_mode(state, "Dump", now_ms, cfg));
            }
        }
        SequenceStep::NitrogenFill => {
            if valves.nitrogen_open != Some(true) {
                enabled.insert("Nitrogen", pending_mode(state, "Nitrogen", now_ms, cfg));
            }
        }
        SequenceStep::CloseNitrogen => {
            if valves.nitrogen_open != Some(false) {
                enabled.insert("Nitrogen", pending_mode(state, "Nitrogen", now_ms, cfg));
            }
        }
        SequenceStep::NitrogenLeakCheck => {}
        SequenceStep::DumpNitrogen => {
            if valves.dump_open != Some(true) {
                enabled.insert("Dump", pending_mode(state, "Dump", now_ms, cfg));
            }
        }
        SequenceStep::CloseDump => {
            if valves.dump_open != Some(false) {
                enabled.insert("Dump", pending_mode(state, "Dump", now_ms, cfg));
            }
        }
        SequenceStep::OpenNitrous => {
            if valves.nitrous_open != Some(true) {
                enabled.insert("Nitrous", pending_mode(state, "Nitrous", now_ms, cfg));
            }
        }
        SequenceStep::ArmedReady => {
            enabled.insert("Launch", BlinkMode::Slow);
        }
    }

    policy_with_overrides(true, valves, enabled)
}

fn read_key_enabled(state: &AppState, cfg: &SequenceConfig) -> bool {
    if !cfg.key_required {
        return true;
    }
    state
        .gpio
        .read_input_pin(cfg.key_enable_pin)
        .unwrap_or(false)
}

pub fn start_sequence_task(state: Arc<AppState>) {
    let cfg = SequenceConfig::from_env();
    if cfg.key_required
        && let Err(err) = state.gpio.setup_input_pin(cfg.key_enable_pin)
    {
        eprintln!(
            "Sequence key GPIO setup failed (pin {}): {}",
            cfg.key_enable_pin, err
        );
    }

    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(200));
        let mut runtime = SequenceRuntime::default();

        loop {
            tick.tick().await;

            let flight_state = *state.state.lock().unwrap();
            let valves = ValveSnapshot::read(&state);
            let pressure_psi = *state.latest_fuel_tank_pressure.lock().unwrap();
            let now = Instant::now();
            let now_ms = crate::telemetry_task::get_current_timestamp_ms();
            let key_enabled = read_key_enabled(&state, &cfg);

            update_sequence_runtime(&state, &mut runtime, &cfg, valves, pressure_psi, now);
            let policy = build_policy(
                &state,
                &cfg,
                &runtime,
                flight_state,
                key_enabled,
                valves,
                now_ms,
            );
            state.set_action_policy(policy);
        }
    });
}
