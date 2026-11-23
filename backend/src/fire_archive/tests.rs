use super::*;

fn temp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "seds-fire-archive-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn seeds_load_once_and_only_the_accepted_fire_enters_the_fit() {
    let data = temp("seed");
    assert_eq!(ensure_seeded(&data).unwrap(), 2);
    assert_eq!(ensure_seeded(&data).unwrap(), 0, "idempotent");
    let fires = load_all(&data);
    assert_eq!(fires.len(), 2);
    let nov = fires
        .iter()
        .find(|f| f.id == "2026-11-16_static_fire")
        .unwrap();
    assert!(nov.in_fit && nov.review.status == "accepted");
    assert_eq!(
        (nov.fill_kg, nov.impulse_ns, nov.tank_psi_at_ignition),
        (Some(5.65), Some(9192.0), Some(404.1))
    );
    let oct = fires
        .iter()
        .find(|f| f.id == "2026-10-31_static_fire")
        .unwrap();
    assert!(!oct.in_fit && oct.review.exclude_from_fit);
    let fit = fit_inputs_in(&data);
    assert_eq!(fit.len(), 1);
    assert_eq!(fit[0].id, "2026-11-16_static_fire");
    assert_eq!(
        (fit[0].fill_kg, fit[0].impulse_ns, fit[0].impulse_sigma_ns),
        (5.65, 9192.0, 900.0)
    );
    let points = curve(&data, "2026-11-16_static_fire").unwrap();
    let impulse: f64 = points
        .windows(2)
        .map(|w| (w[1][0] - w[0][0]) * 0.5 * (w[0][1] + w[1][1]))
        .sum();
    assert!((impulse - 9192.0).abs() < 20.0, "{impulse}");
    assert!(curve(&data, "2026-10-31_static_fire").is_none());
    // A reviewed seed is not overwritten by a later start.
    apply_patch(
        &data,
        "2026-11-16_static_fire",
        Patch {
            exclude_from_fit: Some(true),
            ..Default::default()
        },
        "op",
        1,
    )
    .unwrap();
    ensure_seeded(&data).unwrap();
    assert!(
        get(&data, "2026-11-16_static_fire")
            .unwrap()
            .review
            .exclude_from_fit
    );
    assert!(fit_inputs_in(&data).is_empty());
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn review_validates_logs_and_controls_the_fit() {
    let data = temp("review");
    ensure_seeded(&data).unwrap();
    let id = "2026-10-31_static_fire";
    let bad = apply_patch(
        &data,
        id,
        Patch {
            status: Some("maybe".into()),
            ..Default::default()
        },
        "op",
        1,
    );
    assert_eq!(bad.unwrap_err().0, StatusCode::BAD_REQUEST);
    let nan = apply_patch(
        &data,
        id,
        Patch {
            fill_kg: Some(f64::NAN),
            ..Default::default()
        },
        "op",
        1,
    );
    assert_eq!(nan.unwrap_err().0, StatusCode::BAD_REQUEST);
    assert_eq!(
        apply_patch(&data, "nope", Patch::default(), "op", 1)
            .unwrap_err()
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        apply_patch(&data, "../x", Patch::default(), "op", 1)
            .unwrap_err()
            .0,
        StatusCode::NOT_FOUND
    );
    // Accepting without an impulse still keeps it out of the fit.
    let (rec, text) = apply_patch(
        &data,
        id,
        Patch {
            status: Some("accepted".into()),
            exclude_from_fit: Some(false),
            notes: Some("checked".into()),
            ambient_c: Some(12.0),
            ..Default::default()
        },
        "alice",
        42,
    )
    .unwrap();
    assert!(text.contains("alice") && text.contains("accepted"));
    assert_eq!(
        (
            rec.review.reviewed_by.as_deref(),
            rec.review.reviewed_ms,
            rec.ambient_c
        ),
        (Some("alice"), Some(42), Some(12.0))
    );
    assert!(!rec.in_fit, "no impulse");
    // Persisted.
    assert_eq!(get(&data, id).unwrap().review.notes, "checked");
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn import_validates_and_refuses_duplicates() {
    let data = temp("import");
    let body = |id: Option<&str>| Import {
        label: "Cold-day fire".into(),
        date: "2026-12-05".into(),
        id: id.map(str::to_owned),
        fill_kg: 6.2,
        impulse_ns: 10_100.0,
        impulse_sigma_ns: Some(600.0),
        fill_before_kg: None,
        fill_after_kg: None,
        tank_psi_at_ignition: Some(520.0),
        tank_temp_c: None,
        supercharge_psi: Some(600.0),
        ambient_c: Some(3.0),
        weather: None,
        burn_time_s: Some(5.0),
        peak_thrust_n: None,
        chamber_valid: false,
        result: None,
        notes: String::new(),
        curve: vec![[0.0, 0.0], [1.0, 2000.0], [5.0, 0.0]],
    };
    let rec = import(&data, body(None), "admin", 5).unwrap();
    assert_eq!(rec.source, "import");
    assert_eq!(rec.review.status, "pending");
    assert!(!get(&data, &rec.id).unwrap().in_fit);
    assert_eq!(curve(&data, &rec.id).unwrap().len(), 3);
    assert_eq!(
        import(&data, body(Some(&rec.id)), "admin", 5)
            .unwrap_err()
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        import(&data, body(Some("bad/id")), "admin", 5)
            .unwrap_err()
            .0,
        StatusCode::BAD_REQUEST
    );
    let mut wrong = body(Some("x1"));
    wrong.date = "05/12/2026".into();
    assert_eq!(
        import(&data, wrong, "admin", 5).unwrap_err().0,
        StatusCode::BAD_REQUEST
    );
    let mut zero = body(Some("x2"));
    zero.impulse_ns = 0.0;
    assert_eq!(
        import(&data, zero, "admin", 5).unwrap_err().0,
        StatusCode::BAD_REQUEST
    );
    apply_patch(
        &data,
        &rec.id,
        Patch {
            status: Some("accepted".into()),
            ..Default::default()
        },
        "op",
        6,
    )
    .unwrap();
    let fit = fit_inputs_in(&data);
    assert_eq!(fit.len(), 1);
    assert_eq!(
        (fit[0].fill_kg, fit[0].supercharge_psi, fit[0].ambient_c),
        (6.2, Some(600.0), Some(3.0))
    );
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn records_round_trip_on_disk_and_serve_in_fit() {
    let data = temp("roundtrip");
    let record = FireRecord {
        id: "2027-02-01_120000_launch".into(),
        label: "x".into(),
        date: "2027-02-01".into(),
        source: "burn_report".into(),
        fill_kg: Some(6.0),
        impulse_ns: Some(9000.0),
        simulated: true,
        review: Review {
            status: "accepted".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    save_record(&data, &record, None).unwrap();
    let back = get(&data, &record.id).unwrap();
    assert_eq!(back.label, "x");
    assert!(!back.in_fit, "simulated fires never enter the fit");
    let json = serde_json::to_value(&back).unwrap();
    for key in [
        "id",
        "label",
        "date",
        "source",
        "burn_report_id",
        "fill_kg",
        "fill_before_kg",
        "fill_after_kg",
        "tank_psi_at_ignition",
        "tank_temp_c",
        "supercharge_psi",
        "ambient_c",
        "weather",
        "impulse_ns",
        "impulse_sigma_ns",
        "burn_time_s",
        "peak_thrust_n",
        "chamber_valid",
        "result",
        "applied_plan",
        "quality_flags",
        "simulated",
        "review",
        "in_fit",
    ] {
        assert!(json.get(key).is_some(), "{key}");
    }
    let _ = std::fs::remove_dir_all(&data);
}

pub(crate) async fn http_state() -> Arc<AppState> {
    let mut state = crate::state::tests::test_app_state().await;
    let users = std::path::Path::new(&state.placeholder_db_path)
        .parent()
        .unwrap()
        .join("users.json");
    let user = |name: &str, send: bool| {
        serde_json::json!({"username": name, "password": {"salt_b64": "", "hash_b64": ""}, "roles": [],
            "permissions": {"view_data": true, "send_commands": send}, "command_access": {"allowed_commands": []}})
    };
    std::fs::write(
        &users,
        serde_json::to_vec(
            &serde_json::json!({"version": 1, "users": [user("op", true), user("viewer", false)]}),
        )
        .unwrap(),
    )
    .unwrap();
    Arc::get_mut(&mut state).unwrap().auth = Arc::new(crate::auth::AuthManager::new(users));
    crate::ensure_auth_sessions_table(&state.auth_db)
        .await
        .unwrap();
    for (token, name, send) in [("op-token", "op", 1), ("viewer-token", "viewer", 0)] {
        sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES(?,?,'session',1,?,0,9999999999999)")
            .bind(token)
            .bind(name)
            .bind(send)
            .execute(&state.auth_db)
            .await
            .unwrap();
    }
    state
}

pub(crate) async fn call(
    state: &Arc<AppState>,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: &str,
) -> (StatusCode, serde_json::Value) {
    use tower::ServiceExt;
    let app = crate::web::router(state.clone(), "unused".into());
    let mut request = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .oneshot(
            request
                .body(axum::body::Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 22)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn routes_serve_review_and_guard_the_archive() {
    let state = http_state().await;
    ensure_seeded(&data_dir(&state)).unwrap();
    let (status, _) = call(&state, "GET", "/api/fire_archive", None, "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body) = call(&state, "GET", "/api/fire_archive", Some("viewer-token"), "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["fires"].as_array().unwrap().len(), 2);
    let (status, body) = call(
        &state,
        "GET",
        "/api/fire_archive/2026-11-16_static_fire/thrust",
        Some("viewer-token"),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["curve"].as_array().unwrap().len() > 100);
    let patch = r#"{"status":"rejected","notes":"KG50 slope unverified"}"#;
    let (status, _) = call(
        &state,
        "PATCH",
        "/api/fire_archive/2026-11-16_static_fire",
        Some("viewer-token"),
        patch,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = call(
        &state,
        "PATCH",
        "/api/fire_archive/2026-11-16_static_fire",
        Some("op-token"),
        patch,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        (body["review"]["status"].as_str(), body["in_fit"].as_bool()),
        (Some("rejected"), Some(false))
    );
    assert_eq!(body["review"]["reviewed_by"], "op");
    let (status, body) = call(
        &state,
        "PATCH",
        "/api/fire_archive/2026-11-16_static_fire",
        Some("op-token"),
        r#"{"bogus":1}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string());
    let (status, _) = call(
        &state,
        "PATCH",
        "/api/fire_archive/nope",
        Some("op-token"),
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let import = r#"{"label":"x","date":"2026-12-01","fill_kg":5,"impulse_ns":8000}"#;
    let (status, _) = call(
        &state,
        "POST",
        "/api/fire_archive/import",
        Some("op-token"),
        import,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "admin only");
    assert!(fit_inputs(&state).is_empty());
    let (status, body) = call(&state, "GET", "/api/burn_reports", Some("viewer-token"), "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["reports"].is_array());
    let (status, _) = call(
        &state,
        "GET",
        "/api/burn_reports/launch.missing",
        Some("viewer-token"),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
