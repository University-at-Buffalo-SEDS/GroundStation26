//! Observational sequence hooks; no reporting result is an engine/control input.
use super::*;
use crate::{
    sequence_progress::{self as wire, Kind, RunKey},
    sequence_progress_projection::{Cause, TerminalProof},
};
use axum::extract::Path;

pub(super) fn reserve(state: &AppState) -> Option<(RunKey, LaunchAdmission)> {
    let ticket = LaunchAdmission::capture(state);
    state
        .sequence_progress
        .reserve(ticket.generation)
        .ok()
        .map(|key| (key, ticket))
}
impl Runtime {
    pub(super) fn progress_start(
        &mut self,
        reserved: Option<(RunKey, LaunchAdmission)>,
        kind: Kind,
        input: Inputs,
    ) {
        if let Some((key, ticket)) = reserved {
            let view = self.engine.progress_observation(input);
            let plan = self.engine.progress_nitrogen_plan();
            self.progress.accepted(
                key,
                kind,
                view,
                plan,
                crate::sequence_progress_service::now_ms(),
            );
            self.progress_admission = Some(ticket);
        } else {
            self.progress.complete(
                TerminalProof::Interrupted,
                crate::sequence_progress_service::now_ms(),
            );
            self.progress.error = Some(wire::Error::Unavailable);
            self.progress_admission = None;
        }
    }
    pub(super) fn progress_observe(&mut self, input: Inputs, cause: Cause) {
        // Turbocharge's independent Controller supplies its exact phase,
        // desired outputs and original qualified inputs through its own hook.
        if self.engine.status.phase != Phase::Fault
            && self.progress.current().is_some_and(|s| {
                (s.kind == Kind::Turbocharge && self.turbocharge.is_some())
                    || (s.kind == Kind::Fill && self.fill_trim.is_some())
            })
        {
            return;
        }

        if super::cold_flow::runtime_active(self){return;}
        let view = self.engine.progress_observation(input);
        if view.phase == Phase::Fault {
            self.progress_failed_current();
            return;
        }
        if self
            .progress
            .current()
            .is_some_and(|s| s.kind == Kind::Fill)
        {
            self.progress.observe_fill(
                view,
                self.fresh_mass(input.now_ms),
                self.progress_fill_target_kg,
                input.now_ms,
                cause,
                crate::sequence_progress_service::now_ms(),
            );
        } else {
            self.progress
                .observe(view, cause, crate::sequence_progress_service::now_ms());
        }
        if view.phase == Phase::Cancelled && view.all_valves_confirmed_by_engine {
            self.progress.complete(
                TerminalProof::SafeCancelled(view),
                crate::sequence_progress_service::now_ms(),
            );
        }
        // Turbocharge cannot resume: its pause ends the run once every valve
        // is confirmed closed.
        if view.phase == Phase::Paused
            && view.all_valves_confirmed_by_engine
            && self.turbocharge.is_none()
            && self.progress.current().is_some_and(|s| s.kind == Kind::Turbocharge)
        {
            self.progress.complete(
                TerminalProof::SafeStopped(view),
                crate::sequence_progress_service::now_ms(),
            );
        }
    }
    pub(super) fn progress_failed_current(&mut self) {
        self.progress.complete_failure(
            &self.engine.status.message,
            crate::sequence_progress_service::now_ms(),
        );
    }

