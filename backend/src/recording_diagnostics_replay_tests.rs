#[tokio::test]
async fn diagnostics_replay_carries_exact_payload_and_self_contained_descriptors() {
    let db = crate::telemetry_db::open_in_memory_telemetry_db()
        .await
        .unwrap();
    let payload = serde_json::json!({"context":{"version":1},"counter":u64::MAX.to_string(),"topology_valid":true,"links":[{"source":"GS","target":"DAQ"}]});
    sqlx::query("INSERT INTO telemetry(timestamp_ms,source_timestamp_ms,sender_id,data_type,values_json,payload_json) VALUES(1000,900,'GS/radio','REC_SIDE_COUNTERS',?,?)")
        .bind(format!("[{}]",u64::MAX)).bind(payload.to_string()).execute(&db).await.unwrap();
    let export = export_from(&db).await.unwrap();
    assert_eq!(
        export["samples"][0]["diagnostic"]["counter"],
        u64::MAX.to_string()
    );
    assert_eq!(export["samples"][0]["source_timestamp_ms"], 900);
    assert_eq!(export["recording_diagnostics_schema"]["version"], 1);
    let page = page_from(
        &db,
        &Window {
            upper_id: 1,
            after_id: 0,
            start_ms: 0,
            end_ms: 2000,
            types: "[\"REC_SIDE_COUNTERS\"]".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        page.rows[0].diagnostic.as_ref().unwrap()["links"][0]["target"],
        "DAQ"
    );
    assert!(page.diagnostic_streams.is_some());
}
#[tokio::test]
async fn legacy_replay_without_payload_column_stays_readable_and_oversize_is_not_truncated() {
    let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
    sqlx::query("CREATE TABLE telemetry(id INTEGER PRIMARY KEY,timestamp_ms INTEGER,source_timestamp_ms INTEGER,sender_id TEXT,data_type TEXT,values_json TEXT)").execute(&db).await.unwrap();
    sqlx::query("INSERT INTO telemetry VALUES(1,1000,900,'FC','BAROMETER_DATA','[12]')")
        .execute(&db)
        .await
        .unwrap();
    let export = export_from(&db).await.unwrap();
    assert_eq!(export["samples"][0]["values"][0], 12.0);
    assert!(export["samples"][0].get("diagnostic").is_none());
    let actual = crate::telemetry_db::open_in_memory_telemetry_db()
        .await
        .unwrap();
    sqlx::query("INSERT INTO telemetry(timestamp_ms,sender_id,data_type,values_json,payload_json) VALUES(1000,'GS','REC_NETWORK_STATE','[]',?)")
        .bind(format!("\"{}\"","x".repeat(16384))).execute(&actual).await.unwrap();
    assert!(
        export_from(&actual)
            .await
            .unwrap_err()
            .to_string()
            .contains("oversized recorded diagnostic")
    );
}
