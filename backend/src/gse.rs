mod automatic_abort;
#[cfg(test)]
mod rrls_http_tests;
pub(crate) use automatic_abort::claim_automatic_abort;
#[cfg(not(any(feature = "hitl_mode", feature = "test_fire_mode")))]
pub(crate) use automatic_abort::{
    AutomaticAbortClaim, LivenessAbortDecision, claim_liveness_abort,
};
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
use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

// Serialize effect preparation/admission with manual handoff. Transport
// processing happens after this gate is released (callbacks may reenter GSE).
static DISPATCH_GATE: Mutex<()> = Mutex::new(());

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

/// At most two semantic states can be admitted from one valve source tick.
/// A changed packet nonce cannot turn the same capture into a new sample.
/// At most one flip is admitted per source tick: A→B→A is conservatively
/// withheld even if it was a real second flip, because it is indistinguishable
/// from a re-nonced replay of the already-seen A capture.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ValveCaptureHistory {
    source_ms: i64,
    packet_ids: [u64; 2],
    seen_values: u8,
}

pub(crate) struct ValveReceipt {
    pub network_ms: i64,
    pub monotonic_at: Instant,
    pub simulation_ms: Option<u64>,
}

impl ValveCaptureHistory {
    fn admit(
        &mut self,
        source_ms: i64,
        receipt_ms: i64,
        now: u64,
        open: bool,
        packet_id: u64,
        previous_at: Option<u64>,
    ) -> Option<u64> {
        if source_ms < 0 {
            return None;
        }
        let value = usize::from(open);
        let bit = 1_u8 << value;
        let same_tick = self.seen_values != 0 && self.source_ms == source_ms;
        if (same_tick && self.seen_values & bit != 0)
            || (0..2).any(|index| {
                self.seen_values & (1 << index) != 0 && self.packet_ids[index] == packet_id
            })
        {
            return None;
        }
        // Only the duplicate-timestamp part is bypassed for an unseen opposite
        // value. The unchanged UTC age, future, backwards and underflow checks
        // still run before either history or live state is committed.
        let mut previous = if same_tick || self.seen_values == 0 {
            None
        } else {
            Some(self.source_ms)
        };
        let mut at = source_sample_at(Some(source_ms), receipt_ms, now, &mut previous)?;
        if same_tick {
            let previous_at = previous_at?;
            now.checked_sub(previous_at)?;
            at = at.min(previous_at);
        }
        // Boot-time captures cannot be aged from UTC. An equal-tick exception
        // still retains its first capture anchor and the existing two-second
        // freshness bound; a late opposite value cannot rearm confirmation.
        if now.checked_sub(at)? > 2_000 {
            return None;
        }
        if !same_tick {
            self.source_ms = source_ms;
            self.seen_values = 0;
            self.packet_ids = [0; 2];
        }
        self.seen_values |= bit;
        self.packet_ids[value] = packet_id;
        Some(at)
    }
}

pub struct Runtime {
    pub engine: Engine,
    #[cfg(not(feature = "hitl_mode"))]
    operational: operational::Operational,
    clock: Instant,
    pressure: Option<Sample>,
    valves: [Option<ValveState>; 5],
    mass: Option<Sample>,
    last_mass_source_ms: Option<i64>,
    last_pressure_source_ms: Option<i64>,
    last_valve_captures: [ValveCaptureHistory; 5],
    fill_cutoff: crate::sequences::FillCutoff,
    fill_trim: Option<gse_sequence::fill_trim::Controller>,
    ground_station_control: bool,
    manual_control: bool,
    effects_queued: bool,
    #[cfg(not(feature = "hitl_mode"))]
    automatic_abort_in_flight: bool,
}
impl Default for Runtime {
    fn default() -> Self {
        #[cfg(not(any(test, feature = "hitl_mode")))]
        crate::checklists::paired::preflight();
        Self {
            engine: Engine::default(),
            #[cfg(not(feature = "hitl_mode"))]
            operational: Default::default(),
            clock: Instant::now(),
            pressure: None,
            valves: [None; 5],
            mass: None,
            last_mass_source_ms: None,
            last_pressure_source_ms: None,
            last_valve_captures: [ValveCaptureHistory::default(); 5],
            fill_cutoff: Default::default(),
            fill_trim: None,
            ground_station_control: true,
            manual_control: false,
            effects_queued: false,
            #[cfg(not(feature = "hitl_mode"))]
            automatic_abort_in_flight: false,
        }
    }
}
// Valid network UTC captures retain their source age. Unsynchronized boot
// counters retain receipt-time compatibility, but replaying the same capture
// must never manufacture a new settled sample or renew its freshness.
fn source_sample_at(
    source: Option<i64>,
    receipt: i64,
    now: u64,
    previous: &mut Option<i64>,
) -> Option<u64> {
    const UTC_2020_MS: i64 = 1_577_836_800_000;
    let Some(source) = source.filter(|v| *v >= 0) else {
        return Some(now);
    };
    if *previous == Some(source) {
        return None;
    }
    let at = if source >= UTC_2020_MS && receipt >= UTC_2020_MS {
        let delta = receipt.checked_sub(source)?;
        // Network clocks may lead by a small synchronization/receipt jitter.
        // Clamp only a bounded100 ms lead, never a materially future capture.
        if delta < -100 {
            return None;
        }
        let age = delta.max(0) as u64;
        if age > 2_000 || previous.is_some_and(|old| old >= UTC_2020_MS && source < old) {
            return None;
        }
        now.checked_sub(age)?
    } else {
        now
    };
    *previous = Some(source);
    Some(at)
}
fn sequence_clock_ms(elapsed_ms: u64, simulated_ms: Option<u64>) -> u64 {
    simulated_ms.unwrap_or(elapsed_ms)
}

