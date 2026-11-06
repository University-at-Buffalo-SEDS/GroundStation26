//! Verified application-state recovery with explicit authenticated intent.
//! SDK ACKs certify delivery ownership, never board state or physical outputs.
pub(crate) mod coordinator;
mod exchange;

pub(crate) mod scope;
use crate::{auth::Permission, state::AppState, types::Board};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use ring::{
    digest,
    rand::{SecureRandom, SystemRandom},
};
use sedsnet::packet::Packet;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    array,
    sync::{Arc, Mutex},
    time::{Duration, Instant as WallInstant},
};

/// Query/state validity follows explicit model monotonic time only when a
/// simulation clock is configured. Human confirmation tickets use WallInstant
/// separately; network UTC and router-time divisors never enter this clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    wall: WallInstant,
    model_ms: Option<u64>,
}
impl Stamp {
    fn now() -> Self {
        Self {
            wall: WallInstant::now(),
            model_ms: crate::simulation_clock_ms(),
        }
    }
    fn checked_duration_since(self, earlier: Self) -> Option<Duration> {
        match (self.model_ms, earlier.model_ms) {
            (Some(now), Some(start)) => now.checked_sub(start).map(Duration::from_millis),
            (None, None) => self.wall.checked_duration_since(earlier.wall),
            _ => None,
        }
    }
    fn duration_since(self, earlier: Self) -> Duration {
        self.checked_duration_since(earlier)
            .unwrap_or(Duration::MAX)
    }
}
#[cfg(test)]
impl std::ops::Sub<Duration> for Stamp {
    type Output = Self;
    fn sub(self, duration: Duration) -> Self {
        Self {
            wall: self.wall - duration,
            model_ms: self
                .model_ms
                .map(|time| time.checked_sub(duration.as_millis() as u64).unwrap()),
        }
    }
}
#[cfg(test)]
impl std::ops::Add<Duration> for Stamp {
    type Output = Self;
    fn add(self, duration: Duration) -> Self {
        Self {
            wall: self.wall + duration,
            model_ms: self
                .model_ms
                .map(|time| time.checked_add(duration.as_millis() as u64).unwrap()),
        }
    }
}

const BOARDS: [Board; 7] = [
    Board::RFBoard,
    Board::FlightComputer,
    Board::PowerBoard,
    Board::ActuatorBoard,
    Board::ValveBoard,
    Board::GatewayBoard,
    Board::DaqBoard,
];
const READBACK_TTL: Duration = Duration::from_secs(5);
const QUERY_TTL: Duration = Duration::from_secs(5);
const QUERY_TYPE: &str = "BOARD_RECOVERY_REQUEST";
const STATUS_TYPE: &str = "BOARD_STATE_STATUS";
const STATE_VALID: u8 = 8;
const OUTPUTS_VALID: u8 = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Readback {
    result: u8,
    state: Option<u8>,
    flags: u8,
    nonce: u64,
    boot: u64,
    epoch: u32,
    uptime: u32,
    outputs: u32,
    issuer: u64,
}
impl Readback {
    fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != 40 || bytes[0] != 1 || bytes[1] > 7 || bytes[3] & !0x7f != 0 {
            return None;
        }
        let flags = bytes[3];
        if flags & 2 != 0 && flags & OUTPUTS_VALID == 0 {
            return None;
        }
        let state = if flags & STATE_VALID != 0 {
            crate::types::u8_to_flight_state(bytes[2])?;
            Some(bytes[2])
        } else {
            if bytes[2] != 255 {
                return None;
            }
            None
        };
        let u64_at = |at| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        let u32_at = |at| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        let readback = Self {
            result: bytes[1],
            state,
            flags,
            nonce: u64_at(4),
            boot: u64_at(12),
            epoch: u32_at(20),
            uptime: u32_at(24),
            outputs: u32_at(28),
            issuer: u64_at(32),
        };
        if readback.nonce == 0 || readback.issuer == 0 {
            return None;
        }
        Some(readback)
    }
}
#[derive(Clone)]
struct Observation {
    value: Readback,
    received: Stamp,
    // A nonce-correlated reply was sampled after this request began. Counting
    // from receipt alone could disguise several seconds of queue residence.
    observed_after: Stamp,
}
impl Observation {
    fn age(&self, now: Stamp) -> Duration {
        now.checked_duration_since(self.observed_after)
            .unwrap_or(Duration::MAX)
    }
    fn fresh(&self, now: Stamp) -> bool {
        self.age(now) <= READBACK_TTL
    }
}
#[derive(Clone)]
struct Query {
    abort_revision: u64,
    nonce: u64,
    sent: Stamp,
    received: bool,
}
struct Inner {
    values: [Option<Observation>; 7],
    queries: [Option<Query>; 7],
    rejected_statuses: u64,
    last_rejection: Option<&'static str>,
}
pub(crate) struct Runtime {
    identity: Option<[u8; 16]>,
    coordinator: coordinator::Runtime,
    inner: Mutex<Inner>,
    pub(crate) manual_utc: crate::manual_abort_utc::Runtime,
}
impl Default for Runtime {
    fn default() -> Self {
        let mut bytes = [0; 16];
        let identity = SystemRandom::new()
            .fill(&mut bytes)
            .ok()
            .filter(|_| bytes[..8] != [0; 8] && bytes[8..] != [0; 8])
            .map(|_| bytes);
        Self {
            identity,
            manual_utc: Default::default(),
            coordinator: Default::default(),
            inner: Mutex::new(Inner {
                values: array::from_fn(|_| None),
                queries: array::from_fn(|_| None),
                rejected_statuses: 0,
                last_rejection: None,
            }),
        }
    }
}
fn index(board: Board) -> Option<usize> {
    BOARDS.iter().position(|&value| value == board)
}
fn endpoint(board: Board) -> &'static str {
    match board {
        Board::ActuatorBoard => "BOARD_RECOVERY_AB",
        Board::ValveBoard => "BOARD_RECOVERY_VB",
        Board::RFBoard => "BOARD_RECOVERY_RF",
        Board::FlightComputer => "BOARD_RECOVERY_FC",
        Board::PowerBoard => "BOARD_RECOVERY_PB",
        Board::GatewayBoard => "BOARD_RECOVERY_GB",
        Board::DaqBoard => "BOARD_RECOVERY_DAQ",
        Board::GroundStation => unreachable!(),
    }
}
/// Application target only; this never requests a reboot or a new flight.
fn targeted_recovery_state(board: Board) -> Option<u8> {
    match board {
        Board::ActuatorBoard | Board::ValveBoard => Some(1),
        Board::FlightComputer | Board::RFBoard | Board::PowerBoard
        | Board::GatewayBoard | Board::DaqBoard => Some(0),
        Board::GroundStation => None,
    }
}