    pub(super) fn presentation_now(&self, simulation: Option<u64>) -> u64 {
        #[cfg(test)]
        if let Some(now) = self.test_now_ms {
            return now;
        }
        sequence_clock_ms(self.clock.elapsed().as_millis() as u64, simulation)
    }
    pub(super) fn progress_pass(&mut self, kind: Kind, event_id: String) {
        self.progress.complete(
            TerminalProof::DurablePass { kind, event_id },
            crate::sequence_progress_service::now_ms(),
        );
    }
}
fn publication_gate(state: &AppState) -> Option<Arc<Mutex<()>>> {
    state
        .sequence_progress
        .lock()
        .ok()
        .map(|s| s.publication_gate())
}
fn publication_lease<'a>(
    state: &AppState,
    gate: &'a Arc<Mutex<()>>,
) -> std::sync::MutexGuard<'a, ()> {
    match gate.lock() {
        Ok(guard) => guard,
        Err(poison) => {
            if let Ok(mut service) = state.sequence_progress.lock() {
                service.report_error(wire::Error::Unavailable);
            }
            poison.into_inner()
        }
    }
}
/// Must be called with no GSE/Stop/dispatch guard held. Reporting failures only
/// make the observation unavailable; they never fail or cancel automation.
pub(super) fn flush(state: &AppState) {
    let gate = publication_gate(state);
    let _publication = gate.as_ref().map(|gate| publication_lease(state, gate));

    let (events, error) = {
        let mut rt = state.gse.lock().unwrap();
        (rt.progress.take(), rt.progress.error)
    };
    project_and_publish(state, events, error);
}
#[cfg(all(test, unix))]
struct ReportClockProbe {
    path: std::path::PathBuf,
    started: std::sync::atomic::AtomicBool,
}
#[cfg(all(test, unix))]
static REPORT_CLOCK_PROBE: Mutex<Option<Arc<ReportClockProbe>>> = Mutex::new(None);
#[cfg(all(test, unix))]
fn report_clock_probe() {
    let probe = REPORT_CLOCK_PROBE.lock().unwrap().clone();
    if let Some(probe) = probe {
        probe
            .started
            .store(true, std::sync::atomic::Ordering::Release);
        // Real blocking filesystem read in the owned fixture, never a production
        // dependency or clock substitute. This must remain post-control.
        let bytes = std::fs::read_to_string(&probe.path).unwrap();
        assert_eq!(bytes, "100");
    }
}
fn project_and_publish(
    state: &AppState,
    batch: crate::sequence_progress_stage::Batch,
    error: Option<wire::Error>,
) {
    #[cfg(all(test, unix))]
    report_clock_probe();
    let (events, project_error) = match state.sequence_progress.projector.lock() {
        Ok(mut projector) => projector.apply(batch),
        Err(_) => (Vec::new(), Some(wire::Error::Unavailable)),
    };
    publish(state, events, error.or(project_error));
}
fn publish(
    state: &AppState,
    events: Vec<crate::sequence_progress_producer::Publication>,
    error: Option<wire::Error>,
) {
    if let Some(error) = error
        && let Ok(mut service) = state.sequence_progress.lock()
    {
        service.report_error(error);
    }
    for event in events {
        let intent = state.sequence_progress.lock().ok().and_then(|mut s| {
            s.publish(event.snapshot, event.observed_at_ms)
                .ok()
                .flatten()
        });
        if let Some(result) = intent {
            let warning = result.outcome != wire::Outcome::Passed;
            let id = state.add_temporary_notification(format!(
                "{}{}: {}. Sequence results are available in Notifications.",
                if warning { "Warning: " } else { "" },
                result.title,
                result.message
            ));
            let time = state
                .notifications
                .lock()
                .ok()
                .and_then(|rows| rows.iter().find(|n| n.id == id).map(|n| n.timestamp_ms));
            if let Some(time) = time
                && let Ok(mut service) = state.sequence_progress.lock()
                && service.bind_notification(&result.key, id, time).is_err()
            {
                service.report_error(wire::Error::Unavailable);
            }
            if time.is_none()
                && let Ok(mut service) = state.sequence_progress.lock()
            {
                service.report_error(wire::Error::Unavailable);
            }
        }
    }
}
pub(crate) fn envelope(state: &AppState) -> serde_json::Value {
    let epoch = automatic_generation(state);
    // Computed before the journal lock; it takes GSE and policy locks.
    let live = controls(state);
    let observation = state
        .sequence_progress
        .lock()
        .ok()
        .map(|s| s.envelope(epoch, crate::sequence_progress_service::now_ms()));
    observation
        .and_then(|mut value| {
            value.board_display = crate::board_recovery::sequence_board_display(state);
            if let Some(mut live) = live
                && let Some(active) = value.active.iter().find(|s| s.key == live.key)
            {
                // A reporting gap or stale epoch also withdraws every control.
                if !active.cancel_allowed {
                    live.cancel = wire::Gate::from_result(Err(active
                        .reporting_error
                        .clone()
                        .unwrap_or_else(|| "Cancel is not available in this step".into())));
                }
                if active.reporting_error.is_some() {
                    let reason = active.reporting_error.clone();
                    for gate in [&mut live.pause, &mut live.resume] {
                        if gate.allowed {
                            *gate = wire::Gate::from_result(Err(reason.clone().unwrap_or_default()));
                        }
                    }
                }
                value.controls.push(live);
            }
            serde_json::to_value(value).ok()
        })
        .unwrap_or_default()
}

