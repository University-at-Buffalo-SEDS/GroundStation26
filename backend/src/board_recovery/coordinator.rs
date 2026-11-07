//! Primary-owned, cancellation-independent verified recovery. An SDK delivery
//! receipt is never board completion. No recovery runs during status viewing.
use super::*;
use crate::{
    launch_history::{
        RecoveryFence, RecoveryIntent, RecoveryIntentKind, RecoveryPending, RecoveryTarget,
    },
    operator_queue::Token,
};
use std::{collections::VecDeque, sync::atomic::Ordering};
use tokio::task::JoinHandle;

const CONFIRM_TTL: Duration = Duration::from_secs(30);
const QUIESCE_TTL: Duration = Duration::from_secs(12);
const POLL: Duration = Duration::from_millis(300);
#[cfg(test)]
#[path = "coordinator_c_board_tests.rs"]
mod c_board_ordering_tests;
#[cfg(test)]
#[path = "coordinator_control_drain_tests.rs"]
mod control_drain_tests;
#[cfg(test)]
#[path = "coordinator_prepare_admission_tests.rs"]
mod prepare_admission_tests;

#[cfg(test)]
mod explicit_launch_reset_tests {
    use super::*;
    #[tokio::test]
    async fn launch_reset_is_explicit_operator_only_and_pending_dispatch_stays_fenced() {
        let state = crate::state::tests::test_app_state().await;
        *state.state.lock().unwrap() = crate::types::FlightState::Launch;
        assert!(!host_safe(&state));
        assert!(!host_action_allowed(&state, "reset_startup"), "no SDK router cannot prove drained ownership");
        let endpoint = crate::telemetry_schema::endpoint("ACTUATOR_BOARD");
        let router = Arc::new(sedsnet::router::Router::new(sedsnet::router::RouterConfig::new([
            sedsnet::router::EndpointHandler::new_packet_handler(endpoint, |_| Ok(())),
        ])));
        state.topology_router.set(router.clone()).unwrap();
        assert_eq!(host_action_allowed(&state, "reset_startup"), cfg!(any(feature="hitl_mode",feature="test_fire_mode")));
        router.tx_queue(Packet::new(crate::telemetry_schema::data_type("ACTUATOR_COMMAND"),
            &[endpoint], "GS", 100, Arc::from(&b"\x00"[..])).unwrap()).unwrap();
        assert!(!host_action_allowed(&state, "reset_startup"), "actual SDK controller owner blocks Reset");
        router.process_all_queues_with_timeout(0).unwrap();
        assert_eq!(host_action_allowed(&state, "reset_startup"), cfg!(any(feature="hitl_mode",feature="test_fire_mode")));
        for action in ["recover_abort", "resync", "reset_startup_all7", "unknown"] {
            assert!(!host_action_allowed(&state, action), "{action}");
        }
        // A Launch still waiting on its Valve Board must not hold the way out.
        state.launch_sequence_command_pending.store(true, Ordering::Release);
        assert_eq!(host_action_allowed(&state, "reset_startup"), cfg!(any(feature="hitl_mode",feature="test_fire_mode")));
        state.launch_sequence_command_pending.store(false, Ordering::Release);
        *state.state.lock().unwrap() = crate::types::FlightState::Armed;
        assert_eq!(host_action_allowed(&state, "reset_startup"), cfg!(any(feature="hitl_mode",feature="test_fire_mode")));
        *state.state.lock().unwrap() = crate::types::FlightState::Idle;
        assert!(host_action_allowed(&state, "reset_startup"));
    }
}

#[cfg(test)]
mod shutdown_durability_tests {
    use super::*;
    #[tokio::test]
    async fn sealed_taken_running_owner_never_looks_idle_or_accepts_late_confirmation() {
        let state=crate::state::tests::test_app_state().await;
        let release=Arc::new(tokio::sync::Notify::new());
        let released=release.clone();
        {
            let mut control=state.board_recovery.coordinator.0.lock().unwrap();
            control.expected=Some((Board::DaqBoard,123));
            control.operation=Some(json!({"completed":false,"phase":"applying"}));
            control.task=Some(tokio::spawn(async move { released.notified().await; }));
        }
        let origin=Token::capture(&state,0);
        let mut owner=state.board_recovery.coordinator.seal_shutdown_and_take_owner().unwrap();
        assert!(!manual_utc_idle(&state),"taken handle incorrectly reopened idle");
        assert_eq!(state.board_recovery.coordinator.0.lock().unwrap().expected,Some((Board::DaqBoard,123)),"shutdown destroyed admitted application context");
        assert!(tokio::time::timeout(Duration::from_millis(1),&mut owner).await.is_err());
        // Taking/awaiting this owner holds no coordinator or Stop lock.
        assert!(state.board_recovery.coordinator.0.try_lock().is_ok());
        let before=state.manual_abort_intent.epoch();
        state.manual_abort_intent.request();
        assert!(state.manual_abort_intent.epoch()>before);
        let response=admit_confirmation(state.clone(),HeaderMap::new(),crate::auth::AuthPrincipal {
            roles:vec![],username:Some("shutdown-fixture".into()),permissions:Default::default(),expires_at_ms:None,
            anonymous:false,session_type:None,command_access:Default::default(),calibration_access:Default::default(),
        },Confirm {ticket:"old-ticket".into(),backend_instance:"old-backend".into(),principal_context:"old-principal".into(),reset_epoch:"0".into(),confirmation:"RECOVER".into()},origin,None);
        assert_eq!(response.status(),StatusCode::SERVICE_UNAVAILABLE);
        release.notify_one();
        owner.await.unwrap();
        assert!(!manual_utc_idle(&state),"finished owner silently reopened shutdown");
    }
}

#[derive(Default)]
pub(super) struct Runtime(Mutex<Control>);
#[derive(Default)]
struct Control {
    shutdown_closed: bool,
    shutdown_failure: bool,
    ticket: Option<Ticket>,
    operation: Option<Value>,
    task: Option<JoinHandle<()>>,
    expected: Option<(Board, u64)>,
    boundary: Option<u64>,
    inbox: VecDeque<Packet>,
}
#[derive(Clone)]
struct Target {
    board: Board,
    desired: u8,
    context: Readback,
    prepared_query: Observation,
    intent: Option<RecoveryIntent>,
    original_first: Option<RecoveryIntent>,
    uncertain_first: Option<RecoveryIntent>,
    first_binding: Option<FirstBinding>,
}
/// Opaque proof derived ONLY from an actual retained, unpublished HostPending.
/// It never authorizes boot0. It preserves the original MAX intent and binds
/// the independently fresh query's actual epoch/issuer to a NEW expectedMAX.
#[derive(Clone)]
pub(super) struct FirstBinding {
    original: RecoveryIntent,
    uncertain: RecoveryIntent,
    prior_boundary: u64,
    epoch: u32,
    issuer: u64,
}
impl FirstBinding {
    fn from_pending(pending: &RecoveryPending, board: Board, query: &Readback) -> Option<Self> {
        if pending.published
            || !query_domain_consistent(board, query)
            || query.boot != 0
            || query.result != 7
            || query.flags & 0x25 != 0x25
            || query.epoch == 0
            || query.epoch >= u32::MAX - 1
            || query.issuer == 0
        {
            return None;
        }
        let target = pending
            .targets
            .iter()
            .find(|target| target.board == board.sender_id() && target.boot == u64::MAX)?;
        let original = target.original_first.clone()?;
        let uncertain = match target.kind {
            RecoveryIntentKind::First => target.intent.clone(),
            RecoveryIntentKind::ReconcileFirst => target.uncertain_first.clone()?,
            RecoveryIntentKind::Normal => return None,
        };
        if original.boot() != u64::MAX
            || uncertain.boot() != u64::MAX
            || original.source_utc_ms > uncertain.source_utc_ms
        {
            return None;
        }
        Some(Self {
            original,
            uncertain,
            prior_boundary: pending.origin_utc_ms,
            epoch: query.epoch,
            issuer: query.issuer,
        })
    }
    pub(super) fn authorizes(
        &self,
        query: &Observation,
        boundary: super::exchange::ConfirmBoundary,
    ) -> bool {
        query.fresh(Stamp::now())
            && query.value.boot == 0
            && query.value.result == 7
            && query.value.flags & 0x25 == 0x25
            && query.value.epoch == self.epoch
            && query.value.issuer == self.issuer
            && boundary.utc() > self.prior_boundary
            && boundary.utc() > self.uncertain.source_utc_ms
    }
}
pub(super) fn first_uncertainty(
    pending: Option<&RecoveryPending>,
    board: Board,
    row: &Readback,
) -> bool {
    pending
        .and_then(|pending| FirstBinding::from_pending(pending, board, row))
        .is_some()
}
#[derive(Clone)]
struct Ticket {
    id: String,
    principal: String,
    backend: String,
    mode: crate::station_mode::Mode,
    created: WallInstant,
    boundary: Option<super::exchange::ConfirmBoundary>,
    prior_inhibition: bool,
    token: Token,
    clock: crate::launch_history::ClockRecord,
    action: String,
    phrase: String,
    targets: Vec<Target>,
    reset_selection: Option<super::scope::Selection>,
}
/// Constructed only from the dedicated final QUERY path, never from an SDK
/// acknowledgement or UI-supplied values. Validation runs inside final gate.
pub(crate) struct FinalProofs {
    owner: u64,
    generation: u64,
    rows: Vec<(Board, Observation)>,
}
impl FinalProofs {
    pub(crate) fn valid_at(&self, pending: &RecoveryPending, generation: u64,
        now: crate::final_proof_clock::Now) -> bool {
        let now = match now {
            crate::final_proof_clock::Now::Wall(wall) => Stamp { wall, model_ms: None },
            crate::final_proof_clock::Now::Model(model_ms) => Stamp { wall: WallInstant::now(), model_ms: Some(model_ms) },
        };
        self.owner == pending.owner
            && self.generation == generation
            && self.rows.len() == pending.targets.len()
            && pending.targets.iter().all(|target| {
                self.rows
                    .iter()
                    .filter(|(board, _)| board.sender_id() == target.board)
                    .count()
                    == 1
                    && self
                        .rows
                        .iter()
                        .find(|(board, _)| board.sender_id() == target.board)
                        .is_some_and(|(board, observation)| {
                            let row = &observation.value;
                            let physical = matches!(
                                board,
                                Board::ActuatorBoard | Board::ValveBoard | Board::FlightComputer
                            );
                            observation.fresh(now)
                                && row.result == 0
                                && row.boot == target.boot
                                && target.boot != u64::MAX
                                && row.epoch == target.epoch + 1
                                && row.state == Some(target.desired)
                                && row.flags & 0x25 == 0x24
                                && if physical {
                                    row.flags & 0x1a == 0x1a
                                        && safe_output_mask(*board) == Some(row.outputs)
                                } else {
                                    row.flags & 0x1a == 0x08
                                }
                        })
            })
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepare {
    action: String,
    board: Option<String>,
    mode: Option<String>,
    #[serde(default)]
    boards: Vec<String>,
    backend_instance: String,
    reset_epoch: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Confirm {
    ticket: String,
    backend_instance: String,
    principal_context: String,
    reset_epoch: String,
    confirmation: String,
}
pub(crate) fn manual_utc_idle(state: &AppState) -> bool {
    state
        .board_recovery
        .coordinator
        .0
        .try_lock()
        .ok()
        .is_some_and(|control| {
            !control.shutdown_closed && control.ticket.is_none()
                && !control
                    .operation
                    .as_ref()
                    .is_some_and(|v| v["completed"] == false)
        })
}
/// Read-only health may resume after a confirmation expires. This does not
/// clear its ticket or relax the separate manual UTC/recovery ownership guard.
/// Serialize external QUERY admission with sealing CONFIRM. Once an operation
/// owns recovery, only its private query path may emit later verification queries.
pub(super) fn admit_external_query<T>(state:&AppState,offer:impl FnOnce()->T)->Option<T> {
    let control=state.board_recovery.coordinator.0.lock().unwrap();
    if control.shutdown_closed || control.operation.as_ref().is_some_and(|row|row["completed"]==false) {
        return None;
    }
    Some(offer())
}
pub(crate) fn readonly_query_idle(state: &AppState) -> bool {
    state
        .board_recovery
        .coordinator
        .0
        .try_lock()
        .ok()
        .is_some_and(|control| {
            !control.shutdown_closed
                && control
                    .ticket
                    .as_ref()
                    .is_none_or(|ticket| ticket.created.elapsed() >= CONFIRM_TTL)
                && !control
                    .operation
                    .as_ref()
                    .is_some_and(|value| value["completed"] == false)
        })
}
pub(crate) fn retained_boundary(state: &AppState) -> Result<Option<u64>, &'static str> {
    let active = state
        .board_recovery
        .coordinator
        .0
        .try_lock()
        .map_err(|_| "Recovery admission busy")?
        .boundary;
    let history = state
        .launch_history
        .try_lock()
        .map_err(|_| "Recovery journal busy")?;
    let durable = history
        .recovery_pending()
        .map(|marker| marker.origin_utc_ms);
    let ceiling = history
        .retained_manual_utc_ceiling()
        .map_err(|_| "Manual UTC journal unavailable")?;
    let attempted = state
        .manual_abort_intent
        .try_known_highwater()
        .ok_or("Manual Stop owner busy")?;
    Ok(active
        .into_iter()
        .chain(durable)
        .chain([ceiling, attempted])
        .filter(|v| *v != 0)
        .max())
}

fn random() -> Result<u64, &'static str> {
    let mut bytes = [0; 8];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "Entropy unavailable")?;
    let value = u64::from_le_bytes(bytes);
    if matches!(value, 0 | u64::MAX) {
        return Err("Entropy identity unavailable");
    }
    Ok(value)
}
fn stop_unchanged(state: &AppState, token: Token) -> bool {
    state
        .manual_abort_intent
        .admit_if_unchanged(token.manual_epoch, || {
            state
                .deferred_abort
                .admit_if_no_manual_since(token.observed_manual_epoch, || ())
        })
        .flatten()
        .is_some()
}
fn origin_current(state: &AppState, token: Token) -> bool {
    token.generation != 0
        && token.generation != u64::MAX
        && crate::gse::automatic_generation(state) == token.generation
        && state.operator_queue.relief_epoch() == token.relief_epoch
        && stop_unchanged(state, token)
}
pub(super) fn host_safe(state: &AppState) -> bool {
    matches!(
        state.local_flight_state_snapshot(),
        crate::types::FlightState::Startup
            | crate::types::FlightState::Idle
            | crate::types::FlightState::Aborted
    ) && crate::gse::verified_recovery_host_idle(state)
}
/// HITL and test-fire operators may reset from any point up to and including
/// Launch once host work has drained. Flight builds never can. This grants only
/// a fresh board transaction, never completion inferred from host time.
pub(super) fn host_action_allowed(state: &AppState, action: &str) -> bool {
    if host_safe(state) { return true; }
    #[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
    if action == "reset_startup"
        && {
            let flight_state = state.local_flight_state_snapshot();
            flight_state == crate::types::FlightState::Launch
                || crate::gse::prelaunch_flight_state(flight_state)
        }
        && crate::gse::operator_reset_host_idle(state)
    {
        return true;
    }
    let _ = action;
    false
}
/// The reset itself already cleared retained Launch inhibition. Finish what the
/// operator asked for: a stopped T-clock, an unlatched Launch button and, in
/// HITL where nothing publishes it automatically, the Idle phase. Test-fire
/// reaches Idle through its automatic Startup transition. The boards are already
/// verified, so a clock failure is reported rather than failing the reset.
/// Flight builds keep their T0 across a reset.
#[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
fn finish_operator_reset(state: &AppState) {
    if let Err(error) = state.try_reset_launch_clock() {
        state.add_notification(format!("Boards reset, but the T-clock did not reset: {error}"));
    }
    state.set_launch_indicator_latched(false);
    #[cfg(feature = "hitl_mode")]
    if state.local_flight_state_snapshot() == crate::types::FlightState::Startup {
        state.set_local_flight_state(crate::types::FlightState::Idle);
    }
}
fn context_same(a: &Readback, b: &Readback) -> bool {
    // A dedicated fresh QUERY necessarily has a different nonce and uptime.
    a.boot == b.boot
        && a.epoch == b.epoch
        && a.state == b.state
        && a.result == b.result
        && a.flags == b.flags
        && a.outputs == b.outputs
}
fn original_query_remaining(targets:&[Target],now:Stamp)->Duration {
    targets.iter().filter(|target| target.first_binding.is_none()&&target.context.boot!=u64::MAX)
        .map(|target| READBACK_TTL.saturating_sub(target.prepared_query.age(now)))
        .min().unwrap_or(CONFIRM_TTL)
}
fn normal_pre_exchange_query(target:&Target,now:Stamp)->Result<Option<Observation>,&'static str> {
    if target.first_binding.is_some()||target.context.boot==u64::MAX {return Ok(None);}
    if !target.prepared_query.fresh(now) {return Err("Original preparation query expired; prepare and confirm a fresh recovery intent");}
    Ok(Some(target.prepared_query.clone()))
}
impl Runtime {
    pub(super) fn seal_shutdown_and_take_owner(&self) -> Option<JoinHandle<()>> {
        let mut control = self.0.lock().unwrap();
        control.shutdown_closed = true;
        control.ticket = None; // Only a proposal, never an admitted operation.
        control.task.take()
    }
    pub(super) fn shutdown_clean_eligible(&self) -> bool {
        let control = self.0.lock().unwrap();
        control.shutdown_closed && !control.shutdown_failure
    }
    pub(super) fn operation(&self) -> Option<Value> {
        self.0.lock().unwrap().operation.clone()
    }
    pub(super) fn confirmation(&self, state: &AppState, principal: &str) -> Option<Value> {
        let control = self.0.lock().unwrap();
        if control.shutdown_closed { return None; }
        let ticket = control.ticket.as_ref()?;
        let remaining = CONFIRM_TTL.checked_sub(ticket.created.elapsed())?
            .min(original_query_remaining(&ticket.targets,Stamp::now()));
        if remaining.is_zero(){return None;}
        if ticket.principal != principal
            || !origin_current(state, ticket.token)
            || ticket.reset_selection.is_some_and(|selection| selection.current(state).is_err())
        || ticket.mode != crate::station_mode::local_mode()
            || !host_action_allowed(state, &ticket.action)
            || state.launch_history.lock().unwrap().snapshot() != ticket.clock
        {
            return None;
        }
        Some(
            json!({"ticket":ticket.id,"backend_instance":ticket.backend,"principal_context":ticket.principal,"reset_epoch":format!("{:016x}",ticket.token.generation),"action":ticket.action,"targets":ticket.targets.iter().map(|target|target.board.sender_id()).collect::<Vec<_>>(),"expires_in_ms":remaining.as_millis() as u64}),
        )
    }
    fn busy(&self) -> bool {
        self.0
            .lock()
            .unwrap()
            .operation
            .as_ref()
            .is_some_and(|v| v["completed"] == false)
    }
    fn expected(&self, board: Board, nonce: u64) {
        let mut control = self.0.lock().unwrap();
        control.expected = Some((board, nonce));
        control.inbox.clear();
    }
    fn pop(&self) -> Option<Packet> {
        self.0.lock().unwrap().inbox.pop_front()
    }
}
pub(super) fn retain_status(state: &AppState, packet: &Packet) {
    let Some(board) = state.board_from_network_sender(packet.sender()) else {
        return;
    };
    if packet.data_type() != crate::telemetry_schema::data_type(STATUS_TYPE)
        || packet.endpoints() != [crate::telemetry_schema::endpoint("GROUND_STATION")]
    {
        return;
    }
    let Some(row) = Readback::decode(packet.payload()) else {
        return;
    };
    let mut control = state.board_recovery.coordinator.0.lock().unwrap();
    if control.expected == Some((board, row.nonce)) && control.inbox.len() < 16 {
        control.inbox.push_back(packet.clone());
    }
}

