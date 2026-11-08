// Resync through the actual HTTP coordinator and dedicated public-SDK peers.
// The peer models firmware RESYNC (bit 7, operation 2); it is not MCU firmware.
fn resync_model(f: &Fixture, board: Board) -> Arc<Mutex<Model>> {
    f.models.iter().find(|(b, _)| *b == board).unwrap().1.clone()
}
async fn resync_prepare_raw(f: &Fixture, board: &str) -> (StatusCode, Value) {
    let state = f.state();
    let response = super::super::tests::http(
        state.clone(),
        "POST",
        "/api/board-recovery/prepare",
        Some("operator"),
        json!({"action":"resync","board":board,"mode":null,
            "backend_instance":hex(&state.board_recovery.identity.unwrap()),
            "reset_epoch":format!("{:016x}",crate::gse::automatic_generation(&state))}),
    )
    .await;
    let status = response.status();
    (status, body(response).await)
}
async fn resync_status(f: &Fixture) -> Value {
    body(
        super::super::tests::http(f.state(), "GET", "/api/board-recovery/status", Some("operator"), json!({}))
            .await,
    )
    .await
}
fn resync_row<'a>(status: &'a Value, id: &str) -> &'a Value {
    status["boards"].as_array().unwrap().iter().find(|row| row["id"] == id).unwrap()
}
fn resync_router(state: &AppState) {
    let endpoint = crate::telemetry_schema::endpoint("ACTUATOR_BOARD");
    let router = Arc::new(Router::new(RouterConfig::new([
        EndpointHandler::new_packet_handler(endpoint, |_| Ok(())),
    ])));
    state.topology_router.set(router).unwrap();
}

/// The board rebooted mid-fill and latched Abort. Resync brings it back to the
/// station's phase: only the state machine and epoch move, never an output.
#[tokio::test]
async fn resync_moves_only_the_board_phase_and_never_its_outputs() {
    if isolated("resync_moves_only_the_board_phase_and_never_its_outputs") {
        return;
    }
    let f = Fixture::new().await;
    let state = f.state();
    *state.state.lock().unwrap() = crate::types::FlightState::NitrousFill;
    let model = resync_model(&f, Board::ActuatorBoard);
    model.lock().unwrap().resync = true;
    f.query(Board::ActuatorBoard).await;
    f.query(Board::ValveBoard).await;
    let status = resync_status(&f).await;
    assert_eq!(status["resync_target_state"], 5, "{status}");
    assert_eq!(status["resync_target_label"], "NitrousFill");
    let row = resync_row(&status, "AB");
    assert_eq!(row["capabilities"]["resync"], true, "{status}");
    assert!(row["resync_blocked_reason"].is_null(), "{status}");
    assert_eq!(row["abort_latched"], true);
    let outputs_before = row["commanded_outputs"].clone();
    assert!(outputs_before.is_u64());
    // A board whose firmware does not advertise Resync never offers it.
    assert_eq!(resync_row(&status, "VB")["capabilities"]["resync"], false);

    let ticket = f.prepare("resync", Some("AB")).await;
    assert_eq!(ticket["confirmation"], "RESYNC AB TO NITROUSFILL");
    assert_eq!(ticket["targets"], json!(["AB"]));
    assert_eq!(ticket["resync_target_state"], 5);
    assert_eq!(ticket["outputs_unchanged"], true);
    assert!(ticket["summary"].as_str().unwrap().contains("No outputs move"), "{ticket}");
    assert_eq!(f.confirm(&ticket).await.status(), StatusCode::OK);
    let op = f.terminal().await;
    assert_eq!(op["success"], true, "{op}");
    assert!(op["message"].as_str().unwrap().contains("No output was commanded"), "{op}");
    assert!(op["reset_applied_state"].is_null());
    {
        let m = model.lock().unwrap();
        assert_eq!((m.state, m.epoch, m.applied), (5, 9, 1));
        let mutations: Vec<_> = m.requests.iter().filter(|p| p.payload()[1] != 0).collect();
        assert_eq!(mutations.len(), 1, "exactly one RESYNC, no replay");
        assert_eq!((mutations[0].payload()[1], mutations[0].payload()[2]), (2, 5));
    }
    for (id, other) in &f.models {
        if *id != Board::ActuatorBoard {
            assert_eq!(other.lock().unwrap().applied, 0, "{id:?}");
        }
    }
    // The station keeps its phase. Resync never publishes or resets it.
    assert_eq!(state.local_flight_state_snapshot(), crate::types::FlightState::NitrousFill);
    let after = resync_status(&f).await;
    let row = resync_row(&after, "AB");
    assert_eq!(row["state"], 5, "{after}");
    assert_eq!(row["abort_latched"], false);
    assert_eq!(row["commanded_outputs"], outputs_before, "outputs unchanged");
    assert!(row["resync_blocked_reason"].as_str().unwrap().contains("already matches"), "{after}");
    // The completed journal marker validates and acknowledged publication.
    let pending = state.launch_history.lock().unwrap().recovery_pending().unwrap();
    assert!(pending.published && pending.targets[0].intent.body[1] == 2);
    assert!(!pending.verified_startup_policy(), "a resync is never a Startup reset");
}

