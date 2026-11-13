use super::*;
use crate::station_mode::Mode;

const MODES: [Mode; 4] = [Mode::Normal, Mode::Hitl, Mode::TestFire, Mode::Testing];

fn grouped(state: &AppState, on: bool) {
    state.gse.lock().unwrap().engine.config.grouped_panel = on;
}

#[tokio::test]
async fn without_a_choice_every_mode_keeps_the_gse_setting() {
    for mode in MODES {
        let _mode = crate::runtime_mode::test_support::thread_mode(mode);
        let state = crate::state::tests::test_app_state().await;
        restore(&state);
        assert_eq!(choice(&state), None, "{mode:?}");
        for on in [true, false] {
            grouped(&state, on);
            for button in &DUAL_BUTTONS {
                let expected = if on { button.sequence } else { button.valve };
                assert_eq!(crate::gse::panel_name(&state, button.valve), expected, "{mode:?}");
            }
            for fixed in ["Launch", "Abort", "Igniter", "RetractPlumbing"] {
                assert_eq!(crate::gse::panel_name(&state, fixed), fixed, "{mode:?}");
            }
            let status = status(&state);
            assert_eq!(status.effective_mode, PanelMode::from_grouped(on));
            assert_eq!(status.station_default, PanelMode::from_grouped(on));
            assert_eq!(status.panel_mode, None);
        }
    }
}

#[tokio::test]
async fn the_choice_overrides_the_gse_setting_and_survives_a_restart() {
    let first = crate::state::tests::test_app_state().await;
    *first.state.lock().unwrap() = FlightState::Idle;
    grouped(&first, true);
    set(&first, Some(PanelMode::Valves), Some("admin".into())).unwrap();
    assert_eq!(crate::gse::panel_name(&first, "Pilot"), "Pilot");
    let reopen = |placeholder: String| async move {
        let mut state = crate::state::tests::test_app_state().await;
        Arc::get_mut(&mut state).unwrap().placeholder_db_path = placeholder;
        restore(&state);
        state
    };
    let second = reopen(first.placeholder_db_path.clone()).await;
    grouped(&second, true);
    assert_eq!(choice(&second), Some(PanelMode::Valves));
    assert_eq!(crate::gse::panel_name(&second, "Nitrous"), "Nitrous");
    assert_eq!(status(&second).updated_by.as_deref(), Some("admin"));

    *second.state.lock().unwrap() = FlightState::Idle;
    grouped(&second, false);
    set(&second, Some(PanelMode::Sequences), None).unwrap();
    let third = reopen(first.placeholder_db_path.clone()).await;
    grouped(&third, false);
    assert_eq!(crate::gse::panel_name(&third, "Dump"), "CancelFill");

    // Back to the station default.
    *third.state.lock().unwrap() = FlightState::Idle;
    set(&third, None, None).unwrap();
    let fourth = reopen(first.placeholder_db_path.clone()).await;
    grouped(&fourth, false);
    assert_eq!(choice(&fourth), None);
    assert_eq!(crate::gse::panel_name(&fourth, "Dump"), "Dump");
}

#[tokio::test]
async fn switching_is_refused_while_armed_flying_or_a_sequence_runs() {
    let state = crate::state::tests::test_app_state().await;
    for flight in [FlightState::Armed, FlightState::Launch, FlightState::InFlight, FlightState::Descent] {
        *state.state.lock().unwrap() = flight;
        let (code, reason) = set(&state, Some(PanelMode::Valves), None).unwrap_err();
        assert_eq!(code, StatusCode::CONFLICT);
        assert!(reason.contains(&format!("{flight:?}")), "{reason}");
        assert_eq!(status(&state).change_blocked_reason, Some(reason));
        assert_eq!(choice(&state), None);
        assert!(!path(&state).exists(), "a refused change is not saved");
    }
    *state.state.lock().unwrap() = FlightState::Idle;
    state.gse.lock().unwrap().engine.status.phase = gse_sequence::Phase::Raising;
    let (code, reason) = set(&state, Some(PanelMode::Valves), None).unwrap_err();
    assert_eq!(code, StatusCode::CONFLICT);
    assert!(reason.contains("running or paused"), "{reason}");
    assert_eq!(choice(&state), None);
    state.gse.lock().unwrap().engine.status.phase = gse_sequence::Phase::Idle;
    assert_eq!(status(&state).change_blocked_reason, None);
    set(&state, Some(PanelMode::Valves), None).unwrap();
    assert_eq!(choice(&state), Some(PanelMode::Valves));
}