pub(super) async fn prepare(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    // Recovery can be explicitly retried while CLOSED. Do not use the ordinary
    // ingress helper that deliberately returns no origin in that state.
    let token = Token::capture(&state, 0);
    // Capture immutable policy before authentication can wait, but preserve
    // authorization response precedence for malformed/unsupported requests.
    let parsed = serde_json::from_value::<Prepare>(body);
    let selected = match parsed.as_ref() {
        Ok(request) if request.action == "reset_startup" =>
            super::scope::Selection::capture(&state, &request.boards).map(Some),
        Ok(request) if !request.boards.is_empty() =>
            Err("Optional proof targets apply only to the explicit global Startup reset"),
        _ => Ok(None),
    };
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::SendCommands).await {
            Ok(value) => value,
            Err(response) => return response,
        };
    if let Err(response) = primary_only() {
        return response;
    }
    let request = match parsed {
        Ok(request) => request,
        Err(_) => return error(StatusCode::CONFLICT, "invalid_intent", "A complete explicit recovery intent is required."),
    };
    let reset_selection = match selected {
        Ok(selection) => selection,
        Err(message) => return error(StatusCode::CONFLICT, "reset_policy_unavailable", message),
    };
    if let Err(message) = crate::manual_abort_utc::before_prepare(&state).await {
        return error(StatusCode::CONFLICT, "manual_utc_pending", message);
    }
    // Ceiling waiting can consume the two-second observation window. Refresh
    // dedicated queries now, under the same explicit request's original origin.
    if let Err(message) = refresh_prepare_queries(&state, &headers, &request, token, reset_selection).await {
        return error(StatusCode::CONFLICT, "fresh_query_required", message);
    }
    let result = prepare_inner(&state, &headers, &principal, request, token, reset_selection);
    match result {
        Ok(value) => Json(value).into_response(),
        Err(message) => error(StatusCode::CONFLICT, "recovery_unavailable", message),
    }
}
async fn refresh_prepare_queries(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    request: &Prepare,
    token: Token,
    reset_selection: Option<super::scope::Selection>,
) -> Result<(), &'static str> {
    if let Some(selection) = reset_selection { selection.current(state)?; }
    let boards = match request.action.as_str() {
        "reset_startup" if request.board.is_none() => reset_selection.ok_or("Reset policy missing")?.boards(),
        "reset_startup_all7" if request.board.is_none() && request.boards.is_empty() => BOARDS.to_vec(),
        "recover_abort" => vec![
            Board::from_sender_id(request.board.as_deref().ok_or("Select a supported MCU board")?)
                .filter(|board| targeted_recovery_state(*board).is_some())
                .ok_or("Unsupported board")?,
        ],
        _ => return Err("Unsupported recovery action"),
    };
    for board in boards {
        // Background status collection may already own this read-only slot.
        // Wait for that request to finish/expire, then create a DISTINCT nonce;
        // its old reply is never reused as the explicit preparation's proof.
        let waiting_since = WallInstant::now();
        loop {
            if !origin_current(state, token) {
                return Err("Recovery origin changed while waiting");
            }
            let now = Stamp::now();
            let pending = state.board_recovery.inner.lock().unwrap().queries[index(board).unwrap()]
                .as_ref().is_some_and(|query| !query.received && now.checked_duration_since(query.sent)
                    .is_none_or(|age| age <= QUERY_TTL));
            if !pending { break; }
            if waiting_since.elapsed() >= QUERY_TTL {
                return Err("Previous read-only query has not completed; preparation timed out");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        admit_prepare_query(
            || {
                if !origin_current(state, token) {
                    return Err("Recovery origin changed while admitting query");
                }
                // Read-only: finish the frozen cohort so one refusal reveals every
                // Abort episode. prepare_inner still fences the changed selection.
                Ok(())
            },
            || super::query(
                State(state.clone()),
                headers.clone(),
                Json(QueryRequest { board: board.sender_id().into() }),
            ),
        ).await?;
        let nonce = state.board_recovery.inner.lock().unwrap().queries[index(board).unwrap()]
            .as_ref()
            .ok_or("Query missing")?
            .nonce;
        let start = WallInstant::now();
        loop {
            if !origin_current(state, token) {
                return Err("Stop/epoch changed before challenge");
            }
            let now = Stamp::now();
            let fresh = state.board_recovery.inner.lock().unwrap().values[index(board).unwrap()]
                .as_ref()
                .is_some_and(|row| row.value.nonce == nonce && row.fresh(now));
            if fresh {
                break;
            }
            if start.elapsed() > QUERY_TTL {
                return Err("Fresh query timed out before challenge");
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
    Ok(())
}
// Retry only a rejected, never-owned read-only offer, before Confirm seals UTC.
// Accepted queries, RECOVER and post-confirmation queries never retry here.
async fn admit_prepare_query<C, F, Fut>(mut current: C, mut offer: F) -> Result<(), &'static str>
where
    C: FnMut() -> Result<(), &'static str>,
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Response>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
    loop {
        current()?;
        let response = tokio::time::timeout_at(deadline, offer()).await
            .map_err(|_| "Fresh dedicated QUERY admission remained busy; Query and prepare again")?;
        if response.status() == StatusCode::OK {
            return Ok(());
        }
        if response.extensions().get::<super::QueryAdmissionCoreBusy>().is_none() {
            return Err("Fresh dedicated QUERY could not be admitted; Query and prepare again");
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("Fresh dedicated QUERY admission remained busy; Query and prepare again");
        }
        tokio::time::sleep_until(
            (tokio::time::Instant::now() + Duration::from_millis(10)).min(deadline),
        ).await;
    }
}

fn prepare_inner(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    principal: &crate::auth::AuthPrincipal,
    request: Prepare,
    token: Token,
    reset_selection: Option<super::scope::Selection>,
) -> Result<Value, &'static str> {
    if let Some(selection) = reset_selection { selection.current(state)?; }
    let identity = state
        .board_recovery
        .identity
        .ok_or("Backend identity unavailable")?;
    if request.backend_instance != hex(&identity)
        || request.reset_epoch != format!("{:016x}", token.generation)
        || !origin_current(state, token)
        || !host_action_allowed(state, &request.action)
        || state.board_recovery.coordinator.busy()
        || principal.anonymous
        || request.mode.is_some()
    {
        return Err("Recovery origin/session changed or host work is active. Recovery sends no Stop; selected boards must independently verify recovery eligibility.");
    }
    let router = state.topology_router.get().ok_or("Router unavailable")?;
    crate::system_clock::required_control_network_utc_ms(router)
        .map_err(|_| "Network UTC unavailable")?;
    let boards = match request.action.as_str() {
        "reset_startup" if request.board.is_none() => reset_selection.ok_or("Reset policy missing")?.boards(),
        "reset_startup_all7" if request.board.is_none() && request.boards.is_empty() => BOARDS.to_vec(),
        "recover_abort" => vec![
            Board::from_sender_id(request.board.as_deref().ok_or("Select a supported MCU board")?)
                .filter(|board| targeted_recovery_state(*board).is_some())
                .ok_or("Unsupported targeted application recovery board")?,
        ],
        _ => return Err("Unsupported recovery action; runtime mode switching is unavailable"),
    };
    let retained = state.launch_history.lock().unwrap().recovery_pending();
    let now = Stamp::now();
    let topology = router.export_topology();
    let inner = state.board_recovery.inner.lock().unwrap();
    let mut targets = Vec::new();
    for board in boards {
        let row = inner.values[index(board).unwrap()]
            .as_ref()
            .filter(|row| row.fresh(now))
            .ok_or("Explicit fresh dedicated QUERY is required")?;
        let first_binding = retained
            .as_ref()
            .and_then(|pending| FirstBinding::from_pending(pending, board, &row.value));
        if !endpoint_verified(state, board, &topology)
            || !query_domain_consistent(board, &row.value)
            || !matches!(row.value.result, 0 | 7)
            || row.value.flags & 0x24 != 0x24
            || row.value.boot == 0 && first_binding.is_none()
            || row.value.epoch == 0
            || row.value.epoch >= u32::MAX - 1
        {
            return Err("Board does not advertise a usable hardened recovery context");
        }
        let desired = if request.action == "recover_abort" {
            targeted_recovery_state(board).ok_or("Unsupported targeted application recovery board")?
        } else {
            0
        };
        if row.value.boot == u64::MAX
            && retained
                .as_ref()
                .filter(|pending| !pending.published)
                .and_then(|pending| {
                    pending
                        .targets
                        .iter()
                        .find(|target| target.board == board.sender_id())
                })
                .is_some_and(|target| {
                    target.kind == RecoveryIntentKind::ReconcileFirst
                        || target.uncertain_first.is_some()
                })
        {
            return Err("Retained FIRST uncertainty cannot regain virgin MAX eligibility");
        }
        let uncertain_first = first_binding
            .as_ref()
            .map(|binding| binding.uncertain.clone())
            .or_else(|| {
                retained
                    .as_ref()
                    .filter(|pending| !pending.published)
                    .and_then(|pending| {
                        pending
                            .targets
                            .iter()
                            .find(|target| target.board == board.sender_id())
                    })
                    .and_then(|target| target.uncertain_first.clone())
            });
        let original_first = first_binding
            .as_ref()
            .map(|binding| binding.original.clone())
            .or_else(|| {
                retained
                    .as_ref()
                    .filter(|pending| !pending.published)
                    .and_then(|pending| {
                        pending
                            .targets
                            .iter()
                            .find(|target| target.board == board.sender_id())
                    })
                    .and_then(|target| target.original_first.clone())
            });
        targets.push(Target {
            board,
            desired,
            context: row.value.clone(),
            prepared_query: row.clone(),
            intent: None,
            original_first,
            uncertain_first,
            first_binding,
        });
    }
    drop(inner);
    // An interrupted operation cannot be forgotten by choosing a smaller set.
    let history = state.launch_history.lock().unwrap();
    if let Some(pending) = history.recovery_pending().filter(|p| !p.published)
        && pending
            .targets
            .iter()
            .any(|old| !targets.iter().any(|new| new.board.sender_id() == old.board))
        {
            return Err(
                "Explicit retry must include every target retained in the durable pending recovery",
            );
        }
    if !history.recovery_available() {
        return Err(
            "Healthy durable primary journal required; unreadable history cannot be repaired by board recovery",
        );
    }
    let prior_inhibition = history
        .recovery_pending()
        .filter(|marker| !marker.published)
        .map_or(history.automatic_actions_inhibited(), |marker| {
            marker.prior_automatic_actions_inhibited
        });
    let clock = history.snapshot();
    drop(history);
    let id = format!("{:016x}{:016x}", random()?, random()?);
    let names = targets
        .iter()
        .map(|t| t.board.sender_id())
        .collect::<Vec<_>>();
    let commissioning = targets
        .iter()
        .any(|t| t.context.boot == u64::MAX || t.context.result == 7 || t.first_binding.is_some());
    let phrase = format!(
        "{} {}{}",
        if matches!(request.action.as_str(), "reset_startup" | "reset_startup_all7") {
            "RESET ALL STARTUP; VERIFY"
        } else {
            "RECOVER"
        },
        if names.is_empty() { "NONE".into() } else { names.join(" ") },
        if commissioning { " COMMISSION" } else { "" }
    );
    let context = principal_context(&identity, headers, principal);
    let value = json!({"ticket":id,"backend_instance":hex(&identity),"principal_context":context,"reset_epoch":format!("{:016x}",token.generation),"expires_in_ms":original_query_remaining(&targets,Stamp::now()).as_millis() as u64,"action":request.action,"targets":names,"global_startup_best_effort":matches!(request.action.as_str(),"reset_startup"|"reset_startup_all7"),"required_proof_boards":reset_selection.map(|s|s.required_boards()),"unselected_board_verification":"unverified_best_effort","summary":if targets.iter().any(|target| target.first_binding.is_some()) {"Identity reconciliation ONLY for retained uncertain FIRST targets. Other targets will not reset/apply in this attempt. IO cannot clear state; a fresh Query and another distinct human confirmation are required."}else if commissioning{"Explicit commissioning leg; IO is not completion. A separate fresh confirmation is required after provisioning."}else if request.action=="recover_abort" {match targets.first().map(|target|target.board) {Some(Board::FlightComputer)=>"Reset/recover FC to Startup with verified evaluator and safe-output ownership. Preserve deployment history; a separate new flight is required. No MCU reboot or output replay.",Some(Board::RFBoard|Board::PowerBoard|Board::GatewayBoard|Board::DaqBoard)=>"Reset only selected cached application metadata to Startup. No physical recovery, power cycling, acquisition reset, reboot or resync.",_=>"Recover selected AB/VB application state to Idle with verified safe outputs. A completed launch can recover directly; active launch work must finish or be explicitly stopped first. No MCU reboot or physical-position proof."}}else{"Explicit GLOBAL Startup broadcast to ALL boards (best effort). Selected boards are verified; active launch work must finish or be explicitly stopped first. Clock, calibration and network power are preserved. Unselected boards remain unverified."},"confirmation":phrase});
    let mut control = state.board_recovery.coordinator.0.lock().unwrap();
    if control.shutdown_closed || state.shutdown_requested.load(Ordering::Acquire) { return Err("Ground station is shutting down"); }
    if control
        .operation
        .as_ref()
        .is_some_and(|v| v["completed"] == false)
    {
        return Err("Another recovery owns admission");
    }
    control.ticket = Some(Ticket {
        id,
        principal: context,
        backend: hex(&identity),
        mode: crate::station_mode::local_mode(),
        created: WallInstant::now(),
        boundary: None,
        prior_inhibition,
        token,
        clock,
        action: request.action,
        phrase,
        targets,
        reset_selection,
    });
    Ok(value)
}

pub(super) async fn confirm(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let origin = Token::capture(&state, 0);
    // Immutable UTC and monotonic deadline are captured BEFORE auth can wait.
    let boundary = retained_boundary(&state).ok().and_then(|floor| {
        state
            .topology_router
            .get()
            .and_then(|router| super::exchange::ConfirmBoundary::capture(router).ok())
            .filter(|boundary| floor.is_none_or(|floor| boundary.utc() > floor))
    });
    #[cfg(test)]
    tests::notify_confirm_received(&state);
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::SendCommands).await {
            Ok(value) => value,
            Err(response) => return response,
        };
    if let Err(response) = primary_only() {
        return response;
    }
    let request: Confirm = match serde_json::from_value(body) {
        Ok(value) => value,
        Err(_) => {
            return error(
                StatusCode::CONFLICT,
                "invalid_confirmation",
                "Complete frozen ticket and typed confirmation required.",
            );
        }
    };
    let response = admit_confirmation(state.clone(), headers, principal, request, origin, boundary);
    #[cfg(test)]
    if response.status() == StatusCode::OK {
        tests::pause_response(&state).await;
    }
    response
}
fn admit_confirmation(
    state: Arc<AppState>,
    headers: HeaderMap,
    principal: crate::auth::AuthPrincipal,
    request: Confirm,
    origin: Token,
    boundary: Option<super::exchange::ConfirmBoundary>,
) -> Response {
    let Some(identity) = state.board_recovery.identity else {
        return error(
            StatusCode::CONFLICT,
            "identity_unavailable",
            "Backend identity unavailable",
        );
    };
    let mut control = state.board_recovery.coordinator.0.lock().unwrap();
    if control.shutdown_closed || state.shutdown_requested.load(Ordering::Acquire) { return error(StatusCode::SERVICE_UNAVAILABLE,"shutdown_closed","Ground station is shutting down"); }
    let Some(ticket) = control.ticket.as_ref() else {
        return error(
            StatusCode::CONFLICT,
            "ticket_unavailable",
            "Prepare a fresh explicit intent.",
        );
    };
    if ticket.created.elapsed() > CONFIRM_TTL
        || ticket.id != request.ticket
        || ticket.backend != request.backend_instance
        || ticket.principal != request.principal_context
        || ticket.principal != principal_context(&identity, &headers, &principal)
        || ticket.token.generation != origin.generation
        || !origin_current(&state, ticket.token)
        || request.reset_epoch != format!("{:016x}", ticket.token.generation)
        || ticket.phrase != request.confirmation
        || ticket.reset_selection.is_some_and(|selection| selection.current(&state).is_err())
        || ticket.mode != crate::station_mode::local_mode()
        || !host_action_allowed(&state, &ticket.action)
        || state.launch_history.lock().unwrap().snapshot() != ticket.clock
        || control
            .operation
            .as_ref()
            .is_some_and(|v| v["completed"] == false)
    {
        return error(
            StatusCode::CONFLICT,
            "stale_confirmation",
            "Ticket expired or immutable intent/session/Stop context changed.",
        );
    }
    if ticket.targets.iter().any(|target| target.first_binding.is_none()
        && target.context.boot != u64::MAX && !target.prepared_query.fresh(Stamp::now())) {
        return error(StatusCode::CONFLICT,"preparation_query_expired",
            "Original preparation query expired; prepare and confirm a fresh recovery intent.");
    }
    let boundary = match boundary.filter(|boundary| {
        state
            .topology_router
            .get()
            .is_some_and(|router| boundary.current(router, Stamp::now()))
    }) {
        Some(boundary) => boundary,
        None => {
            return error(
                StatusCode::CONFLICT,
                "boundary_expired",
                "Original CONFIRM source-time boundary is unavailable or exceeded its five-second deadline. Prepare a new explicit intent; no restamping.",
            );
        }
    };
    let manual_floor = match crate::manual_abort_utc::retained_bound(&state) {
        Ok(bound) => bound,
        Err(_) => {
            return error(
                StatusCode::CONFLICT,
                "manual_utc_unavailable",
                "Manual UTC history/owner busy; prepare a new intent",
            );
        }
    };
    let durable_floor = state
        .launch_history
        .lock()
        .unwrap()
        .recovery_pending()
        .map(|marker| marker.origin_utc_ms);
    if control
        .boundary
        .into_iter()
        .chain(durable_floor)
        .chain([manual_floor])
        .any(|floor| boundary.utc() <= floor)
    {
        return error(
            StatusCode::CONFLICT,
            "boundary_not_newer",
            "A new canonical boundary strictly newer than retained recovery is required; no timestamp is fabricated.",
        );
    }
    let mut ticket = control.ticket.take().unwrap(); // One-use before any spawned work.
    let owner = match random() {
        Ok(value) => value,
        Err(message) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "entropy_unavailable",
                message,
            );
        }
    };
    ticket.token.owner = owner;
    ticket.boundary = Some(boundary);
    control.boundary = Some(boundary.utc());
    for target in &mut ticket.targets {
        if let Some(binding) = &target.first_binding
            && (boundary.utc() <= binding.prior_boundary
                || boundary.utc() <= binding.uncertain.source_utc_ms)
            {
                return error(
                    StatusCode::CONFLICT,
                    "boundary_not_newer",
                    "A new canonical source boundary newer than the retained FIRST attempt is required.",
                );
            }
        let nonce = match random() {
            Ok(nonce)
                if nonce != target.context.nonce
                    && target
                        .original_first
                        .as_ref()
                        .is_none_or(|first| first.nonce() != nonce)
                    && target
                        .uncertain_first
                        .as_ref()
                        .is_none_or(|first| first.nonce() != nonce) =>
            {
                nonce
            }
            _ => {
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "intent_nonce_unavailable",
                    "Distinct application nonce unavailable",
                );
            }
        };
        let mut wire = [0u8; 32];
        wire[0] = 1;
        wire[1] = 1;
        wire[2] = target.desired;
        wire[4..12].copy_from_slice(&nonce.to_le_bytes());
        wire[12..20].copy_from_slice(
            &if target.first_binding.is_some() {
                u64::MAX
            } else {
                target.context.boot
            }
            .to_le_bytes(),
        );
        wire[20..24].copy_from_slice(&target.context.epoch.to_le_bytes());
        wire[24..32].copy_from_slice(&target.context.issuer.to_le_bytes());
        let intent = RecoveryIntent {
            body: wire,
            source_utc_ms: boundary.utc(),
            backend: ticket.backend.clone(),
            principal_context: ticket.principal.clone(),
        };
        if target.context.boot == u64::MAX && target.original_first.is_none() {
            target.original_first = Some(intent.clone());
        }
        target.intent = Some(intent);
    }
    let operation = json!({"id":ticket.id,"action":ticket.action,"phase":"quiescing","targets":ticket.targets.iter().map(|t|t.board.sender_id()).collect::<Vec<_>>(),"global_startup_best_effort":matches!(ticket.action.as_str(),"reset_startup"|"reset_startup_all7"),"selected_proof_count":ticket.targets.len(),"unselected_board_verification":if matches!(ticket.action.as_str(),"reset_startup"|"reset_startup_all7"){"unverified_best_effort"}else{"not_requested"},"completed":false,"success":null,"age_ms":0,"reset_applied_state":null,"message":"Owned coordinator admitted; actual board completion pending."});
    let old_task = control.task.take();
    control.operation = Some(operation.clone());
    let owned = state.clone();
    control.task = Some(tokio::spawn(async move {
        // A terminal prior worker has no remaining board/journal operation.
        // Join it before transferring its closed owner or replacing its proxy.
        if let Some(old) = old_task
            && old.await.is_err() { owned.board_recovery.coordinator.0.lock().unwrap().shutdown_failure = true; }
        let result = run(&owned, &headers, &ticket).await;
        let mut control = owned.board_recovery.coordinator.0.lock().unwrap();
        control.expected = None;
        control.inbox.clear();
        if let Some(operation) = control.operation.as_mut() {
            operation["phase"] = json!(if result.is_ok() {
                "completed"
            } else {
                "failed"
            });
            operation["completed"] = json!(true);
            operation["success"] = json!(result.is_ok());
            operation["reset_applied_state"] =
                json!(if result.is_ok() && matches!(ticket.action.as_str(), "reset_startup" | "reset_startup_all7") {
                    Some(0u8)
                } else {
                    None
                });
            operation["current_host_state"] = json!(owned.local_flight_state_snapshot() as u8);
            operation["age_ms"] = json!(ticket.created.elapsed().as_millis() as u64);
            operation["message"] = json!(match result {
                Ok(()) if matches!(ticket.action.as_str(), "reset_startup" | "reset_startup_all7") && ticket.targets.is_empty() =>
                    "Host Startup reset completed. Global Startup broadcast was best effort; no board terminal completion or fresh board proof is claimed.",
                Ok(()) if matches!(ticket.action.as_str(), "reset_startup" | "reset_startup_all7") =>
                    "Host Startup reset completed after terminal completion and fresh final readbacks from the listed proof targets. Global Startup broadcast was best effort; unlisted boards remain unverified. Physical position is not certified.",
                Ok(()) =>
                    "Selected board application terminal completion and fresh final readback verified. Unselected boards were not reset by this targeted recovery. Physical position is not certified.",
                Err(message) => message,
            });
        }
        drop(control);
        if result.is_ok() {
            owned.board_recovery.manual_utc.after_completed();
        }
    }));
    Json(json!({"accepted":true,"operation":operation,"message":"Owned operation retained independently of this HTTP request."})).into_response()
}

