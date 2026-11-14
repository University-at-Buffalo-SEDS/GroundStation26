//! Re-send a retained operator Abort while an AV-bay board has not read it back.
//!
//! ABORT is an unordered reliable SDK packet with a 4.8 s delivery window
//! (400 ms x 12, crate::reliable_delivery_span). If the radio cannot carry it for that long, the SDK retires
//! it silently. RF's own modem check, for one, holds the radio for up to 16 s
//! with the modem in command mode, and nothing on the air reaches RF then. FC
//! and PB used to latch such an Abort only when a later recovery query reached
//! them. Now, while the operator Abort is retained and a present AV-bay board
//! this mode tracks does not yet read Aborted, the station sends the ABORT
//! again when the last one ran out of retries before that board acknowledged
//! it (one at a time; a fresh packet ID, the original wire and stamp). An
//! ABORT the AV bay acknowledged is never re-sent because its readback is
//! late: every copy re-latches the umbilical boards and costs radio air. It stops when every such board reads Aborted,
//! when the station leaves Aborted (Clear or Reset all), when a recovery is
//! prepared or running, or on a newer operator Abort (which re-arms it).
//! Re-sending an Abort only ever moves a board toward the safe state.
//!
//! The SDK sends ABORT only along fresh discovery routes (30 s TTL), and only
//! discovery frames refresh a route. After a radio outage longer than that,
//! the radio route is gone until the AV bay is rediscovered, even while its
//! telemetry is arriving again. An Abort sent in that window goes to the
//! umbilical side only (or nowhere), never owes an AV-bay board, and so was
//! never re-sent (rrls_takeover, Abort 0.4 s after hand-back). Now every send
//! is checked: unless a pending copy names an AV-bay board within 500 ms, the
//! station asks for topology and sends it again (at most once a second) while
//! an AV-bay route exists, until one does. This holds while the AV bay is
//! unheard too.
use crate::manual_abort_intent::WirePolicy;
use sedsnet::router::Router;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};

/// AV-bay boards (BOARDS mask) whose copy of this station's own Aborted
/// FLIGHT_STATE ran out of retries before they acknowledged it. Exhausting
/// once never ends its delivery: the re-send below sends the station's own
/// Aborted value again until each such board acknowledges it or reads
/// Aborted, the station leaves Aborted (Reset), or a newer own write
/// supersedes it. PB takes an Abort only from FLIGHT_STATE.
static FLIGHT_STATE_UNACKED: AtomicU8 = AtomicU8::new(0);

fn board_mask(router: &Router, owners: &[u64]) -> u8 {
    owners.iter().fold(0u8, |mask, owner| {
        crate::board_recovery::coordinator::pending_destination_board(router, *owner)
            .and_then(|board| crate::board_abort::BOARDS.iter().position(|b| *b == board))
            .map_or(mask, |i| mask | (1 << i))
    })
}

/// Ordered FLIGHT_STATE writes that ran out of retries (packet ID, owners
/// still pending), from the router worker. Records the AV-bay boards that
/// never acknowledged this station's own Aborted value.
pub(crate) fn note_flight_state_exhausted(router: &Router, gone: &[(u64, Vec<u64>)]) {
    let ty = crate::telemetry_schema::data_type("FLIGHT_STATE");
    let Some(own) = router.own_network_variable(ty) else { return };
    if own.payload() != [crate::types::NetworkFlightState::Aborted as u8] {
        return;
    }
    let boards = gone
        .iter()
        .filter(|(id, _)| *id == own.packet_id())
        .fold(0u8, |mask, (_, owners)| mask | board_mask(router, owners))
        & AV_BAY_MASK;
    if boards != 0 {
        FLIGHT_STATE_UNACKED.fetch_or(boards, Ordering::AcqRel);
        log::warn!(
            "Aborted FLIGHT_STATE ran out of retries before {} acknowledged it; it will be sent again",
            board_names(boards)
        );
    }
}