impl Runtime {
    fn cancel_automation_for_manual(&mut self) {
        self.manual_control = true;
        let faulted = self.engine.status.phase == Phase::Fault;
        self.engine.manual_override();
        if faulted {
            self.engine.status.phase = Phase::Fault;
            self.engine.claimed = true;
        }
        self.fill_cutoff.reset();
        if let Some(mut controller) = self.fill_trim.take() {
            controller.cancel();
        }
        #[cfg(not(feature = "hitl_mode"))]
        {
            self.operational.running = false;
            self.operational.target_reached = None;
            self.operational.last_correction_ms = None;
            self.operational.qd = QdWait::default();
        }
    }
    /// Actual valve reports and their source ages; command intent is never evidence.
    pub(crate) fn mirror_valves(&self) -> [Option<(bool, u64)>; 5] {
        let now = self.now();
        self.valves.map(|value| value.map(|v| (v.open, now.saturating_sub(v.at_ms))))
    }

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
    #[cfg(not(feature = "hitl_mode"))]
    pub(crate) fn checklist_definition_edit_allowed(&self) -> bool {
        self.calibration_import_allowed()
            && !self.operational.running
            && self.operational.saved.pending_checklists.is_empty()
            && self.operational.saved.qualification.fault().is_none()
    }
    #[cfg(not(feature = "hitl_mode"))]
    pub(crate) fn checklist_template_bound(&self) -> bool {
        crate::checklists::paired::qualification_allowed(
            self.operational.saved.checklist_digest.as_deref(),
            self.operational.has_checklist_evidence(),
        )
    }
    fn fresh_mass(&self, now_ms: u64) -> Option<Sample> {
        self.mass
            .filter(|m| m.at_ms <= now_ms && now_ms - m.at_ms <= 2000)
    }
    pub fn observe_mass(&mut self, value: Option<f32>) {
        self.last_mass_source_ms = None;
        self.mass = value.filter(|v| v.is_finite()).map(|value| Sample {
            value,
            at_ms: self.now(),
        });
    }
    pub(crate) fn observe_mass_source(
        &mut self,
        value: Option<f32>,
        source_ms: Option<i64>,
        receipt_ms: i64,
    ) {
        if let Some(at_ms) = source_sample_at(
            source_ms,
            receipt_ms,
            self.now(),
            &mut self.last_mass_source_ms,
        ) {
            self.mass = value
                .filter(|v| v.is_finite())
                .map(|value| Sample { value, at_ms });
        }
    }
    pub(crate) fn observe_pressure_source(
        &mut self,
        value: Option<f32>,
        source_ms: Option<i64>,
        receipt_ms: i64,
    ) {
        if let Some(at_ms) = source_sample_at(
            source_ms,
            receipt_ms,
            self.now(),
            &mut self.last_pressure_source_ms,
        ) {
            self.pressure = value
                .filter(|v| v.is_finite())
                .map(|value| Sample { value, at_ms });
        }
    }
    fn now(&self) -> u64 {
        sequence_clock_ms(
            self.clock.elapsed().as_millis() as u64,
            crate::simulation_clock_ms(),
        )
    }
    pub fn observe_pressure(&mut self, value: Option<f32>) {
        self.pressure = value.filter(|p| p.is_finite()).map(|value| Sample {
            value,
            at_ms: self.now(),
        });
    }
    pub(crate) fn observe_valve_source(
        &mut self,
        key: u8,
        open: bool,
        source_ms: i64,
        packet_id: u64,
        receipt: ValveReceipt,
    ) -> Option<u64> {
        let processing_now = self.now();
        self.observe_valve_source_at(key, open, source_ms, packet_id, receipt, processing_now)
    }

