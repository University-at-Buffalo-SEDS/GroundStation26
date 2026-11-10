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

#[derive(Default)]
pub(crate) struct AbortConfirmRefresh {
    since: Option<Stamp>,
    /// Umbilical boards (BOARDS mask) whose Abort readback is still owed.
    owed: u8,
    /// An AV-bay BOARD_RECOVERY_REQUEST was in flight at the last poll.
    av_bay_head: bool,
}
impl AbortConfirmRefresh {
    /// Start (or restart) the window for an Abort the station just sent.
    pub(crate) fn start(&mut self) {
        self.since = Some(Stamp::now());
        self.owed = UMBILICAL.iter().fold(0, |mask, at| mask | (1 << at));
    }
    /// True while the window is open and an umbilical board still owes its
    /// Abort readback.
    pub(crate) fn active(&mut self) -> bool {
        let now = Stamp::now();
        let window = if self.av_bay_head { CONFIRM_WINDOW_MAX } else { CONFIRM_WINDOW };
        if self.owed == 0
            || self.since.is_some_and(|since| now.checked_duration_since(since).is_none_or(|age| age >= window))
        {
            self.since = None;
            self.owed = 0;
        }
        self.since.is_some()
    }
    /// Ask the umbilical boards that still owe an Abort readback. Returns the
    /// mask (BOARDS order) of boards a query went to.
    pub(crate) fn poll(&mut self, state: &Arc<AppState>) -> u8 {
        if self.since.is_some() {
            self.av_bay_head = state.topology_router.get().is_some_and(|router| av_bay_query_in_flight(router));
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
        let due = {
            let inner = state.board_recovery.inner.lock().unwrap();
            UMBILICAL.into_iter()
                .filter(|&at| self.owed & (1 << at) != 0 && confirm_query_due(inner.queries[at].as_ref(), since, now))
                .collect::<Vec<_>>()
        };
        due.into_iter().fold(0, |sent, at| {
            if requery_board_query(state, BOARDS[at]).status() == StatusCode::OK { sent | (1 << at) } else { sent }
        })
    }
}
/// Whether an AV-bay BOARD_RECOVERY_REQUEST is in flight: the one ordered
/// stream then holds every umbilical query behind it.
fn av_bay_query_in_flight(router: &sedsnet::router::Router) -> bool {
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
fn confirm_query_due(query: Option<&Query>, since: Stamp, now: Stamp) -> bool {
    match query {
        // Asked since the Abort: wait for the reply, then ask again shortly
        // after a reply that did not show the Abort, or once the query looks
        // lost (dropped behind an AV-bay query that gave up).
        Some(query) if query.sent.checked_duration_since(since).is_some() => {
            let age = now.checked_duration_since(query.sent);
            if query.received {
                age.is_some_and(|age| age >= CONFIRM_REQUERY)
            } else {
                age.is_some_and(|age| age >= CONFIRM_LOST)
            }
        }
        // Not asked since the Abort; an older query cannot show it.
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sedsnet::router::{Router, RouterConfig, RouterSideOptions};

    #[test]
    fn a_board_is_asked_once_per_reply_until_it_reads_aborted() {
        let since = Stamp::now();
        let at = |ms: u64| since + Duration::from_millis(ms);
        let query = |sent: Stamp, received: bool| Query { abort_revision: 0, nonce: 1, sent, received };
        // Nothing asked since the Abort: ask now, even if an older background
        // query is still unanswered.
        assert!(confirm_query_due(None, since, at(0)));
        assert!(confirm_query_due(Some(&query(since - Duration::from_millis(500), false)), since, at(0)));
        assert!(confirm_query_due(Some(&query(since - Duration::from_millis(500), true)), since, at(0)));
        // One prompt query in flight at a time.
        assert!(!confirm_query_due(Some(&query(at(10), false)), since, at(2_000)));
        // Unanswered past CONFIRM_LOST: dropped with the ordered stream's tail
        // behind an AV-bay query that gave up. Ask again.
        assert!(confirm_query_due(Some(&query(at(10), false)), since, at(10) + CONFIRM_LOST));
        // It answered without the Abort (an Aborted reply sets the board's
        // Abort row and stops the asking): ask again shortly after.
        assert!(!confirm_query_due(Some(&query(at(10), true)), since, at(200)));
        assert!(confirm_query_due(Some(&query(at(10), true)), since, at(10) + CONFIRM_REQUERY));
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
}
