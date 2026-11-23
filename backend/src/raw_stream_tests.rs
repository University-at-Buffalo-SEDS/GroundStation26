use super::*;
use crate::recording_export::tests::{TestDirectory, export_state};
use axum::http::Request as HttpRequest;
use tower::ServiceExt;

// --- fixtures -------------------------------------------------------------

async fn fixture() -> (TestDirectory, Arc<AppState>, Router) {
    let root = TestDirectory::new();
    let state = export_state(&root.0, false).await;
    write_users(&root, true);
    for (token, name) in [
        ("admin-session", "administrator"),
        ("viewer-session", "viewer"),
    ] {
        sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES(?,?,'session',1,1,0,9999999999999)")
            .bind(token).bind(name).execute(&state.auth_db).await.unwrap();
    }
    let app = Router::new()
        .route("/api/command", post(|| async { "reached" }))
        .route("/api/recent", get(|| async { "reached" }))
        .route("/ws", get(|| async { "reached" }))
        .route("/api/media-assets/program", get(|| async { "reached" }))
        .route("/api/voice/ws", get(|| async { "reached" }))
        .route("/api/auth/logout", post(crate::web::logout))
        .merge(crate::admin::routes())
        .merge(routes())
        .layer(axum::middleware::from_fn(crate::watch::scope_watch_tokens))
        .layer(axum::middleware::from_fn(scope_stream_tokens))
        .with_state(state.clone());
    (root, state, app)
}

