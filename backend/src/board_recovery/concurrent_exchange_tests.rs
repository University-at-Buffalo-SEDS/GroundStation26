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