#[tokio::test]
async fn launch_only_note_and_reasons_follow_the_mode() {
    for mode in MODES {
        let _mode = crate::runtime_mode::test_support::thread_mode(mode);
        let state = crate::state::tests::test_app_state().await;
        *state.state.lock().unwrap() = FlightState::Idle;
        grouped(&state, true);
        assert_eq!(status(&state).launch_only_note, None);
        crate::sequence_access::set(&state, true, None).unwrap();
        let status_now = status(&state);
        let note = status_now.launch_only_note.clone().expect("note when both are on");
        let refused: Vec<_> = status_now
            .buttons
            .iter()
            .filter(|b| b.disabled_reason.is_some())
            .map(|b| b.physical.as_str())
            .collect();
        if mode == Mode::Hitl {
            assert_eq!(refused, ["Pilot", "Nitrogen", "Nitrous"]);
            assert!(note.contains("do not fall back"), "{note}");
            assert!(note.contains(crate::sequence_access::DISABLED_REASON));
        } else {
            assert!(refused.is_empty(), "{mode:?}");
            assert!(note.contains("unaffected"), "{note}");
        }
        // Valve control is never touched by launch-only.
        set(&state, Some(PanelMode::Valves), None).unwrap();
        let status_now = status(&state);
        assert_eq!(status_now.launch_only_note, None);
        assert!(status_now.buttons.iter().all(|b| b.disabled_reason.is_none()));
    }
}

#[tokio::test]
async fn press_test_reports_levels_or_no_gpio() {
    let gpio = crate::gpio::GpioPins::new_isolated();
    let none = press_test(&gpio, false);
    assert!(!none.gpio_available);
    assert_eq!(none.message.as_deref(), Some("No GPIO on this host."));
    for button in &DUAL_BUTTONS {
        gpio.setup_input_pin(button.pin).unwrap();
    }
    gpio.set_input_for_test(crate::gpio_panel::PILOT_VALVE_PIN, true);
    let test = press_test(&gpio, true);
    assert!(test.gpio_available);
    let pressed: Vec<_> = test
        .inputs
        .iter()
        .filter(|i| i.pressed == Some(true))
        .map(|i| i.physical.as_str())
        .collect();
    assert_eq!(pressed, ["Pilot"]);
    assert_eq!(test.inputs.len(), DUAL_BUTTONS.len() + FIXED_INPUTS.len());
    // Unconfigured lines read as unknown, not released.
    assert!(test.inputs.iter().any(|i| i.pressed.is_none()));
}

#[tokio::test]
async fn admin_routes_require_an_administrator() {
    let root = crate::recording_export::tests::TestDirectory::new();
    let state = crate::recording_export::tests::export_state(&root.0, true).await;
    *state.state.lock().unwrap() = FlightState::Idle;
    std::fs::write(root.0.join("auth.json"), serde_json::json!({"users":[
        {"username":"administrator","roles":["admin"],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true}},
        {"username":"operator","roles":[],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true,"send_commands":true}}
    ]}).to_string()).unwrap();
    for (token, name) in [("admin-test", "administrator"), ("operator-test", "operator")] {
        sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES(?,?,'session',1,1,0,9999999999999)")
            .bind(token).bind(name).execute(&state.auth_db).await.unwrap();
    }
    let app = routes().with_state(state.clone());
    let request = |uri: &str, token: &str, body: Option<&str>| {
        let builder = axum::http::Request::builder()
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json");
        match body {
            Some(body) => builder.method("POST").body(axum::body::Body::from(body.to_string())),
            None => builder.method("GET").body(axum::body::Body::empty()),
        }
        .unwrap()
    };
    use tower::ServiceExt;
    for uri in ["/api/admin/physical-panel", "/api/admin/physical-panel/press-test"] {
        let refused = app.clone().oneshot(request(uri, "operator-test", None)).await.unwrap();
        assert_eq!(refused.status(), StatusCode::FORBIDDEN, "{uri}");
    }
    let refused = app
        .clone()
        .oneshot(request("/api/admin/physical-panel", "operator-test", Some(r#"{"panel_mode":"valves"}"#)))
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(choice(&state), None);
    let accepted = app
        .clone()
        .oneshot(request("/api/admin/physical-panel", "admin-test", Some(r#"{"panel_mode":"valves"}"#)))
        .await
        .unwrap();
    assert_eq!(accepted.status(), StatusCode::OK);
    assert_eq!(choice(&state), Some(PanelMode::Valves));
    let body = axum::body::to_bytes(
        app.oneshot(request("/api/admin/physical-panel/press-test", "admin-test", None))
            .await
            .unwrap()
            .into_body(),
        65536,
    )
    .await
    .unwrap();
    let test: PressTest = serde_json::from_slice(&body).unwrap();
    assert!(!test.gpio_available, "this build has no GPIO");
    assert_eq!(test.message.as_deref(), Some("No GPIO on this host."));
}
