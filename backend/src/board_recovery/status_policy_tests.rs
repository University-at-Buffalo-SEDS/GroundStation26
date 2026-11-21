//! Actual authenticated GET contract. Capability data never grants recovery.
use super::*;
use std::sync::atomic::Ordering;

/// Serializes the tests that set or depend on `rrls::primary_relaying()`.
static RELAYING_FLAG: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    assert_eq!(value["permissions"]["reset_startup"], false);
    assert_eq!(value["clock"]["network_utc_available"], false);
    assert_eq!(value["mode_capability"]["runtime_switch_supported"], true);
    assert_eq!(value["mode_capability"]["requires_restart"], json!([]));
    assert_eq!(value["mode_capability"]["supported_modes"].as_array().unwrap().len(), 4);
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
    if crate::runtime_mode::is_test_fire() {
        // A test fire's reset scope is the stand: an aborted AV-bay board
        // neither blocks Launch nor becomes a mandatory reset target.
        assert!(!selected.boards().contains(&Board::RFBoard));
        f.state
            .board_abort
            .observe_abort(Board::ValveBoard, WallInstant::now(), 101);
        let selected = scope::Selection::capture(&f.state, &[]).unwrap();
        assert!(
            selected.boards().contains(&Board::ValveBoard),
            "PREPARE still includes an actual stand Abort independent of base capability"
        );
        return;
    }
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
    if crate::runtime_mode::is_hitl() {
        f.state.toggle_hitl_safety_interlock(3);
        f.state.toggle_hitl_safety_interlock(3);
    } else {
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

#[tokio::test]
async fn actual_get_network_utc_requirement_matches_confirm_boundary_without_writing() {
    let f = fixture(Some("GS")).await;
    let router = f.state.topology_router.get().unwrap();
    assert!(exchange::ConfirmBoundary::capture(router).is_err());
    let (_, before) = status(&f, Some("operator")).await;
    assert_eq!(before["permissions"]["recover"], false);
    router
        .try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(
            1_793_000_000_000,
        ))
        .unwrap();
    assert!(exchange::ConfirmBoundary::capture(router).is_ok());
    let (_, after) = status(&f, Some("operator")).await;
    assert_eq!(after["clock"]["network_utc_available"], true);
    assert_eq!(after["permissions"]["recover"], true);
    assert!(f.state.board_recovery.coordinator.operation().is_none());
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
}
#[test]
fn commissioning_and_expired_context_diagnostics_never_authorize_zero_identity() {
    let now = Stamp::now();
    let mut row = Observation {
        received: now,
        observed_after: now,
        value: Readback {
            boot: u64::MAX,
            epoch: 1,
            nonce: 2,
            issuer: 3,
            result: 7,
            state: Some(0),
            flags: 0x7e,
            outputs: 0,
            uptime: 4,
        },
    };
    assert_eq!(
        recovery_diagnostic(true, true, true, Some(&row), None, now, false).0,
        "commissioning_required"
    );
    assert_eq!(
        recovery_diagnostic(true, true, false, Some(&row), None, now, false).0,
        "network_utc_unavailable"
    );
    row.observed_after = now - READBACK_TTL - Duration::from_millis(1);
    assert_eq!(
        recovery_diagnostic(true, true, true, Some(&row), None, now, false).0,
        "commissioning_readback_expired"
    );
    row.value.boot = 0;
    row.observed_after = now;
    assert_eq!(
        recovery_diagnostic(true, true, true, Some(&row), None, now, false).0,
        "identity_unavailable"
    );
    row.value.boot = 42;
    row.value.result = 4;
    assert_eq!(
        recovery_diagnostic(true, true, true, Some(&row), None, now, false).0,
        "stale_recovery_context"
    );
    let query = Query {
        abort_revision: 0,
        nonce: 2,
        sent: now,
        received: false,
    };
    assert_eq!(
        recovery_diagnostic(true, true, true, None, Some(&query), now, false).0,
        "query_pending"
    );
    assert_eq!(
        recovery_diagnostic(true, true, true, None, None, now, false).0,
        "query_required"
    );
}

