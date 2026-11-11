//! Retirement of stale AUTOMATIC aborts.
//!
//! An automatic ABORT the station raised (board offline, overpressure, a
//! safety fault) is an end-to-end reliable frame. On a dead or saturated
//! radio the SDK keeps retrying it for as long as the radio refuses it, so a
//! frame raised once could still land on the AV bay an hour later, long after
//! the condition cleared, and abort it out of nowhere (bench, 03:00:23).
//!
//! A pending automatic ABORT is retired once any of these holds:
//! (a) the operator cleared the GSE abort;
//! (b) every pending destination proved, by a validated status reply to a
//!     query issued after the abort was raised, that it is not aborted, and
//!     the abort is past its normal delivery window ([`undeliverable_after`]),
//!     so a frame still in ordinary flight is never cancelled by a reply that
//!     simply raced it;
//! (c) it is older than [`AUTOMATIC_ABORT_MAX_AGE`] AND the condition that
//!     raised it is no longer present (the offline board is back, the
//!     overpressure cleared). While its cause persists, or when the cause
//!     cannot be evaluated, an automatic abort is never retired by age.
//!
//! Manual operator Aborts are never registered here and keep their delivery.
//!
//! The SDK has no API to cancel a pending reliable packet. A retired frame is
//! instead refused at the station's own transport writer: the radio budget
//! accepts any copy the router offers and drops it, and the writers drop any
//! copy already queued or parked, so it never reaches the air. Each dropped
//! offer counts as a send to the SDK, whose bounded retry budget then gives
//! the frame up. Until it does, station checks that wait on a pending ABORT
//! ignore a retired one (see [`try_has_pending_reliable_tx_type`]).
//!
//! [`undeliverable_after`]: crate::gse::undeliverable_after
use crate::state::AppState;
use sedsnet::router::{Router, TryPendingTypeError};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

/// The age past which an automatic ABORT whose cause has cleared is retired. A small ABORT frame takes
/// under 100 ms of RFD airtime and the SDK's whole retry budget is 4.8 s
/// (12 attempts 400 ms apart); the station already calls an ABORT
/// undeliverable after 5.8 s (crate::gse::undeliverable_after). 30 s is five of those windows: an abort that has
/// not landed by then is waiting on a dead or saturated link, and landing it
/// later would abort a board on a condition that is gone. A cause still
/// present keeps it in delivery, however old.
pub(crate) const AUTOMATIC_ABORT_MAX_AGE: Duration = Duration::from_secs(30);
/// A retired id stays refused this long after the SDK has let it go, so a
/// copy already handed to a writer queue is still dropped.
const RETIRED_SUPPRESSION: Duration = Duration::from_secs(300);
/// Bounds the registry. Automatic aborts are latched one per Clear.
const MAX_ENTRIES: usize = 64;
/// How often [`spawn`] re-evaluates pending automatic aborts.
const EVALUATE_EVERY: Duration = Duration::from_millis(250);

/// Who raised a pending ABORT frame.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AbortOrigin {
    /// Raised by the station itself; eligible for retirement.
    Automatic,
    /// Anything not registered as automatic: an operator Abort. Never retired.
    Manual,
}

/// What raised an automatic ABORT.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AbortCause {
    /// A board stopped reporting.
    BoardOffline(crate::types::Board),
    /// Tank overpressure.
    Overpressure,
    /// A cause the station cannot re-evaluate (its text).
    Unknown(String),
}

/// Whether an automatic abort's cause still holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CauseState {
    Present,
    Cleared,
    /// Not evaluable: treated as present.
    Unknown,
}