/// Common station-level refusals shared by every run control.
pub(super) fn station_block(state: &AppState) -> Option<String> {
    if let Err((_, message)) = require_primary_operational_station() {
        return Some(message);
    }
    if !super::prelaunch(state) {
        return Some("Vehicle is not in a prelaunch state".into());
    }
    if state
        .launch_history
        .lock()
        .unwrap()
        .automatic_actions_inhibited()
    {
        return Some("Automatic GSE actions are inhibited after Launch".into());
    }
    if manual_stop_pending(state) {
        return Some("A Stop is still being delivered".into());
    }
    None
}
fn with_common(
    gate: Result<(), String>,
    block: &Option<String>,
    interlock_needed: bool,
    interlocked: bool,
    command_allowed: bool,
) -> wire::Gate {
    wire::Gate::from_result(gate.and_then(|()| {
        if let Some(block) = block {
            return Err(block.clone());
        }
        if interlock_needed && !interlocked {
            return Err("Enable the key and software interlock".into());
        }
        if !command_allowed {
            return Err("The station is not accepting this command right now".into());
        }
        Ok(())
    }))
}
fn cancel_phase(rt: &Runtime, kind: Kind) -> wire::CancelPhase {
    if kind == Kind::Fill && rt.fill_trim.is_some() {
        wire::CancelPhase::FillTrim
    } else {
        phase(rt.engine.status.phase)
    }
}
/// Live Pause/Resume/Cancel admission for the current run. The endpoints use
/// the same gates, then re-check under the dispatch gate before acting.
pub(crate) fn controls(state: &AppState) -> Option<wire::RunControls> {
    if crate::rrls::is_rrls() {
        return None;
    }
    let block = station_block(state);
    let policy = state.action_policy_snapshot();
    let interlocked = interlock(state, &policy);
    if super::cold_flow::active(state) {
        return super::cold_flow::controls(state, &block, interlocked);
    }
    let is_prelaunch = super::prelaunch(state);
    let pause_command = state.is_command_allowed(&TelemetryCommand::PauseFill);
    let cancel_command = state.is_command_allowed(&TelemetryCommand::CancelFill);
    let fill_command = state.is_command_allowed(&TelemetryCommand::StartFill);
    let mass_target = state.fill_targets_snapshot().nitrous.target_mass_kg;
    let retired = super::commands_retired(state);
    let rt = state.gse.lock().unwrap();
    let summary = rt.progress.current().filter(|s| !s.outcome.terminal())?;
    let (key, kind) = (summary.key.clone(), summary.kind);
    let phase_now = rt.engine.status.phase;
    let input = rt.input(is_prelaunch, interlocked);
    let test = matches!(kind, Kind::ValveSelfTest | Kind::NitrogenTest);
    let pause = match phase_now {
        _ if kind == Kind::Turbocharge => {
            if rt.turbocharge.is_some() {
                Ok(())
            } else {
                Err("Turbocharge is no longer running".into())
            }
        }
        _ if kind == Kind::Fill && rt.fill_trim.is_some() => Ok(()),
        Phase::PauseSetup => Err("Pause in progress: closing valves".into()),
        Phase::Paused => Err("Already paused".into()),
        Phase::CancelSetup | Phase::Cancelled => Err("Cancelling".into()),
        Phase::FillSetup | Phase::Filling if kind == Kind::Fill => Ok(()),
        _ if test && rt.engine.allows(Action::PauseFill, input) => Ok(()),
        _ => Err("Pause is not available in this step".into()),
    };
    let resume: Result<(), String> = match kind {
        Kind::Turbocharge => {
            Err("Turbocharge cannot resume after a pause; start it again from Actions".into())
        }
        Kind::Fill if phase_now != Phase::Paused || rt.fill_trim.is_some() => {
            Err("Resume is available once the fill is paused".into())
        }
        Kind::Fill => {
            let mass = rt.fresh_mass(input.now_ms);
            let target_psi = state.fill_targets_snapshot().nitrous.target_pressure_psi;
            if rt.ground_station_control
                && rt.valve_pressure.is_some_and(|p| {
                    p.value >= target_psi
                        && p.at_ms <= input.now_ms
                        && input.now_ms - p.at_ms <= 2000
                })
            {
                Err("Nitrous pressure is already at its target".into())
            } else if rt.ground_station_control && mass.is_none() {
                Err("Automatic fill requires fresh calibrated fill weight".into())
            } else {
                match rt.engine.fill_resume_required(input, mass, mass_target) {
                    Ok(true) => operational_gate(&rt, Action::StartFill),
                    Ok(false) => Err("Fill weight is within 0.2 kg of the target or above it; no refill needed".into()),
                    Err(error) => Err(error),
                }
            }
        }
        _ if test => match rt.engine.paused_test() {
            Some(paused) if phase_now == Phase::Paused => rt
                .engine
                .resume_test_check(input)
                .and_then(|()| {
                    operational_gate(
                        &rt,
                        match paused {
                            gse_sequence::PausedTest::ValveSelfTest => Action::SelfTest,
                            gse_sequence::PausedTest::NitrogenTest => Action::NitrogenTest,
                        },
                    )
                }),
            _ => Err("Resume is available once the test is paused".into()),
        },
        _ => Err("This sequence cannot resume".into()),
    };
    let resume = resume.and_then(|()| {
        if retired {
            Ok(())
        } else {
            Err(super::RETIREMENT_PENDING.into())
        }
    });
    let cancel = wire::validate_cancel(
        &key,
        &wire::LiveCancelContext {
            key: &key,
            kind,
            phase: cancel_phase(&rt, kind),
            current_send_commands: true,
            admission_epoch_current: true,
        },
    )
    .map_err(|_| "Cancel is not available in this step".to_string())
    .and_then(|()| {
        if summary.cancel_allowed {
            Ok(())
        } else {
            Err("Cancel is not available in this step".into())
        }
    });
    let fill_resume = kind == Kind::Fill;
    let cancel = with_common(cancel, &block, false, interlocked, cancel_command);
    let hold = rt.engine.test_hold().cloned();
    drop(rt);
    let hold_choice = hold.map(|hold| wire::HoldChoice {
        message: hold.message(),
        passed: hold == gse_sequence::TestHold::Passed,
        cold_flow: {
            let status = super::cold_flow::status(state);
            wire::Gate::from_result(if status.start_available {
                Ok(())
            } else if status.reason.is_empty() {
                Err("Waiting for the valves to confirm closed".into())
            } else {
                Err(status.reason.into())
            })
        },
        dump: cancel.clone(),
    });
    Some(wire::RunControls {
        key,
        pause: with_common(pause, &block, true, interlocked, pause_command),
        resume: with_common(resume, &block, true, interlocked, !fill_resume || fill_command),
        cancel,
        hold_choice,
    })
}
#[cfg(not(feature = "hitl_mode"))]
fn operational_gate(rt: &Runtime, action: Action) -> Result<(), String> {
    if !operational_action_allowed(rt, action) {
        return Err("Complete prerequisites in order and wait for stable fill-system routes".into());
    }
    if action != Action::StartFill
        && rt.operational.run_generation != rt.operational.saved.qualification.generation()
    {
        return Err("Qualification was reset during the pause; cancel and start a new test".into());
    }
    Ok(())
}
#[cfg(feature = "hitl_mode")]
fn operational_gate(_rt: &Runtime, _action: Action) -> Result<(), String> {
    Ok(())
}
pub(super) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/gse/sequences/cancel", axum::routing::post(cancel))
        .route("/api/gse/sequences/pause", axum::routing::post(pause))
        .route("/api/gse/sequences/resume", axum::routing::post(resume))
        .route(
            "/api/gse/sequences/results/{backend}/{epoch}/{run}",
            get(result),
        )
}
async fn result(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((backend, epoch, run)): Path<(String, String, String)>,
) -> Response {
    if let Err(error) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await
    {
        return error;
    }
    let key = RunKey {
        backend_instance: backend,
        reset_epoch: epoch,
        run_id: run,
    };
    if !key.valid() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if crate::rrls::is_rrls() {
        return (StatusCode::GONE,"Results are not cached on this station; open the authoritative station's Notifications.").into_response();
    }
    match state
        .sequence_progress
        .lock()
        .ok()
        .and_then(|s| s.result(&key).ok())
    {
        Some(value) => Json(value).into_response(),
        None => (
            StatusCode::GONE,
            "Sequence result was pruned, the backend restarted, or the run is unknown.",
        )
            .into_response(),
    }
}
fn phase(value: Phase) -> wire::CancelPhase {
    use wire::CancelPhase as C;
    match value {
        Phase::BaselineSetup | Phase::Baseline => C::Baseline,
        Phase::SelfSetup
        | Phase::SelfOpen
        | Phase::SelfHold
        | Phase::SelfClose
        | Phase::SelfFinish => C::SelfTest,
        Phase::NitrogenSetup
        | Phase::Raising
        | Phase::Settling
        | Phase::Holding
        | Phase::Dumping => C::NitrogenTest,
        Phase::FillSetup | Phase::Filling => C::Fill,
        Phase::PauseSetup | Phase::Paused => C::Paused,
        Phase::Turbocharge => C::Turbocharge,
        Phase::CancelSetup => C::Cancelling,
        Phase::Idle | Phase::Passed | Phase::Cancelled | Phase::Fault => C::Terminal,
    }
}
fn same_ticket(a: LaunchAdmission, b: LaunchAdmission) -> bool {
    a.trackable()
        && b.trackable()
        && a.generation == b.generation
        && a.manual_epoch == b.manual_epoch
        && a.observed_manual_epoch == b.observed_manual_epoch
}
async fn cancel(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(key): Json<RunKey>,
) -> Response {
    // Capture before awaiting authentication; never freshen an old request.
    let requested = LaunchAdmission::capture(&state);
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::SendCommands).await {
            Ok(p) => p,
            Err(error) => return error,
        };
    if !principal.allows_command_name("CancelFill") {
        return StatusCode::FORBIDDEN.into_response();
    }
    if let Err(error) = require_primary_operational_station() {
        return error.into_response();
    }
    if super::cold_flow::owns_run(&state,&key){return super::cold_flow::cancel_authenticated(&state,key,requested);}

    let Some(_claim) = claim_operator_dispatch(&state, requested.generation) else {
        return StatusCode::CONFLICT.into_response();
    };
    let accepted = {
        let _dispatch = DISPATCH_GATE.lock().unwrap();
        let stop_pending = manual_stop_pending(&state);
        let command_allowed = state.is_command_allowed(&TelemetryCommand::CancelFill)
            && !state
                .launch_history
                .lock()
                .unwrap()
                .automatic_actions_inhibited();
        let valid = {
            let rt = state.gse.lock().unwrap();
            let Some(snapshot) = rt.progress.current() else {
                return StatusCode::CONFLICT.into_response();
            };
            let current = LaunchAdmission::capture(&state);
            let epochs = rt.progress_admission.is_some_and(|saved| {
                same_ticket(saved, current) && same_ticket(requested, current)
            }) && !stop_pending
                && command_allowed;
            wire::validate_cancel(
                &key,
                &wire::LiveCancelContext {
                    key: &snapshot.key,
                    kind: snapshot.kind,
                    phase: if snapshot.kind == Kind::Fill && rt.fill_trim.is_some() {
                        wire::CancelPhase::FillTrim
                    } else {
                        phase(rt.engine.status.phase)
                    },
                    current_send_commands: true,
                    admission_epoch_current: epochs,
                },
            )
            .is_ok()
                && snapshot.cancel_allowed
                && !snapshot.outcome.terminal()
        };
        if valid {
            handle_command_inner(&state, &TelemetryCommand::CancelFill, None, None);
            let rt = state.gse.lock().unwrap();
            rt.progress.current().is_some_and(|snapshot| {
                snapshot.key == key && snapshot.outcome == wire::Outcome::Cancelling
            }) && matches!(
                rt.engine.status.phase,
                Phase::CancelSetup | Phase::Cancelled
            )
        } else {
            false
        }
    };
    flush_effects(&state);
    flush(&state);
    if accepted {
        Json(serde_json::json!({"version":1,"accepted":true,"key":key,"completion":"pending_authoritative_confirmation"})).into_response()
    } else {
        (
            StatusCode::CONFLICT,
            "Sequence changed or cancellation is not currently permitted",
        )
            .into_response()
    }
}

