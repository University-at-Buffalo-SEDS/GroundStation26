// Actual HTTP coordinator + dedicated public-SDK peers with per-board link
// latency. Peer lifecycle is a bounded fixture, not MCU firmware.
fn concurrent_requests(f: &Fixture) -> Vec<(Board, u64)> {
    f.models.iter().flat_map(|(board, model)| {
        model.lock().unwrap().requests.iter()
            .filter(|packet| packet.payload().len() == 32 && packet.payload()[1] == OP_RECOVER)
            .map(|packet| (*board, packet.timestamp())).collect::<Vec<_>>()
    }).collect()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reset_exchanges_every_board_concurrently_within_one_request_window() {
    if isolated("reset_exchanges_every_board_concurrently_within_one_request_window") { return; }
    let fixture = Fixture::new().await;
    let state = fixture.state();
    *state.state.lock().unwrap() = crate::types::FlightState::Aborted;
    for board in BOARDS { fixture.query(board).await; }
    // Each board answers its recovery request only after this latency. One
    // after another, the boards would need longer than one request lifetime
    // from CONFIRM, which is what the boards and the station both enforce.
    let latency = QUERY_TTL / 4;
    assert!(latency * fixture.models.len() as u32 > QUERY_TTL);
    for (_, model) in &fixture.models { model.lock().unwrap().recover_delay = latency; }
    let ticket = fixture.prepare("reset_startup", None).await;
    let confirmed = WallInstant::now();
    assert_eq!(fixture.confirm(&ticket).await.status(), StatusCode::OK);
    let result = fixture.terminal().await;
    assert_eq!(result["success"], true, "{result}");
    assert!(confirmed.elapsed() < QUERY_TTL, "all exchanges overlapped");
    assert!(fixture.models.iter().all(|(_, model)| model.lock().unwrap().applied == 1));
    let requests = concurrent_requests(&fixture);
    let boards = requests.iter().map(|(board, _)| board.sender_id()).collect::<std::collections::BTreeSet<_>>();
    assert_eq!(boards.len(), fixture.models.len(), "every board received its request");
    let stamps = requests.iter().map(|(_, stamp)| *stamp).collect::<std::collections::BTreeSet<_>>();
    assert_eq!(stamps.len(), 1, "every board carries the one canonical CONFIRM timestamp");
    let marker = state.launch_history.lock().unwrap().recovery_pending().unwrap();
    assert!(marker.verified && marker.published);
    assert_eq!(state.operator_admission_barrier.load(Ordering::Acquire), 0);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_reset_pending_targets_that_read_back_healthy_do_not_block_a_single_board_retry() {
    if isolated("failed_reset_pending_targets_that_read_back_healthy_do_not_block_a_single_board_retry") { return; }
    let fixture = Fixture::new().await;
    let state = fixture.state();
    *state.state.lock().unwrap() = crate::types::FlightState::Aborted;
    for board in BOARDS { fixture.query(board).await; }
    let daq = fixture.models.iter().find(|(board, _)| *board == Board::DaqBoard).unwrap().1.clone();
    daq.lock().unwrap().fail_result = Some(5);
    let ticket = fixture.prepare("reset_startup", None).await;
    assert_eq!(fixture.confirm(&ticket).await.status(), StatusCode::OK);
    let failed = fixture.terminal().await;
    assert_eq!(failed["success"], false, "{failed}");
    let pending = state.launch_history.lock().unwrap().recovery_pending().unwrap();
    assert!(!pending.published && pending.targets.len() == BOARDS.len());
    assert_ne!(state.operator_admission_barrier.load(Ordering::Acquire), 0, "a failed reset stays closed");
    // The other boards still got their requests concurrently and applied.
    for (board, model) in &fixture.models {
        assert_eq!(model.lock().unwrap().applied, usize::from(*board != Board::DaqBoard), "{board:?}");
    }
    assert!(!state.launch_history.lock().unwrap().launch_locked(), "a failed reset is not a launch attempt");
    for board in BOARDS { fixture.query(board).await; }
    let status = body(super::super::tests::http(state.clone(), "GET", "/api/board-recovery/status",
        Some("operator"), json!({})).await).await;
    for row in status["boards"].as_array().unwrap() {
        if row["id"] == "DAQ" {
            assert_eq!(row["recovery_required"], true, "{row}");
        } else {
            assert_eq!(row["recovery_required"], false, "a healthy pending target needs nothing: {row}");
        }
    }
    // Only the unsatisfied board needs the retry; it completes the recovery.
    daq.lock().unwrap().fail_result = None;
    let retry = fixture.prepare("recover_abort", Some("DAQ")).await;
    assert_eq!(retry["targets"], json!(["DAQ"]));
    assert_eq!(fixture.confirm(&retry).await.status(), StatusCode::OK);
    let done = fixture.terminal().await;
    assert_eq!(done["success"], true, "{done}");
    let marker = state.launch_history.lock().unwrap().recovery_pending().unwrap();
    assert!(marker.verified && marker.published);
    assert_eq!(state.operator_admission_barrier.load(Ordering::Acquire), 0);
    assert!(!state.launch_history.lock().unwrap().launch_locked());
}
/// After a failed Reset all, the operator can retry the boards that still
/// need it one at a time. A retry that leaves out an unhealthy board of the
/// unfinished recovery carries it: its own board recovers, the station stays
/// closed and names the rest, and recovering the last board publishes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_reset_single_board_retries_carry_the_remaining_unhealthy_boards() {
    if isolated("failed_reset_single_board_retries_carry_the_remaining_unhealthy_boards") { return; }
    let fixture = Fixture::new().await;
    let state = fixture.state();
    *state.state.lock().unwrap() = crate::types::FlightState::Aborted;
    for board in BOARDS { fixture.query(board).await; }
    let model = |wanted: Board| fixture.models.iter().find(|(board, _)| *board == wanted).unwrap().1.clone();
    let (ab, daq) = (model(Board::ActuatorBoard), model(Board::DaqBoard));
    ab.lock().unwrap().fail_result = Some(4);
    daq.lock().unwrap().fail_result = Some(4);
    let ticket = fixture.prepare("reset_startup", None).await;
    assert_eq!(fixture.confirm(&ticket).await.status(), StatusCode::OK);
    assert_eq!(fixture.terminal().await["success"], false);
    // Clear GSE abort names the boards still to recover.
    let message = admission_closed_message(&state, "Clear GSE abort");
    assert!(message.contains("AB") && message.contains("DAQ") && message.contains("Review recovery"), "{message}");
    // Recover AB alone; DAQ is carried.
    ab.lock().unwrap().fail_result = None;
    for board in BOARDS { fixture.query(board).await; }
    let retry = fixture.prepare("recover_abort", Some("AB")).await;
    assert_eq!(retry["targets"], json!(["AB"]));
    assert_eq!(retry["still_unfinished_after"], json!(["DAQ"]));
    assert_eq!(fixture.confirm(&retry).await.status(), StatusCode::OK);
    let done = fixture.terminal().await;
    assert_eq!(done["success"], true, "{done}");
    assert_eq!(done["still_pending"], true, "{done}");
    assert!(done["message"].as_str().unwrap().contains("DAQ"), "{done}");
    let marker = state.launch_history.lock().unwrap().recovery_pending().unwrap();
    assert!(!marker.published);
    assert_eq!(marker.targets.iter().map(|t| t.board.as_str()).collect::<Vec<_>>(), ["AB", "DAQ"]);
    assert_ne!(state.operator_admission_barrier.load(Ordering::Acquire), 0, "still closed for DAQ");
    let message = admission_closed_message(&state, "Clear GSE abort");
    assert!(message.contains("DAQ") && !message.contains("AB,"), "{message}");
    // Recovering DAQ finishes the recovery and reopens the station.
    daq.lock().unwrap().fail_result = None;
    for board in BOARDS { fixture.query(board).await; }
    let last = fixture.prepare("recover_abort", Some("DAQ")).await;
    assert_eq!(last["still_unfinished_after"], json!([]));
    assert_eq!(fixture.confirm(&last).await.status(), StatusCode::OK);
    let done = fixture.terminal().await;
    assert_eq!(done["success"], true, "{done}");
    assert_eq!(done["still_pending"], false, "{done}");
    let marker = state.launch_history.lock().unwrap().recovery_pending().unwrap();
    assert!(marker.verified && marker.published);
    assert_eq!(state.operator_admission_barrier.load(Ordering::Acquire), 0);
}
/// A failed Reset all whose boards all read back healthy afterwards (the
/// refusing board recovered by other means) finishes by itself: the marker
/// is retired, admission reopens and no launch lock is left behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_reset_whose_boards_later_read_back_healthy_finishes_and_reopens() {
    if isolated("failed_reset_whose_boards_later_read_back_healthy_finishes_and_reopens") { return; }
    let fixture = Fixture::new().await;
    let state = fixture.state();
    *state.state.lock().unwrap() = crate::types::FlightState::Aborted;
    for board in BOARDS { fixture.query(board).await; }
    let daq = fixture.models.iter().find(|(board, _)| *board == Board::DaqBoard).unwrap().1.clone();
    daq.lock().unwrap().fail_result = Some(4);
    let ticket = fixture.prepare("reset_startup", None).await;
    assert_eq!(fixture.confirm(&ticket).await.status(), StatusCode::OK);
    assert_eq!(fixture.terminal().await["success"], false);
    assert!(state.launch_history.lock().unwrap().automatic_actions_inhibited());
    // DAQ still reads back aborted: nothing finishes yet.
    for board in BOARDS { fixture.query(board).await; }
    assert!(!complete_satisfied_pending(&state));
    assert_ne!(state.operator_admission_barrier.load(Ordering::Acquire), 0);
    // DAQ now reads back healthy (same boot and epoch, Startup, not aborted).
    { let mut m = daq.lock().unwrap(); m.fail_result = None; m.state = 0; }
    for board in BOARDS { fixture.query(board).await; }
    assert!(complete_satisfied_pending(&state));
    assert_eq!(state.operator_admission_barrier.load(Ordering::Acquire), 0);
    let history = state.launch_history.lock().unwrap();
    assert!(history.recovery_pending().is_none() && !history.recovery_closed());
    assert!(!history.automatic_actions_inhibited() && !history.launch_locked(), "no real Launch: nothing stays locked");
    drop(history);
    assert!(crate::gse::capture_operator_origin(&state).is_some(), "operator commands are admitted again");
    assert!(!complete_satisfied_pending(&state), "nothing left to finish");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_query_replies_reach_every_board_with_no_operation_running() {
    if isolated("background_query_replies_reach_every_board_with_no_operation_running") { return; }
    let fixture = Fixture::new().await;
    let state = fixture.state();
    assert!(state.board_recovery.coordinator.operation().is_none());
    for board in BOARDS {
        state.network_health.lock().unwrap().observe(board.sender_id(), WallInstant::now(), 100);
    }
    let mut refresh = ReadonlyQueryRefresh::default();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let _ = refresh.poll(&state);
            // Every board this station mode tracks (test-fire has no AV bay).
            let scope = super::super::scope::mode_scope(crate::station_mode::local_mode());
            let done = BOARDS.iter().enumerate().filter(|(at, _)| scope & (1 << at) != 0).all(|(at, _)| {
                let inner = state.board_recovery.inner.lock().unwrap();
                inner.queries[at].as_ref().is_some_and(|query| query.received)
                    && inner.values[at].as_ref().is_some_and(|row| row.fresh(Stamp::now()))
            });
            if done { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("every background read-only query reply is observed without any recovery operation");
    let control = state.board_recovery.coordinator.0.lock().unwrap();
    assert!(control.expected.iter().all(Option::is_none), "background replies never need an operation slot");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_prepare_queries_complete_while_background_queries_are_outstanding() {
    if isolated("concurrent_prepare_queries_complete_while_background_queries_are_outstanding") { return; }
    let fixture = Fixture::new().await;
    let state = fixture.state();
    *state.state.lock().unwrap() = crate::types::FlightState::Aborted;
    // Learn every board's Abort episode first, as an operator's status page does.
    for board in BOARDS { fixture.query(board).await; }
    // Hold the wire so every background query is outstanding when PREPARE starts.
    fixture.ignition_wire_hold.store(1, Ordering::Release);
    tokio::time::timeout(Duration::from_secs(1), async {
        while fixture.ignition_wire_hold.load(Ordering::Acquire) != 2 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }).await.unwrap();
    let mut old = [0u64; 7];
    for (at, board) in BOARDS.iter().enumerate() {
        // A never-owned CoreBusy offer may be retried, exactly as PREPARE does.
        tokio::time::timeout(Duration::from_secs(2), async {
            while super::super::request_board_query(&state, *board).status() != StatusCode::OK {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.expect("background query admitted");
        old[at] = state.board_recovery.inner.lock().unwrap().queries[at].as_ref().unwrap().nonce;
    }
    let release = fixture.ignition_wire_hold.clone();
    let released = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        release.store(0, Ordering::Release);
    });
    let ticket = fixture.prepare("reset_startup", None).await;
    released.await.unwrap();
    assert_eq!(ticket["targets"].as_array().unwrap().len(), BOARDS.len(), "{ticket}");
    let inner = state.board_recovery.inner.lock().unwrap();
    for (at, old) in old.iter().enumerate() {
        let query = inner.queries[at].as_ref().unwrap();
        assert_ne!(query.nonce, *old, "PREPARE used its own distinct query");
        assert!(query.received);
    }
    drop(inner);
    assert!(fixture.models.iter().all(|(_, model)| model.lock().unwrap().applied == 0));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepare_replaces_a_lost_background_query_at_once_and_never_accepts_its_reply() {
    if isolated("prepare_replaces_a_lost_background_query_at_once_and_never_accepts_its_reply") { return; }
    let fixture = Fixture::new().await;
    let state = fixture.state();
    *state.state.lock().unwrap() = crate::types::FlightState::Aborted;
    fixture.query(Board::ActuatorBoard).await;
    let at = index(Board::ActuatorBoard).unwrap();
    // A background query whose reply was lost on the link: still pending.
    let lost = 0x5eed_u64;
    state.board_recovery.inner.lock().unwrap().queries[at] =
        Some(Query { abort_revision: state.board_abort.query_revision(Board::ActuatorBoard), nonce: lost, sent: Stamp::now(), received: false });
    // Background demand never replaces a pending query.
    let mut refresh = ReadonlyQueryRefresh::default();
    assert!(!refresh.request(&state, Board::ActuatorBoard));
    assert_eq!(state.board_recovery.inner.lock().unwrap().queries[at].as_ref().unwrap().nonce, lost);
    let started = WallInstant::now();
    let ticket = fixture.prepare("recover_abort", Some("AB")).await;
    assert!(started.elapsed() < Duration::from_secs(3), "PREPARE did not wait out the lost query");
    assert!(ticket["ticket"].is_string());
    let inner = state.board_recovery.inner.lock().unwrap();
    let query = inner.queries[at].as_ref().unwrap();
    assert_ne!(query.nonce, lost, "PREPARE used its own distinct nonce");
    assert!(query.received);
    assert_eq!(inner.values[at].as_ref().unwrap().value.nonce, query.nonce);
    drop(inner);
    // A late reply to the replaced query is rejected, never taken as proof.
    let mut wire = [0u8; 40];
    wire[0] = 1; wire[2] = 1; wire[3] = 0x3e;
    wire[4..12].copy_from_slice(&lost.to_le_bytes());
    wire[12..20].copy_from_slice(&42u64.to_le_bytes());
    wire[20..24].copy_from_slice(&9u32.to_le_bytes());
    wire[28..32].copy_from_slice(&0u32.to_le_bytes());
    wire[32..40].copy_from_slice(&issuer(&state.board_recovery.identity.unwrap()).to_le_bytes());
    let late = Packet::new(crate::telemetry_schema::data_type(STATUS_TYPE),
        &[crate::telemetry_schema::endpoint("GROUND_STATION")], "AB", 1, Arc::from(wire)).unwrap();
    assert!(super::super::observe(&state, &late).unwrap_err().contains("Query the board again"));
    assert_ne!(state.board_recovery.inner.lock().unwrap().values[at].as_ref().unwrap().value.nonce, lost);
}
