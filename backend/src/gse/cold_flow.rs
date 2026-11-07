//! Ground-only cold flow. Dedicated admission never enters any Launch/igniter path.
//! Clock filesystem/auth reads and packet preparation occur outside control guards.
use super::*;
use crate::sequence_progress::{Kind, RunKey};
use axum::Json;
use gse_sequence::cold_flow::{Controller, Phase as ColdPhase};
use serde::{Deserialize, Serialize};

pub(super) struct Session {
    pub key: RunKey,
    controller: Controller,
    ticket: LaunchAdmission,
    origin_headers: HeaderMap,
    origin_username: Option<String>,
    revision: u64,
    retired: bool,
    pending: Option<PendingPlan>,
}
impl Session {
    fn active(&self) -> bool {
        !self.retired && self.controller.observation().phase != ColdPhase::Cancelled
    }
}
#[derive(Clone, Debug)]
pub(crate) struct PanelIntent {
    pub key: RunKey,
    pub generation: u64,
    pub awaiting_at_edge: bool,
}
struct Plan {
    initial_recording: Option<crate::fill_recording::Receipt>,
    key: RunKey,
    revision: u64,
    ticket: LaunchAdmission,
    effects: Effects,
    input: Inputs,
    next_effect: usize,
    packet: Option<sedsnet::packet::Packet>,
    failed: bool,
    deferred: bool,
    release_rollback: Option<Controller>,
}
struct PendingPlan {
    plan: Plan,
    recover: bool,
}
#[derive(Clone, Copy)]
enum Request {
    Release,
    Cancel,
    Park,
    Resume,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartRequest {}

pub(crate) fn active(state: &AppState) -> bool {
    state
        .gse
        .lock()
        .unwrap()
        .cold_flow
        .as_ref()
        .is_some_and(Session::active)
}
fn pending_plan(state: &AppState) -> bool {
    state.gse.lock().unwrap().cold_flow.as_ref().is_some_and(|s| s.pending.is_some())
}
pub(super) fn desired(rt: &Runtime) -> Option<[bool; 5]> {
    rt.cold_flow
        .as_ref()
        .filter(|s| s.active())
        .map(|s| s.controller.observation().desired)
}
pub(super) fn runtime_active(rt: &Runtime) -> bool {
    rt.cold_flow.as_ref().is_some_and(Session::active)
}
pub(super) fn owns_run(state: &AppState, key: &RunKey) -> bool {
    state
        .gse
        .lock()
        .unwrap()
        .cold_flow
        .as_ref()
        .is_some_and(|s| s.key == *key)
}
/// Captured at the physical EDGE, before policy/queue waits. A non-ready cold
/// event remains cold and can never become a normal Launch after delay/reset.
pub(crate) fn panel_edge(state: &AppState, generation: u64) -> Option<PanelIntent> {
    let rt = state.gse.lock().unwrap();
    let session = rt.cold_flow.as_ref().filter(|s| s.active())?;
    Some(PanelIntent {
        key: session.key.clone(),
        generation,
        awaiting_at_edge: session.controller.observation().phase == ColdPhase::AwaitingRelease,
    })
}
fn same(a: LaunchAdmission, b: LaunchAdmission) -> bool {
    a.trackable() && b.trackable() && a.original_ownership() == b.original_ownership()
}
fn ownership_state(state: &AppState, ticket: LaunchAdmission) -> CurrentState {
    if !same(ticket, LaunchAdmission::capture(state))
        || state.operator_admission_barrier.load(std::sync::atomic::Ordering::Acquire) != 0
    {
        return CurrentState::Invalid;
    }
    // Busy is temporary uncertainty, never a stopped/replaced run. This only
    // tries the SDK core; no blocking SDK inspection runs under DISPATCH.
    manual_stop_state(state)
}
fn ground_permitted(state: &AppState) -> bool {
    prelaunch(state)
        && !state.launch_history.lock().unwrap().automatic_actions_inhibited()
}
fn permitted(state: &AppState, ticket: LaunchAdmission) -> bool {
    ownership_state(state, ticket) == CurrentState::Current && ground_permitted(state)
}
fn input(rt: &Runtime, state: &AppState, simulation: Option<u64>, is_interlocked: bool) -> Inputs {
    let mut input = rt.input_at(
        rt.presentation_now(simulation),
        prelaunch(state),
        is_interlocked,
    );
    // Use the established calibrated GSE tank-PT lane, never substitute a
    // chamber ADC or Turbocharge's separately qualified VB pressure lane.
    input.pressure = rt.pressure;
    input
}
fn observe(rt: &mut Runtime, input: Inputs) {
    let Some(session) = &rt.cold_flow else {
        return;
    };
    let cold = session.controller.observation();
    let mut view = rt.engine.progress_observation(input);
    for (i, v) in view.valves.iter_mut().enumerate() {
        v.target_open = cold.desired[i];
    }
    rt.progress
        .observe_cold_flow(cold, view, crate::sequence_progress_service::now_ms());
    rt.engine.status.message = format!("Auto Cold Flow: {:?}; {:?}", cold.phase, cold.issue);
    if cold.phase == ColdPhase::Cancelled {
        rt.engine.status.phase = Phase::Cancelled;
    }
}
fn make_plan(rt: &mut Runtime, effects: Effects, input: Inputs) -> Option<Plan> {
    let session = rt.cold_flow.as_mut()?;
    session.revision = session.revision.checked_add(1)?;
    session.pending = None;
    let plan = Plan {
        initial_recording: None,
        key: session.key.clone(),
        revision: session.revision,
        ticket: session.ticket,
        effects,
        input,
        next_effect: 0,
        packet: None,
        failed: false,
        deferred: false,
        release_rollback: None,
    };
    observe(rt, input);
    Some(plan)
}
/// Prepare packets without DGate/Stop/GSE, then recheck exact run+revision at
/// actual SDK admission. A replaced/cancelled plan is never replayed.
fn apply(state: &Arc<AppState>, plan: Plan) -> bool {
    apply_bounded(state, plan, true)
}
/// A refused attempt owns no SDK packet. Retain the exact caller-owned copy
/// and cursor, so later service never retimestamps or republishes a success.
fn retain_busy(state: &Arc<AppState>, mut plan: Plan, recover: bool) -> bool {
    plan.deferred = true;
    let _dispatch = DISPATCH_GATE.lock().unwrap();
    let mut rt = state.gse.lock().unwrap();
    if let Some(session) = rt.cold_flow.as_mut().filter(|s| {
        s.key == plan.key && s.revision == plan.revision && !s.retired
    }) {
        if let Some(snapshot) = plan.release_rollback.take() {
            // Explicit PILOT release never becomes an autonomous retry. Only
            // this still-unadmitted release may restore its own bounded state;
            // Stop/replacement cannot be rolled back or granted a new ticket.
            if plan.next_effect == 0 && same(plan.ticket, LaunchAdmission::capture(state)) {
                let _ = with_stop_admission(state, plan.ticket, || {
                    session.controller = snapshot;
                    session.pending = None;
                });
                observe(&mut rt, plan.input);
            }
            return false;
        }
        session.pending = Some(PendingPlan { plan, recover });
        return true;
    }
    false
}
/// A retained unadmitted opening needs current source/isolation evidence, not
/// the earlier sample that originally constructed its packet. Closure attempts
/// remain available with invalid pressure; relief requires fresh source/PILOT
/// closure just as the controller's existing next_checked boundary does.
fn deferred_open_current(rt: &Runtime, state: &AppState, simulation: Option<u64>, index: usize) -> bool {
    let Some(session) = rt.cold_flow.as_ref() else { return false; };
    let policy = state.action_policy_snapshot();
    let input = input(rt, state, simulation, interlock(state, &policy));
    if !input.prelaunch { return false; }
    let o = session.controller.observation();
    session.controller.opening_readbacks_current(input, index)
        && (!matches!(index, 0 | 3)
            || (input.interlock && input.pressure.is_some_and(|p| {
                p.value.is_finite() && p.value < o.ceiling_psi
                    && (index != 3 || p.value < o.target_psi)
                    && p.at_ms <= input.now_ms && input.now_ms - p.at_ms <= 2000
            })))

}
fn retryable_queue_error(error: sedsnet::router::TryQueuedPacketError) -> bool {
    matches!(error, sedsnet::router::TryQueuedPacketError::CoreBusy
        | sedsnet::router::TryQueuedPacketError::SchemaBusy)
}
fn apply_bounded(state: &Arc<AppState>, mut plan: Plan, recover: bool) -> bool {
    while let Some(&(index, open)) = plan.effects.valves.get(plan.next_effect) {
        // Do not enter SDK packet/clock preparation while the core is known
        // busy. This inspection owns no application guard.
        match ownership_state(state, plan.ticket) {
            CurrentState::PendingBusy => return retain_busy(state, plan, recover),
            CurrentState::Invalid => return false,
            CurrentState::Current => {}
        }
        let Some(router) = state.topology_router.get() else {
            plan.failed = true;
            plan.next_effect += 1;
            continue;
        };
        if plan.packet.is_none() {
            let kind = crate::telemetry_schema::data_type(if index < 3 {
                "VALVE_COMMAND"
            } else {
                "ACTUATOR_COMMAND"
            });
            match crate::telemetry_task::controller_packet(
                router, kind, &[(if open { OPEN[index] } else { CLOSE[index] }) | 0x80],
            ) {
                Ok(packet) => plan.packet = Some(packet),
                Err(_) => {
                    plan.failed = true;
                    plan.next_effect += 1;
                    continue;
                }
            }
        }
        #[cfg(test)]
        tests::before_packet_admission(state, plan.packet.as_ref().unwrap());
        // Prepare metadata outside control guards. The SDK prepared owner keeps
        // the exact packet on refusal and transfers it only on one success.
        let mut prepared = match router.prepare_tx_queue(plan.packet.take().unwrap()) {
            Ok(prepared) => prepared,
            Err(failure) => {
                #[cfg(test)] eprintln!("COLD_FLOW_DIAGNOSTIC prepare index={index} error={:?}", failure.error);
                plan.failed = true;
                plan.next_effect += 1;
                drop(failure);
                continue;
            }
        };
        let simulation = crate::simulation_clock_ms();
        let accepted = {
            let _dispatch = DISPATCH_GATE.lock().unwrap();
            let valid = {
                let rt = state.gse.lock().unwrap();
                rt.cold_flow.as_ref().is_some_and(|s| {
                    s.key == plan.key && s.revision == plan.revision && !s.retired
                }) && (!open || !plan.deferred || deferred_open_current(&rt, state, simulation, index))
            };
            let ownership = ownership_state(state, plan.ticket);
            if !valid || ownership == CurrentState::Invalid
                || plan.initial_recording.as_ref().is_some_and(|r| !state.fill_recording.current(r))
                || (open && !ground_permitted(state))
                || (open && matches!(index, 0 | 3 | 4)
                    && !interlock(state, &state.action_policy_snapshot()))
            {
                Err(false)
            } else if ownership == CurrentState::PendingBusy {
                Err(true)
            } else {
                match with_stop_admission(state, plan.ticket, || {
                    router.try_tx_queue_unless_pending_reliable_type(
                        &mut prepared, crate::telemetry_schema::types::ABORT,
                    )
                }) {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(error)) if retryable_queue_error(error) => Err(true),
                    _ => Err(false),
                }
            }
        };
        match accepted {
            Err(true) => {
                plan.packet = prepared.into_packet();
                debug_assert!(plan.packet.is_some());
                return retain_busy(state, plan, recover);
            }
            Ok(()) => state.set_pending_umbilical_valve_state(KEYS[index], open),
            Err(false) => {
                log::warn!("Cold flow output admission unavailable: index={index} open={open}; confirmation remains incomplete");
                plan.failed = true;
            },
        }
        plan.packet = None;
        plan.next_effect += 1;
    }
    if plan.failed && recover {
        let closure = {
            let _dispatch = DISPATCH_GATE.lock().unwrap();
            let mut rt = state.gse.lock().unwrap();
            if rt.cold_flow.as_ref().is_some_and(|s| {
                s.key == plan.key && s.revision == plan.revision && !s.retired
                    && !matches!(s.controller.observation().phase,
                        ColdPhase::PauseClosing | ColdPhase::ParkedUnverified | ColdPhase::Paused
                            | ColdPhase::CancelClosing | ColdPhase::CancelRelieving)
            }) {
                let effects = rt.cold_flow.as_mut().unwrap().controller.park_for_transport(plan.input.now_ms);
                make_plan(&mut rt, effects, plan.input)
            } else { None }
        };
        if let Some(closure) = closure { apply_bounded(state, closure, false); }
    }
    if let Some(router) = state.topology_router.get() {
        crate::telemetry_task::flush_command_tx(router, "Auto Cold Flow");
    }
    !plan.failed
}

