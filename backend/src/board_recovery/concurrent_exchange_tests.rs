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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_reset_retry_still_must_include_an_unhealthy_pending_board() {
    if isolated("failed_reset_retry_still_must_include_an_unhealthy_pending_board") { return; }
    let fixture = Fixture::new().await;
    let state = fixture.state();
    *state.state.lock().unwrap() = crate::types::FlightState::Aborted;
    for board in BOARDS { fixture.query(board).await; }
    let daq = fixture.models.iter().find(|(board, _)| *board == Board::DaqBoard).unwrap().1.clone();
    daq.lock().unwrap().fail_result = Some(5);
    let ticket = fixture.prepare("reset_startup", None).await;
    assert_eq!(fixture.confirm(&ticket).await.status(), StatusCode::OK);
    assert_eq!(fixture.terminal().await["success"], false);
    for board in BOARDS { fixture.query(board).await; }
    let response = super::super::tests::http(state.clone(), "POST", "/api/board-recovery/prepare", Some("operator"),
        json!({"action":"recover_abort","board":"GB","mode":null,
            "backend_instance":hex(&state.board_recovery.identity.unwrap()),
            "reset_epoch":format!("{:016x}",crate::gse::automatic_generation(&state))})).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let value = body(response).await;
    assert!(value["message"].as_str().unwrap().contains("has not since read back healthy"), "{value}");
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