/// Commanded-register safe domains from the actual board owners. VB's safe
/// state opens both servos (bits1/2) and switches its pilot solenoid off.
/// These are commanded registers, never mechanical position verification.
fn safe_output_mask(board: Board) -> Option<u32> {
    match board {
        Board::ActuatorBoard | Board::FlightComputer => Some(0),
        Board::ValveBoard => Some(6),
        _ => None,
    }
}
fn query_domain_consistent(board: Board, row: &Readback) -> bool {
    match safe_output_mask(board) {
        Some(mask) => row.flags & 2 == 0 || row.flags & OUTPUTS_VALID != 0 && row.outputs == mask,
        None => row.flags & (2 | OUTPUTS_VALID) == 0,
    }
}
/// Explanatory status only. It never grants a recovery ticket or changes the
/// nonce/boot/epoch freshness and source-time checks at PREPARE/CONFIRM.
fn recovery_diagnostic(
    primary: bool,
    verified: bool,
    network_utc_available: bool,
    observation: Option<&Observation>,
    query: Option<&Query>,
    now: Stamp,
    reconcilable: bool,
) -> (&'static str, &'static str) {
    if !primary {
        return (
            "not_primary",
            "Recovery authority belongs to the current primary.",
        );
    }
    if !verified {
        return (
            "endpoint_unverified",
            "Dedicated recovery endpoint is not verified. Query and recovery are unavailable.",
        );
    }
    if !network_utc_available {
        return (
            "network_utc_unavailable",
            "Network UTC is unavailable. Query remains read-only; establish network time before preparing recovery.",
        );
    }
    let Some(observation) = observation else {
        return if query.is_some_and(|q| {
            !q.received
                && now
                    .checked_duration_since(q.sent)
                    .is_some_and(|age| age <= QUERY_TTL)
        }) {
            (
                "query_pending",
                "Query is awaiting board readback. No recovery has been sent.",
            )
        } else {
            (
                "query_required",
                "Waiting for automatic board status. Recovery needs an identified board and explicit confirmation.",
            )
        };
    };
    if !observation.fresh(now) {
        return if observation.value.boot == u64::MAX {
            (
                "commissioning_readback_expired",
                "This board needs explicit commissioning. Status refreshes automatically; Recover checks a fresh readback before confirmation.",
            )
        } else {
            (
                "readback_expired",
                "The board readback is older than five seconds. Status refreshes automatically; preparing Recover requests a fresh readback.",
            )
        };
    }
    let row = &observation.value;
    if row.boot == u64::MAX && row.result == 7 {
        return (
            "commissioning_required",
            "Not yet commissioned. The first confirmed Recover provisions identity and keeps outputs inhibited. A fresh status check and separate second confirmation apply the requested state.",
        );
    }
    if row.boot == 0 && !reconcilable {
        return (
            "identity_unavailable",
            "The board has no usable recovery identity. A zero boot generation does not authorize commissioning; inspect its query result.",
        );
    }
    if row.result == 4 {
        return (
            "stale_recovery_context",
            "The board rejected a stale recovery context. Check network time; status refreshes automatically before another attempt.",
        );
    }
    if row.result == 7 {
        return (
            "board_io",
            "The board reports a storage or identity recovery error. Recovery requires a new explicit confirmation; this is not a completed reset.",
        );
    }
    (
        "readback_current",
        "Current board readback. Recovery still requires explicit confirmation and fresh server checks.",
    )
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn issuer(identity: &[u8; 16]) -> u64 {
    u64::from_le_bytes(identity[..8].try_into().unwrap())
}
fn endpoint_verified(
    state: &AppState,
    board: Board,
    topology: &sedsnet::discovery::TopologySnapshot,
) -> bool {
    endpoint_advertisement_age(state, board, topology).is_some_and(|age| age <= 12_000)
}

/// Age of the newest matching identity/endpoint advertisement. Discovery
/// refresh may use this before expiry; it never grants command authority.
pub(crate) fn endpoint_advertisement_age(
    state: &AppState,
    board: Board,
    topology: &sedsnet::discovery::TopologySnapshot,
) -> Option<u64> {
    let expected = crate::telemetry_schema::endpoint(endpoint(board));
    topology
        .routes
        .iter()
        .flat_map(|route| {
            route
                .announcers
                .iter()
                .filter(move |peer| {
                    (state.board_from_network_sender(&peer.sender_id) == Some(board)
                        && peer.reachable_endpoints.contains(&expected))
                        || peer.routers.iter().any(|node| {
                            state.board_from_network_sender(&node.sender_id) == Some(board)
                                && node.reachable_endpoints.contains(&expected)
                        })
                })
                .map(move |peer| route.age_ms.max(peer.age_ms))
        })
        .min()
}
/// Called directly at SDK ingress, before DB/GSE work. Normal-runtime time is
/// RAM-only; an explicit simulation uses the router's existing model clock.
pub(crate) fn observe(state: &AppState, packet: &Packet) -> Result<(), &'static str> {
    let board = state
        .board_from_network_sender(packet.sender())
        .ok_or("Unknown recovery status sender")?;
    let at = index(board).ok_or("Host is not a board recovery target")?;
    if packet.data_type() != crate::telemetry_schema::data_type(STATUS_TYPE)
        || packet.endpoints() != [crate::telemetry_schema::endpoint("GROUND_STATION")]
    {
        return Err("Wrong recovery status destination");
    }
    let value = Readback::decode(packet.payload()).ok_or("Malformed recovery status")?;
    if value.flags & 0x40 != 0
        && !matches!(
            board,
            Board::ActuatorBoard | Board::ValveBoard | Board::FlightComputer
        )
    {
        return Err("Metadata node advertised physical manual Stop capability");
    }

    let id = state
        .board_recovery
        .identity
        .ok_or("Backend recovery identity unavailable")?;
    if value.issuer != issuer(&id) {
        return Err("Recovery issuer context changed");
    }
    let now = Stamp::now();
    let mut inner = state.board_recovery.inner.lock().unwrap();
    let query = inner.queries[at]
        .as_mut()
        .ok_or("Unsolicited recovery status")?;
    if query.nonce != value.nonce
        || now
            .checked_duration_since(query.sent)
            .is_none_or(|age| age > QUERY_TTL)
    {
        return Err("Expired or different recovery query");
    }
    if query.received {
        return Ok(());
    } // Duplicate replies do not slide freshness.
    // PENDING is an interim wire result, not a completed correlated query.
    // Preserve the last confirmed presentation but never renew its proof age.
    if value.result == 1 {
        if inner.values[at].as_ref().is_some_and(|old|
            old.value.boot != value.boot || old.value.epoch != value.epoch) {
            inner.values[at] = None;
        }
        return Ok(());
    }
    query.received = true;
    let observed_after = query.sent;
    let abort_revision = query.abort_revision;
    let accepted = value.clone();
    inner.values[at] = Some(Observation {
        value,
        received: now,
        observed_after,
    });
    drop(inner);
    state.board_abort.observe_status(
        board,
        accepted.boot,
        accepted.epoch,
        accepted.nonce,
        accepted.state,
        accepted.flags & 1 != 0,
        accepted.result == 0
            && accepted.flags & 0x0c == 0x0c
            && query_domain_consistent(board, &accepted),
        observed_after.wall,
        abort_revision,
        now.wall,
        crate::telemetry_task::get_current_timestamp_ms(),
    );
    Ok(())
}
/// Last board-reported output registers for presentation only. No lease or
/// command capability is granted by connectivity or this retained readback.
pub(crate) fn sequence_board_display(state:&AppState)->std::collections::BTreeMap<String,crate::sequence_progress_service::BoardDisplay> {
    let health=state.network_health.lock().unwrap().snapshot(WallInstant::now(),cfg!(feature="test_fire_mode"));
    let now=Stamp::now();
    let inner=state.board_recovery.inner.lock().unwrap();
    [Board::ActuatorBoard,Board::ValveBoard].into_iter().map(|board| {
        let connected=health.iter().any(|row|row.sender_id==board.sender_id()&&row.healthy);
        let value=inner.values[index(board).unwrap()].as_ref().map(|row|&row.value)
            .filter(|row|connected&&row.result==0&&row.boot!=0&&row.boot!=u64::MAX&&row.epoch!=0&&row.state.is_some()&&row.flags&OUTPUTS_VALID!=0&&query_domain_consistent(board,row));
        (board.sender_id().to_owned(),crate::sequence_progress_service::BoardDisplay {connected,known:value.is_some(),boot:value.map(|row|format!("{:016x}",row.boot)),epoch:value.map(|row|row.epoch),outputs:value.map(|row|row.outputs),outputs_age_ms:value.and_then(|_|inner.values[index(board).unwrap()].as_ref()).map(|row|row.age(now).as_millis().min(u64::MAX as u128)as u64)})
    }).collect()
}
/// A semantic rejection is not application confirmation. ACK/drop data status
/// instead of trapping later valid ordered replies behind an obsolete context.
pub(crate) fn receive_status(state: &AppState, packet: &Packet) {
    coordinator::retain_status(state, packet);
    if let Err(reason) = observe(state, packet) {
        let mut inner = state.board_recovery.inner.lock().unwrap();
        inner.rejected_statuses = inner.rejected_statuses.saturating_add(1);
        inner.last_rejection = Some(reason);
    } else {
        state.board_recovery.manual_utc.wake.notify_one();
    }
}
pub(crate) fn manual_utc_proof(
    state: &AppState,
    key: crate::launch_history::ManualUtcKey,
) -> Option<[u64; 3]> {
    let (model_ms, _) = crate::cached_simulation_clock_sample()?;
    let now = Stamp {
        wall: WallInstant::now(),
        model_ms,
    };
    let inner = state.board_recovery.inner.try_lock().ok()?;
    let identity = state.board_recovery.identity?;
    let mut nonces = [0; 3];
    for (i, board) in [
        Board::ActuatorBoard,
        Board::ValveBoard,
        Board::FlightComputer,
    ]
    .into_iter()
    .enumerate()
    {
        let at = index(board)?;
        let row = inner.values[at].as_ref()?;
        let query = inner.queries[at].as_ref()?;
        if !query.received
            || query.nonce != row.value.nonce
            || !row.fresh(now)
            || row.value.issuer != issuer(&identity)
            || row.value.result != 0
            || row.value.flags & 0x64 != 0x64
            || row.value.boot != key.boots[i]
            || row.value.epoch < key.epochs[i]
            || !query_domain_consistent(board, &row.value)
        {
            return None;
        }
        nonces[i] = query.nonce;
    }
    Some(nonces)
}
pub(crate) fn manual_utc_endpoints(state: &AppState, router: &sedsnet::router::Router) -> bool {
    let topology = router.export_topology();
    [
        Board::ActuatorBoard,
        Board::ValveBoard,
        Board::FlightComputer,
    ]
    .into_iter()
    .all(|board| endpoint_verified(state, board, &topology))
}

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({"error":code,"message":message}))).into_response()
}
/// Mounted by production and exercised unchanged by HTTP authorization tests.
pub(crate) fn routes() -> axum::Router<Arc<AppState>> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/api/board-recovery/status", get(get_status))
        .route("/api/board-recovery/query", post(query))
        .route("/api/board-recovery/prepare", post(coordinator::prepare))
        .route("/api/board-recovery/confirm", post(coordinator::confirm))
}
#[expect(clippy::result_large_err, reason = "Axum HTTP helpers retain the explicit Response rejection API without boxing or changing endpoint behavior")]
fn primary_only() -> Result<(), Response> {
    if crate::rrls::is_rrls() {
        Err(error(
            StatusCode::CONFLICT,
            "primary_required",
            "Recovery queries and writes require the current primary. This RRLS does not proxy recovery authority.",
        ))
    } else {
        Ok(())
    }
}
fn principal_context(
    identity: &[u8; 16],
    headers: &HeaderMap,
    principal: &crate::auth::AuthPrincipal,
) -> String {
    let mut context = digest::Context::new(&digest::SHA256);
    context.update(identity);
    if let Some(value) = headers.get(axum::http::header::AUTHORIZATION) {
        context.update(value.as_bytes());
    }
    context.update(format!("{:?}", principal).as_bytes());
    hex(&context.finish().as_ref()[..16])
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct ActuatorIdleProof {
    pub(crate) boot: u64,
    pub(crate) epoch: u32,
    pub(crate) outputs: u32,
}
/// RAM-only fresh correlated proof for the final motion/launch gate. It
/// does not query SDK topology or storage while that gate is held.
pub(crate) fn actuator_cached_motion_safe_proof(state: &AppState) -> Option<ActuatorIdleProof> {
    let inner = state.board_recovery.inner.lock().unwrap();
    let row = inner.values[index(Board::ActuatorBoard)?].as_ref()?;
    let value = &row.value;
    if row.observed_after.wall.elapsed() > READBACK_TTL
        || value.result != 0
        || !value.state.is_some_and(|state| state <= 6)
        || value.flags & (STATE_VALID | OUTPUTS_VALID) != (STATE_VALID | OUTPUTS_VALID)
        || value.flags & 1 != 0
        || matches!(value.boot, 0 | u64::MAX)
        || matches!(value.epoch, 0 | u32::MAX)
    {
        return None;
    }
    Some(ActuatorIdleProof {
        boot: value.boot,
        epoch: value.epoch,
        outputs: value.outputs,
    })
}
/// Drive selection is configuration, not motion admission. A freshly observed
/// Startup board can be configured before flight-state synchronization reaches
/// Idle; launch and motion retain their independent admission checks.
pub(crate) fn actuator_configuration_proof(state: &AppState) -> Option<ActuatorIdleProof> {
    let router = state.topology_router.get()?;
    if !endpoint_verified(state, Board::ActuatorBoard, &router.export_topology()) {
        return None;
    }
    let inner = state.board_recovery.inner.lock().unwrap();
    let row = inner.values[index(Board::ActuatorBoard)?].as_ref()?;
    let value = &row.value;
    if !row.fresh(Stamp::now())
        || value.result != 0
        || !matches!(value.state, Some(0 | 1))
        || value.flags & (STATE_VALID | OUTPUTS_VALID) != (STATE_VALID | OUTPUTS_VALID)
        || value.flags & 1 != 0
        || matches!(value.boot, 0 | u64::MAX)
        || matches!(value.epoch, 0 | u32::MAX)
    {
        return None;
    }
    Some(ActuatorIdleProof { boot: value.boot, epoch: value.epoch, outputs: value.outputs })
}
pub(crate) async fn get_status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
            Ok(p) => p,
            Err(r) => return r,
        };
    let Some(identity) = state.board_recovery.identity else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "identity_unavailable",
            "Backend recovery identity is unavailable; no recovery query or write can be admitted.",
        );
    };
    let now = Stamp::now();
    let (values, queries, rejected_statuses, last_rejection) = {
        let inner = state.board_recovery.inner.lock().unwrap();
        (
            inner.values.clone(),
            inner.queries.clone(),
            inner.rejected_statuses,
            inner.last_rejection,
        )
    };
    let network = state
        .network_health
        .lock()
        .unwrap()
        .snapshot(now.wall, cfg!(feature = "test_fire_mode"));
    let router = state.topology_router.get();
    let topology = router.map(|router| router.export_topology());
    let primary =
        !crate::rrls::is_rrls() && router.is_some_and(|router| router.sender().as_ref() == "GS");
    // Descriptive compiled policy only, never a grant or a board proof.
    // PREPARE captures actual Abort/retained/optional union independently.
    let reset_startup_policy = primary.then(|| {
        let mode=crate::station_mode::local_mode();
        let mask=scope::base_mask(mode);
        let base=BOARDS.iter().enumerate().filter_map(|(i,board)|
            (mask & (1 << i) != 0).then_some(board.sender_id())).collect::<Vec<_>>();
        json!({"version":1,"global_startup_best_effort":true,"mode":mode,
            "base_required_boards":base,"optional_proofs":true})
    });
    let network_utc_available = router
        .is_some_and(|router| crate::system_clock::required_control_network_utc_ms(router).is_ok());
    let can_recover = primary
        && network_utc_available
        && !principal.anonymous
        && principal.permissions.send_commands
        && coordinator::host_safe(&state)
        && state.launch_history.lock().unwrap().recovery_available();
    let can_reset_startup = primary
        && network_utc_available
        && !principal.anonymous
        && principal.permissions.send_commands
        && coordinator::host_action_allowed(&state, "reset_startup")
        && state.launch_history.lock().unwrap().recovery_available();
    let principal_identity = principal_context(&identity, &headers, &principal);
    let confirmation = state
        .board_recovery
        .coordinator
        .confirmation(&state, &principal_identity);
    let host_pending = state.launch_history.lock().unwrap().recovery_pending();
    let boards=BOARDS.iter().enumerate().map(|(at,&board)| {
        let health=network.iter().find(|entry|entry.sender_id==board.sender_id());
        let verified=topology.as_ref().is_some_and(|topology|endpoint_verified(&state,board,topology));
        let observation=values[at].as_ref();
        let readback=observation.map(|row|&row.value);
        let age=observation.map(|row|row.age(now).as_millis().min(u64::MAX as u128)as u64);
        let fresh=observation.is_some_and(|row|row.fresh(now));
        let reconcilable=verified&&fresh&&readback.is_some_and(|row|coordinator::first_uncertainty(host_pending.as_ref(),board,row));
        let known=readback.filter(|row|(row.boot!=0||reconcilable)&&row.epoch!=0);
        let supported=can_recover&&verified&&fresh&&readback.is_some_and(|row|query_domain_consistent(board,row)&&row.flags&0x24==0x24&&matches!(row.result,0|7)&&(row.boot!=0||reconcilable)&&row.epoch!=0&&row.epoch<u32::MAX-1);
        // Preparation grants only a fresh query/challenge, never an application
        // proof. PREPARE re-queries before creating its ticket; CONFIRM rechecks.
        let prepare_recover=can_recover&&verified&&targeted_recovery_state(board).is_some()
            && readback.is_some_and(|row|query_domain_consistent(board,row)&&row.flags&0x24==0x24
                &&matches!(row.result,0|7)&&(row.boot!=0||coordinator::first_uncertainty(host_pending.as_ref(),board,row))
                &&row.epoch!=0&&row.epoch<u32::MAX-1);
        let (readback_status, message) = recovery_diagnostic(primary, verified, network_utc_available,
            observation, queries[at].as_ref(), now, reconcilable);
        let operation=queries[at].as_ref().map(|query| {
            let mut operation_id=digest::Context::new(&digest::SHA256);operation_id.update(&identity);operation_id.update(&query.nonce.to_le_bytes());operation_id.update(board.sender_id().as_bytes());
            let pending=!query.received&&now.checked_duration_since(query.sent).is_some_and(|age|age<=QUERY_TTL);
            json!({"id":hex(&operation_id.finish().as_ref()[..16]),"action":"query","phase":if pending{"querying"}else if query.received{"completed"}else{"failed"},"targets":[board.sender_id()],"completed":!pending,"success":if pending{None}else{Some(query.received&&readback.is_some_and(|row|row.nonce==query.nonce&&row.result==0))},"age_ms":now.checked_duration_since(query.sent).unwrap_or(Duration::MAX).as_millis().min(u64::MAX as u128)as u64,"message":if pending{"Query admitted; actual board readback pending."}else if query.received{"Board answered; its result and actual-state validity are shown separately."}else{"Query readback timed out; recovery was not issued."}})
        });
        json!({"id":board.sender_id(),"label":board.as_str(),
            "display_connected":health.is_some_and(|h|h.healthy),
            "display_known":health.is_some_and(|h|h.healthy)&&known.is_some_and(|row|row.result==0&&row.state.is_some()&&query_domain_consistent(board,row)),
            "network":{"healthy":health.is_some_and(|h|h.healthy),"age_ms":health.map_or(u64::MAX,|h|h.age_ms),"disconnects":health.map_or(0,|h|h.disconnects),"reconnects":health.map_or(0,|h|h.reconnects)},
            "state":readback.and_then(|row|row.state),"state_label":readback.and_then(|row|row.state).and_then(crate::types::u8_to_flight_state).map_or("Unknown".to_string(),|value|format!("{value:?}")),
            "boot":known.map(|row|format!("{:016x}",row.boot)),"epoch":known.map(|row|row.epoch),"nonce":known.map(|row|format!("{:016x}",row.nonce)),"age_ms":age,"readback_valid_for_ms":observation.map(|row|READBACK_TTL.saturating_sub(row.age(now)).as_millis().min(u64::MAX as u128)as u64),
            "receipt_age_ms":observation.map(|row|now.duration_since(row.received).as_millis().min(u64::MAX as u128)as u64),"uptime_ms":readback.map(|row|row.uptime),"abort_latched":readback.filter(|_|matches!(board,Board::ActuatorBoard|Board::ValveBoard|Board::FlightComputer)).map(|row|row.flags&1!=0),"safe_outputs":readback.filter(|row|row.flags&OUTPUTS_VALID!=0).map(|row|row.flags&2!=0&&query_domain_consistent(board,row)),"commanded_outputs":readback.filter(|row|row.flags&OUTPUTS_VALID!=0).map(|row|row.outputs),
            "output_readback":if readback.is_some_and(|row|row.flags&OUTPUTS_VALID!=0){"gpio_servo_command_registers"}else{"unknown"},"physical_position_verified":false,
            "capabilities":{"query":primary&&verified,"endpoint_verified":verified,"hardened_transport":verified&&fresh&&readback.is_some_and(|row|row.flags&0x20!=0),"known_utc_manual_stop":verified&&fresh&&matches!(board,Board::ActuatorBoard|Board::ValveBoard|Board::FlightComputer)&&readback.is_some_and(|row|row.flags&0x40!=0),"recover":supported&&targeted_recovery_state(board).is_some(),"prepare_recover":prepare_recover,"resync":false,"reset_startup":supported},"operation":operation,
            "result":readback.filter(|_|fresh).map(|row|row.result),"commissioning_required":fresh&&readback.is_some_and(|row|row.boot==u64::MAX||row.result==7),"first_uncertainty_reconcilable":reconcilable,"identity_kind":if reconcilable{"retained_first_uncertainty"}else if readback.is_some_and(|row|row.boot==u64::MAX){"commissioning_sentinel"}else if known.is_some(){"retained_generation"}else{"unknown"},
            "readback_status":readback_status,"message":message})
    }).collect::<Vec<_>>();
    let mode = crate::station_mode::snapshot();
    let current = mode
        .effective_mode
        .map(|mode| serde_json::to_value(mode).unwrap())
        .unwrap_or_else(|| Value::String("unknown".into()));
    let build = serde_json::to_value(mode.local_build_mode).unwrap();
    Json(json!({"version":1,"backend_instance":hex(&identity),"principal":{"username":principal.username,"context":principal_context(&identity,&headers,&principal)},"reset_epoch":format!("{:016x}",crate::gse::automatic_generation(&state)),"flight_state":state.local_flight_state_snapshot()as u8,"automatic_actions_inhibited":state.launch_history.lock().unwrap().automatic_actions_inhibited(),
        "clock":{"network_utc_available":network_utc_available,"required_for_recovery":true},
        "permissions":{"query":primary,"recover":can_recover,"reset_startup":can_reset_startup,"change_mode":false},
        "reset_startup_policy":reset_startup_policy,
        "confirmation":confirmation,"host_recovery":host_pending.map(|pending|json!({"pending":!pending.published,"verified":pending.verified,"publication_acknowledged":pending.published,"targets":pending.targets.iter().map(|target|target.board.as_str()).collect::<Vec<_>>()})),
        "rejected_statuses":rejected_statuses,"last_rejection":last_rejection,
        "mode_capability":{"current":current,"build_mode":build,"supported_modes":[build],"runtime_switch_supported":false,"requires_restart":["normal","hitl","test_fire","testing"],"message":"Only the compiled runtime is supported; changing labels does not switch hardware policy."},"boards":boards,"operation":state.board_recovery.coordinator.operation()})).into_response()
}
/// Background reads share a two-second budget with demand-triggered reads.
/// No method here prepares, confirms, stops or recovers a board.
pub(crate) struct ReadonlyQueryRefresh {
    last: [Option<WallInstant>; 7],
    cursor: usize,
}
impl Default for ReadonlyQueryRefresh {
    fn default() -> Self {
        Self {
            last: [None; 7],
            cursor: 0,
        }
    }
}
impl ReadonlyQueryRefresh {
    pub(crate) fn request(&mut self, state: &Arc<AppState>, board: Board) -> bool {
        let Some(at) = index(board) else {
            return false;
        };
        let now = WallInstant::now();
        if !readonly_query_due(self.last[at], now) {
            return false;
        }
        // Attempt budget also bounds refusals; pending QUERY's own guard remains
        // in request_board_query and is shared with authenticated manual requests.
        self.last[at] = Some(now);
        request_board_query(state, board).status() == StatusCode::OK
    }
    pub(crate) fn poll(&mut self, state: &Arc<AppState>) -> Option<Board> {
        if crate::rrls::is_rrls() || !coordinator::readonly_query_idle(state) {
            return None;
        }
        let router = state.topology_router.get()?;
        if router.sender().as_ref() != "GS" {
            return None;
        }
        let now = Stamp::now();
        let topology = router.export_topology();
        let healthy = state
            .network_health
            .lock()
            .unwrap()
            .snapshot(now.wall, cfg!(feature = "test_fire_mode"));
        let candidate = {
            let inner = state.board_recovery.inner.lock().unwrap();
            (0..BOARDS.len())
                .map(|offset| (self.cursor + offset) % BOARDS.len())
                .find(|&at| {
                    let board = BOARDS[at];
                    let seen = healthy
                        .iter()
                        .any(|row| row.sender_id == board.sender_id() && row.healthy);
                    readonly_query_candidate(
                        seen,
                        endpoint_verified(state, board, &topology),
                        inner.values[at].as_ref(),
                        inner.queries[at].as_ref(),
                        now,
                    ) && readonly_query_due(self.last[at], now.wall)
                })
        }?;
        self.cursor = (candidate + 1) % BOARDS.len();
        let board = BOARDS[candidate];
        self.request(state, board).then_some(board)
    }
}
fn readonly_query_due(last: Option<WallInstant>, now: WallInstant) -> bool {
    last.is_none_or(|last| {
        now.checked_duration_since(last)
            .is_some_and(|age| age >= Duration::from_secs(2))
    })
}
fn readonly_query_candidate(
    healthy: bool,
    verified: bool,
    row: Option<&Observation>,
    query: Option<&Query>,
    now: Stamp,
) -> bool {
    healthy
        && verified
        && row.is_none_or(|row| row.age(now) >= Duration::from_secs(2))
        && !query.is_some_and(|query| {
            !query.received && now.checked_duration_since(query.sent)
                .is_none_or(|age| age <= QUERY_TTL)
        })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueryRequest {
    board: String,
}
pub(crate) async fn query(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<QueryRequest>,
) -> Response {
    if let Err(response) =
        crate::web::authorize_headers(&state, &headers, Permission::ViewData).await
    {
        return response;
    }
    if let Err(response) = primary_only() {
        return response;
    }
    let Some(board) = Board::from_sender_id(&request.board).filter(|board| index(*board).is_some())
    else {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_target",
            "Select one known board.",
        );
    };
    request_board_query(&state, board)
}
/// Internal read-only collection shares EXACT HTTP nonce/correlation/budget rules.
/// It cannot emit RECOVER, change board state or bypass primary identity checks.
pub(crate) fn request_board_query(state: &Arc<AppState>, board: Board) -> Response {
    if let Err(response) = primary_only() {
        return response;
    }
    if index(board).is_none() {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_target",
            "Select one known board",
        );
    }
    let Some(identity) = state.board_recovery.identity else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "identity_unavailable",
            "Backend recovery identity unavailable.",
        );
    };
    let Some(router) = state.topology_router.get() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "router_unavailable",
            "Telemetry router unavailable.",
        );
    };
    if router.sender().as_ref() != "GS" {
        return error(
            StatusCode::CONFLICT,
            "primary_required",
            "Recovery router identity is not the primary GS.",
        );
    }
    if !endpoint_verified(state, board, &router.export_topology()) {
        return error(
            StatusCode::CONFLICT,
            "endpoint_unverified",
            "Dedicated board endpoint not verified; legacy command endpoints are never probed.",
        );
    }
    let mut bytes = [0; 8];
    if SystemRandom::new().fill(&mut bytes).is_err() || bytes == [0; 8] {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "entropy_unavailable",
            "Query nonce unavailable.",
        );
    }
    let nonce = u64::from_le_bytes(bytes);
    let abort_revision = state.board_abort.query_revision(board);
    let at = index(board).unwrap();
    let now = Stamp::now();
    let mut inner = state.board_recovery.inner.lock().unwrap();
    if inner.queries[at]
        .as_ref()
        .is_some_and(|query| query.nonce == nonce)
        || inner.values[at]
            .as_ref()
            .is_some_and(|row| row.value.nonce == nonce)
    {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "entropy_duplicate",
            "Query nonce duplicated its live context; no request was queued.",
        );
    }
    if inner.queries[at].as_ref().is_some_and(|query| {
        !query.received
            && now
                .checked_duration_since(query.sent)
                .is_some_and(|age| age <= QUERY_TTL)
    }) {
        return error(
            StatusCode::CONFLICT,
            "query_pending",
            "This board already has an outstanding query; no duplicate request was queued.",
        );
    }
    let mut wire = [0; 32];
    wire[0] = 1;
    wire[4..12].copy_from_slice(&bytes);
    wire[24..32].copy_from_slice(&issuer(&identity).to_le_bytes());
    // QUERY remains inspectable before network UTC synchronization. RECOVER
    // must use required_control_network_utc_ms; this fallback is read-only only.
    let packet = match Packet::new(
        crate::telemetry_schema::data_type(QUERY_TYPE),
        &[crate::telemetry_schema::endpoint(endpoint(board))],
        &router.sender(),
        crate::system_clock::control_packet_timestamp_ms(router),
        Arc::from(wire),
    ) {
        Ok(packet) => packet,
        Err(_) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "packet_rejected",
                "Query packet preparation failed.",
            );
        }
    };
    // Install correlation before SDK queue admission. tx_queue never invokes callbacks.
    let previous = inner.queries[at].take();
    inner.queries[at] = Some(Query {
        abort_revision,
        nonce,
        sent: now,
        received: false,
    });
    if router.tx_queue(packet).is_err() {
        inner.queries[at] = previous;
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "queue_rejected",
            "SDK did not accept query ownership.",
        );
    }
    drop(inner);
    Json(json!({"accepted":true,"operation":null,"message":"Query admitted to SDK; fresh board readback is still required."})).into_response()
}/// Seal new recovery proposals/confirmation atomically before extracting its
/// cancellation-independent owner. Main must await outside coordinator locks.
pub(crate) fn seal_shutdown_and_take_owner(state: &AppState) -> Option<tokio::task::JoinHandle<()>> {
    state.board_recovery.coordinator.seal_shutdown_and_take_owner()
}
pub(crate) fn shutdown_clean_eligible(state: &AppState) -> bool {
    state.board_recovery.coordinator.shutdown_clean_eligible()
}