    fn observe_valve_source_at(
        &mut self,
        key: u8,
        open: bool,
        source_ms: i64,
        packet_id: u64,
        receipt: ValveReceipt,
        processing_now: u64,
    ) -> Option<u64> {
        let Some(index) = KEYS.iter().position(|candidate| *candidate == key) else {
            return Some(0);
        };
        // Use the receipt clock captured before waiting for the GSE mutex.
        // Processing or lock delay must never renew capture freshness.
        let now = match receipt.simulation_ms {
            Some(now) => now,
            None => u64::try_from(receipt.monotonic_at.checked_duration_since(self.clock)?.as_millis()).ok()?,
        };
        let mut history = self.last_valve_captures[index];
        let at_ms = history.admit(
            source_ms,
            receipt.network_ms,
            now,
            open,
            packet_id,
            self.valves[index].map(|sample| sample.at_ms),
        )?;
        // Waiting for the mutex may have consumed the original freshness
        // budget. Do not acknowledge or poison replay history with a capture
        // that was fresh at receipt but stale (or future) at processing time.
        if processing_now.checked_sub(at_ms)? > 2_000 {
            return None;
        }
        self.last_valve_captures[index] = history;
        self.valves[index] = Some(ValveState { open, at_ms });
        now.checked_sub(at_ms)
    }
    #[cfg(test)]
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
pub(crate) fn persist_settings(config: &Config) -> Result<(), String> {
    config.validate_settings()?;
    let path = config_path();
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
    Ok(())
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
    #[cfg(not(feature = "hitl_mode"))]
    {
        let claimed = {
            let mut rt = state.gse.lock().unwrap();
            let claimed = rt.operational.saved.automatic_abort_claimed;
            if claimed {
                rt.operational.running = false;
                rt.operational.startup_configured = true;
                rt.engine.claimed = true;
                rt.engine.status.phase = Phase::Fault;
                rt.engine.status.message = "Automatic abort remains latched; clear GSE abort before another operation".into();
            }
            claimed
        };
        if claimed { state.set_local_flight_state(FlightState::Aborted); }
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
    let rt = state.gse.lock().unwrap();
    rt.engine.claimed || rt.manual_control
}
pub fn active(state: &AppState) -> bool {
    state.gse.lock().unwrap().engine.active()
}
// A filled-tank hold remains Engine-active after operational Fire is
// committed. Actual pilot feedback must still advance its launch countdown.
fn pilot_clock_allowed(engine_active: bool, fire_committed: bool, hitl_mode: bool) -> bool {
    !engine_active || (!hitl_mode && fire_committed)
}
pub(crate) fn pilot_report_can_advance_launch_clock(state: &AppState) -> bool {
    let runtime = state.gse.lock().unwrap();
    #[cfg(feature = "hitl_mode")]
    let fire_committed = false;
    #[cfg(not(feature = "hitl_mode"))]
    let fire_committed = runtime.operational.saved.qualification.stage()
        == gse_sequence::qualification::MissionStage::Fire;
    pilot_clock_allowed(
        runtime.engine.active(),
        fire_committed,
        cfg!(feature = "hitl_mode"),
    )
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
    // A manual handoff never grants an opening that the retained physical
    // supply interlock forbids. Explicit/toggle closes remain available.
    let now = rt.now();
    let closed = |index: usize| {
        rt.valves[index].is_some_and(|v| !v.open && v.at_ms <= now && now - v.at_ms <= 2_000)
    };
    if matches!(cmd, TelemetryCommand::Nitrogen)
        && !rt.valves[3].is_some_and(|v| v.open)
        && (rt.engine.nitrous_session() || !closed(4))
    {
        return Some(false);
    }
    if matches!(cmd, TelemetryCommand::Nitrous)
        && !rt.valves[4].is_some_and(|v| v.open)
        && !closed(3)
    {
        return Some(false);
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
        state.gse.lock().unwrap().effects_queued = true;
        if let Some(router) = state.topology_router.get() {
            for (index, open) in effects.valves {
                let kind = crate::telemetry_schema::data_type(if index < 3 {
                    "VALVE_COMMAND"
                } else {
                    "ACTUATOR_COMMAND"
                });
                let payload = if open { OPEN[index] } else { CLOSE[index] };
                if let Err(err) = crate::telemetry_task::queue_controller_command(router, kind, &[payload]) {
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
                        rt.fill_trim = None;
                        rt.engine.fault(now, "Command transport failed")
                    };
                    // Best effort closes only; never continue opening a supply after an error.
                    for (index, _) in recovery.valves.into_iter().filter(|(index, _)| *index >= 3) {
                        let _ = crate::telemetry_task::queue_controller_command(
                            router,
                            crate::telemetry_schema::data_type("ACTUATOR_COMMAND"),
                            &[CLOSE[index]],
                        );
                    }
                    state.add_notification(
                        "GSE command transport failed. Verify supply valves are closed.",
                    );
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
    let handled = {
        let _dispatch = DISPATCH_GATE.lock().unwrap();
        handle_command_inner(state, cmd)
    };
    flush_effects(state);
    handled
}
fn flush_effects(state: &Arc<AppState>) {
    if !std::mem::take(&mut state.gse.lock().unwrap().effects_queued) {
        return;
    }
    if let Some(router) = state.topology_router.get() {
        crate::telemetry_task::flush_command_tx(router, "GSE sequence");
    }
}
fn handle_command_inner(state: &Arc<AppState>, cmd: &TelemetryCommand) -> bool {
    // Admission and handoff share the effect gate: a supply report may have
    // changed since the outer command worker checked the policy.
    if valve_command(cmd) && !state.is_command_allowed(cmd) {
        state.add_temporary_notification(
            "Manual valve command blocked: current interlock or supply safety state changed",
        );
        return true;
    }
    #[cfg(not(feature = "hitl_mode"))]
    if handle_operational_command(state, cmd) {
        return true;
    }
    #[cfg(feature = "hitl_mode")]
    if matches!(cmd, TelemetryCommand::ToggleGroundStationControl) {
        let enabled = {
            let mut rt = state.gse.lock().unwrap();
            rt.ground_station_control = !rt.ground_station_control;
            if !rt.ground_station_control && rt.fill_trim.is_some() {
                rt.cancel_automation_for_manual();
                rt.engine.status.message =
                    "Automatic mass trim disabled; operator controls the current valve states"
                        .into();
            }
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
            if matches!(action, Action::PauseFill | Action::CancelFill) && rt.fill_trim.is_some() {
                if let Some(mut controller) = rt.fill_trim.take() {
                    controller.cancel();
                }
                rt.engine.status.phase = Phase::Filling;
            }
            if action == Action::StartFill
                && rt.ground_station_control
                && rt.fresh_mass(input.now_ms).is_none()
            {
                Err("Automatic fill requires fresh calibrated fill weight before starting".into())
            } else {
                let result = rt.engine.request(action, input);
                if result.is_ok() {
                    rt.manual_control = false;
                }
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
            rt.fill_trim = None;
            rt.engine.fault(now, "Operator abort")
        };
        apply(state, effects);
    } else if valve_command(cmd) {
        let mut rt = state.gse.lock().unwrap();
        rt.cancel_automation_for_manual();
    }
    false
}
#[cfg(any(test, not(feature = "hitl_mode")))]
fn fill_mass_in_band(config: &Config, mass: f32, target: f32) -> bool {
    mass.is_finite()
        && target.is_finite()
        && target != 0.0
        && (mass - target).abs() <= config.fill_tolerance_kg
}
fn fill_stop_reached(config: &Config, mass: f32, target: f32) -> bool {
    mass.is_finite()
        && target.is_finite()
        && target != 0.0
        && mass * target.signum() >= target.abs() + config.fill_overfill_kg
}
#[cfg(not(feature = "hitl_mode"))]
fn remember_automatic_cutoff_target(runtime: &mut Runtime, mass: f32, target: f32) {
    // The stop and qualification candidate must agree on the existing target
    // tolerance. A pressure plateau alone must never qualify a short fill.
    if fill_mass_in_band(&runtime.engine.config, mass, target) {
        runtime.operational.target_reached = Some(target);
    }
}
pub fn tick(state: &Arc<AppState>, interlock: bool) {
    {
        let _dispatch = DISPATCH_GATE.lock().unwrap();
        tick_inner(state, interlock);
    }
    flush_effects(state);
}
fn tick_inner(state: &Arc<AppState>, interlock: bool) {
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
        if let Some(mut controller) = rt.fill_trim.take() {
            let mass = rt.fresh_mass(input.now_ms);
            let update = controller.tick(input, mass);
            if let Some(message) = update.fault {
                #[cfg(not(feature = "hitl_mode"))]
                {
                    rt.operational
                        .saved
                        .qualification
                        .latch(Fault::ValveMismatch);
                    rt.operational.running = false;
                    let _ = rt.operational.persist();
                }
                // Backend owns the established emergency relief policy:
                // close both supplies and open vent/dump without ACK gating.
                rt.engine.fault(input.now_ms, &message)
            } else if update.completed {
                rt.engine.status.phase = Phase::Paused;
                rt.engine.status.message =
                    "Fill mass settled within tolerance; all valves confirmed closed".into();
                #[cfg(not(feature = "hitl_mode"))]
                {
                    if let Some(mass) = mass {
                        remember_automatic_cutoff_target(&mut rt, mass.value, target);
                    }
                    complete_operational_milestones(&mut rt, Phase::PauseSetup);
                }
                let mut effects = update.effects;
                effects.notification = Some("Fill complete: settled mass is within tolerance and all valves are confirmed closed".into());
                effects
            } else {
                rt.engine.status.message =
                    format!("Final fill mass verification: {:?}", controller.phase());
                rt.fill_trim = Some(controller);
                update.effects
            }
        } else {
            let effects = rt.engine.tick(input);
            #[cfg(not(feature = "hitl_mode"))]
            complete_operational_milestones(&mut rt, previous_phase);
            if rt.engine.status.phase != Phase::Filling || !rt.ground_station_control {
                rt.fill_cutoff.reset();
                effects
            } else if let Some(mass) = rt.fresh_mass(input.now_ms) {
                if fill_stop_reached(&rt.engine.config, mass.value, target) {
                    match rt
                        .engine
                        .config
                        .pressure_limit(rt.engine.nitrous_target_psi, 50.0)
                        .and_then(|limit| {
                            gse_sequence::fill_trim::Controller::start(
                                input.now_ms,
                                target,
                                limit,
                                &rt.engine.config,
                            )
                        }) {
                        Ok((controller, mut effects)) => {
                            rt.engine.manual_override();
                            rt.engine.status.phase = Phase::PauseSetup;
                            rt.engine.claimed = true;
                            rt.engine.status.message =
                                "Closing supplies and vent before settled mass verification".into();
                            rt.fill_trim = Some(controller);
                            effects.notification = Some("Automatic fill cutoff: closing valves, then settling and verifying final mass".into());
                            effects
                        }
                        Err(error) => rt.engine.fault(input.now_ms, &error),
                    }
                } else {
                    effects
                }
            } else {
                rt.engine.fault(
                    input.now_ms,
                    "Automatic fill stopped: fresh calibrated fill weight is required",
                )
            }
        }
    };
    apply(state, effects);
    #[cfg(not(feature = "hitl_mode"))]
    publish_qualified_flight_armed(state);
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
    if let Err(error) = require_primary_operational_station() {
        return error.into_response();
    }
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
    if crate::rrls::is_rrls() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Primary GSE configuration is not mirrored; use the primary ground station.",
        )
            .into_response();
    }
    Json(state.gse.lock().unwrap().engine.config.clone()).into_response()
}
fn nitrogen_configuration_changed(before: &Config, after: &Config) -> bool {
    before.nitrogen_plan() != after.nitrogen_plan()
        || before.pressure_ceiling_psi != after.pressure_ceiling_psi
        || before.maximum_zero_offset_psi != after.maximum_zero_offset_psi
}
fn fill_configuration_changed(before: &Config, after: &Config) -> bool {
    before.fill_overfill_kg != after.fill_overfill_kg
        || before.fill_tolerance_kg != after.fill_tolerance_kg
        || before.fill_settle_ms != after.fill_settle_ms
}
fn invalidate_changed_configuration(rt: &mut Runtime, config: &Config) -> Result<(), String> {
    let nitrogen = nitrogen_configuration_changed(&rt.engine.config, config);
    let fill = fill_configuration_changed(&rt.engine.config, config);
    if !nitrogen && !fill {
        return Ok(());
    }
    #[cfg(not(feature = "hitl_mode"))]
    {
        if !rt.operational.saved.pending_checklists.is_empty() {
            return Err(
                "Save pending checklist progress before changing procedure settings".into(),
            );
        }
        let affected = if nitrogen {
            rt.operational.saved.qualification.nitrogen_satisfied()
                || rt
                    .operational
                    .saved
                    .qualification
                    .completed(Milestone::FillTarget)
        } else {
            rt.operational
                .saved
                .qualification
                .completed(Milestone::FillTarget)
        };
        if affected {
            if nitrogen {
                rt.operational.saved.qualification.invalidate_nitrogen()
            } else {
                rt.operational.saved.qualification.invalidate_fill()
            }
            .map_err(str::to_owned)?;
            rt.operational.saved.fill_target_kg = None;
            rt.operational.target_reached = None;
            let sequences: &[crate::checklists::Sequence] = if nitrogen {
                &[
                    crate::checklists::Sequence::NitrogenTest,
                    crate::checklists::Sequence::FillTarget,
                ]
            } else {
                &[crate::checklists::Sequence::FillTarget]
            };
            for sequence in sequences {
                rt.operational
                    .saved
                    .pending_checklists
                    .push(crate::checklists::CompletionEvent {
                        id: crate::checklists::paired::completion_event_id(),
                        sequence: *sequence,
                        at_ms: crate::telemetry_task::get_current_timestamp_ms() as i64,
                        action: crate::checklists::CompletionAction::Clear,
                    });
            }
            // Evidence is revoked durably before edited settings can be saved.
            // Any failure remains fail-closed and never dispatches outputs.
            rt.operational.persist()?;
            rt.operational.flush_checklists()?;
        }
    }
    if nitrogen {
        rt.engine.status.nitrogen_passed = false;
        rt.engine.status.nitrogen_overridden = false;
    }
    Ok(())
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
    let result = (|| -> Result<(), (StatusCode, String)> {
        let mut rt = state.gse.lock().unwrap();
        if rt.engine.active() {
            return Err((
                StatusCode::CONFLICT,
                "Stop the active sequence before editing GSE configuration".into(),
            ));
        }
        // Bulk settings may preserve or revoke consent, but never grant it.
        // Compare and assign under this same lock so an intervening nitrogen
        // test cannot have its invalidated confirmation revived by stale UI.
        if config.dry_self_test_confirmed && !rt.engine.config.dry_self_test_confirmed {
            return Err((
                StatusCode::FORBIDDEN,
                "Grant dry self-test confirmation through its dedicated control".into(),
            ));
        }
        invalidate_changed_configuration(&mut rt, &config)
            .map_err(|error| (StatusCode::CONFLICT, error))?;
        persist_settings(&config).map_err(|error| (StatusCode::CONFLICT, error))?;
        rt.engine.config = config;
        Ok(())
    })();
    match result {
        Ok(()) => {
            crate::sequences::refresh_action_policy_now(&state);
            Json(state.gse.lock().unwrap().engine.config.clone()).into_response()
        }
        Err(error) => error.into_response(),
    }
}
async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(e) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return e;
    }
    if crate::rrls::is_rrls() {
        return match crate::primary_mirror::gse_status(&state) {
            Some(observation) => Json(observation).into_response(),
            None => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Fresh primary GSE observations are unavailable; valve states are unknown.",
            )
                .into_response(),
        };
    }
    let flight_state = *state.state.lock().unwrap();
    let prelaunch = prelaunch_flight_state(flight_state);
    let interlock = interlock(&state, &state.action_policy_snapshot());
    let mut response = serde_json::to_value(state.gse.lock().unwrap().engine.status.clone())
        .expect("GSE status is serializable");
    let mode = crate::station_mode::snapshot();
    response["operating_mode"] = serde_json::to_value(&mode).expect("mode serializable");
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
        response["automatic_abort_claimed"] = serde_json::json!(rt.operational.saved.automatic_abort_claimed);
        response["launch_queued"] = serde_json::json!(rt.operational.qd.queued());
        response["qd"] = serde_json::json!(format!("{:?}", rt.operational.qd.status(rt.now())));
    }
    Json(response).into_response()
}

