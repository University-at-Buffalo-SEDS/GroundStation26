// Same actual ingress -> DbWrite -> SQLite writer before and after the fix.
#[tokio::test]
async fn admitted_board_health_is_saved_with_original_clock_and_raw_words() {
    let (db_tx, mut db_rx) = mpsc::channel(32);
    let state = test_app_state(db_tx.clone()).await;
    let db = state.telemetry_db_pool();
    crate::telemetry_db::ensure_telemetry_schema(&db).await.unwrap();
    let mut words = [0u32; 24];
    words[0] = 1;
    words[1] = 0x100 | ((-123i16 as u16 as u32) << 16);
    words[2] = 123;
    words[4] = 1000;
    let bytes: Vec<u8> = words.iter().flat_map(|v| v.to_le_bytes()).collect();
    let packet = Packet::new(
        crate::telemetry_schema::data_type("BOARD_HEALTH"),
        &[crate::telemetry_schema::endpoint("GROUND_STATION")],
        "DAQ", 123_456, bytes.clone().into(),
    ).unwrap();
    assert!(handle_packet(&state, &db_tx, &test_db_overflow(), packet).await.is_empty());
    let health = state.network_health.lock().unwrap().snapshot(std::time::Instant::now(), false);
    assert_eq!(health.iter().find(|v| v.sender_id == "DAQ").unwrap()
        .diagnostics.as_ref().unwrap().die_temperature_centi_c, Some(-123));
    let mut writes = Vec::new();
    while let Ok(item) = db_rx.try_recv() {
        if let DbQueueItem::Write(write) = item { writes.push(write); }
    }
    insert_db_batch_once(&db, &writes).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM telemetry WHERE data_type='BOARD_HEALTH' AND sender_id='DAQ' AND source_timestamp_ms=123456")
        .fetch_one(&db).await.unwrap();
    assert_eq!(count, 1, "Live health evidence must also be saved through the real SQLite writer");
    let raw: String = sqlx::query_scalar("SELECT payload_json FROM telemetry WHERE data_type='BOARD_HEALTH' AND sender_id='DAQ'")
        .fetch_one(&db).await.unwrap();
    assert_eq!(serde_json::from_str::<Vec<u8>>(&raw).unwrap(), bytes);
}

