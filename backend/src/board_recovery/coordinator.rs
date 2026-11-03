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
}
/// Constructed only from the dedicated final QUERY path, never from an SDK
/// acknowledgement or UI-supplied values. Validation runs inside final gate.
pub(crate) struct FinalProofs {
    owner: u64,
    generation: u64,
    rows: Vec<(Board, Observation)>,
}
impl FinalProofs {
    pub(crate) fn valid(&self, pending: &RecoveryPending, generation: u64) -> bool {
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
                            observation.fresh(Stamp::now())
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
fn context_same(a: &Readback, b: &Readback) -> bool {
    // A dedicated fresh QUERY necessarily has a different nonce and uptime.
    a.boot == b.boot
        && a.epoch == b.epoch
        && a.state == b.state
        && a.result == b.result
        && a.flags == b.flags
        && a.outputs == b.outputs
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
        let remaining = CONFIRM_TTL.checked_sub(ticket.created.elapsed())?;
        if ticket.principal != principal
            || !origin_current(state, ticket.token)
            || ticket.mode != crate::station_mode::local_mode()
            || !host_safe(state)
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
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::SendCommands).await {
            Ok(value) => value,
            Err(response) => return response,
        };
    if let Err(response) = primary_only() {
        return response;
    }
    let request: Prepare = match serde_json::from_value(body) {
        Ok(value) => value,
        Err(_) => {
            return error(
                StatusCode::CONFLICT,
                "invalid_intent",
                "A complete explicit recovery intent is required.",
            );
        }
    };
    if let Err(message) = crate::manual_abort_utc::before_prepare(&state).await {
        return error(StatusCode::CONFLICT, "manual_utc_pending", message);
    }
    // Ceiling waiting can consume the two-second observation window. Refresh
    // dedicated queries now, under the same explicit request's original origin.
    if let Err(message) = refresh_prepare_queries(&state, &headers, &request, token).await {
        return error(StatusCode::CONFLICT, "fresh_query_required", message);
    }
    let result = prepare_inner(&state, &headers, &principal, request, token);
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
) -> Result<(), &'static str> {
    let boards = match request.action.as_str() {
        "reset_startup" if request.board.is_none() => BOARDS.to_vec(),
        "recover_abort" => vec![
            Board::from_sender_id(request.board.as_deref().ok_or("Select AB or VB")?)
                .filter(|b| matches!(b, Board::ActuatorBoard | Board::ValveBoard))
                .ok_or("Unsupported board")?,
        ],
        _ => return Err("Unsupported recovery action"),
    };
    for board in boards {
        if !origin_current(state, token) {
            return Err("Recovery origin changed while waiting");
        }
        let response = super::query(
            State(state.clone()),
            headers.clone(),
            Json(QueryRequest {
                board: board.sender_id().into(),
            }),
        )
        .await;
        if response.status() != StatusCode::OK {
            return Err("Fresh dedicated QUERY could not be admitted; Query and prepare again");
        }
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
fn prepare_inner(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    principal: &crate::auth::AuthPrincipal,
    request: Prepare,
    token: Token,
) -> Result<Value, &'static str> {
    let identity = state
        .board_recovery
        .identity
        .ok_or("Backend identity unavailable")?;
    if request.backend_instance != hex(&identity)
        || request.reset_epoch != format!("{:016x}", token.generation)
        || !origin_current(state, token)
        || !host_safe(state)
        || state.board_recovery.coordinator.busy()
        || principal.anonymous
        || request.mode.is_some()
    {
        return Err("Recovery origin, current primary/session or safe host state changed");
    }
    let router = state.topology_router.get().ok_or("Router unavailable")?;
    crate::system_clock::required_control_network_utc_ms(router)
        .map_err(|_| "Network UTC unavailable")?;
    let boards = match request.action.as_str() {
        "reset_startup" if request.board.is_none() => BOARDS.to_vec(),
        "recover_abort" => vec![
            Board::from_sender_id(request.board.as_deref().ok_or("Select AB or VB")?)
                .filter(|b| matches!(b, Board::ActuatorBoard | Board::ValveBoard))
                .ok_or("Per-board recovery is supported only for AB/VB")?,
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
        let desired = if request.action != "reset_startup"
            && matches!(board, Board::ActuatorBoard | Board::ValveBoard)
        {
            1
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
        if request.action == "reset_startup" {
            "RESET STARTUP"
        } else {
            "RECOVER"
        },
        names.join(" "),
        if commissioning { " COMMISSION" } else { "" }
    );
    let context = principal_context(&identity, headers, principal);
    let value = json!({"ticket":id,"backend_instance":hex(&identity),"principal_context":context,"reset_epoch":format!("{:016x}",token.generation),"expires_in_ms":30000,"action":request.action,"targets":names,"summary":if targets.iter().any(|target| target.first_binding.is_some()) {"Identity reconciliation ONLY for retained uncertain FIRST targets. Other targets will not reset/apply in this attempt. IO cannot clear state; a fresh Query and another distinct human confirmation are required."}else if commissioning{"Explicit commissioning leg; IO is not completion. A separate fresh confirmation is required after provisioning."}else{"Recover only selected application state; no power, sensor, physical position or runtime mode reset."},"confirmation":phrase});
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
        || ticket.mode != crate::station_mode::local_mode()
        || !host_safe(&state)
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
    let operation = json!({"id":ticket.id,"action":ticket.action,"phase":"quiescing","targets":ticket.targets.iter().map(|t|t.board.sender_id()).collect::<Vec<_>>(),"completed":false,"success":null,"age_ms":0,"reset_applied_state":null,"message":"Owned coordinator admitted; actual board completion pending."});
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
                json!(if result.is_ok() && ticket.action == "reset_startup" {
                    Some(0u8)
                } else {
                    None
                });
            operation["current_host_state"] = json!(owned.local_flight_state_snapshot() as u8);
            operation["age_ms"] = json!(ticket.created.elapsed().as_millis() as u64);
            operation["message"] = json!(match result {
                Ok(()) =>
                    "Actual application terminal completion and fresh final readbacks verified. Physical position is not certified.",
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
        || !host_safe(state)
    {
        return Err("Recovery principal/mode/safe host state changed");
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
        ||exchange.packet(Stamp::now()).map(|_|())).await?;
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
                    return Err("Board rejected recovery; admission remains closed");
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
fn sdk_pending(state: &AppState, router: &sedsnet::router::Router) -> bool {
    static TYPES: std::sync::OnceLock<Vec<sedsnet::config::DataType>> = std::sync::OnceLock::new();
    let types = TYPES.get_or_init(|| {
        let schema: Value =
            serde_json::from_slice(include_bytes!("../../config/telemetry_config.json"))
                .expect("compiled telemetry schema");
        schema["types"]
            .as_array()
            .expect("compiled type array")
            .iter()
            .map(|row| {
                crate::telemetry_schema::data_type(
                    row["name"].as_str().expect("compiled type name"),
                )
            })
            .collect()
    });
    state.has_pending_transport_tx()
        || types
            .iter()
            .any(|ty| router.has_pending_reliable_tx_type(*ty))
}
async fn wait_sdk(
    state: &Arc<AppState>,
    router: &sedsnet::router::Router,
    token: Token,
) -> Result<(), &'static str> {
    let start = WallInstant::now();
    while sdk_pending(state, router) {
        if !stop_unchanged(state, token) || start.elapsed() > QUIESCE_TTL {
            return Err(
                "SDK/local transport still owns work or new Stop; hidden modem retirement is not inferred",
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
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
    let proxy =
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
        let query = fresh_query(state, headers, ticket, token, target.board, &router).await?;
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
    let reset_all = ticket.action == "reset_startup" && ticket.targets.len() == BOARDS.len();
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
    crate::gse::reset_generation::finalize_verified_recovery(
        state, token, &fence, proxy, &proofs, reset_all,
    )?;
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
    struct Model {
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
    struct Link {
        board: Board,
        router: Router,
        host_side: usize,
        board_side: usize,
        outgoing: Arc<Mutex<Vec<Vec<u8>>>>,
        incoming: Arc<Mutex<Vec<Vec<u8>>>>,
        requests: Arc<Mutex<Vec<Packet>>>,
        model: Arc<Mutex<Model>>,
        pending_rx: VecDeque<Vec<u8>>,
    }
    struct Fixture {
        http: super::super::tests::HttpFixture,
        pump: JoinHandle<()>,
        receiver: JoinHandle<crate::telemetry_task::TelemetryShutdownOutcome>,
        models: Vec<(Board, Arc<Mutex<Model>>)>,
        raw_state_receipts: Arc<Mutex<Vec<Packet>>>,
        controller_receipts: Arc<Mutex<Vec<Packet>>>,
        _db: mpsc::Sender<crate::telemetry_db::DbQueueItem>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let healthy = !self.pump.is_finished() && !self.receiver.is_finished();
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
            let mut http = super::super::tests::http_fixture().await;
            let restarting = journal.is_some();
            if let Some(bytes) = journal {
                std::fs::write(http.directory.join("clock.json"), bytes).unwrap();
            }
            let (tx, commands) = mpsc::channel(8);
            Arc::get_mut(&mut http.state).unwrap().cmd_tx = tx;
            let state = http.state.clone();
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
            let weak = Arc::downgrade(&state);
            let config = RouterConfig::new([EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint("GROUND_STATION"),
                move |packet| {
                    if ["FLIGHT_COMMAND","VALVE_COMMAND","ACTUATOR_COMMAND"].iter()
                        .any(|name|packet.data_type()==crate::telemetry_schema::data_type(name)) {
                        received_controls.lock().unwrap().push(packet.clone());
                    }
                    if let Some(state) = weak.upgrade() {
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
                        if packet.data_type() == crate::telemetry_schema::data_type("FLIGHT_STATE")
                        {
                            received_raw.lock().unwrap().push(packet.clone());
                        }
                    }
                    Ok(())
                },
            )])
            .with_sender("GS");
            let host = Arc::new(match clock {
                Some(clock) => Router::new_with_clock(config, clock),
                None => Router::new(config),
            });
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
            for board in BOARDS {
                let requests = Arc::new(Mutex::new(Vec::<Packet>::new()));
                let capture = requests.clone();
                let dedicated = crate::telemetry_schema::endpoint(endpoint(board));
                let router = Router::new(
                    RouterConfig::new([
                        EndpointHandler::new_packet_handler(dedicated, move |packet| {
                            capture.lock().unwrap().push(packet.clone());
                            Ok(())
                        }),
                        EndpointHandler::new_packet_handler(
                            crate::telemetry_schema::endpoint("FLIGHT_STATE"),
                            |_| Ok(()),
                        ),
                    ])
                    .with_sender(board.sender_id()),
                );
                let outgoing = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
                let output = outgoing.clone();
                let hs = host.add_side_packed_with_options(
                    format!("wire-{}", board.sender_id()),
                    move |wire| {
                        output.lock().unwrap().push(wire.to_vec());
                        Ok(())
                    },
                    RouterSideOptions {
                        reliable_enabled: true,
                        ..Default::default()
                    },
                );
                let incoming = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
                let output = incoming.clone();
                let bs = router.add_side_packed_with_options(
                    "wire",
                    move |wire| {
                        output.lock().unwrap().push(wire.to_vec());
                        Ok(())
                    },
                    RouterSideOptions {
                        reliable_enabled: true,
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
                });
            }
            let pump_host = host.clone();
            let pump = tokio::spawn(async move {
                loop {
                    // The actual telemetry task is the sole host queue owner.
                    for link in &mut links {
                        for wire in std::mem::take(&mut *link.outgoing.lock().unwrap()) {
                            link.router
                                .rx_packed_from_side(&wire, link.board_side)
                                .unwrap();
                        }
                        link.router.process_all_queues_with_timeout(0).unwrap();
                        for packet in std::mem::take(&mut *link.requests.lock().unwrap()) {
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
                            if wire[1] == 1 {
                                if let Some(old) = m.seen.get(&nonce) {
                                    response = old.clone();
                                } else {
                                    let expected_boot =
                                        u64::from_le_bytes(wire[12..20].try_into().unwrap());
                                    assert_eq!(
                                        expected_boot,
                                        if m.boot == 0 && m.retained_first.is_some() {
                                            u64::MAX
                                        } else {
                                            m.boot
                                        }
                                    );
                                    assert_eq!(
                                        u32::from_le_bytes(wire[20..24].try_into().unwrap()),
                                        m.epoch
                                    );
                                    if let Some(failure) = m.fail_result {
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
                                response[3] = 0x24
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
                                if wire[1] == 1 && result != 1 {
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
                        link.router.process_all_queues_with_timeout(0).unwrap();
                        link.pending_rx
                            .extend(std::mem::take(&mut *link.incoming.lock().unwrap()));
                        assert!(
                            link.pending_rx.len() <= 1024,
                            "bounded test wire retains backpressure"
                        );
                        while let Some(wire) = link.pending_rx.front() {
                            match pump_host.rx_packed_from_side(wire, link.host_side) {
                                Ok(()) => {
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
            let (db, db_rx) = mpsc::channel(8);
            let receiver = tokio::spawn(crate::telemetry_task::telemetry_task(
                state.clone(),
                host,
                vec![],
                commands,
                db_rx,
                state.shutdown_subscribe(),
            ));
            Self {
                http,
                pump,
                receiver,
                models,
                raw_state_receipts,
                controller_receipts,
                _db: db,
            }
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
            let response = super::super::tests::http(
                self.state(),
                "POST",
                "/api/board-recovery/query",
                Some("operator"),
                json!({"board":board.sender_id()}),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "{}",
                body(response).await
            );
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
            let response = super::super::tests::http(state.clone(), "POST", "/api/board-recovery/prepare", Some("operator"), json!({"action":action,"board":board,"mode":null,"backend_instance":hex(&state.board_recovery.identity.unwrap()),"reset_epoch":format!("{:016x}",crate::gse::automatic_generation(&state))})).await;
            let status = response.status();
            let value = body(response).await;
            assert_eq!(status, StatusCode::OK, "{value}");
            value
        }
        async fn confirm(&self, ticket: &Value) -> Response {
            super::super::tests::http(self.state(), "POST", "/api/board-recovery/confirm", Some("operator"), json!({"ticket":ticket["ticket"],"backend_instance":ticket["backend_instance"],"principal_context":ticket["principal_context"],"reset_epoch":ticket["reset_epoch"],"confirmation":ticket["confirmation"]})).await
        }
        async fn terminal(&self) -> Value {
            tokio::time::timeout(Duration::from_secs(8), async {
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
            .await
            .unwrap()
        }
    }
    fn isolated(name: &str) -> bool {
        crate::operator_queue::isolated_case(module_path!(), name)
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
            super::super::tests::http(prepare_state.clone(),"POST","/api/board-recovery/prepare",Some("operator"),json!({"action":"reset_startup","board":null,"mode":null,"backend_instance":hex(&prepare_state.board_recovery.identity.unwrap()),"reset_epoch":format!("{:016x}",crate::gse::automatic_generation(&prepare_state))})).await
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manual_utc_prepare_wait_refreshes_queries_and_cold_rollback_refuses_ticket() {
        if isolated("manual_utc_prepare_wait_refreshes_queries_and_cold_rollback_refuses_ticket") {
            return;
        }
        use std::sync::atomic::AtomicU64;
        let ticks = Arc::new(AtomicU64::new(0));
        let clock_ticks = ticks.clone();
        let fixture = Fixture::from_saved_with_clock(
            None,
            None,
            Some(Box::new(move || clock_ticks.load(Ordering::Acquire))),
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
            super::super::tests::http(prepare_state.clone(),"POST","/api/board-recovery/prepare",Some("operator"),json!({"action":"reset_startup","board":null,"mode":null,"backend_instance":hex(&prepare_state.board_recovery.identity.unwrap()),"reset_epoch":format!("{:016x}",crate::gse::automatic_generation(&prepare_state))})).await
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
        let response = prepare.await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let ticket = body(response).await;
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
            super::super::tests::http(prepare_state.clone(),"POST","/api/board-recovery/prepare",Some("operator"),json!({"action":"reset_startup","board":null,"mode":null,"backend_instance":hex(&prepare_state.board_recovery.identity.unwrap()),"reset_epoch":format!("{:016x}",crate::gse::automatic_generation(&prepare_state))})).await
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
        if isolated(
            "board_abort_actual_sdk_refusal_at_each_prepared_batch_position_keeps_ownership_truthful",
        ) {
            return;
        }
        let fixture = Fixture::new().await;
        completed_manual_cohort(&fixture).await;
        let state = fixture.state();
        let _inhibited = crate::gse::inhibit_automatic_actions_for_launch(&state).unwrap();
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
            for board in BOARDS {
                fixture.query(board).await;
            }
            let router = make_router(lower);
            let prepared = crate::network_variables::prepare_launch_state(&router).unwrap();
            let original_clock = state.launch_clock_snapshot();
            let original_state = state.local_flight_state_snapshot();
            let original_desired = crate::network_variables::flight_state();
            let before = router.queue_counters()[2];
            let error = crate::gse::queue_prepared_launch_packets(
                &state,
                &router,
                crate::gse::LaunchAdmission::capture(&state),
                physical(&router),
                Some(prepared),
            )
            .unwrap_err();
            assert!(error.contains(&format!("{admitted}/3")), "{error}");
            assert_eq!(
                router.queue_counters()[2] - before,
                admitted as u32,
                "exact real SDK admitted prefix"
            );
            if admitted == 0 {
                assert_eq!(state.local_flight_state_snapshot(), original_state);
                assert_eq!(state.launch_clock_snapshot(), original_clock);
                assert_eq!(crate::network_variables::flight_state(), original_desired);
            } else {
                assert_eq!(
                    state.local_flight_state_snapshot(),
                    crate::types::FlightState::Launch
                );
                assert_eq!(crate::network_variables::flight_state(), 7);
                assert!(error.contains("explicit manual recovery required"));
            }
            assert!(
                state
                    .launch_history
                    .lock()
                    .unwrap()
                    .automatic_actions_inhibited()
            );
            assert_eq!(
                state.manual_abort_intent.epoch(),
                0,
                "queue refusal never automatically Aborts"
            );
        }
    }

    #[tokio::test]
    async fn board_abort_healthy_session_does_not_poll_until_a_new_explicit_launch_demand() {
        if isolated("board_abort_healthy_session_does_not_poll_until_a_new_explicit_launch_demand")
        {
            return;
        }
        let fixture = Fixture::new().await;
        completed_manual_cohort(&fixture).await;
        let state = fixture.state();
        let count = || {
            fixture
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
                .sum::<usize>()
        };
        let baseline = count();
        assert_eq!(state.board_abort.take_queries(), 0);
        let worker = tokio::spawn(crate::board_abort::query_service(
            state.clone(),
            state.shutdown_tx.subscribe(),
        ));
        tokio::time::sleep(Duration::from_millis(2100)).await;
        for _ in 0..100 {
            state.action_policy_snapshot();
            state.broadcast_action_policy_snapshot();
            crate::primary_mirror::encode_control_observation(&state, 96);
            state
                .board_abort
                .snapshot(crate::board_abort::required_mask(&state));
        }
        assert_eq!(
            count(),
            baseline,
            "healthy proof expiration and periodic presentation never poll boards"
        );
        assert_eq!(state.board_abort.take_queries(), 0);
        assert!(
            state.board_abort.snapshot(0).active_errors.is_empty(),
            "proof TTL cannot recreate a cleared condition"
        );
        let router = state.topology_router.get().unwrap();
        let wanted = crate::board_abort::required_mask(&state).count_ones() as usize;
        // HITL intentionally makes disconnected layout domains optional. Do
        // not manufacture seven required boards merely to match normal mode.
        if wanted != 0 {
            assert!(
                !crate::gse::queue_controller_with_stop_fence(
                    &state,
                    router,
                    crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
                    &[1]
                )
                .unwrap()
            );
        } else {
            let denied = super::super::tests::http(
                state.clone(),
                "POST",
                "/api/board_abort/refresh",
                None,
                json!({}),
            )
            .await;
            assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(state.board_abort.take_queries(), 0);
            let response = super::super::tests::http(
                state.clone(),
                "POST",
                "/api/board_abort/refresh",
                Some("viewer"),
                json!({}),
            )
            .await;
            assert_eq!(response.status(), StatusCode::ACCEPTED);
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            while count() < baseline + wanted {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            count(),
            baseline + wanted,
            "one coalesced batch, no mode synchronization/recovery writes"
        );
        assert!(
            !router
                .has_pending_reliable_tx_type(crate::telemetry_schema::data_type("FLIGHT_COMMAND")),
            "check never retries Fire"
        );
        worker.abort();
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
        if isolated(
            "board_abort_actual_status_must_refuse_sdk_launch_without_changing_manual_owner",
        ) {
            return;
        }
        let fixture = Fixture::new().await;
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
                crate::types::FlightState::Startup
            );
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
        assert_eq!(original.session, current.session);
        assert_eq!(original.t0_utc_ms, current.t0_utc_ms);
        assert_eq!(current.revision, original.revision + 1);
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
    async fn immutable_ticket_survives_readback_expiry_but_server_refreshes_before_recover() {
        if isolated("immutable_ticket_survives_readback_expiry_but_server_refreshes_before_recover")
        {
            return;
        }
        let fixture = Fixture::new().await;
        fixture.query(Board::ActuatorBoard).await;
        let ticket = fixture.prepare("recover_abort", Some("AB")).await;
        tokio::time::sleep(READBACK_TTL + Duration::from_millis(20)).await;
        assert!(
            !fixture
                .http
                .state
                .board_recovery
                .inner
                .lock()
                .unwrap()
                .values[index(Board::ActuatorBoard).unwrap()]
            .as_ref()
            .unwrap()
            .fresh(Stamp::now())
        );
        assert_eq!(fixture.confirm(&ticket).await.status(), StatusCode::OK);
        let operation = fixture.terminal().await;
        assert_eq!(operation["success"], true, "{operation}");
        let model = fixture
            .models
            .iter()
            .find(|(b, _)| *b == Board::ActuatorBoard)
            .unwrap()
            .1
            .lock()
            .unwrap();
        assert!(
            model
                .requests
                .iter()
                .filter(|p| p.payload()[1] == 0)
                .count()
                >= 3
        );
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
    #[tokio::test]
    async fn fresh_query_cannot_adopt_a_boot_epoch_changed_after_confirmation_context() {
        if isolated("fresh_query_cannot_adopt_a_boot_epoch_changed_after_confirmation_context") {
            return;
        }
        let fixture = Fixture::new().await;
        fixture.query(Board::ActuatorBoard).await;
        let ticket = fixture.prepare("recover_abort", Some("AB")).await;
        fixture
            .models
            .iter()
            .find(|(b, _)| *b == Board::ActuatorBoard)
            .unwrap()
            .1
            .lock()
            .unwrap()
            .boot = 43;
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
        for (_, model) in &fixture.models {
            assert_eq!(model.lock().unwrap().applied, 0);
        }
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
        assert_eq!(stale["confirmation"]["ticket"], ticket["ticket"]);
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
        assert_eq!(rf["capabilities"]["recover"], false);
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
}
