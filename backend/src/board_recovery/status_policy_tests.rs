//! Actual authenticated GET contract. Capability data never grants recovery.
use super::*;
use std::sync::atomic::Ordering;

async fn fixture(sender: Option<&str>) -> tests::HttpFixture {
    let fixture = tests::http_fixture().await;
    *fixture.state.launch_history.lock().unwrap() = crate::launch_history::Store::open(
        fixture.directory.join("policy-clock.json"),
        false,
        None,
        1_793_000_000_000,
        0,
    )
    .unwrap();
    if let Some(sender) = sender {
        let router = Arc::new(sedsnet::router::Router::new(
            sedsnet::router::RouterConfig::new([]).with_sender(sender),
        ));
        assert!(fixture.state.topology_router.set(router).is_ok());
    }
    fixture
}
async fn status(fixture: &tests::HttpFixture, token: Option<&str>) -> (StatusCode, Value) {
    let response = tests::http(
        fixture.state.clone(),
        "GET",
        "/api/board-recovery/status",
        token,
        json!({}),
    )
    .await;
    let code = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (
        code,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned())),
    )
}
fn exact_policy() -> Value {
    let mode = crate::station_mode::local_mode();
    let mask = scope::base_mask(mode);
    let base = BOARDS
        .iter()
        .enumerate()
        .filter_map(|(i, board)| (mask & (1 << i) != 0).then_some(board.sender_id()))
        .collect::<Vec<_>>();
    json!({"version":1,"global_startup_best_effort":true,"mode":mode,
        "base_required_boards":base,"optional_proofs":true})
}
#[tokio::test]
async fn actual_get_requires_auth_before_exposing_capability() {
    let f = fixture(Some("GS")).await;
    let (code, value) = status(&f, None).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
    assert!(value.get("reset_startup_policy").is_none());
}
#[tokio::test]
async fn actual_get_viewer_sees_policy_without_command_permission() {
    let f = fixture(Some("GS")).await;
    let (code, value) = status(&f, Some("viewer")).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(value["reset_startup_policy"], exact_policy());
    assert_eq!(value["permissions"]["reset_startup"], false);
    assert_eq!(value["permissions"]["recover"], false);
}
#[tokio::test]
async fn actual_get_primary_operator_has_exact_mode_policy_vocabulary() {
    let f = fixture(Some("GS")).await;
    let (code, value) = status(&f, Some("operator")).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(value["reset_startup_policy"], exact_policy());
    assert_eq!(value["reset_startup_policy"].as_object().unwrap().len(), 5);
    assert_eq!(value["permissions"]["reset_startup"], true);
    assert_eq!(value["mode_capability"]["runtime_switch_supported"], false);
    assert_eq!(
        value["reset_startup_policy"]["mode"],
        value["mode_capability"]["current"]
    );
}
#[tokio::test]
async fn actual_get_without_router_has_null_capability_and_no_command_grant() {
    let f = fixture(None).await;
    let (code, value) = status(&f, Some("operator")).await;
    assert_eq!(code, StatusCode::OK);
    assert!(value["reset_startup_policy"].is_null());
    assert_eq!(value["permissions"]["reset_startup"], false);
}
#[tokio::test]
async fn actual_get_foreign_router_identity_has_null_capability() {
    let f = fixture(Some("RRLS")).await;
    let (code, value) = status(&f, Some("operator")).await;
    assert_eq!(code, StatusCode::OK);
    assert!(value["reset_startup_policy"].is_null());
    assert_eq!(value["permissions"]["reset_startup"], false);
}
#[tokio::test]
async fn actual_get_rrls_role_stays_view_only_even_with_gs_router_name() {
    // Role is immutable within each process. Never mutate process environment
    // while unrelated async tests may read it.
    const MARKER: &str = "GS_RESET_POLICY_RRLS_CHILD";
    if std::env::var_os(MARKER).is_none() {
        let case = "board_recovery::status_policy_tests::actual_get_rrls_role_stays_view_only_even_with_gs_router_name";
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", case, "--nocapture", "--test-threads=1"])
            .env(MARKER, "1")
            .env("GS_RRLS_ROLE", "rrls")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&result.stdout);
        assert!(
            result.status.success() && stdout.contains("1 passed; 0 failed; 0 ignored"),
            "actual RRLS subprocess failed: {stdout}\n{}",
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    assert!(crate::rrls::is_rrls());
    let f = fixture(Some("GS")).await;
    let (code, value) = status(&f, Some("operator")).await;
    assert_eq!(code, StatusCode::OK);
    assert!(value["reset_startup_policy"].is_null());
    assert_eq!(value["permissions"]["reset_startup"], false);
}
#[tokio::test]
async fn actual_repeated_get_has_no_query_apply_journal_or_generation_effect() {
    let f = fixture(Some("GS")).await;
    let state = &f.state;
    let generation = state.automatic_generation.load(Ordering::Acquire);
    let clock = state.launch_history.lock().unwrap().snapshot();
    let file = f.directory.join("policy-clock.json");
    let existed = file.exists();
    for _ in 0..3 {
        assert_eq!(status(&f, Some("operator")).await.0, StatusCode::OK);
    }
    assert_eq!(
        state.automatic_generation.load(Ordering::Acquire),
        generation
    );
    assert_eq!(state.launch_history.lock().unwrap().snapshot(), clock);
    assert!(
        state
            .launch_history
            .lock()
            .unwrap()
            .recovery_pending()
            .is_none()
    );
    assert_eq!(file.exists(), existed);
    let inner = state.board_recovery.inner.lock().unwrap();
    assert!(inner.queries.iter().all(Option::is_none));
    assert!(inner.values.iter().all(Option::is_none));
    drop(inner);
    assert!(state.board_recovery.coordinator.operation().is_none());
    assert!(
        !state
            .topology_router
            .get()
            .unwrap()
            .has_pending_reliable_tx_type(crate::telemetry_schema::data_type(QUERY_TYPE))
    );
    assert_eq!(state.operator_admission_barrier.load(Ordering::Acquire), 0);
}
#[tokio::test]
async fn actual_get_active_launch_exposes_data_but_cannot_grant_reset() {
    let f = fixture(Some("GS")).await;
    *f.state.state.lock().unwrap() = crate::types::FlightState::Launch;
    let (code, value) = status(&f, Some("operator")).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(value["reset_startup_policy"], exact_policy());
    assert_eq!(value["permissions"]["reset_startup"], false);
    assert_eq!(
        f.state.local_flight_state_snapshot(),
        crate::types::FlightState::Launch
    );
    assert!(
        f.state
            .launch_history
            .lock()
            .unwrap()
            .recovery_pending()
            .is_none()
    );
}
#[tokio::test]
async fn actual_get_abort_observation_never_turns_capability_into_recovery_proof() {
    let f = fixture(Some("GS")).await;
    f.state
        .board_abort
        .observe_abort(Board::RFBoard, WallInstant::now(), 100);
    let before = f.state.board_abort.masks();
    let (code, value) = status(&f, Some("operator")).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(value["reset_startup_policy"], exact_policy());
    assert_eq!(f.state.board_abort.masks(), before);
    assert!(
        f.state
            .launch_history
            .lock()
            .unwrap()
            .recovery_pending()
            .is_none()
    );
    assert!(
        f.state
            .board_recovery
            .inner
            .lock()
            .unwrap()
            .queries
            .iter()
            .all(Option::is_none)
    );
    let selected = scope::Selection::capture(&f.state, &[]).unwrap();
    assert!(
        selected.boards().contains(&Board::RFBoard),
        "PREPARE still includes actual Abort independent of base capability"
    );
}
#[tokio::test]
async fn actual_get_identity_epoch_and_policy_aba_remain_server_owned() {
    let f = fixture(Some("GS")).await;
    let (_, before) = status(&f, Some("operator")).await;
    let (_, viewer) = status(&f, Some("viewer")).await;
    assert_eq!(before["backend_instance"], viewer["backend_instance"]);
    assert_ne!(
        before["principal"]["context"],
        viewer["principal"]["context"]
    );
    let original = scope::Selection::capture(&f.state, &[]).unwrap();
    #[cfg(feature = "hitl_mode")]
    {
        f.state.toggle_hitl_safety_interlock(3);
        f.state.toggle_hitl_safety_interlock(3);
    }
    #[cfg(not(feature = "hitl_mode"))]
    {
        assert!(f.state.board_abort.change_launch_context_if(|| true));
        assert!(f.state.board_abort.change_launch_context_if(|| true));
    }
    assert!(original.current(&f.state).is_err());
    let old = f.state.automatic_generation.load(Ordering::Acquire);
    let next = crate::gse::reset_generation::invalidate_capture_epoch(&f.state, old).unwrap();
    let (_, after) = status(&f, Some("operator")).await;
    assert_eq!(
        before["reset_startup_policy"], after["reset_startup_policy"],
        "static capability is not an authority revision"
    );
    assert_eq!(before["backend_instance"], after["backend_instance"]);
    assert_ne!(before["reset_epoch"], after["reset_epoch"]);
    assert_eq!(after["reset_epoch"], format!("{next:016x}"));
    assert!(after["confirmation"].is_null());
}