fn perform(
    state: &Arc<AppState>,
    key: &RunKey,
    requested: LaunchAdmission,
    request: Request,
) -> Result<(), &'static str> {
    let simulation = crate::simulation_clock_ms();
    let policy = state.action_policy_snapshot();
    let plan = {
        let _dispatch = DISPATCH_GATE.lock().unwrap();
        match ownership_state(state, requested) {
            CurrentState::PendingBusy => return Err("Cold-flow admission busy; retry this action"),
            CurrentState::Invalid => return Err("Cold-flow admission changed or Stop/Launch is active"),
            CurrentState::Current => {}
        }
        if !ground_permitted(state) {
            return Err("Cold-flow admission changed or Stop/Launch is active");
        }
        let retired = matches!(request, Request::Resume) && commands_retired(state);
        let mut rt = state.gse.lock().unwrap();
        let input = input(&rt, state, simulation, interlock(state, &policy));
        let session = rt
            .cold_flow
            .as_mut()
            .filter(|s| s.key == *key && !s.retired)
            .ok_or("Cold-flow run changed")?;
        if !same(session.ticket, requested) {
            return Err("Cold-flow original admission is stale");
        }
        let release_rollback = matches!(request, Request::Release).then(|| session.controller.clone());
        let effects = match request {
            Request::Release => session.controller.release(input)?,
            Request::Cancel => {
                if matches!(
                    session.controller.observation().phase,
                    ColdPhase::CancelClosing | ColdPhase::CancelRelieving | ColdPhase::Cancelled
                ) {
                    return Err("Cold-flow cancellation already pending or complete");
                }
                session.controller.cancel(input.now_ms)
            }
            Request::Park => {
                match session.controller.observation().phase {
                    ColdPhase::CancelClosing | ColdPhase::CancelRelieving | ColdPhase::Cancelled => {
                        return Err("Cold-flow cancellation already pending or complete");
                    }
                    ColdPhase::PauseClosing | ColdPhase::ParkedUnverified | ColdPhase::Paused => {
                        return Err("Cold flow is already paused");
                    }
                    _ => session.controller.park(input.now_ms),
                }
            }
            Request::Resume => {
                if !retired || session.pending.is_some() {
                    return Err("Earlier valve commands are still being delivered; resume after they retire");
                }
                session.controller.resume(input)?
            }
        };
        let mut plan = make_plan(&mut rt, effects, input).ok_or("Cold-flow revision exhausted")?;
        plan.release_rollback = release_rollback;
        plan
    };
    let applied = apply(state, plan);
    super::progress::flush(state);
    if matches!(request, Request::Release) && !applied {
        return Err("Cold-flow release was not admitted; explicitly retry with current authority");
    }
    // Policy refresh can inspect SDK topology. A retained Busy owner must
    // return now; the ordinary periodic refresh owns its later observation.
    if !pending_plan(state) { crate::sequences::refresh_action_policy_now(state); }
    Ok(())
}
fn reply(result: Result<(), &'static str>, key: RunKey) -> Response {
    match result{Ok(())=>Json(serde_json::json!({"version":1,"accepted":true,"key":key,"completion":"pending_authoritative_confirmation"})).into_response(),Err(message)=>(StatusCode::CONFLICT,message).into_response()}
}
async fn authorized(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    command: &str,
) -> Result<crate::auth::AuthPrincipal, Box<Response>> {
    let principal = crate::web::authorize_headers(state, headers, Permission::SendCommands)
        .await
        .map_err(Box::new)?;
    if !principal.allows_command_name(command) {
        return Err(Box::new(StatusCode::FORBIDDEN.into_response()));
    }
    require_primary_operational_station().map_err(|error| Box::new(error.into_response()))?;
    Ok(principal)
}
#[derive(Serialize)]
pub(super) struct Status {
    version: u16,
    target_psi: Option<f32>,
    start_available: bool,
    valid_for_ms: u64,
    backend_instance: String,
    reset_epoch: String,
    reason: &'static str,
    active: bool,
    phase: Option<ColdPhase>,
    key: Option<RunKey>,
}
pub(super) fn status(state: &AppState) -> Status {
    let simulation = crate::simulation_clock_ms();
    let policy = state.action_policy_snapshot();
    let ticket = LaunchAdmission::capture(state);
    let (target, ceiling, source, busy, ready, phase, key, owned) = {
        let rt = state.gse.lock().unwrap();
        #[cfg(not(feature = "hitl_mode"))]
        let ready = rt.operational.startup_configured
            && rt.operational.topology_stable
            && !rt.operational.running
            && rt.operational.persist_error.is_none()
            && !rt.operational.saved.automatic_abort_claimed
            && rt.operational.saved.qualification.fault().is_none();
        #[cfg(feature = "hitl_mode")]
        let ready = true;
        (
            rt.engine.config.cold_flow_target_psi,
            rt.engine.config.pressure_ceiling_psi,
            input(&rt, state, simulation, interlock(state, &policy)),
            rt.engine.active() || rt.fill_trim.is_some() || rt.turbocharge.is_some(),
            ready,
            rt.cold_flow
                .as_ref()
                .map(|s| s.controller.observation().phase),
            rt.cold_flow.as_ref().map(|s| s.key.clone()),
            runtime_active(&rt),
        )
    };
    let reason = if owned || busy {
        "Another ground sequence owns the valves"
    } else if target.is_none() {
        "Set the separate Cold Flow nitrogen target in GSE settings"
    } else if !ready {
        "Current ground readiness is required"
    } else if state.topology_router.get().is_none() {
        "Current command router unavailable"
    } else if !permitted(state, ticket) {
        "Current Stop/state/Launch admission is unavailable"
    } else if matches!(
        state.launch_clock_snapshot().kind,
        crate::telemetry_db::LaunchClockKind::TMinus | crate::telemetry_db::LaunchClockKind::TPlus
    ) {
        "An existing Launch clock is active"
    } else {
        Controller::start(target, ceiling, source)
            .err()
            .unwrap_or("")
    };
    Status {
        version: 1,
        target_psi: target,
        start_available: reason.is_empty(),
        valid_for_ms: if reason.is_empty() { gse_sequence::cold_flow::start_input_valid_for_ms(source) } else { 0 },
        backend_instance: state.sequence_progress.backend_instance(),
        reset_epoch: format!("{:016x}", ticket.generation),
        reason,
        active: owned,
        phase,
        key,
    }
}
pub(super) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/gse/cold-flow/start", axum::routing::post(start))
        .route("/api/gse/cold-flow/release", axum::routing::post(release))
        .route("/api/gse/cold-flow/cancel", axum::routing::post(cancel))
}
#[derive(Clone, Copy)]
enum PreparationError {
    PendingBusy,
    Invalid(&'static str),
}
impl From<&'static str> for PreparationError {
    fn from(value: &'static str) -> Self {
        Self::Invalid(value)
    }
}
fn preparation_ownership(
    state: &AppState,
    requested: LaunchAdmission,
) -> Result<(), PreparationError> {
    match ownership_state(state, requested) {
        CurrentState::PendingBusy => Err(PreparationError::PendingBusy),
        CurrentState::Invalid => Err(PreparationError::Invalid(
            "Cold flow is inhibited by current Stop/Launch/state",
        )),
        CurrentState::Current if ground_permitted(state) => Ok(()),
        CurrentState::Current => Err(PreparationError::Invalid(
            "Cold flow is inhibited by current Stop/Launch/state",
        )),
    }
}
fn recording_preflight(
    state: &Arc<AppState>,
    requested: LaunchAdmission,
) -> Result<(), PreparationError> {
    let simulation = crate::simulation_clock_ms();
    let policy = state.action_policy_snapshot();
    let _dispatch = DISPATCH_GATE.lock().unwrap();
    preparation_ownership(state, requested)?;
    if matches!(
        state.launch_clock_snapshot().kind,
        crate::telemetry_db::LaunchClockKind::TMinus | crate::telemetry_db::LaunchClockKind::TPlus
    ) {
        return Err(PreparationError::Invalid(
            "Reset the existing Launch clock before cold flow",
        ));
    }
    if state.topology_router.get().is_none() {
        return Err(PreparationError::Invalid(
            "Cold flow requires a current command router",
        ));
    }
    let rt = state.gse.lock().unwrap();
    if rt.engine.active()
        || runtime_active(&rt)
        || rt.fill_trim.is_some()
        || rt.turbocharge.is_some()
    {
        return Err(PreparationError::Invalid(
            "Another ground sequence owns the valves",
        ));
    }
    #[cfg(not(feature = "hitl_mode"))]
    if rt.operational.running
        || rt.operational.saved.automatic_abort_claimed
        || rt.operational.saved.qualification.fault().is_some()
        || !rt.operational.startup_configured
        || !rt.operational.topology_stable
        || rt.operational.persist_error.is_some()
    {
        return Err(PreparationError::Invalid(
            "Cold flow requires current ground safety readiness",
        ));
    }
    let input = input(&rt, state, simulation, interlock(state, &policy));
    let (_controller, _effects) = Controller::start(
        rt.engine.config.cold_flow_target_psi,
        rt.engine.config.pressure_ceiling_psi,
        input,
    )
    .map_err(PreparationError::Invalid)?;
    Ok(())
}
async fn start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(_): Json<StartRequest>,
) -> Response {
    let recording_origin = crate::fill_recording::Captured::capture(&state);
    let requested = LaunchAdmission::capture(&state);
    let principal = match authorized(&state, &headers, "AutoColdFlow").await {
        Ok(p) => p,
        Err(e) => return *e,
    };
    let Some(_claim) = claim_operator_dispatch(&state, requested.generation) else {
        return StatusCode::CONFLICT.into_response();
    };
    let weak = std::sync::Arc::downgrade(&state);
    let origin = recording_origin.bind(
        crate::fill_recording::Authority::Session {
            token: crate::web::bearer_token_from_headers(&headers).map(str::to_owned),
            actor: principal.username.clone(),
            admission_current: std::sync::Arc::new(move || {
                match weak
                    .upgrade()
                    .map(|state| recording_preflight(&state, requested))
                {
                    Some(Ok(())) => CurrentState::Current,
                    Some(Err(PreparationError::PendingBusy)) => CurrentState::PendingBusy,
                    Some(Err(PreparationError::Invalid(_))) | None => CurrentState::Invalid,
                }
            }),
        },
        Some(&principal),
    );
    let cancellation = crate::fill_recording::Cancellation::new(origin.clone());
    #[cfg(test)]
    crate::fill_recording::coldflow_test_hooks::pause(
        &state,
        crate::fill_recording::coldflow_test_hooks::Stage::Initial,
    );
    // This preparation owns the original pre-auth deadline. No control lock
    // spans a Busy wait, and no permission, ticket or timestamp is renewed.
    loop {
        if let Err(error) = origin.remaining() {
            return crate::fill_recording::http_result(Err(error)).into_response();
        }
        match recording_preflight(&state, requested) {
            Ok(()) => break,
            Err(PreparationError::Invalid(message)) => {
                return (StatusCode::CONFLICT, message).into_response();
            }
            Err(PreparationError::PendingBusy) => {
                let remaining = match origin.remaining() {
                    Ok(value) => value,
                    Err(error) => {
                        return crate::fill_recording::http_result(Err(error)).into_response();
                    }
                };
                tokio::time::sleep(remaining.min(std::time::Duration::from_millis(5))).await;
            }
        }
    }
    let _recording_preparation = match state.fill_recording.preparation.clone().try_acquire_owned()
    {
        Ok(permit) => permit,
        Err(_) => {
            return crate::fill_recording::http_result(Err(crate::fill_recording::Error::Busy))
                .into_response();
        }
    };
    let recording =
        match crate::fill_recording::ensure(&state, origin.clone(), "AutoColdFlow").await {
            Ok(receipt) => receipt,
            Err(error) => {
                crate::fill_recording::notify_failure(&state, &error);
                return crate::fill_recording::http_result(Err(error)).into_response();
            }
        };
    let mut reserved = super::progress::reserve(&state);
    #[cfg(test)]
    {
        if let Some((key, _)) = &reserved {
            crate::fill_recording::coldflow_test_hooks::reserved(&state, key);
        }
        crate::fill_recording::coldflow_test_hooks::pause(
            &state,
            crate::fill_recording::coldflow_test_hooks::Stage::Final,
        );
    }
    let result = loop {
        let simulation = crate::simulation_clock_ms();
        let policy = state.action_policy_snapshot();
        let attempt = (|| -> Result<(RunKey, Plan), PreparationError> {
            let _dispatch = DISPATCH_GATE.lock().unwrap();
            if origin.remaining().is_err() || !state.fill_recording.current(&recording) {
                return Err(PreparationError::Invalid(
                    "Recording proof expired before cold-flow admission",
                ));
            }
            preparation_ownership(&state, requested)?;
            if matches!(
                state.launch_clock_snapshot().kind,
                crate::telemetry_db::LaunchClockKind::TMinus
                    | crate::telemetry_db::LaunchClockKind::TPlus
            ) {
                return Err(PreparationError::Invalid(
                    "Reset the existing Launch clock before cold flow",
                ));
            }
            if state.topology_router.get().is_none() {
                return Err(PreparationError::Invalid(
                    "Cold flow requires a current command router",
                ));
            }
            let mut rt = state.gse.lock().unwrap();
            if rt.engine.active()
                || runtime_active(&rt)
                || rt.fill_trim.is_some()
                || rt.turbocharge.is_some()
            {
                return Err(PreparationError::Invalid(
                    "Another ground sequence owns the valves",
                ));
            }
            #[cfg(not(feature = "hitl_mode"))]
            if rt.operational.running
                || rt.operational.saved.automatic_abort_claimed
                || rt.operational.saved.qualification.fault().is_some()
                || !rt.operational.startup_configured
                || !rt.operational.topology_stable
                || rt.operational.persist_error.is_some()
            {
                return Err(PreparationError::Invalid(
                    "Cold flow requires current ground safety readiness",
                ));
            }
            let input = input(&rt, &state, simulation, interlock(&state, &policy));
            let (controller, effects) = Controller::start(
                rt.engine.config.cold_flow_target_psi,
                rt.engine.config.pressure_ceiling_psi,
                input,
            )
            .map_err(PreparationError::Invalid)?;
            let (key, ticket) = reserved
                .take()
                .ok_or("Cold-flow reporting identity unavailable")?;
            if !same(ticket, requested) {
                return Err(PreparationError::Invalid("Cold-flow origin changed"));
            }
            let mut origin_headers = HeaderMap::new();
            if let Some(value) = headers.get(axum::http::header::AUTHORIZATION) {
                if value.as_bytes().len() > 4096 {
                    return Err(PreparationError::Invalid(
                        "Cold-flow credential exceeds bound",
                    ));
                }
                origin_headers.insert(axum::http::header::AUTHORIZATION, value.clone());
            }
            rt.engine.manual_override();
            rt.engine.claimed = true;
            rt.engine.status.phase = Phase::Paused;
            rt.progress_start(Some((key.clone(), ticket)), Kind::ColdFlow, input);
            rt.cold_flow = Some(Session {
                key: key.clone(),
                controller,
                ticket,
                origin_headers,
                origin_username: principal.username.clone(),
                revision: 0,
                retired: false,
                pending: None,
            });
            let mut plan =
                make_plan(&mut rt, effects, input).ok_or("Cold-flow revision exhausted")?;
            plan.initial_recording = Some(recording.clone());
            Ok((key, plan))
        })();
        match attempt {
            Err(PreparationError::PendingBusy) => {
                // No controller, run, plan or output exists for this retry. The
                // same ready receipt and reserved reporting key stay local.
                if let Err(error) = origin.authorize(&state, "AutoColdFlow").await {
                    return crate::fill_recording::http_result(Err(error)).into_response();
                }
                let remaining = match origin.remaining() {
                    Ok(value) => value,
                    Err(error) => {
                        return crate::fill_recording::http_result(Err(error)).into_response();
                    }
                };
                tokio::time::sleep(remaining.min(std::time::Duration::from_millis(5))).await;
            }
            other => break other,
        }
    };
    match result {
        Ok((key, plan)) => {
            audit_admission("start", &key, &principal);
            let applied = apply(&state, plan);
            if !applied && !state.fill_recording.current(&recording) {
                let error = crate::fill_recording::Error::Unavailable;
                crate::fill_recording::notify_failure(&state, &error);
                return crate::fill_recording::http_result(Err(error)).into_response();
            }
            cancellation.transfer();
            super::progress::flush(&state);
            if !pending_plan(&state) {
                crate::sequences::refresh_action_policy_now(&state);
            }
            reply(Ok(()), key)
        }
        Err(PreparationError::Invalid(e)) => (StatusCode::CONFLICT, e).into_response(),
        Err(PreparationError::PendingBusy) => {
            crate::fill_recording::http_result(Err(crate::fill_recording::Error::PendingBusy))
                .into_response()
        }
    }
}
// Records accepted operator intent, never output/flow completion. No credentials.
fn audit_admission(action: &str, key: &RunKey, actor: &crate::auth::AuthPrincipal) {
    log::info!(
        "cold_flow action={action} actor={:?} backend={} reset={} run={} accepted_intent=true output_confirmation=pending",
        actor.username,
        key.backend_instance,
        key.reset_epoch,
        key.run_id
    );
}
async fn release(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(key): Json<RunKey>,
) -> Response {
    let ticket = LaunchAdmission::capture(&state);
    let actor = match authorized(&state, &headers, "ColdFlow").await {
        Ok(actor) => actor,
        Err(e) => return *e,
    };
    let Some(_claim) = claim_operator_dispatch(&state, ticket.generation) else {
        return StatusCode::CONFLICT.into_response();
    };
    let result = perform(&state, &key, ticket, Request::Release);
    if result.is_ok() {
        audit_admission("release", &key, &actor);
    }
    reply(result, key)
}
async fn cancel(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(key): Json<RunKey>,
) -> Response {
    let ticket = LaunchAdmission::capture(&state);
    let actor = match authorized(&state, &headers, "CancelFill").await {
        Ok(actor) => actor,
        Err(e) => return *e,
    };
    let response = cancel_authenticated(&state, key.clone(), ticket);
    if response.status().is_success() {
        audit_admission("cancel", &key, &actor);
    }
    response
}
pub(super) fn cancel_authenticated(
    state: &Arc<AppState>,
    key: RunKey,
    ticket: LaunchAdmission,
) -> Response {
    let Some(_claim) = claim_operator_dispatch(state, ticket.generation) else {
        return StatusCode::CONFLICT.into_response();
    };
    reply(perform(state, &key, ticket, Request::Cancel), key)
}
pub(super) fn park_authenticated(
    state: &Arc<AppState>,
    key: RunKey,
    ticket: LaunchAdmission,
) -> Response {
    let Some(_claim) = claim_operator_dispatch(state, ticket.generation) else {
        return StatusCode::CONFLICT.into_response();
    };
    reply(perform(state, &key, ticket, Request::Park), key)
}
pub(super) fn resume_authenticated(
    state: &Arc<AppState>,
    key: RunKey,
    ticket: LaunchAdmission,
) -> Response {
    let Some(_claim) = claim_operator_dispatch(state, ticket.generation) else {
        return StatusCode::CONFLICT.into_response();
    };
    reply(perform(state, &key, ticket, Request::Resume), key)
}
/// Live Pause/Resume/Cancel admission for the owned cold-flow run.
pub(super) fn controls(
    state: &AppState,
    block: &Option<String>,
    interlocked: bool,
) -> Option<crate::sequence_progress::RunControls> {
    use crate::sequence_progress::Gate;
    let simulation = crate::simulation_clock_ms();
    let ticket = state.gse.lock().unwrap().cold_flow.as_ref().filter(|s| s.active())?.ticket;
    let owned = permitted(state, ticket);
    let retired = commands_retired(state);
    let rt = state.gse.lock().unwrap();
    let session = rt.cold_flow.as_ref().filter(|s| s.active())?;
    let input = input(&rt, state, simulation, interlocked);
    let phase = session.controller.observation().phase;
    let cancelling = matches!(
        phase,
        ColdPhase::CancelClosing | ColdPhase::CancelRelieving | ColdPhase::Cancelled
    );
    let common = |gate: Result<(), String>| {
        Gate::from_result(gate.and_then(|()| {
            if let Some(block) = block {
                return Err(block.clone());
            }
            if !owned {
                return Err("Cold-flow admission changed or Stop/Launch is active".into());
            }
            Ok(())
        }))
    };
    let pause = if cancelling {
        Err("Cancelling".into())
    } else if phase == ColdPhase::PauseClosing {
        Err("Pause in progress: closing valves".into())
    } else if matches!(phase, ColdPhase::ParkedUnverified | ColdPhase::Paused) {
        Err("Already paused".into())
    } else {
        Ok(())
    };
    let resume = match phase {
        ColdPhase::Paused if !retired || session.pending.is_some() => {
            Err("Earlier valve commands are still being delivered".into())
        }
        ColdPhase::Paused => session
            .controller
            .clone()
            .resume(input)
            .map(|_| ())
            .map_err(str::to_string),
        ColdPhase::ParkedUnverified | ColdPhase::PauseClosing => Err(
            "Waiting until no earlier valve command can still be delivered and fresh closed reports follow"
                .into(),
        ),
        _ => Err("Resume is available once cold flow is paused".into()),
    };
    let cancel = if cancelling {
        Err("Cancelling".into())
    } else {
        Ok(())
    };
    Some(crate::sequence_progress::RunControls {
        key: session.key.clone(),
        pause: common(pause),
        resume: common(resume),
        cancel: common(cancel),
    })
}
pub(crate) async fn dispatch_panel(state: &Arc<AppState>, intent: PanelIntent) {
    if !intent.awaiting_at_edge {
        return;
    }
    let origin = {
        let rt = state.gse.lock().unwrap();
        rt.cold_flow
            .as_ref()
            .filter(|s| s.key == intent.key && s.active())
            .map(|s| {
                (
                    s.origin_headers.clone(),
                    s.origin_username.clone(),
                    s.ticket,
                )
            })
    };
    let Some((headers, username, ticket)) = origin else {
        return;
    };
    if intent.generation != ticket.generation {
        return;
    }
    let principal = match authorized(state, &headers, "ColdFlow").await {
        Ok(p) => p,
        Err(_) => return,
    };
    if principal.username != username {
        return;
    }
    let Some(_claim) = claim_operator_dispatch(state, intent.generation) else {
        return;
    };
    if perform(state, &intent.key, ticket, Request::Release).is_ok() {
        audit_admission("physical_panel_release", &intent.key, &principal);
    }
    // Every path ends here. There is NO normal Launch fallback.
}
/// Called instead of the normal engine tick while cold flow owns the outputs.
pub(super) fn tick(state: &Arc<AppState>, is_interlocked: bool) -> bool {
    let simulation = crate::simulation_clock_ms();
    let plan = {
        let _dispatch = DISPATCH_GATE.lock().unwrap();
        let ticket = {
            let rt = state.gse.lock().unwrap();
            let Some(session) = rt.cold_flow.as_ref().filter(|s| s.active()) else {
                return false;
            };
            session.ticket
        };
        let current = ownership_state(state, ticket);
        if current == CurrentState::PendingBusy { return true; }
        let retired = commands_retired(state);
        let mut rt = state.gse.lock().unwrap();
        let input = input(&rt, state, simulation, is_interlocked);
        if current == CurrentState::Invalid {
            if let Some(session) = &mut rt.cold_flow {
                session.retired = true;
                session.pending = None;
            }
            rt.progress.complete(
                crate::sequence_progress_projection::TerminalProof::Interrupted,
                crate::sequence_progress_service::now_ms(),
            );
            return true;
        }
        let session = rt.cold_flow.as_mut().unwrap();
        if retired && session.pending.is_none() {
            session.controller.commands_retired(input.now_ms);
        } else {
            session.controller.commands_pending();
        }
        if let Some(pending) = rt.cold_flow.as_mut().unwrap().pending.take() {
            Some(pending)
        } else {
            let effects = rt.cold_flow.as_mut().unwrap().controller.tick(input);
            make_plan(&mut rt, effects, input).map(|plan| PendingPlan { plan, recover: true })
        }
    };
    if let Some(pending) = plan {
        apply_bounded(state, pending.plan, pending.recover);
    }
    true
}
pub(super) fn cancel_or_park_current(state: &Arc<AppState>, park: bool) {
    let current = {
        let rt = state.gse.lock().unwrap();
        rt.cold_flow
            .as_ref()
            .filter(|s| s.active())
            .map(|s| (s.key.clone(), s.ticket))
    };
    if let Some((key, ticket)) = current {
        let _ = perform(
            state,
            &key,
            ticket,
            if park { Request::Park } else { Request::Cancel },
        );
    }
}
pub(super) fn manual_handoff(cmd: &TelemetryCommand) -> bool {
    super::valve_command(cmd) || matches!(cmd, TelemetryCommand::Igniter | TelemetryCommand::RetractPlumbing)
}
pub(super) fn handle_under_dispatch(state: &Arc<AppState>, cmd: &TelemetryCommand) -> bool {
    if !active(state) {
        return false;
    }
    if manual_handoff(cmd) {
        // Retire scheduled effects without changing any output. The original
        // manual packet continues through the normal dispatch path once.
        let mut rt = state.gse.lock().unwrap();
        if let Some(session) = &mut rt.cold_flow {
            session.retired = true;
            session.pending = None;
        }
        rt.cancel_automation_for_manual();
        return false;
    }
    if matches!(cmd, TelemetryCommand::Abort) {
        let mut rt = state.gse.lock().unwrap();
        if let Some(s) = &mut rt.cold_flow {
            s.retired = true;
            s.pending = None;
        }
        rt.progress.complete(
            crate::sequence_progress_projection::TerminalProof::Interrupted,
            crate::sequence_progress_service::now_ms(),
        );
        return false;
    }
    // Only competing automation belongs to this controller. Utilities, lights
    // and other operator commands keep their normal dispatch path.
    super::action(cmd).is_some()
        || matches!(cmd, TelemetryCommand::Launch | TelemetryCommand::Turbocharge)
        || crate::sequences::command_name(cmd) == "GroundStationLaunch"
        || crate::sequences::command_name(cmd) == "IgniterSequence"
}

#[cfg(test)]
#[path = "cold_flow_tests.rs"]
mod tests;
