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
