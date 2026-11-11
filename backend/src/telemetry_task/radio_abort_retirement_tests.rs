//! A stale automatic ABORT owed to an unreachable AV bay is retired and then
//! never written to the radio; a manual Abort is not; a fresh automatic abort
//! raised afterwards is delivered normally. Actual SDK router, radio budget
//! and writer drop rule; the AV bay is advertised but never acknowledges.
use super::*;
use crate::abort_retirement::{self as retirement, AbortCause, AbortOrigin, RetireReason};
use crate::types::{Board, FlightState};
use sedsnet::router::{RetryTrackerLimits, RouterConfig, RouterSideOptions};
use std::sync::atomic::AtomicU64;
use std::time::Instant;

const AV_BAY: [Board; 3] = [Board::FlightComputer, Board::RFBoard, Board::PowerBoard];

fn abort() -> sedsnet::config::DataType {
    crate::telemetry_schema::data_type("ABORT")
}

struct Bench {
    state: Arc<AppState>,
    router: Arc<Router>,
    clock: Arc<AtomicU64>,
    budget: Arc<TxQueueBudget>,
    activity: Arc<TransportTxActivity>,
    rx: mpsc::UnboundedReceiver<(u8, Vec<u8>)>,
}

impl Bench {
    async fn new() -> Self {
        let state = crate::state::tests::test_app_state().await;
        let clock = Arc::new(AtomicU64::new(1_000));
        let model = clock.clone();
        let router = Arc::new(Router::new_with_clock(
            RouterConfig::new([]),
            Box::new(move || model.load(Ordering::SeqCst)),
        ));
        state.topology_router.set(router.clone()).unwrap();
        let budget = Arc::new(TxQueueBudget::new_radio(2048, 256));
        let activity = Arc::new(TransportTxActivity::default());
        let (tx, rx) = mpsc::unbounded_channel();
        let (side_budget, side_activity) = (budget.clone(), activity.clone());
        let (side, _tracker) = router
            .add_side_packed_leased_with_priority_and_options(
                "rocket_comms",
                move |lease, priority| {
                    side_budget.try_send_leased_tracked(&tx, priority, lease, &side_activity)
                },
                RouterSideOptions { reliable_enabled: false, ..Default::default() }
                    .with_small_packet_transport(192),
                RetryTrackerLimits::default(),
            )
            .unwrap();
        // The AV bay is on the radio behind RF and never acknowledges.
        let node = |board: Board, connection: &str| {
            let mut endpoints = vec![crate::telemetry_schema::endpoint("ABORT")];
            if board == Board::FlightComputer {
                endpoints.push(crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"));
            }
            sedsnet::discovery::TopologyBoardNode {
                sender_id: board.sender_id().into(),
                reachable_endpoints: endpoints,
                reachable_timesync_sources: vec![],
                connections: vec![connection.into()],
            }
        };
        let announce = sedsnet::discovery::build_discovery_announce(
            "RF",
            123,
            &[crate::telemetry_schema::endpoint("ABORT"), crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")],
        )
        .unwrap();
        router.rx_packed_from_side(&sedsnet::wire_format::pack_packet(&announce), side).unwrap();
        let topology = sedsnet::discovery::build_discovery_topology(
            "RF",
            123,
            &[node(Board::RFBoard, "GS"), node(Board::FlightComputer, "RF"), node(Board::PowerBoard, "RF")],
        )
        .unwrap();
        router.rx_from_side(&topology, side).unwrap();
        router.process_all_queues_with_timeout(0).unwrap();
        Self { state, router, clock, budget, activity, rx }
    }

    fn pump(&self, ms: u64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
        let _ = self.router.process_all_queues_with_timeout(0);
    }

    fn abort_ids(&self) -> Vec<u64> {
        self.router
            .export_reliable_pending_stats()
            .into_iter()
            .filter(|row| row.data_type == abort())
            .map(|row| row.packet_id)
            .collect()
    }

    /// Raise the GB-offline automatic abort; returns the SDK's tracking id.
    fn raise(&self) -> u64 {
        self.raise_for(AbortCause::BoardOffline(Board::GatewayBoard))
    }

    fn raise_for(&self, cause: AbortCause) -> u64 {
        let before = self.abort_ids();
        crate::telemetry_task::queue_automatic_abort_packet(&self.router, "Safety Task Abort Command Issued", cause)
            .unwrap();
        self.pump(0);
        let id = *self
            .abort_ids()
            .iter()
            .find(|id| !before.contains(id))
            .expect("the AV bay is advertised, so the SDK tracks the abort");
        assert_eq!(
            retirement::origin(&self.router, id),
            AbortOrigin::Automatic,
            "the registered id is the SDK's own tracking id"
        );
        let row = self.router.export_reliable_pending_stats().into_iter().find(|r| r.packet_id == id).unwrap();
        let owed = row
            .pending_destinations
            .iter()
            .map(|owner| crate::board_recovery::coordinator::pending_destination_board(&self.router, *owner))
            .collect::<Vec<_>>();
        assert!(
            !owed.is_empty() && owed.iter().all(|board| board.is_some_and(|b| AV_BAY.contains(&b))),
            "owed only to named AV bay boards: {owed:?}"
        );
        id
    }

    fn owed(&self, id: u64) -> Vec<Board> {
        let row = self.router.export_reliable_pending_stats().into_iter().find(|r| r.packet_id == id).unwrap();
        row.pending_destinations
            .iter()
            .filter_map(|owner| crate::board_recovery::coordinator::pending_destination_board(&self.router, *owner))
            .collect()
    }

    /// The radio writer: drain everything accepted, apply its drop rule, and
    /// return the ABORT ids that reached the air.
    fn write(&mut self) -> Vec<u64> {
        let mut aired = Vec::new();
        while let Ok((priority, bytes)) = self.rx.try_recv() {
            if discard_retired_write(Some(&self.budget), Some(&self.activity), priority, &bytes) {
                continue;
            }
            if serialize::peek_frame_info(&bytes).is_ok_and(|info| info.envelope.ty == abort()) {
                aired.push(serialize::packet_id_from_wire(&bytes).unwrap());
            }
            self.budget.complete(priority, &bytes);
            self.activity.complete_frame(&bytes);
        }
        aired
    }

    /// Run the router's retries for `seconds` while the writer keeps writing.
    fn run(&mut self, seconds: u64) -> Vec<u64> {
        let mut aired = Vec::new();
        for _ in 0..seconds * 4 {
            self.pump(250);
            aired.extend(self.write());
        }
        aired
    }

    fn prove_non_abort(&self, boards: &[Board]) {
        std::thread::sleep(std::time::Duration::from_millis(2));
        let query_after = Instant::now();
        for board in boards {
            self.state.board_abort.observe_status(
                *board,
                7,
                1,
                22,
                Some(FlightState::Idle as u8),
                false,
                true,
                query_after,
                self.state.board_abort.query_revision(*board),
                Instant::now(),
                crate::telemetry_task::get_current_timestamp_ms(),
            );
        }
    }

    /// The retired frame is never written again and the SDK gives it up.
    fn assert_never_aired_again(&mut self, id: u64) {
        assert!(
            !retirement::try_has_pending_reliable_tx_type(&self.router, abort()).unwrap(),
            "a retired abort no longer holds station checks"
        );
        assert!(crate::gse::commands_still_in_delivery(&self.router, &[abort()]).is_empty());
        let aired = self.run(15);
        assert!(!aired.contains(&id), "retired ABORT reached the air: {aired:?}");
        assert!(
            !self.abort_ids().contains(&id),
            "every dropped offer spends an SDK retry, so the SDK gives the frame up"
        );
    }

    /// The GB is back online and healthy.
    fn gb_back(&self) {
        self.state.mark_board_seen(Board::GatewayBoard.sender_id(), crate::telemetry_task::get_current_timestamp_ms());
    }

    fn notified(&self, text: &str) -> bool {
        let notes = self.state.notifications.lock().unwrap();
        let found = notes.iter().any(|n| {
            n.message.contains("Stale automatic Abort retired")
                && AV_BAY.iter().any(|board| n.message.contains(&format!("owed to {}", board.as_str())))
                && n.message.contains(text)
        });
        if !found {
            eprintln!("notifications: {:?}", notes.iter().map(|n| &n.message).collect::<Vec<_>>());
        }
        found
    }
}

fn after(base: std::time::Duration) -> Instant {
    Instant::now() + crate::sim_freshness::wall(base) + std::time::Duration::from_secs(1)
}

#[tokio::test]
async fn automatic_abort_is_retired_when_the_operator_clears() {
    let mut bench = Bench::new().await;
    let id = bench.raise();
    // Saturated: the first copy waits in the writer queue, retries are refused.
    bench.saturated();
    assert_eq!(retirement::retire_on_operator_clear(&bench.state, &bench.router), 1);
    assert_eq!(retirement::retired_reason(&bench.router, id), Some(RetireReason::OperatorCleared));
    assert!(bench.notified("the operator cleared the GSE abort"));
    assert!(bench.write().is_empty(), "the queued copy is dropped, not written");
    bench.assert_never_aired_again(id);
}

#[tokio::test]
async fn automatic_abort_is_retired_once_every_destination_reports_non_abort() {
    let mut bench = Bench::new().await;
    let id = bench.raise();
    assert_eq!(bench.write(), vec![id], "it is on the air while owed");
    // A reply that races an abort still in its delivery window cancels nothing.
    bench.prove_non_abort(&AV_BAY);
    retirement::evaluate(&bench.state, &bench.router, Instant::now());
    assert_eq!(retirement::retired_reason(&bench.router, id), None);
    // Past the window, a destination without a fresh proof holds it.
    let owed = bench.owed(id);
    bench.state.board_abort.observe_abort(owed[0], Instant::now(), 0);
    retirement::evaluate(&bench.state, &bench.router, after(crate::gse::undeliverable_after()));
    assert_eq!(retirement::retired_reason(&bench.router, id), None);
    bench.prove_non_abort(&owed[..1]);
    retirement::evaluate(&bench.state, &bench.router, after(crate::gse::undeliverable_after()));
    assert_eq!(
        retirement::retired_reason(&bench.router, id),
        Some(RetireReason::DestinationsReportedNonAbort)
    );
    assert!(bench.notified("fresh non-Abort state"));
    bench.assert_never_aired_again(id);
}

#[tokio::test]
async fn automatic_abort_is_retired_after_its_bounded_age() {
    let mut bench = Bench::new().await;
    let id = bench.raise();
    bench.saturated();
    retirement::evaluate(&bench.state, &bench.router, after(crate::gse::undeliverable_after()));
    assert_eq!(retirement::retired_reason(&bench.router, id), None, "no proof yet");
    bench.gb_back();
    retirement::evaluate(&bench.state, &bench.router, after(retirement::AUTOMATIC_ABORT_MAX_AGE));
    assert_eq!(retirement::retired_reason(&bench.router, id), Some(RetireReason::Expired));
    assert!(bench.notified("still undelivered after 30 s"));
    assert!(bench.notified("cause: Gateway Board offline (cleared: the board is back online)"));
    assert!(bench.write().is_empty());
    bench.assert_never_aired_again(id);
}

#[tokio::test]
async fn manual_abort_is_never_retired() {
    let mut bench = Bench::new().await;
    let (id, _, _) = crate::telemetry_task::queue_manual_abort_wire_receipted(
        &bench.router,
        crate::manual_abort_intent::WirePolicy::Untimed,
    )
    .unwrap();
    bench.pump(0);
    assert!(bench.abort_ids().contains(&id));
    assert_eq!(retirement::origin(&bench.router, id), AbortOrigin::Manual);
    bench.prove_non_abort(&AV_BAY);
    assert_eq!(retirement::retire_on_operator_clear(&bench.state, &bench.router), 0);
    retirement::evaluate(&bench.state, &bench.router, after(retirement::AUTOMATIC_ABORT_MAX_AGE));
    assert_eq!(retirement::retired_reason(&bench.router, id), None);
    assert!(retirement::try_has_pending_reliable_tx_type(&bench.router, abort()).unwrap());
    let aired = bench.run(2);
    assert!(aired.contains(&id), "a manual Abort keeps its delivery");
}

#[tokio::test]
async fn fresh_abort_after_a_retirement_is_delivered_normally() {
    let mut bench = Bench::new().await;
    let old = bench.raise();
    bench.saturated();
    bench.gb_back();
    retirement::evaluate(&bench.state, &bench.router, after(retirement::AUTOMATIC_ABORT_MAX_AGE));
    assert_eq!(retirement::retired_reason(&bench.router, old), Some(RetireReason::Expired));
    std::thread::sleep(std::time::Duration::from_millis(3));
    let fresh = bench.raise();
    assert_ne!(fresh, old);
    assert_eq!(retirement::retired_reason(&bench.router, fresh), None);
    assert!(retirement::try_has_pending_reliable_tx_type(&bench.router, abort()).unwrap());
    assert!(!crate::gse::commands_still_in_delivery(&bench.router, &[abort()]).is_empty());
    let aired = bench.run(2);
    assert!(aired.contains(&fresh), "the fresh abort reaches the air");
    assert!(!aired.contains(&old));
}

impl Bench {
    /// The radio is saturated: nothing the writer holds is written, and the
    /// router's retries are refused behind the copy it already holds.
    fn saturated(&self) {
        for _ in 0..2 {
            self.pump(250);
        }
    }
}

#[tokio::test]
async fn board_offline_abort_is_retired_by_age_only_once_the_board_is_back() {
    let mut bench = Bench::new().await;
    let id = bench.raise();
    bench.saturated();
    // GB is still offline: the abort keeps being delivered past 30 s.
    retirement::evaluate(&bench.state, &bench.router, after(retirement::AUTOMATIC_ABORT_MAX_AGE));
    assert_eq!(retirement::retired_reason(&bench.router, id), None, "GB still offline");
    assert!(bench.abort_ids().contains(&id), "still owed while GB is offline");
    assert!(retirement::try_has_pending_reliable_tx_type(&bench.router, abort()).unwrap());
    bench.gb_back();
    retirement::evaluate(&bench.state, &bench.router, after(retirement::AUTOMATIC_ABORT_MAX_AGE));
    assert_eq!(retirement::retired_reason(&bench.router, id), Some(RetireReason::Expired));
    assert!(bench.notified("cause: Gateway Board offline (cleared: the board is back online)"));
    bench.assert_never_aired_again(id);
}

#[tokio::test]
async fn overpressure_abort_is_retired_by_age_only_after_the_overpressure_clears() {
    let mut bench = Bench::new().await;
    crate::gse::set_overpressure_latch_for_test(&bench.state, Some(crate::gse::overpressure::Latch {
        peak_psi: 900.0,
        threshold_psi: 850.0,
        at_ms: 1,
        message: "fixture".into(),
        relief_commanded: true,
        abort: crate::gse::overpressure::AbortStage::Aborted,
        ab_abort_sent: false,
    }));
    let id = bench.raise_for(AbortCause::Overpressure);
    bench.saturated();
    retirement::evaluate(&bench.state, &bench.router, after(retirement::AUTOMATIC_ABORT_MAX_AGE));
    assert_eq!(retirement::retired_reason(&bench.router, id), None, "overpressure persists");
    assert!(retirement::try_has_pending_reliable_tx_type(&bench.router, abort()).unwrap());
    crate::gse::set_overpressure_latch_for_test(&bench.state, None);
    retirement::evaluate(&bench.state, &bench.router, after(retirement::AUTOMATIC_ABORT_MAX_AGE));
    assert_eq!(retirement::retired_reason(&bench.router, id), Some(RetireReason::Expired));
    assert!(bench.notified("cause: tank overpressure (cleared: no overpressure is latched)"));
    bench.assert_never_aired_again(id);
}

#[tokio::test]
async fn an_unevaluable_cause_is_never_retired_by_age() {
    let bench = Bench::new().await;
    let id = bench.raise_for(AbortCause::Unknown("Automatic zero failure abort".into()));
    retirement::evaluate(&bench.state, &bench.router, after(retirement::AUTOMATIC_ABORT_MAX_AGE));
    assert_eq!(retirement::retired_reason(&bench.router, id), None);
    // The operator can still clear it.
    assert_eq!(retirement::retire_on_operator_clear(&bench.state, &bench.router), 1);
    assert!(bench.notified("cause: Automatic zero failure abort (cannot be evaluated; treated as present)"));
}
