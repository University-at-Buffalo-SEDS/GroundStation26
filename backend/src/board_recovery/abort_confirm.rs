//! Prompt readback after an operator Abort.
//!
//! The umbilical boards latch an Abort within a few tens of milliseconds, but
//! the station only shows a board Aborted once that board's own readback says
//! so. The background refresh asks each umbilical board once every two
//! seconds in turn, so the station used to show the latch up to two seconds
//! late. For a short window after an Abort the station now asks every present
//! umbilical board that does not yet read Aborted at once, and again shortly
//! after any reply that still did not show the Abort.
//!
//! BOARD_RECOVERY_REQUEST is one ordered SDK stream (one packet in flight per
//! type, whatever the destination), so an AV-bay query waiting on the radio
//! holds every umbilical query behind it. The background refresh therefore
//! sends no new AV-bay query during the window (see `ReadonlyQueryRefresh`).
use super::*;

/// How long after an Abort the station keeps asking for prompt readback.
pub(crate) const CONFIRM_WINDOW: Duration = Duration::from_secs(3);
/// A board that answered without showing the Abort is asked again after this.
const CONFIRM_REQUERY: Duration = Duration::from_millis(250);
/// The window stays open this long at most while an AV-bay query (one ordered
/// BOARD_RECOVERY_REQUEST stream) still holds the umbilical queries behind it.
const CONFIRM_WINDOW_MAX: Duration = Duration::from_secs(10);
/// A prompt query unanswered this long was most likely dropped with the
/// ordered stream's tail when an AV-bay query ahead of it gave up: ask again.
const CONFIRM_LOST: Duration = Duration::from_secs(3);
/// Umbilical (fill) board indices into BOARDS.
const UMBILICAL: [usize; 4] = [3, 4, 5, 6];
/// AV-bay boards in the order their Abort readback is asked: FC, PB, RF.
const AV_BAY_ORDER: [usize; 3] = [1, 2, 0];
/// How long after an Abort the AV-bay boards are asked for readback. Over an
/// RRLS relay one query can take its whole 4.5 s retry span.
const AV_BAY_CONFIRM_WINDOW: Duration = Duration::from_secs(20);
/// An AV-bay board that answered without showing the Abort is asked again
/// after this: each radio query holds the one ordered stream.
const AV_BAY_REQUERY: Duration = Duration::from_secs(2);

/// Bumped each time a BOARD_RECOVERY_REQUEST head gave up its retries. The
/// SDK drops the queued tail with it, so every prompt query asked before the
/// bump and still unanswered never left the station.
static QUERY_STREAM_RETIRED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static QUERY_STREAM_RETIRED_NOTIFY: tokio::sync::Notify = tokio::sync::Notify::const_new();
/// The router worker saw an ordered BOARD_RECOVERY_REQUEST exhaust its
/// retries (or could not tell which type did): ask again at once.
pub(crate) fn note_query_stream_retired() {
    QUERY_STREAM_RETIRED.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    QUERY_STREAM_RETIRED_NOTIFY.notify_one();
}
/// Resolves when a BOARD_RECOVERY_REQUEST head has given up (see above).
pub(crate) async fn query_stream_retired() {
    QUERY_STREAM_RETIRED_NOTIFY.notified().await
}
fn query_stream_generation() -> u64 {
    QUERY_STREAM_RETIRED.load(std::sync::atomic::Ordering::Acquire)
}