impl AbortCause {
    pub(crate) fn state(&self, state: &AppState) -> CauseState {
        match self {
            Self::BoardOffline(board) => {
                if state.operational_board_recent(*board, crate::state::BOARD_SEEN_TIMEOUT_MS) {
                    CauseState::Cleared
                } else {
                    CauseState::Present
                }
            }
            Self::Overpressure => {
                if crate::gse::overpressure_holds_abort(state) {
                    CauseState::Present
                } else {
                    CauseState::Cleared
                }
            }
            Self::Unknown(_) => CauseState::Unknown,
        }
    }
    fn describe(&self) -> String {
        match self {
            Self::BoardOffline(board) => format!("{} offline", board.as_str()),
            Self::Overpressure => "tank overpressure".into(),
            Self::Unknown(text) => text.clone(),
        }
    }
    fn describe_state(&self, cause_state: CauseState) -> &'static str {
        match (self, cause_state) {
            (_, CauseState::Unknown) => "cannot be evaluated; treated as present",
            (Self::BoardOffline(_), CauseState::Cleared) => "cleared: the board is back online",
            (Self::BoardOffline(_), CauseState::Present) => "present: the board is still offline",
            (Self::Overpressure, CauseState::Cleared) => "cleared: no overpressure is latched",
            (Self::Overpressure, CauseState::Present) => "present: the overpressure is still latched",
            (Self::Unknown(_), _) => "cannot be evaluated; treated as present",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RetireReason {
    OperatorCleared,
    DestinationsReportedNonAbort,
    Expired,
}
impl RetireReason {
    fn describe(self) -> String {
        match self {
            Self::OperatorCleared => "the operator cleared the GSE abort".into(),
            Self::DestinationsReportedNonAbort => {
                "every board it was owed to reported a fresh non-Abort state after it was raised".into()
            }
            Self::Expired => format!(
                "it was still undelivered after {} s and its cause has cleared",
                AUTOMATIC_ABORT_MAX_AGE.as_secs()
            ),
        }
    }
}

struct Entry {
    /// The router that holds the frame. A dropped router's handle never
    /// upgrades, so a later Router that reuses its address (tests create and
    /// drop many) never inherits its entries.
    router: std::sync::Weak<Router>,
    packet_id: u64,
    reason: String,
    cause: AbortCause,
    raised_at: Instant,
    /// Boards still owed the frame, as `board_abort::BOARDS` bits.
    destinations: u8,
    /// A pending destination the station cannot name.
    unnamed: bool,
    /// Seen in the SDK's pending table at least once.
    seen_pending: bool,
    /// Status queries were requested for its destinations.
    queried: bool,
    retired: Option<RetireReason>,
    /// When the SDK stopped listing it.
    gone_since: Option<Instant>,
}

static ENTRIES: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
/// Retired entries in [`ENTRIES`]; zero keeps the writer check lock-free.
static RETIRED: AtomicUsize = AtomicUsize::new(0);

/// Every entry stays a valid record if a panic interrupts an update, so a
/// poisoned lock is taken over rather than failing every later ABORT check.
/// The retired count is rebuilt because the panic may have preceded it.
fn entries() -> std::sync::MutexGuard<'static, Vec<Entry>> {
    ENTRIES.lock().unwrap_or_else(|poisoned| {
        let entries = poisoned.into_inner();
        recount(&entries);
        ENTRIES.clear_poison();
        entries
    })
}

/// Whether `entry` belongs to `router`: the very Router it was registered
/// with, still alive, not merely one at the same address.
fn owned_by(entry: &Entry, router: &Router) -> bool {
    std::ptr::eq(entry.router.as_ptr(), router) && entry.router.strong_count() > 0
}

fn abort_type() -> sedsnet::config::DataType {
    crate::telemetry_schema::data_type("ABORT")
}

fn recount(entries: &[Entry]) {
    RETIRED.store(entries.iter().filter(|e| e.retired.is_some()).count(), Ordering::Release);
}

/// Record an automatic ABORT the router has just accepted.
pub(crate) fn register_automatic(router: &Arc<Router>, packet_id: u64, reason: &str, cause: AbortCause) {
    let mut entries = entries();
    // Entries of dropped routers are dead weight; their frames can no longer
    // be offered to a writer.
    entries.retain(|e| e.router.strong_count() > 0);
    if entries.iter().any(|e| owned_by(e, router) && e.packet_id == packet_id) {
        return;
    }
    while entries.len() >= MAX_ENTRIES {
        // Drop the oldest entry the SDK no longer holds; else the oldest.
        let index = entries.iter().position(|e| e.gone_since.is_some()).unwrap_or(0);
        entries.remove(index);
    }
    entries.push(Entry {
        router: Arc::downgrade(router),
        packet_id,
        reason: reason.to_string(),
        cause,
        raised_at: Instant::now(),
        destinations: 0,
        unnamed: false,
        seen_pending: false,
        queried: false,
        retired: None,
        gone_since: None,
    });
    recount(&entries);
}

#[cfg(test)]
pub(crate) fn origin(router: &Router, packet_id: u64) -> AbortOrigin {
    if entries().iter().any(|e| owned_by(e, router) && e.packet_id == packet_id) {
        AbortOrigin::Automatic
    } else {
        AbortOrigin::Manual
    }
}

pub(crate) fn is_retired(router: &Router, packet_id: u64) -> bool {
    if RETIRED.load(Ordering::Acquire) == 0 {
        return false;
    }
    entries()
        .iter()
        .any(|e| owned_by(e, router) && e.packet_id == packet_id && e.retired.is_some())
}

/// The radio writer's check: true for a canonical ABORT frame whose packet id
/// was retired. Such a copy must be dropped, never written.
pub(crate) fn suppress_write(payload: &[u8]) -> bool {
    if RETIRED.load(Ordering::Acquire) == 0 || payload.starts_with(b"SDT") {
        // Reliable end-to-end frames are never SDT-compacted by the SDK.
        return false;
    }
    // Cheap type peek first: this runs on every radio offer, including
    // refusal retries. Only an ABORT pays for the CRC-checked id below.
    if !sedsnet::wire_format::peek_frame_info_unchecked(payload).is_ok_and(|info| info.envelope.ty == abort_type()) {
        return false;
    }
    let Ok(id) = sedsnet::wire_format::packet_id_from_wire(payload) else {
        return false;
    };
    entries()
        .iter()
        .any(|e| e.packet_id == id && e.retired.is_some())
}

/// `Router::try_has_pending_reliable_tx_type`, except that an ABORT is not
/// pending once every tracked ABORT left is a retired automatic one.
pub(crate) fn try_has_pending_reliable_tx_type(
    router: &Router,
    ty: sedsnet::config::DataType,
) -> Result<bool, TryPendingTypeError> {
    let pending = router.try_has_pending_reliable_tx_type(ty)?;
    if !pending || ty != abort_type() || RETIRED.load(Ordering::Acquire) == 0 {
        return Ok(pending);
    }
    let rows = router.export_reliable_pending_stats();
    let mut aborts = rows.iter().filter(|row| row.data_type == ty).peekable();
    // Pending but not yet tracked: a new frame still queued in the SDK.
    if aborts.peek().is_none() {
        return Ok(true);
    }
    Ok(aborts.any(|row| !is_retired(router, row.packet_id)))
}

/// Blocking form of [`try_has_pending_reliable_tx_type`].
pub(crate) fn has_pending_reliable_tx_type(router: &Router, ty: sedsnet::config::DataType) -> bool {
    loop {
        match try_has_pending_reliable_tx_type(router, ty) {
            Ok(pending) => return pending,
            Err(TryPendingTypeError::CoreBusy) => std::thread::yield_now(),
        }
    }
}

/// The operator cleared the GSE abort: retire every automatic ABORT this
/// router still holds. Returns how many were retired.
pub(crate) fn retire_on_operator_clear(state: &AppState, router: &Router) -> usize {
    let any_abort_pending = router.try_has_pending_reliable_tx_type(abort_type()).unwrap_or(true);
    let rows = router.export_reliable_pending_stats();
    let causes = cause_states(state, router);
    let retired = {
        let mut entries = entries();
        refresh_locked(&mut entries, router, &rows, any_abort_pending, Instant::now());
        let mut retired = Vec::new();
        for entry in entries.iter_mut().filter(|e| owned_by(e, router)) {
            if entry.retired.is_none() && entry.gone_since.is_none() {
                entry.retired = Some(RetireReason::OperatorCleared);
                retired.push(Retired::of(entry, &causes));
            }
        }
        recount(&entries);
        retired
    };
    let count = retired.len();
    announce(state, retired);
    count
}

/// Apply (b) and (c) to this router's pending automatic aborts.
pub(crate) fn evaluate(state: &AppState, router: &Router, now: Instant) {
    if entries().is_empty() {
        return;
    }
    let any_abort_pending = router.try_has_pending_reliable_tx_type(abort_type()).unwrap_or(true);
    let rows = router.export_reliable_pending_stats();
    let delivery_window = crate::sim_freshness::wall(crate::gse::undeliverable_after());
    let max_age = crate::sim_freshness::wall(AUTOMATIC_ABORT_MAX_AGE);
    let causes = cause_states(state, router);
    let (retired, query) = {
        let mut entries = entries();
        refresh_locked(&mut entries, router, &rows, any_abort_pending, now);
        let mut retired = Vec::new();
        let mut query = 0u8;
        for entry in entries.iter_mut().filter(|e| owned_by(e, router)) {
            if entry.retired.is_some() || entry.gone_since.is_some() {
                continue;
            }
            let age = now.saturating_duration_since(entry.raised_at);
            let cleared = causes.iter().any(|(id, c)| *id == entry.packet_id && *c == CauseState::Cleared);
            if age >= max_age && cleared {
                entry.retired = Some(RetireReason::Expired);
            } else if age >= delivery_window && entry.seen_pending && entry.destinations != 0 && !entry.unnamed {
                let proven = crate::board_abort::BOARDS.iter().enumerate()
                    .filter(|(i, _)| entry.destinations & (1 << i) != 0)
                    .all(|(_, board)| state.board_abort.non_abort_proven_after(*board, entry.raised_at));
                if proven {
                    entry.retired = Some(RetireReason::DestinationsReportedNonAbort);
                } else if !entry.queried {
                    // Ask once, so (b) can be proven without an operator.
                    entry.queried = true;
                    query |= entry.destinations;
                }
            }
            if entry.retired.is_some() {
                retired.push(Retired::of(entry, &causes));
            }
        }
        recount(&entries);
        (retired, query)
    };
    if query != 0 {
        state.board_abort.request_queries(query);
    }
    announce(state, retired);
}

/// Each unretired entry's cause state, read without holding the registry
/// (the predicates take station locks).
fn cause_states(state: &AppState, router: &Router) -> Vec<(u64, CauseState)> {
    let causes = entries()
        .iter()
        .filter(|e| owned_by(e, router) && e.retired.is_none())
        .map(|e| (e.packet_id, e.cause.clone()))
        .collect::<Vec<_>>();
    causes.into_iter().map(|(id, cause)| (id, cause.state(state))).collect()
}

/// Update destinations and SDK presence; forget long-gone entries.
fn refresh_locked(
    entries: &mut Vec<Entry>,
    router: &Router,
    rows: &[sedsnet::diagnostics::ReliablePendingStats],
    any_abort_pending: bool,
    now: Instant,
) {
    for entry in entries.iter_mut().filter(|e| owned_by(e, router)) {
        match rows.iter().find(|row| row.packet_id == entry.packet_id) {
            Some(row) => {
                entry.seen_pending = true;
                entry.gone_since = None;
                let (mut mask, mut unnamed) = (0u8, false);
                for owner in &row.pending_destinations {
                    match crate::board_recovery::coordinator::pending_destination_board(router, *owner)
                        .and_then(|board| crate::board_abort::BOARDS.iter().position(|b| *b == board))
                    {
                        Some(i) => mask |= 1 << i,
                        None => unnamed = true,
                    }
                }
                entry.destinations = mask;
                entry.unnamed = unnamed;
            }
            // Never seen yet may still be queued ahead of tracking; it is gone
            // once the SDK holds no ABORT at all, or tracks others but not it
            // past the delivery window (delivered before the first look).
            None if entry.seen_pending
                || entry.retired.is_some()
                || !any_abort_pending
                || (rows.iter().any(|row| row.data_type == abort_type())
                    && now.saturating_duration_since(entry.raised_at)
                        >= crate::sim_freshness::wall(crate::gse::undeliverable_after())) =>
            {
                entry.gone_since.get_or_insert(now);
            }
            None => {}
        }
    }
    // Also drop the entries of routers that no longer exist.
    entries.retain(|e| {
        e.router.strong_count() > 0
            && (!owned_by(e, router)
                || e.gone_since.is_none_or(|gone| {
                    let keep = if e.retired.is_some() { RETIRED_SUPPRESSION } else { Duration::ZERO };
                    now.saturating_duration_since(gone) < keep
                }))
    });
}

struct Retired {
    packet_id: u64,
    reason_text: String,
    cause: AbortCause,
    cause_state: CauseState,
    destinations: u8,
    unnamed: bool,
    reason: RetireReason,
    age: Duration,
}
impl Retired {
    fn of(entry: &Entry, causes: &[(u64, CauseState)]) -> Self {
        Self {
            packet_id: entry.packet_id,
            reason_text: entry.reason.clone(),
            cause: entry.cause.clone(),
            cause_state: causes
                .iter()
                .find(|(id, _)| *id == entry.packet_id)
                .map_or(CauseState::Unknown, |(_, c)| *c),
            destinations: entry.destinations,
            unnamed: entry.unnamed,
            reason: entry.retired.expect("retired entry"),
            age: entry.raised_at.elapsed(),
        }
    }
    fn destinations(&self) -> String {
        let mut names = crate::board_abort::board_names(self.destinations);
        if self.unnamed || names.is_empty() {
            if !names.is_empty() {
                names.push_str(", ");
            }
            names.push_str("an unnamed board");
        }
        names
    }
}

fn announce(state: &AppState, retired: Vec<Retired>) {
    for r in retired {
        let destinations = r.destinations();
        let cause = format!("{} ({})", r.cause.describe(), r.cause.describe_state(r.cause_state));
        log::warn!(
            "automatic_abort_retired id={:#x} text={:?} cause={:?} cause_state={:?} reason={:?} age_ms={} destinations=[{}]",
            r.packet_id,
            r.reason_text,
            r.cause,
            r.cause_state,
            r.reason,
            r.age.as_millis(),
            destinations
        );
        state.add_notification(format!(
            "Stale automatic Abort retired: it was still owed to {destinations} and will not be sent again, because {}; cause: {cause}. Send a new Abort if one is still needed.",
            r.reason.describe()
        ));
    }
}

/// Re-evaluate pending automatic aborts until the station shuts down.
pub(crate) fn spawn(state: Arc<AppState>, router: Arc<Router>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(EVALUATE_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        while !state.shutdown_requested.load(Ordering::Acquire) {
            tick.tick().await;
            let (state, router) = (state.clone(), router.clone());
            // The SDK pending table takes the router's core lock.
            let _ = tokio::task::spawn_blocking(move || evaluate(&state, &router, Instant::now())).await;
        }
    })
}