/// Re-check the caller's run, ownership epoch and live gate under DISPATCH.
fn admitted(
    state: &AppState,
    key: &RunKey,
    requested: LaunchAdmission,
    pick: fn(&wire::RunControls) -> &wire::Gate,
) -> Result<(), String> {
    let current = LaunchAdmission::capture(state);
    let saved = state.gse.lock().unwrap().progress_admission;
    if !(saved.is_some_and(|saved| same_ticket(saved, current)) && same_ticket(requested, current))
    {
        return Err("Sequence ownership changed".into());
    }
    let live = controls(state).ok_or("No sequence is running")?;
    if live.key != *key {
        return Err("Sequence changed".into());
    }
    let gate = pick(&live);
    if !gate.allowed {
        return Err(gate.reason.clone().unwrap_or_else(|| "Not permitted".into()));
    }
    Ok(())
}
fn control_reply(result: Result<(), String>, key: RunKey) -> Response {
    match result {
        Ok(()) => Json(serde_json::json!({"version":1,"accepted":true,"key":key,"completion":"pending_authoritative_confirmation"})).into_response(),
        Err(message) => (StatusCode::CONFLICT, message).into_response(),
    }
}
async fn pause(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(key): Json<RunKey>,
) -> Response {
    let requested = LaunchAdmission::capture(&state);
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::SendCommands).await {
            Ok(p) => p,
            Err(error) => return error,
        };
    if !principal.allows_command_name("PauseFill") {
        return StatusCode::FORBIDDEN.into_response();
    }
    if let Err(error) = require_primary_operational_station() {
        return error.into_response();
    }
    if super::cold_flow::owns_run(&state, &key) {
        return super::cold_flow::park_authenticated(&state, key, requested);
    }
    let Some(_claim) = claim_operator_dispatch(&state, requested.generation) else {
        return StatusCode::CONFLICT.into_response();
    };
    let result = {
        let _dispatch = DISPATCH_GATE.lock().unwrap();
        admitted(&state, &key, requested, |c| &c.pause).and_then(|()| {
            handle_command_inner(&state, &TelemetryCommand::PauseFill, None, None);
            let rt = state.gse.lock().unwrap();
            if rt.progress.current().is_some_and(|s| s.key == key)
                && matches!(rt.engine.status.phase, Phase::PauseSetup | Phase::Paused)
            {
                Ok(())
            } else {
                Err(rt.engine.status.message.clone())
            }
        })
    };
    flush_effects(&state);
    flush(&state);
    crate::sequences::refresh_action_policy_projection(&state);
    control_reply(result, key)
}
async fn resume(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(key): Json<RunKey>,
) -> Response {
    let requested = LaunchAdmission::capture(&state);
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::SendCommands).await {
            Ok(p) => p,
            Err(error) => return error,
        };
    if let Err(error) = require_primary_operational_station() {
        return error.into_response();
    }
    if super::cold_flow::owns_run(&state, &key) {
        if !principal.allows_command_name("AutoColdFlow") {
            return StatusCode::FORBIDDEN.into_response();
        }
        return super::cold_flow::resume_authenticated(&state, key, requested);
    }
    let kind = state
        .gse
        .lock()
        .unwrap()
        .progress
        .current()
        .filter(|s| s.key == key)
        .map(|s| s.kind);
    let command = match kind {
        Some(Kind::Fill) => "StartFill",
        Some(Kind::ValveSelfTest) => "ValveSelfTest",
        Some(Kind::NitrogenTest) => "NitrogenTest",
        _ => {
            return (StatusCode::CONFLICT, "This sequence cannot resume").into_response();
        }
    };
    if !principal.allows_command_name(command) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(_claim) = claim_operator_dispatch(&state, requested.generation) else {
        return StatusCode::CONFLICT.into_response();
    };
    let result = {
        let _dispatch = DISPATCH_GATE.lock().unwrap();
        admitted(&state, &key, requested, |c| &c.resume).and_then(|()| {
            if kind == Some(Kind::Fill) {
                handle_command_inner(&state, &TelemetryCommand::StartFill, None, None);
                let rt = state.gse.lock().unwrap();
                if rt.progress.current().is_some_and(|s| s.key == key)
                    && matches!(rt.engine.status.phase, Phase::FillSetup | Phase::Filling)
                {
                    Ok(())
                } else {
                    Err(rt.engine.status.message.clone())
                }
            } else {
                super::resume_test_under_dispatch(&state)
            }
        })
    };
    flush_effects(&state);
    flush(&state);
    crate::sequences::refresh_action_policy_projection(&state);
    control_reply(result, key)
}

