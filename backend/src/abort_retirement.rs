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
//!     the abort is past its normal delivery window ([`UNDELIVERABLE_AFTER`]),
//!     so a frame still in ordinary flight is never cancelled by a reply that
//!     simply raced it;
//! (c) it is older than [`AUTOMATIC_ABORT_MAX_AGE`].
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
//! [`UNDELIVERABLE_AFTER`]: crate::gse::UNDELIVERABLE_AFTER
use crate::state::AppState;
use sedsnet::router::{Router, TryPendingTypeError};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

/// The longest an automatic ABORT is kept trying. A small ABORT frame takes
/// under 100 ms of RFD airtime and the SDK's whole retry budget is 4.5 s
/// (18 attempts 250 ms apart); the station already calls an ABORT
/// undeliverable after 5 s. 30 s is six of those windows: an abort that has
/// not landed by then is waiting on a dead or saturated link, and landing it
/// later would abort a board on a condition the operator can no longer see.
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
                "it was still undelivered after {} s",
                AUTOMATIC_ABORT_MAX_AGE.as_secs()
            ),
        }
    }
}

struct Entry {
    router: usize,
    packet_id: u64,
    cause: String,
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

fn router_key(router: &Router) -> usize {
    router as *const Router as usize
}

fn abort_type() -> sedsnet::config::DataType {
    crate::telemetry_schema::data_type("ABORT")
}

fn recount(entries: &[Entry]) {
    RETIRED.store(entries.iter().filter(|e| e.retired.is_some()).count(), Ordering::Release);
}

/// Record an automatic ABORT the router has just accepted.
pub(crate) fn register_automatic(router: &Router, packet_id: u64, cause: &str) {
    let mut entries = ENTRIES.lock().unwrap();
    let key = router_key(router);
    if entries.iter().any(|e| e.router == key && e.packet_id == packet_id) {
        return;
    }
    while entries.len() >= MAX_ENTRIES {
        // Drop the oldest entry the SDK no longer holds; else the oldest.
        let index = entries.iter().position(|e| e.gone_since.is_some()).unwrap_or(0);
        entries.remove(index);
    }
    entries.push(Entry {
        router: key,
        packet_id,
        cause: cause.to_string(),
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
    let key = router_key(router);
    if ENTRIES.lock().unwrap().iter().any(|e| e.router == key && e.packet_id == packet_id) {
        AbortOrigin::Automatic
    } else {
        AbortOrigin::Manual
    }
}

pub(crate) fn is_retired(router: &Router, packet_id: u64) -> bool {
    if RETIRED.load(Ordering::Acquire) == 0 {
        return false;
    }
    let key = router_key(router);
    ENTRIES
        .lock()
        .unwrap()
        .iter()
        .any(|e| e.router == key && e.packet_id == packet_id && e.retired.is_some())
}

/// The radio writer's check: true for a canonical ABORT frame whose packet id
/// was retired. Such a copy must be dropped, never written.
pub(crate) fn suppress_write(payload: &[u8]) -> bool {
    if RETIRED.load(Ordering::Acquire) == 0 || payload.starts_with(b"SDT") {
        // Reliable end-to-end frames are never SDT-compacted by the SDK.
        return false;
    }
    let Ok(info) = sedsnet::wire_format::peek_frame_info(payload) else {
        return false;
    };
    if info.envelope.ty != abort_type() {
        return false;
    }
    let Ok(id) = sedsnet::wire_format::packet_id_from_wire(payload) else {
        return false;
    };
    ENTRIES
        .lock()
        .unwrap()
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
    let key = router_key(router);
    let retired = {
        let mut entries = ENTRIES.lock().unwrap();
        refresh_locked(&mut entries, key, router, &rows, any_abort_pending, Instant::now());
        let mut retired = Vec::new();
        for entry in entries.iter_mut().filter(|e| e.router == key) {
            if entry.retired.is_none() && entry.gone_since.is_none() {
                entry.retired = Some(RetireReason::OperatorCleared);
                retired.push(Retired::of(entry));
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
    if ENTRIES.lock().unwrap().is_empty() {
        return;
    }
    let any_abort_pending = router.try_has_pending_reliable_tx_type(abort_type()).unwrap_or(true);
    let rows = router.export_reliable_pending_stats();
    let key = router_key(router);
    let delivery_window = crate::sim_freshness::wall(crate::gse::UNDELIVERABLE_AFTER);
    let max_age = crate::sim_freshness::wall(AUTOMATIC_ABORT_MAX_AGE);
    let (retired, query) = {
        let mut entries = ENTRIES.lock().unwrap();
        refresh_locked(&mut entries, key, router, &rows, any_abort_pending, now);
        let mut retired = Vec::new();
        let mut query = 0u8;
        for entry in entries.iter_mut().filter(|e| e.router == key) {
            if entry.retired.is_some() || entry.gone_since.is_some() {
                continue;
            }
            let age = now.saturating_duration_since(entry.raised_at);
            if age >= max_age {
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
                retired.push(Retired::of(entry));
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

/// Update destinations and SDK presence; forget long-gone entries.
fn refresh_locked(
    entries: &mut Vec<Entry>,
    key: usize,
    router: &Router,
    rows: &[sedsnet::diagnostics::ReliablePendingStats],
    any_abort_pending: bool,
    now: Instant,
) {
    for entry in entries.iter_mut().filter(|e| e.router == key) {
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
                        >= crate::sim_freshness::wall(crate::gse::UNDELIVERABLE_AFTER)) =>
            {
                entry.gone_since.get_or_insert(now);
            }
            None => {}
        }
    }
    entries.retain(|e| {
        e.router != key
            || e.gone_since.is_none_or(|gone| {
                let keep = if e.retired.is_some() { RETIRED_SUPPRESSION } else { Duration::ZERO };
                now.saturating_duration_since(gone) < keep
            })
    });
}

struct Retired {
    packet_id: u64,
    cause: String,
    destinations: u8,
    unnamed: bool,
    reason: RetireReason,
    age: Duration,
}
impl Retired {
    fn of(entry: &Entry) -> Self {
        Self {
            packet_id: entry.packet_id,
            cause: entry.cause.clone(),
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
        log::warn!(
            "automatic_abort_retired id={:#x} cause={:?} reason={:?} age_ms={} destinations=[{}]",
            r.packet_id,
            r.cause,
            r.reason,
            r.age.as_millis(),
            destinations
        );
        state.add_notification(format!(
            "Stale automatic Abort retired: it was still owed to {destinations} and will not be sent again, because {}. Send a new Abort if one is still needed.",
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
    let key = router_key(router);
    ENTRIES.lock().unwrap().iter()
        .find(|e| e.router == key && e.packet_id == packet_id)
        .and_then(|e| e.retired)
}