/// CONFIRM rechecks the frozen phase: a station that moved on refuses the
/// reviewed ticket and the board is never asked to change.
#[tokio::test]
async fn resync_confirmation_is_bound_to_the_frozen_station_phase() {
    if isolated("resync_confirmation_is_bound_to_the_frozen_station_phase") {
        return;
    }
    let f = Fixture::new().await;
    let state = f.state();
    *state.state.lock().unwrap() = crate::types::FlightState::NitrogenFill;
    let model = resync_model(&f, Board::ValveBoard);
    model.lock().unwrap().resync = true;
    f.query(Board::ValveBoard).await;
    let ticket = f.prepare("resync", Some("VB")).await;
    assert_eq!(ticket["confirmation"], "RESYNC VB TO NITROGENFILL");
    *state.state.lock().unwrap() = crate::types::FlightState::NitrousFill;
    let response = f.confirm(&ticket).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let m = model.lock().unwrap();
    assert_eq!(m.applied, 0);
    assert!(m.requests.iter().all(|p| p.payload()[1] == 0));
}

/// Refusals happen before any board mutation and say why in plain text.
#[tokio::test]
async fn resync_refusals_name_the_reason_and_send_nothing() {
    if isolated("resync_refusals_name_the_reason_and_send_nothing") {
        return;
    }
    let f = Fixture::new().await;
    let state = f.state();
    resync_model(&f, Board::ActuatorBoard).lock().unwrap().resync = true;
    for (phase, needle) in [
        (crate::types::FlightState::Idle, "Recover to Idle"),
        (crate::types::FlightState::Startup, "Startup"),
        (crate::types::FlightState::Aborted, "Reset all boards"),
        (crate::types::FlightState::Launch, "Launch onward"),
        (crate::types::FlightState::Landed, "Launch onward"),
    ] {
        *state.state.lock().unwrap() = phase;
        let (code, value) = resync_prepare_raw(&f, "AB").await;
        assert_eq!(code, StatusCode::CONFLICT, "{phase:?}: {value}");
        assert_eq!(value["error"], "resync_unavailable");
        assert!(value["message"].as_str().unwrap().contains(needle), "{phase:?}: {value}");
    }
    *state.state.lock().unwrap() = crate::types::FlightState::Armed;
    // The Flight Computer owns its flight states; metadata boards follow the
    // published phase. Neither is a resync target.
    for board in ["FC", "GB", "DAQ", "RF", "PB"] {
        let (code, value) = resync_prepare_raw(&f, board).await;
        assert_eq!(code, StatusCode::CONFLICT, "{board}: {value}");
        assert!(value["message"].as_str().unwrap().contains("only for the Actuator Board and the Valve Board"), "{value}");
    }
    // Firmware without the capability bit.
    let (code, value) = resync_prepare_raw(&f, "VB").await;
    assert_eq!(code, StatusCode::CONFLICT, "{value}");
    assert!(value["message"].as_str().unwrap().contains("does not support Resync"), "{value}");
    for (_, model) in &f.models {
        let m = model.lock().unwrap();
        assert_eq!(m.applied, 0);
        assert!(m.requests.iter().all(|p| p.payload()[1] == 0));
    }
    assert!(state.launch_history.lock().unwrap().recovery_pending().is_none());
}

