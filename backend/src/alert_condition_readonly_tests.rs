use super::*;
#[tokio::test]
async fn old_selected_recording_projects_null_correlation_without_schema_or_file_mutation() {
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    let mut state = crate::state::tests::test_app_state().await;
    crate::ensure_auth_sessions_table(&state.auth_db)
        .await
        .unwrap();
    let dir = std::path::Path::new(&state.placeholder_db_path)
        .parent()
        .unwrap()
        .to_owned();
    let auth = dir.join("auth.json");
    std::fs::write(
        &auth,
        r#"{"version":1,"anonymous":{"view_data":true,"send_commands":false},"users":[]}"#,
    )
    .unwrap();
    Arc::get_mut(&mut state).unwrap().auth = Arc::new(crate::auth::AuthManager::new(auth));
    let path = dir.join("old-recording.db");
    let old = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true),
        )
        .await
        .unwrap();
    sqlx::query("CREATE TABLE alerts(id INTEGER PRIMARY KEY,timestamp_ms INTEGER,severity TEXT,message TEXT)").execute(&old).await.unwrap();
    sqlx::query("INSERT INTO alerts VALUES(1,100,'warning','Historical warning')")
        .execute(&old)
        .await
        .unwrap();
    old.close().await;
    let bytes = std::fs::read(&path).unwrap();
    let readonly = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::new().filename(&path).read_only(true))
        .await
        .unwrap();
    state.replace_telemetry_db(readonly.clone(), path.display().to_string());
    let response = get_alerts(
        State(state.clone()),
        HeaderMap::new(),
        Query(HistoryParams { minutes: Some(20) }),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 8192)
        .await
        .unwrap();
    let rows: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(rows[0]["message"], "Historical warning");
    assert!(rows[0]["condition_occurrence_id"].is_null());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let columns = sqlx::query("PRAGMA table_info(alerts)")
        .fetch_all(&readonly)
        .await
        .unwrap();
    assert!(
        !columns
            .iter()
            .any(|row| row.get::<String, _>("name") == "condition_json")
    );
    readonly.close().await;
    drop(state);
    assert!(!dir.exists());
}