/// Exact current engine observation copied into this programme frame; all
/// output evidence is sampled separately with its original capture age.
pub(crate) fn stream_observation(state: &AppState) -> gse_sequence::ProgressObservation {
    let prelaunch = super::prelaunch(state);
    let simulation = crate::simulation_clock_ms();
    let rt = state.gse.lock().unwrap();
    let mut view = rt.engine.progress_observation(rt.input_at(
        rt.presentation_now(simulation),
        prelaunch,
        false,
    ));
    if let Some(controller) = &rt.turbocharge {
        let actual = controller.progress_observation();
        for (value, target) in view.valves.iter_mut().zip(actual.desired) {
            value.target_open = target;
        }
        view.nitrogen_absolute_target_psi = Some(actual.pressure_target_psi);
    }
    if let Some(controller) = &rt.fill_trim {
        for (value, target) in view
            .valves
            .iter_mut()
            .zip(controller.progress_observation().desired)
        {
            value.target_open = target;
        }
    }
    if let Some(desired)=super::cold_flow::desired(&rt) {for(i,v)in view.valves.iter_mut().enumerate(){v.target_open=desired[i];}}
    view
}

#[cfg(test)]
#[path = "progress_tests.rs"]
mod tests;

/// Same Runtime replacement point as the existing Reset Sim. Caller has no
/// GSE/Stop/dispatch mutex; journal and notifier are touched only after unlock.
/// Read one Copy marker before entering final owner/DGate/Stop critical guards.
pub(crate) fn verified_reset_marker(
    state: &AppState,
) -> Option<crate::sequence_progress_stage::Marker> {
    state.gse.lock().ok().and_then(|rt| rt.progress.marker())
}
/// Reporting-only staging at the existing final Reset linearization point.
/// A changed owner/revision is NEVER attributed to the captured old run.
pub(crate) fn capture_verified_reset_under_gate(
    rt: &mut Runtime,
    marker: Option<crate::sequence_progress_stage::Marker>,
    at_ms: u64,
) {
    if rt.progress.interrupt_if_current(marker, at_ms) { rt.progress_admission = None; }
}
pub(crate) fn publish_verified_reset_after_guards(state: &AppState) {
    flush(state);
}