#[cfg(test)]
mod tests {
    #[cfg(not(feature = "hitl_mode"))]
    #[test]
    fn template_switch_waits_for_local_reset_receipts() {
        let mut runtime = super::Runtime::default();
        assert!(runtime.checklist_definition_edit_allowed());
        runtime
            .operational
            .saved
            .pending_checklists
            .push(crate::checklists::CompletionEvent {
                id: "0".repeat(80),
                sequence: crate::checklists::Sequence::ValveSelfTest,
                at_ms: 0,
                action: crate::checklists::CompletionAction::Clear,
            });
        assert!(!runtime.checklist_definition_edit_allowed());
        runtime.operational.saved.pending_checklists.clear();
        assert!(runtime.checklist_definition_edit_allowed());
    }
    #[test]
    fn committed_fire_pilot_clock_advances_a_paused_hold_but_qualification_does_not() {
        let mut engine = Engine::default();
        engine.status.phase = Phase::Paused;
        assert!(
            engine.active(),
            "The filled-tank hold still owns GSE controls"
        );
        assert!(!pilot_clock_allowed(engine.active(), false, false));
        assert!(pilot_clock_allowed(engine.active(), true, false));
        // The former !Engine::active guard rejected this genuine Fire case.
        let old_guard_allowed = !engine.active();
        assert!(!old_guard_allowed);
    }
    #[cfg(feature = "hitl_mode")]
    #[tokio::test]
    async fn hitl_pilot_clock_wrapper_keeps_the_original_active_engine_guard() {
        let state = crate::state::tests::test_app_state().await;
        state.gse.lock().unwrap().engine.status.phase = Phase::Paused;
        assert!(!pilot_report_can_advance_launch_clock(&state));
        state.gse.lock().unwrap().engine.status.phase = Phase::Idle;
        assert!(pilot_report_can_advance_launch_clock(&state));
    }
    #[test]
    fn pilot_clock_guard_preserves_hitl_and_inactive_behavior() {
        for fire in [false, true] {
            assert!(!pilot_clock_allowed(true, fire, true));
            assert!(pilot_clock_allowed(false, fire, true));
            assert!(pilot_clock_allowed(false, fire, false));
        }
    }
    #[cfg(not(feature = "hitl_mode"))]
    #[test]
    fn quantized_automatic_target_cutoff_keeps_the_existing_qualification_tolerance() {
        let mut runtime = Runtime::default();
        let target = 5.0;
        let quantized_mass = target * 0.996;
        assert!(quantized_mass < target);
        assert!(runtime.fill_cutoff.target_met(quantized_mass, target));
        remember_automatic_cutoff_target(&mut runtime, quantized_mass, target);
        assert_eq!(runtime.operational.target_reached, Some(target));
        assert!(
            !runtime
                .operational
                .saved
                .qualification
                .completed(Milestone::FillTarget),
            "The candidate alone must not grant qualification before fresh mass and valve ACKs"
        );
    }
    #[cfg(not(feature = "hitl_mode"))]
    #[test]
    fn below_target_plateau_and_invalid_inputs_do_not_record_a_fill_target() {
        for (mass, target) in [
            (4.9, 5.0),
            (f32::NAN, 5.0),
            (f32::INFINITY, 5.0),
            (5.0, f32::NAN),
            (5.0, f32::INFINITY),
            (0.0, 0.0),
            (-4.0, -5.0),
        ] {
            let mut runtime = Runtime::default();
            remember_automatic_cutoff_target(&mut runtime, mass, target);
            assert_eq!(runtime.operational.target_reached, None, "{mass}/{target}");
        }
    }
    #[test]
    fn precision_inputs_keep_source_age_and_replays_cannot_create_settled_samples() {
        let utc = 1_798_000_000_000;
        let mut previous = None;
        assert_eq!(
            source_sample_at(Some(utc - 1500), utc, 5000, &mut previous),
            Some(3500)
        );
        assert_eq!(
            source_sample_at(Some(utc - 1500), utc + 100, 5100, &mut previous),
            None
        );
        assert_eq!(
            source_sample_at(Some(utc - 3000), utc, 5000, &mut previous),
            None
        );
        assert_eq!(
            source_sample_at(Some(utc + 101), utc, 5000, &mut previous),
            None
        );
        assert_eq!(
            source_sample_at(Some(utc - 100), utc, 5000, &mut previous),
            Some(4900)
        );
        assert_eq!(
            source_sample_at(Some(utc - 200), utc, 5000, &mut previous),
            None
        );
        let mut previous = None;
        assert_eq!(
            source_sample_at(Some(1234), utc, 5000, &mut previous),
            Some(5000)
        );
        assert_eq!(
            source_sample_at(Some(1234), utc + 100, 5100, &mut previous),
            None
        );
        assert_eq!(
            source_sample_at(Some(1235), utc + 100, 5100, &mut previous),
            Some(5100)
        );
    }
    #[tokio::test]
    async fn replayed_valve_capture_does_not_acknowledge_a_new_target() {
        let state = crate::state::tests::test_app_state().await;
        state.gse.lock().unwrap().clock = Instant::now() - std::time::Duration::from_secs(5);
        let utc = 1_798_000_000_000;
        assert!(state.set_umbilical_valve_state_source(0, false, utc - 1000, utc, 11));
        let before = state.gse.lock().unwrap().valves[0].unwrap().at_ms;
        state.set_pending_umbilical_valve_state(0, false);
        assert!(!state.set_umbilical_valve_state_source(0, false, utc - 1000, utc + 100, 11));
        assert_eq!(state.gse.lock().unwrap().valves[0].unwrap().at_ms, before);
        assert_eq!(state.get_pending_umbilical_valve_state(0), Some(false));
        assert!(!state.set_umbilical_valve_state_source(0, true, utc - 3000, utc, 22));
        assert!(state.set_umbilical_valve_state_source(0, false, utc + 1, utc + 1, 33));
    }
    #[test]
    fn precision_band_and_overshoot_are_absolute_and_signed_not_percentage() {
        let config = Config::default();
        for sign in [1.0, -1.0] {
            assert!(fill_mass_in_band(&config, 100.06 * sign, 100.0 * sign));
            assert!(!fill_mass_in_band(&config, 99.6 * sign, 100.0 * sign));
            assert!(!fill_stop_reached(&config, 100.0 * sign, 100.0 * sign));
            assert!(fill_stop_reached(&config, 100.06 * sign, 100.0 * sign));
        }
    }
    #[test]
    fn sequence_clock_uses_only_explicit_simulation_time() {
        assert_eq!(super::sequence_clock_ms(25_000, Some(1_000)), 1_000);
        assert_eq!(super::sequence_clock_ms(26_000, Some(1_000)), 1_000);
        assert_eq!(super::sequence_clock_ms(27_000, Some(2_000)), 2_000);
        assert_eq!(super::sequence_clock_ms(25_000, None), 25_000);
    }

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