fn board_names(mask: u8) -> String {
    crate::board_abort::BOARDS
        .iter()
        .enumerate()
        .filter(|(i, _)| mask & (1 << i) != 0)
        .map(|(_, board)| board.sender_id())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
pub(crate) fn flight_state_unacked_for_test() -> u8 {
    FLIGHT_STATE_UNACKED.load(Ordering::Acquire)
}

/// RF, FC and PB in `board_abort::BOARDS` order.
const AV_BAY_MASK: u8 = 0b111;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Armed {
    epoch: u64,
    wire: WirePolicy,
    /// Recovery activity seen when armed; any other activity stops re-sending.
    recovery: Option<String>,
    last_sent_ms: Option<u64>,
    resends: u32,
    /// Resumed after a bounced recovery: take the next observed recovery
    /// identity as the baseline instead of treating it as a new recovery.
    adopt_recovery: bool,
    /// The ABORT in flight as last seen: (AV-bay boards it still owed, its
    /// retries). Only an ABORT that ran out of retries with an owed AV-bay
    /// board still pending is sent again.
    in_flight: Option<(u8, u32)>,
    /// No ABORT is known to have been routed toward the AV bay: no pending
    /// ABORT has named an AV-bay board since the last send.
    unrouted: bool,
    /// The last send is being checked: since when (None until the first pass
    /// after it). It counts as routed once a pending ABORT names an AV-bay
    /// board or an AV-bay side transmitted an ABORT since `mark`.
    verify: Option<Option<u64>>,
    mark: SendMark,
    /// An unrouted Abort was re-sent: once it reaches the AV bay, send the
    /// Aborted FLIGHT_STATE again too (PB takes an Abort only from it).
    flight_state_owed: bool,
    last_topology_request_ms: Option<u64>,
    unrouted_reported: bool,
    /// AV-bay boards (BOARDS mask) without a route of their own when the
    /// Abort was sent (an umbilical route to FC says nothing about RF or PB).
    /// Each is sent the ABORT and the Aborted FLIGHT_STATE again once it is
    /// routed. Owners cannot show it: the SDK tracks at most four ABORT owners
    /// (the umbilical boards fill them) and PB is behind RF's segment.
    unaddressed: u8,
}

#[derive(Default)]
pub(crate) struct AbortResend {
    armed: Mutex<Option<Armed>>,
    /// Held while a recovery is being prepared; restored if it resets nothing.
    suspended: Mutex<Option<Armed>>,
}

/// What the caller observed this pass.
pub(crate) struct Observation {
    pub now_ms: u64,
    pub epoch: u64,
    pub station_aborted: bool,
    pub recovery: Option<String>,
    /// AV-bay boards (BOARDS mask) present, in scope and not reading Aborted.
    pub owed: u8,
    /// In-scope boards (BOARDS mask) not heard recently and not reading Aborted.
    pub unheard: u8,
}

/// How often a held Abort asks for topology while the AV bay has no route.
const TOPOLOGY_REQUEST_MS: u64 = 2_000;
/// A send whose pending ABORT names no AV-bay board within this long went
/// only to the umbilical (or nowhere). Radio delivery takes longer than one
/// 100 ms pass, so a routed copy is seen pending at least once.
const ROUTE_VERIFY_MS: u64 = 500;
/// Least interval between sends of an Abort the AV bay has not been routed.
const UNROUTED_RESEND_MS: u64 = 1_000;

/// Sides with a fresh discovery route whose direct announcer is an AV-bay
/// board. Only the announcer counts: a side's topology also carries boards
/// reflected back from elsewhere (the umbilical lists the AV bay).
fn av_bay_sides(router: &Router) -> Vec<String> {
    let ttl = sedsnet::discovery::DISCOVERY_ROUTE_TTL_MS;
    let av_bay = |sender: &str| {
        crate::board_abort::BOARDS[..3].iter().any(|board| board.sender_id() == sender)
    };
    router
        .export_topology()
        .routes
        .into_iter()
        .filter(|route| {
            route.age_ms <= ttl
                && route.announcers.iter().any(|peer| peer.age_ms <= ttl && av_bay(&peer.sender_id))
        })
        .map(|route| route.side_name)
        .collect()
}

/// Whether the router has a fresh discovery route to the AV bay. Without one
/// the SDK cannot put an ABORT on the radio. A hint only: whether a send went
/// toward the AV bay is read back from the SDK (see `SendMark`).
pub(crate) fn av_bay_abort_route(router: &Router) -> bool {
    !av_bay_sides(router).is_empty()
}

/// ABORT transmissions per side before a send, and whether the AV bay had a
/// route then.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SendMark {
    av_bay_routed: bool,
    abort_tx: Vec<(String, u64)>,
    /// AV-bay boards (BOARDS mask) with a route of their own then.
    routed_boards: u8,
}

