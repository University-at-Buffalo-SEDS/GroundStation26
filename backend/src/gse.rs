#[cfg(not(feature = "hitl_mode"))]
mod operational_policy;
#[cfg(not(feature = "hitl_mode"))]
use operational_policy::*;
#[cfg(not(feature = "hitl_mode"))]
mod operational;
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
#[cfg(not(feature = "hitl_mode"))]
use gse_sequence::{
    qd_wait::{QdStatus, QdWait},
    qualification::{Fault, Milestone},
};
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
    #[cfg(not(feature = "hitl_mode"))]
    operational: operational::Operational,
    clock: Instant,
    pressure: Option<Sample>,
    valves: [Option<ValveState>; 5],
    mass: Option<Sample>,
    fill_cutoff: crate::sequences::FillCutoff,
    ground_station_control: bool,
}
impl Default for Runtime {
    fn default() -> Self {
        Self {
            engine: Engine::default(),
            #[cfg(not(feature = "hitl_mode"))]
            operational: Default::default(),
            clock: Instant::now(),
            pressure: None,
            valves: [None; 5],
            mass: None,
            fill_cutoff: Default::default(),
            ground_station_control: true,
        }
    }
}
impl Runtime {
    /// Imported calibration must not invalidate an active or already-qualified
    /// procedure. Retain it as pending until the operator returns to Idle and
    /// explicitly clears qualification. Never clear evidence automatically.
    pub fn calibration_import_allowed(&self) -> bool {
        #[cfg(feature = "hitl_mode")]
        {
            false
        }
        #[cfg(not(feature = "hitl_mode"))]
        {
            self.engine.status.phase == Phase::Idle
                && !self.engine.active()
                && self.operational.saved.qualification.stage()
                    == gse_sequence::qualification::MissionStage::Idle
                && ![
                    Milestone::SelfTest,
                    Milestone::NitrogenTest,
                    Milestone::FillTarget,
                ]
                .into_iter()
                .any(|m| self.operational.saved.qualification.completed(m))
        }
    }
    fn fresh_mass(&self, now_ms: u64) -> Option<Sample> {
        self.mass
            .filter(|m| m.at_ms <= now_ms && now_ms - m.at_ms <= 2000)
    }
    pub fn observe_mass(&mut self, value: Option<f32>) {
        self.mass = value.filter(|v| v.is_finite()).map(|value| Sample {
            value,
            at_ms: self.now(),
        });
    }
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
    prelaunch_flight_state(*state.state.lock().unwrap())
}
fn prelaunch_flight_state(state: FlightState) -> bool {
    #[cfg(feature = "hitl_mode")]
    if matches!(state, FlightState::Startup | FlightState::Armed) {
        return true;
    }
    matches!(
        state,
        FlightState::Idle
            | FlightState::Armed
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
    #[cfg(not(feature = "hitl_mode"))]
    {
        let path = std::env::var_os("GS_GSE_QUALIFICATION")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| config_path().with_file_name("gse_qualification.json"));
        let mut rt = state.gse.lock().unwrap();
        rt.operational.path = Some(path.clone());
        match operational::Saved::load(&path) {
            Ok(saved) => {
                if saved.qualification.nitrogen_satisfied() {
                    rt.engine.status.nitrogen_passed =
                        saved.qualification.completed(Milestone::NitrogenTest);
                    rt.engine.status.nitrogen_overridden =
                        saved.qualification.nitrogen_override().is_some();
                    rt.engine.status.self_test_locked = true;
                    rt.engine.status.baseline = saved.baseline.clone();
                    rt.engine.status.phase = Phase::Passed;
                    rt.engine.claimed = true;
                }
                if saved.qualification.stage() == gse_sequence::qualification::MissionStage::Fire {
                    rt.engine.claimed = true;
                    rt.engine.status.message = "Fire was already committed; manual reset is required before a new operation".into();
                }
                rt.operational.run_generation = saved.qualification.generation();
                rt.operational.saved = saved;
                if let Err(error) = rt
                    .operational
                    .persist()
                    .and_then(|()| rt.operational.flush_checklists())
                {
                    log::error!(
                        "Qualification storage unavailable; automatic actions blocked: {error}"
                    );
                }
            }
            Err(error) => {
                rt.operational
                    .saved
                    .qualification
                    .latch(Fault::MissingTelemetry);
                rt.operational.persist_error = Some(error.clone());
                log::error!("Cannot restore GSE qualification; manual reset required: {error}");
            }
        }
    }
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
        #[cfg(feature = "hitl_mode")]
        {
            let _ = (action, is_prelaunch);
            // Admit the request, not the actuation: handle_command still
            // enforces every engine prerequisite before emitting effects.
            return Some(is_prelaunch && interlock(state, &policy));
        }
        #[cfg(not(feature = "hitl_mode"))]
        return Some(
            operational_action_allowed(&rt, action)
                && rt
                    .engine
                    .allows(action, rt.input(is_prelaunch, interlock(state, &policy))),
        );
    }
    #[cfg(not(feature = "hitl_mode"))]
    if rt.operational.saved.qualification.fault().is_some() && valve_command(cmd) {
        return Some(matches!(
            cmd,
            TelemetryCommand::NitrogenClose | TelemetryCommand::NitrousClose
        ));
    }
    #[cfg(not(feature = "hitl_mode"))]
    if matches!(
        crate::sequences::command_name(cmd),
        "Launch" | "GroundStationLaunch" | "RetractPlumbing"
    ) {
        return Some(
            is_prelaunch
                && interlock(state, &policy)
                && operational_launch_allowed(state, &rt)
                && (crate::sequences::command_name(cmd) == "RetractPlumbing"
                    || cfg!(feature = "test_fire_mode")
                    || !matches!(
                        rt.operational.qd.status(rt.now()),
                        QdStatus::NotStarted | QdStatus::AckTimedOut
                    )),
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
fn sequence_button_enabled(engine: &Engine, action: Action, input: Inputs) -> bool {
    #[cfg(feature = "hitl_mode")]
    {
        input.prelaunch
            && input.interlock
            && (action != Action::SelfTest
                || (engine.config.dry_self_test_confirmed
                    && !engine.status.self_test_locked
                    && !engine.active()))
    }
    #[cfg(not(feature = "hitl_mode"))]
    engine.allows(action, input)
}

pub fn decorate_policy(state: &AppState, policy: &mut ActionPolicyMsg) {
    let is_prelaunch = prelaunch(state);
    let rt = state.gse.lock().unwrap();
    let input = rt.input(is_prelaunch, interlock(state, policy));
    for (cmd, action) in ACTIONS {
        // HITL buttons accept a sequence request whenever manual controls are
        // unlocked. Execution still goes through Engine::allows/request below.
        let enabled = sequence_button_enabled(&rt.engine, action, input);
        #[cfg(not(feature = "hitl_mode"))]
        let enabled = enabled && operational_action_allowed(&rt, action);
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
    #[cfg(feature = "hitl_mode")]
    {
        policy
            .controls
            .retain(|c| c.cmd != "ToggleGroundStationControl");
        policy.controls.push(ActionControl {
            cmd: "ToggleGroundStationControl".into(),
            enabled: true,
            blink: BlinkMode::None,
            actuated: Some(rt.ground_station_control),
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
    #[cfg(not(feature = "hitl_mode"))]
    {
        let ready = input.prelaunch && input.interlock && operational_launch_allowed(state, &rt);
        for control in &mut policy.controls {
            if matches!(
                control.cmd.as_str(),
                "Launch" | "GroundStationLaunch" | "RetractPlumbing"
            ) {
                control.enabled = ready
                    && (control.cmd == "RetractPlumbing"
                        || cfg!(feature = "test_fire_mode")
                        || !matches!(
                            rt.operational.qd.status(rt.now()),
                            QdStatus::NotStarted | QdStatus::AckTimedOut
                        ));
                control.blink = if rt.operational.qd.queued() {
                    BlinkMode::Fast
                } else {
                    BlinkMode::None
                };
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
                        #[cfg(not(feature = "hitl_mode"))]
                        {
                            rt.operational
                                .saved
                                .qualification
                                .latch(Fault::MissingTelemetry);
                            rt.operational.running = false;
                            rt.operational.qd = QdWait::default();
                            let _ = rt.operational.persist();
                        }
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
                log::info!(
                    "GSE command queued phase={:?} valve={} requested={} type={} payload={:02x}",
                    state.gse.lock().unwrap().engine.status.phase,
                    ["Pilot", "Vent", "Dump", "Nitrogen", "Nitrous"][index],
                    if open { "open" } else { "closed" },
                    if index < 3 {
                        "VALVE_COMMAND"
                    } else {
                        "ACTUATOR_COMMAND"
                    },
                    payload,
                );
                state.set_pending_umbilical_valve_state(KEYS[index], open);
            }
            crate::telemetry_task::flush_command_tx(router, "GSE sequence");
        } else {
            let mut rt = state.gse.lock().unwrap();
            let now = rt.now();
            #[cfg(not(feature = "hitl_mode"))]
            {
                rt.operational
                    .saved
                    .qualification
                    .latch(Fault::TopologyLost);
                rt.operational.running = false;
                let _ = rt.operational.persist();
            }
            rt.engine
                .fault(now, "No hardware command router is available");
            drop(rt);
            state.add_notification("GSE sequence failed: no hardware command router is available");
            return;
        }
    }
    if let Some(message) = effects.notification {
        // Completed state transitions are informational, not unresolved alarms.
        // Keep faults persistent until acknowledged, but do not replay ordinary
        // pause/cancel/pass/self-test notices every time a client reconnects.
        let persistent = state.gse.lock().unwrap().engine.status.phase == Phase::Fault;
        state.add_notification_with_persistence(message, persistent);
    }
}
pub fn handle_command(state: &Arc<AppState>, cmd: &TelemetryCommand) -> bool {
    #[cfg(not(feature = "hitl_mode"))]
    if handle_operational_command(state, cmd) {
        return true;
    }
    #[cfg(feature = "hitl_mode")]
    if matches!(cmd, TelemetryCommand::ToggleGroundStationControl) {
        let enabled = {
            let mut rt = state.gse.lock().unwrap();
            rt.ground_station_control = !rt.ground_station_control;
            rt.fill_cutoff.reset();
            rt.ground_station_control
        };
        state.add_temporary_notification(if enabled {
            "Ground station automatic fill cutoff enabled"
        } else {
            "Ground station automatic fill cutoff disabled; operator must stop filling"
        });
        crate::sequences::refresh_action_policy_now(state);
        return true;
    }
    if let Some(action) = action(cmd) {
        let is_prelaunch = prelaunch(state);
        let policy = state.action_policy_snapshot();
        let nitrous_target = state.fill_targets_snapshot().nitrous.target_pressure_psi;
        let result = {
            let mut rt = state.gse.lock().unwrap();
            #[cfg(not(feature = "hitl_mode"))]
            if !operational_action_allowed(&rt, action) {
                rt.engine.status.message =
                    "Complete prerequisites in order and wait for stable fill-system routes".into();
                return true;
            }
            if action == Action::StartFill && !rt.engine.active() {
                rt.engine.nitrous_target_psi = nitrous_target;
            }
            let input = rt.input(is_prelaunch, interlock(state, &policy));
            if action == Action::StartFill
                && rt.ground_station_control
                && rt.fresh_mass(input.now_ms).is_none()
            {
                Err("Automatic fill requires fresh calibrated fill weight before starting".into())
            } else {
                let result = rt.engine.request(action, input);
                #[cfg(not(feature = "hitl_mode"))]
                if result.is_ok() {
                    rt.operational.running = true;
                    rt.operational.run_generation = rt.operational.saved.qualification.generation();
                    rt.operational.target_reached = None;
                    if action == Action::StartFill {
                        if let Err(error) = rt.operational.begin_fill() {
                            rt.operational.running = false;
                            let now = rt.now();
                            return {
                                let effects = rt
                                    .engine
                                    .fault(now, &format!("Fill stage could not be saved: {error}"));
                                drop(rt);
                                apply(state, effects);
                                true
                            };
                        }
                    }
                }
                result
            }
        };
        match result {
            Ok(effects) => apply(state, effects),
            Err(err) => {
                state.gse.lock().unwrap().engine.status.message = err.clone();
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
        #[cfg(feature = "hitl_mode")]
        state.gse.lock().unwrap().engine.manual_override();
    }
    false
}
pub fn tick(state: &Arc<AppState>, interlock: bool) {
    #[cfg(not(feature = "hitl_mode"))]
    if operational_tick(state, interlock) {
        return;
    }
    let is_prelaunch = prelaunch(state);
    let target = state.fill_targets_snapshot().nitrous.target_mass_kg;
    let effects = {
        let mut rt = state.gse.lock().unwrap();
        let input = rt.input(is_prelaunch, interlock);
        #[cfg(not(feature = "hitl_mode"))]
        let previous_phase = rt.engine.status.phase;
        let effects = rt.engine.tick(input);
        #[cfg(not(feature = "hitl_mode"))]
        complete_operational_milestones(&mut rt, previous_phase);
        if rt.engine.status.phase != Phase::Filling || !rt.ground_station_control {
            rt.fill_cutoff.reset();
            effects
        } else {
            let mass = rt.fresh_mass(input.now_ms);
            match (mass, input.pressure) {
                (Some(m), Some(p)) => {
                    if rt
                        .fill_cutoff
                        .reached(Instant::now(), m.value, p.value, target)
                    {
                        #[cfg(not(feature = "hitl_mode"))]
                        if target.is_finite() && target > 0.0 && m.value >= target {
                            rt.operational.target_reached = Some(target);
                        }
                        match rt.engine.request(Action::PauseFill, input) {
                            Ok(mut stopped) => {
                                stopped.notification = Some("Automatic fill cutoff: target weight reached or fill stabilized. Closing supplies and vent; waiting for valve acknowledgements.".into());
                                stopped
                            }
                            Err(_) => effects,
                        }
                    } else {
                        effects
                    }
                }
                _ => rt.engine.fault(
                    input.now_ms,
                    "Automatic fill stopped: fresh calibrated fill weight is required",
                ),
            }
        }
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
        .route(
            "/api/gse/cold-flow/override",
            axum::routing::post(override_cold_flow),
        )
        .route("/api/gse/abort/clear", axum::routing::post(clear_abort))
        .route(
            "/api/gse/qualification/reset",
            axum::routing::post(reset_qualification),
        )
        .route(
            "/api/gse/self-test-confirmation",
            axum::routing::post(self_test_confirmation),
        )
}
#[derive(serde::Deserialize, serde::Serialize)]
struct SelfTestConfirmation {
    confirmed: bool,
}
async fn self_test_confirmation(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<SelfTestConfirmation>,
) -> Response {
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::SendCommands).await {
            Ok(p) => p,
            Err(e) => return e,
        };
    if !principal.allows_command_name("ValveSelfTest") {
        return StatusCode::FORBIDDEN.into_response();
    }
    {
        let mut rt = state.gse.lock().unwrap();
        if rt.engine.active() || (request.confirmed && rt.engine.status.self_test_locked) {
            return (StatusCode::CONFLICT,"Self-test confirmation is locked during a sequence or after nitrogen testing starts").into_response();
        }
        rt.engine.config.dry_self_test_confirmed = request.confirmed;
    }
    crate::sequences::refresh_action_policy_now(&state);
    Json(request).into_response()
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
        #[cfg(not(feature = "hitl_mode"))]
        {
            let mut before = rt.engine.config.clone();
            let mut after = config.clone();
            before.dry_self_test_confirmed = false;
            after.dry_self_test_confirmed = false;
            before.grouped_panel = after.grouped_panel;
            if before != after
                && rt
                    .operational
                    .saved
                    .qualification
                    .completed(Milestone::SelfTest)
            {
                return Err("Reset operation checks before changing test pressure limits".into());
            }
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
        #[cfg(feature = "hitl_mode")]
        {
            rt.engine.status.nitrogen_passed = false;
        }
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
    let flight_state = *state.state.lock().unwrap();
    let prelaunch = prelaunch_flight_state(flight_state);
    let interlock = interlock(&state, &state.action_policy_snapshot());
    let mut response = serde_json::to_value(state.gse.lock().unwrap().engine.status.clone())
        .expect("GSE status is serializable");
    response["request_gate"] = serde_json::json!({
        "hitl_mode": cfg!(feature = "hitl_mode"),
        "flight_state": flight_state,
        "prelaunch": prelaunch,
        "button_interlock_satisfied": interlock,
    });
    response["dry_self_test_confirmed"] = serde_json::json!(
        state
            .gse
            .lock()
            .unwrap()
            .engine
            .config
            .dry_self_test_confirmed
    );
    response["configuration_error"] =
        serde_json::json!(state.gse.lock().unwrap().engine.config.validate().err());
    response["physical_panel"] = crate::gpio_panel::panel_indicators(&state);
    #[cfg(not(feature = "hitl_mode"))]
    {
        let rt = state.gse.lock().unwrap();
        response["qualification"] =
            serde_json::to_value(&rt.operational.saved.qualification).unwrap();
        response["mission_stage"] =
            serde_json::to_value(rt.operational.saved.qualification.stage()).unwrap();
        response["topology_stable"] = serde_json::json!(rt.operational.topology_stable);
        response["storage_error"] = serde_json::json!(rt.operational.persist_error);
        response["launch_queued"] = serde_json::json!(rt.operational.qd.queued());
        response["qd"] = serde_json::json!(format!("{:?}", rt.operational.qd.status(rt.now())));
    }
    Json(response).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn clear_abort_rejects_non_aborted_and_active_sequences() {
        let state = crate::state::tests::test_app_state().await;
        for phase in [
            FlightState::Idle,
            FlightState::NitrousFill,
            FlightState::Launch,
            FlightState::Ascent,
        ] {
            *state.state.lock().unwrap() = phase;
            assert_eq!(
                clear_ground_abort(&state).unwrap_err().0,
                StatusCode::CONFLICT
            );
            assert_eq!(state.local_flight_state_snapshot(), phase);
        }
        *state.state.lock().unwrap() = FlightState::Aborted;
        state.gse.lock().unwrap().engine.status.phase = Phase::BaselineSetup;
        assert_eq!(
            clear_ground_abort(&state).unwrap_err().0,
            StatusCode::CONFLICT
        );
        assert_eq!(state.local_flight_state_snapshot(), FlightState::Aborted);
    }

    #[tokio::test]
    async fn clear_abort_publishes_idle_without_output_commands() {
        let (state, sent, peer) = button_test_state().await;
        *state.state.lock().unwrap() = FlightState::Aborted;
        crate::network_variables::set_flight_state(
            state.topology_router.get().unwrap(),
            FlightState::Aborted as u8,
        )
        .unwrap();
        let clock = state.launch_clock_snapshot();
        #[cfg(not(feature = "hitl_mode"))]
        let path = std::env::temp_dir().join(format!("gs-clear-abort-{}.json", std::process::id()));
        {
            let mut rt = state.gse.lock().unwrap();
            rt.engine.fault(0, "Operator abort");
            rt.engine.config.dry_self_test_confirmed = true;
            #[cfg(not(feature = "hitl_mode"))]
            {
                rt.operational.path = Some(path.clone());
                rt.operational
                    .saved
                    .qualification
                    .latch(Fault::TopologyLost);
            }
        }
        clear_ground_abort(&state).unwrap();
        for _ in 0..10 {
            tick(&state, true);
        }
        pump_test_link(&state, &peer);
        let cached = state
            .topology_router
            .get()
            .unwrap()
            .cached_managed_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
            .expect("reconnecting nodes need a replacement managed value");
        assert_eq!(cached.payload(), &[FlightState::Idle as u8]);
        assert_eq!(state.local_flight_state_snapshot(), FlightState::Idle);
        // The persisted default store is process-global and other parallel
        // tests legitimately update it through their independent AppStates.
        // The router cache above is this station's reconnection contract.
        assert_eq!(state.launch_clock_snapshot(), clock);
        assert!(
            sent.lock().unwrap().is_empty(),
            "clearing must not actuate outputs"
        );
        let rt = state.gse.lock().unwrap();
        assert_eq!(rt.engine.status.phase, Phase::Idle);
        assert!(!rt.engine.config.dry_self_test_confirmed);
        #[cfg(not(feature = "hitl_mode"))]
        {
            let saved = operational::Saved::load(&path).unwrap();
            assert!(saved.qualification.fault().is_none());
            assert!(!saved.qualification.completed(Milestone::SelfTest));
            assert_eq!(saved.qualification.generation(), 1);
            assert!(rt.operational.startup_configured);
            std::fs::remove_file(path).unwrap();
        }
    }

    pub(super) type TestCommands = Arc<std::sync::Mutex<Vec<(String, Vec<u8>)>>>;

    pub(super) async fn button_test_state()
    -> (Arc<AppState>, TestCommands, Arc<sedsnet::router::Router>) {
        let state = crate::state::tests::test_app_state().await;
        *state.state.lock().unwrap() = FlightState::Idle;
        let sent: TestCommands = Default::default();
        let endpoints = [
            ("VALVE_BOARD", "VALVE_COMMAND"),
            ("ACTUATOR_BOARD", "ACTUATOR_COMMAND"),
        ]
        .map(|(endpoint, name)| {
            let sent = sent.clone();
            sedsnet::router::EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint(endpoint),
                move |packet: &sedsnet::packet::Packet| {
                    sent.lock()
                        .unwrap()
                        .push((name.into(), packet.payload().to_vec()));
                    Ok(())
                },
            )
        });
        let peer = Arc::new(sedsnet::router::Router::new(
            sedsnet::router::RouterConfig::new(endpoints).with_sender("GSE_TEST"),
        ));
        let router = Arc::new(sedsnet::router::Router::new(
            sedsnet::router::RouterConfig::new([]).with_sender("GS"),
        ));
        let ground_weak = Arc::downgrade(&router);
        let ground_ingress = Arc::new(std::sync::OnceLock::new());
        let ingress = ground_ingress.clone();
        let options = sedsnet::router::RouterSideOptions {
            // Match the deployed CAN/I2C and RF sides: no hop ACKs;
            // ordered end-to-end command ACKs still drive this test link.
            reliable_enabled: false,
            link_local_enabled: true,
            ..Default::default()
        }
        .with_small_packet_transport(128);
        let peer_side = peer.add_side_packed_with_options(
            "test_ground",
            move |bytes| {
                ground_weak
                    .upgrade()
                    .unwrap()
                    .rx_packed_from_side(bytes, *ingress.get().unwrap())
            },
            options.clone(),
        );
        let receiver = peer.clone();
        let ground_side = router.add_side_packed_with_options(
            "test_fill",
            move |bytes| receiver.rx_packed_from_side(bytes, peer_side),
            options,
        );
        ground_ingress.set(ground_side).unwrap();
        peer.announce_discovery().unwrap();
        peer.process_all_queues_with_timeout(0).unwrap();
        state.topology_router.set(router).unwrap();
        {
            let mut rt = state.gse.lock().unwrap();
            rt.engine.config.maximum_zero_offset_psi = Some(8.0);
            rt.observe_pressure(Some(0.0));
            rt.observe_mass(Some(0.0));
        }
        (state, sent, peer)
    }

    pub(super) fn pump_test_link(state: &AppState, peer: &sedsnet::router::Router) {
        // Ordered command delivery requires the receiver's protocol ACK queue
        // to be serviced too, just as the board's telemetry worker does.
        for _ in 0..20 {
            peer.process_all_queues_with_timeout(0).unwrap();
            state
                .topology_router
                .get()
                .unwrap()
                .process_all_queues_with_timeout(0)
                .unwrap();
        }
    }

    pub(super) fn confirm_test_valves(state: &AppState, valves: [bool; 5]) {
        let mut rt = state.gse.lock().unwrap();
        for (key, open) in KEYS.into_iter().zip(valves) {
            rt.observe_valve(key, open);
        }
        rt.observe_pressure(Some(0.0));
        rt.observe_mass(Some(0.0));
    }

    #[tokio::test]
    #[cfg(feature = "hitl_mode")]
    async fn nitrogen_button_enters_baseline_and_requests_safe_valve_configuration() {
        let (state, sent, peer) = button_test_state().await;
        // Decode the command name emitted by the frontend button.
        let cmd: TelemetryCommand = serde_json::from_str("\"NitrogenTest\"").unwrap();
        assert_eq!(command_allowed(&state, &cmd), Some(true));
        assert!(handle_command(&state, &cmd));
        pump_test_link(&state, &peer);
        assert_eq!(
            state.gse.lock().unwrap().engine.status.phase,
            Phase::BaselineSetup
        );
        assert_eq!(
            *sent.lock().unwrap(),
            vec![
                ("ACTUATOR_COMMAND".into(), vec![12]),
                ("ACTUATOR_COMMAND".into(), vec![13]),
                ("VALVE_COMMAND".into(), vec![3]),
                ("VALVE_COMMAND".into(), vec![1]),
                ("VALVE_COMMAND".into(), vec![2]),
            ]
        );
        for (key, expected) in KEYS.into_iter().zip(gse_sequence::RELIEVED) {
            assert_eq!(state.get_pending_umbilical_valve_state(key), Some(expected));
        }
        confirm_test_valves(&state, gse_sequence::RELIEVED);
        tick(&state, true);
        assert_eq!(
            state.gse.lock().unwrap().engine.status.phase,
            Phase::Baseline
        );
    }

    #[tokio::test]
    #[cfg(feature = "hitl_mode")]
    async fn fill_button_requires_nitrogen_pass_then_opens_only_after_confirmations_and_stops_at_mass()
     {
        let (state, sent, peer) = button_test_state().await;
        let cmd: TelemetryCommand = serde_json::from_str("\"StartFill\"").unwrap();
        assert!(handle_command(&state, &cmd));
        pump_test_link(&state, &peer);
        assert_eq!(state.gse.lock().unwrap().engine.status.phase, Phase::Idle);
        assert!(state.get_pending_umbilical_valve_state(10).is_none());
        assert!(sent.lock().unwrap().is_empty());
        {
            // The full nitrogen progression is covered by the sequence-engine test.
            let mut rt = state.gse.lock().unwrap();
            rt.engine.status.phase = Phase::Passed;
            rt.engine.status.nitrogen_passed = true;
            rt.engine.status.baseline = Some(gse_sequence::Baseline {
                average_psi: 0.0,
                min_psi: 0.0,
                max_psi: 0.0,
                noise_psi: 0.1,
                samples: 50,
            });
        }
        assert!(handle_command(&state, &cmd));
        pump_test_link(&state, &peer);
        {
            let rt = state.gse.lock().unwrap();
            assert_eq!(rt.engine.status.phase, Phase::FillSetup);
            assert_eq!(
                rt.engine.nitrous_target_psi,
                state.fill_targets_snapshot().nitrous.target_pressure_psi
            );
        }
        assert_eq!(state.get_pending_umbilical_valve_state(10), Some(false));
        tick(&state, true);
        assert_eq!(
            state.gse.lock().unwrap().engine.status.phase,
            Phase::FillSetup
        );
        confirm_test_valves(&state, gse_sequence::FILL_READY);
        tick(&state, true);
        pump_test_link(&state, &peer);
        assert_eq!(
            state.gse.lock().unwrap().engine.status.phase,
            Phase::Filling
        );
        assert_eq!(state.get_pending_umbilical_valve_state(10), Some(true));
        assert!(
            sent.lock()
                .unwrap()
                .contains(&("ACTUATOR_COMMAND".into(), vec![10]))
        );
        let target = state.fill_targets_snapshot().nitrous.target_mass_kg;
        state.gse.lock().unwrap().observe_mass(Some(target));
        tick(&state, true);
        pump_test_link(&state, &peer);
        assert_eq!(
            state.gse.lock().unwrap().engine.status.phase,
            Phase::PauseSetup
        );
        assert_eq!(state.get_pending_umbilical_valve_state(10), Some(false));
        assert!(
            sent.lock()
                .unwrap()
                .iter()
                .rev()
                .take(5)
                .any(|(kind, data)| kind == "ACTUATOR_COMMAND" && data == &[13])
        );
        confirm_test_valves(&state, gse_sequence::CLOSED);
        tick(&state, true);
        assert_eq!(state.gse.lock().unwrap().engine.status.phase, Phase::Paused);
    }

    #[tokio::test]
    #[cfg(feature = "hitl_mode")]
    async fn button_rejections_are_visible_and_do_not_enqueue_valve_actions() {
        for cmd in [TelemetryCommand::NitrogenTest, TelemetryCommand::StartFill] {
            let (state, sent, peer) = button_test_state().await;
            state
                .gse
                .lock()
                .unwrap()
                .engine
                .config
                .maximum_zero_offset_psi = None;
            assert!(handle_command(&state, &cmd));
            pump_test_link(&state, &peer);
            let rt = state.gse.lock().unwrap();
            assert_eq!(rt.engine.status.phase, Phase::Idle);
            assert!(rt.engine.status.message.contains("empty-tank PT offset"));
            drop(rt);
            for key in KEYS {
                assert!(state.get_pending_umbilical_valve_state(key).is_none());
            }
            assert!(sent.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn fill_nitrogen_and_self_test_commands_remain_registered_in_every_mode() {
        for (name, command, expected) in [
            ("StartFill", TelemetryCommand::StartFill, Action::StartFill),
            ("PauseFill", TelemetryCommand::PauseFill, Action::PauseFill),
            (
                "CancelFill",
                TelemetryCommand::CancelFill,
                Action::CancelFill,
            ),
            (
                "NitrogenTest",
                TelemetryCommand::NitrogenTest,
                Action::NitrogenTest,
            ),
            (
                "ValveSelfTest",
                TelemetryCommand::ValveSelfTest,
                Action::SelfTest,
            ),
        ] {
            assert!(ACTIONS.contains(&(name, expected)));
            assert_eq!(action(&command), Some(expected));
        }
    }

    #[test]
    fn hitl_covers_all_prelaunch_states_but_never_flight_or_recovery() {
        for state in [
            FlightState::Idle,
            FlightState::PreFill,
            FlightState::FillTest,
            FlightState::NitrogenFill,
            FlightState::NitrousFill,
        ] {
            assert!(prelaunch_flight_state(state));
        }
        assert!(prelaunch_flight_state(FlightState::Armed));
        assert_eq!(
            prelaunch_flight_state(FlightState::Startup),
            cfg!(feature = "hitl_mode")
        );
        for state in [
            FlightState::Launch,
            FlightState::Ascent,
            FlightState::Coast,
            FlightState::Apogee,
            FlightState::ParachuteDeploy,
            FlightState::Descent,
            FlightState::Landed,
            FlightState::Recovery,
            FlightState::Aborted,
        ] {
            assert!(!prelaunch_flight_state(state));
        }
        #[cfg(feature = "hitl_mode")]
        for (_, action) in ACTIONS {
            let rt = Runtime::default();
            assert!(!sequence_button_enabled(
                &rt.engine,
                action,
                rt.input(false, true)
            ));
        }
    }

    #[test]
    fn automatic_fill_defaults_on_and_rejects_stale_or_invalid_weight() {
        let mut rt = Runtime::default();
        assert!(rt.ground_station_control);
        assert!(rt.fresh_mass(0).is_none());
        rt.observe_mass(Some(f32::NAN));
        assert!(rt.mass.is_none());
        rt.mass = Some(Sample {
            value: 10.0,
            at_ms: 100,
        });
        assert!(rt.fresh_mass(2100).is_some());
        assert!(rt.fresh_mass(2101).is_none());
        assert!(rt.fresh_mass(99).is_none());
    }

    #[test]
    fn button_availability_does_not_bypass_sequence_safety() {
        let mut rt = Runtime::default();
        let input = rt.input(true, true);
        for (_, action) in ACTIONS {
            #[cfg(feature = "hitl_mode")]
            assert_eq!(
                sequence_button_enabled(&rt.engine, action, input),
                action != Action::SelfTest
            );
            #[cfg(not(feature = "hitl_mode"))]
            assert_eq!(
                sequence_button_enabled(&rt.engine, action, input),
                rt.engine.allows(action, input)
            );
        }
        let before = rt.engine.status.phase;
        assert!(
            rt.engine
                .request(Action::NitrogenTest, input)
                .unwrap_err()
                .contains("empty-tank PT offset")
        );
        assert_eq!(rt.engine.status.phase, before);
        rt.engine.config.dry_self_test_confirmed = true;
        #[cfg(feature = "hitl_mode")]
        {
            assert!(sequence_button_enabled(&rt.engine, Action::SelfTest, input));
            rt.engine.status.self_test_locked = true;
            assert!(!sequence_button_enabled(
                &rt.engine,
                Action::SelfTest,
                input
            ));
            for (_, action) in ACTIONS {
                assert!(!sequence_button_enabled(
                    &rt.engine,
                    action,
                    rt.input(true, false)
                ));
            }
        }
    }
}

/// Status receipt is checked before the next sequence tick; never wait five
/// seconds to react to an incompatible supply opening during nitrous fill.
pub fn report_safety(state: &Arc<AppState>) {
    #[cfg(not(feature = "hitl_mode"))]
    check_report_safety(state);
    #[cfg(feature = "hitl_mode")]
    let _ = state;
}
pub fn qd_confirmation(state: &AppState, sender: &str, key: u8, open: bool, source_ms: u64) {
    #[cfg(not(feature = "hitl_mode"))]
    if sender == crate::types::Board::ActuatorBoard.sender_id()
        && key == crate::rocket_commands::ActuatorBoardCommands::RetractPlumbing as u8
        && open
    {
        let mut rt = state.gse.lock().unwrap();
        if rt
            .operational
            .qd_dispatch_utc_ms
            .is_some_and(|sent| source_ms > sent)
        {
            let now = rt.now();
            rt.operational.qd.confirm(now);
        }
    }
    #[cfg(feature = "hitl_mode")]
    let _ = (state, sender, key, open, source_ms);
}
/// Operator recovery only: never dispatch a valve, ignition or launch command.
fn clear_ground_abort(state: &Arc<AppState>) -> Result<(), (StatusCode, String)> {
    let mut rt = state.gse.lock().unwrap();
    if *state.state.lock().unwrap() != FlightState::Aborted {
        return Err((StatusCode::CONFLICT, "GroundStation is not aborted".into()));
    }
    if rt.engine.active() {
        return Err((
            StatusCode::CONFLICT,
            "Stop the active sequence first".into(),
        ));
    }
    #[cfg(not(feature = "hitl_mode"))]
    {
        if rt.operational.running {
            return Err((
                StatusCode::CONFLICT,
                "Stop the active sequence first".into(),
            ));
        }
        // Invalidate delayed completions and require explicit requalification.
        rt.operational
            .saved
            .qualification
            .reset()
            .map_err(|e| (StatusCode::CONFLICT, e.into()))?;
        rt.operational.saved.baseline = None;
        rt.operational.saved.fill_target_kg = None;
        rt.operational.saved.pending_checklists.clear();
        rt.operational
            .persist()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
        crate::checklists::reset_sequence_completions()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
        rt.operational.persist_error = None;
        rt.operational.qd = QdWait::default();
        rt.operational.qd_dispatch_utc_ms = None;
        // Do not restart automatic boot valve setup during abort recovery.
        // The next explicit self-test establishes and checks its own baseline.
        rt.operational.startup_configured = true;
    }
    let router = state.topology_router.get().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "Network is not initialized".into(),
    ))?;
    // Update the persisted managed value as well as the local/UI state, so
    // discovery and reconnection cannot replay this station's old abort value.
    crate::network_variables::set_flight_state(router, FlightState::Idle as u8)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let config = rt.engine.config.clone();
    rt.engine = Engine::default();
    rt.engine.config = config;
    rt.engine.config.dry_self_test_confirmed = false;
    state.set_local_flight_state(FlightState::Idle);
    drop(rt);
    crate::sequences::refresh_action_policy_now(state);
    state.broadcast_action_policy_snapshot();
    Ok(())
}

async fn clear_abort(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::SendCommands).await {
            Ok(p) => p,
            Err(error) => return error,
        };
    if !principal.allows_command_name("ValveSelfTest") {
        return StatusCode::FORBIDDEN.into_response();
    }
    match clear_ground_abort(&state) {
        Ok(()) => Json(serde_json::json!({"cleared": true})).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn reset_qualification(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::SendCommands).await {
            Ok(p) => p,
            Err(error) => return error,
        };
    if !principal.allows_command_name("ValveSelfTest") {
        return StatusCode::FORBIDDEN.into_response();
    }
    #[cfg(feature = "hitl_mode")]
    {
        return (
            StatusCode::CONFLICT,
            "Operational qualification is not used in HITL",
        )
            .into_response();
    }
    #[cfg(not(feature = "hitl_mode"))]
    {
        if !matches!(
            *state.state.lock().unwrap(),
            FlightState::Startup | FlightState::Idle
        ) {
            return (
                StatusCode::CONFLICT,
                "Return to Idle before resetting qualification",
            )
                .into_response();
        }
        let mut rt = state.gse.lock().unwrap();
        if rt.operational.running {
            return (StatusCode::CONFLICT, "Stop the active sequence first").into_response();
        }
        if let Err(error) = rt.operational.saved.qualification.reset() {
            return (StatusCode::CONFLICT, error).into_response();
        }
        rt.operational.saved.pending_checklists.clear();
        rt.operational.saved.baseline = None;
        rt.operational.saved.fill_target_kg = None;
        rt.operational.persist_error = None;
        rt.operational.qd = QdWait::default();
        let config = rt.engine.config.clone();
        rt.engine = Engine::default();
        rt.engine.config = config;
        rt.engine.config.dry_self_test_confirmed = false;
        rt.operational.startup_configured = false;
        if let Err(error) = rt.operational.persist() {
            return (StatusCode::INTERNAL_SERVER_ERROR, error).into_response();
        }
        if let Err(error) = crate::checklists::reset_sequence_completions() {
            rt.operational.persist_error = Some(error.clone());
            return (StatusCode::INTERNAL_SERVER_ERROR, error).into_response();
        }
        drop(rt);
        crate::sequences::refresh_action_policy_now(&state);
        Json(serde_json::json!({"reset":true})).into_response()
    }
}

#[cfg(all(test, not(feature = "hitl_mode")))]
pub(crate) fn set_operational_prerequisites_for_test(state: &AppState) {
    let mut rt = state.gse.lock().unwrap();
    rt.operational.topology_stable = true;
    rt.operational.startup_configured = true;
    rt.operational
        .saved
        .qualification
        .complete(Milestone::SelfTest, 0)
        .unwrap();
}

/// All GroundStation automatic abort producers share this stage boundary.
/// Manual Abort remains a separate operator command.
pub fn automatic_abort_allowed(state: &AppState) -> bool {
    #[cfg(not(feature = "hitl_mode"))]
    if state
        .gse
        .lock()
        .unwrap()
        .operational
        .saved
        .qualification
        .stage()
        == gse_sequence::qualification::MissionStage::Fire
    {
        return false;
    }
    cfg!(feature = "hitl_mode")
        || matches!(
            *state.state.lock().unwrap(),
            FlightState::Startup
                | FlightState::Idle
                | FlightState::PreFill
                | FlightState::FillTest
                | FlightState::NitrogenFill
                | FlightState::NitrousFill
                | FlightState::Armed
        )
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ColdFlowOverrideRequest {
    reason: String,
}
async fn override_cold_flow(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<ColdFlowOverrideRequest>,
) -> Response {
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::SendCommands).await {
            Ok(principal) => principal,
            Err(error) => return error,
        };
    if principal.anonymous || !principal.allows_command_name("NitrogenTest") {
        return StatusCode::FORBIDDEN.into_response();
    }
    #[cfg(feature = "hitl_mode")]
    {
        let _ = request.reason;
        (
            StatusCode::CONFLICT,
            "Operational cold-flow overrides are not used in HITL",
        )
            .into_response()
    }
    #[cfg(not(feature = "hitl_mode"))]
    {
        match apply_cold_flow_override(
            &state,
            principal.username.as_deref().unwrap_or(""),
            &request.reason,
        ) {
            Ok(()) => {
                crate::sequences::refresh_action_policy_now(&state);
                Json(serde_json::json!({"overridden":true})).into_response()
            }
            Err(error) => (StatusCode::CONFLICT, error).into_response(),
        }
    }
}
#[cfg(not(feature = "hitl_mode"))]
fn apply_cold_flow_override(
    state: &Arc<AppState>,
    operator: &str,
    reason: &str,
) -> Result<(), String> {
    if !matches!(
        *state.state.lock().unwrap(),
        FlightState::Startup | FlightState::Idle
    ) {
        return Err("Return to Idle before overriding cold flow".into());
    }
    let mut rt = state.gse.lock().unwrap();
    let now = rt.now();
    if rt.operational.running
        || rt.engine.active()
        || !rt.operational.topology_stable
        || !rt.operational.startup_configured
        || rt.operational.persist_error.is_some()
        || !interlock(state, &state.action_policy_snapshot())
    {
        return Err(
            "Override requires stable connections, interlock and no active sequence".into(),
        );
    }
    let baseline = rt
        .operational
        .saved
        .baseline
        .clone()
        .ok_or("Run the valve self-test to record an empty-tank baseline first")?;
    if !rt
        .pressure
        .is_some_and(|p| p.at_ms <= now && now - p.at_ms <= 2000 && baseline.is_zero(p.value))
        || ![3, 4].into_iter().all(|i| {
            rt.valves[i].is_some_and(|v| !v.open && v.at_ms <= now && now - v.at_ms <= 2000)
        })
    {
        return Err(
            "Override requires fresh zero-band pressure and both supply valves confirmed closed"
                .into(),
        );
    }
    rt.operational
        .override_cold_flow(gse_sequence::qualification::ColdFlowOverride {
            operator: operator.into(),
            reason: reason.into(),
            at_ms: crate::telemetry_task::get_current_timestamp_ms() as i64,
        })?;
    rt.engine.status.nitrogen_overridden = true;
    rt.engine.status.nitrogen_passed = false;
    rt.engine.status.baseline = Some(baseline);
    rt.engine.status.phase = Phase::Passed;
    rt.engine.status.self_test_locked = true;
    rt.engine.claimed = true;
    rt.engine.status.message =
        "Cold-flow requirement overridden by operator; fresh fill interlocks still apply".into();
    Ok(())
}
/// Commit once before dispatch, so service restart never repeats an irreversible Fire intent.
#[cfg(not(feature = "hitl_mode"))]
pub(crate) fn commit_fire_intent(state: &Arc<AppState>) -> Result<(), String> {
    let mut rt = state.gse.lock().unwrap();
    if !prelaunch(state)
        || !operational_launch_allowed(state, &rt)
        || !interlock(state, &state.action_policy_snapshot())
    {
        return Err("Fire prerequisites changed; launch was not dispatched".into());
    }
    if !cfg!(feature = "test_fire_mode")
        && !matches!(rt.operational.qd.status(rt.now()), QdStatus::Ready)
    {
        return Err("Quick disconnect is not acknowledged and settled".into());
    }
    rt.operational.commit_fire()?;
    rt.engine.claimed = true;
    // From this point even a checklist-storage failure must not re-arm the operation.
    let checklist_result = rt.operational.flush_checklists();
    drop(rt);
    state.set_local_flight_state(FlightState::Launch);
    checklist_result.map_err(|error| {
        format!("Fire intent saved, but checklist update failed; manual recovery required: {error}")
    })
}

#[cfg(all(test, not(feature = "hitl_mode")))]
pub(crate) fn armed_fixture_for_test(
    state: &Arc<AppState>,
    router: &Arc<sedsnet::router::Router>,
) -> std::path::PathBuf {
    use crate::types::Board;
    state.set_local_flight_state(FlightState::Armed);
    let side = router.add_side_packed("test_fixture_other_boards", |_| Ok(()));
    let mut endpoints = vec![crate::telemetry_schema::endpoint("ACTUATOR_BOARD")];
    if !cfg!(feature = "test_fire_mode") {
        endpoints.push(crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"));
    }
    let announce = sedsnet::discovery::build_discovery_announce("GB", 123, &endpoints).unwrap();
    router
        .rx_packed_from_side(&sedsnet::wire_format::pack_packet(&announce), side)
        .unwrap();
    router.process_all_queues_with_timeout(0).unwrap();
    if state.topology_router.get().is_none() {
        state.topology_router.set(router.clone()).unwrap();
    }
    for board in [
        Board::GatewayBoard,
        Board::ValveBoard,
        Board::ActuatorBoard,
        Board::DaqBoard,
    ] {
        state.mark_board_seen(
            board.sender_id(),
            crate::telemetry_task::get_current_timestamp_ms(),
        );
    }
    if !cfg!(feature = "test_fire_mode") {
        state.mark_board_seen("FC", crate::telemetry_task::get_current_timestamp_ms());
        crate::antenna::observe(&crate::types::TelemetryRow {
            timestamp_ms: crate::telemetry_task::get_current_timestamp_ms() as i64,
            data_type: "GPS_DATA".into(),
            sender_id: "RF".into(),
            values: vec![Some(43.0), Some(-78.0), Some(150.0)],
        });
    }
    let path =
        std::env::temp_dir().join(format!("gse-armed-fixture-{}.json", rand::random::<u128>()));
    let target = state.fill_targets_snapshot().nitrous.target_mass_kg;
    let mut rt = state.gse.lock().unwrap();
    rt.clock = Instant::now() - std::time::Duration::from_secs(10);
    rt.operational.path = Some(path.clone());
    rt.operational.topology_stable = true;
    rt.operational.startup_configured = true;
    rt.operational.saved.baseline = Some(gse_sequence::Baseline {
        average_psi: 0.0,
        min_psi: 0.0,
        max_psi: 0.0,
        noise_psi: 0.1,
        samples: 50,
    });
    for milestone in [
        Milestone::SelfTest,
        Milestone::NitrogenTest,
        Milestone::FillTarget,
    ] {
        rt.operational
            .saved
            .qualification
            .complete(milestone, 0)
            .unwrap();
    }
    rt.operational.saved.fill_target_kg = Some(target);
    rt.operational.qd.dispatched(0);
    rt.operational.qd.confirm(1);
    rt.operational.persist().unwrap();
    rt.engine.status.phase = Phase::Paused;
    rt.observe_pressure(Some(0.0));
    rt.observe_mass(Some(target));
    for key in KEYS {
        rt.observe_valve(key, false);
    }
    drop(rt);
    let mut policy = state.action_policy_snapshot();
    policy.key_enabled = true;
    policy.software_buttons_enabled = true;
    state.set_action_policy(policy);
    path
}

#[cfg(test)]
mod mission_calibration_guard_tests {
    use super::*;
    #[cfg(not(feature = "hitl_mode"))]
    #[test]
    fn imported_calibration_waits_for_idle_and_explicit_qualification_reset() {
        let mut runtime = Runtime::default();
        assert!(runtime.calibration_import_allowed());
        runtime.engine.status.phase = Phase::BaselineSetup;
        assert!(!runtime.calibration_import_allowed());
        runtime.engine.status.phase = Phase::Idle;
        let generation = runtime.operational.saved.qualification.generation();
        runtime
            .operational
            .saved
            .qualification
            .complete(Milestone::SelfTest, generation)
            .unwrap();
        assert!(!runtime.calibration_import_allowed());
        runtime.operational.saved.qualification.reset().unwrap();
        assert!(runtime.calibration_import_allowed());
    }
    #[cfg(feature = "hitl_mode")]
    #[test]
    fn hitl_does_not_import_paired_calibration() {
        assert!(!Runtime::default().calibration_import_allowed());
    }
}