// An FC that refuses flight commands says why in BOARD_HEALTH word 6 bits
// 24..27; the operator gets one notice through the actual ingress.
#[tokio::test]
async fn fc_board_health_refusal_bits_raise_one_operator_notice() {
    let (db_tx, _db_rx) = mpsc::channel(32);
    let state = test_app_state(db_tx.clone()).await;
    crate::fc_command_gate::forget(&state);
    let packet = |word6: u32, stamp: u64| {
        let mut words = [0u32; 24];
        words[0] = 1;
        words[2] = 1000;
        words[4] = 1000;
        words[6] = word6;
        let bytes: Vec<u8> = words.iter().flat_map(|v| v.to_le_bytes()).collect();
        Packet::new(
            crate::telemetry_schema::data_type("BOARD_HEALTH"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "FC", stamp, bytes.into(),
        ).unwrap()
    };
    let refusing = |state: &AppState| state.notifications_snapshot().iter()
        .filter(|n| n.message.starts_with("FC refuses flight commands: its persistent store is not ready and its recovery floor failed"))
        .count();
    assert!(handle_packet(&state, &db_tx, &test_db_overflow(), packet(5, 1_000)).await.is_empty());
    assert_eq!(refusing(&state), 0, "watchdog bits alone are not a refusal");
    handle_packet(&state, &db_tx, &test_db_overflow(), packet(5 | (9 << 24), 6_000)).await;
    handle_packet(&state, &db_tx, &test_db_overflow(), packet(9 << 24, 11_000)).await;
    if crate::station_mode::local_mode() == crate::station_mode::Mode::TestFire {
        // A test fire has no AV bay: no FC refusal notices.
        assert_eq!(refusing(&state), 0);
        crate::fc_command_gate::forget(&state);
        return;
    }
    assert_eq!(refusing(&state), 1, "{:?}", state.notifications_snapshot());
    // A diagnostics word from another board never reads as an FC refusal.
    let mut words = [0u32; 24];
    words[0] = 1;
    words[4] = 1000;
    words[6] = 9 << 24;
    let bytes: Vec<u8> = words.iter().flat_map(|v| v.to_le_bytes()).collect();
    let pb = Packet::new(crate::telemetry_schema::data_type("BOARD_HEALTH"),
        &[crate::telemetry_schema::endpoint("GROUND_STATION")], "PB", 12_000, bytes.into()).unwrap();
    handle_packet(&state, &db_tx, &test_db_overflow(), pb).await;
    assert_eq!(refusing(&state), 1);
    crate::fc_command_gate::forget(&state);
}

// An AB that refuses an operator command says why in BOARD_HEALTH words
// 16..18; the operator gets one notice through the actual ingress, and a
// refused drive change also shows on the fill-drive panel.
#[tokio::test]
async fn ab_board_health_refusal_raises_one_operator_notice() {
    let (db_tx, _db_rx) = mpsc::channel(32);
    let state = test_app_state(db_tx.clone()).await;
    crate::ab_command_gate::forget(&state);
    let packet = |sender: &str, words: [u32; 24], stamp: u64| {
        let bytes: Vec<u8> = words.iter().flat_map(|v| v.to_le_bytes()).collect();
        Packet::new(
            crate::telemetry_schema::data_type("BOARD_HEALTH"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            sender, stamp, bytes.into(),
        ).unwrap()
    };
    let count = |state: &AppState, prefix: &str| state.notifications_snapshot().iter()
        .filter(|n| n.message.starts_with(prefix))
        .count();
    let qd = "QD retract: refused by AB because the selected fill drive does not support it";
    let words = crate::ab_command_gate::health_words;
    handle_packet(&state, &db_tx, &test_db_overflow(), packet("AB", words(5_000, 0, 0, 0, 0), 1_000)).await;
    assert_eq!(count(&state, "QD retract"), 0, "no refusal since boot");
    handle_packet(&state, &db_tx, &test_db_overflow(), packet("AB", words(6_000, 1, 8, 3, 0), 2_000)).await;
    handle_packet(&state, &db_tx, &test_db_overflow(), packet("AB", words(11_000, 1, 8, 3, 5_000), 7_000)).await;
    assert_eq!(count(&state, qd), 1, "{:?}", state.notifications_snapshot());
    // The same words from another board never read as an AB refusal.
    handle_packet(&state, &db_tx, &test_db_overflow(), packet("VB", words(12_000, 2, 8, 3, 0), 8_000)).await;
    assert_eq!(count(&state, qd), 1);
    handle_packet(&state, &db_tx, &test_db_overflow(), packet("AB", words(12_500, 2, 0x42, 5, 0), 8_500)).await;
    let drive = "Fill drive change to Auxiliary: refused by AB because a fill-drive motion or drive change is still in progress.";
    assert_eq!(count(&state, drive), 1, "{:?}", state.notifications_snapshot());
    let view = crate::actuator_plumbing::view_for_test(&state);
    assert_eq!(view["message"], drive);
    crate::ab_command_gate::forget(&state);
}

// The FC's Kalman filter status (BOARD_HEALTH words 13/15) through the actual
// ingress: after Launch its start raises the operator notification, the
// status reaches the API/WebSocket view, and the change is recorded. A
// listening RRLS (not the primary role) decodes the same broadcast report.
#[tokio::test]
async fn fc_estimator_start_after_launch_is_notified_shown_and_recorded() {
    for primary in [true, false] {
        let (db_tx, mut db_rx) = mpsc::channel(64);
        let state = test_app_state(db_tx.clone()).await;
        state.set_preexisting_flight_state_for_test(FlightState::Launch);
        crate::fc_estimator::tick(&state);
        let packet = |phase: u32, cycles: u32, start: u32, uptime: u32, stamp: u64| {
            let mut words = [0u32; 24];
            words[0] = 1;
            words[2] = uptime;
            words[4] = 1000;
            words[13] = (crate::fc_estimator::TAG << 24) | (phase << 20) | cycles;
            words[15] = crate::fc_estimator::word15(start, 0);
            let bytes: Vec<u8> = words.iter().flat_map(|v| v.to_le_bytes()).collect();
            Packet::new(crate::telemetry_schema::data_type("BOARD_HEALTH"),
                &[crate::telemetry_schema::endpoint("GROUND_STATION")], "FC", stamp, bytes.into()).unwrap()
        };
        let overflow = test_db_overflow();
        let run = |pkt| handle_packet_inner(&state, &db_tx, &overflow, pkt, primary, None,
            crate::gse::automatic_generation(&state));
        assert!(run(packet(1, 0, 0, 50_000, 7_000)).await.is_empty());
        run(packet(2, 12, 50_001, 50_251, 7_250)).await;
        let running: Vec<_> = state.notifications_snapshot().into_iter()
            .filter(|n| n.message.starts_with("FC flight estimator running (Kalman filter started at T+")).collect();
        assert_eq!(running.len(), 1, "primary={primary}: {:?}", state.notifications_snapshot());
        let status = state.fc_estimator.snapshot();
        assert_eq!(status.phase, Some(crate::fc_estimator::Phase::Running), "primary={primary}");
        assert_eq!(status.started_network_ms, Some(6_999), "start 50_001 travels as 50_000");
        assert!(status.launch_seen);
        if crate::station_mode::local_mode() != crate::station_mode::Mode::TestFire {
            assert_eq!(status.state, "running");
        }
        let mut recorded = Vec::new();
        while let Ok(item) = db_rx.try_recv() {
            if let DbQueueItem::Write(crate::telemetry_db::DbWrite::Telemetry { data_type, payload_json, .. }) = item
                && data_type == "GS_FC_ESTIMATOR"
            {
                recorded.push(serde_json::from_str::<serde_json::Value>(&payload_json).unwrap());
            }
        }
        assert!(recorded.iter().any(|row| row["phase"] == "running"), "primary={primary}: {recorded:?}");
        let api = serde_json::to_value(crate::web::WsOutMsg::FcEstimator(status)).unwrap();
        assert_eq!(api["ty"], "FcEstimator");
        assert_eq!(api["data"]["phase"], "running");
    }
}