#[test]
fn readonly_refresh_budget_and_pending_context_are_bounded() {
    let now = Stamp::now();
    assert!(readonly_query_due(None, now.wall));
    assert!(!readonly_query_due(Some(now.wall), now.wall));
    assert!(!readonly_query_due(
        Some(now.wall - Duration::from_millis(1999)),
        now.wall
    ));
    assert!(readonly_query_due(
        Some(now.wall - Duration::from_secs(2)),
        now.wall
    ));
    assert!(readonly_query_candidate(true, true, None, None, now));
    assert!(!readonly_query_candidate(false, true, None, None, now));
    assert!(!readonly_query_candidate(true, false, None, None, now));
    let mut query = Query {
        abort_revision: 0,
        nonce: 9,
        sent: now,
        received: false,
    };
    assert!(!readonly_query_candidate(
        true,
        true,
        None,
        Some(&query),
        now
    ));
    query.received = true;
    assert!(readonly_query_candidate(
        true,
        true,
        None,
        Some(&query),
        now
    ));
    query.sent = now - QUERY_TTL - Duration::from_millis(1);
    assert!(readonly_query_candidate(
        true,
        true,
        None,
        Some(&query),
        now
    ));
    let mut row = Observation {
        received: now,
        observed_after: now,
        value: Readback {
            boot: 42,
            epoch: 1,
            nonce: 2,
            issuer: 3,
            result: 0,
            state: Some(0),
            flags: 0x7e,
            outputs: 0,
            uptime: 4,
        },
    };
    assert!(!readonly_query_candidate(true, true, Some(&row), None, now));
    row.observed_after = now - READBACK_TTL - Duration::from_millis(1);
    assert!(readonly_query_candidate(true, true, Some(&row), None, now));
}

#[tokio::test]
async fn stale_commissioning_identity_allows_only_prepare_not_actual_recovery_proof() {
    let f = fixture(Some("GS")).await;
    let router = f.state.topology_router.get().unwrap();
    router
        .try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(
            1_793_000_000_000,
        ))
        .unwrap();
    let side = router.add_side_packed("readonly-status-test", |_| Ok(()));
    router
        .rx_from_side(
            &sedsnet::discovery::build_discovery_announce(
                "AB",
                1,
                &[crate::telemetry_schema::endpoint("BOARD_RECOVERY_AB")],
            )
            .unwrap(),
            side,
        )
        .unwrap();
    let now = Stamp::now();
    let at = index(Board::ActuatorBoard).unwrap();
    f.state.board_recovery.inner.lock().unwrap().values[at] = Some(Observation {
        received: now,
        observed_after: now - READBACK_TTL - Duration::from_millis(1),
        value: Readback {
            boot: u64::MAX,
            epoch: 1,
            nonce: 2,
            issuer: 3,
            result: 7,
            state: Some(0),
            flags: 0x7e,
            outputs: 0,
            uptime: 4,
        },
    });
    let (_, value) = status(&f, Some("operator")).await;
    let board = value["boards"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "AB")
        .unwrap();
    assert_eq!(board["capabilities"]["prepare_recover"], true);
    assert_eq!(board["capabilities"]["recover"], false);
    assert_eq!(board["readback_status"], "commissioning_readback_expired");
    assert!(f.state.board_recovery.coordinator.operation().is_none());
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
}