    #[tokio::test]
    async fn manual_handoff_fences_prepared_effects_before_the_requested_command() {
        let (state, sent, peer) = button_test_state().await;
        let prepared = DISPATCH_GATE.lock().unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let manual_state = state.clone();
        let manual = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let handled = handle_command(&manual_state, &TelemetryCommand::NitrogenClose);
            finished_tx.send(handled).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(
            finished_rx.try_recv().is_err(),
            "manual cancellation must wait for prepared admission"
        );
        apply(
            &state,
            Effects {
                valves: vec![(3, true)],
                notification: None,
            },
        );
        drop(prepared);
        assert!(
            !finished_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
        );
        manual.join().unwrap();
        // Model the existing outer command dispatch after handle_command(false).
        crate::telemetry_task::queue_controller_command(
            state.topology_router.get().unwrap(),
            crate::telemetry_schema::data_type("ACTUATOR_COMMAND"),
            &[12],
        )
        .unwrap();
        pump_test_link(&state, &peer);
        let before = sent.lock().unwrap().clone();
        assert_eq!(
            before,
            vec![
                ("ACTUATOR_COMMAND".into(), vec![9]),
                ("ACTUATOR_COMMAND".into(), vec![12])
            ]
        );
        tick(&state, true);
        pump_test_link(&state, &peer);
        assert_eq!(
            *sent.lock().unwrap(),
            before,
            "cancelled ACK queues and startup must stay inert"
        );
    }