#[cfg(test)]
pub(crate) fn retired_reason(router: &Router, packet_id: u64) -> Option<RetireReason> {
    entries().iter()
        .find(|e| owned_by(e, router) && e.packet_id == packet_id)
        .and_then(|e| e.retired)
}

#[cfg(test)]
mod poison_tests {
    use super::*;
    /// A panic part-way through an update must not stop later ABORT checks;
    /// the next update rebuilds the retired count from what was written.
    #[test]
    fn a_panic_inside_the_entries_lock_keeps_abort_retirement_working() {
        // Own process: this poisons a process-wide lock on purpose.
        if crate::operator_queue::isolated_case(
            module_path!(),
            "a_panic_inside_the_entries_lock_keeps_abort_retirement_working",
        ) {
            return;
        }
        let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([])));
        register_automatic(&router, 7, "test", AbortCause::Unknown("test".into()));
        let held = router.clone();
        let _ = std::thread::spawn(move || {
            let mut entries = ENTRIES.lock().unwrap();
            for entry in entries.iter_mut().filter(|e| owned_by(e, &held)) {
                entry.retired = Some(RetireReason::OperatorCleared);
            }
            panic!("interrupted before the retired count was updated");
        })
        .join();
        assert!(ENTRIES.is_poisoned());
        // The next locked update takes the lock over and rebuilds the count.
        register_automatic(&router, 8, "test", AbortCause::Unknown("test".into()));
        assert!(!ENTRIES.is_poisoned());
        assert!(is_retired(&router, 7));
        assert!(!is_retired(&router, 8));
    }
}