async fn authorize(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    ticket: &Ticket,
) -> Result<(), &'static str> {
    let principal = crate::web::authorize_headers(state, headers, Permission::SendCommands)
        .await
        .map_err(|_| "Command permission/session expired or was revoked")?;
    primary_only().map_err(|_| "Primary authority changed")?;
    let identity = state
        .board_recovery
        .identity
        .ok_or("Backend identity unavailable")?;
    if principal.anonymous
        || ticket.principal != principal_context(&identity, headers, &principal)
        || ticket.mode != crate::station_mode::local_mode()
        || !host_action_allowed(state, &ticket.action)
    {
        return Err("Recovery principal/mode/safe host state changed");
    }
    if let Some(selection) = ticket.reset_selection { selection.current(state)?; }
    if let Some(boundary) = ticket.boundary {
        boundary.clock_failure().current().map_err(|error| error.message())?;
    }
    Ok(())
}
async fn wait_prefix(state: &Arc<AppState>, fence: &RecoveryFence) -> Result<(), &'static str> {
    let start = WallInstant::now();
    loop {
        let ready = state
            .launch_history
            .lock()
            .unwrap()
            .recovery_quiescent(fence)
            .map_err(|_| "Journal writer failed; admission remains closed")?;
        if ready {
            return Ok(());
        }
        if start.elapsed() > QUIESCE_TTL {
            return Err("Journal timeout is not writer quiescence; admission remains closed");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
async fn verify_marker(state: &Arc<AppState>, fence: &RecoveryFence) -> Result<(), &'static str> {
    wait_prefix(state, fence).await?;
    let (path, expected) = state
        .launch_history
        .lock()
        .unwrap()
        .recovery_durable_snapshot(fence)
        .map_err(|_| "Durable recovery prefix unavailable")?;
    tokio::task::spawn_blocking(move || {
        crate::launch_history::Store::verify_recovery_file(&path, &expected)
    })
    .await
    .map_err(|_| "Recovery readback worker failed")?
    .map_err(|_| "Recovery marker disk readback failed")
}
async fn send(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    ticket: &Ticket,
    router: &sedsnet::router::Router,
    token: Token,
    packet: Packet,
    valid: impl Fn() -> Result<(), &'static str>,
) -> Result<(), &'static str> {
    let started=WallInstant::now();
    loop {
        // The same immutable SDK envelope has no owner on core-busy. Its
        // original operation deadline and current authority are checked again
        // before each bounded retry, outside every Stop/dispatch gate.
        authorize(state,headers,ticket).await?;
        valid()?;
        if started.elapsed()>QUERY_TTL {return Err("SDK core remained busy through recovery admission deadline");}
        let attempt_packet=packet.clone();
        let result=crate::gse::with_closed_operator_queue(state,token,||router.try_tx_queue(attempt_packet))
            .ok_or("Stop/owner changed before SDK admission")?;
        match result {
            Ok(())=>return Ok(()),
            Err(sedsnet::TelemetryError::Io("router core busy"))=>tokio::time::sleep(Duration::from_millis(10)).await,
            Err(_)=>return Err("SDK refused recovery ownership"),
        }
    }
}
async fn fresh_query(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    ticket: &Ticket,
    token: Token,
    board: Board,
    router: &Arc<sedsnet::router::Router>,
) -> Result<Observation, &'static str> {
    authorize(state, headers, ticket).await?;
    if !endpoint_verified(state, board, &router.export_topology()) {
        return Err("Dedicated hardened endpoint disappeared");
    }
    let nonce = random()?;
    let now = Stamp::now();
    let mut wire = [0u8; 32];
    wire[0] = 1;
    wire[4..12].copy_from_slice(&nonce.to_le_bytes());
    wire[24..32].copy_from_slice(&issuer(&state.board_recovery.identity.unwrap()).to_le_bytes());
    let packet = Packet::new(
        crate::telemetry_schema::data_type(QUERY_TYPE),
        &[crate::telemetry_schema::endpoint(endpoint(board))],
        &router.sender(),
        crate::system_clock::required_control_network_utc_ms(router)
            .map_err(|_| "Network UTC unavailable")?,
        Arc::from(wire),
    )
    .map_err(|_| "Query packet rejected")?;
    state.board_recovery.coordinator.expected(board, nonce);
    state.board_recovery.inner.lock().unwrap().queries[index(board).unwrap()] = Some(Query {
        abort_revision: state.board_abort.query_revision(board),
        nonce,
        sent: now,
        received: false,
    });
    send(state,headers,ticket,router,token,packet,|| {
        Stamp::now().checked_duration_since(now).filter(|age|*age<=QUERY_TTL)
            .map(|_|()).ok_or("Original dedicated query expired before SDK ownership")
    }).await?;
    let start = WallInstant::now();
    loop {
        if crate::gse::with_closed_operator_queue(state, token, || ()).is_none() {
            return Err("Stop superseded dedicated query");
        }
        if let Some(packet) = state.board_recovery.coordinator.pop() {
            let row = Readback::decode(packet.payload()).ok_or("Malformed query reply")?;
            if row.issuer != issuer(&state.board_recovery.identity.unwrap()) {
                return Err("Query issuer changed");
            }
            let observation = Observation {
                value: row,
                received: Stamp::now(),
                observed_after: now,
            };
            if !observation.fresh(Stamp::now()) {
                return Err("Dedicated query exceeded actual readback freshness");
            }
            return Ok(observation);
        }
        if start.elapsed() > QUERY_TTL {
            return Err("Dedicated query timed out; SDK ACK is not readback");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
enum BoardResult {
    Applied(Readback),
    Io(Readback),
}
async fn exchange(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    ticket: &Ticket,
    token: Token,
    target: &Target,
    row: &Observation,
    router: Arc<sedsnet::router::Router>,
) -> Result<BoardResult, &'static str> {
    use super::exchange::{Exchange, Outcome};
    let intent = target
        .intent
        .as_ref()
        .ok_or("Sealed durable request missing")?;
    let boundary = ticket.boundary.ok_or("Confirm boundary unavailable")?;
    let mut exchange = if let Some(binding) = &target.first_binding {
        Exchange::prepare_first_reconciliation_at(
            target.board,
            row,
            intent.nonce(),
            target.desired,
            Stamp::now(),
            router.clone(),
            boundary,
            binding,
        )?
    } else {
        Exchange::prepare_at(
            target.board,
            row,
            intent.nonce(),
            target.desired,
            Stamp::now(),
            router.clone(),
            boundary,
        )?
    };
    if exchange.packet(Stamp::now())?.payload() != intent.body
        || exchange.packet(Stamp::now())?.timestamp() != intent.source_utc_ms
    {
        return Err("Prepared SDK request differs from sealed durable intent");
    }
    state
        .board_recovery
        .coordinator
        .expected(target.board, exchange.nonce);
    authorize(state, headers, ticket).await?;
    send(state,headers,ticket,&router,token,exchange.packet(Stamp::now())?.clone(),
        || {
            if !row.fresh(Stamp::now()) {return Err("Original preparation query expired before SDK admission");}
            exchange.packet(Stamp::now()).map(|_|())
        }).await?;
    let mut next_poll = WallInstant::now() + POLL;
    loop {
        if crate::gse::with_closed_operator_queue(state, token, || ()).is_none() {
            return Err("Stop superseded board recovery");
        }
        while let Some(packet) = state.board_recovery.coordinator.pop() {
            match exchange.observe(&packet, Stamp::now())? {
                Outcome::Pending => (),
                Outcome::Applied => {
                    return Readback::decode(packet.payload())
                        .map(BoardResult::Applied)
                        .ok_or("Malformed terminal reply");
                }
                Outcome::Rejected(7) => {
                    return Readback::decode(packet.payload())
                        .map(BoardResult::Io)
                        .ok_or("Malformed commissioning IO reply");
                }
                Outcome::Rejected(_) => {
                    return Err("Board rejected recovery; no reset applied. Wait for the active launch owner to complete, or explicitly stop it, then Query and confirm a new recovery intent.");
                }
            }
        }
        exchange.packet(Stamp::now())?;
        if WallInstant::now() >= next_poll {
            authorize(state, headers, ticket).await?;
            let packet=exchange.poll_packet(Stamp::now())?;
            send(state,headers,ticket,&router,token,packet,||exchange.packet(Stamp::now()).map(|_|())).await?;
            next_poll = WallInstant::now() + POLL;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
fn sdk_pending(state:&AppState,router:&sedsnet::router::Router)->bool {
    state.has_pending_recovery_control_tx() || crate::telemetry_task::RECOVERY_CONTROL_TYPES.iter()
        .any(|name| !matches!(router.try_has_pending_reliable_tx_type(crate::telemetry_schema::data_type(name)),Ok(false)))
        ||state.has_pending_recovery_control_tx() // Close the SDK→physical ownership handoff window.
}
async fn wait_sdk(
    state: &Arc<AppState>,
    router: &sedsnet::router::Router,
    token: Token,
) -> Result<(), &'static str> {
    let start = WallInstant::now();
    loop {
        if !origin_current(state,token){return Err("Recovery generation, Stop or newer manual control changed");}
        if !sdk_pending(state,router){
            return if origin_current(state,token){Ok(())}else{Err("Recovery generation, Stop or newer manual control changed")};
        }
        if start.elapsed() > QUIESCE_TTL {
            return Err(
                "SDK/local transport still owns work or new Stop; hidden modem retirement is not inferred",
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
async fn run(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    ticket: &Ticket,
) -> Result<(), &'static str> {
    authorize(state, headers, ticket).await?;
    if !origin_current(state, ticket.token) {
        return Err("Original recovery Stop/generation changed");
    }
    let router = state
        .topology_router
        .get()
        .ok_or("Router unavailable")?
        .clone();
    let previous_owner = state.operator_admission_barrier.load(Ordering::Acquire);
    if previous_owner == 0 {
        crate::gse::reset_generation::close_operator_admission(
            state,
            ticket.token.generation,
            ticket.token.owner,
        )?;
    }
    let mut proxy =
        crate::command_proxy::begin_primary_recovery().map_err(|_| "Proxy revocation failed")?;
    let start = WallInstant::now();
    while state.operator_dispatch_inflight.load(Ordering::Acquire) != 0
        || proxy.as_ref().is_some_and(|p| !p.is_quiescent())
    {
        if !origin_current(state, ticket.token) || start.elapsed() > QUIESCE_TTL {
            return Err("Host/proxy lease timeout or new Stop; admission stays closed");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    wait_sdk(state, &router, ticket.token).await?;
    let mut fence = {
        let mut history = state.launch_history.lock().unwrap();
        history
            .inhibit_for_recovery()
            .map_err(|_| "Durable primary journal unavailable")?;
        history
            .close_for_recovery(if previous_owner == 0 {
                ticket.token.owner
            } else {
                previous_owner
            })
            .map_err(|_| "Journal recovery owner unavailable")?
    };
    if previous_owner != 0 {
        fence = crate::gse::reset_generation::transfer_closed_recovery_owner(
            state,
            ticket.token.generation,
            previous_owner,
            ticket.token.owner,
            &fence,
        )?;
    }
    // This is an explicit newly confirmed retry, never automatic repair of a
    // pending writer. Only an actual terminal failed known snapshot can retry.
    if state
        .launch_history
        .lock()
        .unwrap()
        .recovery_quiescent(&fence)
        .is_err()
    {
        fence = state
            .launch_history
            .lock()
            .unwrap()
            .retry_recovery_save(&fence)
            .map_err(|_| "Prior journal failed; explicit known-snapshot retry refused")?;
    }
    wait_prefix(state, &fence).await?;
    let receipt = state
        .operator_queue
        .request(ticket.token)
        .map_err(|_| "Receiver administrative queue unavailable")?;
    tokio::time::timeout(QUERY_TTL, receipt)
        .await
        .map_err(|_| "Receiver drain timed out")?
        .map_err(|_| "Receiver drain owner stopped")??;
    // Pending is durable BEFORE the first board mutation, and startup restores
    // it closed. The marker is not a substitute for actual board completion.
    let pending = RecoveryPending {
        reset_policy: ticket.reset_selection.map(|s| s.policy()),
        owner: ticket.token.owner,
        backend: ticket.backend.clone(),
        generation: ticket
            .token
            .generation
            .checked_add(1)
            .filter(|g| *g != u64::MAX)
            .ok_or("Host generation exhausted")?,
        manual_epoch: ticket.token.manual_epoch,
        observed_manual_epoch: ticket.token.observed_manual_epoch,
        origin_utc_ms: ticket.boundary.ok_or("Confirm boundary unavailable")?.utc(),
        prior_automatic_actions_inhibited: ticket.prior_inhibition,
        targets: ticket
            .targets
            .iter()
            .map(|t| {
                Ok(RecoveryTarget {
                    board: t.board.sender_id().into(),
                    desired: t.desired,
                    boot: if t.first_binding.is_some() {
                        u64::MAX
                    } else {
                        t.context.boot
                    },
                    epoch: t.context.epoch,
                    intent: t.intent.clone().ok_or("Immutable intent missing")?,
                    kind: if t.first_binding.is_some() {
                        RecoveryIntentKind::ReconcileFirst
                    } else if t.context.boot == u64::MAX {
                        RecoveryIntentKind::First
                    } else {
                        RecoveryIntentKind::Normal
                    },
                    original_first: t.original_first.clone(),
                    uncertain_first: t.uncertain_first.clone(),
                })
            })
            .collect::<Result<Vec<_>, &'static str>>()?,
        verified: false,
        published: false,
    };
    fence = state
        .launch_history
        .lock()
        .unwrap()
        .stage_recovery_pending(&fence, pending)
        .map_err(|_| "Recovery pending marker could not be staged")?;
    verify_marker(state, &fence).await?;
    let generation =
        crate::gse::reset_generation::advance_verified_recovery(state, ticket.token, &fence)?;
    let token = Token {
        generation,
        ..ticket.token
    };
    let mut completed = Vec::new();
    let reconcile_only = ticket
        .targets
        .iter()
        .any(|target| target.first_binding.is_some());
    for target in &ticket.targets {
        if reconcile_only && target.first_binding.is_none() {
            continue;
        }
        // NORMAL completion is armed by the dedicated PREPARE query A.
        // CONFIRM seals immutable source B>A. A new QUERY C after confirmation
        // would overwrite that proof and make B<C permanently stale on-device.
        // Do not renew the original observation or confirmation clock after IO.
        let query = if let Some(original)=normal_pre_exchange_query(target,Stamp::now())? {original} else {
            fresh_query(state, headers, ticket, token, target.board, &router).await?
        };
        if !context_same(&target.context, &query.value) {
            return Err("Fresh query differs from immutable board confirmation context");
        }
        match exchange(
            state,
            headers,
            ticket,
            token,
            target,
            &query,
            router.clone(),
        )
        .await?
        {
            BoardResult::Applied(row) if !reconcile_only => completed.push((target.board, row)),
            BoardResult::Applied(_) => {
                return Err("Identity reconciliation cannot apply/reset any target");
            }
            BoardResult::Io(row) if reconcile_only && matches!(row.boot, 0 | 1) => (),
            BoardResult::Io(_) => {
                return Err(
                    "Commissioning/IO is not success: remain closed; explicit fresh QUERY and NEW confirmation required",
                );
            }
        }
    }
    if reconcile_only {
        return Err(
            "Identity reconciliation only; no target was reset/applied. Remain closed. Fresh Query and another distinct human intent are required.",
        );
    }
    let reset_all = matches!(ticket.action.as_str(), "reset_startup" | "reset_startup_all7");
    if reset_all {
        // Persist/prebuild outside DISPATCH/Stop/SDK locks. Queue admission is
        // bounded and Stop-fenced; SDK callbacks run later in telemetry owner.
        let owned = state.clone();
        let source = router.clone();
        let packet = tokio::task::spawn_blocking(move || {
            crate::gse::prepare_verified_startup(&owned)?;
            crate::network_variables::prepare_verified_recovery_startup(&source)
                .map_err(|_| "Managed Startup persistence failed")
        })
        .await
        .map_err(|_| "Startup publication worker failed")??;
        send(state,headers,ticket,&router,token,packet,||Ok(())).await?;
        wait_sdk(state, &router, token).await?;
    }
    authorize(state, headers, ticket).await?;
    #[cfg(all(test, any(feature="hitl_mode",feature="firmware_sim")))]
    tests::pause_clock_marker(state).await;
    fence = state
        .launch_history
        .lock()
        .unwrap()
        .stage_recovery_verified(&fence)
        .map_err(|_| "Verified terminal marker could not be staged")?;
    verify_marker(state, &fence).await?;
    authorize(state, headers, ticket).await?;
    // Durability can take seconds. Refresh ALL final proofs AFTER its wait;
    // never claim old observations remain fresh merely because IO finished.
    let mut rows = Vec::new();
    for (board, terminal) in &completed {
        let query = fresh_query(state, headers, ticket, token, *board, &router).await?;
        if !context_same(terminal, &query.value) {
            return Err("Fresh final QUERY differs from owned terminal completion");
        }
        rows.push((*board, query));
    }
    let proofs = FinalProofs {
        owner: token.owner,
        generation: token.generation,
        rows,
    };
    authorize(state, headers, ticket).await?;
    #[cfg(test)]
    tests::pause_finalization(state).await;
    // Source IO is outside all admission/Stop/history locks. Busy never creates
    // a new confirmation/query/owner or a new proof observation timestamp.
    let clock = crate::final_proof_clock::Prepared::capture(
        ticket.boundary.ok_or("Original confirmation boundary missing")?.clock_failure(),
    ).map_err(|error| error.message())?;
    #[cfg(all(test, any(feature="hitl_mode",feature="firmware_sim")))]
    tests::pause_clock_ready(state).await;
    let mut admission = None;
    loop {
        authorize(state, headers, ticket).await?;
        // Start the NEW 100ms contention-only budget exactly once, immediately
        // before the first final attempt. Prior durability/auth is not a new
        // universal confirmation lifetime, and original proof clocks stay put.
        let clock_admission = admission.get_or_insert_with(|| crate::final_proof_clock::Admission::new(clock));
        let publication = match (reset_all, ticket.reset_selection) {
            (true, Some(selection)) => crate::gse::reset_generation::StartupPublication::ModeAware(selection),
            (true, None) => crate::gse::reset_generation::StartupPublication::LegacyAllSeven,
            (false, _) => crate::gse::reset_generation::StartupPublication::TargetedRecovery,
        };
        match crate::gse::reset_generation::finalize_verified_recovery(
            state, token, &fence, &mut proxy, &proofs, publication, clock_admission,
        ) {
            Ok(()) => break,
            Err(crate::gse::reset_generation::FinalizeError::ClockBusy) => {
                #[cfg(all(test, any(feature="hitl_mode",feature="firmware_sim")))]
                tests::notify_final_clock_busy(state);
                clock_admission.note_busy().map_err(|error| error.message())?;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Err(crate::gse::reset_generation::FinalizeError::Refused(message)) => return Err(message),
        }
    }
    if reset_all {
        state.publish_verified_startup_observation();
    }
    let (path, expected, writer, write_ticket) = state
        .launch_history
        .lock()
        .unwrap()
        .acknowledge_recovery_publication(token.owner)
        .map_err(|_| "Publication acknowledgement could not be staged")?;
    let ack = tokio::task::spawn_blocking(move || {
        writer.wait(write_ticket)?;
        crate::launch_history::Store::verify_recovery_file(&path, &expected)
    })
    .await
    .map_err(|_| "Publication acknowledgement worker failed")?;
    if ack.is_err() {
        let _ = crate::gse::reset_generation::close_operator_admission(
            state,
            token.generation,
            token.owner,
        );
        return Err("Publication acknowledgement durability failed; host admission closed");
    }
    state
        .launch_history
        .lock()
        .unwrap()
        .activate_acknowledged_recovery(token.owner)
        .map_err(|_| "Durable recovery activation changed")?;
    if reset_all {
        #[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
        finish_operator_reset(state);
        // A NEW legitimate Normal policy owner starts only after the durable
        // publication acknowledgement is verified and activation succeeds.
        // It captures the current finite Stop/epoch before any observations;
        // an intervening or retained Stop still vetoes automatic publication.
        crate::sequences::refresh_action_policy_now(state);
        state.broadcast_action_policy_snapshot();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(not(any(feature = "hitl_mode", feature = "test_fire_mode", feature = "firmware_sim")))]
    mod normal_automatic_phase { include!("normal_automatic_phase_tests.rs"); }
    include!("targeted_recovery_tests.rs");
    #[cfg(any(feature="hitl_mode",feature="firmware_sim"))]
    include!("final_clock_tests.rs");
    use super::*;
    include!("postmission_stop_reset_native.rs");
    include!("queued_fc_preentry_native.rs");
    include!("ignition_pair_causal_tests.rs");
    include!("ignition_pair_controls.rs");
    #[cfg(feature="test_fire_mode")]
    mod pending_native { include!("pending_recovery_native_tests.rs"); }
    use futures::FutureExt;
    use sedsnet::router::{EndpointHandler, Router, RouterConfig, RouterSideOptions};
    use std::collections::HashMap;
    use tokio::sync::mpsc;

    struct Pause {
        entered: tokio::sync::Notify,
        released: tokio::sync::Notify,
    }
    static AUTH_RECEIVED: Mutex<Option<(usize, Arc<tokio::sync::Notify>)>> = Mutex::new(None);
    static RESPONSE: Mutex<Option<(usize, Arc<Pause>)>> = Mutex::new(None);
    static FINAL: Mutex<Option<(usize, Arc<Pause>)>> = Mutex::new(None);
    #[cfg(any(feature="hitl_mode",feature="firmware_sim"))]
    static CLOCK_READY: Mutex<Option<(usize, Arc<Pause>)>> = Mutex::new(None);
    #[cfg(any(feature="hitl_mode",feature="firmware_sim"))]
    static CLOCK_MARKER: Mutex<Option<(usize, Arc<Pause>)>> = Mutex::new(None);
    #[cfg(any(feature="hitl_mode",feature="firmware_sim"))]
    static CLOCK_BUSY: Mutex<Option<(usize, Arc<tokio::sync::Notify>)>> = Mutex::new(None);

    struct PauseGuard {
        slot: &'static Mutex<Option<(usize, Arc<Pause>)>>,
        key: usize,
        pause: Arc<Pause>,
    }
    impl PauseGuard {
        fn install(
            slot: &'static Mutex<Option<(usize, Arc<Pause>)>>,
            state: &Arc<AppState>,
        ) -> Self {
            let key = Arc::as_ptr(state) as usize;
            let pause = Arc::new(Pause {
                entered: tokio::sync::Notify::new(),
                released: tokio::sync::Notify::new(),
            });
            *slot.lock().unwrap() = Some((key, pause.clone()));
            Self { slot, key, pause }
        }
        async fn entered(&self) {
            tokio::time::timeout(Duration::from_secs(3), self.pause.entered.notified())
                .await
                .unwrap();
        }
        fn release(&self) {
            self.pause.released.notify_one();
        }
    }
    impl Drop for PauseGuard {
        fn drop(&mut self) {
            self.release();
            let mut slot = self.slot.lock().unwrap();
            if slot.as_ref().is_some_and(|(key, _)| *key == self.key) {
                *slot = None;
            }
        }
    }
    async fn pause(slot: &Mutex<Option<(usize, Arc<Pause>)>>, state: &Arc<AppState>) {
        let item = slot
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(key, _)| *key == Arc::as_ptr(state) as usize)
            .map(|(_, pause)| pause.clone());
        if let Some(item) = item {
            item.entered.notify_one();
            item.released.notified().await;
        }
    }
    pub(super) fn notify_confirm_received(state: &Arc<AppState>) {
        if let Some((key, notify)) = AUTH_RECEIVED.lock().unwrap().as_ref()
            && *key == Arc::as_ptr(state) as usize {
                notify.notify_one();
            }
    }
    pub(super) async fn pause_response(state: &Arc<AppState>) {
        pause(&RESPONSE, state).await;
    }
    pub(super) async fn pause_finalization(state: &Arc<AppState>) {
        pause(&FINAL, state).await;
    }
    #[cfg(any(feature="hitl_mode",feature="firmware_sim"))]
    pub(super) async fn pause_clock_ready(state: &Arc<AppState>) {
        pause(&CLOCK_READY, state).await;
    }
    #[cfg(any(feature="hitl_mode",feature="firmware_sim"))]
    pub(super) async fn pause_clock_marker(state: &Arc<AppState>) {
        pause(&CLOCK_MARKER, state).await;
    }
    #[cfg(any(feature="hitl_mode",feature="firmware_sim"))]
    pub(super) fn notify_final_clock_busy(state: &Arc<AppState>) {
        if let Some((key, notify)) = CLOCK_BUSY.lock().unwrap().as_ref()
            && *key == Arc::as_ptr(state) as usize { notify.notify_one(); }
    }
    struct Model {
        hardened: bool,
        manual_v1: bool,
        boot: u64,
        epoch: u32,
        state: u8,
        result: u8,
        applied: usize,
        requests: Vec<Packet>,
        seen: HashMap<u64, Vec<u8>>,
        pending_only: bool,
        fail_result: Option<u8>,
        output_override: Option<u32>,
        fail_first_readback: bool,
        retained_first: Option<(Vec<u8>, u64)>,
    }
    #[cfg(feature="test_fire_mode")]
    type NativePeerRouter = Arc<Router>;
    #[cfg(not(feature="test_fire_mode"))]
    type NativePeerRouter = Router;
    struct Link {
        board: Board,
        router: NativePeerRouter,
        host_side: usize,
        board_side: usize,
        outgoing: Arc<Mutex<Vec<Vec<u8>>>>,
        incoming: Arc<Mutex<Vec<Vec<u8>>>>,
        requests: Arc<Mutex<Vec<Packet>>>,
        model: Arc<Mutex<Model>>,
        pending_rx: VecDeque<Vec<u8>>,
        c_bridge: Option<super::c_board_ordering_tests::Bridge>,
    }
    struct Fixture {
        http: super::super::tests::HttpFixture,
        pump: JoinHandle<()>,
        receiver: JoinHandle<crate::telemetry_task::TelemetryShutdownOutcome>,
        models: Vec<(Board, Arc<Mutex<Model>>)>,
        raw_state_receipts: Arc<Mutex<Vec<Packet>>>,
        incoming_startup: Arc<Mutex<Vec<(Board, Packet)>>>,
        managed_state_receipts: Arc<Mutex<Vec<(Board, Packet)>>>,
        controller_receipts: Arc<Mutex<Vec<Packet>>>,
        ignition_wire_hold: Arc<std::sync::atomic::AtomicU8>,
        _db: mpsc::Sender<crate::telemetry_db::DbQueueItem>,
        #[cfg(feature="test_fire_mode")]
        native_peers: Vec<(Board,Arc<Router>)>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let healthy = !self.pump.is_finished() && !self.receiver.is_finished();
            #[cfg(feature="test_fire_mode")]
            crate::recovery_pending_probe::clear_for_state(&self.http.state);
            self.http.state.request_shutdown();
            self.pump.abort();
            self.receiver.abort();
            if let Some(task) = self
                .http
                .state
                .board_recovery
                .coordinator
                .0
                .lock()
                .unwrap()
                .task
                .take()
            {
                task.abort();
            }
            if !std::thread::panicking() {
                assert!(
                    healthy,
                    "actual test wire/receiver worker must not terminate silently"
                );
            }
        }
    }
    async fn body(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
    impl Fixture {
        async fn new() -> Self {
            Self::from_saved(None, None).await
        }
        async fn new_with_managed_peers() -> Self {
            Self::from_saved_with_clock_and_services(None,None,None,true).await
        }
        async fn from_saved(
            journal: Option<Vec<u8>>,
            retained: Option<Vec<(Board, Arc<Mutex<Model>>)>>,
        ) -> Self {
            Self::from_saved_with_clock(journal, retained, None).await
        }
        async fn from_saved_with_clock(
            journal: Option<Vec<u8>>,
            retained: Option<Vec<(Board, Arc<Mutex<Model>>)>>,
            clock: Option<Box<dyn sedsnet::router::Clock + Send + Sync>>,
        ) -> Self {
            Self::from_saved_with_clock_and_services(journal,retained,clock,false).await
        }
        async fn from_saved_with_clock_and_services(
            journal: Option<Vec<u8>>,
            retained: Option<Vec<(Board, Arc<Mutex<Model>>)>>,
            clock: Option<Box<dyn sedsnet::router::Clock + Send + Sync>>,
            managed_peers: bool,
        ) -> Self {
            Self::from_saved_with_services_profile_and_c_boards(journal, retained, clock, managed_peers, false, false, false).await
        }
        #[cfg(not(feature="hitl_mode"))]
        async fn new_with_active_startup_peer_services() -> Self {
            Self::from_saved_with_services_profile_and_c_boards(None,None,None,true,true,false,false).await
        }
        async fn from_saved_with_services_profile(
            journal: Option<Vec<u8>>, retained: Option<Vec<(Board,Arc<Mutex<Model>>)>>,
            clock: Option<Box<dyn sedsnet::router::Clock+Send+Sync>>,
            managed_peers: bool, active_startup_services: bool, ignition_peer_services: bool,
        )->Self {
            Self::from_saved_with_services_profile_and_c_boards(journal,retained,clock,managed_peers,active_startup_services,ignition_peer_services,false).await
        }
        async fn from_saved_with_services_profile_and_c_boards(
            journal: Option<Vec<u8>>,
            retained: Option<Vec<(Board, Arc<Mutex<Model>>)>>,
            clock: Option<Box<dyn sedsnet::router::Clock + Send + Sync>>,
            managed_peers: bool,
            active_startup_services: bool,
            ignition_peer_services: bool,
            actual_c_boards: bool,
        ) -> Self {
            let mut http = super::super::tests::http_fixture().await;
            let restarting = journal.is_some();
            if let Some(bytes) = journal {
                std::fs::write(http.directory.join("clock.json"), bytes).unwrap();
            }
            let (tx, commands) = mpsc::channel(8);
            Arc::get_mut(&mut http.state).unwrap().cmd_tx = tx;
            // Bind the real recording owner and the producer to the same channel,
            // matching production main rather than the old metadata-only fixture.
            let (db, db_rx) = mpsc::channel(64);
            Arc::get_mut(&mut http.state).unwrap().db_queue_tx = db.clone();
            let state = http.state.clone();
            #[cfg(feature="test_fire_mode")]
            let native_probe=crate::recovery_pending_probe::take_next();
            #[cfg(feature="test_fire_mode")]
            if let Some(p)=&native_probe { p.bind(&state); }
            *state.launch_history.lock().unwrap() = crate::launch_history::Store::open(
                http.directory.join("clock.json"),
                false,
                None,
                10000,
                0,
            )
            .unwrap();
            let (owner, generation) = state
                .launch_history
                .lock()
                .unwrap()
                .restored_recovery_barrier();
            state
                .operator_admission_barrier
                .store(owner, Ordering::Release);
            state
                .automatic_generation
                .store(generation, Ordering::Release);
            let initial_utc = state
                .launch_history
                .lock()
                .unwrap()
                .recovery_pending()
                .map_or(1_793_000_000_000, |marker| marker.origin_utc_ms + 100);
            crate::gse::initialize(&state);
            let raw_state_receipts = Arc::new(Mutex::new(Vec::<Packet>::new()));
            let received_raw = raw_state_receipts.clone();
            let controller_receipts=Arc::new(Mutex::new(Vec::<Packet>::new()));
            let received_controls=controller_receipts.clone();
            #[cfg(feature="test_fire_mode")]
            let native_enabled=native_probe.is_some();
            let weak = Arc::downgrade(&state);
            let config = RouterConfig::new([EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint("GROUND_STATION"),
                move |packet| {
                    let ingress = weak.upgrade().map(|state| {
                        let generation = crate::gse::automatic_generation(&state);
                        (state, generation)
                    });
                    if ["FLIGHT_COMMAND","VALVE_COMMAND","ACTUATOR_COMMAND"].iter()
                        .any(|name|packet.data_type()==crate::telemetry_schema::data_type(name)) {
                        received_controls.lock().unwrap().push(packet.clone());
                    }
                    if let Some((state, ingress_generation)) = ingress {
                        // Private test ingress mirrors production ABORT endpoint observation.
                        if packet.data_type() == crate::telemetry_schema::data_type("ABORT")
                            && postmission_native_ingress_enabled(&state) {
                            crate::deferred_abort::observe(&state, packet, crate::gse::automatic_generation(&state));
                        }
                        crate::board_abort::observe_raw(&state, packet);
                        // Production ingress records actual board contact
                        // before applying a correlated readback. Mirror that
                        // receipt here; never seed synthetic board liveness.
                        if packet.data_type() == crate::telemetry_schema::data_type(STATUS_TYPE)
                            && Readback::decode(packet.payload()).is_some()
                            && let Some(board) = state.board_from_network_sender(packet.sender())
                        {
                            state.mark_board_seen(
                                board.sender_id(),
                                crate::telemetry_task::get_current_timestamp_ms(),
                            );
                        }
                        receive_status(&state, packet);
                        #[cfg(feature = "test_fire_mode")]
                        pending_epoch_note_ingress(&state, packet);
                        #[cfg(feature="test_fire_mode")]
                        if native_enabled && packet.data_type()==crate::telemetry_schema::data_type("HEARTBEAT") {
                            state.ring_buffer.lock().unwrap().push(crate::ring_buffer::ReceivedPacket::Telemetry { packet: packet.clone(), ingress_generation });
                        }
                        if packet.data_type() == crate::telemetry_schema::data_type("FLIGHT_STATE")
                        {
                            received_raw.lock().unwrap().push(packet.clone());
                            if crate::telemetry_task::queued_phase_probe::observe_sdk_ingress(&state, packet, ingress_generation) {
                                // Exact production main flight-state ingress behavior:
                                // the dedicated status path above remains independent.
                                state.ring_buffer.lock().unwrap().push(crate::ring_buffer::ReceivedPacket::Telemetry { packet: packet.clone(), ingress_generation });
                            }
                        }
                    }
                    Ok(())
                },
            )])
            .with_sender("GS")
            // Identical host resource policy to actual main, not peer policy.
            .with_memory_config(crate::network_memory_config()).unwrap();
            let host = Arc::new(match clock {
                Some(clock) => Router::new_with_clock(config, clock),
                None => Router::new(config),
            });
            let actual_memory: Value = serde_json::from_str(&host.export_memory_layout_json()).unwrap();
            let configured_memory = crate::network_memory_config();
            assert_eq!(actual_memory["shared_queue_bytes_allocated"], configured_memory.max_queue_budget);
            eprintln!("actual GS fixture host memory: {actual_memory}; config starting={}, recent={}, growth={}",
                configured_memory.starting_queue_size, configured_memory.max_recent_rx_ids, configured_memory.queue_grow_step);
            host.try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(
                initial_utc,
            ))
            .unwrap();
            host.enable_managed_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
                .unwrap();
            assert!(state.topology_router.set(host.clone()).is_ok());
            if restarting {
                crate::network_variables::initialize_for_state(&state, &host).unwrap();
            }
            let mut links = Vec::new();
            let mut models = Vec::new();
            #[cfg(feature="test_fire_mode")]
            let mut native_peers=Vec::new();
            let incoming_startup = Arc::new(Mutex::new(Vec::<(Board,Packet)>::new()));
            let managed_state_receipts = Arc::new(Mutex::new(Vec::<(Board,Packet)>::new()));
            for board in BOARDS {
                let requests = Arc::new(Mutex::new(Vec::<Packet>::new()));
                let capture = requests.clone();
                let dedicated = crate::telemetry_schema::endpoint(endpoint(board));
                let state_receipts = incoming_startup.clone();
                let mut peer_handlers = vec![
                        EndpointHandler::new_packet_handler(dedicated, move |packet| {
                            capture.lock().unwrap().push(packet.clone());
                            Ok(())
                        }),
                        EndpointHandler::new_packet_handler(
                            crate::telemetry_schema::endpoint("FLIGHT_STATE"),
                            move |packet| {
                                state_receipts.lock().unwrap().push((board,packet.clone()));
                                Ok(())
                            },
                        ),
                    ];
                if active_startup_services && board == Board::GatewayBoard {
                    // Native SDK delivery without an application valve ACK:
                    // the real startup owner must wait, not invent completion.
                    peer_handlers.push(EndpointHandler::new_packet_handler(crate::telemetry_schema::endpoint("VALVE_BOARD"), |_| Ok(())));
                    peer_handlers.push(EndpointHandler::new_packet_handler(crate::telemetry_schema::endpoint("ACTUATOR_BOARD"), |_| Ok(())));
                }
                if ignition_peer_services || actual_c_boards {
                    let name = match board {
                        Board::ActuatorBoard => Some("ACTUATOR_BOARD"),
                        Board::ValveBoard => Some("VALVE_BOARD"),
                        Board::FlightComputer => Some("FLIGHT_CONTROLLER"),
                        _ => None,
                    };
                    if let Some(name) = name {
                        // Actual peer-owned command endpoint: discover via SDK,
                        // not an injected host route or a synthetic proof mask.
                        peer_handlers.push(EndpointHandler::new_packet_handler(
                            crate::telemetry_schema::endpoint(name), {
                                let capture=requests.clone();
                                move |packet| {
                                    if actual_c_boards && matches!(board,Board::ActuatorBoard|Board::ValveBoard) && ["ACTUATOR_COMMAND","VALVE_COMMAND"].iter().any(|name|packet.data_type()==crate::telemetry_schema::data_type(name)) { capture.lock().unwrap().push(packet.clone()); }
                                    Ok(())
                                }
                            }));
                    }
                }
                let peer_config = RouterConfig::new(peer_handlers).with_sender(board.sender_id());
                #[cfg(not(feature="test_fire_mode"))]
                let router=Router::new(peer_config);
                #[cfg(feature="test_fire_mode")]
                let router=Arc::new(if let Some(clock)=native_probe.as_ref().and_then(|p|p.clock.clone()) {
                    Router::new_with_clock(peer_config,Box::new(crate::recovery_pending_probe::ClockSource(clock)))
                } else { Router::new(peer_config) });
                if managed_peers {
                    // Match all seven actual flight_state_cache_init owners:
                    // seds_router_enable_network_variable(FSTATE,true,false).
                    let ty=crate::telemetry_schema::data_type("FLIGHT_STATE");
                    router.enable_network_variable(ty,sedsnet::router::NetworkVariablePermissions::READ_ONLY).unwrap();
                    let managed_receipts=managed_state_receipts.clone();
                    #[cfg(feature="test_fire_mode")]
                    let probe=native_probe.clone();
                    #[cfg(feature="test_fire_mode")]
                    let weak_peer=Arc::downgrade(&router);
                    router.on_network_variable_update(ty,move |packet| {
                        managed_receipts.lock().unwrap().push((board,packet.clone()));
                        #[cfg(feature="test_fire_mode")]
                        if let Some(p)=&probe {
                            p.observe(board,packet);
                            if board==Board::GatewayBoard && p.reject_gb.load(Ordering::Acquire) {
                                weak_peer.upgrade().unwrap().invalidate_network_variable_cache(ty);
                                return Err(sedsnet::TelemetryError::HandlerError("native GB strict-readiness refusal model"));
                            }
                        }
                        Ok(())
                    }).unwrap();
                }
                let outgoing = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
                let output = outgoing.clone();
                #[cfg(feature="test_fire_mode")]
                let outgoing_wire_probe = native_probe.clone();
                let hs = host.add_side_packed_with_options(
                    format!("wire-{}", board.sender_id()),
                    move |wire| {
                        #[cfg(feature="test_fire_mode")]
                        if let Some(p) = &outgoing_wire_probe { p.observe_wire("host_emit", board, wire); }
                        output.lock().unwrap().push(wire.to_vec());
                        Ok(())
                    },
                    RouterSideOptions {
                        reliable_enabled: {
                            #[cfg(feature="test_fire_mode")]
                            {native_probe.is_none()}
                            #[cfg(not(feature="test_fire_mode"))]
                            {true}
                        },
                        ..Default::default()
                    },
                );
                let incoming = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
                let output = incoming.clone();
                #[cfg(feature="test_fire_mode")]
                let incoming_wire_probe = native_probe.clone();
                let bs = router.add_side_packed_with_options(
                    "wire",
                    move |wire| {
                        #[cfg(feature="test_fire_mode")]
                        if let Some(p) = &incoming_wire_probe { p.observe_wire("peer_emit", board, wire); }
                        output.lock().unwrap().push(wire.to_vec());
                        Ok(())
                    },
                    RouterSideOptions {
                        reliable_enabled: {
                            #[cfg(feature="test_fire_mode")]
                            {native_probe.is_none()}
                            #[cfg(not(feature="test_fire_mode"))]
                            {true}
                        },
                        ..Default::default()
                    },
                );
                host.rx_from_side(
                    &sedsnet::discovery::build_discovery_announce(
                        board.sender_id(),
                        1,
                        &[dedicated, crate::telemetry_schema::endpoint("FLIGHT_STATE")],
                    )
                    .unwrap(),
                    hs,
                )
                .unwrap();
                router
                    .rx_from_side(
                        &sedsnet::discovery::build_discovery_announce(
                            "GS",
                            1,
                            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                        )
                        .unwrap(),
                        bs,
                    )
                    .unwrap();
                let model = retained
                    .as_ref()
                    .and_then(|models| models.iter().find(|(id, _)| *id == board))
                    .map(|(_, model)| model.clone())
                    .unwrap_or_else(|| {
                        Arc::new(Mutex::new(Model {
                            hardened: true,
                            manual_v1: false,
                            boot: 42,
                            epoch: 8,
                            state: 15,
                            result: 0,
                            applied: 0,
                            requests: Vec::new(),
                            seen: HashMap::new(),
                            pending_only: false,
                            fail_result: None,
                            output_override: None,
                            fail_first_readback: false,
                            retained_first: None,
                        }))
                    });
                models.push((board, model.clone()));
                #[cfg(feature="test_fire_mode")]
                native_peers.push((board,router.clone()));
                links.push(Link {
                    board,
                    router,
                    host_side: hs,
                    board_side: bs,
                    outgoing,
                    incoming,
                    requests,
                    model,
                    pending_rx: VecDeque::new(),
                    c_bridge: (actual_c_boards && matches!(board,Board::ActuatorBoard|Board::ValveBoard)).then(||super::c_board_ordering_tests::Bridge::start(board)),
                });
            }
            let ignition_wire_hold = Arc::new(std::sync::atomic::AtomicU8::new(0));
            let pump_hold = ignition_wire_hold.clone();
            let pump_host = host.clone();
            let pump = tokio::spawn(async move {
                loop {
                    if pump_hold.load(Ordering::Acquire) != 0 {
                        pump_hold.store(2, Ordering::Release);
                        tokio::time::sleep(Duration::from_millis(2)).await;
                        continue;
                    }
                    // The actual telemetry task is the sole host queue owner.
                    for link in &mut links {
                        #[cfg(feature="test_fire_mode")]
                        if link.board==Board::GatewayBoard && native_probe.as_ref().is_some_and(|p|p.get_due()) {
                            link.router.request_managed_variable(crate::telemetry_schema::data_type("FLIGHT_STATE")).unwrap();
                        }
                        for wire in std::mem::take(&mut *link.outgoing.lock().unwrap()) {
                            let result=link.router.rx_packed_from_side(&wire,link.board_side);
                            #[cfg(feature="test_fire_mode")]
                            if result.is_ok() && let Some(p) = &native_probe { p.observe_wire("peer_ingress_accepted", link.board, &wire); }
                            #[cfg(feature="test_fire_mode")]
                            crate::recovery_pending_probe::peer_result(native_probe.as_ref(),result);
                            #[cfg(not(feature="test_fire_mode"))]
                            result.unwrap();
                        }
                        let result=link.router.process_all_queues_with_timeout(0);
                        #[cfg(feature="test_fire_mode")]
                        crate::recovery_pending_probe::peer_result(native_probe.as_ref(),result);
                        #[cfg(not(feature="test_fire_mode"))]
                        result.unwrap();
                        for packet in std::mem::take(&mut *link.requests.lock().unwrap()) {
                            if let Some(bridge)=&mut link.c_bridge {
                                let response=bridge.packet(&packet);
                                link.model.lock().unwrap().requests.push(packet.clone());
                                if packet.data_type()!=crate::telemetry_schema::data_type(QUERY_TYPE) {
                                    assert_eq!(response[0],0,"actual C ordinary command admission");
                                    continue;
                                }
                                let reply=Packet::new(crate::telemetry_schema::data_type(STATUS_TYPE),
                                    &[crate::telemetry_schema::endpoint("GROUND_STATION")],&link.router.sender(),packet.timestamp(),Arc::from(response)).unwrap();
                                link.router.tx_queue(reply).unwrap();
                                continue;
                            }
                            let wire = packet.payload();
                            assert_eq!(wire.len(), 32);
                            let nonce = u64::from_le_bytes(wire[4..12].try_into().unwrap());
                            let mut m = link.model.lock().unwrap();
                            m.requests.push(packet.clone());
                            let mut response = vec![0u8; 40];
                            response[0] = 1;
                            response[4..12].copy_from_slice(&wire[4..12]);
                            response[32..40].copy_from_slice(&wire[24..32]);
                            let physical = matches!(
                                link.board,
                                Board::ActuatorBoard | Board::ValveBoard | Board::FlightComputer
                            );
                            let mut result = m.result;
                            let mut identity_rejected = false;
                            if wire[1] == 1 {
                                if let Some(old) = m.seen.get(&nonce) {
                                    response = old.clone();
                                } else {
                                    let expected_boot =
                                        u64::from_le_bytes(wire[12..20].try_into().unwrap());
                                    let live_boot = if m.boot == 0 && m.retained_first.is_some() {
                                        u64::MAX
                                    } else {
                                        m.boot
                                    };
                                    let expected_epoch = u32::from_le_bytes(wire[20..24].try_into().unwrap());
                                    // Actual AB/VB V1 storage admission rejects stale boot/epoch
                                    // with result4, then transport reports CURRENT state/identity.
                                    // This fixture must not panic or rewrite the frozen request.
                                    if expected_boot != live_boot || expected_epoch != m.epoch {
                                        result = 4; // BOARD_RECOVERY_RESULT_STALE
                                        identity_rejected = true;
                                    } else if let Some(failure) = m.fail_result {
                                        result = failure;
                                    } else if m.pending_only {
                                        result = 1;
                                    } else if m.boot == u64::MAX {
                                        m.retained_first =
                                            Some((wire.to_vec(), packet.timestamp()));
                                        if m.fail_first_readback {
                                            m.boot = 0;
                                            m.result = 7;
                                            m.retained_first =
                                                Some((wire.to_vec(), packet.timestamp()));
                                        } else {
                                            m.boot = 1;
                                            m.result = 0;
                                        }
                                        result = 7;
                                    } else if m.boot == 0 && m.retained_first.is_some() {
                                        let (failed, failed_time) =
                                            m.retained_first.as_ref().unwrap();
                                        assert!(packet.timestamp() > *failed_time);
                                        assert_ne!(&wire[4..12], &failed[4..12]);
                                        m.boot = 1;
                                        m.result = 0;
                                        result = 7;
                                    } else {
                                        m.epoch += 1;
                                        m.state = wire[2];
                                        m.result = 0;
                                        result = 0;
                                        m.applied += 1;
                                    }
                                }
                            }
                            if !m.seen.contains_key(&nonce) || wire[1] == 0 {
                                response[1] = result;
                                response[2] = m.state;
                                response[3] = 0x04 | if m.hardened { 0x20 } else { 0 }
                                    | if physical && m.manual_v1 { 0x40 } else { 0 }
                                    | if m.state != 255 { 8 } else { 0 }
                                    | if physical { 0x12 } else { 0 }
                                    | if m.state == 15
                                        || m.retained_first.is_some() && m.applied == 0
                                    {
                                        1
                                    } else {
                                        0
                                    };
                                response[12..20].copy_from_slice(&m.boot.to_le_bytes());
                                response[20..24].copy_from_slice(&m.epoch.to_le_bytes());
                                response[28..32].copy_from_slice(
                                    &m.output_override
                                        .unwrap_or_else(|| {
                                            safe_output_mask(link.board).unwrap_or(0)
                                        })
                                        .to_le_bytes(),
                                );
                                if wire[1] == 1 && result != 1 && !identity_rejected {
                                    m.seen.insert(nonce, response.clone());
                                }
                            }
                            drop(m);
                            let reply = Packet::new(
                                crate::telemetry_schema::data_type(STATUS_TYPE),
                                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                                &link.router.sender(),
                                packet.timestamp(),
                                Arc::from(response),
                            )
                            .unwrap();
                            link.router.tx_queue(reply).unwrap();
                        }
                        let result=link.router.process_all_queues_with_timeout(0);
                        #[cfg(feature="test_fire_mode")]
                        crate::recovery_pending_probe::peer_result(native_probe.as_ref(),result);
                        #[cfg(not(feature="test_fire_mode"))]
                        result.unwrap();
                        link.pending_rx
                            .extend(std::mem::take(&mut *link.incoming.lock().unwrap()));
                        assert!(
                            link.pending_rx.len() <= 1024,
                            "bounded test wire retains backpressure"
                        );
                        while let Some(wire) = link.pending_rx.front() {
                            #[cfg(feature="test_fire_mode")]
                            if native_probe.as_ref().is_some_and(|p| p.should_hold_get(link.board, wire)) { break; }
                            match pump_host.rx_packed_from_side(wire, link.host_side) {
                                Ok(()) => {
                                    #[cfg(feature="test_fire_mode")]
                                    if let Some(p) = &native_probe { p.observe_wire("host_ingress_accepted", link.board, wire); }
                                    link.pending_rx.pop_front();
                                }
                                Err(sedsnet::TelemetryError::Io("rx queue busy")) => break,
                                Err(error) => panic!(
                                    "test wire admission failed without losing frame: {error:?}"
                                ),
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            });
            let receiver = tokio::spawn(crate::telemetry_task::telemetry_task(
                state.clone(),
                host,
                vec![],
                commands,
                db_rx,
                state.shutdown_subscribe(),
            ));
            let fixture=Self {
                http,
                pump,
                receiver,
                models,
                raw_state_receipts,
                incoming_startup,
                managed_state_receipts,
                controller_receipts,
                ignition_wire_hold,
                _db: db,
                #[cfg(feature="test_fire_mode")]
                native_peers,
            };
            if managed_peers {
                // Observe REAL SDK service discovery, never inject owner IDs or
                // rewrite routes to force multicast. No application request yet.
                tokio::time::timeout(Duration::from_secs(2),async {
                    loop {
                        let topology=fixture.http.state.topology_router.get().unwrap().export_topology();
                        let ty=crate::telemetry_schema::data_type("FLIGHT_STATE");
                        let ready=BOARDS.iter().all(|board|topology.routes.iter().any(|route|
                            route.announcers.iter().any(|announcer|
                                announcer.sender_id==board.sender_id() && announcer.reachable_network_variables.contains(&ty))));
                        if ready {break;}
                        tokio::time::sleep(Duration::from_millis(2)).await;
                    }
                }).await.expect("actual READ_ONLY peer services must be discovered before reset intent");
            }
            if ignition_peer_services {
                tokio::time::timeout(Duration::from_secs(2), async {
                    loop {
                        if crate::ignition_pair::require_routes(&fixture.state()).is_ok() { break; }
                        tokio::time::sleep(Duration::from_millis(2)).await;
                    }
                }).await.expect("actual command peer endpoints must be SDK-discovered");
            }
            fixture
        }
        fn state(&self) -> Arc<AppState> {
            self.http.state.clone()
        }
        async fn inject_state(&self, packet: Packet) {
            let router = self.http.state.topology_router.get().unwrap();
            let id = packet.packet_id();
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    match router.rx_queue(packet.clone()) {
                        Ok(()) => break,
                        Err(sedsnet::TelemetryError::Io("rx queue busy")) => {
                            tokio::time::sleep(Duration::from_millis(2)).await
                        }
                        Err(error) => panic!("actual ingress refused fixture packet: {error:?}"),
                    }
                }
                while !self
                    .raw_state_receipts
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|p| p.packet_id() == id)
                {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
            .await
            .expect("actual receiver-owned state callback must finish");
        }
        async fn query(&self, board: Board) {
            let state = self.state();
            let origin = Token::capture(&state, 0);
            // Fixture warm-up precedes the operation under test. Like production
            // Prepare, retry ONLY marked never-owned read-only CoreBusy offers.
            // HTTP 200 stops immediately; generic failures and owner 409 do not retry.
            admit_prepare_query(
                || origin_current(&state, origin)
                    .then_some(())
                    .ok_or("Fixture Query origin changed during warm-up"),
                || super::super::tests::http(
                    state.clone(),
                    "POST",
                    "/api/board-recovery/query",
                    Some("operator"),
                    json!({"board":board.sender_id()}),
                ),
            ).await.expect("Fixture read-only Query warm-up must acquire actual SDK ownership before the operation under test");
            let nonce = self.http.state.board_recovery.inner.lock().unwrap().queries
                [index(board).unwrap()]
            .as_ref()
            .unwrap()
            .nonce;
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let fresh = self.http.state.board_recovery.inner.lock().unwrap().values
                        [index(board).unwrap()]
                    .as_ref()
                    .is_some_and(|row| row.value.nonce == nonce && row.fresh(Stamp::now()));
                    if fresh {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
            .await
            .unwrap();
        }
        async fn prepare(&self, action: &str, board: Option<&str>) -> Value {
            let state = self.state();
            let response = super::super::tests::http(state.clone(), "POST", "/api/board-recovery/prepare", Some("operator"), json!({"action":action,"boards":if action=="reset_startup" {BOARDS.iter().map(|b|b.sender_id()).collect::<Vec<_>>()}else{vec![]},"board":board,"mode":null,"backend_instance":hex(&state.board_recovery.identity.unwrap()),"reset_epoch":format!("{:016x}",crate::gse::automatic_generation(&state))})).await;
            let status = response.status();
            let value = body(response).await;
            assert_eq!(status, StatusCode::OK, "{value}");
            value
        }
        async fn confirm(&self, ticket: &Value) -> Response {
            super::super::tests::http(self.state(), "POST", "/api/board-recovery/confirm", Some("operator"), json!({"ticket":ticket["ticket"],"backend_instance":ticket["backend_instance"],"principal_context":ticket["principal_context"],"reset_epoch":ticket["reset_epoch"],"confirmation":ticket["confirmation"]})).await
        }
        async fn terminal(&self) -> Value {
            let terminal = tokio::time::timeout(Duration::from_secs(8), async {
                loop {
                    assert!(
                        !self.pump.is_finished() && !self.receiver.is_finished(),
                        "actual wire/receiver must be alive"
                    );
                    if let Some(op) = self.http.state.board_recovery.coordinator.operation()
                        && op["completed"] == true {
                            return op;
                        }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await;
            if terminal.is_err() {
                // Failure-only RAM observation after the ORIGINAL deadline.
                // No SDK, clock provider, QUERY, retry or authority mutation.
                let control = match self.http.state.board_recovery.coordinator.0.try_lock() {
                    Ok(control) => json!({
                        "operation":control.operation.as_ref().map(|operation|json!({
                            "phase":operation.get("phase"),
                            "completed":operation.get("completed"),
                            "success":operation.get("success"),
                            "selected_proof_count":operation.get("selected_proof_count"),
                            "reset_applied_state":operation.get("reset_applied_state"),
                        })),
                        "expected":control.expected.map(|(board,nonce)|json!({"board":board.sender_id(),"nonce":nonce})),
                        "task_finished":control.task.as_ref().map(JoinHandle::is_finished),
                        "inbox_len":control.inbox.len(),
                        "shutdown_closed":control.shutdown_closed,
                        "shutdown_failure":control.shutdown_failure,
                    }),
                    Err(_) => json!({"snapshot_unavailable":"owner busy or poisoned"}),
                };
                let readbacks = match self.http.state.board_recovery.inner.try_lock() {
                    Ok(inner) => json!({
                        "rejected_statuses":inner.rejected_statuses,
                        "last_rejection":inner.last_rejection,
                        "boards":BOARDS.iter().enumerate().map(|(i,board)|json!({
                            "board":board.sender_id(),
                            "readback":inner.values[i].as_ref().map(|v|format!("{:?}",v.value)),
                            "query_nonce":inner.queries[i].as_ref().map(|q|q.nonce),
                            "query_received":inner.queries[i].as_ref().map(|q|q.received),
                        })).collect::<Vec<_>>(),
                    }),
                    Err(_) => json!({"snapshot_unavailable":"owner busy or poisoned"}),
                };
                eprintln!("ORIGINAL_TERMINAL_DEADLINE_FAILURE {}",json!({
                    "control":control,"readbacks":readbacks,
                    "pump_finished":self.pump.is_finished(),
                    "receiver_finished":self.receiver.is_finished(),
                }));
            }
            terminal.unwrap()
        }
    }
    #[cfg(feature="test_fire_mode")]
    fn tf_fresh_receipt_probe() -> Arc<crate::recovery_pending_probe::Probe> {
        // Match the existing native fixture's explicit desired-cache load.
        let _ = crate::network_variables::flight_state();
        let probe = crate::recovery_pending_probe::Probe::new(None);
        probe.reject_gb.store(false, Ordering::Release);
        probe.install();
        probe
    }
    #[cfg(feature="test_fire_mode")]
    async fn assert_tf_idle_from_fresh_mcu_receipts(fixture: &Fixture) {
        let state = fixture.state();
        let before = crate::network_variables::automatic_idle_test_snapshot();
        let cached_before = state.topology_router.get().unwrap()
            .cached_managed_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
            .map(|packet| (packet.packet_id(), packet.timestamp(), packet.payload().to_vec()));
        crate::sequences::refresh_action_policy_now(&state);
        assert_eq!(state.local_flight_state_snapshot(), crate::types::FlightState::Startup,
            "TF policy projection cannot remint readiness from cached contacts");
        assert!(crate::network_variables::automatic_idle_test_snapshot().durable_unchanged(&before));
        assert_eq!(state.topology_router.get().unwrap()
            .cached_managed_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
            .map(|packet| (packet.packet_id(), packet.timestamp(), packet.payload().to_vec())), cached_before);
        let required: Vec<_> = Board::ALL.iter().copied()
            .filter(|board| state.board_required_for_progression(*board)).collect();
        assert!(!required.is_empty());
        for board in required {
            let peer = &fixture.native_peers.iter().find(|(seen, _)| *seen == board).unwrap().1;
            assert_eq!(peer.sender().as_ref(), board.sender_id());
            let utc = state.topology_router.get().unwrap().network_time_ms().unwrap();
            let packet = Packet::new(crate::telemetry_schema::data_type("HEARTBEAT"),
                &[crate::telemetry_schema::endpoint("GROUND_STATION")], board.sender_id(), utc,
                Arc::<[u8]>::from([])).unwrap();
            peer.tx_queue(packet).unwrap();
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            while state.local_flight_state_snapshot() != crate::types::FlightState::Idle {
                assert!(!fixture.pump.is_finished() && !fixture.receiver.is_finished());
                tokio::task::yield_now().await;
            }
        }).await.expect("fresh real MCU packet must drive TF readiness through actual telemetry worker");
        assert_eq!(crate::network_variables::flight_state(), 0,
            "automatic Idle remains live-only after explicit Startup");
        assert!(crate::network_variables::automatic_idle_test_snapshot().durable_unchanged(&before));
        let cached = state.topology_router.get().unwrap()
            .cached_managed_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
            .expect("actual public SDK must retain the new Idle");
        assert_eq!(cached.sender(), "GS");
        assert_eq!(cached.payload(), &[1]);
        assert!(cached.timestamp() > 0);
        assert_ne!(cached_before.as_ref().map(|p|p.0), Some(cached.packet_id()));
    }
    fn isolated(name: &str) -> bool {
        crate::operator_queue::isolated_case(module_path!(), name)
    }

    #[tokio::test(flavor="multi_thread",worker_threads=2)]
    async fn actual_mode_reset_preserves_global_broadcast_but_verifies_only_frozen_policy() {
        if isolated("actual_mode_reset_preserves_global_broadcast_but_verifies_only_frozen_policy") { return; }
        #[cfg(feature="test_fire_mode")]
        let _tf_probe = tf_fresh_receipt_probe();
        let fixture=Fixture::new_with_managed_peers().await;
        let state=fixture.state();
        let request=json!({"action":"reset_startup","board":null,"mode":null,
            "backend_instance":hex(&state.board_recovery.identity.unwrap()),
            "reset_epoch":format!("{:016x}",crate::gse::automatic_generation(&state))});
        let first=super::super::tests::http(state.clone(),"POST","/api/board-recovery/prepare",Some("operator"),request.clone()).await;
        let first_status=first.status();
        let first_body=body(first).await;
        let mask=super::super::scope::base_mask(crate::station_mode::local_mode());
        let ticket=if mask!=0 {
            // Actual QUERY discovered seeded Abort episodes AFTER the original
            // selection. That intent must die; never auto retry/restamp it.
            assert_eq!(first_status,StatusCode::CONFLICT,"{first_body}");
            assert_eq!(first_body["error"],"recovery_unavailable");
            assert!(first_body["message"].as_str().unwrap().contains("Abort episode changed"));
            assert!(state.launch_history.lock().unwrap().recovery_pending().is_none());
            assert!(state.board_recovery.coordinator.0.lock().unwrap().ticket.is_none());
            for (_,model) in &fixture.models { assert_eq!(model.lock().unwrap().applied,0); }
            // A SECOND explicit request is a new operator intent. The runtime
            // contains no retry loop and the retired first request is not used.
            let second=super::super::tests::http(state.clone(),"POST","/api/board-recovery/prepare",Some("operator"),request).await;
            let status=second.status();let value=body(second).await;
            assert_eq!(status,StatusCode::OK,"new explicit prepare: {value}");value
        } else {
            assert_eq!(first_status,StatusCode::OK,"empty HITL: {first_body}");
            assert_eq!(first_body["confirmation"],"RESET ALL STARTUP; VERIFY NONE");
            for (_,model) in &fixture.models { assert!(model.lock().unwrap().requests.is_empty()); }
            first_body
        };
        assert_eq!(ticket["targets"].as_array().unwrap().len(),mask.count_ones() as usize);
        assert_eq!(ticket["global_startup_best_effort"],true);
        assert_eq!(ticket["unselected_board_verification"],"unverified_best_effort");
        assert_eq!(fixture.confirm(&ticket).await.status(),StatusCode::OK);
        let result=fixture.terminal().await;
        assert_eq!(result["success"],true,"{result}");
        assert_eq!(result["global_startup_best_effort"],true);
        assert_eq!(result["selected_proof_count"],mask.count_ones());
        assert_eq!(result["unselected_board_verification"],"unverified_best_effort");
        if mask==0 {
            assert!(result["message"].as_str().unwrap().contains("no board terminal completion or fresh board proof is claimed"));
        } else {
            assert!(result["message"].as_str().unwrap().contains("listed proof targets"));
        }
        {
        let history=state.launch_history.lock().unwrap();
        let marker=history.recovery_pending().unwrap();
        assert!(marker.verified && marker.published);
        assert_eq!(marker.reset_policy.as_ref().unwrap().verified_mask,mask);
        assert_eq!(marker.targets.len(),mask.count_ones() as usize);
        assert!(!history.automatic_actions_inhibited());
        if mask!=0x7f { assert!(history.manual_utc_key().is_none(),"subset reset cannot fabricate all-seven Manual UTC cohort"); }
        }
        for (i,board) in BOARDS.iter().enumerate() {
            let model=fixture.models.iter().find(|(b,_)|b==board).unwrap().1.lock().unwrap();
            assert_eq!(model.applied,usize::from(mask&(1<<i)!=0));
        }
        // This fixture explicitly installs each existing peer's FLIGHT_STATE
        // discovery announcement before requests; assert that availability,
        // then observe ACTUAL peer endpoint receipts without adding routes.
        let router=state.topology_router.get().unwrap();
        let topology=router.export_topology();
        assert!(BOARDS.iter().all(|board| endpoint_verified(&state,*board,&topology)));
        let delivered=tokio::time::timeout(Duration::from_secs(2),async {
            loop {
                let all={
                    let received=fixture.managed_state_receipts.lock().unwrap();
                    BOARDS.iter().all(|board| received.iter().any(|(seen,packet)|seen==board && packet.sender()=="GS" && packet.payload()==[0]))
                };
                if all {break;}
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }).await;
        let cached = {
            let received=fixture.managed_state_receipts.lock().unwrap();
            let facts=received.iter().take(32).map(|(board,packet)|json!({"board":board.sender_id(),"sender":packet.sender(),"packet_id":packet.packet_id(),"source_utc":packet.timestamp(),"payload":packet.payload(),"type":packet.data_type().as_u32()})).collect::<Vec<_>>();
            let endpoint_count=fixture.incoming_startup.lock().unwrap().len();
            let cached=router.cached_managed_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
                .map(|packet|json!({"sender":packet.sender(),"packet_id":packet.packet_id(),"source_utc":packet.timestamp(),"payload":packet.payload()}));
            assert!(delivered.is_ok(),"all seven available disposable peers must receive global Startup; actual managed callback count={}, endpoint_count={endpoint_count}, first32={facts:?}, host_cache={cached:?}",received.len());
            let original=&received.iter().find(|(_,packet)|packet.sender()=="GS" && packet.payload()==[0]).unwrap().1;
            assert!(original.timestamp()>0);
            for (_,packet) in received.iter().filter(|(_,packet)|packet.sender()=="GS" && packet.payload()==[0]) {
                assert_eq!(packet.sender(),"GS");
                assert_eq!(packet.data_type(),crate::telemetry_schema::data_type("FLIGHT_STATE"));
                assert_eq!(packet.payload(),[0]);
                assert_eq!(packet.packet_id(),original.packet_id());
                assert_eq!(packet.timestamp(),original.timestamp());
            }
            cached
        };
        // Normal may publish Idle after Startup. TF requires a NEW actual MCU
        // packet; the READ_ONLY peer callbacks never grant that authority.
        if !cfg!(feature="hitl_mode") {
            assert_eq!(cached.as_ref().unwrap()["sender"],"GS");
            if cfg!(feature="test_fire_mode") {
                assert_eq!(state.local_flight_state_snapshot(),crate::types::FlightState::Startup);
                assert_eq!(cached.as_ref().unwrap()["payload"],json!([0]));
                #[cfg(feature="test_fire_mode")]
                assert_tf_idle_from_fresh_mcu_receipts(&fixture).await;
            } else {
                // Startup delivery and the owner's next Idle publication are
                // asynchronous. Observe both real completion signals instead
                // of assuming that receiving Startup already completed Idle.
                tokio::time::timeout(Duration::from_secs(2), async {
                    loop {
                        let idle = state.local_flight_state_snapshot() == crate::types::FlightState::Idle;
                        let cached_idle = router.cached_managed_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
                            .is_some_and(|packet| packet.sender() == "GS" && packet.payload() == [1]);
                        if idle && cached_idle { break; }
                        tokio::time::sleep(Duration::from_millis(2)).await;
                    }
                }).await.expect("actual normal owner must advance Startup to Idle and publish its own cached Idle");
                assert_eq!(state.local_flight_state_snapshot(),crate::types::FlightState::Idle);
                let idle = router.cached_managed_variable(crate::telemetry_schema::data_type("FLIGHT_STATE")).unwrap();
                assert_eq!(idle.sender(),"GS");
                assert_eq!(idle.payload(),[1]);
            }
        }
        // Wire receipts do NOT certify unselected MCU application/physical state.
    }

    /// Entire HTTP/coordinator/SDK path; only GPIO/NVM/ThreadX are inert.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires prebuilt actual AB/VB C bridge fixtures"]
    async fn actual_http_single_prepare_confirm_recovers_completed_c_boards_then_reset() {
        run_http_c_completed_recovery(false).await;
    }
    /// Separate explicit review-refresh experiment, never a single-Prepare proof.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires prebuilt actual AB/VB C bridge fixtures"]
    async fn actual_http_c_recovery_with_explicit_review_refreshes() {
        run_http_c_completed_recovery(true).await;
    }
    async fn run_http_c_completed_recovery(explicit_review_refresh:bool) {
        let fixture=Fixture::from_saved_with_services_profile_and_c_boards(None,None,None,true,false,true,true).await;
        let state=fixture.state();
        // Unrelated metadata peers start normally; discovering a new Abort
        // during review must still invalidate a frozen scope (tested separately).
        for (board,model) in &fixture.models {
            if !matches!(board,Board::ActuatorBoard|Board::ValveBoard) {
                model.lock().unwrap().state=0;
            }
        }
        let router=state.topology_router.get().unwrap().clone();
        // A live 100 Hz unrelated producer continues through every drain and
        // recovery. Keep physical telemetry owners outstanding as well.
        let mut held=Vec::new();
        for _ in 0..100 {
            let p=Packet::new(crate::telemetry_schema::data_type("KG50"),
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],"DAQ",1,Arc::from(1f32.to_le_bytes())).unwrap();
            let bytes=sedsnet::wire_format::pack_packet(&p).to_vec();
            state.transport_tx_activity.try_enqueue_owned(false,bytes,|b|{held.push(b);Ok(())}).unwrap();
        }
        let producing=Arc::new(std::sync::atomic::AtomicBool::new(false));
        let producer_active=producing.clone();
        let produced=Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let producer_count=produced.clone();
        let traffic=tokio::spawn(async move {
            loop {
                if !producer_active.load(Ordering::Acquire) {tokio::time::sleep(Duration::from_millis(2)).await;continue;}
                let p=Packet::new(crate::telemetry_schema::data_type("KG50"),
                    &[crate::telemetry_schema::endpoint("GROUND_STATION")],"DAQ",
                    crate::system_clock::required_control_network_utc_ms(&router).unwrap(),Arc::from(1f32.to_le_bytes())).unwrap();
                if router.try_tx_queue(p).is_ok() {producer_count.fetch_add(1,Ordering::Relaxed);} // Busy telemetry retries on the next tick.
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        struct StopTraffic(JoinHandle<()>);
        impl Drop for StopTraffic {fn drop(&mut self){self.0.abort();}}
        let _traffic=StopTraffic(traffic);
        let mut initial_epochs=Vec::new();
        for board in [Board::ActuatorBoard,Board::ValveBoard] {
            // Human review is separate from the SDK receive callback completing.
            if explicit_review_refresh {tokio::time::sleep(Duration::from_millis(20)).await;}
            let response=super::super::tests::http(state.clone(),"POST","/api/board-recovery/prepare",Some("operator"),json!({"action":"recover_abort","board":board.sender_id(),"mode":null,"backend_instance":hex(&state.board_recovery.identity.unwrap()),"reset_epoch":format!("{:016x}",crate::gse::automatic_generation(&state))})).await;
            let status=response.status();let ticket=body(response).await;
            if status!=StatusCode::OK {
                let diagnostic=super::super::tests::http(state.clone(),"POST","/api/board-recovery/query",Some("operator"),json!({"board":board.sender_id()})).await;
                eprintln!("Prepare {board:?}={ticket}; following QUERY {} {}",diagnostic.status(),body(diagnostic).await);
            }
            assert_eq!(status,StatusCode::OK,"{ticket}");
            let initial=state.board_recovery.inner.lock().unwrap().values[index(board).unwrap()].as_ref().unwrap().value.clone();
            assert_eq!(initial.state,Some(7),"Prepare queried actual C completed Launch owner");
            initial_epochs.push((board,initial.epoch));
            producing.store(true,Ordering::Release);
            let response=fixture.confirm(&ticket).await;
            assert_eq!(response.status(),StatusCode::OK,"{}",body(response).await);
            let result=fixture.terminal().await;
            assert_eq!(result["success"],true,"{result}");
            producing.store(false,Ordering::Release);
            if explicit_review_refresh {tokio::time::sleep(Duration::from_millis(20)).await;}
            let row=state.board_recovery.inner.lock().unwrap().values[index(board).unwrap()].as_ref().unwrap().value.clone();
            assert_eq!(row.state,Some(1));assert_eq!(row.epoch,initial.epoch+1);
        }
        // Explicit separate HTTP review attempts, not a mutating retry or an
        // overwritten sealed intent. Keep admission failures observable.
        let mut review_attempts=0;
        let ticket=loop {
            review_attempts+=1;
            let response=super::super::tests::http(state.clone(),"POST","/api/board-recovery/prepare",Some("operator"),json!({"action":"reset_startup","board":null,"boards":BOARDS.iter().map(|b|b.sender_id()).collect::<Vec<_>>(),"mode":null,"backend_instance":hex(&state.board_recovery.identity.unwrap()),"reset_epoch":format!("{:016x}",crate::gse::automatic_generation(&state))})).await;
            let status=response.status();let ticket=body(response).await;
            if status==StatusCode::OK {break ticket;}
            eprintln!("Explicit global review attempt {review_attempts}: {status} {ticket}");
            assert_eq!(ticket["error"],"fresh_query_required","{ticket}");
            assert!(explicit_review_refresh && review_attempts<20,"single Prepare must succeed; bounded explicit review experiment is separate");
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        eprintln!("Global HTTP review accepted on attempt {review_attempts}; explicit refresh allowed={explicit_review_refresh}");
        producing.store(true,Ordering::Release);
        eprintln!("Global HTTP Confirm starting");
        let response=fixture.confirm(&ticket).await;
        eprintln!("Global HTTP Confirm returned");
        assert_eq!(response.status(),StatusCode::OK,"{}",body(response).await);
        let result=fixture.terminal().await;assert_eq!(result["success"],true,"{result}");
        eprintln!("Global HTTP reset completed with verified C readbacks");
        producing.store(false,Ordering::Release);
        if explicit_review_refresh {tokio::time::sleep(Duration::from_millis(20)).await;}
        assert_eq!(result["reset_applied_state"],0);
        for (board,epoch) in initial_epochs {
            let row=state.board_recovery.inner.lock().unwrap().values[index(board).unwrap()].as_ref().unwrap().value.clone();
            assert_eq!(row.state,Some(0));assert_eq!(row.epoch,epoch+2);
            let name=if board==Board::ActuatorBoard {"ACTUATOR_COMMAND"}else{"VALVE_COMMAND"};
            let destination=if board==Board::ActuatorBoard {"ACTUATOR_BOARD"}else{"VALVE_BOARD"};
            let router=state.topology_router.get().unwrap();
            let command=Packet::new(crate::telemetry_schema::data_type(name),&[crate::telemetry_schema::endpoint(destination)],"GS",
                crate::system_clock::required_control_network_utc_ms(router).unwrap(),Arc::from([9u8])).unwrap();
            let id=command.packet_id();
            eprintln!("Ordinary {board:?} command admission starting");
            tokio::time::timeout(Duration::from_secs(2),async {
                loop {
                    match router.try_tx_queue(command.clone()) {
                        Ok(())=>break,
                        Err(sedsnet::TelemetryError::Io("router core busy"))=>tokio::time::sleep(Duration::from_millis(2)).await,
                        Err(reason)=>panic!("ordinary fixture command rejected: {reason:?}"),
                    }
                }
            }).await.expect("non-admitted ordinary fixture packet retained within deadline");
            tokio::time::timeout(Duration::from_secs(2),async {
                while !fixture.models.iter().find(|(b,_)|*b==board).unwrap().1.lock().unwrap().requests.iter().any(|p|p.packet_id()==id) {
                    assert!(!fixture.pump.is_finished(),"C command admission must succeed");
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            }).await.unwrap();
        }
        assert!(produced.load(Ordering::Acquire)>0,"actual SDK accepted telemetry during recovery drains");
        assert!(state.has_pending_transport_tx(),"continuous unrelated ownership survived recovery");
        for bytes in &held {state.transport_tx_activity.complete_frame(bytes);}
    }

    async fn completed_manual_cohort(fixture: &Fixture) {
        for board in BOARDS {
            fixture.query(board).await;
        }
        let ticket = fixture.prepare("reset_startup", None).await;
        assert_eq!(fixture.confirm(&ticket).await.status(), StatusCode::OK);
        assert_eq!(fixture.terminal().await["success"], true);
        for (_, model) in &fixture.models {
            model.lock().unwrap().manual_v1 = true;
        }
        for board in [
            Board::ActuatorBoard,
            Board::ValveBoard,
            Board::FlightComputer,
        ] {
            fixture.query(board).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deferred_abort_original_utc_actual_sdk_replay_after_completed_cohort() {
        if isolated("deferred_abort_original_utc_actual_sdk_replay_after_completed_cohort") {
            return;
        }
        let fixture = Fixture::new().await;
        completed_manual_cohort(&fixture).await;
        crate::deferred_abort::tests::assert_completed_cohort_retires_original_aborts(
            fixture.state(),
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manual_utc_actual_sdk_clock_wait_preserves_independent_new_stop_epoch() {
        if isolated("manual_utc_actual_sdk_clock_wait_preserves_independent_new_stop_epoch") {
            return;
        }
        use std::sync::{
            Condvar,
            atomic::{AtomicBool, AtomicU64},
        };
        let ticks = Arc::new(AtomicU64::new(0));
        let armed = Arc::new(AtomicBool::new(false));
        let pause = Arc::new((Mutex::new(false), Condvar::new()));
        struct Release(Arc<(Mutex<bool>, Condvar)>);
        impl Drop for Release {
            fn drop(&mut self) {
                *self.0.0.lock().unwrap() = true;
                self.0.1.notify_all();
            }
        }
        let release = Release(pause.clone());
        let (entered, reached) = std::sync::mpsc::channel();
        let clock_ticks = ticks.clone();
        let clock_armed = armed.clone();
        let clock_pause = pause.clone();
        let fixture = Fixture::from_saved_with_clock(
            None,
            None,
            Some(Box::new(move || {
                if clock_armed.load(Ordering::Acquire)
                    && std::thread::current().name() == Some("manual-utc-clock-test")
                {
                    let _ = entered.send(());
                    let mut released = clock_pause.0.lock().unwrap();
                    while !*released {
                        released = clock_pause.1.wait(released).unwrap();
                    }
                }
                clock_ticks.load(Ordering::Acquire)
            })),
        )
        .await;
        completed_manual_cohort(&fixture).await;
        let state = fixture.state();
        let router = state.topology_router.get().unwrap().clone();
        crate::manual_abort_utc::refresh(&state, &router).await;
        state.manual_abort_intent.request();
        armed.store(true, Ordering::Release);
        let worker_state = state.clone();
        let worker_router = router.clone();
        let worker = std::thread::Builder::new()
            .name("manual-utc-clock-test".into())
            .spawn(move || {
                let mut claim = worker_state.manual_abort_intent.try_claim().unwrap();
                let result = crate::manual_abort_utc::publish_claim(
                    &worker_state,
                    &worker_router,
                    &mut claim,
                );
                (result, claim.wire())
            })
            .unwrap();
        reached
            .recv_timeout(Duration::from_secs(2))
            .expect("actual SDK clock call reached");
        let start = std::time::Instant::now();
        assert_eq!(state.manual_abort_intent.request(), 2);
        assert!(start.elapsed() < Duration::from_millis(100));
        assert_eq!(state.manual_abort_intent.pending_epoch(), Some(2));
        ticks.store(1, Ordering::Release);
        drop(release);
        let (result, wire) = worker.join().unwrap();
        assert!(matches!(result, Ok(false)));
        assert_eq!(wire, None);
        let mut fresh = state.manual_abort_intent.try_claim().unwrap();
        assert!(crate::manual_abort_utc::publish_claim(&state, &router, &mut fresh).unwrap());
        assert!(matches!(
            fresh.wire(),
            Some(crate::manual_abort_intent::WirePolicy::KnownUtc(_))
        ));
        fresh.accepted();
    }

    #[tokio::test]
    async fn manual_utc_journal_reservation_is_max_merged_verified_and_preserved_on_restart() {
        if isolated(
            "manual_utc_journal_reservation_is_max_merged_verified_and_preserved_on_restart",
        ) {
            return;
        }
        let fixture = Fixture::new().await;
        completed_manual_cohort(&fixture).await;
        let state = fixture.state();
        let (first, second, ceiling) = {
            let mut store = state.launch_history.lock().unwrap();
            let key = store.manual_utc_key().unwrap();
            let ceiling = key.boundary + 2000;
            let first = store.reserve_manual_utc_ceiling(key, ceiling).unwrap();
            let second = store
                .reserve_manual_utc_ceiling(first.key(), ceiling - 1)
                .unwrap();
            assert_eq!(second.key().ceiling, ceiling);
            (first, second, ceiling)
        };
        first.wait_and_verify().unwrap();
        second.wait_and_verify().unwrap();
        let path = fixture.http.directory.join("clock.json");
        let mut restored =
            crate::launch_history::Store::open(path.clone(), false, None, 10000, 0).unwrap();
        assert_eq!(restored.retained_manual_utc_ceiling().unwrap(), ceiling);
        restored.reset_primary(11000, 0).unwrap();
        if let Some((writer, ticket)) = restored.waiter() {
            writer.wait(ticket).unwrap();
        }
        drop(restored);
        let restored =
            crate::launch_history::Store::open(path.clone(), false, None, 12000, 0).unwrap();
        assert_eq!(restored.retained_manual_utc_ceiling().unwrap(), ceiling);
        let mut disk: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        disk["manual_abort_utc_ceiling"] = json!(u64::MAX);
        std::fs::write(&path, serde_json::to_vec(&disk).unwrap()).unwrap();
        assert!(crate::launch_history::Store::open(path, false, None, 12000, 0).is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manual_utc_cancelled_prepare_cannot_restore_late_durable_reservation() {
        if isolated("manual_utc_cancelled_prepare_cannot_restore_late_durable_reservation") {
            return;
        }
        let fixture = Fixture::new().await;
        completed_manual_cohort(&fixture).await;
        let state = fixture.state();
        let router = state.topology_router.get().unwrap().clone();
        let writer = crate::web::clock_worker_tests::WriterGuard::install(
            fixture.http.directory.join("clock.json"),
        );
        let refresh_state = state.clone();
        let refresh_router = router.clone();
        let refresh = tokio::spawn(async move {
            crate::manual_abort_utc::refresh(&refresh_state, &refresh_router).await;
        });
        writer.entered().await;
        assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 1);
        let prepare_state = state.clone();
        let prepare = tokio::spawn(async move {
            super::super::tests::http(prepare_state.clone(),"POST","/api/board-recovery/prepare",Some("operator"),json!({"action":"reset_startup","boards":["RF","FC","PB","AB","VB","GB","DAQ"],"board":null,"mode":null,"backend_instance":hex(&prepare_state.board_recovery.identity.unwrap()),"reset_epoch":format!("{:016x}",crate::gse::automatic_generation(&prepare_state))})).await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !state.board_recovery.manual_utc.is_revoked() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        prepare.abort();
        assert!(prepare.await.unwrap_err().is_cancelled());
        assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 1);
        writer.release();
        refresh.await.unwrap();
        assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 0);
        assert!(
            state
                .launch_history
                .lock()
                .unwrap()
                .retained_manual_utc_ceiling()
                .unwrap()
                > 0
        );
        state.manual_abort_intent.request();
        let mut claim = state.manual_abort_intent.try_claim().unwrap();
        assert!(crate::manual_abort_utc::publish_claim(&state, &router, &mut claim).unwrap());
        assert_eq!(
            claim.wire(),
            Some(crate::manual_abort_intent::WirePolicy::Untimed)
        );
        claim.accepted();
        assert!(
            state
                .board_recovery
                .coordinator
                .0
                .lock()
                .unwrap()
                .ticket
                .is_none()
        );
        assert!(
            fixture
                .models
                .iter()
                .all(|(_, m)| m.lock().unwrap().applied == 1)
        );
    }

    #[tokio::test]
    async fn manual_utc_pending_stop_refuses_actual_prepare_until_sdk_ownership_transfer() {
        if isolated("manual_utc_pending_stop_refuses_actual_prepare_until_sdk_ownership_transfer") {
            return;
        }
        let fixture = Fixture::new().await;
        fixture.query(Board::ActuatorBoard).await;
        let state = fixture.state();
        state.manual_abort_intent.request();
        let response=super::super::tests::http(state.clone(),"POST","/api/board-recovery/prepare",Some("operator"),json!({"action":"recover_abort","board":"AB","mode":null,"backend_instance":hex(&state.board_recovery.identity.unwrap()),"reset_epoch":format!("{:016x}",crate::gse::automatic_generation(&state))})).await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(
            fixture
                .models
                .iter()
                .all(|(_, m)| m.lock().unwrap().applied == 0)
        );
        let mut claim = state.manual_abort_intent.try_claim().unwrap();
        assert!(
            crate::manual_abort_utc::publish_claim(
                &state,
                state.topology_router.get().unwrap(),
                &mut claim
            )
            .unwrap()
        );
        assert_eq!(
            claim.wire(),
            Some(crate::manual_abort_intent::WirePolicy::Untimed)
        );
        claim.accepted();
        fixture.query(Board::ActuatorBoard).await;
        let ticket = fixture.prepare("recover_abort", Some("AB")).await;
        assert!(ticket["ticket"].is_string());
    }

    #[tokio::test]
    async fn manual_utc_actual_status_capability_and_per_board_history_remain_conservative() {
        if isolated("manual_utc_actual_status_capability_and_per_board_history_remain_conservative")
        {
            return;
        }
        let fixture = Fixture::new().await;
        completed_manual_cohort(&fixture).await;
        let state = fixture.state();
        let response = super::super::tests::http(
            state.clone(),
            "GET",
            "/api/board-recovery/status",
            Some("operator"),
            json!({}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let status = body(response).await;
        for row in status["boards"].as_array().unwrap() {
            assert_eq!(
                row["capabilities"]["known_utc_manual_stop"],
                json!(matches!(row["id"].as_str(), Some("AB" | "VB" | "FC")))
            );
        }
        let key = state
            .launch_history
            .lock()
            .unwrap()
            .manual_utc_key()
            .unwrap();
        let proof = crate::board_recovery::manual_utc_proof(&state, key).unwrap();
        assert!(proof.iter().all(|nonce| *nonce != 0));
        for flags in [0x24u8, 0x64, 0xe4] {
            let mut bytes = [0u8; 40];
            bytes[0] = 1;
            bytes[2] = 255;
            bytes[3] = flags;
            bytes[4..12].copy_from_slice(&1u64.to_le_bytes());
            bytes[12..20].copy_from_slice(&42u64.to_le_bytes());
            bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
            bytes[32..40].copy_from_slice(&1u64.to_le_bytes());
            assert_eq!(Readback::decode(&bytes).is_some(), flags != 0xe4);
        }
        fixture.query(Board::ActuatorBoard).await;
        let ticket = fixture.prepare("recover_abort", Some("AB")).await;
        assert_eq!(fixture.confirm(&ticket).await.status(), StatusCode::OK);
        assert_eq!(fixture.terminal().await["success"], true);
        assert!(
            state
                .launch_history
                .lock()
                .unwrap()
                .manual_utc_key()
                .is_none(),
            "a newer per-board marker cannot inherit full-cohort proof"
        );
        state.manual_abort_intent.request();
        let mut claim = state.manual_abort_intent.try_claim().unwrap();
        assert!(
            crate::manual_abort_utc::publish_claim(
                &state,
                state.topology_router.get().unwrap(),
                &mut claim
            )
            .unwrap()
        );
        assert_eq!(
            claim.wire(),
            Some(crate::manual_abort_intent::WirePolicy::Untimed)
        );
        claim.accepted();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manual_utc_cancelled_reservation_waiter_retains_actual_writer_claim() {
        if isolated("manual_utc_cancelled_reservation_waiter_retains_actual_writer_claim") {
            return;
        }
        let fixture = Fixture::new().await;
        completed_manual_cohort(&fixture).await;
        let state = fixture.state();
        let router = state.topology_router.get().unwrap().clone();
        let writer = crate::web::clock_worker_tests::WriterGuard::install(
            fixture.http.directory.join("clock.json"),
        );
        let refresh_state = state.clone();
        let refresh_router = router.clone();
        let refresh = tokio::spawn(async move {
            crate::manual_abort_utc::refresh(&refresh_state, &refresh_router).await;
        });
        writer.entered().await;
        refresh.abort();
        assert!(refresh.await.unwrap_err().is_cancelled());
        assert_eq!(
            state.operator_dispatch_inflight.load(Ordering::Acquire),
            1,
            "actual blocking durability worker still owns lease"
        );
        assert!(
            state.board_recovery.manual_utc.reservation_pending(),
            "cancelling the async waiter cannot settle actual durability/readback ownership"
        );
        assert!(
            !state
                .launch_history
                .lock()
                .unwrap()
                .manual_writer_settled()
                .unwrap()
        );
        state.board_recovery.manual_utc.revoke();
        writer.release();
        tokio::time::timeout(Duration::from_secs(2), async {
            while state.operator_dispatch_inflight.load(Ordering::Acquire) != 0
                || state.board_recovery.manual_utc.reservation_pending()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            state
                .launch_history
                .lock()
                .unwrap()
                .manual_writer_settled()
                .unwrap()
        );
        state.manual_abort_intent.request();
        let mut claim = state.manual_abort_intent.try_claim().unwrap();
        assert!(crate::manual_abort_utc::publish_claim(&state, &router, &mut claim).unwrap());
        assert_eq!(
            claim.wire(),
            Some(crate::manual_abort_intent::WirePolicy::Untimed)
        );
        claim.accepted();
    }

    #[tokio::test]
    async fn frozen_sdk_clock_defers_same_accepted_original_until_next_tick() {
        if isolated("frozen_sdk_clock_defers_same_accepted_original_until_next_tick") { return; }
        use std::sync::atomic::AtomicU64;
        let _http = super::super::tests::http_fixture().await;
        let ticks = Arc::new(AtomicU64::new(2001));
        let clock = ticks.clone();
        let attempts = Arc::new(Mutex::new(Vec::<Packet>::new()));
        let observed = attempts.clone();
        let endpoint = crate::telemetry_schema::endpoint("BOARD_RECOVERY_FC");
        let router = Router::new_with_clock(RouterConfig::new([]).with_sender("GS"),
            Box::new(move || clock.load(Ordering::Acquire)));
        let side=router.add_side_packet("wire", move |packet| {
            if packet.data_type() != crate::telemetry_schema::data_type(QUERY_TYPE) { return Ok(()); }
            let mut attempts = observed.lock().unwrap();
            attempts.push(packet.clone());
            if attempts.len() == 1 { Err(sedsnet::TelemetryError::Io("side tx busy")) }
            else { Ok(()) }
        });
        router.rx_from_side(&sedsnet::discovery::build_discovery_announce(
            "FC",1,&[endpoint]).unwrap(),side).unwrap();
        let packet = Packet::new(crate::telemetry_schema::data_type(QUERY_TYPE),
            &[endpoint], "GS", 1793000002001, Arc::from([0u8;32])).unwrap();
        router.tx_queue(packet.clone()).unwrap();
        router.process_all_queues_with_timeout(0).unwrap();
        assert_eq!(attempts.lock().unwrap().len(),1,"actual callback must refuse first dispatch");
        for _ in 0..8 { router.process_all_queues_with_timeout(0).unwrap(); }
        assert_eq!(attempts.lock().unwrap().len(),1,"wall progress cannot mature frozen SDK deadline");
        ticks.store(2002,Ordering::Release);
        router.process_all_queues_with_timeout(0).unwrap();
        let attempts=attempts.lock().unwrap();
        assert_eq!(attempts.len(),2,"next monotonic tick releases retained original");
        for actual in attempts.iter() {
            assert_eq!(actual.packet_id(),packet.packet_id());
            assert_eq!(actual.timestamp(),packet.timestamp());
            assert_eq!(actual.payload(),packet.payload());
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manual_utc_prepare_wait_refreshes_queries_and_cold_rollback_refuses_ticket() {
        if isolated("manual_utc_prepare_wait_refreshes_queries_and_cold_rollback_refuses_ticket") {
            return;
        }
        use std::sync::atomic::AtomicU64;
        let ticks = Arc::new(AtomicU64::new(0));
        let clock_ticks = ticks.clone();
        let thaw = Arc::new(std::sync::OnceLock::<std::time::Instant>::new());
        let clock_thaw = thaw.clone();
        let fixture = Fixture::from_saved_with_clock(
            None,
            None,
            Some(Box::new(move || {
                clock_ticks.load(Ordering::Acquire).saturating_add(
                    clock_thaw.get().map_or(0, |start| start.elapsed().as_millis() as u64),
                )
            })),
        )
        .await;
        completed_manual_cohort(&fixture).await;
        let state = fixture.state();
        let router = state.topology_router.get().unwrap().clone();
        crate::manual_abort_utc::refresh(&state, &router).await;
        let key = state
            .launch_history
            .lock()
            .unwrap()
            .manual_utc_key()
            .unwrap();
        assert_eq!(key.ceiling, key.boundary + 2000);
        let before = fixture
            .models
            .iter()
            .map(|(_, m)| {
                m.lock()
                    .unwrap()
                    .requests
                    .iter()
                    .filter(|p| p.payload()[1] == 0)
                    .count()
            })
            .collect::<Vec<_>>();
        let prepare_state = state.clone();
        let prepare = tokio::spawn(async move {
            super::super::tests::http(prepare_state.clone(),"POST","/api/board-recovery/prepare",Some("operator"),json!({"action":"reset_startup","boards":["RF","FC","PB","AB","VB","GB","DAQ"],"board":null,"mode":null,"backend_instance":hex(&prepare_state.board_recovery.identity.unwrap()),"reset_epoch":format!("{:016x}",crate::gse::automatic_generation(&prepare_state))})).await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !state.board_recovery.manual_utc.is_revoked() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!prepare.is_finished());
        assert!(
            state
                .board_recovery
                .coordinator
                .0
                .lock()
                .unwrap()
                .ticket
                .is_none()
        );
        ticks.store(2001, Ordering::Release);
        // The ceiling crossing is explicit and retains the original intent.
        // Afterwards the SDK clock must advance like the independently live
        // peer clocks and HTTP deadlines. Public SDK deferred accepted originals
        // are eligible at now+1; freezing here strands a valid transient retry.
        thaw.set(std::time::Instant::now()).unwrap();
        let response = prepare.await.unwrap();
        let status = response.status();
        let ticket = body(response).await;
        // Failure-only, nonblocking RAM observations after the ORIGINAL prepare
        // future has completed. No second QUERY, clock advance, provider read,
        // SDK export, await or diagnostic mutex wait can repair this intent.
        let failed_query_snapshot = if status != StatusCode::OK {
            let wall = std::time::Instant::now();
            let queries = match state.board_recovery.inner.try_lock() {
                Ok(inner) => json!({
                    "rejected_statuses":inner.rejected_statuses,
                    "last_rejection":inner.last_rejection,
                    "boards":BOARDS.iter().map(|board| {
                        let at=index(*board).unwrap();
                        json!({
                            "board":board.sender_id(),
                            "query":inner.queries[at].as_ref().map(|q|json!({
                                "nonce":q.nonce,"received":q.received,
                                "sent_model_ms":q.sent.model_ms,
                                "sent_wall_age_ms":wall.checked_duration_since(q.sent.wall).map(|v|v.as_millis())
                            })),
                            "observation":inner.values[at].as_ref().map(|v|json!({
                                "nonce":v.value.nonce,"issuer":v.value.issuer,
                                "result":v.value.result,"state":v.value.state,
                                "boot":v.value.boot,"epoch":v.value.epoch,
                                "received_model_ms":v.received.model_ms,
                                "received_wall_age_ms":wall.checked_duration_since(v.received.wall).map(|d|d.as_millis())
                            }))
                        })
                    }).collect::<Vec<_>>()
                }),
                Err(error) => json!({"unavailable":format!("{error:?}")}),
            };
            let models=fixture.models.iter().map(|(board,model)|match model.try_lock() {
                Ok(model)=>json!({
                    "board":board.sender_id(),"request_count":model.requests.len(),
                    "last_requests":model.requests.iter().rev().take(4).map(|packet| {
                        let bytes=packet.payload();
                        json!({"packet_id":packet.packet_id(),"source_utc_ms":packet.timestamp(),
                            "command":bytes.get(1).copied(),
                            "nonce":bytes.get(4..12).map(|v|u64::from_le_bytes(v.try_into().unwrap()))})
                    }).collect::<Vec<_>>()
                }),
                Err(error)=>json!({"board":board.sender_id(),"unavailable":format!("{error:?}")}),
            }).collect::<Vec<_>>();
            let snapshot=json!({"sdk_clock_ticks":ticks.load(Ordering::Acquire),
                "original_manual_boundary":key.boundary,"retained_manual_ceiling":key.ceiling,
                "queries":queries,"models":models,
                "scope":"post-response/non-atomic; last4requests perboard; no wire/reply/SDK-worker-progress or causality proof"});
            eprintln!("MANUAL_UTC_FRESH_QUERY_FAILURE {snapshot}");
            Some(snapshot)
        } else {None};
        let diagnostic = json!({
            "failure_query_snapshot":failed_query_snapshot,
            "response":ticket,
            "state":format!("{:?}",state.local_flight_state_snapshot()),
            "generation":crate::gse::automatic_generation(&state),
            "barrier":state.operator_admission_barrier.load(Ordering::Acquire),
            "recovery":state.launch_history.lock().unwrap().recovery_pending(),
            "manual_utc_revoked":state.board_recovery.manual_utc.is_revoked(),
            "wire_alive":!fixture.pump.is_finished(),"receiver_alive":!fixture.receiver.is_finished()
        });
        assert_eq!(status, StatusCode::OK, "original UTC prepare refused; no retry: {diagnostic}");
        for ((_, model), before) in fixture.models.iter().zip(before) {
            let model = model.lock().unwrap();
            assert_eq!(
                model
                    .requests
                    .iter()
                    .filter(|p| p.payload()[1] == 0)
                    .count(),
                before + 1,
                "fresh dedicated query after ceiling wait"
            );
        }
        // Live public437 disciplines a backward adjustment. Preserve this fact;
        // the actual rollback threat here is a restarted router with retained disk.
        router
            .try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(
                key.boundary,
            ))
            .unwrap();
        assert!(
            crate::system_clock::required_control_network_utc_ms(&router).unwrap() > key.ceiling
        );
        assert!(ticket["ticket"].is_string());
        let journal = std::fs::read(fixture.http.directory.join("clock.json")).unwrap();
        let retained = fixture.models.clone();
        drop(fixture);
        let cold =
            Fixture::from_saved_with_clock(Some(journal), Some(retained), Some(Box::new(|| 0)))
                .await;
        let cold_state = cold.state();
        assert!(
            crate::system_clock::required_control_network_utc_ms(
                cold_state.topology_router.get().unwrap()
            )
            .unwrap()
                < key.ceiling
        );
        let prepare_state = cold_state.clone();
        let prepare = tokio::spawn(async move {
            super::super::tests::http(prepare_state.clone(),"POST","/api/board-recovery/prepare",Some("operator"),json!({"action":"reset_startup","boards":["RF","FC","PB","AB","VB","GB","DAQ"],"board":null,"mode":null,"backend_instance":hex(&prepare_state.board_recovery.identity.unwrap()),"reset_epoch":format!("{:016x}",crate::gse::automatic_generation(&prepare_state))})).await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !cold_state.board_recovery.manual_utc.is_revoked() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!prepare.is_finished());
        let start = std::time::Instant::now();
        cold_state.manual_abort_intent.request();
        assert!(start.elapsed() < Duration::from_millis(100));
        let mut stop = cold_state.manual_abort_intent.try_claim().unwrap();
        assert!(
            crate::manual_abort_utc::publish_claim(
                &cold_state,
                cold_state.topology_router.get().unwrap(),
                &mut stop
            )
            .unwrap()
        );
        assert_eq!(
            stop.wire(),
            Some(crate::manual_abort_intent::WirePolicy::Untimed)
        );
        stop.accepted();
        let response = prepare.await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(
            cold_state
                .board_recovery
                .coordinator
                .0
                .lock()
                .unwrap()
                .ticket
                .is_none()
        );
        assert!(
            cold.models
                .iter()
                .all(|(_, m)| m.lock().unwrap().applied == 1)
        );
        assert_eq!(
            cold_state
                .launch_history
                .lock()
                .unwrap()
                .retained_manual_utc_ceiling()
                .unwrap(),
            key.ceiling
        );
    }

    #[tokio::test]
    async fn manual_utc_missing_busy_rrls_and_first_max_histories_select_only_legacy() {
        if isolated("manual_utc_missing_busy_rrls_and_first_max_histories_select_only_legacy") {
            return;
        }
        let fixture = Fixture::new().await;
        completed_manual_cohort(&fixture).await;
        let state = fixture.state();
        let router = state.topology_router.get().unwrap().clone();
        crate::manual_abort_utc::refresh(&state, &router).await;
        let legacy = || {
            state.manual_abort_intent.request();
            let mut claim = state.manual_abort_intent.try_claim().unwrap();
            assert!(crate::manual_abort_utc::publish_claim(&state, &router, &mut claim).unwrap());
            assert_eq!(
                claim.wire(),
                Some(crate::manual_abort_intent::WirePolicy::Untimed)
            );
            claim.accepted();
        };
        let history = state.launch_history.lock().unwrap();
        legacy();
        drop(history);
        // Isolated process: secondary lacks its own durable cohort and UTC proof.
        unsafe {
            std::env::set_var("GS_RRLS_ROLE", "rrls");
        }
        legacy();
        unsafe {
            std::env::set_var("GS_RRLS_ROLE", "primary");
        }
        *state.launch_history.lock().unwrap() = crate::launch_history::Store::ephemeral();
        legacy();
        drop(fixture);
        let virgin = Fixture::new().await;
        {
            let mut ab = virgin
                .models
                .iter()
                .find(|(b, _)| *b == Board::ActuatorBoard)
                .unwrap()
                .1
                .lock()
                .unwrap();
            ab.boot = u64::MAX;
            ab.result = 7;
        }
        for board in BOARDS {
            virgin.query(board).await;
        }
        let ticket = virgin.prepare("reset_startup", None).await;
        assert_eq!(virgin.confirm(&ticket).await.status(), StatusCode::OK);
        assert_eq!(virgin.terminal().await["success"], false);
        let state = virgin.state();
        assert!(
            state
                .launch_history
                .lock()
                .unwrap()
                .manual_utc_key()
                .is_none()
        );
        state.manual_abort_intent.request();
        let mut claim = state.manual_abort_intent.try_claim().unwrap();
        assert!(
            crate::manual_abort_utc::publish_claim(
                &state,
                state.topology_router.get().unwrap(),
                &mut claim
            )
            .unwrap()
        );
        assert_eq!(
            claim.wire(),
            Some(crate::manual_abort_intent::WirePolicy::Untimed)
        );
        claim.accepted();
    }

    #[tokio::test]
    async fn manual_utc_actual_clock_record_revision_invalidates_cached_selection() {
        if isolated("manual_utc_actual_clock_record_revision_invalidates_cached_selection") {
            return;
        }
        let fixture = Fixture::new().await;
        completed_manual_cohort(&fixture).await;
        let state = fixture.state();
        let router = state.topology_router.get().unwrap().clone();
        crate::manual_abort_utc::refresh(&state, &router).await;
        let before = state
            .launch_history
            .lock()
            .unwrap()
            .manual_utc_key()
            .unwrap();
        let clock = state.launch_history.lock().unwrap().snapshot();
        // After an operator reset the clock waits for a new launch cycle, so an
        // explicit T-clock reset is what moves the journal revision there.
        #[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
        state.try_reset_launch_clock().unwrap();
        #[cfg(not(any(feature = "hitl_mode", feature = "test_fire_mode")))]
        state.set_launch_clock(crate::telemetry_db::LaunchClockMsg {
            kind: crate::telemetry_db::LaunchClockKind::TMinus,
            anchor_timestamp_ms: Some(crate::telemetry_task::get_current_timestamp_ms() as i64),
            duration_ms: Some(60_000),
        });
        let after = state
            .launch_history
            .lock()
            .unwrap()
            .manual_utc_key()
            .unwrap();
        assert!(state.launch_history.lock().unwrap().snapshot().revision > clock.revision);
        assert_ne!(before.ticket, after.ticket);
        assert!(
            crate::board_recovery::manual_utc_proof(&state, after).is_some(),
            "fresh capability remains; journal revision alone revokes old cache identity"
        );
        state.manual_abort_intent.request();
        let mut claim = state.manual_abort_intent.try_claim().unwrap();
        assert!(crate::manual_abort_utc::publish_claim(&state, &router, &mut claim).unwrap());
        assert_eq!(
            claim.wire(),
            Some(crate::manual_abort_intent::WirePolicy::Untimed)
        );
        claim.accepted();
    }

    #[tokio::test]
    async fn manual_utc_authorized_proxy_remains_untimed_with_eligible_primary_cache_and_denied_actor_cannot_retain()
     {
        if isolated(
            "manual_utc_authorized_proxy_remains_untimed_with_eligible_primary_cache_and_denied_actor_cannot_retain",
        ) {
            return;
        }
        let fixture = Fixture::new().await;
        completed_manual_cohort(&fixture).await;
        let state = fixture.state();
        let router = state.topology_router.get().unwrap().clone();
        crate::manual_abort_utc::refresh(&state, &router).await;
        state.manual_abort_intent.request();
        let mut local = state.manual_abort_intent.try_claim().unwrap();
        assert!(crate::manual_abort_utc::publish_claim(&state, &router, &mut local).unwrap());
        assert!(matches!(
            local.wire(),
            Some(crate::manual_abort_intent::WirePolicy::KnownUtc(_))
        ));
        local.accepted();
        let highwater = state.manual_abort_intent.try_known_highwater().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer viewer".parse().unwrap());
        let viewer = crate::web::authorize_headers(&state, &headers, Permission::ViewData)
            .await
            .unwrap();
        let denied = crate::web::dispatch_proxied_command(
            state.clone(),
            viewer,
            crate::types::TelemetryCommand::Abort,
            None,
        )
        .await;
        assert_eq!(denied.0, StatusCode::FORBIDDEN);
        assert_eq!(state.manual_abort_intent.epoch(), 1);
        assert_eq!(state.manual_abort_intent.pending_epoch(), None);
        headers.insert("authorization", "Bearer operator".parse().unwrap());
        let operator = crate::web::authorize_headers(&state, &headers, Permission::SendCommands)
            .await
            .unwrap();
        let accepted = crate::web::dispatch_proxied_command(
            state.clone(),
            operator,
            crate::types::TelemetryCommand::Abort,
            None,
        )
        .await;
        assert_eq!(accepted.0, StatusCode::ACCEPTED);
        assert_eq!(state.manual_abort_intent.pending_epoch(), Some(2));
        let mut proxy = state.manual_abort_intent.try_claim().unwrap();
        assert!(crate::manual_abort_utc::publish_claim(&state, &router, &mut proxy).unwrap());
        assert_eq!(
            proxy.wire(),
            Some(crate::manual_abort_intent::WirePolicy::Untimed)
        );
        proxy.accepted();
        assert_eq!(
            state.manual_abort_intent.try_known_highwater(),
            Some(highwater)
        );
    }

    #[tokio::test]
    async fn board_abort_actual_sdk_any_of_seven_retains_error_and_fresh_query_alone_clears() {
        if isolated(
            "board_abort_actual_sdk_any_of_seven_retains_error_and_fresh_query_alone_clears",
        ) {
            return;
        }
        let fixture = Fixture::new().await;
        completed_manual_cohort(&fixture).await;
        let state = fixture.state();
        let router = state.topology_router.get().unwrap();
        let manual = state.manual_abort_intent.epoch();
        let deferred = state.deferred_abort.manual_epoch();
        for (board, model) in &fixture.models {
            model.lock().unwrap().state = 15;
            let packet = Packet::new(
                crate::telemetry_schema::data_type("FLIGHT_STATE"),
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                board.sender_id(),
                router.network_time_ms().unwrap(),
                Arc::from([crate::types::FlightState::Aborted as u8]),
            )
            .unwrap();
            fixture.inject_state(packet).await;
            assert_eq!(
                state.board_abort.snapshot(0).active_errors.len(),
                1,
                "{}",
                board.sender_id()
            );
            let launches=||fixture.controller_receipts.lock().unwrap().iter().filter(|packet|
                packet.data_type()==crate::telemetry_schema::data_type("FLIGHT_COMMAND")&&packet.payload()==[1]).count();
            let before=launches();
            assert!(
                !crate::gse::queue_controller_with_stop_fence(
                    &state,
                    router,
                    crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
                    &[1]
                )
                .unwrap()
            );
            router.process_all_queues_with_timeout(0).unwrap();
            assert_eq!(launches(),before,"no actual SDK Launch delivery after observed Abort");
            assert!(!router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type("FLIGHT_COMMAND")),
                "no retained SDK Launch owner after observed Abort");
            assert_eq!(state.manual_abort_intent.epoch(), manual);
            assert_eq!(state.deferred_abort.manual_epoch(), deferred);
            // Raw non-Abort is only a request for confirmation, never exit authority.
            model.lock().unwrap().state = crate::types::FlightState::Recovery as u8;
            let raw = Packet::new(
                crate::telemetry_schema::data_type("FLIGHT_STATE"),
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                board.sender_id(),
                router.network_time_ms().unwrap(),
                Arc::from([crate::types::FlightState::Recovery as u8]),
            )
            .unwrap();
            fixture.inject_state(raw).await;
            assert_eq!(state.board_abort.snapshot(0).active_errors.len(), 1);
            fixture.query(*board).await;
            assert!(state.board_abort.snapshot(0).active_errors.is_empty());
        }
    }

    #[tokio::test]
    async fn board_abort_two_actual_boards_clear_independently_and_report_all_names() {
        if isolated("board_abort_two_actual_boards_clear_independently_and_report_all_names") {
            return;
        }
        let fixture = Fixture::new().await;
        completed_manual_cohort(&fixture).await;
        let state = fixture.state();
        for board in [Board::ActuatorBoard, Board::DaqBoard] {
            fixture
                .models
                .iter()
                .find(|(b, _)| *b == board)
                .unwrap()
                .1
                .lock()
                .unwrap()
                .state = 15;
            fixture.query(board).await;
        }
        let message = crate::board_abort::blocked_message(&state).unwrap();
        assert!(message.contains(Board::ActuatorBoard.as_str()), "{message}");
        assert!(message.contains(Board::DaqBoard.as_str()), "{message}");
        assert_eq!(state.board_abort.snapshot(0).active_errors.len(), 2);
        state.acknowledge_alerts_through(i64::MAX, i64::MAX);
        assert!(
            crate::board_abort::blocked_message(&state).is_some(),
            "acknowledgment cannot clear actual condition"
        );
        assert!(!state.is_command_allowed(&crate::types::TelemetryCommand::Launch));
        fixture
            .models
            .iter()
            .find(|(b, _)| *b == Board::ActuatorBoard)
            .unwrap()
            .1
            .lock()
            .unwrap()
            .state = 14;
        fixture.query(Board::ActuatorBoard).await;
        let message = crate::board_abort::blocked_message(&state).unwrap();
        assert!(
            !message.contains(Board::ActuatorBoard.as_str()),
            "{message}"
        );
        assert!(message.contains(Board::DaqBoard.as_str()), "{message}");
        assert_eq!(state.board_abort.snapshot(0).active_errors.len(), 1);
        fixture
            .models
            .iter()
            .find(|(b, _)| *b == Board::DaqBoard)
            .unwrap()
            .1
            .lock()
            .unwrap()
            .state = 14;
        fixture.query(Board::DaqBoard).await;
        assert!(crate::board_abort::blocked_message(&state).is_none());
        // Exiting a board Abort is not authority to bypass the remaining policy.
        *state.state.lock().unwrap() = crate::types::FlightState::Startup;
        #[cfg(feature = "hitl_mode")]
        state
            .hitl_launch_interlock_enabled
            .store(true, Ordering::Release);
        assert!(!state.is_command_allowed(&crate::types::TelemetryCommand::Launch));
    }

    #[tokio::test]
    async fn board_abort_after_pure_launch_preparation_admits_zero_and_cannot_create_restart_launch()
     {
        if isolated(
            "board_abort_after_pure_launch_preparation_admits_zero_and_cannot_create_restart_launch",
        ) {
            return;
        }
        let fixture = Fixture::new().await;
        completed_manual_cohort(&fixture).await;
        let state = fixture.state();
        let router = state.topology_router.get().unwrap();
        let ticket = crate::gse::LaunchAdmission::capture(&state);
        let desired = crate::network_variables::flight_state();
        let clock = state.launch_clock_snapshot();
        let host = state.local_flight_state_snapshot();
        let prepared = crate::network_variables::prepare_launch_state(router).unwrap();
        assert_eq!(
            crate::network_variables::flight_state(),
            desired,
            "pure preparation must not persist Launch7"
        );
        fixture
            .models
            .iter()
            .find(|(b, _)| *b == Board::ValveBoard)
            .unwrap()
            .1
            .lock()
            .unwrap()
            .state = 15;
        fixture.query(Board::ValveBoard).await;
        let before = router.queue_counters()[2];
        let error = crate::gse::queue_prepared_launch_packets(
            &state,
            router,
            ticket,
            Vec::new(),
            Some(prepared),
        )
        .unwrap_err();
        assert!(error.contains(Board::ValveBoard.as_str()), "{error}");
        assert_eq!(
            router.queue_counters()[2],
            before,
            "Abort won final gate: no Launch7 or physical Launch owner"
        );
        assert_eq!(state.local_flight_state_snapshot(), host);
        assert_eq!(state.launch_clock_snapshot(), clock);
        assert_eq!(crate::network_variables::flight_state(), desired);
        // The actual restart/bootstrap path must not invent a Launch from prep.
        let restart = Router::new(RouterConfig::new([]));
        crate::network_variables::initialize_for_state(&state, &restart).unwrap();
        assert_ne!(
            restart
                .get_cached_network_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
                .unwrap()
                .map(|p| p.payload().to_vec()),
            Some(vec![7])
        );
    }

    #[tokio::test]
    async fn board_abort_managed_cache_keeps_original_board_provenance_without_overwriting_host_seed()
     {
        if isolated(
            "board_abort_managed_cache_keeps_original_board_provenance_without_overwriting_host_seed",
        ) {
            return;
        }
        let fixture = Fixture::new().await;
        completed_manual_cohort(&fixture).await;
        let state = fixture.state();
        let router = state.topology_router.get().unwrap();
        crate::network_variables::initialize_for_state(&state, router).unwrap();
        let original = crate::network_variables::flight_state();
        for board in [
            Board::ActuatorBoard,
            Board::ValveBoard,
            Board::DaqBoard,
            Board::FlightComputer,
        ] {
            let packet = Packet::new(
                crate::telemetry_schema::data_type("FLIGHT_STATE"),
                &[
                    crate::telemetry_schema::endpoint("GROUND_STATION"),
                    crate::telemetry_schema::endpoint("FLIGHT_STATE"),
                ],
                board.sender_id(),
                router.network_time_ms().unwrap(),
                Arc::from([15u8]),
            )
            .unwrap();
            let id = packet.packet_id();
            let stamp = packet.timestamp();
            fixture.inject_state(packet).await;
            assert_eq!(crate::network_variables::flight_state(), original);
            let cached = router
                .get_cached_network_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
                .unwrap()
                .unwrap();
            assert_eq!(cached.sender(), board.sender_id());
            assert_eq!(cached.packet_id(), id);
            assert_eq!(cached.timestamp(), stamp);
        }
        let legitimate = Packet::new(
            crate::telemetry_schema::data_type("FLIGHT_STATE"),
            &[
                crate::telemetry_schema::endpoint("GROUND_STATION"),
                crate::telemetry_schema::endpoint("FLIGHT_STATE"),
            ],
            Board::FlightComputer.sender_id(),
            router.network_time_ms().unwrap(),
            Arc::from([8u8]),
        )
        .unwrap();
        fixture.inject_state(legitimate).await;
        assert_eq!(
            crate::network_variables::flight_state(),
            8,
            "legitimate normalized FC authority remains durable"
        );
        for reboot in [0u8, 1, 6] {
            let packet = Packet::new(
                crate::telemetry_schema::data_type("FLIGHT_STATE"),
                &[
                    crate::telemetry_schema::endpoint("GROUND_STATION"),
                    crate::telemetry_schema::endpoint("FLIGHT_STATE"),
                ],
                "FC",
                router.network_time_ms().unwrap(),
                Arc::from([reboot]),
            )
            .unwrap();
            fixture.inject_state(packet).await;
            assert_eq!(
                crate::network_variables::flight_state(),
                8,
                "FC reboot cannot change durable global flight phase"
            );
        }
        assert_eq!(state.board_abort.snapshot(0).active_errors.len(), 4);
    }

    #[tokio::test]
    async fn board_abort_actual_sdk_refusal_at_each_prepared_batch_position_keeps_ownership_truthful()
     {
        if crate::operator_queue::isolated_native_case(module_path!(),
            "board_abort_actual_sdk_refusal_at_each_prepared_batch_position_keeps_ownership_truthful",
        ) {
            return;
        }
        crate::telemetry_schema::initialize().unwrap();
        let make_router = |budget| {
            let memory =
                sedsnet::config::RuntimeMemoryConfig::new(budget, 1, budget.min(128), 1.25)
                    .unwrap();
            let router = Router::new(
                RouterConfig::new([])
                    .with_sender("GS")
                    .with_memory_config(memory)
                    .unwrap(),
            );
            crate::telemetry_task::refresh_host_network_time(
                &router,
                crate::telemetry_task::get_current_timestamp_ms(),
            );
            router
        };
        let physical = |router: &Router| {
            [
                ("FLIGHT_COMMAND", "FLIGHT_CONTROLLER", 1u8),
                ("VALVE_COMMAND", "VALVE_BOARD", 6u8),
            ]
            .into_iter()
            .map(|(ty, endpoint, command)| {
                Packet::new(
                    crate::telemetry_schema::data_type(ty),
                    &[
                        crate::telemetry_schema::endpoint("GROUND_STATION"),
                        crate::telemetry_schema::endpoint(endpoint),
                    ],
                    "GS",
                    router.network_time_ms().unwrap(),
                    Arc::from([command]),
                )
                .unwrap()
            })
            .collect::<Vec<_>>()
        };
        for admitted in 0..3 {
            // Discover the real SDK's exact budget boundary for the next member.
            // No production queue, budget, rate, or priority is changed.
            let probe = |budget| {
                let router = make_router(budget);
                let prepared = crate::network_variables::prepare_launch_state(&router).unwrap();
                let mut packets = vec![prepared.packet];
                packets.extend(physical(&router));
                let mut count = 0;
                for packet in packets {
                    if router.tx_queue(packet).is_err() {
                        break;
                    }
                    count += 1;
                }
                count
            };
            let mut lower = 16usize;
            let mut upper = 32768usize;
            assert!(probe(upper) > admitted);
            while upper - lower > 1 {
                let middle = lower + (upper - lower) / 2;
                if probe(middle) > admitted {
                    upper = middle;
                } else {
                    lower = middle;
                }
            }
            assert_eq!(
                probe(lower),
                admitted,
                "actual prefix must exist at every batch position"
            );
            let router=make_router(lower);let prepared=crate::network_variables::prepare_launch_state(&router).unwrap();let before=router.queue_counters()[2];
            let mut packets=vec![prepared.packet];packets.extend(physical(&router));
            let actual=packets.into_iter().take_while(|p|router.try_tx_queue(p.clone()).is_ok()).count();
            assert_eq!(actual,admitted);assert_eq!(router.queue_counters()[2]-before,admitted as u32,"exact real SDK admitted prefix on original bare-router control");
        }
        // Genuine same-Router admission under existing readonly queue pressure.
        // Plain and GPS+QUERY cases have actual observed2/3 and0/3 respectively.
        // Middle positions remain an explicit assertion across bounded readonly
        // profiles below, never
        // inferred from net occupancy: priority16 metadata can be evicted by
        // priority200 commands after its original admission.
        for admitted in 0..3usize {
            let mut seen=false;
            'search: for budget in [18905usize,18957,19009] {for pressure in 0..7 {
                let mut scene=crate::ignition_pair::constrained_fixture::Scene::new(budget).await;
                scene.warm().await.unwrap();
                // Fresh queried SAME Router scope; this does not claim the old
                // completed-recovery premise from a different Fixture Router.
                // APP HEARTBEAT and QUERY-alone are bounded readonly alternatives
                // for the middle prefix; no policy or memory parameter changes.
                let pressure_ready=match pressure {
                    0=>true,
                    1=>{scene.preload_readonly_gps_pressure();true},
                    2..=4=>{scene.preload_readonly_gps_pressure();scene.preload_readonly_query([Board::RFBoard,Board::PowerBoard,Board::FlightComputer][pressure-2])},
                    5=>scene.preload_readonly_heartbeat(),
                    6=>scene.preload_readonly_query(Board::RFBoard),
                    _=>unreachable!(),
                };
                if !pressure_ready{scene.state.request_shutdown();scene.state.telemetry_db_pool().close().await;scene.state.auth_db.close().await;continue;}
                assert!(scene.scope_current(),"pair and required board scope must remain current at offer");
                let state=&scene.state;let router=&scene.host;
                let ticket=crate::gse::inhibit_automatic_actions_for_launch(state).unwrap();
                let prepared=crate::network_variables::prepare_launch_state(router).unwrap();
                let original_clock=state.launch_clock_snapshot();let original_state=state.local_flight_state_snapshot();let original_desired=crate::network_variables::flight_state();
                let result=crate::gse::queue_prepared_launch_packets(state,router,ticket,physical(router),Some(prepared));
                // Never process, flush, clear or cancel after any pressure offer.
                if let Err(error)=result && error.contains(&format!("accepted {admitted}/3 packets:"))&&error.contains("queue saturated"){
                    assert!(error.contains(&format!("{admitted}/3")),"{error}");
                    assert_eq!(router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type("FLIGHT_COMMAND")),admitted>=2);
                    assert!(!router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type("VALVE_COMMAND")));
                    if admitted==0{assert_eq!(state.local_flight_state_snapshot(),original_state);assert_eq!(state.launch_clock_snapshot(),original_clock);assert_eq!(crate::network_variables::flight_state(),original_desired);}else{
                        assert_eq!(state.local_flight_state_snapshot(),crate::types::FlightState::Launch);assert_eq!(crate::network_variables::flight_state(),7);assert!(error.contains("explicit manual recovery required"));
                    }
                    assert!(state.launch_history.lock().unwrap().automatic_actions_inhibited());assert_eq!(state.manual_abort_intent.epoch(),0,"queue refusal never automatically Aborts");
                    seen=true;
                }
                scene.state.request_shutdown();scene.state.telemetry_db_pool().close().await;scene.state.auth_db.close().await;
                if seen{break 'search;}
            }}
            assert!(seen,"genuine qualified same-Router actual SDK prefix {admitted}/3 must be exercised; a precondition refusal is not ownership evidence");
        }

    }

    #[tokio::test]
    async fn board_abort_healthy_session_refreshes_only_queries_without_settings_or_launch_demand() {
        if crate::operator_queue::isolated_native_case(module_path!(),"board_abort_healthy_session_refreshes_only_queries_without_settings_or_launch_demand") { return; }
        let fixture = Fixture::from_saved_with_services_profile_and_c_boards(None,None,None,false,false,true,false).await;
        completed_manual_cohort(&fixture).await;
        let state = fixture.state();
        let baseline = fixture.models.iter().map(|(_,m)|m.lock().unwrap().requests.len()).collect::<Vec<_>>();
        assert_eq!(state.board_abort.take_queries(), 0);
        let worker = tokio::spawn(crate::board_abort::query_service(state.clone(), state.shutdown_tx.subscribe()));
        let started = std::time::Instant::now();
        while started.elapsed() < Duration::from_secs(10) {
            for (board,_) in &fixture.models {
                state.network_health.lock().unwrap().observe(board.sender_id(), std::time::Instant::now(),100);
            }
            // Presentation does not itself create requests, grant proof, or
            // publish a mode. The dedicated backend worker owns refresh.
            state.action_policy_snapshot();
            let now = Stamp::now();
            {
                let inner = state.board_recovery.inner.lock().unwrap();
                for row in inner.values.iter().flatten() {
                    assert!(row.fresh(now), "healthy state expired despite proactive read-only refresh");
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        worker.abort();
        let mut total = 0;
        for ((_,model),before) in fixture.models.iter().zip(baseline) {
            let model=model.lock().unwrap();
            let added=&model.requests[before..];
            assert!(!added.is_empty(), "healthy board was never refreshed");
            assert!(added.iter().all(|packet|packet.payload()[..4]==[1,0,0,0]), "background must emit QUERY only");
            total+=added.len();
        }
        assert!(total<=60, "background requests exceeded the bounded fair refresh budget");
        assert!(state.board_abort.snapshot(0).active_errors.is_empty());
        assert!(!state.topology_router.get().unwrap().has_pending_reliable_tx_type(crate::telemetry_schema::data_type("FLIGHT_COMMAND")));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn board_abort_shared_gate_real_sdk_core_hold_and_reentrant_ingress_do_not_invert() {
        if isolated(
            "board_abort_shared_gate_real_sdk_core_hold_and_reentrant_ingress_do_not_invert",
        ) {
            return;
        }
        use std::sync::{Condvar, OnceLock, atomic::AtomicBool};
        let state = crate::state::tests::test_app_state().await;
        let held = Arc::new(AtomicBool::new(false));
        let pause = Arc::new((Mutex::new(false), Condvar::new()));
        struct Release(Arc<(Mutex<bool>, Condvar)>);
        impl Drop for Release {
            fn drop(&mut self) {
                *self.0.0.lock().unwrap() = true;
                self.0.1.notify_all();
            }
        }
        let release = Release(pause.clone());
        let clock_pause = pause.clone();
        let clock_held = held.clone();
        let (core_entered, got_core) = std::sync::mpsc::channel();
        let slot = Arc::new(OnceLock::<Arc<Router>>::new());
        let callback_slot = slot.clone();
        let observed = state.clone();
        let router = Arc::new(Router::new_with_clock(
            RouterConfig::new([EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint("GROUND_STATION"),
                move |packet| {
                    if packet.sender() == "AB" {
                        // This ACTUAL SDK callback can re-enter tx_queue on the same router.
                        // It would deadlock here if the public RX path retained Router.state.
                        let echo = Packet::new(
                            crate::telemetry_schema::data_type("FLIGHT_STATE"),
                            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                            "GS",
                            1,
                            Arc::from([crate::types::FlightState::Recovery as u8]),
                        )
                        .unwrap();
                        callback_slot.get().unwrap().tx_queue(echo)?;
                        crate::board_abort::observe_raw(&observed, packet);
                    }
                    Ok(())
                },
            )])
            .with_sender("GS"),
            Box::new(move || {
                if clock_held.load(Ordering::Acquire)
                    && std::thread::current().name() == Some("actual-sdk-core-holder")
                {
                    let _ = core_entered.send(());
                    let mut ready = clock_pause.0.lock().unwrap();
                    while !*ready {
                        ready = clock_pause.1.wait(ready).unwrap();
                    }
                }
                0
            }),
        ));
        slot.set(router.clone()).ok().unwrap();
        state.topology_router.set(router.clone()).ok().unwrap();
        let seed = Packet::new(
            crate::telemetry_schema::data_type("AV_BAY_UNDERGLOW"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "GS",
            1,
            Arc::from([0u8]),
        )
        .unwrap();
        let launch = crate::telemetry_task::controller_packet(
            &router,
            crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
            &[1],
        )
        .unwrap();
        let aborted = Packet::new(
            crate::telemetry_schema::data_type("FLIGHT_STATE"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "AB",
            1,
            Arc::from([15u8]),
        )
        .unwrap();
        held.store(true, Ordering::Release);
        let core = router.clone();
        let holder = std::thread::Builder::new()
            .name("actual-sdk-core-holder".into())
            .spawn(move || core.seed_managed_variable(seed))
            .unwrap();
        got_core
            .recv_timeout(Duration::from_secs(2))
            .expect("seed Clock called while actual Router.state held");
        let launch_state = state.clone();
        let launch_router = router.clone();
        let (reached, got_gate) = std::sync::mpsc::channel();
        let final_admission = std::thread::spawn(move || {
            launch_state
                .board_abort
                .admit_launch(&launch_state, 0, 0, || {
                    reached.send(()).unwrap();
                    launch_router.tx_queue(launch)
                })
        });
        got_gate.recv_timeout(Duration::from_secs(2)).unwrap();
        let ingress_router = router.clone();
        let (rx_done, got_rx) = std::sync::mpsc::channel();
        let ingress = std::thread::spawn(move || {
            ingress_router.rx_queue(aborted).unwrap();
            ingress_router.process_rx_queue().unwrap();
            rx_done.send(()).unwrap();
        });
        drop(release);
        holder.join().unwrap().unwrap();
        final_admission.join().unwrap().unwrap().unwrap();
        got_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("actual ingress must complete without SDK-to-register inversion");
        ingress.join().unwrap();
        assert_ne!(state.board_abort.masks().0, 0);
        assert_eq!(state.manual_abort_intent.epoch(), 0);
        assert_eq!(state.deferred_abort.manual_epoch(), 0);
        // Launch won the final gate first; later observation gates only future launches.
        assert!(
            state
                .board_abort
                .admit_launch(&state, 0, state.board_abort.abort_revision(), || ())
                .is_err()
        );
    }

    #[tokio::test]
    async fn board_abort_actual_status_must_refuse_sdk_launch_without_changing_manual_owner() {
        if crate::operator_queue::isolated_native_case(module_path!(),"board_abort_actual_status_must_refuse_sdk_launch_without_changing_manual_owner") {
            return;
        }
        let fixture = Fixture::from_saved_with_services_profile_and_c_boards(None,None,None,false,false,true,false).await;
        completed_manual_cohort(&fixture).await;
        let state = fixture.state();
        let router = state.topology_router.get().unwrap();
        let model = &fixture
            .models
            .iter()
            .find(|(b, _)| *b == Board::ActuatorBoard)
            .unwrap()
            .1;
        model.lock().unwrap().state = crate::types::FlightState::Aborted as u8;
        fixture.query(Board::ActuatorBoard).await;
        let manual = state.manual_abort_intent.epoch();
        let observed = state.deferred_abort.manual_epoch();
        let accepted = crate::gse::queue_controller_with_stop_fence(
            &state,
            router,
            crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
            &[crate::rocket_commands::FlightComputerCommands::Launch as u8],
        )
        .unwrap();
        assert!(
            !accepted,
            "fresh actual AB Abort must refuse real SDK Launch admission"
        );
        assert_eq!(state.manual_abort_intent.epoch(), manual);
        assert_eq!(state.deferred_abort.manual_epoch(), observed);
        model.lock().unwrap().state = crate::types::FlightState::Recovery as u8;
        fixture.query(Board::ActuatorBoard).await;
        assert!(
            crate::gse::queue_controller_with_stop_fence(
                &state,
                router,
                crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
                &[crate::rocket_commands::FlightComputerCommands::Launch as u8]
            )
            .unwrap(),
            "fresh actual normalized Recovery14 exit proof releases only board guard"
        );
    }

    #[tokio::test]
    async fn real_reset_all_routes_recover_aborted_with_seven_sdk_queries_and_durable_ack() {
        if isolated("real_reset_all_routes_recover_aborted_with_seven_sdk_queries_and_durable_ack")
        {
            return;
        }
        #[cfg(feature="test_fire_mode")]
        let _tf_probe = tf_fresh_receipt_probe();
        #[cfg(feature="test_fire_mode")]
        let fixture = Fixture::new_with_managed_peers().await;
        #[cfg(not(feature="test_fire_mode"))]
        let fixture = Fixture::new().await;
        *fixture.http.state.state.lock().unwrap() = crate::types::FlightState::Aborted;
        let original = fixture.http.state.launch_history.lock().unwrap().snapshot();
        let mut publications = fixture.http.state.state_tx.subscribe();
        for board in BOARDS {
            fixture.query(board).await;
        }
        let ticket = fixture.prepare("reset_startup", None).await;
        assert_eq!(fixture.confirm(&ticket).await.status(), StatusCode::OK);
        let operation = fixture.terminal().await;
        assert_eq!(operation["success"], true, "{operation}");
        let mut observed_startup = false;
        let mut observed_idle_after_startup = false;
        while let Ok(message) = publications.try_recv() {
            if message.state == crate::types::FlightState::Startup {
                observed_startup = true;
            }
            if message.state == crate::types::FlightState::Idle && observed_startup {
                observed_idle_after_startup = true;
            }
        }
        assert!(
            observed_startup,
            "actual Startup publication must precede policy refresh"
        );
        assert_eq!(operation["reset_applied_state"], 0);
        if cfg!(feature = "hitl_mode") {
            assert_eq!(
                fixture.http.state.local_flight_state_snapshot(),
                crate::types::FlightState::Idle,
                "HITL reset finishes at Idle"
            );
        } else if cfg!(feature="test_fire_mode") {
            assert!(!observed_idle_after_startup, "TF reset cleanup must not invent a fresh MCU readiness receipt");
            assert_eq!(fixture.http.state.local_flight_state_snapshot(),crate::types::FlightState::Startup);
            #[cfg(feature="test_fire_mode")]
            assert_tf_idle_from_fresh_mcu_receipts(&fixture).await;
        } else {
            assert!(
                observed_idle_after_startup,
                "existing normal power-up policy must run AFTER actual Startup cleanup"
            );
            assert_eq!(
                fixture.http.state.local_flight_state_snapshot(),
                crate::types::FlightState::Idle
            );
        }
        assert_eq!(
            fixture
                .http
                .state
                .operator_admission_barrier
                .load(Ordering::Acquire),
            0
        );
        for (_, model) in &fixture.models {
            let m = model.lock().unwrap();
            assert_eq!((m.state, m.epoch, m.applied), (0, 9, 1));
            assert!(m.requests.iter().filter(|p| p.payload()[1] == 0).count() >= 3);
        }
        let history = fixture.http.state.launch_history.lock().unwrap();
        let current = history.snapshot();
        if cfg!(any(feature = "hitl_mode", feature = "test_fire_mode")) {
            assert_ne!(original.session, current.session, "operator reset starts a new T-clock session");
            assert_eq!(current.t0_utc_ms, None);
            assert_eq!(current.clock.kind, crate::telemetry_db::LaunchClockKind::Idle);
            assert!(!fixture.http.state.launch_indicator_latched());
        } else {
            assert_eq!(original.session, current.session);
            assert_eq!(original.t0_utc_ms, current.t0_utc_ms);
        }
        // One revision for recovery, one for the operator reset's T-clock reset.
        let revisions = if cfg!(any(feature = "hitl_mode", feature = "test_fire_mode")) { 2 } else { 1 };
        assert_eq!(current.revision, original.revision + revisions);
        let marker = history.recovery_pending().unwrap();
        assert!(marker.published);
        for (_, model) in &fixture.models {
            let model = model.lock().unwrap();
            for packet in model
                .requests
                .iter()
                .filter(|packet| packet.payload()[1] == 1)
            {
                assert_eq!(
                    packet.timestamp(),
                    marker.origin_utc_ms,
                    "all seven targets/polls must retain exact CONFIRM boundary"
                );
            }
        }
        assert!(
            !history.automatic_actions_inhibited(),
            "verified Reset ALL must rearm only after actual durable publication acknowledgement"
        );
        drop(history);
        let reopened = crate::launch_history::Store::open(
            fixture.http.directory.join("clock.json"),
            false,
            None,
            10000,
            0,
        )
        .unwrap();
        assert_eq!(reopened.restored_recovery_barrier(), (0, 2));
    }
    #[tokio::test]
    async fn per_board_recovery_does_not_clear_global_abort_and_ticket_is_one_use() {
        if isolated("per_board_recovery_does_not_clear_global_abort_and_ticket_is_one_use") {
            return;
        }
        let fixture = Fixture::new().await;
        *fixture.http.state.state.lock().unwrap() = crate::types::FlightState::Aborted;
        fixture.query(Board::ActuatorBoard).await;
        let ticket = fixture.prepare("recover_abort", Some("AB")).await;
        assert_eq!(fixture.confirm(&ticket).await.status(), StatusCode::OK);
        assert_eq!(fixture.terminal().await["success"], true);
        assert_eq!(
            fixture.confirm(&ticket).await.status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            fixture.http.state.local_flight_state_snapshot(),
            crate::types::FlightState::Aborted
        );
        for (board, model) in &fixture.models {
            assert_eq!(
                model.lock().unwrap().applied,
                usize::from(*board == Board::ActuatorBoard)
            );
        }
    }
    #[tokio::test]
    async fn virgin_first_leg_io_remains_durably_closed_without_automatic_second_intent() {
        if isolated("virgin_first_leg_io_remains_durably_closed_without_automatic_second_intent") {
            return;
        }
        let fixture = Fixture::new().await;
        {
            let mut m = fixture
                .models
                .iter()
                .find(|(b, _)| *b == Board::ActuatorBoard)
                .unwrap()
                .1
                .lock()
                .unwrap();
            m.boot = u64::MAX;
            m.result = 7;
        }
        fixture.query(Board::ActuatorBoard).await;
        let ticket = fixture.prepare("recover_abort", Some("AB")).await;
        assert!(
            ticket["confirmation"]
                .as_str()
                .unwrap()
                .ends_with("COMMISSION")
        );
        assert_eq!(fixture.confirm(&ticket).await.status(), StatusCode::OK);
        let op = fixture.terminal().await;
        assert_eq!(op["success"], false, "{op}");
        assert_ne!(
            fixture
                .http
                .state
                .operator_admission_barrier
                .load(Ordering::Acquire),
            0
        );
        let m = fixture
            .models
            .iter()
            .find(|(b, _)| *b == Board::ActuatorBoard)
            .unwrap()
            .1
            .lock()
            .unwrap();
        assert_eq!(m.boot, 1);
        assert_eq!(m.applied, 0);
        assert_eq!(m.requests.iter().filter(|p| p.payload()[1] == 1).count(), 1);
        drop(m);
        let reopened = crate::launch_history::Store::open(
            fixture.http.directory.join("clock.json"),
            false,
            None,
            10000,
            0,
        )
        .unwrap();
        assert_ne!(reopened.restored_recovery_barrier().0, 0);
        assert!(!reopened.recovery_pending().unwrap().published);
    }
    #[tokio::test]
    async fn real_routes_require_sendcommands_and_new_stop_invalidates_original_ticket() {
        if isolated("real_routes_require_sendcommands_and_new_stop_invalidates_original_ticket") {
            return;
        }
        let fixture = Fixture::new().await;
        for path in ["/api/board-recovery/prepare", "/api/board-recovery/confirm"] {
            assert_eq!(
                super::super::tests::http(fixture.state(), "POST", path, None, json!({}))
                    .await
                    .status(),
                StatusCode::UNAUTHORIZED
            );
            assert_eq!(
                super::super::tests::http(fixture.state(), "POST", path, Some("viewer"), json!({}))
                    .await
                    .status(),
                StatusCode::FORBIDDEN
            );
        }
        fixture.query(Board::ActuatorBoard).await;
        let ticket = fixture.prepare("recover_abort", Some("AB")).await;
        fixture.http.state.manual_abort_intent.request();
        assert_eq!(
            fixture.confirm(&ticket).await.status(),
            StatusCode::CONFLICT
        );
        assert!(
            fixture
                .http
                .state
                .board_recovery
                .coordinator
                .operation()
                .is_none()
        );
        for (_, model) in &fixture.models {
            assert_eq!(model.lock().unwrap().applied, 0);
        }
    }
    fn confirmation(ticket: &Value) -> Value {
        json!({"ticket":ticket["ticket"],"backend_instance":ticket["backend_instance"],"principal_context":ticket["principal_context"],"reset_epoch":ticket["reset_epoch"],"confirmation":ticket["confirmation"]})
    }
    #[tokio::test]
    async fn actual_http_cancellation_cannot_drop_owned_supervisor_or_bypass_pending_writer() {
        if isolated(
            "actual_http_cancellation_cannot_drop_owned_supervisor_or_bypass_pending_writer",
        ) {
            return;
        }
        let fixture = Fixture::new().await;
        fixture.query(Board::ActuatorBoard).await;
        let ticket = fixture.prepare("recover_abort", Some("AB")).await;
        let response = PauseGuard::install(&RESPONSE, &fixture.state());
        let writer = crate::web::clock_worker_tests::WriterGuard::install(
            fixture.http.directory.join("clock.json"),
        );
        let state = fixture.state();
        let value = confirmation(&ticket);
        let request = tokio::spawn(async move {
            super::super::tests::http(
                state,
                "POST",
                "/api/board-recovery/confirm",
                Some("operator"),
                value,
            )
            .await
        });
        response.entered().await;
        writer.entered().await;
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        assert_ne!(
            fixture
                .http
                .state
                .operator_admission_barrier
                .load(Ordering::Acquire),
            0
        );
        assert_eq!(
            fixture
                .http
                .state
                .board_recovery
                .coordinator
                .operation()
                .unwrap()["completed"],
            false
        );
        for (_, model) in &fixture.models {
            assert_eq!(model.lock().unwrap().applied, 0);
        }
        writer.release();
        response.release();
        let operation = fixture.terminal().await;
        assert_eq!(operation["success"], true, "{operation}");
    }
    #[tokio::test]
    async fn new_stop_at_actual_final_gate_cannot_be_recaptured_or_publish_success() {
        if isolated("new_stop_at_actual_final_gate_cannot_be_recaptured_or_publish_success") {
            return;
        }
        let fixture = Fixture::new().await;
        fixture.query(Board::ActuatorBoard).await;
        let ticket = fixture.prepare("recover_abort", Some("AB")).await;
        let final_gate = PauseGuard::install(&FINAL, &fixture.state());
        assert_eq!(fixture.confirm(&ticket).await.status(), StatusCode::OK);
        final_gate.entered().await;
        assert!(
            fixture
                .http
                .state
                .launch_history
                .lock()
                .unwrap()
                .recovery_pending()
                .unwrap()
                .verified
        );
        fixture.http.state.manual_abort_intent.request();
        final_gate.release();
        let operation = fixture.terminal().await;
        assert_eq!(operation["success"], false, "{operation}");
        assert_ne!(
            fixture
                .http
                .state
                .operator_admission_barrier
                .load(Ordering::Acquire),
            0
        );
        assert!(
            !fixture
                .http
                .state
                .launch_history
                .lock()
                .unwrap()
                .recovery_pending()
                .unwrap()
                .published
        );
    }
    #[tokio::test]
    async fn expired_preparation_query_requires_distinct_new_intent_without_auto_refresh() {
        if isolated("expired_preparation_query_requires_distinct_new_intent_without_auto_refresh") { return; }
        let fixture = Fixture::new().await;
        fixture.query(Board::ActuatorBoard).await;
        let ticket = fixture.prepare("recover_abort", Some("AB")).await;
        tokio::time::sleep(READBACK_TTL + Duration::from_millis(20)).await;
        let state = fixture.state();
        assert!(!state.board_recovery.inner.lock().unwrap().values[index(Board::ActuatorBoard).unwrap()].as_ref().unwrap().fresh(Stamp::now()));
        let original_offers = || fixture.models.iter().map(|(_, model)| {
            model.lock().unwrap().requests.iter().map(|packet|(packet.packet_id(), packet.timestamp())).collect::<Vec<_>>()
        }).collect::<Vec<_>>();
        let before = original_offers();
        let response = fixture.confirm(&ticket).await;
        let status = response.status();
        let rejected = body(response).await;
        eprintln!("EXPIRED_PREPARATION_CONFIRM status={status} body={rejected}");
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(rejected["error"], "preparation_query_expired");
        assert_eq!(original_offers(), before, "an expired confirmation cannot silently query, restamp, or offer recovery");
        assert!(state.launch_history.lock().unwrap().recovery_pending().is_none());
        assert!(fixture.models.iter().all(|(_, model)| model.lock().unwrap().applied == 0));
        // Fresh read-only metadata does not replace the ORIGINAL preparation
        // observation retained inside this ticket.
        fixture.query(Board::ActuatorBoard).await;
        let after_readonly = original_offers();
        let response = fixture.confirm(&ticket).await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(body(response).await["error"], "preparation_query_expired");
        assert_eq!(original_offers(), after_readonly);
        assert!(fixture.models.iter().all(|(_, model)| model.lock().unwrap().applied == 0));
        let fresh = fixture.prepare("recover_abort", Some("AB")).await;
        assert_ne!(fresh["ticket"], ticket["ticket"], "only a distinct explicit intent may bind a new preparation");
        assert_eq!(fixture.confirm(&fresh).await.status(), StatusCode::OK);
        let operation = fixture.terminal().await;
        assert_eq!(operation["success"], true, "{operation}");
        assert_eq!(fixture.models.iter().find(|(board,_)|*board==Board::ActuatorBoard).unwrap().1.lock().unwrap().applied, 1);
        assert!(fixture.models.iter().filter(|(board,_)|*board!=Board::ActuatorBoard).all(|(_,model)|model.lock().unwrap().applied==0));
    }
    #[tokio::test]
    async fn partial_board_failure_stays_closed_and_explicit_retry_joins_and_transfers_owner() {
        if isolated(
            "partial_board_failure_stays_closed_and_explicit_retry_joins_and_transfers_owner",
        ) {
            return;
        }
        let fixture = Fixture::new().await;
        fixture
            .models
            .iter()
            .find(|(b, _)| *b == Board::FlightComputer)
            .unwrap()
            .1
            .lock()
            .unwrap()
            .fail_result = Some(5);
        for board in BOARDS {
            fixture.query(board).await;
        }
        let first = fixture.prepare("reset_startup", None).await;
        assert_eq!(fixture.confirm(&first).await.status(), StatusCode::OK);
        let operation = fixture.terminal().await;
        assert_eq!(operation["success"], false, "{operation}");
        let old_owner = fixture
            .http
            .state
            .operator_admission_barrier
            .load(Ordering::Acquire);
        assert_ne!(old_owner, 0);
        assert!(
            !fixture
                .http
                .state
                .launch_history
                .lock()
                .unwrap()
                .recovery_pending()
                .unwrap()
                .published
        );
        fixture
            .models
            .iter()
            .find(|(b, _)| *b == Board::FlightComputer)
            .unwrap()
            .1
            .lock()
            .unwrap()
            .fail_result = None;
        for board in BOARDS {
            fixture.query(board).await;
        }
        let second = fixture.prepare("reset_startup", None).await;
        assert_ne!(first["ticket"], second["ticket"]);
        assert_eq!(fixture.confirm(&second).await.status(), StatusCode::OK);
        let operation = fixture.terminal().await;
        assert_eq!(operation["success"], true, "{operation}");
        let marker = fixture
            .http
            .state
            .launch_history
            .lock()
            .unwrap()
            .recovery_pending()
            .unwrap();
        assert_ne!(marker.owner, old_owner);
        assert!(marker.published);
        assert_eq!(
            fixture
                .http
                .state
                .operator_admission_barrier
                .load(Ordering::Acquire),
            0
        );
    }
    #[tokio::test]
    async fn current_permission_revocation_and_ticket_expiry_have_no_board_or_journal_effect() {
        if isolated(
            "current_permission_revocation_and_ticket_expiry_have_no_board_or_journal_effect",
        ) {
            return;
        }
        let fixture = Fixture::new().await;
        fixture.query(Board::ActuatorBoard).await;
        let ticket = fixture.prepare("recover_abort", Some("AB")).await;
        sqlx::query("UPDATE auth_sessions SET can_send_commands=0 WHERE token='operator'")
            .execute(&fixture.http.state.auth_db)
            .await
            .unwrap();
        assert_eq!(
            fixture.confirm(&ticket).await.status(),
            StatusCode::FORBIDDEN
        );
        assert!(
            fixture
                .http
                .state
                .launch_history
                .lock()
                .unwrap()
                .recovery_pending()
                .is_none()
        );
        sqlx::query("UPDATE auth_sessions SET can_send_commands=1 WHERE token='operator'")
            .execute(&fixture.http.state.auth_db)
            .await
            .unwrap();
        fixture
            .http
            .state
            .board_recovery
            .coordinator
            .0
            .lock()
            .unwrap()
            .ticket
            .as_mut()
            .unwrap()
            .created -= CONFIRM_TTL + Duration::from_millis(1);
        assert_eq!(
            fixture.confirm(&ticket).await.status(),
            StatusCode::CONFLICT
        );
        for (_, model) in &fixture.models {
            assert_eq!(model.lock().unwrap().applied, 0);
        }
    }

    #[tokio::test]
    async fn actual_sqlite_auth_wait_cannot_freshen_confirm_boundary_after_clock_window() {
        if isolated("actual_sqlite_auth_wait_cannot_freshen_confirm_boundary_after_clock_window") {
            return;
        }
        let fixture = Fixture::new().await;
        fixture.query(Board::ActuatorBoard).await;
        let ticket = fixture.prepare("recover_abort", Some("AB")).await;
        let connection = fixture.http.state.auth_db.acquire().await.unwrap();
        let state = fixture.state();
        let received = Arc::new(tokio::sync::Notify::new());
        *AUTH_RECEIVED.lock().unwrap() = Some((Arc::as_ptr(&state) as usize, received.clone()));
        let mut request = Box::pin(super::super::tests::http(
            state.clone(),
            "POST",
            "/api/board-recovery/confirm",
            Some("operator"),
            confirmation(&ticket),
        ));
        assert!(
            futures::poll!(request.as_mut()).is_pending(),
            "actual request must reach held SQLite auth acquire"
        );
        assert!(
            received.notified().now_or_never().is_some(),
            "request reached actual confirm ingress before auth wait"
        );
        // The real SDK disciplines/slews clock corrections; a setter is NOT a
        // substitute for elapsed source/monotonic time in this control.
        tokio::time::sleep(QUERY_TTL + Duration::from_millis(30)).await;
        drop(connection);
        let response = request.await;
        let status = response.status();
        // A legacy restamp control can actually complete; observe the effect
        // before asserting HTTP status so rejection is not just a DTO check.
        if status == StatusCode::OK {
            let _ = fixture.terminal().await;
        }
        for (_, model) in &fixture.models {
            assert_eq!(
                model.lock().unwrap().applied,
                0,
                "auth-delayed original intent must never gain fresh source time"
            );
        }
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(
            fixture
                .http
                .state
                .launch_history
                .lock()
                .unwrap()
                .recovery_pending()
                .is_none()
        );
    }
    async fn changed_review_identity_is_rejected_without_adopting_fresh_query(change_boot: bool) {
        let fixture = Fixture::new().await;
        fixture.query(Board::ActuatorBoard).await;
        let ticket = fixture.prepare("recover_abort", Some("AB")).await;
        let model = fixture.models.iter().find(|(board, _)| *board == Board::ActuatorBoard).unwrap().1.clone();
        let (reviewed_boot, reviewed_epoch, original_state) = {
            let mut model = model.lock().unwrap();
            let original = (model.boot, model.epoch, model.state);
            if change_boot { model.boot += 1; } else { model.epoch += 1; }
            original
        };
        assert_eq!(fixture.confirm(&ticket).await.status(), StatusCode::OK);
        let operation = fixture.terminal().await;
        assert_eq!(operation["success"], false, "{operation}");
        let state = fixture.state();
        let retained_owner = state.operator_admission_barrier.load(Ordering::Acquire);
        assert_ne!(retained_owner, 0);
        {
            let model = model.lock().unwrap();
            let mutations = model.requests.iter().filter(|packet| packet.payload()[1] == 1).collect::<Vec<_>>();
            assert!(!mutations.is_empty(), "actual SDK must reach the rejecting peer");
            for packet in mutations {
                let wire = packet.payload();
                assert_eq!(u64::from_le_bytes(wire[12..20].try_into().unwrap()), reviewed_boot);
                assert_eq!(u32::from_le_bytes(wire[20..24].try_into().unwrap()), reviewed_epoch);
                let nonce = u64::from_le_bytes(wire[4..12].try_into().unwrap());
                assert!(!model.seen.contains_key(&nonce), "rejected identity cannot seed a cached apply/replay");
            }
            assert_eq!(model.boot, reviewed_boot + u64::from(change_boot));
            assert_eq!(model.epoch, reviewed_epoch + u32::from(!change_boot));
            assert_eq!(model.state, original_state);
        }
        // A separately accepted fresh READ-ONLY query may describe the new
        // identity; it cannot repair or rebind the already failed frozen intent.
        fixture.query(Board::ActuatorBoard).await;
        assert_eq!(state.board_recovery.coordinator.operation().unwrap()["success"], false);
        assert_eq!(state.operator_admission_barrier.load(Ordering::Acquire), retained_owner);
        assert!(fixture.models.iter().all(|(_, model)| model.lock().unwrap().applied == 0));
    }
    #[tokio::test]
    async fn fresh_query_cannot_adopt_a_boot_epoch_changed_after_confirmation_context() {
        if isolated("fresh_query_cannot_adopt_a_boot_epoch_changed_after_confirmation_context") { return; }
        changed_review_identity_is_rejected_without_adopting_fresh_query(true).await;
    }
    #[tokio::test]
    async fn fresh_query_cannot_adopt_an_abort_epoch_changed_after_confirmation_context() {
        if isolated("fresh_query_cannot_adopt_an_abort_epoch_changed_after_confirmation_context") { return; }
        changed_review_identity_is_rejected_without_adopting_fresh_query(false).await;
    }
    #[tokio::test]
    async fn pending_restart_marker_is_validated_and_corrupt_identity_never_opens_fallback() {
        if isolated("pending_restart_marker_is_validated_and_corrupt_identity_never_opens_fallback")
        {
            return;
        }
        let fixture = Fixture::new().await;
        fixture
            .models
            .iter()
            .find(|(b, _)| *b == Board::ActuatorBoard)
            .unwrap()
            .1
            .lock()
            .unwrap()
            .fail_result = Some(5);
        fixture.query(Board::ActuatorBoard).await;
        let ticket = fixture.prepare("recover_abort", Some("AB")).await;
        assert_eq!(fixture.confirm(&ticket).await.status(), StatusCode::OK);
        assert_eq!(fixture.terminal().await["success"], false);
        let path = fixture.http.directory.join("clock.json");
        let restored =
            crate::launch_history::Store::open(path.clone(), false, None, 10000, 0).unwrap();
        let (owner, generation) = restored.restored_recovery_barrier();
        assert_ne!(owner, 0);
        assert_eq!(generation, 2);
        let restored_state = crate::state::tests::test_app_state().await;
        restored_state
            .operator_admission_barrier
            .store(owner, Ordering::Release);
        restored_state
            .automatic_generation
            .store(generation, Ordering::Release);
        *restored_state.launch_history.lock().unwrap() = restored;
        assert!(
            crate::gse::capture_operator_origin(&restored_state).is_none(),
            "same startup-loaded barrier blocks producers"
        );
        let mut bytes: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        bytes["recovery_pending"]["generation"] = json!(0);
        let corrupt = serde_json::to_vec(&bytes).unwrap();
        std::fs::write(&path, &corrupt).unwrap();
        assert!(crate::launch_history::Store::open(path.clone(), false, None, 10000, 0).is_err());
        assert_eq!(
            std::fs::read(path).unwrap(),
            corrupt,
            "invalid critical journal must never be replaced with defaults"
        );
    }
    #[tokio::test]
    async fn real_status_exposes_same_principal_immutable_ticket_without_stale_capabilities() {
        if isolated(
            "real_status_exposes_same_principal_immutable_ticket_without_stale_capabilities",
        ) {
            return;
        }
        let fixture = Fixture::new().await;
        fixture.query(Board::ActuatorBoard).await;
        let ticket = fixture.prepare("recover_abort", Some("AB")).await;
        let get = |state, token| {
            super::super::tests::http(
                state,
                "GET",
                "/api/board-recovery/status",
                Some(token),
                Value::Null,
            )
        };
        let current = body(get(fixture.state(), "operator").await).await;
        assert_eq!(current["confirmation"]["ticket"], ticket["ticket"]);
        let ab = current["boards"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == "AB")
            .unwrap();
        assert_eq!(ab["capabilities"]["recover"], true);
        assert_eq!(ab["capabilities"]["resync"], false);
        let viewer = body(get(fixture.state(), "viewer").await).await;
        assert!(viewer["confirmation"].is_null());
        assert_eq!(viewer["permissions"]["recover"], false);
        tokio::time::sleep(READBACK_TTL + Duration::from_millis(20)).await;
        let stale = body(get(fixture.state(), "operator").await).await;
        assert!(stale["confirmation"].is_null(), "expired original preparation is not an actionable confirmation");
        let ab = stale["boards"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == "AB")
            .unwrap();
        assert_eq!(
            ab["capabilities"]["recover"], false,
            "stale transient proof is not advertised fresh"
        );
    }

    #[tokio::test]
    async fn actual_environment_refuses_corrupt_critical_journal_without_replacing_it() {
        if isolated("actual_environment_refuses_corrupt_critical_journal_without_replacing_it") {
            return;
        }
        let path = std::path::PathBuf::from(std::env::var_os("GS_LAUNCH_CLOCK_STATE").unwrap());
        let corrupt = b"{broken recovery journal";
        std::fs::write(&path, corrupt).unwrap();
        let store = crate::launch_history::Store::environment().unwrap();
        assert_eq!(store.restored_recovery_barrier(), (u64::MAX, 1));
        assert!(!store.recovery_available());
        assert!(store.automatic_actions_inhibited());
        assert_eq!(std::fs::read(&path).unwrap(), corrupt);
        let fixture = Fixture::new().await;
        let state = fixture.state();
        *state.launch_history.lock().unwrap() = store;
        state
            .operator_admission_barrier
            .store(u64::MAX, Ordering::Release);
        assert!(crate::gse::capture_operator_origin(&state).is_none());
        let router = state.topology_router.get().unwrap();
        crate::network_variables::initialize_for_state(&state, router).unwrap();
        assert!(!crate::network_variables::publish_bootstrap_for_state(&state, router).unwrap());
        for name in [
            "FLIGHT_STATE",
            "DAQ_LOG_CLOCK",
            "AV_BAY_UNDERGLOW",
            "FLIGHT_BUZZER",
        ] {
            assert!(
                router
                    .get_cached_network_variable(crate::telemetry_schema::data_type(name))
                    .unwrap()
                    .is_none(),
                "closed startup cannot seed/serve synthesized defaults: {name}"
            );
        }
        let before = router
            .get_cached_network_variable(crate::telemetry_schema::data_type("DAQ_LOG_CLOCK"))
            .unwrap()
            .map(|packet| (packet.timestamp(), packet.payload().to_vec()));
        assert!(
            state.try_reset_launch_clock().is_err(),
            "actual mutating clock entrypoint must refuse closed startup"
        );
        state
            .cmd_tx
            .send(crate::types::TelemetryCommand::QueuedOperator {
            recovery_origin: None,
                launch_revision: None,
                generation: crate::gse::automatic_generation(&state),
                command: Box::new(crate::types::TelemetryCommand::ToggleFlightBuzzer),
                plumbing_origin: None,
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            !state
                .last_command_ms
                .lock()
                .unwrap()
                .contains_key("ToggleFlightBuzzer")
        );
        let after = router
            .get_cached_network_variable(crate::telemetry_schema::data_type("DAQ_LOG_CLOCK"))
            .unwrap()
            .map(|packet| (packet.timestamp(), packet.payload().to_vec()));
        assert_eq!(
            before, after,
            "synthesized fallback cannot publish managed clock data"
        );
        assert!(state.manual_abort_intent.pending_epoch().is_none());
        let epoch = state.manual_abort_intent.request();
        assert_eq!(state.manual_abort_intent.pending_epoch(), Some(epoch));
        assert_eq!(
            std::fs::read(path).unwrap(),
            corrupt,
            "manual Stop never formats unreadable recovery journal"
        );
    }

    #[tokio::test]
    async fn retained_first_uncertainty_reconciles_only_identity_then_requires_third_human_intent()
    {
        if isolated(
            "retained_first_uncertainty_reconciles_only_identity_then_requires_third_human_intent",
        ) {
            return;
        }
        let fixture = Fixture::new().await;
        {
            let mut m = fixture
                .models
                .iter()
                .find(|(b, _)| *b == Board::ActuatorBoard)
                .unwrap()
                .1
                .lock()
                .unwrap();
            m.boot = u64::MAX;
            m.result = 7;
            m.fail_first_readback = true;
        }
        for board in BOARDS {
            fixture.query(board).await;
        }
        let first = fixture.prepare("reset_startup", None).await;
        assert_eq!(fixture.confirm(&first).await.status(), StatusCode::OK);
        assert_eq!(fixture.terminal().await["success"], false);
        let marker = fixture
            .http
            .state
            .launch_history
            .lock()
            .unwrap()
            .recovery_pending()
            .unwrap();
        let first_target = marker
            .targets
            .iter()
            .find(|target| target.board == "AB")
            .unwrap();
        let original = first_target.original_first.clone().unwrap();
        assert_eq!(original, first_target.intent);
        let counts: Vec<_> = fixture
            .models
            .iter()
            .map(|(_, m)| m.lock().unwrap().applied)
            .collect();
        for board in BOARDS {
            fixture.query(board).await;
        }
        let status = body(
            super::super::tests::http(
                fixture.state(),
                "GET",
                "/api/board-recovery/status",
                Some("operator"),
                Value::Null,
            )
            .await,
        )
        .await;
        let ab = status["boards"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == "AB")
            .unwrap();
        assert_eq!(ab["boot"], "0000000000000000");
        assert_eq!(ab["identity_kind"], "retained_first_uncertainty");
        assert_eq!(ab["first_uncertainty_reconcilable"], true);
        assert_eq!(ab["capabilities"]["reset_startup"], true);
        let second = fixture.prepare("reset_startup", None).await;
        assert!(
            second["summary"]
                .as_str()
                .unwrap()
                .contains("Identity reconciliation ONLY")
        );
        assert_eq!(fixture.confirm(&second).await.status(), StatusCode::OK);
        let op = fixture.terminal().await;
        assert_eq!(op["success"], false, "{op}");
        assert_eq!(
            fixture
                .models
                .iter()
                .map(|(_, m)| m.lock().unwrap().applied)
                .collect::<Vec<_>>(),
            counts,
            "no normal target may reset/apply in identity-only attempt"
        );
        let marker = fixture
            .http
            .state
            .launch_history
            .lock()
            .unwrap()
            .recovery_pending()
            .unwrap();
        let target = marker.targets.iter().find(|t| t.board == "AB").unwrap();
        assert_eq!(target.original_first.as_ref(), Some(&original));
        assert_eq!(target.uncertain_first.as_ref(), Some(&original));
        assert_eq!(target.intent.boot(), u64::MAX);
        assert_ne!(target.intent.nonce(), original.nonce());
        assert!(target.intent.source_utc_ms > original.source_utc_ms);
        assert!(!marker.published);
        for board in BOARDS {
            fixture.query(board).await;
        }
        let third = fixture.prepare("reset_startup", None).await;
        assert_eq!(fixture.confirm(&third).await.status(), StatusCode::OK);
        let op = fixture.terminal().await;
        assert_eq!(op["success"], true, "{op}");
        let marker = fixture
            .http
            .state
            .launch_history
            .lock()
            .unwrap()
            .recovery_pending()
            .unwrap();
        let target = marker.targets.iter().find(|t| t.board == "AB").unwrap();
        assert_eq!(target.intent.boot(), 1);
        assert_eq!(target.original_first.as_ref(), Some(&original));
        assert_eq!(target.uncertain_first.as_ref(), Some(&original));
        assert!(marker.published);
        let model = fixture
            .models
            .iter()
            .find(|(b, _)| *b == Board::ActuatorBoard)
            .unwrap()
            .1
            .lock()
            .unwrap();
        assert_eq!(model.applied, 1);
        let failed_nonce = original.nonce();
        let cached = model.seen.get(&failed_nonce).unwrap();
        assert_eq!(
            u64::from_le_bytes(cached[12..20].try_into().unwrap()),
            0,
            "failed FIRST tuple remains readonly retired, never rewritten as current success"
        );
    }
    #[tokio::test]
    async fn unsealed_supported_io_generation_zero_never_becomes_virgin_or_recovery_authority() {
        if isolated(
            "unsealed_supported_io_generation_zero_never_becomes_virgin_or_recovery_authority",
        ) {
            return;
        }
        let fixture = Fixture::new().await;
        {
            let mut m = fixture
                .models
                .iter()
                .find(|(b, _)| *b == Board::ActuatorBoard)
                .unwrap()
                .1
                .lock()
                .unwrap();
            m.boot = 0;
            m.result = 7;
        }
        fixture.query(Board::ActuatorBoard).await;
        let state = fixture.state();
        let status = body(
            super::super::tests::http(
                state.clone(),
                "GET",
                "/api/board-recovery/status",
                Some("operator"),
                Value::Null,
            )
            .await,
        )
        .await;
        let ab = status["boards"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == "AB")
            .unwrap();
        assert_eq!(ab["first_uncertainty_reconcilable"], false);
        assert_eq!(ab["capabilities"]["recover"], false);
        let response=super::super::tests::http(state.clone(),"POST","/api/board-recovery/prepare",Some("operator"),json!({"action":"recover_abort","board":"AB","mode":null,"backend_instance":hex(&state.board_recovery.identity.unwrap()),"reset_epoch":format!("{:016x}",crate::gse::automatic_generation(&state))})).await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(
            state
                .launch_history
                .lock()
                .unwrap()
                .recovery_pending()
                .is_none()
        );
        assert!(fixture.models.iter().all(|(_, model)| {
            model
                .lock()
                .unwrap()
                .requests
                .iter()
                .all(|packet| packet.payload()[1] == 0)
        }));
    }
    #[tokio::test]
    async fn actual_marker_write_failure_has_zero_sdk_recovery_effects_and_keeps_closed() {
        if isolated("actual_marker_write_failure_has_zero_sdk_recovery_effects_and_keeps_closed") {
            return;
        }
        let fixture = Fixture::new().await;
        fixture.query(Board::ActuatorBoard).await;
        let ticket = fixture.prepare("recover_abort", Some("AB")).await;
        std::fs::create_dir(fixture.http.directory.join("clock.json")).unwrap();
        assert_eq!(fixture.confirm(&ticket).await.status(), StatusCode::OK);
        let operation = fixture.terminal().await;
        assert_eq!(operation["success"], false, "{operation}");
        assert_ne!(
            fixture
                .http
                .state
                .operator_admission_barrier
                .load(Ordering::Acquire),
            0
        );
        assert!(
            fixture.models.iter().all(|(_, model)| model
                .lock()
                .unwrap()
                .requests
                .iter()
                .all(|packet| packet.payload()[1] == 0)),
            "no board mutation may precede durable marker verification"
        );
    }

    #[tokio::test]
    async fn actual_proxy_lease_blocks_mutation_and_old_capability_cannot_reopen_after_actor() {
        if isolated(
            "actual_proxy_lease_blocks_mutation_and_old_capability_cannot_reopen_after_actor",
        ) {
            return;
        }
        let fixture = Fixture::new().await;
        fixture.query(Board::ActuatorBoard).await;
        let proxy = crate::command_proxy::recovery::TestPrimaryAdmission::register();
        let old_boot = proxy.boot();
        let lease = proxy.admit(old_boot).unwrap();
        let ticket = fixture.prepare("recover_abort", Some("AB")).await;
        assert_eq!(fixture.confirm(&ticket).await.status(), StatusCode::OK);
        tokio::time::timeout(Duration::from_secs(2), async {
            while !proxy.closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(proxy.admit(old_boot).is_err());
        assert_ne!(proxy.boot(), old_boot);
        assert!(
            fixture.models.iter().all(|(_, model)| model
                .lock()
                .unwrap()
                .requests
                .iter()
                .all(|packet| packet.payload()[1] == 0)),
            "no mutation while actual pre-recovery proxy lease lives"
        );
        drop(lease);
        let operation = fixture.terminal().await;
        assert_eq!(operation["success"], true, "{operation}");
        assert!(!proxy.closed());
        assert!(proxy.admit(old_boot).is_err());
        drop(proxy.admit(proxy.boot()).unwrap());
    }
    #[tokio::test]
    async fn actual_operator_claim_quiesces_before_receiver_and_durable_board_intent() {
        if isolated("actual_operator_claim_quiesces_before_receiver_and_durable_board_intent") {
            return;
        }
        let fixture = Fixture::new().await;
        fixture.query(Board::ActuatorBoard).await;
        let state = fixture.state();
        let generation = crate::gse::automatic_generation(&state);
        let claim = crate::gse::claim_operator_dispatch(&state, generation).unwrap();
        let ticket = fixture.prepare("recover_abort", Some("AB")).await;
        assert_eq!(fixture.confirm(&ticket).await.status(), StatusCode::OK);
        tokio::time::timeout(Duration::from_secs(2), async {
            while state.operator_admission_barrier.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 1);
        assert!(
            state
                .launch_history
                .lock()
                .unwrap()
                .recovery_pending()
                .is_none()
        );
        assert!(fixture.models.iter().all(|(_, model)| {
            model
                .lock()
                .unwrap()
                .requests
                .iter()
                .all(|packet| packet.payload()[1] == 0)
        }));
        drop(claim);
        let operation = fixture.terminal().await;
        assert_eq!(operation["success"], true, "{operation}");
    }

    #[tokio::test]
    async fn metadata_only_first_uncertainty_keeps_unknown_state_and_no_physical_proof() {
        if isolated("metadata_only_first_uncertainty_keeps_unknown_state_and_no_physical_proof") {
            return;
        }
        let fixture = Fixture::new().await;
        {
            let mut model = fixture
                .models
                .iter()
                .find(|(board, _)| *board == Board::RFBoard)
                .unwrap()
                .1
                .lock()
                .unwrap();
            model.boot = u64::MAX;
            model.result = 7;
            model.state = 255;
            model.fail_first_readback = true;
        }
        for board in BOARDS {
            fixture.query(board).await;
        }
        let first = fixture.prepare("reset_startup", None).await;
        assert_eq!(fixture.confirm(&first).await.status(), StatusCode::OK);
        assert_eq!(fixture.terminal().await["success"], false);
        for board in BOARDS {
            fixture.query(board).await;
        }
        let status = body(
            super::super::tests::http(
                fixture.state(),
                "GET",
                "/api/board-recovery/status",
                Some("operator"),
                Value::Null,
            )
            .await,
        )
        .await;
        let rf = status["boards"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == "RF")
            .unwrap();
        assert!(rf["state"].is_null());
        assert!(rf["safe_outputs"].is_null());
        assert!(rf["commanded_outputs"].is_null());
        assert_eq!(rf["physical_position_verified"], false);
        assert_eq!(rf["first_uncertainty_reconcilable"], true);
        // Supported targeted metadata Startup is explicit; it certifies no
        // physical state or successful reconciliation of this uncertain board.
        assert_eq!(rf["capabilities"]["recover"], true);
        assert_eq!(rf["capabilities"]["reset_startup"], true);
        let second = fixture.prepare("reset_startup", None).await;
        assert_eq!(fixture.confirm(&second).await.status(), StatusCode::OK);
        assert_eq!(fixture.terminal().await["success"], false);
        for (_, model) in &fixture.models {
            assert_eq!(
                model.lock().unwrap().applied,
                0,
                "metadata reconciliation must not apply any normal/physical target"
            );
        }
        let rf = fixture
            .models
            .iter()
            .find(|(board, _)| *board == Board::RFBoard)
            .unwrap()
            .1
            .lock()
            .unwrap();
        assert_eq!((rf.boot, rf.state), (1, 255));
        assert_ne!(
            fixture
                .http
                .state
                .operator_admission_barrier
                .load(Ordering::Acquire),
            0
        );
    }
    #[tokio::test]
    async fn actual_vb_preflight_rejects_false_safe_mask_zero_and_preserves_real_safe_six() {
        if isolated("actual_vb_preflight_rejects_false_safe_mask_zero_and_preserves_real_safe_six")
        {
            return;
        }
        let fixture = Fixture::new().await;
        fixture
            .models
            .iter()
            .find(|(board, _)| *board == Board::ValveBoard)
            .unwrap()
            .1
            .lock()
            .unwrap()
            .output_override = Some(0);
        fixture.query(Board::ValveBoard).await;
        let state = fixture.state();
        let response=super::super::tests::http(state.clone(),"POST","/api/board-recovery/prepare",Some("operator"),json!({"action":"recover_abort","board":"VB","mode":null,"backend_instance":hex(&state.board_recovery.identity.unwrap()),"reset_epoch":format!("{:016x}",crate::gse::automatic_generation(&state))})).await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(fixture.models.iter().all(|(_, model)| {
            model
                .lock()
                .unwrap()
                .requests
                .iter()
                .all(|packet| packet.payload()[1] == 0)
        }));
        fixture
            .models
            .iter()
            .find(|(board, _)| *board == Board::ValveBoard)
            .unwrap()
            .1
            .lock()
            .unwrap()
            .output_override = None;
        fixture.query(Board::ValveBoard).await;
        let ticket = fixture.prepare("recover_abort", Some("VB")).await;
        assert_eq!(fixture.confirm(&ticket).await.status(), StatusCode::OK);
        let operation = fixture.terminal().await;
        assert_eq!(operation["success"], true, "{operation}");
        let row = state.board_recovery.inner.lock().unwrap().values
            [index(Board::ValveBoard).unwrap()]
        .as_ref()
        .unwrap()
        .value
        .clone();
        assert_eq!(row.outputs, 6);
        assert!(row.flags & 2 != 0);
    }
    #[tokio::test]
    async fn real_journal_restart_starts_closed_and_fresh_backend_can_only_explicitly_reconcile() {
        if isolated(
            "real_journal_restart_starts_closed_and_fresh_backend_can_only_explicitly_reconcile",
        ) {
            return;
        }
        let first = Fixture::new().await;
        {
            let mut model = first
                .models
                .iter()
                .find(|(board, _)| *board == Board::ActuatorBoard)
                .unwrap()
                .1
                .lock()
                .unwrap();
            model.boot = u64::MAX;
            model.result = 7;
            model.fail_first_readback = true;
        }
        first.query(Board::ActuatorBoard).await;
        let old_ticket = first.prepare("recover_abort", Some("AB")).await;
        assert_eq!(first.confirm(&old_ticket).await.status(), StatusCode::OK);
        assert_eq!(first.terminal().await["success"], false);
        let journal = std::fs::read(first.http.directory.join("clock.json")).unwrap();
        let old_marker = first
            .http
            .state
            .launch_history
            .lock()
            .unwrap()
            .recovery_pending()
            .unwrap();
        let original = old_marker.targets[0].original_first.clone().unwrap();
        let retained = first.models.clone();
        let counts: Vec<_> = retained
            .iter()
            .map(|(_, model)| model.lock().unwrap().requests.len())
            .collect();
        drop(first);
        tokio::task::yield_now().await;
        // Retain the same board application owner/failed tuple; reconnect its
        // in-memory SDK wire to a fresh host process-style state/auth/runtime.
        let restarted = Fixture::from_saved(Some(journal), Some(retained)).await;
        assert_eq!(
            restarted
                .http
                .state
                .operator_admission_barrier
                .load(Ordering::Acquire),
            old_marker.owner
        );
        assert!(crate::gse::capture_operator_origin(&restarted.http.state).is_none());
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            restarted
                .models
                .iter()
                .map(|(_, model)| model.lock().unwrap().requests.len())
                .collect::<Vec<_>>(),
            counts,
            "restart cannot autoquery/apply/replay pending intents"
        );
        assert_eq!(
            restarted.confirm(&old_ticket).await.status(),
            StatusCode::CONFLICT
        );
        restarted.query(Board::ActuatorBoard).await;
        let fresh = restarted.prepare("recover_abort", Some("AB")).await;
        assert_ne!(fresh["backend_instance"], old_ticket["backend_instance"]);
        assert_ne!(fresh["principal_context"], old_ticket["principal_context"]);
        assert_eq!(restarted.confirm(&fresh).await.status(), StatusCode::OK);
        let operation = restarted.terminal().await;
        assert_eq!(
            operation["success"], false,
            "identity reconciliation is never stateapply: {operation}"
        );
        let marker = restarted
            .http
            .state
            .launch_history
            .lock()
            .unwrap()
            .recovery_pending()
            .unwrap();
        assert_eq!(marker.targets[0].original_first.as_ref(), Some(&original));
        assert_eq!(marker.targets[0].intent.boot(), u64::MAX);
        assert_ne!(marker.backend, old_marker.backend);
        assert!(marker.origin_utc_ms > old_marker.origin_utc_ms);
        let model = restarted
            .models
            .iter()
            .find(|(board, _)| *board == Board::ActuatorBoard)
            .unwrap()
            .1
            .lock()
            .unwrap();
        assert_eq!(model.boot, 1);
        assert_eq!(model.applied, 0);
    }
    #[cfg(feature = "test_fire_mode")]
    include!("pending_next_epoch_coordinator_tests.rs");

    #[tokio::test]
    async fn prepare_waits_for_background_query_then_uses_new_nonce_without_recovery() {
        let fixture = Fixture::new().await;
        let state = fixture.state();
        fixture.ignition_wire_hold.store(1, Ordering::Release);
        tokio::time::timeout(Duration::from_secs(1), async {
            while fixture.ignition_wire_hold.load(Ordering::Acquire) != 2 {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            super::super::request_board_query(&state, Board::ActuatorBoard).status(),
            StatusCode::OK
        );
        let at = index(Board::ActuatorBoard).unwrap();
        let old = state.board_recovery.inner.lock().unwrap().queries[at]
            .as_ref()
            .unwrap()
            .nonce;
        assert!(
            !state.board_recovery.inner.lock().unwrap().queries[at]
                .as_ref()
                .unwrap()
                .received
        );
        let release = fixture.ignition_wire_hold.clone();
        let released = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            release.store(0, Ordering::Release);
        });
        let ticket = fixture.prepare("recover_abort", Some("AB")).await;
        released.await.unwrap();
        assert!(ticket["ticket"].is_string());
        let inner = state.board_recovery.inner.lock().unwrap();
        assert_ne!(inner.queries[at].as_ref().unwrap().nonce, old);
        assert!(inner.queries[at].as_ref().unwrap().received);
        drop(inner);
        let model = fixture
            .models
            .iter()
            .find(|(board, _)| *board == Board::ActuatorBoard)
            .unwrap()
            .1
            .lock()
            .unwrap();
        assert_eq!(model.applied, 0);
        assert!(model.requests.iter().all(|packet| packet.payload()[1] == 0));
        assert!(
            model
                .requests
                .iter()
                .filter(|packet| packet.payload()[1] == 0)
                .count()
                >= 2
        );
    }

    #[tokio::test]
    async fn expired_confirmation_resumes_readonly_queries_without_reopening_manual_utc() {
        let fixture = Fixture::new().await;
        let state = fixture.state();
        let _ticket = fixture.prepare("recover_abort", Some("AB")).await;
        assert!(!manual_utc_idle(&state));
        assert!(!readonly_query_idle(&state));
        {
            let mut control = state.board_recovery.coordinator.0.lock().unwrap();
            control.ticket.as_mut().unwrap().created =
                WallInstant::now() - CONFIRM_TTL - Duration::from_millis(1);
        }
        assert!(!manual_utc_idle(&state));
        assert!(readonly_query_idle(&state));
        let now = Stamp::now();
        let at = index(Board::ActuatorBoard).unwrap();
        let old = {
            let mut inner = state.board_recovery.inner.lock().unwrap();
            inner.values[at].as_mut().unwrap().observed_after =
                now - QUERY_TTL - Duration::from_millis(1);
            let query = inner.queries[at].as_mut().unwrap();
            query.sent = now - QUERY_TTL - Duration::from_millis(1);
            query.nonce
        };
        state
            .network_health
            .lock()
            .unwrap()
            .observe("AB", now.wall, 100);
        let mut refresh = super::super::ReadonlyQueryRefresh::default();
        assert_eq!(refresh.poll(&state), Some(Board::ActuatorBoard));
        assert_ne!(
            state.board_recovery.inner.lock().unwrap().queries[at]
                .as_ref()
                .unwrap()
                .nonce,
            old
        );
        assert!(!manual_utc_idle(&state));
        {
            let mut control = state.board_recovery.coordinator.0.lock().unwrap();
            control.operation = Some(json!({"completed":false}));
        }
        assert!(!readonly_query_idle(&state));
        state.board_recovery.coordinator.0.lock().unwrap().operation = None;
        assert!(
            fixture
                .models
                .iter()
                .all(|(_, model)| model.lock().unwrap().applied == 0)
        );
    }

    #[tokio::test]
    async fn fixture_query_warmup_never_retries_confirmed_owner_409() {
        if crate::operator_queue::isolated_native_case(module_path!(), "fixture_query_warmup_never_retries_confirmed_owner_409") { return; }
        let fixture = Fixture::new().await;
        let state = fixture.state();
        // Explicit coordinator-state scaffolding: exercise the real external
        // HTTP owner gate, not a synthetic successful SDK/Recovery proof.
        state.board_recovery.coordinator.0.lock().unwrap().operation = Some(json!({"completed":false}));
        let attempts = std::cell::Cell::new(0usize);
        let origin = Token::capture(&state, 0);
        let result = admit_prepare_query(
            || origin_current(&state, origin).then_some(()).ok_or("origin changed"),
            || {
                attempts.set(attempts.get()+1);
                let state = state.clone();
                async move {
                    let response = super::super::tests::http(state, "POST", "/api/board-recovery/query", Some("operator"), json!({"board":"AB"})).await;
                    assert_eq!(response.status(), StatusCode::CONFLICT);
                    assert!(response.extensions().get::<super::super::QueryAdmissionCoreBusy>().is_none());
                    response
                }
            },
        ).await;
        assert!(result.is_err());
        assert_eq!(attempts.get(), 1, "An external owner refusal must not become a repeated query");
        assert_eq!(state.board_recovery.coordinator.0.lock().unwrap().operation.as_ref().unwrap()["completed"], false);
    }

    mod readonly_busy {
        use super::*;
        include!("readonly_busy_tests.rs");
    }

}