    #[cfg(not(feature = "hitl_mode"))]
    pub(super) fn confirm_test_valves(state: &AppState, valves: [bool; 5]) {
        let mut rt = state.gse.lock().unwrap();
        for (key, open) in KEYS.into_iter().zip(valves) {
            rt.observe_valve(key, open);
        }
        rt.observe_pressure(Some(0.0));
        rt.observe_mass(Some(0.0));
    }

    #[cfg(feature = "hitl_mode")]
    fn drive_button_phase(
        state: &Arc<AppState>,
        sent: &TestCommands,
        peer: &sedsnet::router::Router,
        done: Phase,
        mass: f32,
    ) {
        let mut valves = gse_sequence::CLOSED;
        let mut consumed = 0;
        for _ in 0..200 {
            pump_test_link(state, peer);
            {
                let commands = sent.lock().unwrap();
                for (kind, bytes) in &commands[consumed..] {
                    let indices = if kind == "VALVE_COMMAND" { 0..3 } else { 3..5 };
                    let i = indices
                        .into_iter()
                        .find(|i| bytes[0] == OPEN[*i] || bytes[0] == CLOSE[*i])
                        .unwrap();
                    valves[i] = bytes[0] == OPEN[i];
                }
                consumed = commands.len();
            }
            {
                let mut rt = state.gse.lock().unwrap();
                let next = rt.now() + 100;
                rt.clock = Instant::now() - std::time::Duration::from_millis(next);
                for (key, open) in KEYS.into_iter().zip(valves) {
                    rt.observe_valve(key, open);
                }
                rt.observe_pressure(Some(0.));
                rt.observe_mass(Some(mass));
            }
            tick(state, true);
            let rt = state.gse.lock().unwrap();
            assert_ne!(
                rt.engine.status.phase,
                Phase::Fault,
                "{}",
                rt.engine.status.message
            );
            if rt.engine.status.phase == done && (done != Phase::Filling || valves[4]) {
                return;
            }
        }
        panic!("routed sequence did not reach {done:?}");
    }
    #[tokio::test]
    #[cfg(feature = "hitl_mode")]
    async fn nitrogen_button_enters_baseline_and_requests_safe_valve_configuration() {
        let (state, sent, peer) = button_test_state().await;
        assert_eq!(
            command_allowed(&state, &TelemetryCommand::NitrogenTest),
            Some(true)
        );
        assert!(handle_command(&state, &TelemetryCommand::NitrogenTest));
        pump_test_link(&state, &peer);
        assert_eq!(
            *sent.lock().unwrap(),
            vec![("ACTUATOR_COMMAND".into(), vec![12])],
            "serial close admission must precede later targets"
        );
        drive_button_phase(&state, &sent, &peer, Phase::Baseline, 0.);
        assert_eq!(
            sent.lock().unwrap().len(),
            5,
            "all physical targets require reports"
        );
    }
    #[tokio::test]
    #[cfg(feature = "hitl_mode")]
    async fn fill_button_requires_nitrogen_pass_then_opens_only_after_confirmations_and_stops_at_mass()
     {
        let (state, sent, peer) = button_test_state().await;
        assert!(handle_command(&state, &TelemetryCommand::StartFill));
        assert!(sent.lock().unwrap().is_empty());
        {
            let mut rt = state.gse.lock().unwrap();
            rt.engine.status.phase = Phase::Passed;
            rt.engine.status.nitrogen_passed = true;
            rt.engine.status.baseline = Some(gse_sequence::Baseline {
                average_psi: 0.,
                min_psi: 0.,
                max_psi: 0.,
                noise_psi: 0.1,
                samples: 50,
            });
        }
        assert!(handle_command(&state, &TelemetryCommand::StartFill));
        drive_button_phase(&state, &sent, &peer, Phase::Filling, 0.);
        pump_test_link(&state, &peer);
        assert!(
            sent.lock()
                .unwrap()
                .contains(&("ACTUATOR_COMMAND".into(), vec![10]))
        );
        let target = state.fill_targets_snapshot().nitrous.target_mass_kg;
        {
            let mut rt = state.gse.lock().unwrap();
            let stop = target + target.signum() * rt.engine.config.fill_overfill_kg;
            rt.observe_mass(Some(stop));
        }
        tick(&state, true);
        assert!(state.gse.lock().unwrap().fill_trim.is_some());
        assert_eq!(
            state.gse.lock().unwrap().engine.status.phase,
            Phase::PauseSetup
        );
        drive_button_phase(&state, &sent, &peer, Phase::Paused, target);
        assert!(state.gse.lock().unwrap().fill_trim.is_none());
    }