/// Relay-cut runs: background AV-bay queries waiting on a lost radio path
/// held the one ordered BOARD_RECOVERY_REQUEST stream, so the umbilical rows
/// read back an Abort 4-23 s after the boards latched it. The background
/// refresh pauses AV-bay queries while the AV-bay path is degraded, says so
/// on the rows, and resumes only when the direct path is back and every
/// AV-bay board has reported since the pause.
#[tokio::test]
async fn background_av_bay_queries_pause_while_the_av_bay_path_is_degraded() {
    let _relaying = RELAYING_FLAG.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = tests::http_fixture().await;
    let state = fixture.state.clone();
    let mut refresh = ReadonlyQueryRefresh::default();
    let seen = |state: &Arc<AppState>| {
        let mut status = state.board_status.lock().unwrap();
        for board in [Board::RFBoard, Board::FlightComputer, Board::PowerBoard] {
            status.get_mut(&board).unwrap().last_seen_instant = Some(std::time::Instant::now());
        }
    };
    // One background FC query unanswered past its retry window is a lost
    // frame on a ~5% loss link, not a degraded path: no pause.
    state.board_recovery.inner.lock().unwrap().queries[1] =
        Some(Query { abort_revision: 0, nonce: 9, sent: Stamp::now() - query_retry() - Duration::from_millis(100), received: false });
    refresh.update_av_bay_pause(&state, Stamp::now());
    assert_eq!(state.board_recovery.av_bay_pause_reason(), None, "one lost query does not pause");
    // The third unanswered query in a row does.
    refresh.misses[1] = AV_BAY_PAUSE_MISSES - 1;
    refresh.update_av_bay_pause(&state, Stamp::now());
    assert_eq!(state.board_recovery.av_bay_pause_reason(), Some("an AV-bay status query went unanswered"));
    assert!(!refresh.request(&state, Board::FlightComputer), "no background AV-bay query while paused");
    // Status reports right away do not end the pause: the lost query's retry
    // span must run out first, or the refresh flaps (deploy-hitl: ten
    // pause/resume pairs in 60 s, resuming 0.25 s after pausing).
    std::thread::sleep(Duration::from_millis(5));
    seen(&state);
    refresh.update_av_bay_pause(&state, Stamp::now());
    assert!(state.board_recovery.av_bay_pause_reason().is_some(), "no resume within the minimum pause");
    // After the minimum pause, reports from every AV-bay board since the
    // pause prove the link: resume.
    refresh.av_bay_paused_since = refresh.av_bay_paused_since.map(|since| since - av_bay_min_pause());
    seen(&state);
    refresh.update_av_bay_pause(&state, Stamp::now());
    assert_eq!(state.board_recovery.av_bay_pause_reason(), None, "resumed after the minimum pause and status reports");
    // The primary relies on the RRLS relay: paused even with fresh reports,
    // until the direct path is back and the boards have reported again.
    crate::rrls::set_primary_relaying_for_test(true);
    refresh.update_av_bay_pause(&state, Stamp::now());
    assert_eq!(state.board_recovery.av_bay_pause_reason(), Some("the AV bay is reached through the RRLS relay"));
    std::thread::sleep(Duration::from_millis(5));
    seen(&state);
    refresh.update_av_bay_pause(&state, Stamp::now());
    assert!(state.board_recovery.av_bay_pause_reason().is_some(), "still relaying");
    crate::rrls::set_primary_relaying_for_test(false);
    std::thread::sleep(Duration::from_millis(5));
    refresh.av_bay_paused_since = refresh.av_bay_paused_since.map(|since| since - av_bay_min_pause());
    seen(&state);
    refresh.update_av_bay_pause(&state, Stamp::now());
    assert_eq!(state.board_recovery.av_bay_pause_reason(), None, "direct path back and reported");
}

