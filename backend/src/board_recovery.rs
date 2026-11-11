//! Verified application-state recovery with explicit authenticated intent.
//! SDK ACKs certify delivery ownership, never board state or physical outputs.
pub(crate) mod abort_confirm;
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
const READBACK_TTL: Duration = Duration::from_secs(15);
/// Test override (ms) for READBACK_TTL where a board's readback is judged
/// fresh, so proof-ageing paths run in seconds.
#[cfg(test)]
static READBACK_TTL_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn readback_ttl() -> Duration {
    #[cfg(test)]
    {
        let ms = READBACK_TTL_MS.load(std::sync::atomic::Ordering::Relaxed);
        if ms != 0 {
            return Duration::from_millis(ms);
        }
    }
    READBACK_TTL
}
const QUERY_TTL: Duration = Duration::from_secs(15);
/// An unanswered read-only query may be re-issued after this long (the
/// superseded one stays acceptable as `previous`), so one lost reply costs a
/// few seconds of board-state freshness instead of a whole QUERY_TTL.
const QUERY_RETRY: Duration = Duration::from_secs(3);
fn query_retry() -> Duration {
    crate::sim_freshness::wall(QUERY_RETRY)
}
/// Actuator motion/configuration gates keep their own 5 s proof age. The
/// longer recovery windows above exist for slow links, not for these gates.
const CONTROL_PROOF_TTL: Duration = Duration::from_secs(5);
const QUERY_TYPE: &str = "BOARD_RECOVERY_REQUEST";
const STATUS_TYPE: &str = "BOARD_STATE_STATUS";
const STATE_VALID: u8 = 8;
const OUTPUTS_VALID: u8 = 16;
/// Readback flag: the board accepts RESYNC (operation 2) to a station phase.
const RESYNC_IMPLEMENTATION: u8 = 0x80;
/// Wire operations. RESYNC carries the station phase in the desired byte.
const OP_RECOVER: u8 = 1;
const OP_RESYNC: u8 = 2;

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
        Self::decode_wire(bytes).filter(|readback| readback.nonce != 0 && readback.issuer != 0)
    }
    /// The wire shape only. Nonce 0 is an Abort push (`observe_abort_push`);
    /// every query reply carries a nonzero nonce and issuer.
    fn decode_wire(bytes: &[u8]) -> Option<Self> {
        // Every flag bit is defined; bit 7 advertises Resync (AB/VB only).
        if bytes.len() != 40 || bytes[0] != 1 || bytes[1] > 7 {
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
        self.age(now) <= readback_ttl()
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
    /// The query a background or operator re-query superseded while it was
    /// still unanswered. Its reply, if it arrives within QUERY_TTL and before
    /// the current query is answered, still counts (with its own, older send
    /// time), so a slow link never turns every reply into a rejection.
    previous: [Option<Query>; 7],
    rejected_statuses: u64,
    last_rejection: Option<&'static str>,
    /// Latest Abort push accepted from each board: (boot generation, abort
    /// epoch, uptime ms). Within one boot a push is new only if it is later by
    /// (epoch, uptime); anything else is a replay.
    pushed: [Option<(u64, u32, u32)>; 7],
}
pub(crate) struct Runtime {
    identity: Option<[u8; 16]>,
    coordinator: coordinator::Runtime,
    inner: Mutex<Inner>,
    pub(crate) manual_utc: crate::manual_abort_utc::Runtime,
    /// When the station last asked each board (by `index`) to apply a recover
    /// or resync, in station wall ms. A board may briefly stop sampling while
    /// it drives its outputs safe; its warnings from that window are not faults.
    apply_requested_ms: [std::sync::atomic::AtomicU64; 7],
    /// Why background AV-bay status queries are paused, while they are
    /// (`ReadonlyQueryRefresh`); shown on the AV-bay rows and in the status.
    av_bay_paused: Mutex<Option<&'static str>>,
}
impl Runtime {
    pub(crate) fn av_bay_pause_reason(&self) -> Option<&'static str> {
        *self.av_bay_paused.lock().unwrap()
    }
    pub(crate) fn note_apply_requested(&self, board: Board) {
        if let Some(i) = index(board) {
            self.apply_requested_ms[i].store(
                crate::telemetry_task::get_current_timestamp_ms(),
                std::sync::atomic::Ordering::Release,
            );
        }
    }
    pub(crate) fn apply_requested_within(&self, board: Board, window: Duration) -> bool {
        index(board).is_some_and(|i| {
            let at = self.apply_requested_ms[i].load(std::sync::atomic::Ordering::Acquire);
            at != 0
                && crate::telemetry_task::get_current_timestamp_ms().saturating_sub(at)
                    <= window.as_millis() as u64
        })
    }
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
                previous: array::from_fn(|_| None),
                rejected_statuses: 0,
                last_rejection: None,
                pushed: [None; 7],
            }),
            apply_requested_ms: Default::default(),
            av_bay_paused: Mutex::new(None),
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

/// Boards whose firmware owns a RESYNC path. The Flight Computer owns its own
/// flight states, and the metadata boards already follow the published phase.
fn resync_board(board: Board) -> bool {
    matches!(board, Board::ActuatorBoard | Board::ValveBoard)
}
/// Station phases a board may be resynced to: Startup, Idle, the fill phases
/// and Armed. Resync to Startup or Idle brings a board back from any state,
/// including Launch, without an Abort first: the board stops its own launch
/// owner and drives its outputs safe (AB/VB firmware that accepts RESYNC to
/// Startup/Idle). Test fire keeps Startup/Idle on Recover/Reset. From Launch
/// onward the station itself never resyncs: the Flight Computer owns flight.
// Safety catalog (About safety): board-resync. Keep backend/src/safety_catalog_rules.rs in step.
pub(crate) fn resync_target(state: crate::types::FlightState) -> Option<u8> {
    use crate::types::FlightState as F;
    (matches!(state, F::PreFill | F::FillTest | F::NitrogenFill | F::NitrousFill | F::Armed)
        || matches!(state, F::Startup | F::Idle) && !crate::runtime_mode::is_test_fire())
        .then_some(state as u8)
}
fn phase_label(state: u8) -> String {
    crate::types::u8_to_flight_state(state).map_or_else(|| "Unknown".into(), |value| format!("{value:?}"))
}
/// Plain-language reason the station cannot resync any board right now,
/// including commands still waiting on the network. Used at PREPARE.
pub(crate) fn resync_host_block_reason(state: &AppState) -> Option<String> {
    resync_work_block_reason(state).or_else(|| crate::gse::operator_reset_block_reason(state))
}
/// The same window from RAM owners only. A confirmed resync rechecks this while
/// its own recovery packets keep the router busy.
pub(crate) fn resync_work_block_reason(state: &AppState) -> Option<String> {
    use crate::types::FlightState as F;
    let phase = state.local_flight_state_snapshot();
    match phase {
        F::Startup if crate::runtime_mode::is_test_fire() => return Some("The station is in Startup and moves to Idle on its own. In test fire, Resync matches a board to a fill phase or Armed; use Review recovery or Reset all boards to return a board to Startup or Idle.".into()),
        F::Idle if crate::runtime_mode::is_test_fire() => return Some("The station is Idle. In test fire, use Review recovery (Recover to Idle) to return a board to Idle; Resync is for the fill phases and Armed.".into()),
        F::Aborted => return Some("The station is Aborted, so Resync cannot clear a board's Abort. Use Review recovery or Reset all boards.".into()),
        _ if resync_target(phase).is_none() => return Some("Resync is not available from Launch onward: nothing aborts or acts automatically after Launch, and the Flight Computer owns the flight states.".into()),
        _ => {}
    }
    if crate::gse::overpressure_holds_abort(state) {
        return Some("A tank overpressure is latched, so a board Abort may still be protecting the tank. Acknowledge the overpressure once pressure is below relief, then use Review recovery or Reset all boards.".into());
    }
    crate::gse::operator_work_block_reason(state)
}
/// Plain-language reason this board cannot be resynced, from its last readback.
/// PREPARE repeats every check against a fresh dedicated query.
fn resync_board_block_reason(board: Board, row: Option<&Readback>, target: Option<u8>) -> Option<String> {
    let Some(row) = row else {
        return Some("No board readback yet. Query the board first.".into());
    };
    if row.flags & RESYNC_IMPLEMENTATION == 0 {
        return Some("This board's firmware does not support Resync.".into());
    }
    if matches!(row.boot, 0 | u64::MAX) || row.result == 7 {
        return Some("The board needs commissioning first. Use Review recovery.".into());
    }
    if !resync_outputs_ready(board, row, target) && target.is_some_and(|phase| phase <= 1) {
        return Some("This board's outputs do not read back consistently, so Resync cannot verify them. Query the board again, or use Reset all boards.".into());
    }
    if !resync_outputs_ready(board, row, target) {
        return Some("Resync never moves outputs, and this board's outputs do not read safe. Use Review recovery or Reset all boards.".into());
    }
    if let Some(phase) = target && row.state == Some(phase) && row.flags & 1 == 0 {
        return Some(format!("The board already matches the station ({}).", phase_label(phase)));
    }
    None
}
/// The board refuses RESYNC unless its commanded outputs already read safe;
/// it never drives them. Mirror that here so the operator sees why.
/// Resync to a fill phase or Armed never moves an output, so they must
/// already read safe. Resync to Startup or Idle drives every output of the
/// board to its safe state itself (igniter, supplies, valves, stepper asleep)
/// before it changes state, so its outputs need only be readable and
/// consistent; a board left in Launch with its stepper holding is the case
/// this exists for.
fn resync_outputs_ready(board: Board, row: &Readback, target: Option<u8>) -> bool {
    if target.is_some_and(|phase| phase <= 1) {
        return row.flags & (STATE_VALID | OUTPUTS_VALID) == STATE_VALID | OUTPUTS_VALID
            && query_domain_consistent(board, row);
    }
    resync_outputs_safe(board, row)
}
fn resync_outputs_safe(board: Board, row: &Readback) -> bool {
    row.flags & (STATE_VALID | OUTPUTS_VALID | 2) == STATE_VALID | OUTPUTS_VALID | 2
        && safe_output_mask(board) == Some(row.outputs)
}