#[cfg(test)]
mod router_identity_tests {
    use super::*;

    /// A Router created where a dropped one lived must not inherit its
    /// automatic ABORTs: Router::new is cheap and tests make many, so the
    /// allocator hands the same address straight back.
    #[tokio::test]
    async fn a_router_at_a_reused_address_does_not_inherit_dropped_aborts() {
        // Own process: the registry is process-wide.
        if crate::operator_queue::isolated_case(
            module_path!(),
            "a_router_at_a_reused_address_does_not_inherit_dropped_aborts",
        ) {
            return;
        }
        crate::telemetry_schema::initialize().unwrap();
        let state = crate::state::tests::test_app_state().await;
        let make = || Router::new(sedsnet::router::RouterConfig::new([]).with_sender("GS"));
        let first = Arc::new(make());
        let address = Arc::as_ptr(&first) as usize;
        // Registered but never seen pending: exactly the entry a stale lookup
        // keeps alive within the delivery window.
        register_automatic(&first, 0x5EED, "old router", AbortCause::Unknown("old".into()));
        // Build the next Router's internals first, so the only allocation
        // after the drop is its Arc block, which the allocator would place
        // in the freed slot. (A registry entry pins that slot, so with the
        // fix the address is never handed out again while it is tracked.)
        let mut next = make();
        drop(first);
        let mut candidates = Vec::new();
        for _ in 0..16 {
            candidates.push(Arc::new(next));
            next = make();
        }
        let reused = candidates.iter().filter(|r| Arc::as_ptr(r) as usize == address).count();
        for router in &candidates {
            assert_eq!(
                origin(router, 0x5EED),
                AbortOrigin::Manual,
                "a new router inherited a dropped router's ABORT (address reused: {})",
                Arc::as_ptr(router) as usize == address
            );
            assert_eq!(retire_on_operator_clear(&state, router), 0);
            assert_eq!(retired_reason(router, 0x5EED), None);
        }
        eprintln!("candidates at the dropped router's address: {reused}");
    }
}