#[tokio::test]
async fn resync_window_is_the_fill_phases_and_armed_without_active_safety() {
    use crate::types::FlightState as F;
    let state = crate::state::tests::test_app_state().await;
    resync_router(&state);
    for phase in [
        F::Startup, F::Idle, F::PreFill, F::FillTest, F::NitrogenFill, F::NitrousFill, F::Armed,
        F::Launch, F::Ascent, F::Coast, F::Apogee, F::Descent, F::Reefing, F::Landed,
        F::Recovery, F::Aborted,
    ] {
        let allowed = matches!(phase, F::PreFill | F::FillTest | F::NitrogenFill | F::NitrousFill | F::Armed);
        *state.state.lock().unwrap() = phase;
        assert_eq!(resync_target(phase), allowed.then_some(phase as u8), "{phase:?}");
        assert_eq!(host_action_allowed(&state, "resync"), allowed, "{phase:?}");
        assert_eq!(resync_host_block_reason(&state).is_none(), allowed, "{phase:?}");
    }
    // An active tank overpressure means a board Abort may still be protecting
    // the tank: Resync refuses and points to Review recovery / Reset all.
    *state.state.lock().unwrap() = F::NitrousFill;
    crate::gse::set_overpressure_latch_for_test(&state, Some(crate::gse::overpressure::Latch {
        peak_psi: 900.0,
        threshold_psi: 850.0,
        at_ms: 1,
        message: "fixture".into(),
        relief_commanded: true,
        abort: crate::gse::overpressure::AbortStage::NotRequired,
        ab_abort_sent: false,
    }));
    let reason = resync_host_block_reason(&state).unwrap();
    assert!(reason.contains("overpressure") && reason.contains("Review recovery") && reason.contains("Reset all boards"), "{reason}");
    assert!(!host_action_allowed(&state, "resync"));
    crate::gse::set_overpressure_latch_for_test(&state, None);
    assert!(host_action_allowed(&state, "resync"));
    // A sequence moving valves blocks it; one holding Paused does not.
    state.gse.lock().unwrap().engine.status.phase = gse_sequence::Phase::Filling;
    assert!(resync_host_block_reason(&state).unwrap().contains("sequence"));
    state.gse.lock().unwrap().engine.status.phase = gse_sequence::Phase::Paused;
    assert!(host_action_allowed(&state, "resync"));
    // Reset all boards keeps its own wider window.
    *state.state.lock().unwrap() = F::Launch;
    assert!(!host_action_allowed(&state, "resync"));
    assert!(host_action_allowed(&state, "reset_startup"));
}

#[test]
fn resync_board_reasons_follow_the_readback() {
    let row = |flags: u8, outputs: u32, state: u8| Readback {
        result: 0,
        state: Some(state),
        flags,
        nonce: 1,
        boot: 42,
        epoch: 3,
        uptime: 1,
        outputs,
        issuer: 7,
    };
    let safe = 0x24 | STATE_VALID | OUTPUTS_VALID | 2 | 1;
    assert!(resync_board_block_reason(Board::ActuatorBoard, None, Some(5)).unwrap().contains("Query"));
    assert!(resync_board_block_reason(Board::ActuatorBoard, Some(&row(safe, 0, 0)), Some(5)).unwrap().contains("does not support"));
    assert_eq!(resync_board_block_reason(Board::ActuatorBoard, Some(&row(safe | RESYNC_IMPLEMENTATION, 0, 0)), Some(5)), None);
    assert_eq!(resync_board_block_reason(Board::ValveBoard, Some(&row(safe | RESYNC_IMPLEMENTATION, 6, 0)), Some(5)), None);
    // Outputs that do not read safe: Resync would have to move them, so it refuses.
    let unsafe_flags = (safe | RESYNC_IMPLEMENTATION) & !2;
    let reason = resync_board_block_reason(Board::ActuatorBoard, Some(&row(unsafe_flags, 2, 0)), Some(5)).unwrap();
    assert!(reason.contains("never moves outputs") && reason.contains("Review recovery"), "{reason}");
    assert!(resync_board_block_reason(Board::ValveBoard, Some(&row(safe | RESYNC_IMPLEMENTATION, 0, 0)), Some(5)).is_some());
    // Already in the phase with no Abort latched: nothing to do.
    let synced = (safe | RESYNC_IMPLEMENTATION) & !1;
    assert!(resync_board_block_reason(Board::ActuatorBoard, Some(&row(synced, 0, 5)), Some(5)).unwrap().contains("already matches"));
    let mut commissioning = row(safe | RESYNC_IMPLEMENTATION, 0, 0);
    commissioning.boot = u64::MAX;
    assert!(resync_board_block_reason(Board::ActuatorBoard, Some(&commissioning), Some(5)).unwrap().contains("commissioning"));
}
