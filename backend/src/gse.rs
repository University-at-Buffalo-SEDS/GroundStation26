use crate::{
    auth::Permission,
    sequences::{ActionControl, ActionPolicyMsg, BlinkMode},
    state::AppState,
    types::{FlightState, TelemetryCommand},
};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use gse_sequence::{Action, Config, Effects, Engine, Inputs, Phase, Sample, ValveState};
use std::{sync::Arc, time::Instant};

const KEYS: [u8; 5] = [0, 1, 2, 9, 10];
const OPEN: [u8; 5] = [0, 1, 2, 9, 10];
const CLOSE: [u8; 5] = [3, 4, 5, 12, 13];
const ACTIONS: [(&str, Action); 5] = [
    ("StartFill", Action::StartFill),
    ("PauseFill", Action::PauseFill),
    ("CancelFill", Action::CancelFill),
    ("ValveSelfTest", Action::SelfTest),
    ("NitrogenTest", Action::NitrogenTest),
];

pub struct Runtime {
    pub engine: Engine,
    clock: Instant,
    pressure: Option<Sample>,
    valves: [Option<ValveState>; 5],
}
impl Default for Runtime {
    fn default() -> Self {
        Self {
            engine: Engine::default(),
            clock: Instant::now(),
            pressure: None,
            valves: [None; 5],
        }
    }
}
impl Runtime {
    fn now(&self) -> u64 {
        self.clock.elapsed().as_millis() as u64
    }
    pub fn observe_pressure(&mut self, value: Option<f32>) {
        self.pressure = value.filter(|p| p.is_finite()).map(|value| Sample {
            value,
            at_ms: self.now(),
        });
    }
    pub fn observe_valve(&mut self, key: u8, open: bool) {
        if let Some(i) = KEYS.iter().position(|k| *k == key) {
            self.valves[i] = Some(ValveState {
                open,
                at_ms: self.now(),
            });
        }
    }
    fn input(&self, prelaunch: bool, interlock: bool) -> Inputs {
        Inputs {
            now_ms: self.now(),
            pressure: self.pressure,
            valves: self.valves,
            prelaunch,
            interlock,
        }
    }
}
fn prelaunch(state: &AppState) -> bool {
    matches!(
        *state.state.lock().unwrap(),
        FlightState::Idle
            | FlightState::PreFill
            | FlightState::FillTest
            | FlightState::NitrogenFill
            | FlightState::NitrousFill
    )
}
fn config_path() -> std::path::PathBuf {
    std::env::var_os("GS_GSE_CONFIG")
        .map(Into::into)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/gse_sequence.json")
        })
}
pub fn initialize(state: &AppState) {
    match std::fs::read(config_path())
        .map_err(|e| e.to_string())
        .and_then(|b| serde_json::from_slice::<Config>(&b).map_err(|e| e.to_string()))
    {
        Ok(mut config) => {
            config.dry_self_test_confirmed = false;
            state.gse.lock().unwrap().engine.config = config;
        }
        Err(err) => log::warn!(
            "GSE configuration not loaded; automatic actions require configuration: {err}"
        ),
    }
}
pub fn claimed(state: &AppState) -> bool {
    state.gse.lock().unwrap().engine.claimed
}
pub fn active(state: &AppState) -> bool {
    state.gse.lock().unwrap().engine.active()
}
fn action(cmd: &TelemetryCommand) -> Option<Action> {
    match cmd {
        TelemetryCommand::StartFill => Some(Action::StartFill),
        TelemetryCommand::PauseFill => Some(Action::PauseFill),
        TelemetryCommand::CancelFill => Some(Action::CancelFill),
        TelemetryCommand::ValveSelfTest => Some(Action::SelfTest),
        TelemetryCommand::NitrogenTest => Some(Action::NitrogenTest),
        _ => None,
    }
}
pub fn valve_command(cmd: &TelemetryCommand) -> bool {
    matches!(
        cmd,
        TelemetryCommand::Dump
            | TelemetryCommand::NormallyOpen
            | TelemetryCommand::Pilot
            | TelemetryCommand::Nitrogen
            | TelemetryCommand::Nitrous
            | TelemetryCommand::NitrogenClose
            | TelemetryCommand::NitrousClose
    )
}