/// Build outside every control/reset owner. Runtime::default can consult saved
/// operational configuration and must never be hidden inside a reset closure.
pub(crate) fn prepare_runtime_reset() -> Runtime {
    Runtime::default()
}
pub(crate) struct ResetObservation {
    batch: crate::sequence_progress_stage::Batch,
    error: Option<wire::Error>,
    retired: Runtime,
}
/// Small pre-reset owner capture. Caller owns GSE and existing reset admission;
/// no clock file, projection, journal, JSON, publication or Runtime construction.
pub(crate) fn capture_runtime_reset(rt: &mut Runtime, replacement: Runtime) -> ResetObservation {
    rt.progress.complete(
        TerminalProof::Interrupted,
        crate::sequence_progress_service::now_ms(),
    );
    let batch = rt.progress.take();
    let error = rt.progress.error;
    let retired = std::mem::replace(rt, replacement);
    ResetObservation {
        batch,
        error,
        retired,
    }
}
/// Call after ALL reset/GSE/Stop/dispatch guards, before opening new admission.
pub(crate) fn publish_runtime_reset(state: &AppState, observation: ResetObservation) {
    let gate = publication_gate(state);
    let _publication = gate.as_ref().map(|gate| publication_lease(state, gate));
    let ResetObservation {
        batch,
        error,
        retired,
    } = observation;
    drop(retired);
    project_and_publish(state, batch, error);
}
pub(crate) fn replace_runtime_for_sim_reset(state: &AppState) {
    let replacement = prepare_runtime_reset();
    let observation = {
        let mut rt = state.gse.lock().unwrap();
        capture_runtime_reset(&mut rt, replacement)
    };
    publish_runtime_reset(state, observation);
}