fn abort_tx_by_side(router: &Router) -> Vec<(String, u64)> {
    let abort = crate::telemetry_schema::data_type("ABORT");
    router
        .export_runtime_stats()
        .sides
        .into_iter()
        .map(|side| {
            let sent = side
                .data_types
                .iter()
                .filter(|ty| ty.data_type == abort)
                .map(|ty| ty.tx_packets.saturating_add(ty.tx_retries))
                .sum();
            (side.side_name, sent)
        })
        .collect()
}

impl SendMark {
    pub(crate) fn av_bay_routed(&self) -> bool {
        self.av_bay_routed
    }
}

/// Taken just before an operator Abort is sent.
pub(crate) fn send_mark(router: &Router) -> SendMark {
    SendMark {
        av_bay_routed: av_bay_abort_route(router),
        abort_tx: abort_tx_by_side(router),
        routed_boards: routed_av_bay_boards(router),
    }
}

#[cfg(test)]
pub(crate) fn send_mark_routed_for_test() -> SendMark {
    SendMark { av_bay_routed: true, routed_boards: AV_BAY_MASK, ..SendMark::default() }
}

/// AV-bay boards (BOARDS mask) the router has a fresh discovery route to,
/// judged per board: the board announces itself on a side, or a fresh
/// AV-bay announcer lists it behind it. FC reachable over the umbilical does
/// not make RF or PB reachable (fullnet C-fix3: an Abort at hand-back went
/// to FC only, and RF and PB never got its Aborted FLIGHT_STATE).
pub(crate) fn routed_av_bay_boards(router: &Router) -> u8 {
    let ttl = sedsnet::discovery::DISCOVERY_ROUTE_TTL_MS;
    let index = |sender: &str| crate::board_abort::BOARDS[..3].iter().position(|board| board.sender_id() == sender);
    router.export_topology().routes.iter().filter(|route| route.age_ms <= ttl).fold(0u8, |mask, route| {
        route.announcers.iter().filter(|peer| peer.age_ms <= ttl).fold(mask, |mask, peer| {
            let Some(own) = index(&peer.sender_id) else { return mask };
            peer.routers.iter().filter_map(|node| index(&node.sender_id)).fold(mask | (1 << own), |mask, i| mask | (1 << i))
        })
    })
}

/// Whether a side that reaches the AV bay transmitted an ABORT since `mark`.
fn sent_toward_av_bay(router: &Router, mark: &SendMark) -> bool {
    let sides = av_bay_sides(router);
    abort_tx_by_side(router).into_iter().any(|(side, sent)| {
        sides.contains(&side)
            && sent > mark.abort_tx.iter().find(|(name, _)| *name == side).map_or(0, |(_, n)| *n)
    })
}

