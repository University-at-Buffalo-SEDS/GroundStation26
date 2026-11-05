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
    let observation = state
        .sequence_progress
        .lock()
        .ok()
        .map(|s| s.envelope(epoch, crate::sequence_progress_service::now_ms()));
    observation
        .and_then(|value| serde_json::to_value(value).ok())
        .unwrap_or_default()
}
pub(super) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/gse/sequences/cancel", axum::routing::post(cancel))
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
        Phase::Paused => C::Paused,
        Phase::Turbocharge => C::Turbocharge,
        Phase::CancelSetup => C::Cancelling,
        Phase::Idle | Phase::Passed | Phase::Cancelled | Phase::Fault => C::Terminal,
        _ => C::Unknown,
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