fn write_users(root: &TestDirectory, administrator_is_admin: bool) {
    let roles = if administrator_is_admin {
        serde_json::json!(["admin"])
    } else {
        serde_json::json!([])
    };
    std::fs::write(
        root.0.join("auth.json"),
        serde_json::json!({"version":1,"anonymous":{"view_data":false},"users":[
            {"username":"administrator","roles":roles,"password":{"salt_b64":"","hash_b64":""},
             "permissions":{"view_data":true,"send_commands":true}},
            {"username":"viewer","roles":[],"password":{"salt_b64":"","hash_b64":""},
             "permissions":{"view_data":true}}
        ]})
        .to_string(),
    )
    .unwrap();
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<serde_json::Value>,
) -> (StatusCode, String) {
    let mut request = HttpRequest::builder().method(method).uri(uri);
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let request = match body {
        Some(body) => request
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    }
    .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn issue(app: &Router, body: serde_json::Value) -> serde_json::Value {
    let (status, text) = call(
        app,
        "POST",
        "/api/admin/stream-tokens",
        Some("admin-session"),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    serde_json::from_str(&text).unwrap()
}

fn telemetry(data_type: &str, sender: &str, ts: i64, values: Vec<Option<f32>>) -> TelemetryRow {
    TelemetryRow {
        timestamp_ms: ts,
        data_type: data_type.into(),
        sender_id: sender.into(),
        values,
    }
}

fn out(data_type: &str, sender: &str, ts: i64) -> RowOut {
    let (fields, units) = schema(data_type);
    RowOut {
        ty: "row",
        seq: 0,
        data_type: data_type.into(),
        sender: sender.into(),
        received_ms: ts,
        source_ms: None,
        network_ms: None,
        values: vec![Some(ts as f32)],
        fields,
        units,
        provenance: "live",
    }
}

fn query(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

// --- auth and scope ---------------------------------------------------------

#[tokio::test]
async fn stream_token_reads_only_the_feed_and_never_commands() {
    let (_root, state, app) = fixture().await;
    let grant = issue(
        &app,
        serde_json::json!({"label":"OBS overlay","types":["FUEL_TANK_PRESSURE"]}),
    )
    .await;
    let token = grant["token"].as_str().unwrap().to_string();
    assert!(token.starts_with("gss1."));
    assert_eq!(grant["label"], "OBS overlay");
    assert_eq!(grant["issuer"], "administrator");
    assert_eq!(grant["types"], serde_json::json!(["FUEL_TANK_PRESSURE"]));
    // Only the digest is stored.
    let stored: Vec<String> = sqlx::query_scalar("SELECT token FROM auth_sessions")
        .fetch_all(&state.auth_db)
        .await
        .unwrap();
    assert!(stored.iter().all(|t| !t.contains(&token[5..])));
    assert!(stored.iter().any(|t| t.starts_with("gss1h:")));

    for (method, path) in [
        ("POST", "/api/command"),
        ("GET", "/ws"),
        ("GET", "/api/recent"),
        ("GET", "/api/admin/users"),
        ("GET", "/api/admin/stream-tokens"),
        ("POST", "/api/admin/stream-tokens"),
        ("GET", "/api/media-assets/program"),
        ("GET", "/api/voice/ws"),
    ] {
        let (status, _) = call(&app, method, path, Some(&token), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path} with header");
        let (status, _) = call(&app, method, &format!("{path}?token={token}"), None, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path} with query");
    }
    // The auth layer itself refuses it for any ordinary permission.
    for permission in [Permission::ViewData, Permission::SendCommands] {
        assert!(matches!(
            state
                .auth
                .authorize_token(&state.auth_db, Some(&token), permission)
                .await,
            Err(AuthFailure::Forbidden(_))
        ));
    }
    let (status, body) = call(&app, "GET", CATALOG_PATH, Some(&token), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let catalog: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(catalog["schema"], SCHEMA);
    assert_eq!(catalog["viewer"]["kind"], "stream_token");
    let (status, _) = call(
        &app,
        "GET",
        &format!("{CATALOG_PATH}?token={token}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // An ordinary session may read the feed too; nobody anonymous may.
    assert_eq!(
        call(&app, "GET", CATALOG_PATH, Some("viewer-session"), None)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, "GET", CATALOG_PATH, None, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    // Bad parameters are a 400 with a message.
    let (status, body) = call(
        &app,
        "GET",
        &format!("{CATALOG_PATH}?max_hz=5000"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("max_hz"));
}

#[tokio::test]
async fn only_administrators_issue_list_and_revoke() {
    let (_root, _state, app) = fixture().await;
    let (status, _) = call(
        &app,
        "POST",
        "/api/admin/stream-tokens",
        Some("viewer-session"),
        Some(serde_json::json!({"label":"x"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(
        &app,
        "POST",
        "/api/admin/stream-tokens",
        None,
        Some(serde_json::json!({"label":"x"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        call(
            &app,
            "GET",
            "/api/admin/stream-tokens",
            Some("viewer-session"),
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &app,
            "POST",
            "/api/admin/stream-tokens/0123456789abcdef/revoke",
            Some("viewer-session"),
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    for bad in [
        serde_json::json!({"label":""}),
        serde_json::json!({"label":"a","ttl_hours":0}),
        serde_json::json!({"label":"a","ttl_hours":9000}),
        serde_json::json!({"label":"a","max_hz":0.0}),
        serde_json::json!({"label":"a","types":["BAD TYPE"]}),
        serde_json::json!({"label":"a","unknown":1}),
    ] {
        let (status, body) = call(
            &app,
            "POST",
            "/api/admin/stream-tokens",
            Some("admin-session"),
            Some(bad.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad} -> {body}");
    }
    let grant = issue(
        &app,
        serde_json::json!({"label":"script","ttl_hours":2,"senders":["VB"],"max_hz":5}),
    )
    .await;
    let (status, body) = call(
        &app,
        "GET",
        "/api/admin/stream-tokens",
        Some("admin-session"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !body.contains(grant["token"].as_str().unwrap()),
        "list never shows the token"
    );
    let list: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(list[0]["id"], grant["id"]);
    assert_eq!(list[0]["senders"], serde_json::json!(["VB"]));
    assert_eq!(list[0]["max_hz"], 5.0);
    assert!(
        list[0]["expires_at_ms"].as_i64().unwrap() - list[0]["created_ms"].as_i64().unwrap()
            == 2 * 3_600_000
    );
    assert!(list[0].get("token").is_none());
}

#[tokio::test]
async fn revocation_expiry_and_issuer_loss_end_access() {
    let (root, state, app) = fixture().await;
    let token = |grant: &serde_json::Value| grant["token"].as_str().unwrap().to_string();
    let a = issue(&app, serde_json::json!({"label":"a"})).await;
    let b = issue(&app, serde_json::json!({"label":"b"})).await;
    let c = issue(&app, serde_json::json!({"label":"c"})).await;
    let id = a["id"].as_str().unwrap();
    let (status, body) = call(
        &app,
        "POST",
        &format!("/api/admin/stream-tokens/{id}/revoke"),
        Some("admin-session"),
        None,
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::OK, r#"{"revoked":1}"#)
    );
    assert_eq!(
        call(&app, "GET", CATALOG_PATH, Some(&token(&a)), None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&app, "GET", CATALOG_PATH, Some(&token(&b)), None)
            .await
            .0,
        StatusCode::OK
    );
    // Expired.
    sqlx::query("UPDATE auth_sessions SET expires_at_ms = 1 WHERE session_type = ?")
        .bind(format!("stream|{}", b["id"].as_str().unwrap()))
        .execute(&state.auth_db)
        .await
        .unwrap();
    assert_eq!(
        call(&app, "GET", CATALOG_PATH, Some(&token(&b)), None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    // The issuer is no longer an administrator.
    write_users(&root, false);
    assert!(matches!(
        state.auth.authorize_stream(&state.auth_db, &token(&c)).await,
        Err(AuthFailure::Unauthorized(msg)) if msg.contains("administrator")
    ));
    write_users(&root, true);
    // Logging out with a stream token removes only that row.
    let (status, _) = call(&app, "POST", "/api/auth/logout", Some(&token(&c)), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        call(&app, "GET", CATALOG_PATH, Some(&token(&c)), None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&app, "GET", CATALOG_PATH, Some("admin-session"), None)
            .await
            .0,
        StatusCode::OK
    );
    // Bad id.
    assert_eq!(
        call(
            &app,
            "POST",
            "/api/admin/stream-tokens/zz/revoke",
            Some("admin-session"),
            None
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn open_ndjson_stream_delivers_rows_then_ends_with_bye_on_revocation() {
    let (_root, state, app) = fixture().await;
    let grant = issue(
        &app,
        serde_json::json!({"label":"curl","types":["FUEL_TANK_PRESSURE"]}),
    )
    .await;
    let token = grant["token"].as_str().unwrap().to_string();
    let response = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .uri(format!("{FEED_PATH}?token={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/x-ndjson"
    );
    let mut body = response.into_body().into_data_stream();
    let mut lines: Vec<serde_json::Value> = Vec::new();
    let mut pending = String::new();
    let mut next_lines = async |lines: &mut Vec<serde_json::Value>| {
        let chunk = tokio::time::timeout(Duration::from_secs(5), body.next())
            .await
            .ok()
            .flatten();
        let Some(Ok(chunk)) = chunk else { return false };
        pending.push_str(std::str::from_utf8(&chunk).unwrap());
        while let Some(end) = pending.find('\n') {
            let line: String = pending.drain(..=end).collect();
            lines.push(serde_json::from_str(line.trim()).unwrap());
        }
        true
    };
    assert!(next_lines(&mut lines).await);
    assert_eq!(lines[0]["ty"], "hello");
    assert_eq!(
        lines[0]["filters"]["scope_types"],
        serde_json::json!(["FUEL_TANK_PRESSURE"])
    );
    // Rows outside the scope never arrive; rows inside do.
    tokio::time::sleep(Duration::from_millis(30)).await;
    let _ = state
        .ws_tx
        .send(telemetry("KG1000", "DAQ", 10, vec![Some(0.3)]));
    let _ = state
        .ws_tx
        .send(telemetry("FUEL_TANK_PRESSURE", "VB", 11, vec![Some(512.0)]));
    while !lines.iter().any(|l| l["ty"] == "row") {
        assert!(next_lines(&mut lines).await, "no row arrived");
    }
    let row = lines.iter().find(|l| l["ty"] == "row").unwrap();
    assert_eq!(row["data_type"], "FUEL_TANK_PRESSURE");
    assert_eq!(row["units"], serde_json::json!(["psi"]));
    assert!(!lines.iter().any(|l| l["data_type"] == "KG1000"));
    // Revoke while open: the stream says bye and ends.
    let id = grant["id"].as_str().unwrap();
    call(
        &app,
        "POST",
        &format!("/api/admin/stream-tokens/{id}/revoke"),
        Some("admin-session"),
        None,
    )
    .await;
    while next_lines(&mut lines).await {}
    let last = lines.last().unwrap();
    assert_eq!(last["ty"], "bye", "{lines:?}");
    assert_eq!(last["reason"], "revoked");
}

#[tokio::test]
async fn websocket_feed_hello_rows_status_and_bye() {
    use tokio_tungstenite::tungstenite::Message as WsMessage;
    let (_root, state, app) = fixture().await;
    let grant = issue(&app, serde_json::json!({"label":"browser source"})).await;
    let token = grant["token"].as_str().unwrap().to_string();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app.clone()).await.unwrap() });
    // A forbidden WebSocket path for the same token.
    let refused = tokio_tungstenite::connect_async(format!("ws://{addr}/ws?token={token}")).await;
    assert!(refused.is_err());
    let (mut socket, _) = tokio_tungstenite::connect_async(format!(
        "ws://{addr}{WS_PATH}?token={token}&types=PRESSURE*,VALVE_STATE&max_hz=1000"
    ))
    .await
    .unwrap();
    let mut next = async || -> serde_json::Value {
        loop {
            match tokio::time::timeout(Duration::from_secs(5), socket.next())
                .await
                .unwrap()
            {
                Some(Ok(WsMessage::Text(text))) => return serde_json::from_str(&text).unwrap(),
                Some(Ok(WsMessage::Close(_))) | None => return serde_json::json!({"ty":"closed"}),
                Some(Ok(_)) => continue,
                Some(Err(_)) => return serde_json::json!({"ty":"closed"}),
            }
        }
    };
    let hello = next().await;
    assert_eq!(hello["ty"], "hello");
    assert_eq!(
        hello["filters"]["types"],
        serde_json::json!(["PRESSURE*", "VALVE_STATE"])
    );
    assert_eq!(hello["limits"]["max_hz"], 1000.0);
    tokio::time::sleep(Duration::from_millis(30)).await;
    let _ = state.ws_tx.send(telemetry(
        "PRESSURE_TRANSDUCER_CALIBRATED",
        "VB",
        5,
        vec![Some(300.0)],
    ));
    let _ = state
        .ws_tx
        .send(telemetry("KG50", "DAQ", 6, vec![Some(0.1)]));
    let mut seen_rows = false;
    let mut seen_status = false;
    while !(seen_rows && seen_status) {
        let message = next().await;
        match message["ty"].as_str().unwrap() {
            "rows" => {
                let rows = message["rows"].as_array().unwrap();
                assert!(rows.iter().all(|r| r["data_type"] != "KG50"));
                assert_eq!(rows[0]["data_type"], "PRESSURE_TRANSDUCER_CALIBRATED");
                assert_eq!(rows[0]["seq"], 1);
                seen_rows = true;
            }
            "status" => {
                assert!(message.get("dropped_backpressure").is_some());
                seen_status = true;
            }
            other => panic!("unexpected {other}"),
        }
    }
    let id = grant["id"].as_str().unwrap();
    assert!(
        state
            .auth
            .revoke_stream_token(&state.auth_db, id)
            .await
            .unwrap()
            == 1
    );
    loop {
        let message = next().await;
        if message["ty"] == "bye" {
            assert_eq!(message["reason"], "revoked");
            break;
        }
        assert_ne!(message["ty"], "closed", "closed without bye");
    }
    assert_eq!(next().await["ty"], "closed");
    server.abort();
}

// --- filters ----------------------------------------------------------------

#[test]
fn filters_match_exact_prefix_and_sender_and_intersect_the_scope() {
    let scope = StreamScope {
        label: "x".into(),
        types: vec!["LOADCELL_*".into(), "FUEL_TANK_PRESSURE".into()],
        senders: vec!["DAQ".into(), "VB".into()],
        max_hz: Some(10.0),
    };
    let filter = Filter::from_query(
        &query(&[("types", "loadcell_50*,KG1000"), ("max_hz", "200")]),
        Some(&scope),
    )
    .unwrap();
    assert_eq!(filter.types, vec!["LOADCELL_50*", "KG1000"]);
    assert_eq!(filter.max_hz, 10.0, "a request cannot exceed the token cap");
    assert!(filter.admits("LOADCELL_50_WEIGHT_KG", "DAQ"));
    assert!(
        !filter.admits("LOADCELL_WEIGHT_KG", "DAQ"),
        "outside the request"
    );
    assert!(!filter.admits("KG1000", "DAQ"), "outside the token scope");
    assert!(
        !filter.admits("LOADCELL_50_WEIGHT_KG", "FC"),
        "sender outside the scope"
    );
    let open = Filter::from_query(&query(&[("senders", "vb")]), None).unwrap();
    assert!(open.admits("FUEL_TANK_PRESSURE", "VB"));
    assert!(!open.admits("FUEL_TANK_PRESSURE", "AB"));
    assert_eq!(open.max_hz, DEFAULT_MAX_HZ);
    assert_eq!(open.max_rows_per_s, DEFAULT_MAX_ROWS_PER_S);
    // Privacy denylist applies to every filter.
    assert!(!Filter::default().admits("GS_PROXIMITY_LOCK", "GS"));
    assert!(!Filter::default().admits("USER_GPS", "GS"));
    for bad in [
        &[("max_hz", "0")][..],
        &[("max_hz", "abc")],
        &[("max_rows_per_s", "0")],
        &[("max_rows_per_s", "20001")],
        &[("types", "A B")],
        &[("senders", "VB;rm")],
    ] {
        assert!(Filter::from_query(&query(bad), None).is_err(), "{bad:?}");
    }
}

// --- rate caps and backpressure ---------------------------------------------

#[test]
fn per_stream_cap_holds_the_newest_row_until_its_window_opens() {
    let feed = Feed::new(
        Filter {
            max_hz: 10.0,
            ..Filter::default()
        },
        64,
    );
    let t0 = Instant::now();
    feed.offer(out("KG1000", "DAQ", 1), t0);
    feed.offer(out("KG1000", "DAQ", 2), t0 + Duration::from_millis(10));
    feed.offer(out("KG1000", "DAQ", 3), t0 + Duration::from_millis(20));
    // Another stream has its own window.
    feed.offer(out("KG50", "DAQ", 4), t0 + Duration::from_millis(20));
    let rows = feed.take();
    assert_eq!(
        rows.iter().map(|r| r.received_ms).collect::<Vec<_>>(),
        vec![1, 4]
    );
    feed.release(t0 + Duration::from_millis(60));
    assert!(feed.take().is_empty(), "window not open yet");
    feed.release(t0 + Duration::from_millis(100));
    let rows = feed.take();
    assert_eq!(
        rows.iter().map(|r| r.received_ms).collect::<Vec<_>>(),
        vec![3],
        "newest held row"
    );
    assert_eq!(rows[0].seq, 3);
    let c = feed.counters();
    assert_eq!(
        (c.sent, c.dropped_rate_cap, c.dropped_backpressure),
        (3, 1, 0)
    );
}

#[test]
fn total_cap_is_a_one_second_token_bucket() {
    let feed = Feed::new(
        Filter {
            max_rows_per_s: 3,
            max_hz: 1000.0,
            ..Filter::default()
        },
        64,
    );
    let t0 = Instant::now();
    for i in 0..5 {
        feed.offer(out(&format!("T{i}"), "DAQ", i), t0);
    }
    assert_eq!(feed.take().len(), 3);
    assert_eq!(feed.counters().dropped_rate_cap, 2);
    // One second refills three rows.
    for i in 5..10 {
        feed.offer(out(&format!("T{i}"), "DAQ", i), t0 + Duration::from_secs(1));
    }
    assert_eq!(feed.take().len(), 3);
    assert_eq!(feed.counters().dropped_rate_cap, 4);
}

#[test]
fn full_queue_drops_the_oldest_rows_and_leaves_a_seq_gap() {
    let feed = Feed::new(
        Filter {
            max_hz: 1000.0,
            ..Filter::default()
        },
        3,
    );
    let t0 = Instant::now();
    for i in 0..5 {
        feed.offer(out(&format!("T{i}"), "DAQ", i), t0);
    }
    assert_eq!(feed.queued(), 3);
    let rows = feed.take();
    assert_eq!(
        rows.iter().map(|r| r.seq).collect::<Vec<_>>(),
        vec![3, 4, 5]
    );
    assert_eq!(
        rows.iter().map(|r| r.received_ms).collect::<Vec<_>>(),
        vec![2, 3, 4]
    );
    let c = feed.counters();
    assert_eq!((c.sent, c.dropped_backpressure), (3, 2));
    feed.note_lagged(7);
    assert_eq!(feed.counters().lagged, 7);
}

// --- catalog --------------------------------------------------------------

#[test]
fn catalog_tracks_rows_rates_latest_values_and_scope() {
    let catalog = Catalog::new();
    let t0 = catalog.epoch;
    for i in 0..50 {
        catalog.observe(
            &out("KG1000", "DAQ", i),
            t0 + Duration::from_millis(i as u64 * 100),
        );
    }
    catalog.observe(
        &out("FUEL_TANK_PRESSURE", "VB", 99),
        t0 + Duration::from_millis(4_900),
    );
    let all = catalog.snapshot(&Filter::default(), t0 + Duration::from_millis(5_000));
    assert_eq!(all.len(), 2);
    let kg = all.iter().find(|e| e.data_type == "KG1000").unwrap();
    assert_eq!(kg.rows, 50);
    assert_eq!(kg.rate_hz, 10.0);
    assert_eq!(kg.values, vec![Some(49.0)]);
    assert_eq!(kg.units, &["V"]);
    let scoped = Filter {
        scope_senders: vec!["VB".into()],
        ..Filter::default()
    };
    let only = catalog.snapshot(&scoped, t0 + Duration::from_millis(5_000));
    assert_eq!(only.len(), 1);
    assert_eq!(only[0].data_type, "FUEL_TANK_PRESSURE");
    // Quiet for five seconds: rate falls to zero, history stays.
    let later = catalog.snapshot(&Filter::default(), t0 + Duration::from_secs(12));
    assert!(later.iter().all(|e| e.rate_hz == 0.0));
    assert_eq!(
        later.iter().find(|e| e.data_type == "KG1000").unwrap().rows,
        50
    );
}

// --- schema snapshots -------------------------------------------------------

#[tokio::test]
async fn row_and_hello_schema_snapshot() {
    let state = crate::state::tests::test_app_state().await;
    let row = telemetry(
        "DAQ_LOADCELL_CHAMBER",
        "DAQ",
        1_767_225_600_500,
        vec![Some(0.25), Some(312.5), None],
    );
    crate::telemetry_provenance::note(&row, Some(1_767_225_600_480));
    let mut out = row_out(&state, &row);
    out.seq = 42;
    let provenance = provenance(&state, "DAQ_LOADCELL_CHAMBER", "DAQ");
    let expected = format!(
        r#"{{"ty":"row","seq":42,"data_type":"DAQ_LOADCELL_CHAMBER","sender":"DAQ","received_ms":1767225600500,"source_ms":1767225600480,"network_ms":null,"values":[0.25,312.5,null],"fields":["kg1000_pin","chamber_pressure","chamber_connector"],"units":["V","psig","V"],"provenance":"{provenance}"}}"#
    );
    assert_eq!(serde_json::to_string(&out).unwrap(), expected);
    assert!(["live", "sim"].contains(&provenance));
    assert_eq!(
        super::provenance(&state, "SIM_IMU_DATA", "FC") == "replay",
        provenance == "live"
    );

    let scope = StreamScope {
        label: "OBS".into(),
        types: vec!["KG*".into()],
        senders: vec![],
        max_hz: Some(20.0),
    };
    let viewer = Viewer::Stream {
        grant: StreamGrant {
            id: "0123456789abcdef".into(),
            issuer: "administrator".into(),
            scope: scope.clone(),
            expires_at_ms: 99,
        },
        token: "gss1.secret".into(),
    };
    let filter = Filter::from_query(&query(&[("senders", "DAQ")]), Some(&scope)).unwrap();
    let mut hello = hello(&state, &viewer, &filter);
    assert!(hello["server_time_ms"].as_i64().unwrap() > 0);
    hello.as_object_mut().unwrap().remove("server_time_ms");
    let station = hello["provenance"]["station"].as_str().unwrap().to_string();
    let mode = hello["provenance"]["mode"].as_str().unwrap().to_string();
    let replay = hello["provenance"]["igt_replay"].as_bool().unwrap();
    assert_eq!(
        hello,
        serde_json::json!({
            "ty": "hello",
            "schema": "gs26.raw-stream.v1",
            "viewer": {"kind": "stream_token", "id": "0123456789abcdef", "label": "OBS", "expires_at_ms": 99},
            "filters": {"types": [], "senders": ["DAQ"], "scope_types": ["KG*"], "scope_senders": []},
            "limits": {"max_hz": 20.0, "max_rows_per_s": 5000, "queue_rows": 4096, "flush_ms": 50, "status_ms": 1000},
            "provenance": {"station": station, "igt_replay": replay, "mode": mode},
        })
    );
    assert!(!hello.to_string().contains("secret") && !hello.to_string().contains("administrator"));
}

#[test]
fn every_documented_stream_has_fields_and_units_of_equal_length() {
    for data_type in [
        "KG1000",
        "KG50",
        "DAQ_KG50_SELECTED",
        "DAQ_LOADCELL_CHAMBER",
        "LOADCELL_WEIGHT_KG",
        "LOADCELL_50_WEIGHT_KG",
        "LOADCELL_FILL_PERCENT",
        "COMBUSTION_CHAMBER_PRESSURE",
        "COMBUSTION_CHAMBER_VOLTAGE",
        "COMBUSTION_CHAMBER_RANGE_STATUS",
        "COMBUSTION_CHAMBER_PT_INSTALLED",
        "DAQ_ADC_TEMPERATURE",
        "DAQ_SDADC_VOLTAGES",
        "DAQ_SAR_VOLTAGES",
        "DAQ_CURRENT_SENSE",
        "DAQ_POWER_MONITOR",
        "FUEL_TANK_PRESSURE",
        "PRESSURE_TRANSDUCER_CALIBRATED",
        "VALVE_STATE",
        "IGNITER_SENSE_SIGNAL",
        "BATTERY_VOLTAGE",
        "BATTERY_CURRENT",
        "ACCEL_DATA",
        "GYRO_DATA",
        "BAROMETER_DATA",
        "ASCENT_STATE",
        "DESCENT_STATE",
        "GPS_DATA",
        "GPS_SATELLITE_NUMBER",
        "GPS_STATUS",
        "VEHICLE_SPEED",
        "FLIGHT_STATE",
        "SIM_IMU_DATA",
        "SIM_BAROMETER_DATA",
        "SIM_GPS_DATA",
        "SIM_ASCENT_STATE",
        "SIM_DESCENT_STATE",
        "SIM_SAMPLE",
        "FILL_BOX_POWER_PERCENT",
        "AV_BAY_BATTERY_PERCENT",
        "FILL_BOX_POWER_DROP_RATE_V_PER_MIN",
        "FILL_BOX_POWER_REMAINING_MINUTES",
    ] {
        let (fields, units) = schema(data_type);
        assert!(!fields.is_empty(), "{data_type}");
        assert_eq!(fields.len(), units.len(), "{data_type}");
    }
    assert_eq!(schema("SOMETHING_NEW"), (NONE, NONE));
}

#[test]
fn query_tokens_are_redacted_from_logged_urls_and_decoded_by_the_layer() {
    let logged = crate::log_redaction::redact_urls(
        "GET https://gs.local/api/stream/raw/ws?token=gss1.abc&types=KG50",
    );
    assert!(!logged.contains("gss1.abc"));
    assert_eq!(
        query_param(Some("a=1&token=gss1.ab%2Ecd%2b"), "token").as_deref(),
        Some("gss1.ab.cd+")
    );
    assert_eq!(percent_decode("%zz%4"), "%zz%4");
}