/// A powered-off AV-bay board must not hold the pause: on the bench with PB
/// off, RF and FC rows expired for good and showed "recovery required".
#[tokio::test]
async fn an_av_bay_pause_resumes_without_a_board_that_was_already_off() {
    // Test fire tracks no AV-bay board; see the test below.
    crate::mode_suite!(Normal, Hitl, Testing);
    let _relaying = RELAYING_FLAG.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = tests::http_fixture().await;
    let state = fixture.state.clone();
    let mut refresh = ReadonlyQueryRefresh::default();
    let seen = |state: &Arc<AppState>, boards: &[Board]| {
        let mut status = state.board_status.lock().unwrap();
        for board in boards {
            status.get_mut(board).unwrap().last_seen_instant = Some(std::time::Instant::now());
        }
    };
    // PB was last heard long before; RF and FC shortly before the pause
    // (which is back-dated by the minimum pause below).
    let ago = |age: Duration| std::time::Instant::now().checked_sub(age);
    {
        let mut status = state.board_status.lock().unwrap();
        status.get_mut(&Board::PowerBoard).unwrap().last_seen_instant = ago(AV_BAY_OFFLINE * 3);
        for board in [Board::RFBoard, Board::FlightComputer] {
            status.get_mut(&board).unwrap().last_seen_instant = ago(av_bay_min_pause() * 2);
        }
    }
    crate::rrls::set_primary_relaying_for_test(true);
    refresh.update_av_bay_pause(&state, Stamp::now());
    assert!(state.board_recovery.av_bay_pause_reason().is_some());
    crate::rrls::set_primary_relaying_for_test(false);
    refresh.av_bay_paused_since = refresh.av_bay_paused_since.map(|since| since - av_bay_min_pause());
    // FC has not reported since the pause: still paused.
    std::thread::sleep(Duration::from_millis(5));
    seen(&state, &[Board::RFBoard]);
    refresh.update_av_bay_pause(&state, Stamp::now());
    assert!(state.board_recovery.av_bay_pause_reason().is_some(), "a board present before the pause is waited for");
    seen(&state, &[Board::FlightComputer]);
    refresh.update_av_bay_pause(&state, Stamp::now());
    assert_eq!(state.board_recovery.av_bay_pause_reason(), None, "the powered-off PB is not waited for");
}

/// Test fire tracks no AV-bay board (mode scope 0x78), so no report can end
/// a pause: the minimum pause alone resumes it once the direct path is back.
/// (3250eda waited for "at least one" report and never resumed in test fire.)
#[tokio::test]
async fn a_test_fire_av_bay_pause_resumes_after_the_minimum_pause() {
    crate::mode_suite!(TestFire);
    let _relaying = RELAYING_FLAG.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = tests::http_fixture().await;
    let state = fixture.state.clone();
    let mut refresh = ReadonlyQueryRefresh::default();
    crate::rrls::set_primary_relaying_for_test(true);
    refresh.update_av_bay_pause(&state, Stamp::now());
    assert!(state.board_recovery.av_bay_pause_reason().is_some());
    crate::rrls::set_primary_relaying_for_test(false);
    refresh.update_av_bay_pause(&state, Stamp::now());
    assert!(state.board_recovery.av_bay_pause_reason().is_some(), "the minimum pause still holds");
    refresh.av_bay_paused_since = refresh.av_bay_paused_since.map(|since| since - av_bay_min_pause());
    refresh.update_av_bay_pause(&state, Stamp::now());
    assert_eq!(state.board_recovery.av_bay_pause_reason(), None, "no AV-bay report to wait for in test fire");
}