impl AbortResend {
    /// The SDK accepted the operator Abort `epoch` with `wire`; `mark` was
    /// taken just before it was sent.
    pub(crate) fn arm(&self, epoch: u64, wire: WirePolicy, recovery: Option<String>, mark: SendMark) {
        *self.armed.lock().unwrap() = Some(Armed {
            epoch, wire, recovery, last_sent_ms: None, resends: 0, adopt_recovery: false, in_flight: None,
            unrouted: !mark.av_bay_routed, verify: Some(None), flight_state_owed: !mark.av_bay_routed,
            unaddressed: AV_BAY_MASK & !mark.routed_boards, mark,
            last_topology_request_ms: None,
            unrouted_reported: false,
        });
    }
    /// A recovery is being prepared: stop re-sending at once, before the
    /// coordinator drains in-flight ABORTs and takes its fresh queries, so no
    /// later copy can move a board's abort epoch under the prepared intent.
    pub(crate) fn disarm(&self, why: &str) {
        let taken = self.armed.lock().unwrap().take();
        if let Some(armed) = taken {
            if armed.resends > 0 {
                log::info!("operator Abort re-send paused after {} re-send(s): {why}", armed.resends);
            }
            *self.suspended.lock().unwrap() = Some(armed);
        }
    }
    /// The recovery that paused the re-send reset no board: resume it. A
    /// newer operator Abort armed meanwhile is kept instead. The next pass
    /// stops it again if the station has left Aborted or the boards read back.
    pub(crate) fn resume(&self) {
        let Some(mut armed) = self.suspended.lock().unwrap().take() else { return };
        let mut current = self.armed.lock().unwrap();
        if current.is_none() {
            // The recovery identity seen then is gone; adopt the current one.
            armed.recovery = None;
            armed.adopt_recovery = true;
            log::info!("operator Abort re-send resumed: the recovery reset no board");
            *current = Some(armed);
        }
    }
    /// Re-sends so far for the current operator Abort, while still active.
    pub(crate) fn active_resends(&self) -> Option<u32> {
        self.armed.lock().unwrap().as_ref().map(|armed| armed.resends)
    }
    /// One pass. Sends at most one ABORT, only when none is in flight and at
    /// least one retry interval after the last. Returns the boards still owed
    /// when it sent.
    pub(crate) fn step(&self, router: &Router, seen: Observation) -> Option<u8> {
        let mut guard = self.armed.lock().unwrap();
        let armed = guard.as_mut()?;
        let owed = seen.owed & AV_BAY_MASK;
        if armed.adopt_recovery {
            armed.recovery = seen.recovery.clone();
            armed.adopt_recovery = false;
        }
        let unheard = seen.unheard & AV_BAY_MASK;
        if seen.epoch != armed.epoch || !seen.station_aborted || seen.recovery != armed.recovery || owed | unheard == 0 {
            if armed.resends > 0 {
                log::info!(
                    "operator Abort re-send stopped after {} re-send(s): {}",
                    armed.resends,
                    if owed == 0 { "every AV-bay board reads Aborted" } else { "recovery started or the station left Aborted" }
                );
            }
            *guard = None;
            FLIGHT_STATE_UNACKED.store(0, Ordering::Release);
            return None;
        }
        let sent = Self::step_abort(armed, router, &seen, owed, unheard);
        // After any ABORT copy of this pass, so the FLIGHT_STATE queues behind
        // it: the Aborted FLIGHT_STATE an owed board never acknowledged (it
        // ran out of retries; also the case when the exhausted ABORT was just
        // re-sent), or the one held while the Abort had no AV-bay route. A
        // board that acknowledged it is not owed it again.
        let unacked = FLIGHT_STATE_UNACKED.load(Ordering::Acquire) & AV_BAY_MASK;
        let lost = unacked & (owed | unheard);
        if (armed.flight_state_owed || lost != 0) && !armed.unrouted {
            match crate::network_variables::resend_aborted_flight_state(router) {
                Ok(true) => {
                    armed.flight_state_owed = false;
                    FLIGHT_STATE_UNACKED.fetch_and(!unacked, Ordering::AcqRel);
                    if lost != 0 {
                        log::warn!("Aborted FLIGHT_STATE re-sent: {} never acknowledged it", board_names(lost));
                    } else {
                        log::warn!("Aborted FLIGHT_STATE re-sent: the AV bay is routed again after the operator Abort was held");
                    }
                }
                Ok(false) => {}
                Err(error) => log::warn!("Aborted FLIGHT_STATE re-send refused: {error}"),
            }
        }
        sent
    }