pub fn command_allowed(state: &AppState, cmd: &TelemetryCommand) -> Option<bool> {
    let is_prelaunch = prelaunch(state);
    let policy = state.action_policy_snapshot();
    let rt = state.gse.lock().unwrap();
    if let Some(action) = action(cmd) {
        return Some(
            rt.engine
                .allows(action, rt.input(is_prelaunch, interlock(state, &policy))),
        );
    }
    if rt.engine.active()
        && (["GroundStationLaunch", "IgniterSequence"]
            .contains(&crate::sequences::command_name(cmd))
            || valve_command(cmd)
            || matches!(
                cmd,
                TelemetryCommand::Launch
                    | TelemetryCommand::Igniter
                    | TelemetryCommand::Postinit
                    | TelemetryCommand::RetractPlumbing
            ))
    {
        return Some(false);
    }
    None
}
fn interlock(state: &AppState, policy: &ActionPolicyMsg) -> bool {
    #[cfg(feature = "hitl_mode")]
    {
        let _ = policy;
        state.hitl_button_interlock_satisfied()
    }
    #[cfg(not(feature = "hitl_mode"))]
    {
        let _ = state;
        policy.key_enabled && policy.software_buttons_enabled
    }
}
pub fn decorate_policy(state: &AppState, policy: &mut ActionPolicyMsg) {
    let is_prelaunch = prelaunch(state);
    let rt = state.gse.lock().unwrap();
    let input = rt.input(is_prelaunch, interlock(state, policy));
    for (cmd, action) in ACTIONS {
        let enabled = rt.engine.allows(action, input);
        policy.controls.retain(|c| c.cmd != cmd);
        policy.controls.push(ActionControl {
            cmd: cmd.into(),
            enabled,
            blink: BlinkMode::None,
            actuated: Some(match action {
                Action::StartFill => rt.engine.status.phase == Phase::Filling,
                Action::PauseFill => rt.engine.status.phase == Phase::Paused,
                Action::NitrogenTest => rt.engine.status.nitrogen_passed,
                _ => false,
            }),
        });
    }
    if rt.engine.active() {
        for c in &mut policy.controls {
            if [
                "Dump",
                "NormallyOpen",
                "Pilot",
                "Nitrogen",
                "Nitrous",
                "Launch",
                "GroundStationLaunch",
                "Igniter",
                "IgniterSequence",
                "Postinit",
                "RetractPlumbing",
            ]
            .contains(&c.cmd.as_str())
            {
                c.enabled = false;
                c.blink = BlinkMode::None;
            }
        }
    }
}