/// deploy-normal (GS eadd886): two background AV-bay queries ran back to back
/// in the one ordered BOARD_RECOVERY_REQUEST stream across the Abort, so VB
/// and GB were asked only after both and read Aborted 2.1 s late (AB and
/// DAQ, asked between them, read it at 0.7 s). The background refresh keeps
/// at most one AV-bay query in the stream.
#[tokio::test]
async fn background_refresh_keeps_one_av_bay_query_in_the_ordered_stream() {
    // Another test sets the process-wide "primary relaying" flag, which
    // pauses background AV-bay queries.
    let _relaying = RELAYING_FLAG.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = tests::http_fixture().await;
    let state = fixture.state.clone();
    let host = Arc::new(sedsnet::router::Router::new(sedsnet::router::RouterConfig::new([]).with_sender("GS")));
    assert!(state.topology_router.set(host.clone()).is_ok());
    let opts = sedsnet::router::RouterSideOptions { reliable_enabled: true, ..Default::default() };
    // The radio takes every frame and nothing answers yet.
    let radio = host.add_side_packed_with_options("rocket_comms", |_| Ok(()), opts);
    for board in [Board::RFBoard, Board::FlightComputer, Board::PowerBoard] {
        let announce = sedsnet::discovery::build_discovery_announce(
            board.sender_id(), 1, &[crate::telemetry_schema::endpoint(endpoint(board))]).unwrap();
        host.rx_from_side(&announce, radio).unwrap();
    }
    let mut service = crate::telemetry_task::RouterServiceForTest::default();
    service.pump(&host);
    let asked = |state: &Arc<AppState>| {
        let inner = state.board_recovery.inner.lock().unwrap();
        AV_BAY_INDICES.iter().filter(|&&at| inner.queries[at].is_some()).count()
    };
    let mut refresh = ReadonlyQueryRefresh::default();
    for _ in 0..6 {
        let mut status = state.board_status.lock().unwrap();
        for board in [Board::RFBoard, Board::FlightComputer, Board::PowerBoard] {
            status.get_mut(&board).unwrap().last_seen_instant = Some(std::time::Instant::now());
        }
        drop(status);
        let _ = refresh.poll(&state);
        service.pump(&host);
    }
    // Test fire tracks no AV-bay board, so it asks none.
    if scope::mode_scope(crate::station_mode::local_mode()) & 1 == 0 {
        assert_eq!(asked(&state), 0);
        return;
    }
    assert!(abort_confirm::av_bay_query_in_flight(&host), "the first AV-bay query is in flight");
    assert_eq!(asked(&state), 1, "no second AV-bay query queued behind the first");
}

/// C-hitl-0495e79 cycle 2: Reset all's prepare asked PB for a fresh readback
/// while background AV-bay queries were paused. The pause then resumed and
/// cleared PB's unanswered query as "lost", so PB's reply no longer matched
/// and the prepare waited out its 15 s window ("An AV-bay board's fresh query
/// got no reply"). While a prepare's fresh queries are in flight, the
/// background refresh leaves every query slot alone.
#[tokio::test]
async fn the_background_refresh_never_touches_a_prepares_query() {
    let _relaying = RELAYING_FLAG.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = tests::http_fixture().await;
    let state = fixture.state.clone();
    let host = Arc::new(sedsnet::router::Router::new(sedsnet::router::RouterConfig::new([]).with_sender("GS")));
    assert!(state.topology_router.set(host).is_ok());
    let pb = index(Board::PowerBoard).unwrap();
    let mut refresh = ReadonlyQueryRefresh::default();
    // Paused long enough to resume, and every AV-bay board reported since.
    refresh.av_bay_paused_since = Some(WallInstant::now() - av_bay_min_pause() - Duration::from_secs(1));
    *state.board_recovery.av_bay_paused.lock().unwrap() = Some("an AV-bay status query went unanswered");
    for board in [Board::RFBoard, Board::FlightComputer, Board::PowerBoard] {
        state.board_status.lock().unwrap().get_mut(&board).unwrap().last_seen_instant = Some(std::time::Instant::now());
    }
    // The prepare's PB query, unanswered past the background retry age.
    let prepare = Query { abort_revision: 0, nonce: 77, sent: Stamp::now() - query_retry() - Duration::from_millis(500), received: false };
    state.board_recovery.inner.lock().unwrap().queries[pb] = Some(prepare.clone());
    let refreshing = coordinator::PrepareQueries::begin(&state);
    let _ = refresh.poll(&state);
    let kept = state.board_recovery.inner.lock().unwrap().queries[pb].as_ref().map(|query| query.nonce);
    assert_eq!(kept, Some(77), "the prepare's query stays in its slot");
    drop(refreshing);
    // Afterwards the resume may clear a query that really was lost.
    let _ = refresh.poll(&state);
    assert!(state.board_recovery.inner.lock().unwrap().queries[pb].as_ref().is_none_or(|query| query.nonce != 77));
}