    /// The ABORT part of `step`.
    fn step_abort(armed: &mut Armed, router: &Router, seen: &Observation, owed: u8, unheard: u8) -> Option<u8> {
        let abort = crate::telemetry_schema::data_type("ABORT");
        let rows = router.export_reliable_pending_stats();
        let abort_rows: Vec<(u8, u32)> = rows
            .iter()
            .filter(|row| row.data_type == abort)
            .map(|row| (board_mask(router, &row.pending_destinations) & AV_BAY_MASK, row.retries))
            .collect();
        let pending = abort_rows.iter().copied().max_by_key(|(_, retries)| *retries);
        if let Some(sent) = Self::send_to_unaddressed(armed, router, seen, owed | unheard, &abort_rows) {
            return Some(sent);
        }
        // Did the last send go toward the AV bay? The SDK sends ABORT only
        // along fresh discovery routes and, among several candidate sides,
        // may pick just one; a pending ABORT that names an AV-bay board, or an
        // ABORT transmitted on a side that reaches the AV bay, is the proof.
        if abort_rows.iter().any(|(boards, _)| *boards != 0)
            || (armed.verify.is_some() && sent_toward_av_bay(router, &armed.mark))
        {
            armed.unrouted = false;
            armed.unrouted_reported = false;
            armed.verify = None;
        } else if let Some(since) = armed.verify {
            match since {
                None => armed.verify = Some(Some(seen.now_ms)),
                Some(since) if seen.now_ms.saturating_sub(since) >= ROUTE_VERIFY_MS => {
                    armed.unrouted = true;
                    armed.verify = None;
                }
                Some(_) => {}
            }
        }
        if armed.unrouted && pending.is_none_or(|(boards, _)| boards == 0) {
            // Covers an unheard AV bay too: its route comes back first.
            if let Some(sent) = Self::send_when_routed(armed, router, seen, owed | unheard) {
                return sent;
            }
        }
        if owed == 0 {
            return None;
        }
        if let Some(row) = pending {
            armed.in_flight = Some(row);
            return None;
        }
        // No ABORT in flight. Send again only if the last one ran out of
        // retries while an AV-bay board that still owes the Abort had not
        // acknowledged it. One the AV bay acknowledged is never re-sent:
        // a late readback is not a lost Abort.
        let max_retries = sedsnet::config::runtime_tuning_config().reliable_max_retries;
        let exhausted = armed
            .in_flight
            .take()
            .is_some_and(|(boards, retries)| boards & owed != 0 && retries.saturating_add(3) >= max_retries);
        if !exhausted {
            return None;
        }
        let retry_ms = u64::from(sedsnet::config::runtime_tuning_config().reliable_retransmit_ms.max(1));
        if armed.last_sent_ms.is_some_and(|last| seen.now_ms.saturating_sub(last) < retry_ms) {
            return None;
        }
        match crate::telemetry_task::queue_manual_abort_wire_receipted(router, armed.wire) {
            Ok(_) => {
                armed.resends = armed.resends.saturating_add(1);
                armed.last_sent_ms = Some(seen.now_ms);
                log::warn!(
                    "operator Abort re-sent ({}): the last ABORT ran out of retries before {} acknowledged it",
                    armed.resends,
                    board_names(owed)
                );
                Some(owed)
            }
            Err(error) => {
                log::warn!("operator Abort re-send refused: {error}");
                None
            }
        }
    }
    /// An owed AV-bay board the Abort was never addressed to (no route of its
    /// own when it was sent): once it is routed, send the ABORT again and the
    /// Aborted FLIGHT_STATE after it. It ends like every other re-send: the
    /// board reads Aborted, the station leaves Aborted, or a newer Abort.
    fn send_to_unaddressed(armed: &mut Armed, router: &Router, seen: &Observation, owed: u8, abort_rows: &[(u8, u32)]) -> Option<u8> {
        // A board a pending ABORT names was addressed after all.
        armed.unaddressed &= !abort_rows.iter().fold(0u8, |mask, (boards, _)| mask | boards);
        let ready = armed.unaddressed & owed & routed_av_bay_boards(router);
        if ready == 0
            || armed.last_sent_ms.is_some_and(|last| seen.now_ms.saturating_sub(last) < UNROUTED_RESEND_MS)
        {
            return None;
        }
        match crate::telemetry_task::queue_manual_abort_wire_receipted(router, armed.wire) {
            Ok(_) => {
                armed.unaddressed &= !ready;
                armed.flight_state_owed = true;
                armed.in_flight = None;
                armed.resends = armed.resends.saturating_add(1);
                armed.last_sent_ms = Some(seen.now_ms);
                log::warn!(
                    "operator Abort re-sent ({}): {} had no route when it was sent and is routed now",
                    armed.resends,
                    board_names(ready)
                );
                Some(ready)
            }
            Err(error) => {
                log::warn!("operator Abort re-send refused: {error}");
                None
            }
        }
    }

