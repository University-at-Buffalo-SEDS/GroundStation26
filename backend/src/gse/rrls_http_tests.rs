//! Actual authenticated HTTP handlers must share RRLS's live-control boundary.
use super::*;

#[tokio::test]
async fn rrls_http_cannot_clear_abort_or_mutate_operational_qualification() {
    const CHILD: &str = "GS_RRLS_HTTP_OPERATIONAL_BOUNDARY_CHILD";
    const NAME: &str =
        "gse::rrls_http_tests::rrls_http_cannot_clear_abort_or_mutate_operational_qualification";
    if std::env::var_os(CHILD).is_none() {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .env("GS_RRLS_ROLE", "rrls")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    let root = crate::recording_export::tests::TestDirectory::new();
    // This fresh process owns all static stores; no existing account or mission
    // file is read or modified by the diagnostic fixture.
    unsafe {
        std::env::set_var("GS_NETWORK_VARIABLE_CACHE", root.0.join("variables.json"));
        std::env::set_var("GS_CHECKLISTS_FILE", root.0.join("checklists.json"));
    }
    let state = crate::recording_export::tests::export_state(&root.0, false).await;
    std::fs::write(
        root.0.join("auth.json"),
        serde_json::json!({"users":[{
            "username":"fixture-admin", "roles":["admin"],
            "password":{"salt_b64":"","hash_b64":""},
            "permissions":{"view_data":true,"send_commands":true}
        }]})
        .to_string(),
    )
    .unwrap();
    sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES('fixture-token','fixture-admin','session',1,1,0,9999999999999)")
        .execute(&state.auth_db).await.unwrap();
    let packets = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = packets.clone();
    let router = Arc::new(sedsnet::router::Router::new(
        sedsnet::router::RouterConfig::new([sedsnet::router::EndpointHandler::new_packet_handler(
            crate::telemetry_schema::endpoint("FLIGHT_STATE"),
            move |packet| {
                seen.lock().unwrap().push(packet.payload().to_vec());
                Ok(())
            },
        )])
        .with_sender("RRLS"),
    ));
    state.topology_router.set(router.clone()).unwrap();
    // Seed a received state without enqueueing a fixture-originated update.
    *state.state.lock().unwrap() = FlightState::Aborted;
    #[cfg(not(feature = "hitl_mode"))]
    let saved_before = {
        let mut runtime = state.gse.lock().unwrap();
        runtime.operational.path = Some(root.0.join("qualification.json"));
        runtime
            .operational
            .saved
            .save(runtime.operational.path.as_ref().unwrap())
            .unwrap();
        std::fs::read(root.0.join("qualification.json")).unwrap()
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        "Bearer fixture-token".parse().unwrap(),
    );
    let principal = crate::web::authorize_headers(&state, &headers, Permission::SendCommands)
        .await
        .unwrap();
    assert!(principal.allows_command_name("ValveSelfTest"));
    assert!(principal.allows_command_name("NitrogenTest"));
    assert!(crate::rrls::is_rrls());
    assert!(!state.is_command_allowed(&TelemetryCommand::Abort));
    assert_eq!(
        clear_abort(State(state.clone()), headers.clone())
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        self_test_confirmation(
            State(state.clone()),
            headers.clone(),
            Json(SelfTestConfirmation { confirmed: true })
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        reset_qualification(State(state.clone()), headers.clone())
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        override_cold_flow(
            State(state.clone()),
            headers,
            Json(ColdFlowOverrideRequest {
                reason: "fixture operator request".into()
            })
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    router.process_all_queues_with_timeout(0).unwrap();
    assert!(
        packets.lock().unwrap().is_empty(),
        "RRLS must publish no managed Idle"
    );
    assert_eq!(state.local_flight_state_snapshot(), FlightState::Aborted);
    assert!(
        !state
            .gse
            .lock()
            .unwrap()
            .engine
            .config
            .dry_self_test_confirmed
    );
    #[cfg(not(feature = "hitl_mode"))]
    assert_eq!(
        std::fs::read(root.0.join("qualification.json")).unwrap(),
        saved_before
    );
    assert!(
        !root.0.join("variables.json").exists(),
        "No managed-variable cache mutation"
    );
    assert!(
        !root.0.join("checklists.json").exists(),
        "No checklist completion reset"
    );
}