#[derive(Default)]
pub(crate) struct AbortConfirmRefresh {
    since: Option<Stamp>,
    /// Umbilical boards (BOARDS mask) whose Abort readback is still owed.
    owed: u8,
    /// Last poll that saw an AV-bay BOARD_RECOVERY_REQUEST in flight. The
    /// umbilical queries only leave once it clears, so the window runs
    /// CONFIRM_WINDOW past that, not past the Abort.
    av_bay_head: Option<Stamp>,
    /// Retired-stream generation when each board's prompt query was asked.
    asked_generation: [u64; 7],
    /// AV-bay phase: when it started, and the AV-bay boards still owed.
    av_bay_since: Option<Stamp>,
    av_bay_owed: u8,
}
impl AbortConfirmRefresh {
    /// Start (or restart) the window for an Abort the station just sent.
    pub(crate) fn start(&mut self) {
        self.since = Some(Stamp::now());
        self.owed = UMBILICAL.iter().fold(0, |mask, at| mask | (1 << at));
        self.av_bay_head = None;
        self.asked_generation = [query_stream_generation(); 7];
        self.av_bay_since = self.since;
        self.av_bay_owed = AV_BAY_ORDER.iter().fold(0, |mask, at| mask | (1 << at));
    }
    /// True while the window is open and an umbilical board still owes its
    /// Abort readback.
    pub(crate) fn active(&mut self) -> bool {
        let now = Stamp::now();
        let expired = |from: Stamp, window: Duration| now.checked_duration_since(from).is_none_or(|age| age >= window);
        if self.owed == 0
            || self.since.is_some_and(|since| {
                expired(since, CONFIRM_WINDOW_MAX)
                    || (expired(since, CONFIRM_WINDOW) && self.av_bay_head.is_none_or(|head| expired(head, CONFIRM_WINDOW)))
            })
        {
            self.since = None;
            self.owed = 0;
        }
        self.since.is_some()
    }
    /// Ask the umbilical boards that still owe an Abort readback. Returns the
    /// mask (BOARDS order) of boards a query went to.
    pub(crate) fn poll(&mut self, state: &Arc<AppState>) -> u8 {
        if self.since.is_some() && state.topology_router.get().is_some_and(|router| av_bay_query_in_flight(router)) {
            self.av_bay_head = Some(Stamp::now());
        }
        if !self.active() || crate::rrls::is_rrls() {
            return 0;
        }
        let Some(since) = self.since else { return 0 };
        // Present boards this mode tracks that do not yet read Aborted.
        let aborted = state.board_abort.masks().0;
        let candidates = scope::mode_scope(crate::station_mode::local_mode())
            & crate::board_abort::present_mask(state)
            & !aborted;
        self.owed &= candidates;
        let now = Stamp::now();
        let generation = query_stream_generation();
        let due = {
            let inner = state.board_recovery.inner.lock().unwrap();
            UMBILICAL.into_iter()
                .filter(|&at| {
                    self.owed & (1 << at) != 0
                        && confirm_query_due(inner.queries[at].as_ref(), since, now, self.asked_generation[at] != generation)
                })
                .collect::<Vec<_>>()
        };
        due.into_iter().fold(0, |sent, at| {
            if requery_board_query(state, BOARDS[at]).status() == StatusCode::OK {
                self.asked_generation[at] = generation;
                sent | (1 << at)
            } else {
                sent
            }
        })
    }
    /// True while an AV-bay board still owes its Abort readback.
    pub(crate) fn av_bay_active(&mut self) -> bool {
        let now = Stamp::now();
        if self.av_bay_owed == 0
            || self.av_bay_since.is_some_and(|since| {
                now.checked_duration_since(since).is_none_or(|age| age >= AV_BAY_CONFIRM_WINDOW)
            })
        {
            self.av_bay_since = None;
            self.av_bay_owed = 0;
        }
        self.av_bay_since.is_some()
    }
    /// Ask the next AV-bay board that owes its Abort readback. Returns the
    /// mask (BOARDS order) of the board a query went to.
    ///
    /// This is Abort confirmation, not background refresh: the degraded-link
    /// pause and the read-only backoff do not apply. It waits for the
    /// umbilical readback (an AV-bay query ahead of those would hold them in
    /// the one ordered stream) and keeps one AV-bay query in flight at a time.
    pub(crate) fn poll_av_bay(&mut self, state: &Arc<AppState>) -> u8 {
        if !self.av_bay_active() || self.active() || crate::rrls::is_rrls() {
            return 0;
        }
        let Some(since) = self.av_bay_since else { return 0 };
        let Some(router) = state.topology_router.get() else { return 0 };
        let aborted = state.board_abort.masks().0;
        self.av_bay_owed &= scope::mode_scope(crate::station_mode::local_mode())
            & crate::board_abort::present_mask(state)
            & !aborted;
        if self.av_bay_owed == 0 || av_bay_query_in_flight(router) {
            return 0;
        }
        let now = Stamp::now();
        let generation = query_stream_generation();
        // The owed, due board asked longest ago goes next (never asked since
        // the Abort first, then in AV_BAY_ORDER), so a silent board that
        // keeps coming due cannot starve the others.
        let next = {
            let inner = state.board_recovery.inner.lock().unwrap();
            AV_BAY_ORDER
                .into_iter()
                .enumerate()
                .filter(|&(_, at)| {
                    self.av_bay_owed & (1 << at) != 0
                        && query_due(
                            inner.queries[at].as_ref(),
                            since,
                            now,
                            self.asked_generation[at] != generation,
                            AV_BAY_REQUERY,
                        )
                })
                .min_by_key(|&(rank, at)| {
                    // Time from the Abort to this board's latest ask; None
                    // (not asked since the Abort) sorts first.
                    let asked = inner.queries[at]
                        .as_ref()
                        .and_then(|query| query.sent.checked_duration_since(since));
                    (asked, rank)
                })
                .map(|(_, at)| at)
        };
        match next {
            Some(at) if requery_board_query(state, BOARDS[at]).status() == StatusCode::OK => {
                self.asked_generation[at] = generation;
                1 << at
            }
            _ => 0,
        }
    }
}
/// Whether an AV-bay BOARD_RECOVERY_REQUEST is in flight: the one ordered
/// stream then holds every umbilical query behind it.
pub(crate) fn av_bay_query_in_flight(router: &sedsnet::router::Router) -> bool {
    let ty = crate::telemetry_schema::data_type("BOARD_RECOVERY_REQUEST");
    router.export_reliable_pending_stats().iter().any(|row| {
        row.data_type == ty
            && row.pending_destinations.iter().any(|owner| {
                coordinator::pending_destination_board(router, *owner)
                    .is_some_and(|board| matches!(board, Board::RFBoard | Board::FlightComputer | Board::PowerBoard))
            })
    })
}
/// Whether a board needs a prompt query now, given its latest query.
/// `retired_since_asked`: a BOARD_RECOVERY_REQUEST head gave up after this
/// board's prompt query was asked, and dropped the queued tail with it.
fn confirm_query_due(query: Option<&Query>, since: Stamp, now: Stamp, retired_since_asked: bool) -> bool {
    query_due(query, since, now, retired_since_asked, CONFIRM_REQUERY)
}
/// `confirm_query_due` with the re-ask delay after a reply that did not show
/// the Abort as a parameter.
fn query_due(query: Option<&Query>, since: Stamp, now: Stamp, retired_since_asked: bool, requery: Duration) -> bool {
    match query {
        // Asked since the Abort: wait for the reply, then ask again shortly
        // after a reply that did not show the Abort, or once the query is
        // lost (dropped behind an AV-bay query that gave up).
        Some(query) if query.sent.checked_duration_since(since).is_some() => {
            let age = now.checked_duration_since(query.sent);
            if query.received {
                age.is_some_and(|age| age >= requery)
            } else {
                retired_since_asked || age.is_some_and(|age| age >= CONFIRM_LOST)
            }
        }
        // Not asked since the Abort; an older query cannot show it.
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sedsnet::packet::Packet;
    use sedsnet::router::{Router, RouterConfig, RouterSideOptions};

    #[test]
    fn a_board_is_asked_once_per_reply_until_it_reads_aborted() {
        let since = Stamp::now();
        let at = |ms: u64| since + Duration::from_millis(ms);
        let query = |sent: Stamp, received: bool| Query { abort_revision: 0, nonce: 1, sent, received };
        // Nothing asked since the Abort: ask now, even if an older background
        // query is still unanswered.
        assert!(confirm_query_due(None, since, at(0), false));
        assert!(confirm_query_due(Some(&query(since - Duration::from_millis(500), false)), since, at(0), false));
        assert!(confirm_query_due(Some(&query(since - Duration::from_millis(500), true)), since, at(0), false));
        // One prompt query in flight at a time.
        assert!(!confirm_query_due(Some(&query(at(10), false)), since, at(2_000), false));
        // Unanswered past CONFIRM_LOST: dropped with the ordered stream's tail
        // behind an AV-bay query that gave up. Ask again.
        assert!(confirm_query_due(Some(&query(at(10), false)), since, at(10) + CONFIRM_LOST, false));
        // It answered without the Abort (an Aborted reply sets the board's
        // Abort row and stops the asking): ask again shortly after.
        assert!(!confirm_query_due(Some(&query(at(10), true)), since, at(200), false));
        assert!(confirm_query_due(Some(&query(at(10), true)), since, at(10) + CONFIRM_REQUERY, false));
    }

    /// Each radio query holds the one ordered stream: an AV-bay board that
    /// answered without the Abort is asked again after 2 s, not 250 ms. Lost
    /// and retired queries keep their re-ask.
    #[test]
    fn an_av_bay_board_answering_without_the_abort_is_asked_again_after_two_seconds() {
        let since = Stamp::now();
        let at = |ms: u64| since + Duration::from_millis(ms);
        let query = |sent: Stamp, received: bool| Query { abort_revision: 0, nonce: 1, sent, received };
        assert!(!query_due(Some(&query(at(10), true)), since, at(10) + CONFIRM_REQUERY, false, AV_BAY_REQUERY));
        assert!(query_due(Some(&query(at(10), true)), since, at(10) + AV_BAY_REQUERY, false, AV_BAY_REQUERY));
        assert!(query_due(Some(&query(at(10), false)), since, at(20), true, AV_BAY_REQUERY));
        assert!(query_due(Some(&query(at(10), false)), since, at(10) + CONFIRM_LOST, false, AV_BAY_REQUERY));
    }

    /// accept-hitl and hitl-abort runs: AB/VB/GB/DAQ latched within one probe
    /// sample of the Abort, yet the station showed them 0.2-2 s later because
    /// it waited for each board's turn in the two-second background refresh.
    #[tokio::test]
    async fn every_present_umbilical_board_is_asked_at_once_after_an_abort() {
        let fixture = crate::board_recovery::tests::http_fixture().await;
        let state = fixture.state.clone();
        let host = Arc::new(Router::new(RouterConfig::new([]).with_sender("GS")));
        assert!(state.topology_router.set(host.clone()).is_ok());
        let side = host.add_side_packed_with_options("umbilical", |_| Ok(()),
            RouterSideOptions { reliable_enabled: true, ..Default::default() });
        for board in BOARDS {
            let announce = sedsnet::discovery::build_discovery_announce(
                board.sender_id(), 1, &[crate::telemetry_schema::endpoint(endpoint(board))]).unwrap();
            host.rx_from_side(&announce, side).unwrap();
            state.board_status.lock().unwrap().get_mut(&board).unwrap().last_seen_instant = Some(std::time::Instant::now());
        }
        let topology = host.export_topology();
        assert!(BOARDS.iter().all(|board| endpoint_verified(&state, *board, &topology)));
        // VB has a background query still waiting (it answers the same
        // demand, but it was sent before the Abort and cannot show it); AB
        // already reads Aborted.
        let old = Stamp::now() - Duration::from_millis(500);
        state.board_recovery.inner.lock().unwrap().queries[4] = Some(Query { abort_revision: 0, nonce: 7, sent: old, received: false });
        state.board_abort.observe_abort(Board::ActuatorBoard, std::time::Instant::now(), 1);
        let mut confirm = AbortConfirmRefresh::default();
        assert_eq!(confirm.poll(&state), 0, "no Abort, no prompt queries");
        confirm.start();
        assert!(confirm.active());
        assert_eq!(confirm.poll(&state), 0b111_0000, "VB, GB and DAQ at once; AB already reads Aborted; no AV-bay board");
        {
            let inner = state.board_recovery.inner.lock().unwrap();
            for at in [4, 5, 6] {
                let query = inner.queries[at].as_ref().unwrap();
                assert!(!query.received && query.sent.checked_duration_since(old).is_some_and(|age| age >= Duration::from_millis(500)));
            }
            assert_eq!(inner.previous[4].as_ref().map(|query| query.nonce), Some(7), "the superseded query's reply still counts");
            assert!(inner.queries[0..4].iter().all(Option::is_none));
        }
        assert_eq!(confirm.poll(&state), 0, "one prompt query per board in flight");
        assert!(confirm.active(), "readback still owed: AV-bay background queries stay held");
        for board in [Board::ValveBoard, Board::GatewayBoard, Board::DaqBoard] {
            state.board_abort.observe_abort(board, std::time::Instant::now(), 2);
        }
        assert_eq!(confirm.poll(&state), 0);
        assert!(!confirm.active(), "every umbilical board reads Aborted: the window closes early");
    }

    /// pyroF (relay): RF's Abort readback took 19.7 s. RF reports its state
    /// only when asked, and its background query was on a 30 s backoff (the
    /// degraded-link pause on dev holds it for the whole relay). After an
    /// Abort each AV-bay board is asked once the umbilical readback is in,
    /// one AV-bay query in the ordered stream at a time.
    #[tokio::test]
    async fn av_bay_boards_are_asked_one_at_a_time_after_the_umbilical_readback() {
        let fixture = crate::board_recovery::tests::http_fixture().await;
        let state = fixture.state.clone();
        let host = Arc::new(Router::new(RouterConfig::new([]).with_sender("GS")));
        assert!(state.topology_router.set(host.clone()).is_ok());
        let opts = RouterSideOptions { reliable_enabled: true, ..Default::default() };
        let radio = host.add_side_packed_with_options("rocket_comms", |_| Ok(()), opts.clone());
        let umbilical = host.add_side_packed_with_options("umbilical_comms", |_| Ok(()), opts);
        for board in BOARDS {
            let side = if UMBILICAL.contains(&index(board).unwrap()) { umbilical } else { radio };
            let announce = sedsnet::discovery::build_discovery_announce(
                board.sender_id(), 1, &[crate::telemetry_schema::endpoint(endpoint(board))]).unwrap();
            host.rx_from_side(&announce, side).unwrap();
            state.board_status.lock().unwrap().get_mut(&board).unwrap().last_seen_instant = Some(std::time::Instant::now());
        }
        let mut service = crate::telemetry_task::RouterServiceForTest::default();
        service.pump(&host);
        let mut confirm = AbortConfirmRefresh::default();
        confirm.start();
        assert_eq!(confirm.poll_av_bay(&state), 0, "umbilical readback goes first");
        if scope::mode_scope(crate::station_mode::local_mode()) & 0b111 == 0 {
            // Test fire tracks no AV bay: nothing is owed there.
            for board in [Board::ActuatorBoard, Board::ValveBoard, Board::GatewayBoard, Board::DaqBoard] {
                state.board_abort.observe_abort(board, std::time::Instant::now(), 1);
            }
            let _ = confirm.poll(&state);
            assert_eq!(confirm.poll_av_bay(&state), 0);
            assert!(!confirm.av_bay_active());
            return;
        }
        assert_eq!(confirm.poll(&state), 0b111_1000);
        for board in [Board::ActuatorBoard, Board::ValveBoard, Board::GatewayBoard, Board::DaqBoard] {
            state.board_abort.observe_abort(board, std::time::Instant::now(), 1);
        }
        let _ = confirm.poll(&state);
        assert!(!confirm.active(), "umbilical readback is in");
        assert!(confirm.av_bay_active());
        assert_eq!(confirm.poll_av_bay(&state), 0b010, "FC first");
        service.pump(&host);
        assert!(av_bay_query_in_flight(&host));
        assert_eq!(confirm.poll_av_bay(&state), 0, "one AV-bay query in flight at a time");
        // FC reads Aborted and its query is answered: PB next, then RF.
        state.board_abort.observe_abort(Board::FlightComputer, std::time::Instant::now(), 2);
        let mut next = Vec::new();
        for board in [Board::PowerBoard, Board::RFBoard] {
            let start = std::time::Instant::now();
            let sent = loop {
                service.pump(&host);
                let sent = confirm.poll_av_bay(&state);
                if sent != 0 || start.elapsed() > Duration::from_secs(10) {
                    break sent;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            };
            next.push(sent);
            state.board_abort.observe_abort(board, std::time::Instant::now(), 3);
        }
        assert_eq!(next, [0b100, 0b001], "PB, then RF");
        assert_eq!(confirm.poll_av_bay(&state), 0);
        assert!(!confirm.av_bay_active(), "every AV-bay board reads Aborted");
    }

    /// Review of 925b07a: a silent FC (unreachable over the relay) comes due
    /// again each time its query gives up, and must not starve PB and RF.
    #[tokio::test]
    async fn a_silent_av_bay_board_does_not_starve_the_others() {
        let _tuning = crate::network_delivery_tuning_for_test();
        let fixture = crate::board_recovery::tests::http_fixture().await;
        let state = fixture.state.clone();
        let host = Arc::new(Router::new(RouterConfig::new([]).with_sender("GS")));
        assert!(state.topology_router.set(host.clone()).is_ok());
        let opts = RouterSideOptions { reliable_enabled: true, ..Default::default() };
        // The radio takes every frame and nothing comes back.
        let radio = host.add_side_packed_with_options("rocket_comms", |_| Ok(()), opts.clone());
        let umbilical = host.add_side_packed_with_options("umbilical_comms", |_| Ok(()), opts);
        for board in BOARDS {
            let side = if UMBILICAL.contains(&index(board).unwrap()) { umbilical } else { radio };
            let announce = sedsnet::discovery::build_discovery_announce(
                board.sender_id(), 1, &[crate::telemetry_schema::endpoint(endpoint(board))]).unwrap();
            host.rx_from_side(&announce, side).unwrap();
        }
        for board in [Board::ActuatorBoard, Board::ValveBoard, Board::GatewayBoard, Board::DaqBoard] {
            state.board_abort.observe_abort(board, std::time::Instant::now(), 1);
        }
        let mut service = crate::telemetry_task::RouterServiceForTest::default();
        service.pump(&host);
        let mut confirm = AbortConfirmRefresh::default();
        confirm.start();
        if scope::mode_scope(crate::station_mode::local_mode()) & 0b111 == 0 {
            let _ = confirm.poll(&state);
            assert_eq!(confirm.poll_av_bay(&state), 0, "test fire tracks no AV bay");
            return;
        }
        let announce = |host: &Router| {
            for board in BOARDS {
                let side = if UMBILICAL.contains(&index(board).unwrap()) { umbilical } else { radio };
                let announce = sedsnet::discovery::build_discovery_announce(
                    board.sender_id(), 1, &[crate::telemetry_schema::endpoint(endpoint(board))]).unwrap();
                host.rx_from_side(&announce, side).unwrap();
            }
        };
        let mut asked = 0u8;
        let mut order = Vec::new();
        let start = std::time::Instant::now();
        let mut announced = start;
        while asked & 0b111 != 0b111 && start.elapsed() < AV_BAY_CONFIRM_WINDOW {
            // The boards keep announcing, as they do on the bench.
            if announced.elapsed() >= Duration::from_secs(1) {
                announce(&host);
                announced = std::time::Instant::now();
            }
            service.pump(&host);
            for board in BOARDS {
                state.board_status.lock().unwrap().get_mut(&board).unwrap().last_seen_instant = Some(std::time::Instant::now());
            }
            let _ = confirm.poll(&state);
            let sent = confirm.poll_av_bay(&state);
            if sent != 0 {
                order.push(sent);
            }

            asked |= sent;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(asked & 0b111, 0b111, "FC, PB and RF are all asked: {order:?}");
    }

    /// A local Abort clears the AV-bay read-only backoff, so background
    /// refresh after the confirm window starts at the normal cadence.
    #[test]
    fn abort_clears_the_av_bay_readonly_backoff() {
        let mut refresh = ReadonlyQueryRefresh::default();
        refresh.misses = [6, 6, 6, 6, 6, 6, 6];
        refresh.clear_av_bay_backoff();
        assert_eq!(refresh.misses, [0, 0, 0, 6, 6, 6, 6]);
    }

    /// Relay-cut runs: an AV-bay query waiting on the radio holds the one
    /// ordered BOARD_RECOVERY_REQUEST stream, so the umbilical confirm queries
    /// sit behind it for up to its 4.5 s retry span. The window must not close
    /// at 3 s while that query is still in flight.
    #[tokio::test]
    async fn the_window_stays_open_while_an_av_bay_query_holds_the_ordered_stream() {
        let fixture = crate::board_recovery::tests::http_fixture().await;
        let state = fixture.state.clone();
        let host = Arc::new(Router::new(RouterConfig::new([]).with_sender("GS")));
        assert!(state.topology_router.set(host.clone()).is_ok());
        let opts = RouterSideOptions { reliable_enabled: true, ..Default::default() };
        // The radio never takes a frame; the umbilical is fine.
        let radio = host.add_side_packed_with_options("rocket_comms", |_| Err(sedsnet::TelemetryError::Io("side tx busy")), opts.clone());
        let _umbilical = host.add_side_packed_with_options("umbilical_comms", |_| Ok(()), opts);
        let announce = sedsnet::discovery::build_discovery_announce(
            "FC", 1, &[crate::telemetry_schema::endpoint(endpoint(Board::FlightComputer))]).unwrap();
        host.rx_from_side(&announce, radio).unwrap();
        host.process_all_queues_with_timeout(0).unwrap();
        host.tx_queue(sedsnet::packet::Packet::new(
            crate::telemetry_schema::data_type("BOARD_RECOVERY_REQUEST"),
            &[crate::telemetry_schema::endpoint(endpoint(Board::FlightComputer))], "GS", 1, Arc::from(&[0u8; 32][..])).unwrap()).unwrap();
        let _ = host.process_all_queues_with_timeout(0);
        assert!(av_bay_query_in_flight(&host), "the FC query holds the ordered stream");
        for board in BOARDS {
            state.board_status.lock().unwrap().get_mut(&board).unwrap().last_seen_instant = Some(std::time::Instant::now());
        }
        let mut confirm = AbortConfirmRefresh::default();
        confirm.start();
        let _ = confirm.poll(&state);
        tokio::time::sleep(CONFIRM_WINDOW + Duration::from_millis(300)).await;
        for board in BOARDS {
            state.board_status.lock().unwrap().get_mut(&board).unwrap().last_seen_instant = Some(std::time::Instant::now());
        }
        let _ = confirm.poll(&state);
        assert!(confirm.active(), "still open past {CONFIRM_WINDOW:?} while the AV-bay query is in flight");
    }

    /// deploy-relaycut (GS eadd886): an RF query waiting on the cut radio held
    /// the ordered stream at the Abort. When it gave up its retries 1.5 s
    /// later, the SDK dropped the umbilical confirm queries queued behind it;
    /// they were asked again only 3 s after the first ask, so the umbilical
    /// readback read 3.1-3.6 s. Had the head given up past the 3 s window,
    /// they would not have been asked again at all. They must go as soon as
    /// the head gives up.
    #[tokio::test]
    async fn umbilical_readback_is_asked_again_as_soon_as_the_av_bay_head_gives_up() {
        let _tuning = crate::network_delivery_tuning_for_test();
        let fixture = crate::board_recovery::tests::http_fixture().await;
        let state = fixture.state.clone();
        let host = Arc::new(Router::new(RouterConfig::new([]).with_sender("GS")));
        assert!(state.topology_router.set(host.clone()).is_ok());
        let opts = RouterSideOptions { reliable_enabled: true, ..Default::default() };
        // The radio takes every frame and nothing comes back (the cut).
        let radio = host.add_side_packed_with_options("rocket_comms", |_| Ok(()), opts.clone());
        let recovery_request = crate::telemetry_schema::data_type("BOARD_RECOVERY_REQUEST");
        let umbilical_queries = Arc::new(std::sync::Mutex::new(Vec::<std::time::Instant>::new()));
        let seen = umbilical_queries.clone();
        let umbilical = host.add_side_packed_with_options("umbilical_comms", move |bytes| {
            if sedsnet::wire_format::peek_frame_info_unchecked(bytes).is_ok_and(|info| info.envelope.ty == recovery_request) {
                seen.lock().unwrap().push(std::time::Instant::now());
            }
            Ok(())
        }, opts);
        for board in BOARDS {
            let side = if UMBILICAL.contains(&index(board).unwrap()) { umbilical } else { radio };
            let announce = sedsnet::discovery::build_discovery_announce(
                board.sender_id(), 1, &[crate::telemetry_schema::endpoint(endpoint(board))]).unwrap();
            host.rx_from_side(&announce, side).unwrap();
            state.board_status.lock().unwrap().get_mut(&board).unwrap().last_seen_instant = Some(std::time::Instant::now());
        }
        let mut service = crate::telemetry_task::RouterServiceForTest::default();
        service.pump(&host);
        // The RF query is in flight when the Abort goes.
        host.tx_queue(Packet::new(recovery_request,
            &[crate::telemetry_schema::endpoint(endpoint(Board::RFBoard))], "GS", 1, Arc::from(&[0u8; 32][..])).unwrap()).unwrap();
        service.pump(&host);
        assert!(av_bay_query_in_flight(&host), "the RF query holds the ordered stream");
        let mut confirm = AbortConfirmRefresh::default();
        confirm.start();
        assert_ne!(confirm.poll(&state), 0, "the umbilical boards are asked at once");
        service.pump(&host);
        assert!(umbilical_queries.lock().unwrap().is_empty(), "queued behind the RF query");
        // The query service's turn (250 ms tick, or sooner when woken).
        let tuning = crate::network_delivery_tuning();
        let span = Duration::from_millis(u64::from(tuning.reliable_retransmit_ms) * u64::from(tuning.reliable_max_retries + 1));
        let start = std::time::Instant::now();
        let mut head_gone = None;
        let mut last_poll = start;
        while start.elapsed() < span + Duration::from_millis(1_500) {
            service.pump(&host);
            if head_gone.is_none() && !av_bay_query_in_flight(&host) {
                head_gone = Some(std::time::Instant::now());
            }
            if last_poll.elapsed() >= Duration::from_millis(25) {
                for board in BOARDS {
                    state.board_status.lock().unwrap().get_mut(&board).unwrap().last_seen_instant = Some(std::time::Instant::now());
                }
                let _ = confirm.poll(&state);
                last_poll = std::time::Instant::now();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let head_gone = head_gone.expect("the RF query gives up within its retry span");
        let first = umbilical_queries.lock().unwrap().iter().copied().find(|at| *at >= head_gone);
        let after = first.map(|at| at.duration_since(head_gone).as_millis());
        eprintln!("RETIRED_HEAD head_gone_after_ms={} umbilical_query_after_head_ms={after:?}", head_gone.duration_since(start).as_millis());
        assert!(after.is_some_and(|ms| ms <= 300), "umbilical confirm query {after:?} ms after the RF query gave up");
    }
}