    /// No ABORT is on its way to an owed AV-bay board, and none is known to
    /// have been routed there. Send one as soon as the router has an AV-bay
    /// route; until then ask for topology so the route comes back sooner, and
    /// return None so the ordinary exhausted-retry rule still applies.
    fn send_when_routed(armed: &mut Armed, router: &Router, seen: &Observation, owed: u8) -> Option<Option<u8>> {
        if !av_bay_abort_route(router) {
            if armed.last_topology_request_ms.is_none_or(|last| seen.now_ms.saturating_sub(last) >= TOPOLOGY_REQUEST_MS) {
                armed.last_topology_request_ms = Some(seen.now_ms);
                if let Err(error) = router.request_topology() {
                    log::warn!("operator Abort topology request refused: {error}");
                }
            }
            if !armed.unrouted_reported {
                armed.unrouted_reported = true;
                log::error!(
                    "operator Abort has no route to the AV bay (no fresh discovery route); \
                     holding it and sending it again once the AV bay is rediscovered"
                );
            }
            return None;
        }
        if armed.verify.is_some()
            || armed.last_sent_ms.is_some_and(|last| seen.now_ms.saturating_sub(last) < UNROUTED_RESEND_MS)
        {
            return Some(None);
        }
        let mark = send_mark(router);
        match crate::telemetry_task::queue_manual_abort_wire_receipted(router, armed.wire) {
            Ok(_) => {
                // Still unrouted until the SDK shows it went toward the AV bay.
                armed.mark = mark;
                armed.verify = Some(None);
                armed.flight_state_owed = true;
                armed.in_flight = None;
                armed.resends = armed.resends.saturating_add(1);
                armed.last_sent_ms = Some(seen.now_ms);
                log::warn!("operator Abort re-sent ({}): the AV bay has a route and has not been sent this Abort", armed.resends);
                Some(Some(owed))
            }
            Err(error) => {
                log::warn!("operator Abort re-send refused: {error}");
                Some(None)
            }
        }
    }
}

/// Station-side observation for `step`.
pub(crate) fn observe(state: &crate::state::AppState, now_ms: u64) -> Option<Observation> {
    let recovery = crate::board_recovery::coordinator::recovery_activity(state)?;
    let aborted = state.board_abort.masks().0;
    let scope = crate::board_recovery::scope::mode_scope(crate::station_mode::local_mode());
    let present = crate::board_abort::present_mask(state);
    Some(Observation {
        now_ms,
        epoch: state.manual_abort_intent.epoch(),
        station_aborted: state.local_flight_state_snapshot() == crate::types::FlightState::Aborted,
        recovery,
        owed: scope & present & !aborted,
        unheard: scope & !present & !aborted,
    })
}

/// The primary re-sends its operator Abort; an RRLS only while its takeover
/// owns the AV bay (a listening RRLS's Abort is the primary's to deliver).
pub(crate) fn service_runs() -> bool {
    !crate::rrls::is_rrls() || crate::rrls::takeover_active()
}

pub(crate) async fn service(
    state: std::sync::Arc<crate::state::AppState>,
    router: std::sync::Arc<Router>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    let start = std::time::Instant::now();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.recv() => return,
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
        }
        if !service_runs() || state.abort_resend.active_resends().is_none() {
            continue;
        }
        let now_ms = crate::simulation_clock_ms().unwrap_or_else(|| start.elapsed().as_millis() as u64);
        if let Some(seen) = observe(&state, now_ms) {
            state.abort_resend.step(&router, seen);
        }
    }
}

#[cfg(test)]
#[path = "manual_abort_resend_tests.rs"]
mod tests;