/// Commanded-register safe domains from the actual board owners. VB's safe
/// state opens both servos (bits1/2) and switches its pilot solenoid off.
/// These are commanded registers, never mechanical position verification.
// Safety catalog (About safety): reset-all-boards. Keep backend/src/safety_catalog_rules.rs in step.
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
                "The board readback is older than fifteen seconds. Status refreshes automatically; preparing Recover requests a fresh readback.",
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
/// An unsolicited status a board sends once when it latches an Abort (nonce
/// 0, Abort-latched flag set). It does not wait on the station's query
/// stream, so the station sees an umbilical board's Abort within
/// milliseconds even while an AV-bay query holds BOARD_RECOVERY_REQUEST.
/// It can only raise that board's Abort: it never becomes a readback, never
/// counts as launch or ignition proof, and never clears anything. A forged or
/// replayed push can at worst show a false Abort (the safe direction), so it
/// needs no query issuer (0 before the board's first query); it must come
/// from that board's verified endpoint. Within one boot generation, a push is
/// taken only if it is later by (abort epoch, uptime) than the last push
/// taken and not earlier than the board's latest readback, so a replay is
/// ignored even on a board whose epoch does not advance on every latch.
fn observe_abort_push(state: &AppState, board: Board, push: Readback) -> Result<(), &'static str> {
    if push.flags & 1 == 0 {
        return Err("Abort push without the Abort-latched flag (ignored)");
    }
    let verified = state
        .topology_router
        .get()
        .is_some_and(|router| endpoint_verified(state, board, &router.export_topology()));
    if !verified {
        return Err("Abort push from an unverified board endpoint");
    }
    let at = index(board).unwrap();
    {
        let mut inner = state.board_recovery.inner.lock().unwrap();
        let order = (push.epoch, push.uptime);
        let replay = inner.pushed[at].is_some_and(|(boot, epoch, uptime)| boot == push.boot && order <= (epoch, uptime))
            || inner.values[at]
                .as_ref()
                .is_some_and(|row| row.value.boot == push.boot && order < (row.value.epoch, row.value.uptime));
        if replay {
            return Err("Replayed Abort push for an epoch already seen (ignored)");
        }
        inner.pushed[at] = Some((push.boot, push.epoch, push.uptime));
    }
    log::warn!("{} pushed its Abort latch (epoch {})", board.sender_id(), push.epoch);
    state.board_abort.observe_abort(
        board,
        std::time::Instant::now(),
        crate::telemetry_task::get_current_timestamp_ms(),
    );
    Ok(())
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
    if let Some(push) = Readback::decode_wire(packet.payload()).filter(|value| value.nonce == 0) {
        return observe_abort_push(state, board, push);
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
    if value.flags & RESYNC_IMPLEMENTATION != 0 && !resync_board(board) {
        return Err("Board advertised Resync without a resync owner");
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
    let live = |query: &Query| query.nonce == value.nonce
        && now.checked_duration_since(query.sent).is_some_and(|age| age <= QUERY_TTL);
    let current_matches = inner.queries[at].as_ref().is_some_and(live);
    // The reply to the query a re-query superseded still counts while the
    // current query is unanswered: same issuer, its own nonce and its own
    // (older) send time, so it never claims fresher state than it carries.
    let previous_matches = !current_matches
        && inner.queries[at].as_ref().is_some_and(|query| !query.received)
        && inner.previous[at].as_ref().is_some_and(live);
    if inner.queries[at].is_none() {
        return Err("Unsolicited recovery status");
    }
    if !current_matches && !previous_matches {
        return Err("Ignored a late reply to an expired or superseded board query (harmless). Query the board again, or open Review recovery, which takes its own fresh query.");
    }
    let received = if previous_matches { &inner.previous } else { &inner.queries }[at].as_ref().unwrap().received;
    if received {
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
    let query = if previous_matches { &mut inner.previous } else { &mut inner.queries }[at].as_mut().unwrap();
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
    let health=state.network_health.lock().unwrap().snapshot(WallInstant::now(),crate::runtime_mode::is_test_fire());
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
    if row.observed_after.wall.elapsed() > CONTROL_PROOF_TTL
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
    if row.age(Stamp::now()) > CONTROL_PROOF_TTL
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
/// A board's fresh correlated readback that needs no further recovery: not
/// aborted, in Startup or Idle, with safe commanded outputs (or no physical
/// outputs for metadata boards). Presentation and launch-lock helpers only;
/// it never authorizes a recovery write.
fn readback_healthy_idle(board: Board, observation: &Observation, now: Stamp) -> bool {
    let row = &observation.value;
    observation.fresh(now)
        && row.result == 0
        && row.flags & 1 == 0
        && row.flags & STATE_VALID != 0
        && matches!(row.state, Some(0 | 1))
        && !matches!(row.boot, 0 | u64::MAX)
        && row.epoch != 0
        && row.epoch < u32::MAX - 1
        && query_domain_consistent(board, row)
        && match safe_output_mask(board) {
            Some(mask) => row.flags & (OUTPUTS_VALID | 2) == OUTPUTS_VALID | 2 && row.outputs == mask,
            None => true,
        }
}
/// A durable pending recovery target that a later fresh verified readback has
/// satisfied: the same boot identity, the same or a newer recovery epoch, and
/// a healthy Startup/Idle state. Commissioning (FIRST) uncertainty is never
/// satisfied this way; it needs its explicit reconciliation.
// Safety catalog (About safety): reset-all-boards. Keep backend/src/safety_catalog_rules.rs in step.
fn pending_target_satisfied(
    board: Board,
    target: &crate::launch_history::RecoveryTarget,
    observation: Option<&Observation>,
    endpoint_verified: bool,
    now: Stamp,
) -> bool {
    let Some(observation) = observation else { return false; };
    endpoint_verified
        && target.board == board.sender_id()
        && target.kind == crate::launch_history::RecoveryIntentKind::Normal
        && target.uncertain_first.is_none()
        && target.original_first.is_none()
        && observation.value.boot == target.boot
        && observation.value.epoch >= target.epoch
        && readback_healthy_idle(board, observation, now)
}
/// Boards of the durable unpublished pending recovery that still need it.
pub(crate) fn unsatisfied_pending_mask(state: &AppState) -> u8 {
    let Some(pending) = state.launch_history.lock().unwrap().recovery_pending().filter(|p| !p.published) else {
        return 0;
    };
    let topology = state.topology_router.get().map(|router| router.export_topology());
    let now = Stamp::now();
    let inner = state.board_recovery.inner.lock().unwrap();
    pending.targets.iter().fold(0, |mask, target| {
        let Some((at, board)) = BOARDS.iter().enumerate().find(|(_, b)| b.sender_id() == target.board) else {
            return mask | 0x80;
        };
        let verified = topology.as_ref().is_some_and(|topology| endpoint_verified(state, *board, topology));
        if pending_target_satisfied(*board, target, inner.values[at].as_ref(), verified, now) { mask } else { mask | (1 << at) }
    })
}
/// Board readbacks must be sampled at least this long after a launch attempt
/// before they can release its lock, so a pre-launch reply still in flight, or
/// a board that has not yet acted on Launch, can never release it.
const LAUNCH_LOCK_SETTLE: Duration = Duration::from_secs(10);
/// Present required boards must all read back healthy, and at least one board
/// must be present: an empty network never unlocks Launch.
fn launch_lock_release_ready(station_ready: bool, required_present: u8, healthy_after_lock: u8) -> bool {
    station_ready && required_present != 0 && required_present & !healthy_after_lock == 0
}
/// Release the post-attempt launch lock once every present required board has
/// a fresh verified readback, taken after the attempt, that is not aborted, in
/// Startup or Idle with safe outputs, while no board Abort, missed reset or
/// unfinished recovery remains and the station itself is in Startup or Idle.
/// Missing boards do not block (the Abort presence mask). Resetting the T-clock
/// alone changes none of these inputs and so never unlocks Launch. Returns true
/// when it released the lock.
// Safety catalog (About safety): launch-lock-reset. Keep backend/src/safety_catalog_rules.rs in step.
pub(crate) fn try_release_launch_lock(state: &Arc<AppState>) -> bool {
    use std::sync::atomic::Ordering;
    if crate::rrls::is_rrls() {
        return false;
    }
    let locked_at = {
        let Ok(history) = state.launch_history.try_lock() else { return false; };
        if !history.launch_locked() && !history.automatic_actions_inhibited() {
            return false;
        }
        if history.recovery_pending().is_some_and(|pending| !pending.published) {
            return false;
        }
        history.launch_locked_at()
    };
    let station_ready = matches!(state.local_flight_state_snapshot(),
            crate::types::FlightState::Startup | crate::types::FlightState::Idle)
        && !state.launch_sequence_command_pending.load(Ordering::Acquire)
        && state.operator_admission_barrier.load(Ordering::Acquire) == 0
        && coordinator::readonly_query_idle(state)
        && crate::board_abort::blocked_message(state).is_none();
    if !station_ready {
        return false;
    }
    let Some(router) = state.topology_router.get() else { return false; };
    let topology = router.export_topology();
    let required = crate::board_recovery::scope::mode_scope(crate::station_mode::local_mode())
        & crate::board_abort::present_mask(state);
    let now = Stamp::now();
    // HITL locks Launch only while a board reports Abort (station_ready above).
    // Absent, unreachable or not-yet-read-back boards do not hold it.
    let hitl = crate::station_mode::local_mode() == crate::station_mode::Mode::Hitl;
    let healthy = if hitl { required } else {
        let inner = state.board_recovery.inner.lock().unwrap();
        BOARDS.iter().enumerate().fold(0u8, |mask, (at, board)| {
            let ok = inner.values[at].as_ref().is_some_and(|row| {
                row.observed_after.wall.checked_duration_since(locked_at)
                    .is_some_and(|after| after >= LAUNCH_LOCK_SETTLE)
                    && readback_healthy_idle(*board, row, now)
            }) && endpoint_verified(state, *board, &topology);
            if ok { mask | (1 << at) } else { mask }
        })
    };
    if !(hitl || launch_lock_release_ready(station_ready, required, healthy)) {
        return false;
    }
    let released = {
        let Ok(mut history) = state.launch_history.try_lock() else { return false; };
        // Recheck under the owner: a new attempt moves the lock start forward.
        if history.launch_locked_at() != locked_at { return false; }
        history.release_launch_lock().unwrap_or(false)
    };
    if released {
        state.set_launch_indicator_latched(false);
        state.add_notification("Launch unlocked: every board on the network reads back healthy in Startup or Idle.".to_string());
        crate::sequences::refresh_action_policy_now(state);
        state.broadcast_action_policy_snapshot();
    }
    released
}
fn recovery_requirement_reason(actual_abort: bool, unfinished_target: bool) -> Option<&'static str> {
    if actual_abort { Some("abort_latched") }
    else if unfinished_target { Some("unfinished_recovery") }
    else { None }
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
        .snapshot(now.wall, crate::runtime_mode::is_test_fire());
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
    // Why Recover is unavailable, so an operator (or an anonymous view) sees
    // the next action instead of a bare false capability.
    let recover_blocked = (!can_recover).then(|| {
        if !primary { return "Only the primary ground station can recover boards.".to_string(); }
        if principal.anonymous { return "Sign in with an account that can send commands to recover boards.".to_string(); }
        if !principal.permissions.send_commands { return "Your account cannot send commands.".to_string(); }
        if !network_utc_available { return "Network time is not available yet; wait for the boards' time sync.".to_string(); }
        if !state.launch_history.lock().unwrap().recovery_available() {
            return "Launch history storage is unavailable, so a recovery cannot be recorded.".to_string();
        }
        if let Some(reason) = crate::gse::operator_work_block_reason(&state) { return reason; }
        format!("Review recovery needs the station in Startup, Idle or Aborted (now {:?}). Use Reset all boards, or Resync an Actuator or Valve Board.", state.local_flight_state_snapshot())
    });
    let reset_startup_blocked = (!can_reset_startup).then(|| {
        if !primary { return "Only the primary ground station can reset boards.".to_string(); }
        if principal.anonymous { return "Sign in to reset boards.".to_string(); }
        if !principal.permissions.send_commands { return "Your account cannot send commands.".to_string(); }
        if !network_utc_available { return "Network time is not available yet.".to_string(); }
        if !state.launch_history.lock().unwrap().recovery_available() {
            return "Launch history storage is unavailable, so a reset cannot be recorded.".to_string();
        }
        if let Some(reason) = crate::gse::operator_reset_block_reason(&state) { return reason; }
        format!("Reset is not allowed in the current station state ({:?}).", state.local_flight_state_snapshot())
    });
    // Descriptive only. PREPARE and CONFIRM recheck all of this against a fresh
    // dedicated query and the frozen station phase.
    let resync_target_state = resync_target(state.local_flight_state_snapshot());
    let resync_station_reason: Option<String> = if !primary {
        Some("Only the primary ground station can resync boards.".into())
    } else if principal.anonymous {
        Some("Sign in to resync boards.".into())
    } else if !principal.permissions.send_commands {
        Some("Your account cannot send commands.".into())
    } else if !network_utc_available {
        Some("Network time is not available yet.".into())
    } else if !state.launch_history.lock().unwrap().recovery_available() {
        Some("Launch history storage is unavailable, so a resync cannot be recorded.".into())
    } else {
        // A momentarily busy router is not a reason to show; PREPARE waits it out.
        resync_host_block_reason(&state).filter(|reason| !reason.contains("router is busy"))
    };
    let principal_identity = principal_context(&identity, &headers, &principal);
    let confirmation = state
        .board_recovery
        .coordinator
        .confirmation(&state, &principal_identity);
    let host_pending = state.launch_history.lock().unwrap().recovery_pending();
    let actual_abort_mask = state.board_abort.masks().0;
    let unsatisfied_pending = unsatisfied_pending_mask(&state);
    let av_bay_paused = state.board_recovery.av_bay_pause_reason();
    let boards=BOARDS.iter().enumerate().map(|(at,&board)| {
        // A pending target that a fresh verified readback shows healthy no
        // longer needs recovery; only the unsatisfied ones are shown.
        let recovery_reason = recovery_requirement_reason(actual_abort_mask & (1 << at) != 0,
            unsatisfied_pending & (1 << at) != 0);
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
        let resync_capable = verified && resync_board(board)
            && readback.is_some_and(|row| row.flags & RESYNC_IMPLEMENTATION != 0);
        let resync_reason = resync_capable.then(|| resync_station_reason.clone()
            .or_else(|| (unsatisfied_pending & !(1 << at) != 0).then(|| format!("Finish the incomplete recovery of {} first.",
                    BOARDS.iter().enumerate().filter(|(i, _)| unsatisfied_pending & (1 << i) != 0)
                        .map(|(_, b)| b.sender_id()).collect::<Vec<_>>().join(", "))))
            .or_else(|| resync_board_block_reason(board, readback, resync_target_state))).flatten();
        let (readback_status, message) = recovery_diagnostic(primary, verified, network_utc_available,
            observation, queries[at].as_ref(), now, reconcilable);
        // While background AV-bay queries are paused, say where the row comes
        // from instead of promising an automatic refresh. One line: the UI
        // explains the pause in the row's "About these readbacks" disclosure.
        let message = match av_bay_paused {
            Some(_) if AV_BAY_INDICES.contains(&at) && matches!(readback_status, "query_required" | "readback_expired") =>
                "Auto-queries paused (AV-bay link degraded) — last readback shown",
            _ => message,
        };
        let operation=queries[at].as_ref().map(|query| {
            let mut operation_id=digest::Context::new(&digest::SHA256);operation_id.update(&identity);operation_id.update(&query.nonce.to_le_bytes());operation_id.update(board.sender_id().as_bytes());
            let pending=!query.received&&now.checked_duration_since(query.sent).is_some_and(|age|age<=QUERY_TTL);
            json!({"id":hex(&operation_id.finish().as_ref()[..16]),"action":"query","phase":if pending{"querying"}else if query.received{"completed"}else{"failed"},"targets":[board.sender_id()],"completed":!pending,"success":if pending{None}else{Some(query.received&&readback.is_some_and(|row|row.nonce==query.nonce&&row.result==0))},"age_ms":now.checked_duration_since(query.sent).unwrap_or(Duration::MAX).as_millis().min(u64::MAX as u128)as u64,"message":if pending{"Query admitted; actual board readback pending."}else if query.received{"Board answered; its result and actual-state validity are shown separately."}else{"Query readback timed out; recovery was not issued."}})
        });
        json!({"id":board.sender_id(),"label":board.as_str(),
            "recovery_required":recovery_reason.is_some(),"recovery_reason":recovery_reason,
            "display_connected":health.is_some_and(|h|h.healthy),
            "display_known":health.is_some_and(|h|h.healthy)&&known.is_some_and(|row|row.result==0&&row.state.is_some()&&query_domain_consistent(board,row)),
            "network":{"healthy":health.is_some_and(|h|h.healthy),"age_ms":health.map_or(u64::MAX,|h|h.age_ms),"disconnects":health.map_or(0,|h|h.disconnects),"reconnects":health.map_or(0,|h|h.reconnects)},
            "state":readback.and_then(|row|row.state),"state_label":readback.and_then(|row|row.state).and_then(crate::types::u8_to_flight_state).map_or("Unknown".to_string(),|value|format!("{value:?}")),
            "boot":known.map(|row|format!("{:016x}",row.boot)),"epoch":known.map(|row|row.epoch),"nonce":known.map(|row|format!("{:016x}",row.nonce)),"age_ms":age,"readback_valid_for_ms":observation.map(|row|READBACK_TTL.saturating_sub(row.age(now)).as_millis().min(u64::MAX as u128)as u64),
            "receipt_age_ms":observation.map(|row|now.duration_since(row.received).as_millis().min(u64::MAX as u128)as u64),"uptime_ms":readback.map(|row|row.uptime),"abort_latched":readback.filter(|_|matches!(board,Board::ActuatorBoard|Board::ValveBoard|Board::FlightComputer)).map(|row|row.flags&1!=0),"safe_outputs":readback.filter(|row|row.flags&OUTPUTS_VALID!=0).map(|row|row.flags&2!=0&&query_domain_consistent(board,row)),"commanded_outputs":readback.filter(|row|row.flags&OUTPUTS_VALID!=0).map(|row|row.outputs),
            "output_readback":if readback.is_some_and(|row|row.flags&OUTPUTS_VALID!=0){"gpio_servo_command_registers"}else{"unknown"},"physical_position_verified":false,
            "capabilities":{"query":primary&&verified,"endpoint_verified":verified,"hardened_transport":verified&&fresh&&readback.is_some_and(|row|row.flags&0x20!=0),"known_utc_manual_stop":verified&&fresh&&matches!(board,Board::ActuatorBoard|Board::ValveBoard|Board::FlightComputer)&&readback.is_some_and(|row|row.flags&0x40!=0),"recover":supported&&targeted_recovery_state(board).is_some(),"prepare_recover":prepare_recover,"resync":resync_capable,"reset_startup":supported},"resync_blocked_reason":resync_reason,"operation":operation,
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
        "permissions":{"query":primary,"recover":can_recover,"reset_startup":can_reset_startup,"change_mode":false},"reset_startup_blocked_reason":reset_startup_blocked,"recover_blocked_reason":recover_blocked,"av_bay_status_queries_paused":av_bay_paused,
        "resync_target_state":resync_target_state,"resync_target_label":resync_target_state.map(phase_label),
        "reset_startup_policy":reset_startup_policy,
        "confirmation":confirmation,"host_recovery":host_pending.map(|pending|json!({"pending":!pending.published,"verified":pending.verified,"publication_acknowledged":pending.published,"targets":pending.targets.iter().map(|target|target.board.as_str()).collect::<Vec<_>>()})),
        "rejected_statuses":rejected_statuses,"last_rejection":last_rejection,
        "mode_capability":{"current":current,"build_mode":build,"supported_modes":["normal","hitl","test_fire","testing"],"runtime_switch_supported":true,"requires_restart":[],"message":"One backend holds every mode; it switches in process from Station mode settings while the station is idle. Only a Simulation change restarts the backend."},"boards":boards,"operation":state.board_recovery.coordinator.operation()})).into_response()
}
/// Background reads share a two-second budget with demand-triggered reads.
/// No method here prepares, confirms, stops or recovers a board.
#[derive(Default)]
pub(crate) struct ReadonlyQueryRefresh {
    last: [Option<WallInstant>; 7],
    /// Round-robin position per link: AV bay (RF FC PB) and umbilical (AB VB
    /// GB DAQ) each get their own turn, so fill-board queries never wait for
    /// the radio side's turn.
    cursor: [usize; 2],
    core_busy_last: [Option<WallInstant>; 7],
    /// Background queries in a row that got no reply, per board.
    misses: [u8; 7],
    /// No new AV-bay query while an Abort's umbilical readback is owed.
    av_bay_held: bool,
    /// Background AV-bay queries paused since then, because the AV-bay radio
    /// path is degraded. BOARD_RECOVERY_REQUEST is one ordered SDK stream, so
    /// an AV-bay query waiting on a slow or lost radio path holds every
    /// umbilical query behind it for its whole retry span (about 4.5 s), and
    /// drops them if it gives up.
    av_bay_paused_since: Option<WallInstant>,
}
impl ReadonlyQueryRefresh {
    fn due(&self, at: usize, now: WallInstant) -> bool {
        readonly_query_due(self.last[at], now)
            && self.core_busy_last[at].is_none_or(|last| {
                now.checked_duration_since(last)
                    .is_some_and(|age| age >= Duration::from_millis(250))
            })
    }
    fn record_response(&mut self, at: usize, now: WallInstant, response: &Response) -> bool {
        if response.extensions().get::<QueryAdmissionCoreBusy>().is_some() {
            // The SDK accepted no owner. Bound attempts without consuming the
            // two-second network budget for an observation that was never sent.
            self.core_busy_last[at] = Some(now);
        } else {
            // Accepted queries and every other refusal retain the old cooldown.
            self.last[at] = Some(now);
            self.core_busy_last[at] = None;
        }
        response.status() == StatusCode::OK
    }
    pub(crate) fn request(&mut self, state: &Arc<AppState>, board: Board) -> bool {
        let Some(at) = index(board) else {
            return false;
        };
        let now = WallInstant::now();
        if !self.due(at, now) || (self.av_bay_paused_since.is_some() && AV_BAY_INDICES.contains(&at)) {
            return false;
        }
        // Never replace a query still awaiting its reply (for example the one
        // a recovery PREPARE is waiting on); it answers the same demand.
        let stamp = Stamp::now();
        if state.board_recovery.inner.lock().unwrap().queries[at].as_ref().is_some_and(|query| {
            !query.received && stamp.checked_duration_since(query.sent).is_none_or(|age| age <= query_retry())
        }) {
            return false;
        }
        let previous = state.board_recovery.inner.lock().unwrap().queries[at].clone();
        let response = request_board_query(state, board);
        let sent = self.record_response(at, now, &response);
        if sent {
            self.misses[at] = next_readonly_misses(previous.as_ref(), self.misses[at]);
        }
        sent
    }
    /// A local Abort: AV-bay boards owe Abort readback, not background
    /// polling, so their read-only backoff starts over.
    pub(crate) fn clear_av_bay_backoff(&mut self) {
        for at in AV_BAY_INDICES {
            self.misses[at] = 0;
        }
    }
    /// Hold (or release) new AV-bay background queries; see `abort_confirm`.
    pub(crate) fn hold_av_bay(&mut self, held: bool) {
        self.av_bay_held = held;
    }
    /// Pause background AV-bay queries while the AV-bay radio path is
    /// degraded: the primary relies on an RRLS relay, or a background AV-bay
    /// query went unanswered past its retry window. Resume, visibly, only when
    /// the direct path is back, at least AV_BAY_MIN_PAUSE has passed, and every
    /// AV-bay board still on the network has sent a status report since the pause. Operator queries and recovery (Reset
    /// all's fresh queries included) are never paused.
    fn update_av_bay_pause(&mut self, state: &Arc<AppState>, now: Stamp) {
        let relaying = crate::rrls::primary_relaying();
        let lost = {
            let inner = state.board_recovery.inner.lock().unwrap();
            AV_BAY_INDICES.iter().any(|&at| {
                inner.queries[at].as_ref().is_some_and(|query| {
                    !query.received && now.checked_duration_since(query.sent).is_some_and(|age| age > query_retry())
                })
            })
        };
        match self.av_bay_paused_since {
            None if relaying || lost => {
                let reason = if relaying {
                    "the AV bay is reached through the RRLS relay"
                } else {
                    "an AV-bay status query went unanswered"
                };
                log::warn!("AV-bay background status queries paused: {reason}; rows show status reports until the link is healthy");
                self.av_bay_paused_since = Some(now.wall);
                *state.board_recovery.av_bay_paused.lock().unwrap() = Some(reason);
            }
            Some(since) if !relaying && now.wall.checked_duration_since(since).is_some_and(|paused| paused >= av_bay_min_pause()) => {
                // Every AV-bay board this mode tracks that was on the network
                // before the pause must have reported since, and at least one
                // did. A board already silent then (powered off) is not
                // waited for: it would hold the pause, and with it every other
                // AV-bay row, until it is switched on.
                let scope = scope::mode_scope(crate::station_mode::local_mode());
                let present_after = since.checked_sub(AV_BAY_OFFLINE).unwrap_or(since);
                let reported = state.board_status.try_lock().ok().is_some_and(|status| {
                    let seen: Vec<_> = AV_BAY_INDICES.iter().filter(|&&at| scope & (1 << at) != 0)
                        .filter_map(|&at| status.get(&BOARDS[at]).and_then(|row| row.last_seen_instant))
                        .filter(|seen| seen.checked_duration_since(present_after).is_some())
                        .collect();
                    !seen.is_empty() && seen.iter().all(|seen| seen.checked_duration_since(since).is_some())
                });
                if reported {
                    log::info!("AV-bay background status queries resumed: the direct path is back and every AV-bay board reported since the pause");
                    self.av_bay_paused_since = None;
                    for at in AV_BAY_INDICES {
                        self.misses[at] = 0;
                    }
                    // A query lost while paused must not re-pause at once.
                    let mut inner = state.board_recovery.inner.lock().unwrap();
                    for at in AV_BAY_INDICES {
                        if inner.queries[at].as_ref().is_some_and(|query| !query.received) {
                            inner.queries[at] = None;
                        }
                    }
                    drop(inner);
                    *state.board_recovery.av_bay_paused.lock().unwrap() = None;
                }
            }
            _ => {}
        }
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
            .snapshot(now.wall, crate::runtime_mode::is_test_fire());
        self.update_av_bay_pause(state, now);
        // One AV-bay query in the ordered stream at a time: each one queued
        // behind another holds every umbilical query (an Abort's readback
        // included) for its whole round trip too.
        let av_bay_busy = abort_confirm::av_bay_query_in_flight(router);
        let (last, core_busy_last, misses, av_bay_held) = (
            self.last,
            self.core_busy_last,
            self.misses,
            self.av_bay_held || self.av_bay_paused_since.is_some() || av_bay_busy,
        );
        // Only boards this mode tracks (test fire has no AV bay).
        let scope = scope::mode_scope(crate::station_mode::local_mode());
        let picks = {
            let inner = state.board_recovery.inner.lock().unwrap();
            pick_per_link(&mut self.cursor, |at| {
                let board = BOARDS[at];
                let seen = healthy
                    .iter()
                    .any(|row| row.sender_id == board.sender_id() && row.healthy);
                // A board network health marks unhealthy is still asked, at a
                // slower cadence, so one that comes back becomes fresh without
                // an operator Query.
                scope & (1 << at) != 0 && !link_held(at, av_bay_held) && core_busy_last[at].is_none_or(|busy| now.wall.checked_duration_since(busy)
                    .is_some_and(|age| age >= Duration::from_millis(250))) && readonly_query_candidate(
                    true,
                    endpoint_verified(state, board, &topology),
                    inner.values[at].as_ref(),
                    inner.queries[at].as_ref(),
                    now,
                ) && readonly_query_due_for(last[at], now.wall, seen)
                    && readonly_backoff_due(
                        last[at],
                        now.wall,
                        effective_readonly_misses(at, seen,
                            inner.queries[at].as_ref().is_some_and(|query| query.received), misses[at]),
                    )
            })
        };
        let mut first = None;
        for at in picks {
            let board = BOARDS[at];
            if self.request(state, board) && first.is_none() {
                first = Some(board);
            }
        }
        first
    }
}
/// AV-bay (radio) and umbilical (fill) board indices into BOARDS.
const LINK_GROUPS: [&[usize]; 2] = [&AV_BAY_INDICES, &[3, 4, 5, 6]];
/// RF, FC and PB in BOARDS.
const AV_BAY_INDICES: [usize; 3] = [0, 1, 2];
/// A paused AV-bay refresh stays paused at least one reliable retry span
/// (250 ms x 18), so a lost query's tail has drained before the next one goes
/// and status reports alone cannot make it flap.
const AV_BAY_MIN_PAUSE: Duration = Duration::from_millis(4_500);
/// An AV-bay board not heard for this long before a pause began counts as
/// off the network; the resume does not wait for its report.
const AV_BAY_OFFLINE: Duration = Duration::from_secs(10);
fn av_bay_min_pause() -> Duration {
    crate::sim_freshness::wall(AV_BAY_MIN_PAUSE)
}
/// At most one due board per link per pass, each link with its own cursor.
fn pick_per_link(cursor: &mut [usize; 2], eligible: impl Fn(usize) -> bool) -> Vec<usize> {
    let mut picks = Vec::new();
    for (group, members) in LINK_GROUPS.iter().enumerate() {
        let start = cursor[group] % members.len();
        if let Some(offset) = (0..members.len()).find(|offset| eligible(members[(start + offset) % members.len()])) {
            picks.push(members[(start + offset) % members.len()]);
            cursor[group] = (start + offset + 1) % members.len();
        }
    }
    picks
}
/// Background queries that went unanswered back off per board: the first
/// retry after QUERY_RETRY, then doubling up to READONLY_BACKOFF_MAX. Any
/// reply resets it. Operator Query, Prepare/Recover and Resync never wait.
const READONLY_BACKOFF_MAX: Duration = Duration::from_secs(30);
fn readonly_backoff_due(last: Option<WallInstant>, now: WallInstant, misses: u8) -> bool {
    if misses == 0 {
        return true;
    }
    let wait = QUERY_RETRY
        .saturating_mul(1u32 << u32::from(misses.min(8) - 1))
        .min(READONLY_BACKOFF_MAX);
    let wait = crate::sim_freshness::wall(wait);
    last.is_none_or(|last| now.checked_duration_since(last).is_some_and(|age| age >= wait))
}
/// Misses that count toward the backoff. A reply ends the backoff at once.
/// The backoff protects a failing radio link. The umbilical is a wired
/// 1 Mbit/s link where a lost reply means the query was dropped on the way
/// (for example behind an AV-bay query in the one ordered query stream), so
/// an umbilical board network health shows alive is asked at the normal
/// cadence.
fn effective_readonly_misses(at: usize, healthy: bool, received: bool, misses: u8) -> u8 {
    if received || (healthy && LINK_GROUPS[1].contains(&at)) { 0 } else { misses }
}
/// Whether this board's link takes no new background query now.
fn link_held(at: usize, av_bay_held: bool) -> bool {
    av_bay_held && LINK_GROUPS[0].contains(&at)
}
/// Unanswered background queries in a row after issuing the next one.
fn next_readonly_misses(previous: Option<&Query>, misses: u8) -> u8 {
    match previous {
        Some(query) if !query.received => misses.saturating_add(1).min(200),
        _ => 0,
    }
}
/// A board network health marks unhealthy is re-queried this often.
const UNHEALTHY_REQUERY: Duration = Duration::from_secs(10);
fn readonly_query_due_for(last: Option<WallInstant>, now: WallInstant, healthy: bool) -> bool {
    if healthy {
        return readonly_query_due(last, now);
    }
    last.is_none_or(|last| now.checked_duration_since(last).is_some_and(|age| age >= UNHEALTHY_REQUERY))
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
                .is_none_or(|age| age <= query_retry())
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
    operator_board_query(&state, board).await
}
/// The operator's Query. A router worker mid-pass holds the SDK core for
/// moments at a time; that offer was never admitted (no nonce, no budget
/// spent), so it is retried briefly instead of refusing the operator. Every
/// other answer, including query_pending and owner refusals, returns at once.
pub(crate) async fn operator_board_query(state: &Arc<AppState>, board: Board) -> Response {
    crate::core_busy::retry(
        || {
            let response = request_board_query(state, board);
            if response.extensions().get::<QueryAdmissionCoreBusy>().is_some() { Err(response) } else { Ok(response) }
        },
        |_| true,
    )
    .await
    .unwrap_or_else(|response| response)
}
/// Internal read-only collection shares EXACT HTTP nonce/correlation/budget rules.
/// It cannot emit RECOVER, change board state or bypass primary identity checks.
// Private admission detail for the pre-confirmation read-only coordinator.
// HTTP clients retain the existing error status/body.
#[derive(Clone, Copy)]
struct QueryAdmissionCoreBusy;
fn rejected_query_offer(reason: sedsnet::TelemetryError) -> Response {
    let mut response = error(
        StatusCode::SERVICE_UNAVAILABLE,
        "queue_rejected",
        "SDK did not accept query ownership.",
    );
    if matches!(reason, sedsnet::TelemetryError::Io("router core busy")) {
        response.extensions_mut().insert(QueryAdmissionCoreBusy);
    }
    response
}
pub(crate) fn request_board_query(state: &Arc<AppState>, board: Board) -> Response {
    send_board_query(state, board, QueryMode::Background)
}
/// A fresh query even while an older one is unanswered; the older query's
/// reply still counts. Used for prompt Abort readback (`abort_confirm`).
fn requery_board_query(state: &Arc<AppState>, board: Board) -> Response {
    send_board_query(state, board, QueryMode::Requery)
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum QueryMode {
    /// Refused while an earlier query is still waiting for its reply.
    Background,
    /// Replaces a waiting query, which stays acceptable as `previous`.
    Requery,
    /// Replaces a waiting query, whose reply is then rejected.
    Prepare,
}
/// PREPARE's own dedicated query. It replaces an outstanding read-only query
/// at once with a distinct nonce; that query's reply then no longer matches
/// and is rejected, so it can never serve as the preparation's proof.
fn prepare_board_query(state: &Arc<AppState>, board: Board) -> Response {
    send_board_query(state, board, QueryMode::Prepare)
}
fn send_board_query(state: &Arc<AppState>, board: Board, mode: QueryMode) -> Response {
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
    // Packet/clock/topology preparation stays outside the confirmation owner.
    // Only correlation mutation and the nonblocking SDK offer share its gate.
    coordinator::admit_external_query(state, || {
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
    if mode == QueryMode::Background && inner.queries[at].as_ref().is_some_and(|query| {
        !query.received
            && now
                .checked_duration_since(query.sent)
                .is_some_and(|age| age <= query_retry())
    }) {
        return error(
            StatusCode::CONFLICT,
            "query_pending",
            "This board already has an outstanding query; no duplicate request was queued.",
        );
    }
    // Install correlation before SDK queue admission. tx_queue never invokes callbacks.
    let previous = inner.queries[at].take();
    inner.queries[at] = Some(Query {
        abort_revision,
        nonce,
        sent: now,
        received: false,
    });
    if let Err(reason) = router.try_tx_queue(packet) {
        inner.queries[at] = previous;
        return rejected_query_offer(reason);
    }
    // A recovery PREPARE wants its own reply only; a re-query keeps the
    // superseded unanswered one acceptable.
    inner.previous[at] = if mode == QueryMode::Prepare { None } else {
        previous.filter(|query| !query.received
            && now.checked_duration_since(query.sent).is_some_and(|age| age <= QUERY_TTL))
    };
    drop(inner);
    Json(json!({"accepted":true,"operation":null,"message":"Query admitted to SDK; fresh board readback is still required."})).into_response()
    }).unwrap_or_else(||error(StatusCode::CONFLICT,"query_recovery_owned",
        "An immutable recovery operation owns this board query; wait for its terminal result."))
}
/// Seal new recovery proposals/confirmation atomically before extracting its
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

#[cfg(test)]
#[path = "board_recovery/testing_model_tests.rs"]
mod testing_model_tests;
#[cfg(test)]
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
            // A verified board that network health does not (yet) mark
            // healthy is still asked, at the slower cadence, instead of never;
            // unverified boards are not probed at all.
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
    async fn hardened_transport_bit_is_readonly_and_resync_bit_is_ab_vb_only() {
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
        // Bit 7 advertises Resync. It decodes, but only an AB/VB may claim it.
        let mut resync = wire;
        resync[3] |= RESYNC_IMPLEMENTATION;
        assert!(Readback::decode(&resync).is_some());
        wire[3] |= 0x40;
        let mut metadata = wire;
        metadata[2] = 255;
        metadata[3] = 0x64;
        assert_eq!(
            observe(&state, &status_packet("DAQ", metadata)),
            Err("Metadata node advertised physical manual Stop capability")
        );
        metadata[3] = 0x24 | RESYNC_IMPLEMENTATION;
        assert_eq!(
            observe(&state, &status_packet("DAQ", metadata)),
            Err("Board advertised Resync without a resync owner")
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
        assert!(remaining > 0 && remaining <= READBACK_TTL.as_millis() as u64 - 100, "query age, not fresh receipt, spends the server lease");
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
            .observed_after = Stamp::now() - READBACK_TTL - Duration::from_secs(1);
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
        assert!(row.fresh(start + Duration::from_secs(15)));
        assert!(!row.fresh(start + Duration::from_millis(15_001)));
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
    fn readback_freshness_preserves_paused_model_and_rejects_domain_loss() {
        let wall=WallInstant::now();
        let at=Stamp {wall:wall-Duration::from_secs(60),model_ms:Some(1000)};
        let row=observation(at);
        assert!(row.fresh(Stamp {wall,model_ms:Some(1000)}));
        assert!(row.fresh(Stamp {wall,model_ms:Some(16000)}));
        assert!(!row.fresh(Stamp {wall,model_ms:Some(16001)}));
        assert!(!row.fresh(Stamp {wall,model_ms:Some(999)}));
        assert!(!row.fresh(Stamp {wall,model_ms:None}));
        let wall_row=observation(Stamp {wall:wall-Duration::from_secs(16),model_ms:None});
        assert!(!wall_row.fresh(Stamp {wall,model_ms:None}));
    }
}

#[cfg(test)]
#[path = "board_recovery/status_policy_tests.rs"]
mod status_policy_tests;

#[cfg(test)]
mod recovery_requirement_tests {
    use super::*;
    #[tokio::test]
    async fn recovery_needed_http_uses_actual_board_authority_not_other_cached_rows() {
        let fixture = super::tests::http_fixture().await;
        let state = fixture.state.clone();
        state.board_abort.observe_abort(Board::ValveBoard, WallInstant::now(), 100);
        *state.state.lock().unwrap() = crate::types::FlightState::Launch;
        let now=Stamp::now();
        state.board_recovery.inner.lock().unwrap().values[index(Board::ActuatorBoard).unwrap()] = Some(Observation {
            received:now, observed_after:now,
            value:Readback {result:0,state:Some(15),flags:0x3d,nonce:1,boot:1,epoch:1,uptime:1,outputs:0,issuer:1}
        });
        let response = super::tests::http(state.clone(), "GET", "/api/board-recovery/status", Some("viewer"), Value::Null).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes=axum::body::to_bytes(response.into_body(),1024*1024).await.unwrap();
        let body:Value=serde_json::from_slice(&bytes).unwrap();
        for board in body["boards"].as_array().unwrap() {
            if board["id"]=="VB" {
                assert_eq!(board["recovery_required"],true);
                assert_eq!(board["recovery_reason"],"abort_latched");
            } else {
                assert_eq!(board["recovery_required"],false,"unrelated cached Abort/state/health cannot mark another board");
                assert!(board["recovery_reason"].is_null());
            }
        }
        assert!(state.topology_router.get().is_none(),"read-only badge metadata never installs a transport or sends commands");
    }
}
#[cfg(test)]
mod per_link_poll_tests {
    use super::*;
    /// Fill-board queries never wait for the radio's turn: one pass picks a
    /// due board on each link, each link keeps its own round-robin.
    #[test]
    fn each_link_gets_its_own_turn_every_pass() {
        let mut cursor = [0, 0];
        // RF and AB due: both are asked in the same pass.
        assert_eq!(pick_per_link(&mut cursor, |at| at == 0 || at == 3), vec![0, 3]);
        // Only fill boards due (the radio side is busy or cooling): the fill
        // side still advances through VB, GB, DAQ in turn.
        assert_eq!(pick_per_link(&mut cursor, |at| at >= 3), vec![4]);
        assert_eq!(pick_per_link(&mut cursor, |at| at >= 3), vec![5]);
        assert_eq!(pick_per_link(&mut cursor, |at| at >= 3), vec![6]);
        assert_eq!(pick_per_link(&mut cursor, |at| at >= 3), vec![3]);
        assert!(pick_per_link(&mut cursor, |_| false).is_empty());
    }
    /// A board network health marks unhealthy is re-asked every 10 s instead of
    /// never, so a board that comes back becomes fresh without an operator Query.
    #[test]
    fn an_unhealthy_board_is_re_queried_at_a_slower_cadence() {
        let start = WallInstant::now();
        assert!(readonly_query_due_for(None, start, false));
        assert!(!readonly_query_due_for(Some(start), start + Duration::from_secs(9), false));
        assert!(readonly_query_due_for(Some(start), start + UNHEALTHY_REQUERY, false));
        assert!(readonly_query_due_for(Some(start), start + Duration::from_secs(2), true));
    }
}
#[cfg(test)]
mod slow_link_freshness_tests {
    use super::*;
    fn status(state: &AppState, sender: &str, nonce: u64) -> Packet {
        let mut wire = [0u8; 40];
        wire[0] = 1; wire[2] = 1; wire[3] = 0x3e;
        wire[4..12].copy_from_slice(&nonce.to_le_bytes());
        wire[12..20].copy_from_slice(&42u64.to_le_bytes());
        wire[20..24].copy_from_slice(&9u32.to_le_bytes());
        wire[28..32].copy_from_slice(&6u32.to_le_bytes());
        wire[32..40].copy_from_slice(&issuer(&state.board_recovery.identity.unwrap()).to_le_bytes());
        Packet::new(crate::telemetry_schema::data_type(STATUS_TYPE),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")], sender, 1, Arc::from(wire)).unwrap()
    }
    /// Bench fault: replies arrive a few seconds late, after the next
    /// background re-query, so every reply named a superseded nonce and was
    /// rejected ("Expired or different recovery query"), and the 5 s launch
    /// proof was valid only now and then. The reply to the superseded query now
    /// still counts (from its own send time) and the proof outlasts the cycle.
    #[tokio::test]
    async fn a_late_reply_to_the_superseded_query_still_proves_the_board() {
        crate::telemetry_schema::initialize().unwrap();
        let state = crate::state::tests::test_app_state().await;
        let board = Board::ValveBoard;
        let at = index(board).unwrap();
        let revision = state.board_abort.query_revision(board);
        let old_sent = Stamp::now() - Duration::from_secs(4);
        {
            let mut inner = state.board_recovery.inner.lock().unwrap();
            inner.previous[at] = Some(Query { abort_revision: revision, nonce: 1, sent: old_sent, received: false });
            inner.queries[at] = Some(Query { abort_revision: revision, nonce: 2, sent: Stamp::now(), received: false });
        }
        assert_eq!(observe(&state, &status(&state, "VB", 1)), Ok(()), "the superseded query's late reply counts");
        {
            let inner = state.board_recovery.inner.lock().unwrap();
            let row = inner.values[at].as_ref().unwrap();
            assert_eq!(row.value.nonce, 1);
            assert!(row.observed_after == old_sent, "proof dates from the old query's send time");
            assert!(!inner.queries[at].as_ref().unwrap().received, "the current query is still pending");
        }
        assert!(state.board_abort.masks().1 & (1 << crate::board_abort::BOARDS.iter().position(|b| *b == board).unwrap()) != 0,
            "a 4 s old proof is still within the launch proof window");
        // A duplicate of the old reply never slides freshness; the current reply also counts.
        assert_eq!(observe(&state, &status(&state, "VB", 1)), Ok(()));
        assert_eq!(observe(&state, &status(&state, "VB", 2)), Ok(()));
        assert_eq!(state.board_recovery.inner.lock().unwrap().values[at].as_ref().unwrap().value.nonce, 2);
        // Once the current query is answered, an older reply is never taken.
        state.board_recovery.inner.lock().unwrap().previous[at] = Some(Query { abort_revision: revision, nonce: 3, sent: old_sent, received: false });
        assert!(observe(&state, &status(&state, "VB", 3)).is_err());
        assert!(observe(&state, &status(&state, "VB", 99)).is_err(), "an unknown nonce is rejected");
    }
    /// A board whose reply was lost is asked again well inside the launch
    /// proof window, instead of after the whole QUERY_TTL.
    /// Faithful bench run (S5=10, downlink lost): background readback
    /// queries to RF/PB/FC were retransmitted about every 3 s and exhausted
    /// their ordered retries every 5-7 s (197 BOARD_RECOVERY_REQUEST in 75 s)
    /// while no reply ever arrived. Unanswered background queries now back
    /// off per board, 3 -> 6 -> 12 -> 24 -> 30 s, and reset on any reply.
    #[test]
    fn unanswered_background_queries_back_off_per_board_and_reset_on_reply() {
        let start = WallInstant::now();
        let at = |s: u64| start + Duration::from_secs(s);
        // No unanswered queries: the normal two-second cadence.
        assert!(readonly_backoff_due(Some(start), at(2), 0));
        for (misses, wait) in [(1u8, 3u64), (2, 6), (3, 12), (4, 24), (5, 30), (9, 30)] {
            assert!(!readonly_backoff_due(Some(start), at(wait) - Duration::from_millis(1), misses), "{misses}");
            assert!(readonly_backoff_due(Some(start), at(wait), misses), "{misses}");
        }
        let sent = Stamp::now();
        let pending = Query { abort_revision: 0, nonce: 1, sent, received: false };
        let answered = Query { received: true, ..pending };
        assert_eq!(next_readonly_misses(Some(&pending), 0), 1);
        assert_eq!(next_readonly_misses(Some(&pending), 3), 4);
        assert_eq!(next_readonly_misses(Some(&pending), 200), 200, "bounded");
        assert_eq!(next_readonly_misses(Some(&answered), 4), 0, "a reply resets it");
        assert_eq!(next_readonly_misses(None, 4), 0);
    }
    /// accept3-hitl (fadfdbc): VB/GB/DAQ background queries queued behind an
    /// AV-bay query in the one ordered BOARD_RECOVERY_REQUEST stream and were
    /// dropped with it when the radio exhausted its retries. Each loss counted
    /// as a miss, so the boards were asked only every 12-30 s and their
    /// readback expired while they kept sending heartbeats; an Abort then
    /// showed 33-36 s late. An umbilical board the station plainly hears never
    /// backs off; an AV-bay board, or an umbilical board gone quiet, still does.
    #[test]
    fn a_healthy_umbilical_board_never_backs_off() {
        for at in 3..7 {
            assert_eq!(effective_readonly_misses(at, true, false, 4), 0, "{at}");
            assert_eq!(effective_readonly_misses(at, false, false, 4), 4, "{at}");
        }
        for at in 0..3 {
            assert_eq!(effective_readonly_misses(at, true, false, 4), 4, "{at}");
        }
        assert_eq!(effective_readonly_misses(0, true, true, 4), 0, "a reply resets it");
    }
    #[test]
    fn only_the_av_bay_link_is_held_after_an_abort() {
        for at in 0..7 {
            assert!(!link_held(at, false));
            assert_eq!(link_held(at, true), at < 3, "{at}");
        }
    }
    #[test]
    fn an_unanswered_query_is_retried_inside_the_launch_proof_window() {
        assert!(QUERY_RETRY < crate::board_abort::PROOF_TTL);
        assert!(crate::board_abort::PROOF_TTL < READBACK_TTL);
    }
}
#[cfg(test)]
mod launch_lock_release_tests {
    use super::*;
    fn row(board: Board, state: u8, flags: u8, epoch: u32) -> Observation {
        // Sampled a moment ago, so a later "now" sees it fresh.
        let now = Stamp::now() - Duration::from_secs(1);
        Observation {
            received: now,
            observed_after: now,
            value: Readback { result: 0, state: Some(state), flags, nonce: 9, boot: 42, epoch, uptime: 1,
                outputs: safe_output_mask(board).unwrap_or(0), issuer: 1 },
        }
    }
    const PHYSICAL_OK: u8 = 0x04 | 0x20 | STATE_VALID | OUTPUTS_VALID | 2;
    const METADATA_OK: u8 = 0x04 | 0x20 | STATE_VALID;
    fn target(board: Board, epoch: u32) -> crate::launch_history::RecoveryTarget {
        crate::launch_history::RecoveryTarget {
            board: board.sender_id().into(), desired: 0, boot: 42, epoch,
            intent: crate::launch_history::RecoveryIntent { body: [0; 32], source_utc_ms: 1,
                backend: "0".repeat(32), principal_context: "0".repeat(32) },
            kind: crate::launch_history::RecoveryIntentKind::Normal,
            original_first: None, uncertain_first: None,
        }
    }
    #[test]
    fn healthy_idle_needs_no_abort_startup_or_idle_and_safe_outputs() {
        let now = Stamp::now();
        assert!(readback_healthy_idle(Board::ActuatorBoard, &row(Board::ActuatorBoard, 0, PHYSICAL_OK, 3), now));
        assert!(readback_healthy_idle(Board::ValveBoard, &row(Board::ValveBoard, 1, PHYSICAL_OK, 3), now));
        assert!(readback_healthy_idle(Board::DaqBoard, &row(Board::DaqBoard, 1, METADATA_OK, 3), now));
        assert!(!readback_healthy_idle(Board::ActuatorBoard, &row(Board::ActuatorBoard, 0, PHYSICAL_OK | 1, 3), now), "aborted");
        assert!(!readback_healthy_idle(Board::ActuatorBoard, &row(Board::ActuatorBoard, 7, PHYSICAL_OK, 3), now), "Launch state");
        assert!(!readback_healthy_idle(Board::ActuatorBoard, &row(Board::ActuatorBoard, 0, PHYSICAL_OK & !2, 3), now), "unsafe outputs");
        let mut unsafe_outputs = row(Board::ValveBoard, 1, PHYSICAL_OK, 3);
        unsafe_outputs.value.outputs = 1;
        assert!(!readback_healthy_idle(Board::ValveBoard, &unsafe_outputs, now));
        assert!(!readback_healthy_idle(Board::ActuatorBoard, &row(Board::ActuatorBoard, 0, PHYSICAL_OK, 3), now + READBACK_TTL + Duration::from_millis(1)), "stale");
    }
    #[test]
    fn pending_target_is_satisfied_only_by_a_fresh_healthy_same_identity_readback() {
        let now = Stamp::now();
        let ab = Board::ActuatorBoard;
        assert!(pending_target_satisfied(ab, &target(ab, 3), Some(&row(ab, 0, PHYSICAL_OK, 3)), true, now), "same epoch");
        assert!(pending_target_satisfied(ab, &target(ab, 3), Some(&row(ab, 1, PHYSICAL_OK, 4)), true, now), "newer epoch");
        assert!(!pending_target_satisfied(ab, &target(ab, 4), Some(&row(ab, 0, PHYSICAL_OK, 3)), true, now), "older epoch");
        assert!(!pending_target_satisfied(ab, &target(ab, 3), Some(&row(ab, 0, PHYSICAL_OK | 1, 3)), true, now), "aborted");
        assert!(!pending_target_satisfied(ab, &target(ab, 3), Some(&row(ab, 0, PHYSICAL_OK, 3)), false, now), "endpoint unverified");
        assert!(!pending_target_satisfied(ab, &target(ab, 3), None, true, now), "no readback");
        let mut rebooted = row(ab, 0, PHYSICAL_OK, 3);
        rebooted.value.boot = 43;
        assert!(!pending_target_satisfied(ab, &target(ab, 3), Some(&rebooted), true, now), "identity changed");
        let mut first = target(ab, 3);
        first.kind = crate::launch_history::RecoveryIntentKind::First;
        assert!(!pending_target_satisfied(ab, &first, Some(&row(ab, 0, PHYSICAL_OK, 3)), true, now), "commissioning uncertainty");
        assert!(!pending_target_satisfied(Board::ValveBoard, &target(ab, 3), Some(&row(Board::ValveBoard, 0, PHYSICAL_OK, 3)), true, now), "other board");
    }
    #[test]
    fn launch_lock_releases_only_when_every_present_required_board_is_healthy() {
        let ab = 1 << index(Board::ActuatorBoard).unwrap();
        let vb = 1 << index(Board::ValveBoard).unwrap();
        let daq = 1 << index(Board::DaqBoard).unwrap();
        assert!(launch_lock_release_ready(true, ab | vb, ab | vb));
        assert!(launch_lock_release_ready(true, ab | vb, ab | vb | daq), "a missing board does not block");
        assert!(!launch_lock_release_ready(true, ab | vb, ab), "an aborted or unproven board blocks");
        assert!(!launch_lock_release_ready(true, 0, 0), "an empty network never unlocks Launch");
        assert!(!launch_lock_release_ready(false, ab, ab), "station not in Startup/Idle or a board Abort/missed reset");
    }
    #[tokio::test]
    async fn tclock_reset_alone_never_releases_and_an_aborted_board_still_blocks() {
        let state = crate::state::tests::test_app_state().await;
        state.launch_history.lock().unwrap().inhibit_automatic_actions().unwrap();
        state.try_reset_launch_clock().ok();
        assert!(!try_release_launch_lock(&state), "no fresh board proof: a T-clock reset cannot unlock");
        assert!(state.launch_history.lock().unwrap().launch_locked());
        // A board readback that reports Abort is never healthy.
        state.board_recovery.inner.lock().unwrap().values[index(Board::ActuatorBoard).unwrap()] =
            Some(row(Board::ActuatorBoard, 15, PHYSICAL_OK | 1, 3));
        assert!(!try_release_launch_lock(&state));
        assert!(state.launch_history.lock().unwrap().launch_locked());
    }
}
#[cfg(test)]
mod abort_push_tests {
    use super::*;
    /// The 40-byte status a board pushes when it latches an Abort: nonce 0,
    /// issuer as last seen (0 before any query), boot generation 2.
    fn push(flags: u8, epoch: u32, uptime: u32) -> Packet {
        let mut wire = [0u8; 40];
        wire[0] = 1;
        wire[2] = crate::types::FlightState::Aborted as u8;
        wire[3] = STATE_VALID | flags;
        wire[12..20].copy_from_slice(&2u64.to_le_bytes());
        wire[20..24].copy_from_slice(&epoch.to_le_bytes());
        wire[24..28].copy_from_slice(&uptime.to_le_bytes());
        Packet::new(
            crate::telemetry_schema::data_type(STATUS_TYPE),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "AB",
            123,
            Arc::from(wire),
        )
        .unwrap()
    }
    async fn verified_ab() -> Arc<AppState> {
        let state = crate::state::tests::test_app_state().await;
        let host = Arc::new(sedsnet::router::Router::new(sedsnet::router::RouterConfig::new([]).with_sender("GS")));
        let side = host.add_side_packed("umbilical", |_| Ok(()));
        for board in [Board::ActuatorBoard, Board::ValveBoard] {
            host.rx_from_side(
                &sedsnet::discovery::build_discovery_announce(
                    board.sender_id(), 1, &[crate::telemetry_schema::endpoint(endpoint(board))]).unwrap(),
                side,
            )
            .unwrap();
        }
        state.topology_router.set(host).unwrap();
        state
    }
    const AB: usize = 3;

    #[tokio::test]
    async fn a_pushed_abort_latch_raises_the_abort_at_once() {
        let state = verified_ab().await;
        assert_eq!(state.board_abort.masks().0 & (1 << AB), 0);
        observe(&state, &push(1, 9, 1_000)).unwrap();
        assert_ne!(state.board_abort.masks().0 & (1 << AB), 0, "AB reads Aborted from its push");
    }

    #[tokio::test]
    async fn a_push_is_never_readback_or_launch_and_ignition_proof() {
        let state = verified_ab().await;
        observe(&state, &push(1, 9, 1_000)).unwrap();
        let inner = state.board_recovery.inner.lock().unwrap();
        assert!(inner.values[AB].is_none(), "no readback row from a push");
        assert!(inner.queries[AB].is_none(), "no query answered by a push");
        drop(inner);
        assert_eq!(state.board_abort.masks().1 & (1 << AB), 0, "no launch proof");
        assert!(!state.board_abort.ignition_pair_current(), "no ignition-pair proof");
        // A push with the latch flag that also claims healthy outputs and a
        // clear state is still only an Abort raise.
        let mut wire = push(1 | 4 | OUTPUTS_VALID | 2, 10, 2_000).payload().to_vec();
        wire[2] = crate::types::FlightState::Idle as u8;
        let packet = Packet::new(crate::telemetry_schema::data_type(STATUS_TYPE),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")], "AB", 124, Arc::from(wire)).unwrap();
        observe(&state, &packet).unwrap();
        assert!(state.board_recovery.inner.lock().unwrap().values[AB].is_none());
        assert_eq!(state.board_abort.masks().1 & (1 << AB), 0);
        assert_ne!(state.board_abort.masks().0 & (1 << AB), 0);
    }

    #[tokio::test]
    async fn a_replayed_or_older_push_is_ignored() {
        let state = verified_ab().await;
        observe(&state, &push(1, 9, 1_000)).unwrap();
        let revision = state.board_abort.abort_revision();
        assert!(observe(&state, &push(1, 9, 1_000)).is_err(), "the same push again");
        assert!(observe(&state, &push(1, 8, 5_000)).is_err(), "an older epoch");
        assert!(observe(&state, &push(1, 9, 900)).is_err(), "earlier in the same epoch");
        assert_eq!(state.board_abort.abort_revision(), revision, "a replay raises nothing");
        // Older than the board's latest readback (taken after a recovery).
        state.board_recovery.inner.lock().unwrap().values[AB] = Some(Observation {
            value: Readback { result: 0, state: Some(1), flags: STATE_VALID, nonce: 5, boot: 2, epoch: 11,
                uptime: 9_000, outputs: 0, issuer: 1 },
            received: Stamp::now(),
            observed_after: Stamp::now(),
        });
        assert!(observe(&state, &push(1, 10, 8_000)).is_err(), "older than the latest readback");
        assert_eq!(state.board_abort.abort_revision(), revision);
        // A later latch is a new Abort.
        observe(&state, &push(1, 11, 9_500)).unwrap();
        assert!(state.board_abort.abort_revision() > revision);
    }

    #[tokio::test]
    async fn a_push_without_the_latch_flag_or_from_an_unverified_board_does_nothing() {
        let state = verified_ab().await;
        let revision = state.board_abort.abort_revision();
        assert!(observe(&state, &push(0, 9, 1_000)).is_err());
        assert_eq!(state.board_abort.masks().0 & (1 << AB), 0);
        assert_eq!(state.board_abort.abort_revision(), revision);
        assert!(state.board_recovery.inner.lock().unwrap().values[AB].is_none());
        // It never clears an Abort either.
        observe(&state, &push(1, 9, 1_000)).unwrap();
        assert!(observe(&state, &push(0, 10, 2_000)).is_err());
        assert_ne!(state.board_abort.masks().0 & (1 << AB), 0);
        // GB never announced its endpoint here.
        let mut wire = push(1, 9, 1_000).payload().to_vec();
        wire[20] = 9;
        let gb = Packet::new(crate::telemetry_schema::data_type(STATUS_TYPE),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")], "GB", 125, Arc::from(wire)).unwrap();
        assert!(observe(&state, &gb).is_err());
        assert_eq!(state.board_abort.masks().0 & (1 << 5), 0);
    }
}
