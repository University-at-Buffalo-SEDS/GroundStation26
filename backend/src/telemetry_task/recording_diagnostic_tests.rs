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