fn apply(state: &Arc<AppState>, effects: Effects) {
    if !effects.valves.is_empty() {
        if let Some(router) = state.topology_router.get() {
            for (index, open) in effects.valves {
                let kind = crate::telemetry_schema::data_type(if index < 3 {
                    "VALVE_COMMAND"
                } else {
                    "ACTUATOR_COMMAND"
                });
                let payload = if open { OPEN[index] } else { CLOSE[index] };
                if let Err(err) = router.log_queue(kind, &[payload]) {
                    log::error!("GSE command failed: {err}");
                    let recovery = {
                        let mut rt = state.gse.lock().unwrap();
                        let now = rt.now();
                        rt.engine.fault(now, "Command transport failed")
                    };
                    // Best effort closes only; never continue opening a supply after an error.
                    for (index, _) in recovery.valves.into_iter().filter(|(index, _)| *index >= 3) {
                        let _ = router.log_queue(
                            crate::telemetry_schema::data_type("ACTUATOR_COMMAND"),
                            &[CLOSE[index]],
                        );
                    }
                    state.add_notification(
                        "GSE command transport failed. Verify supply valves are closed.",
                    );
                    crate::telemetry_task::flush_command_tx(router, "GSE supply close recovery");
                    return;
                }
                state.set_pending_umbilical_valve_state(KEYS[index], open);
            }
            crate::telemetry_task::flush_command_tx(router, "GSE sequence");
        } else {
            let mut rt = state.gse.lock().unwrap();
            let now = rt.now();
            rt.engine
                .fault(now, "No hardware command router is available");
            drop(rt);
            state.add_notification("GSE sequence failed: no hardware command router is available");
            return;
        }
    }
    if let Some(message) = effects.notification {
        state.add_notification(message);
    }
}
pub fn handle_command(state: &Arc<AppState>, cmd: &TelemetryCommand) -> bool {
    if let Some(action) = action(cmd) {
        let is_prelaunch = prelaunch(state);
        let policy = state.action_policy_snapshot();
        let result = {
            let mut rt = state.gse.lock().unwrap();
            let input = rt.input(is_prelaunch, interlock(state, &policy));
            rt.engine.request(action, input)
        };
        match result {
            Ok(effects) => apply(state, effects),
            Err(err) => {
                state.add_notification(err);
            }
        }
        crate::sequences::refresh_action_policy_now(state);
        return true;
    }
    if matches!(cmd, TelemetryCommand::Abort) && active(state) {
        let effects = {
            let mut rt = state.gse.lock().unwrap();
            let now = rt.now();
            rt.engine.fault(now, "Operator abort")
        };
        apply(state, effects);
    } else if valve_command(cmd) {
        state.gse.lock().unwrap().engine.manual_override();
    }
    false
}
pub fn tick(state: &Arc<AppState>, interlock: bool) {
    let is_prelaunch = prelaunch(state);
    let effects = {
        let mut rt = state.gse.lock().unwrap();
        let input = rt.input(is_prelaunch, interlock);
        rt.engine.tick(input)
    };
    apply(state, effects);
}

/// The existing five-valve hardware cluster can be switched as one group.
pub fn panel_name(state: &AppState, name: &str) -> String {
    if !state.gse.lock().unwrap().engine.config.grouped_panel {
        return name.into();
    }
    match name {
        "Dump" => "CancelFill",
        "NormallyOpen" => "PauseFill",
        "Pilot" => "ValveSelfTest",
        "Nitrogen" => "NitrogenTest",
        "Nitrous" => "StartFill",
        other => other,
    }
    .into()
}
pub fn panel_command(state: &AppState, cmd: TelemetryCommand) -> TelemetryCommand {
    let name = panel_name(state, crate::sequences::command_name(&cmd));
    serde_json::from_value(serde_json::Value::String(name)).unwrap_or(cmd)
}
pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/gse/config",
            get(get_config).put(save_config).post(save_config),
        )
        .route("/api/gse/status", get(status))
}
async fn get_config(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(e) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return e;
    }
    Json(state.gse.lock().unwrap().engine.config.clone()).into_response()
}
async fn save_config(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(config): Json<Config>,
) -> Response {
    if let Err(e) = crate::web::authorize_headers(&state, &headers, Permission::SendCommands).await
    {
        return e;
    }
    if let Err(err) = config.validate_settings() {
        return (StatusCode::BAD_REQUEST, err).into_response();
    }
    let path = config_path();
    let result = (|| -> Result<(), String> {
        let mut rt = state.gse.lock().unwrap();
        if rt.engine.active() {
            return Err("Stop the active sequence before editing GSE configuration".into());
        }
        std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("upload");
        let mut persisted = config.clone();
        persisted.dry_self_test_confirmed = false;
        std::fs::write(
            &tmp,
            serde_json::to_vec_pretty(&persisted).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        std::fs::rename(tmp, path).map_err(|e| e.to_string())?;
        rt.engine.config = config;
        rt.engine.status.nitrogen_passed = false;
        Ok(())
    })();
    match result {
        Ok(()) => {
            crate::sequences::refresh_action_policy_now(&state);
            Json(state.gse.lock().unwrap().engine.config.clone()).into_response()
        }
        Err(err) => (StatusCode::CONFLICT, err).into_response(),
    }
}
async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(e) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return e;
    }
    Json(state.gse.lock().unwrap().engine.status.clone()).into_response()
}