/// Read-only test observation of the actual production nonce/correlation owner.
#[cfg(test)]
pub(crate) fn query_nonce_for_test(state:&AppState,board:Board)->Option<u64>{
    state.board_recovery.inner.lock().unwrap().queries[index(board)?].as_ref().map(|query|query.nonce)
}
#[cfg(test)]
pub(crate) fn query_reply_current_for_test(state:&AppState,board:Board,nonce:u64)->bool{
    let Some(at)=index(board)else{return false;};
    let inner=state.board_recovery.inner.lock().unwrap();
    let(Some(query),Some(row))=(&inner.queries[at],&inner.values[at])else{return false;};
    query.nonce==nonce&&query.received&&row.value.nonce==nonce&&row.value.result==0&&row.fresh(Stamp::now())
}

#[cfg(all(test, feature = "testing", not(any(feature = "hitl_mode", feature = "test_fire_mode"))))]
#[path = "board_recovery/testing_model_tests.rs"]
mod testing_model_tests;
#[cfg(all(test, feature="testing", not(any(feature="hitl_mode",feature="test_fire_mode"))))]
pub(crate) use testing_model_tests::{ReadyIgnitionFixture,ready_ignition_fixture_for_test};

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) struct HttpFixture {
        pub(super) state: Arc<AppState>,
        pub(super) directory: std::path::PathBuf,
    }
    impl Drop for HttpFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }
    pub(super) async fn http_fixture() -> HttpFixture {
        let directory = std::env::temp_dir().join(format!(
            "board-query-http-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("auth.json");
        std::fs::write(&path, json!({"users":[
            {"username":"viewer","roles":[],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true,"send_commands":false}},
            {"username":"operator","roles":[],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true,"send_commands":true}}
        ]}).to_string()).unwrap();
        let mut state = crate::state::tests::test_app_state().await;
        Arc::get_mut(&mut state).unwrap().auth = Arc::new(crate::auth::AuthManager::new(path));
        Arc::get_mut(&mut state).unwrap().auth_db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::ensure_auth_sessions_table(&state.auth_db)
            .await
            .unwrap();
        for (user, commands) in [("viewer", 0), ("operator", 1)] {
            sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES(?,?,'session',1,?,0,9999999999999)")
                .bind(user).bind(user).bind(commands).execute(&state.auth_db).await.unwrap();
        }
        HttpFixture { state, directory }
    }
    pub(super) async fn http(
        state: Arc<AppState>,
        method: &str,
        path: &str,
        token: Option<&str>,
        value: Value,
    ) -> Response {
        use tower::ServiceExt;
        let mut request = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        routes()
            .route(
                "/api/board_abort",
                axum::routing::get(crate::board_abort::get_status),
            )
            .route(
                "/api/board_abort/refresh",
                axum::routing::post(crate::board_abort::refresh_status),
            )
            .with_state(state)
            .oneshot(
                request
                    .body(axum::body::Body::from(value.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }
    #[tokio::test]
    async fn actual_http_routes_require_auth_and_healthy_durable_context() {
        let fixture = http_fixture().await;
        let state = fixture.state.clone();
        assert_eq!(
            http(
                state.clone(),
                "GET",
                "/api/board-recovery/status",
                None,
                Value::Null
            )
            .await
            .status(),
            StatusCode::UNAUTHORIZED
        );
        let response = http(
            state.clone(),
            "GET",
            "/api/board-recovery/status",
            Some("viewer"),
            Value::Null,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["permissions"]["reset_startup"], false);
        assert!(
            state
                .board_recovery
                .inner
                .lock()
                .unwrap()
                .queries
                .iter()
                .all(Option::is_none)
        );
        assert!(
            state.topology_router.get().is_none(),
            "GET is local cache only"
        );
        for path in ["/api/board-recovery/prepare", "/api/board-recovery/confirm"] {
            assert_eq!(
                http(state.clone(), "POST", path, Some("viewer"), json!({}))
                    .await
                    .status(),
                StatusCode::FORBIDDEN
            );
            assert_eq!(
                http(state.clone(), "POST", path, Some("operator"), json!({}))
                    .await
                    .status(),
                StatusCode::CONFLICT
            );
        }
        assert!(
            state
                .board_recovery
                .inner
                .lock()
                .unwrap()
                .queries
                .iter()
                .all(Option::is_none)
        );
    }
    #[tokio::test]
    async fn actual_http_query_never_probes_legacy_endpoints_or_rrls_authority() {
        let fixture = http_fixture().await;
        let state = fixture.state.clone();
        let router = Arc::new(sedsnet::router::Router::new(
            sedsnet::router::RouterConfig::new([]),
        ));
        router.set_sender("GS");
        assert!(state.topology_router.set(router.clone()).is_ok());
        assert_eq!(
            http(
                state.clone(),
                "POST",
                "/api/board-recovery/query",
                Some("viewer"),
                json!({"board":"AB"})
            )
            .await
            .status(),
            StatusCode::CONFLICT
        );
        assert!(
            state
                .board_recovery
                .inner
                .lock()
                .unwrap()
                .queries
                .iter()
                .all(Option::is_none)
        );
        assert!(
            !router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type(QUERY_TYPE))
        );
        router.set_sender("RRLS");
        let response = http(
            state.clone(),
            "POST",
            "/api/board-recovery/query",
            Some("operator"),
            json!({"board":"AB"}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"], "primary_required");
        assert!(
            !router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type(QUERY_TYPE))
        );
    }
    #[tokio::test]
    async fn actual_http_query_reaches_only_dedicated_sdk_owner_and_correlates_readback() {
        dedicated_sdk_query(false).await;
    }
    #[tokio::test]
    async fn automatic_unknown_healthy_query_reaches_only_dedicated_owner_without_recover() {
        dedicated_sdk_query(true).await;
    }
    async fn dedicated_sdk_query(automatic: bool) {
        use sedsnet::router::{EndpointHandler, Router, RouterConfig, RouterSideOptions};
        use std::sync::atomic::{AtomicU64, Ordering};
        let fixture = http_fixture().await;
        let state = fixture.state.clone();
        let clock = Arc::new(AtomicU64::new(0));
        let weak = Arc::downgrade(&state);
        let tick = clock.clone();
        let host = Arc::new(Router::new_with_clock(
            RouterConfig::new([EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint("GROUND_STATION"),
                move |packet| {
                    receive_status(&weak.upgrade().unwrap(), packet);
                    Ok(())
                },
            )])
            .with_sender("GS"),
            Box::new(move || tick.load(Ordering::Relaxed)),
        ));
        assert!(state.topology_router.set(host.clone()).is_ok());
        let requests = Arc::new(Mutex::new(Vec::<Packet>::new()));
        let captured = requests.clone();
        let tick = clock.clone();
        let dedicated = crate::telemetry_schema::endpoint("BOARD_RECOVERY_AB");
        let board = Router::new_with_clock(
            RouterConfig::new([EndpointHandler::new_packet_handler(
                dedicated,
                move |packet| {
                    captured.lock().unwrap().push(packet.clone());
                    Ok(())
                },
            )])
            .with_sender("AB"),
            Box::new(move || tick.load(Ordering::Relaxed)),
        );
        let outgoing = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let output = outgoing.clone();
        let host_side = host.add_side_packed_with_options(
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
        let incoming = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let output = incoming.clone();
        let board_side = board.add_side_packed_with_options(
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
            &sedsnet::discovery::build_discovery_announce("AB", 1, &[dedicated]).unwrap(),
            host_side,
        )
        .unwrap();
        board
            .rx_from_side(
                &sedsnet::discovery::build_discovery_announce(
                    "GS",
                    1,
                    &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                )
                .unwrap(),
                board_side,
            )
            .unwrap();
        assert!(endpoint_verified(
            &state,
            Board::ActuatorBoard,
            &host.export_topology()
        ));
        let mut refresh = ReadonlyQueryRefresh::default();
        if automatic {
            // Unseen or unverified targets are never probed.
            assert_eq!(refresh.poll(&state), None);
            state
                .network_health
                .lock()
                .unwrap()
                .observe("GB", WallInstant::now(), 100);
            assert_eq!(refresh.poll(&state), None);
            state
                .network_health
                .lock()
                .unwrap()
                .observe("AB", WallInstant::now(), 100);
            assert_eq!(refresh.poll(&state), Some(Board::ActuatorBoard));
            assert_eq!(refresh.poll(&state), None); // Pending request / shared cooldown.
        } else {
            let response = http(
                state.clone(),
                "POST",
                "/api/board-recovery/query",
                Some("viewer"),
                json!({"board":"AB"}),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
        }
        // This SDK-only receiver fixture checks application wire/correlation;
        // firmware output/NVM behavior is qualified separately in board C tests.
        let mut responded = false;
        for now in 0..100 {
            clock.store(now, Ordering::Relaxed);
            host.process_all_queues_with_timeout(0).unwrap();
            for packet in std::mem::take(&mut *outgoing.lock().unwrap()) {
                board.rx_packed_from_side(&packet, board_side).unwrap();
            }
            board.process_all_queues_with_timeout(0).unwrap();
            if !responded {
                let captured = requests.lock().unwrap();
                if let Some(packet) = captured.first() {
                    assert_eq!(packet.sender(), "GS");
                    assert_eq!(packet.endpoints(), [dedicated]);
                    assert_eq!(
                        packet.data_type(),
                        crate::telemetry_schema::data_type(QUERY_TYPE)
                    );
                    let payload = packet.payload();
                    assert_eq!(payload.len(), 32);
                    assert_eq!(&payload[..4], &[1, 0, 0, 0]);
                    assert!(payload[12..24].iter().all(|&byte| byte == 0));
                    let nonce = u64::from_le_bytes(payload[4..12].try_into().unwrap());
                    assert_ne!(nonce, 0);
                    assert_eq!(
                        &payload[24..32],
                        &issuer(&state.board_recovery.identity.unwrap()).to_le_bytes()
                    );
                    board
                        .tx_queue(status_packet("AB", status_wire(&state, nonce)))
                        .unwrap();
                    responded = true;
                }
            }
            for packet in std::mem::take(&mut *incoming.lock().unwrap()) {
                host.rx_packed_from_side(&packet, host_side).unwrap();
            }
        }
        assert!(responded);
        assert_eq!(requests.lock().unwrap().len(), 1);
        if automatic {
            assert_eq!(refresh.poll(&state), None);
            assert!(state.board_recovery.coordinator.operation().is_none());
            assert!(
                state
                    .launch_history
                    .lock()
                    .unwrap()
                    .recovery_pending()
                    .is_none()
            );
            // Closed/pending operation owners exclude the same background poll.
            let _ = seal_shutdown_and_take_owner(&state);
            assert_eq!(refresh.poll(&state), None);
            assert_eq!(requests.lock().unwrap().len(), 1);
        }
        assert!(!host.has_pending_reliable_tx_type(crate::telemetry_schema::data_type(QUERY_TYPE)));
        let inner = state.board_recovery.inner.lock().unwrap();
        let at = index(Board::ActuatorBoard).unwrap();
        let readback = inner.values[at].as_ref().unwrap();
        assert_eq!(readback.value.boot, 2);
        assert_eq!(readback.value.epoch, 3);
        assert!(inner.queries[at].as_ref().unwrap().received);
    }
    fn status_wire(state: &AppState, nonce: u64) -> [u8; 40] {
        let mut wire = [0; 40];
        wire[0] = 1;
        wire[2] = 0;
        wire[3] = STATE_VALID | 4 | OUTPUTS_VALID | 2;
        wire[4..12].copy_from_slice(&nonce.to_le_bytes());
        wire[12..20].copy_from_slice(&2u64.to_le_bytes());
        wire[20..24].copy_from_slice(&3u32.to_le_bytes());
        wire[24..28].copy_from_slice(&99u32.to_le_bytes());
        wire[32..40]
            .copy_from_slice(&issuer(&state.board_recovery.identity.unwrap()).to_le_bytes());
        wire
    }
    fn status_packet(sender: &str, wire: [u8; 40]) -> Packet {
        Packet::new(
            crate::telemetry_schema::data_type(STATUS_TYPE),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            sender,
            123,
            Arc::from(wire),
        )
        .unwrap()
    }
    #[tokio::test]
    async fn hardened_transport_bit_is_readonly_and_unknown_flags_remain_rejected() {
        let fixture = http_fixture().await;
        let state = fixture.state.clone();
        let host = Arc::new(sedsnet::router::Router::new(
            sedsnet::router::RouterConfig::new([]).with_sender("GS"),
        ));
        let side = host.add_side_packed("wire", |_| Ok(()));
        let dedicated = crate::telemetry_schema::endpoint("BOARD_RECOVERY_AB");
        host.rx_from_side(
            &sedsnet::discovery::build_discovery_announce("AB", 1, &[dedicated]).unwrap(),
            side,
        )
        .unwrap();
        state.topology_router.set(host).unwrap();
        let at = index(Board::ActuatorBoard).unwrap();
        state.board_recovery.inner.lock().unwrap().queries[at] = Some(Query {
            abort_revision: 0,
            nonce: 7,
            sent: Stamp::now() - Duration::from_millis(100),
            received: false,
        });
        let mut wire = status_wire(&state, 7);
        wire[3] |= 0x20;
        assert!(Readback::decode(&wire).is_some());
        let mut unknown = wire;
        unknown[3] |= 0x80;
        assert!(Readback::decode(&unknown).is_none());
        wire[3] |= 0x40;
        let mut metadata = wire;
        metadata[2] = 255;
        metadata[3] = 0x64;
        assert_eq!(
            observe(&state, &status_packet("DAQ", metadata)),
            Err("Metadata node advertised physical manual Stop capability")
        );
        observe(&state, &status_packet("AB", wire)).unwrap();
        let response = http(
            state.clone(),
            "GET",
            "/api/board-recovery/status",
            Some("viewer"),
            Value::Null,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let board = body["boards"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == "AB")
            .unwrap();
        let remaining = board["readback_valid_for_ms"].as_u64().unwrap();
        assert!(remaining > 0 && remaining <= 4_900, "query age, not fresh receipt, spends the server lease");
        assert!(body["boards"].as_array().unwrap().iter()
            .find(|row| row["id"] == "RF").unwrap()["readback_valid_for_ms"].is_null());
        assert_eq!(board["capabilities"]["hardened_transport"], true);
        assert_eq!(board["capabilities"]["known_utc_manual_stop"], true);
        for capability in ["recover", "resync", "reset_startup"] {
            assert_eq!(board["capabilities"][capability], false);
        }
        state.board_recovery.inner.lock().unwrap().values[at]
            .as_mut()
            .unwrap()
            .observed_after = Stamp::now() - Duration::from_secs(6);
        let response = http(
            state.clone(),
            "GET",
            "/api/board-recovery/status",
            Some("viewer"),
            Value::Null,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let stale: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let board = stale["boards"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == "AB")
            .unwrap();
        assert_eq!(board["readback_valid_for_ms"], 0);
        assert_eq!(board["capabilities"]["hardened_transport"], false);
        // Old transports stay readable and never gain the new implementation
        // capability merely because a fresh dedicated endpoint exists.
        state.board_recovery.inner.lock().unwrap().queries[at] = Some(Query {
            abort_revision: 0,
            nonce: 8,
            sent: Stamp::now(),
            received: false,
        });
        observe(&state, &status_packet("AB", status_wire(&state, 8))).unwrap();
        let response = http(
            state.clone(),
            "GET",
            "/api/board-recovery/status",
            Some("viewer"),
            Value::Null,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let legacy: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let board = legacy["boards"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == "AB")
            .unwrap();
        assert!(board["age_ms"].as_u64().unwrap() <= 2000);
        assert_eq!(board["capabilities"]["hardened_transport"], false);
    }
    #[tokio::test]
    async fn strict_status_shape_flags_and_identity_never_invent_zero_context() {
        let state = crate::state::tests::test_app_state().await;
        let wire = status_wire(&state, 1);
        let decoded = Readback::decode(&wire).unwrap();
        assert_eq!(
            (decoded.boot, decoded.epoch, decoded.uptime, decoded.state),
            (2, 3, 99, Some(0))
        );
        for length in 0..40 {
            assert!(Readback::decode(&wire[..length]).is_none());
        }
        for (at, bad) in [(0, 2), (1, 8), (2, 254), (3, 64), (3, 2)] {
            let mut badwire = wire;
            badwire[at] = bad;
            assert!(Readback::decode(&badwire).is_none());
        }
        for at in [4, 32] {
            let mut bad = wire;
            bad[at..at + 8].fill(0);
            assert!(Readback::decode(&bad).is_none());
        }
        let mut unknown = wire;
        unknown[12..24].fill(0);
        unknown[2] = 255;
        unknown[3] = 0;
        let decoded = Readback::decode(&unknown).unwrap();
        assert_eq!((decoded.boot, decoded.epoch, decoded.state), (0, 0, None));
    }
    #[tokio::test]
    async fn readback_requires_actual_sender_pending_nonce_issuer_and_fixed_age() {
        let state = crate::state::tests::test_app_state().await;
        let at = index(Board::ActuatorBoard).unwrap();
        let packet = status_packet("AB", status_wire(&state, 7));
        assert!(observe(&state, &packet).is_err());
        state.board_recovery.inner.lock().unwrap().queries[at] = Some(Query {
            abort_revision: 0,
            nonce: 7,
            sent: Stamp::now(),
            received: false,
        });
        assert!(observe(&state, &status_packet("VB", status_wire(&state, 7))).is_err());
        assert!(observe(&state, &status_packet("AB", status_wire(&state, 8))).is_err());
        let mut wrong = status_wire(&state, 7);
        wrong[32] ^= 1;
        assert!(observe(&state, &status_packet("AB", wrong)).is_err());
        assert!(state.board_recovery.inner.lock().unwrap().values[at].is_none());
        let mut pending = status_wire(&state,7); pending[1]=1;
        observe(&state,&status_packet("AB",pending)).unwrap();
        {
            let inner=state.board_recovery.inner.lock().unwrap();
            assert!(!inner.queries[at].as_ref().unwrap().received,"Interim PENDING cannot finish QUERY");
            assert!(inner.values[at].is_none(),"PENDING is not a confirmed state");
        }
        observe(&state, &packet).unwrap();
        let first = state.board_recovery.inner.lock().unwrap().values[at]
            .as_ref()
            .unwrap()
            .received;
        observe(&state, &status_packet("AB", status_wire(&state, 7))).unwrap();
        assert_eq!(
            state.board_recovery.inner.lock().unwrap().values[at]
                .as_ref()
                .unwrap()
                .received,
            first,
            "Duplicate status must not slide freshness"
        );
        state.board_recovery.inner.lock().unwrap().queries[at] = Some(Query {
            abort_revision:0,nonce:8,sent:Stamp::now(),received:false,
        });
        let mut pending=status_wire(&state,8);pending[1]=1;
        observe(&state,&status_packet("AB",pending)).unwrap();
        {
            let inner=state.board_recovery.inner.lock().unwrap();
            assert_eq!(inner.values[at].as_ref().unwrap().received,first,"PENDING neither erases nor refreshes prior confirmed proof");
            assert!(!inner.queries[at].as_ref().unwrap().received);
        }
        observe(&state,&status_packet("AB",status_wire(&state,8))).unwrap();
        assert!(state.board_recovery.inner.lock().unwrap().queries[at].as_ref().unwrap().received);
        state.board_recovery.inner.lock().unwrap().queries[at] = Some(Query {
            abort_revision: 0,
            nonce: 9,
            sent: Stamp::now() - QUERY_TTL - Duration::from_millis(1),
            received: false,
        });
        assert!(observe(&state, &status_packet("AB", status_wire(&state, 9))).is_err());
    }
    #[tokio::test]
    async fn connected_board_display_never_renews_mutating_proof_or_accepts_pending_identity() {
        let state=crate::state::tests::test_app_state().await;
        let at=index(Board::ActuatorBoard).unwrap();
        state.network_health.lock().unwrap().observe("AB",WallInstant::now(),100);
        state.board_recovery.inner.lock().unwrap().queries[at]=Some(Query{abort_revision:0,nonce:71,sent:Stamp::now(),received:false});
        observe(&state,&status_packet("AB",status_wire(&state,71))).unwrap();
        {
            let mut inner=state.board_recovery.inner.lock().unwrap();
            let row=inner.values[at].as_mut().unwrap();row.observed_after=Stamp::now()-Duration::from_secs(30);
            assert!(!row.fresh(Stamp::now()));
        }
        let display=sequence_board_display(&state);
        assert!(display["AB"].connected&&display["AB"].known);
        assert_eq!(display["AB"].outputs,Some(0));
        assert!(display["AB"].outputs_age_ms.is_some_and(|age|age>=30000),"display retains the ORIGINAL QUERY stamp instead of making old registers look new");
        assert!(!display["VB"].known);
        let json=serde_json::to_value(display).unwrap();assert_eq!(json["AB"]["boot"],"0000000000000002");
        state.board_recovery.inner.lock().unwrap().queries[at]=Some(Query{abort_revision:0,nonce:72,sent:Stamp::now(),received:false});
        let mut pending=status_wire(&state,72);pending[1]=1;pending[12..20].copy_from_slice(&4u64.to_le_bytes());
        observe(&state,&status_packet("AB",pending)).unwrap();
        assert!(!sequence_board_display(&state)["AB"].known,"new boot invalidates retained presentation immediately");
        assert!(state.board_recovery.inner.lock().unwrap().values[at].is_none());
    }
    #[tokio::test]
    async fn honest_io_readback_keeps_actual_identity_for_manual_repair_without_claiming_success() {
        let state = crate::state::tests::test_app_state().await;
        let at = index(Board::ActuatorBoard).unwrap();
        state.board_recovery.inner.lock().unwrap().queries[at] = Some(Query {
            abort_revision: 0,
            nonce: 1,
            sent: Stamp::now(),
            received: false,
        });
        let mut wire = status_wire(&state, 1);
        wire[1] = 7;
        wire[2] = 15;
        wire[3] |= 1;
        observe(&state, &status_packet("AB", wire)).unwrap();
        let inner = state.board_recovery.inner.lock().unwrap();
        let row = &inner.values[at].as_ref().unwrap().value;
        assert_eq!(
            (row.result, row.boot, row.epoch, row.state),
            (7, 2, 3, Some(15))
        );
    }
    #[tokio::test]
    async fn late_status_is_acked_without_mutation_and_does_not_block_next_valid_reply() {
        use sedsnet::router::{EndpointHandler, Router, RouterConfig, RouterSideOptions};
        use std::sync::atomic::{AtomicU64, Ordering};
        let state = crate::state::tests::test_app_state().await;
        let clock = Arc::new(AtomicU64::new(0));
        let receiving = state.clone();
        let tick = clock.clone();
        let host = Router::new_with_clock(
            RouterConfig::new([EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint("GROUND_STATION"),
                move |packet| {
                    receive_status(&receiving, packet);
                    Ok(())
                },
            )])
            .with_sender("GS"),
            Box::new(move || tick.load(Ordering::Relaxed)),
        );
        let tick = clock.clone();
        let board = Router::new_with_clock(
            RouterConfig::new([]).with_sender("AB"),
            Box::new(move || tick.load(Ordering::Relaxed)),
        );
        let outbound = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let output = outbound.clone();
        let board_side = board.add_side_packed_with_options(
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
        let replies = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let output = replies.clone();
        let host_side = host.add_side_packed_with_options(
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
        board
            .rx_from_side(
                &sedsnet::discovery::build_discovery_announce(
                    "GS",
                    1,
                    &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                )
                .unwrap(),
                board_side,
            )
            .unwrap();
        host.rx_from_side(
            &sedsnet::discovery::build_discovery_announce("AB", 1, &[]).unwrap(),
            host_side,
        )
        .unwrap();
        let status_type = crate::telemetry_schema::data_type(STATUS_TYPE);
        for (round, nonce) in [8, 9].into_iter().enumerate() {
            if round == 1 {
                state.board_recovery.inner.lock().unwrap().queries
                    [index(Board::ActuatorBoard).unwrap()] = Some(Query {
                    abort_revision: 0,
                    nonce,
                    sent: Stamp::now(),
                    received: false,
                });
            }
            board
                .tx_queue(status_packet("AB", status_wire(&state, nonce)))
                .unwrap();
            for tick in 0..100 {
                clock.store((round as u64) * 100 + tick, Ordering::Relaxed);
                board.process_all_queues_with_timeout(0).unwrap();
                for packet in std::mem::take(&mut *outbound.lock().unwrap()) {
                    host.rx_packed_from_side(&packet, host_side).unwrap();
                }
                host.process_all_queues_with_timeout(0).unwrap();
                for packet in std::mem::take(&mut *replies.lock().unwrap()) {
                    board.rx_packed_from_side(&packet, board_side).unwrap();
                }
            }
            assert!(
                !board.has_pending_reliable_tx_type(status_type),
                "Even a semantically obsolete DATA status must retire its transport owner"
            );
            let inner = state.board_recovery.inner.lock().unwrap();
            assert_eq!(
                inner.values[index(Board::ActuatorBoard).unwrap()]
                    .as_ref()
                    .map(|row| row.value.nonce),
                if round == 0 { None } else { Some(9) }
            );
        }
        let inner = state.board_recovery.inner.lock().unwrap();
        assert!(inner.rejected_statuses >= 1);
        assert!(inner.last_rejection.is_some());
    }
    #[tokio::test]
    async fn proactive_readonly_refresh_preserves_five_second_proof_and_pending_nonce() {
        let state = crate::state::tests::test_app_state().await;
        let start = Stamp::now();
        let row = Observation {
            value: Readback::decode(&status_wire(&state, 44)).unwrap(),
            received: start,
            observed_after: start,
        };
        let answered = Query { abort_revision: 0, nonce: 44, sent: start, received: true };
        let pending = Query { received: false, ..answered.clone() };
        let at = start + Duration::from_secs(2);
        assert!(row.fresh(at));
        assert!(readonly_query_candidate(true, true, Some(&row), Some(&answered), at));
        assert!(!readonly_query_candidate(true, true, Some(&row), Some(&pending), at));
        assert!(!readonly_query_candidate(false, true, Some(&row), Some(&answered), at));
        assert!(!readonly_query_candidate(true, false, Some(&row), Some(&answered), at));
        assert!(!readonly_query_candidate(true, true, Some(&row), Some(&answered), start + Duration::from_millis(1_999)));
        assert!(!readonly_query_due(Some(start.wall), start.wall + Duration::from_millis(1_999)));
        assert!(readonly_query_due(Some(start.wall), at.wall));
        assert!(row.fresh(start + Duration::from_secs(5)));
        assert!(!row.fresh(start + Duration::from_millis(5_001)));
        assert_eq!(row.observed_after, start);
    }

    #[tokio::test]
    async fn delayed_reply_cannot_become_fresh_merely_by_arriving_now() {
        let state = crate::state::tests::test_app_state().await;
        let at = index(Board::ActuatorBoard).unwrap();
        let sent = Stamp::now() - READBACK_TTL - Duration::from_millis(10);
        state.board_recovery.inner.lock().unwrap().queries[at] = Some(Query {
            abort_revision: 0,
            nonce: 44,
            sent,
            received: false,
        });
        assert!(observe(&state, &status_packet("AB", status_wire(&state, 44))).is_err());
        let inner = state.board_recovery.inner.lock().unwrap();
        assert!(inner.values[at].is_none());
    }
    #[tokio::test]
    async fn regressed_monotonic_context_cannot_admit_a_fresh_status() {
        let state = crate::state::tests::test_app_state().await;
        let at = index(Board::ActuatorBoard).unwrap();
        state.board_recovery.inner.lock().unwrap().queries[at] = Some(Query {
            abort_revision: 0,
            nonce: 45,
            sent: Stamp::now() + Duration::from_secs(1),
            received: false,
        });
        receive_status(&state, &status_packet("AB", status_wire(&state, 45)));
        let inner = state.board_recovery.inner.lock().unwrap();
        assert!(inner.values[at].is_none());
        assert_eq!(inner.rejected_statuses, 1);
    }
    #[test]
    fn explicit_model_clock_ignores_wall_speed_but_rejects_rollback_or_domain_change() {
        let wall = WallInstant::now();
        let start = Stamp {
            wall,
            model_ms: Some(100),
        };
        let slow_wall = Stamp {
            wall: wall + Duration::from_secs(20),
            model_ms: Some(600),
        };
        assert_eq!(
            slow_wall.checked_duration_since(start),
            Some(Duration::from_millis(500))
        );
        assert_eq!(
            Stamp {
                wall,
                model_ms: Some(99)
            }
            .checked_duration_since(start),
            None
        );
        assert_eq!(
            Stamp {
                wall,
                model_ms: None
            }
            .checked_duration_since(start),
            None
        );
        let real = Stamp {
            wall,
            model_ms: None,
        };
        assert_eq!(
            Stamp {
                wall: wall + Duration::from_millis(500),
                model_ms: None
            }
            .checked_duration_since(real),
            Some(Duration::from_millis(500))
        );
    }
}

#[cfg(test)]
mod final_clock_freshness_tests {
    use super::*;
    fn observation(at: Stamp) -> Observation {
        Observation { received: at, observed_after: at, value: Readback {
            result:0,state:Some(0),flags:0x2c,nonce:1,boot:2,epoch:3,
            uptime:4,outputs:0,issuer:5,
        } }
    }
    #[test]
    fn five_second_freshness_preserves_paused_model_and_rejects_domain_loss() {
        let wall=WallInstant::now();
        let at=Stamp {wall:wall-Duration::from_secs(60),model_ms:Some(1000)};
        let row=observation(at);
        assert!(row.fresh(Stamp {wall,model_ms:Some(1000)}));
        assert!(row.fresh(Stamp {wall,model_ms:Some(6000)}));
        assert!(!row.fresh(Stamp {wall,model_ms:Some(6001)}));
        assert!(!row.fresh(Stamp {wall,model_ms:Some(999)}));
        assert!(!row.fresh(Stamp {wall,model_ms:None}));
        let wall_row=observation(Stamp {wall:wall-Duration::from_secs(6),model_ms:None});
        assert!(!wall_row.fresh(Stamp {wall,model_ms:None}));
    }
}

#[cfg(test)]
#[path = "board_recovery/status_policy_tests.rs"]
mod status_policy_tests;