    #[tokio::test]
    #[cfg(feature = "hitl_mode")]
    async fn disabling_mass_control_cancels_trim_without_changing_valves() {
        let (state, sent, peer) = button_test_state().await;
        {
            let mut rt = state.gse.lock().unwrap();
            let (controller, _) =
                gse_sequence::fill_trim::Controller::start(rt.now(), 5., 200., &rt.engine.config)
                    .unwrap();
            rt.fill_trim = Some(controller);
            rt.engine.status.phase = Phase::PauseSetup;
        }
        assert!(handle_command(
            &state,
            &TelemetryCommand::ToggleGroundStationControl
        ));
        {
            let rt = state.gse.lock().unwrap();
            assert!(!rt.ground_station_control);
            assert!(rt.fill_trim.is_none());
            assert_eq!(rt.engine.status.phase, Phase::Idle);
            assert!(rt.manual_control);
        }
        tick(&state, true);
        pump_test_link(&state, &peer);
        assert!(
            sent.lock().unwrap().is_empty(),
            "turning off must not retain vent pulses or force other outputs"
        );
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
            FlightState::Descent,
            FlightState::Reefing,
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
    {
        {
            let _dispatch = DISPATCH_GATE.lock().unwrap();
            check_report_safety(state);
        }
        flush_effects(state);
    }
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
/// RRLS relays telemetry and shared settings but cannot change live operation.
fn require_primary_operational_station() -> Result<(), (StatusCode, String)> {
    if crate::rrls::is_rrls() {
        Err((
            StatusCode::FORBIDDEN,
            "Live GSE operations are read-only on RRLS".into(),
        ))
    } else {
        Ok(())
    }
}
/// Operator recovery only: never dispatch a valve, ignition or launch command.
fn clear_ground_abort(state: &Arc<AppState>) -> Result<(), (StatusCode, String)> {
    require_primary_operational_station()?;
    #[cfg(not(feature = "hitl_mode"))]
    let _clear_reservation;
    #[allow(unused_mut)] // Operational recovery mutates this guard outside HITL.
    let mut rt = state.gse.lock().unwrap();
    let aborted = *state.state.lock().unwrap() == FlightState::Aborted;
    #[cfg(not(feature = "hitl_mode"))]
    let aborted = aborted || (rt.operational.saved.automatic_abort_claimed
        && matches!(state.local_flight_state_snapshot(), FlightState::Startup | FlightState::Idle | FlightState::Armed));
    if !aborted {
        return Err((StatusCode::CONFLICT, "GroundStation is not aborted".into()));
    }
    #[cfg(not(feature = "hitl_mode"))]
    {
        if rt.automatic_abort_in_flight {
            return Err((
                StatusCode::CONFLICT,
                "Automatic abort dispatch is still in progress".into(),
            ));
        }
        _clear_reservation = automatic_abort::reserve_abort_clear(state, &mut rt);
    }
    // Preserve the existing validation priority even before a router exists.
    if rt.engine.active() {
        return Err((
            StatusCode::CONFLICT,
            "Stop the active sequence first".into(),
        ));
    }
    #[cfg(not(feature = "hitl_mode"))]
    if rt.operational.running {
        return Err((
            StatusCode::CONFLICT,
            "Stop the active sequence first".into(),
        ));
    }
    // Reservation excludes new automatic producers and Fire while ownership is
    // inspected. Do not hold GSE across router/transport locks or callbacks.
    drop(rt);
    let router = state.topology_router.get().ok_or((
        StatusCode::SERVICE_UNAVAILABLE, "Network is not initialized".into(),
    ))?;
    #[cfg(not(feature = "hitl_mode"))]
    if router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type("ABORT"))
        || state.has_pending_transport_tx()
    {
        return Err((
            StatusCode::CONFLICT,
            "Abort delivery is still pending; retry after the network drains".into(),
        ));
    }
    #[allow(unused_mut)] // Operational recovery mutates this guard outside HITL.
    let mut rt = state.gse.lock().unwrap();
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
        rt.operational.saved.checklist_digest = None;
        rt.operational.queue_checklist_reset();
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
    // Update the persisted managed value as well as the local/UI state, so
    // discovery and reconnection cannot replay this station's old abort value.
    drop(rt);
    crate::network_variables::set_flight_state(router, FlightState::Idle as u8)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut rt = state.gse.lock().unwrap();
    #[cfg(not(feature = "hitl_mode"))]
    rt.operational.clear_automatic_abort_claim()
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;
    let config = rt.engine.config.clone();
    rt.engine = Engine::default();
    rt.manual_control = false;
    rt.fill_trim = None;
    rt.engine.config = config;
    rt.engine.config.dry_self_test_confirmed = false;
    drop(rt);
    state.set_local_flight_state(FlightState::Idle);
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
    if let Err(error) = require_primary_operational_station() {
        return error.into_response();
    }
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
        rt.operational.saved.checklist_digest = None;
        rt.operational.queue_checklist_reset();
        rt.operational.saved.baseline = None;
        rt.operational.saved.fill_target_kg = None;
        rt.operational.persist_error = None;
        rt.operational.qd = QdWait::default();
        let config = rt.engine.config.clone();
        rt.engine = Engine::default();
        rt.engine.config = config;
        rt.engine.config.dry_self_test_confirmed = false;
        rt.operational.startup_configured = false;
        rt.manual_control = false;
        rt.fill_trim = None;
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
    #[cfg(feature = "hitl_mode")]
    { let _ = state; true }
    #[cfg(not(feature = "hitl_mode"))]
    {
        let rt = state.gse.lock().unwrap();
        automatic_abort::stage_allows_automatic_abort(&rt, state.local_flight_state_snapshot())
    }
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
    if let Err(error) = require_primary_operational_station() {
        return error.into_response();
    }
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
    if rt.operational.saved.automatic_abort_claimed || rt.automatic_abort_in_flight {
        return Err("Automatic abort is pending; clear GSE abort before Fire".into());
    }
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

#[cfg(test)]
mod valve_capture_tests;
