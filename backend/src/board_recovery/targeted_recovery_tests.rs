// Actual HTTP coordinator + dedicated public-SDK peers. Peer lifecycle is a
// bounded fixture, not execution of MCU firmware or independent NVM proof.
fn targeted_model(f: &Fixture, board: Board) -> Arc<Mutex<Model>> {
    f.models
        .iter()
        .find(|(b, _)| *b == board)
        .unwrap()
        .1
        .clone()
}
async fn targeted_prepare_as(f: &Fixture, board: &str, token: Option<&str>) -> Response {
    let state = f.state();
    super::super::tests::http(
        state.clone(),
        "POST",
        "/api/board-recovery/prepare",
        token,
        json!({"action":"recover_abort","board":board,"mode":null,
            "backend_instance":hex(&state.board_recovery.identity.unwrap()),
            "reset_epoch":format!("{:016x}",crate::gse::automatic_generation(&state))}),
    )
    .await
}
#[tokio::test]
async fn targeted_recovery_all_five_actual_routes_apply_startup_to_selected_domain_only() {
    if isolated("targeted_recovery_all_five_actual_routes_apply_startup_to_selected_domain_only") {
        return;
    }
    for board in [
        Board::FlightComputer,
        Board::RFBoard,
        Board::PowerBoard,
        Board::GatewayBoard,
        Board::DaqBoard,
    ] {
        let f = Fixture::new().await;
        *f.state().state.lock().unwrap() = crate::types::FlightState::Aborted;
        f.query(board).await;
        let status = body(
            super::super::tests::http(
                f.state(),
                "GET",
                "/api/board-recovery/status",
                Some("operator"),
                json!({}),
            )
            .await,
        )
        .await;
        let row = status["boards"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == board.sender_id())
            .unwrap();
        assert_eq!(row["capabilities"]["recover"], true, "{status}");
        assert_eq!(row["capabilities"]["resync"], false);
        let ticket = f.prepare("recover_abort", Some(board.sender_id())).await;
        assert!(
            ticket["summary"]
                .as_str()
                .unwrap()
                .contains(if board == Board::FlightComputer {
                    "deployment history"
                } else {
                    "metadata"
                })
        );
        assert_eq!(f.confirm(&ticket).await.status(), StatusCode::OK);
        let op = f.terminal().await;
        assert_eq!(op["success"], true, "{board:?}: {op}");
        assert_eq!(
            f.state().local_flight_state_snapshot(),
            crate::types::FlightState::Aborted,
            "targeted operation must not publish global Startup"
        );
        for (id, model) in &f.models {
            let m = model.lock().unwrap();
            assert_eq!(m.applied, usize::from(*id == board));
            if *id == board {
                assert_eq!((m.boot, m.epoch, m.state), (42, 9, 0));
                let requests: Vec<_> = m.requests.iter().filter(|p| p.payload()[1] == 1).collect();
                assert!(!requests.is_empty());
                for request in requests {
                    assert_eq!(&request.payload()[..4], &[1, 1, 0, 0]);
                    assert_eq!(
                        request.endpoints(),
                        [crate::telemetry_schema::endpoint(endpoint(board))]
                    );
                }
                assert!(
                    m.requests.iter().filter(|p| p.payload()[1] == 0).count() >= 3,
                    "fresh query before and after terminal required"
                );
            } else {
                assert!(!m.requests.iter().any(|p| p.payload()[1] == 1));
            }
        }
        let read = f.state().board_recovery.inner.lock().unwrap().values[index(board).unwrap()]
            .as_ref()
            .unwrap()
            .value
            .clone();
        assert_eq!(read.state, Some(0));
        assert_eq!(
            read.flags & 0x3f,
            if board == Board::FlightComputer {
                0x3e
            } else {
                0x2c
            }
        );
        assert_eq!(
            f.confirm(&ticket).await.status(),
            StatusCode::CONFLICT,
            "completed intent cannot replay"
        );
    }
}
#[tokio::test]
async fn targeted_recovery_fc_forbidden_expired_and_stop_have_no_owner_effects() {
    if isolated("targeted_recovery_fc_forbidden_expired_and_stop_have_no_owner_effects") {
        return;
    }
    for denial in ["permission", "expired", "stop"] {
        let f = Fixture::new().await;
        f.query(Board::FlightComputer).await;
        let ticket = f.prepare("recover_abort", Some("FC")).await;
        match denial {
            "permission" => {
                sqlx::query("UPDATE auth_sessions SET can_send_commands=0 WHERE token='operator'")
                    .execute(&f.state().auth_db)
                    .await
                    .unwrap();
            }
            "expired" => {
                f.state()
                    .board_recovery
                    .coordinator
                    .0
                    .lock()
                    .unwrap()
                    .ticket
                    .as_mut()
                    .unwrap()
                    .created -= CONFIRM_TTL + Duration::from_millis(1);
            }
            "stop" => {
                f.state().manual_abort_intent.request();
            }
            _ => unreachable!(),
        }
        assert_eq!(
            f.confirm(&ticket).await.status(),
            if denial == "permission" {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::CONFLICT
            }
        );
        assert!(f.models.iter().all(|(_, m)| m.lock().unwrap().applied == 0));
        assert!(
            f.state()
                .launch_history
                .lock()
                .unwrap()
                .recovery_pending()
                .is_none()
        );
    }
    let f = Fixture::new().await;
    assert_eq!(
        targeted_prepare_as(&f, "FC", None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        targeted_prepare_as(&f, "GS", Some("operator"))
            .await
            .status(),
        StatusCode::CONFLICT
    );
}
#[tokio::test]
async fn targeted_recovery_fc_changed_boot_cannot_be_adopted_by_fresh_query() {
    if isolated("targeted_recovery_fc_changed_boot_cannot_be_adopted_by_fresh_query") {
        return;
    }
    let f = Fixture::new().await;
    f.query(Board::FlightComputer).await;
    let ticket = f.prepare("recover_abort", Some("FC")).await;
    targeted_model(&f, Board::FlightComputer)
        .lock()
        .unwrap()
        .boot = 43;
    assert_eq!(f.confirm(&ticket).await.status(), StatusCode::OK);
    let op = f.terminal().await;
    assert_eq!(op["success"], false, "{op}");
    assert!(f.models.iter().all(|(_, m)| m.lock().unwrap().applied == 0));
    assert_ne!(
        f.state().operator_admission_barrier.load(Ordering::Acquire),
        0
    );
}
#[tokio::test]
async fn targeted_recovery_fc_stop_after_owner_before_publication_stays_closed() {
    if isolated("targeted_recovery_fc_stop_after_owner_before_publication_stays_closed") {
        return;
    }
    let f = Fixture::new().await;
    f.query(Board::FlightComputer).await;
    let ticket = f.prepare("recover_abort", Some("FC")).await;
    let pause = PauseGuard::install(&FINAL, &f.state());
    assert_eq!(f.confirm(&ticket).await.status(), StatusCode::OK);
    pause.entered().await;
    f.state().manual_abort_intent.request();
    pause.release();
    let op = f.terminal().await;
    assert_eq!(op["success"], false, "{op}");
    assert_ne!(
        f.state().operator_admission_barrier.load(Ordering::Acquire),
        0
    );
    assert!(f.state().manual_abort_intent.pending_epoch().is_some());
    assert_eq!(
        targeted_model(&f, Board::FlightComputer)
            .lock()
            .unwrap()
            .applied,
        1,
        "already-linearized owner action is not undone by host proof failure"
    );
    assert!(
        !f.state()
            .launch_history
            .lock()
            .unwrap()
            .recovery_pending()
            .unwrap()
            .published
    );
}
#[tokio::test]
async fn targeted_recovery_metadata_old_unhardened_query_never_enables_writes() {
    if isolated("targeted_recovery_metadata_old_unhardened_query_never_enables_writes") {
        return;
    }
    for board in [
        Board::RFBoard,
        Board::PowerBoard,
        Board::GatewayBoard,
        Board::DaqBoard,
    ] {
        let f = Fixture::new().await;
        targeted_model(&f, board).lock().unwrap().hardened = false;
        f.query(board).await;
        let status = body(
            super::super::tests::http(
                f.state(),
                "GET",
                "/api/board-recovery/status",
                Some("operator"),
                json!({}),
            )
            .await,
        )
        .await;
        let row = status["boards"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == board.sender_id())
            .unwrap();
        assert_eq!(row["capabilities"]["recover"], false);
        assert_eq!(row["capabilities"]["hardened_transport"], false);
        assert_eq!(
            targeted_prepare_as(&f, board.sender_id(), Some("operator"))
                .await
                .status(),
            StatusCode::CONFLICT
        );
        assert!(f.models.iter().all(|(_, m)| m.lock().unwrap().applied == 0));
    }
}
#[tokio::test]
async fn targeted_recovery_fc_io_restart_requires_fresh_explicit_intent() {
    if isolated("targeted_recovery_fc_io_restart_requires_fresh_explicit_intent") {
        return;
    }
    let first = Fixture::new().await;
    targeted_model(&first, Board::FlightComputer)
        .lock()
        .unwrap()
        .fail_result = Some(7);
    first.query(Board::FlightComputer).await;
    let old = first.prepare("recover_abort", Some("FC")).await;
    assert_eq!(first.confirm(&old).await.status(), StatusCode::OK);
    assert_eq!(first.terminal().await["success"], false);
    let journal = std::fs::read(first.http.directory.join("clock.json")).unwrap();
    let retained = first.models.clone();
    let counts: Vec<_> = retained
        .iter()
        .map(|(_, m)| m.lock().unwrap().requests.len())
        .collect();
    drop(first);
    tokio::task::yield_now().await;
    let f = Fixture::from_saved(Some(journal), Some(retained)).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(
        f.models
            .iter()
            .map(|(_, m)| m.lock().unwrap().requests.len())
            .collect::<Vec<_>>(),
        counts,
        "no automatic restart retry"
    );
    assert_eq!(f.confirm(&old).await.status(), StatusCode::CONFLICT);
    targeted_model(&f, Board::FlightComputer)
        .lock()
        .unwrap()
        .fail_result = None;
    f.query(Board::FlightComputer).await;
    let fresh = f.prepare("recover_abort", Some("FC")).await;
    assert_ne!(fresh["backend_instance"], old["backend_instance"]);
    assert_eq!(f.confirm(&fresh).await.status(), StatusCode::OK);
    let op = f.terminal().await;
    assert_eq!(op["success"], true, "{op}");
    assert_eq!(
        targeted_model(&f, Board::FlightComputer)
            .lock()
            .unwrap()
            .applied,
        1
    );
    assert!(
        f.models
            .iter()
            .filter(|(b, _)| *b != Board::FlightComputer)
            .all(|(_, m)| m.lock().unwrap().applied == 0)
    );
}
