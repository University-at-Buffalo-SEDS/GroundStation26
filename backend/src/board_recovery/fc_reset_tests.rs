// FC reset to Startup from any state (CONTRACT.md "Reset protocol") through
// the actual HTTP coordinator and dedicated public-SDK peers. The FC peer
// models firmware RESET_STARTUP_ANY (bit 7 on the FC, operation 3); it is not
// MCU firmware.
fn fc_reset_model(f: &Fixture) -> Arc<Mutex<Model>> {
    f.models.iter().find(|(b, _)| *b == Board::FlightComputer).unwrap().1.clone()
}
async fn fc_reset_prepare_raw(f: &Fixture) -> (StatusCode, Value) {
    let state = f.state();
    let response = super::super::tests::http(
        state.clone(),
        "POST",
        "/api/board-recovery/prepare",
        Some("operator"),
        json!({"action":"reset_fc_startup","board":"FC","mode":null,
            "backend_instance":hex(&state.board_recovery.identity.unwrap()),
            "reset_epoch":format!("{:016x}",crate::gse::automatic_generation(&state))}),
    )
    .await;
    let status = response.status();
    (status, body(response).await)
}
async fn fc_reset_status(f: &Fixture) -> Value {
    body(
        super::super::tests::http(f.state(), "GET", "/api/board-recovery/status", Some("operator"), json!({}))
            .await,
    )
    .await
}
fn fc_reset_row(status: &Value) -> &Value {
    status["boards"].as_array().unwrap().iter().find(|row| row["id"] == "FC").unwrap()
}

/// The FC is in Descent after the operator's Deploy. The station resets it to
/// Startup with no Abort first, from In flight: only operation 3 reaches the
/// FC, no other board and no FLIGHT_STATE broadcast, and the station's phase
/// is unchanged.
#[tokio::test]
async fn fc_reset_from_descent_needs_no_abort_and_touches_only_the_fc() {
    if isolated("fc_reset_from_descent_needs_no_abort_and_touches_only_the_fc") {
        return;
    }
    let f = Fixture::new().await;
    let state = f.state();
    *state.state.lock().unwrap() = crate::types::FlightState::InFlight;
    let model = fc_reset_model(&f);
    {
        let mut m = model.lock().unwrap();
        m.reset_any = true;
        m.state = crate::types::FlightState::Descent as u8;
    }
    let desired_before = crate::network_variables::flight_state();
    f.query(Board::FlightComputer).await;
    let status = fc_reset_status(&f).await;
    assert_eq!(status["flight_state"], 16, "{status}");
    assert_eq!(status["flight_state_label"], "In flight");
    let row = fc_reset_row(&status);
    assert_eq!(row["abort_latched"], false, "{status}");
    assert_eq!(row["capabilities"]["reset_startup_any_state"], true, "{status}");
    assert!(row["reset_startup_any_state_blocked_reason"].is_null(), "{status}");
    assert_eq!(row["state_label"], "Descent");
    assert_eq!(row["fc_phase"]["label"], "Descent", "{status}");
    assert_eq!(
        row["reset_fc_startup_warning"],
        "FC in Descent: after reset the FC will NOT deploy automatically; only manual Deploy/Expand."
    );
    let ticket = f.prepare("reset_fc_startup", Some("FC")).await;
    assert_eq!(ticket["confirmation"], "RESET FC TO STARTUP");
    assert_eq!(ticket["targets"], json!(["FC"]));
    assert!(ticket["summary"].as_str().unwrap().contains("deployment history is kept"), "{ticket}");
    assert!(ticket["summary"].as_str().unwrap().contains("never fires a pyro"), "{ticket}");
    assert!(ticket["reset_fc_startup_warning"].as_str().unwrap().starts_with("FC in Descent"), "{ticket}");
    assert_eq!(f.confirm(&ticket).await.status(), StatusCode::OK);
    let op = f.terminal().await;
    assert_eq!(op["success"], true, "{op}");
    for (board, peer) in &f.models {
        let m = peer.lock().unwrap();
        let writes: Vec<_> = m.requests.iter().filter(|p| p.payload()[1] != 0).collect();
        if *board == Board::FlightComputer {
            assert_eq!((m.state, m.epoch, m.applied), (0, 9, 1));
            assert!(!writes.is_empty());
            for request in writes {
                assert_eq!(&request.payload()[..4], &[1, 3, 0, 0], "operation 3 to Startup only");
            }
        } else {
            assert!(writes.is_empty(), "{board:?} must not be touched");
        }
    }
    assert_eq!(state.local_flight_state_snapshot(), crate::types::FlightState::InFlight,
        "an FC-only reset never changes the station's phase");
    assert_eq!(crate::network_variables::flight_state(), desired_before, "no FLIGHT_STATE broadcast");
    let after = fc_reset_status(&f).await;
    let row = fc_reset_row(&after);
    assert_eq!(row["state"], 0, "{after}");
    assert_eq!(row["fc_phase"]["label"], "Startup", "{after}");
}

/// A pulse is running on the FC: the station refuses before any write, and
/// says why.
#[tokio::test]
async fn fc_reset_refuses_while_a_pyro_pulse_runs() {
    if isolated("fc_reset_refuses_while_a_pyro_pulse_runs") {
        return;
    }
    let f = Fixture::new().await;
    let state = f.state();
    *state.state.lock().unwrap() = crate::types::FlightState::InFlight;
    let model = fc_reset_model(&f);
    {
        let mut m = model.lock().unwrap();
        m.reset_any = true;
        m.pulse_active = true;
        m.state = crate::types::FlightState::Descent as u8;
    }
    f.query(Board::FlightComputer).await;
    let row = fc_reset_row(&fc_reset_status(&f).await).clone();
    assert_eq!(row["capabilities"]["reset_startup_any_state"], false, "{row}");
    assert_eq!(row["reset_startup_any_state_blocked_reason"], "A deploy is in progress; wait for it to finish.");
    let (status, value) = fc_reset_prepare_raw(&f).await;
    assert_eq!(status, StatusCode::CONFLICT, "{value}");
    assert!(value.to_string().contains("A deploy is in progress"), "{value}");
    assert!(!model.lock().unwrap().requests.iter().any(|p| p.payload()[1] != 0), "no write reached the FC");
}

/// An operator Deploy pressed less than three seconds ago counts as in
/// progress: the station refuses the reset meanwhile.
#[tokio::test]
async fn fc_reset_refuses_right_after_an_operator_deploy() {
    if isolated("fc_reset_refuses_right_after_an_operator_deploy") {
        return;
    }
    let f = Fixture::new().await;
    let state = f.state();
    *state.state.lock().unwrap() = crate::types::FlightState::InFlight;
    fc_reset_model(&f).lock().unwrap().reset_any = true;
    f.query(Board::FlightComputer).await;
    let router = state.topology_router.get().unwrap().clone();
    crate::operator_command_resend::note_deploy_for_test(&router, 1, Duration::from_millis(100));
    let row = fc_reset_row(&fc_reset_status(&f).await).clone();
    assert_eq!(row["reset_startup_any_state_blocked_reason"], "A deploy is in progress; wait for it to finish.", "{row}");
    let (status, value) = fc_reset_prepare_raw(&f).await;
    assert_eq!(status, StatusCode::CONFLICT, "{value}");
    crate::operator_command_resend::note_deploy_for_test(&router, 1, crate::operator_command_resend::DEPLOY_SETTLE);
    let row = fc_reset_row(&fc_reset_status(&f).await).clone();
    assert!(row["reset_startup_any_state_blocked_reason"].is_null(), "{row}");
}

/// Old FC firmware does not advertise bit 7: no operation 3 is ever offered.
#[tokio::test]
async fn fc_reset_needs_firmware_that_advertises_it() {
    if isolated("fc_reset_needs_firmware_that_advertises_it") {
        return;
    }
    let f = Fixture::new().await;
    *f.state().state.lock().unwrap() = crate::types::FlightState::InFlight;
    f.query(Board::FlightComputer).await;
    let row = fc_reset_row(&fc_reset_status(&f).await).clone();
    assert_eq!(row["capabilities"]["reset_startup_any_state"], false);
    assert_eq!(row["reset_startup_any_state_blocked_reason"],
        "This flight computer firmware does not support reset from any state.");
    let (status, _) = fc_reset_prepare_raw(&f).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

/// Every capability a board row reports as false carries a short reason, and
/// an enabled one carries none.
#[tokio::test]
async fn every_disabled_recovery_capability_has_a_short_reason() {
    if isolated("every_disabled_recovery_capability_has_a_short_reason") {
        return;
    }
    let f = Fixture::new().await;
    *f.state().state.lock().unwrap() = crate::types::FlightState::NitrousFill;
    f.query(Board::FlightComputer).await;
    f.query(Board::ActuatorBoard).await;
    let status = fc_reset_status(&f).await;
    assert_eq!(status["flight_state"], 2, "{status}");
    assert_eq!(status["fill_phase"], "NitrousFill");
    for row in status["boards"].as_array().unwrap() {
        for key in ["query", "recover", "prepare_recover", "resync", "reset_startup_any_state"] {
            let enabled = row["capabilities"][key].as_bool().unwrap();
            let reason = &row["blocked_reasons"][key];
            if enabled {
                assert!(reason.is_null(), "{} {key}: {row}", row["id"]);
            } else {
                let text = reason.as_str().unwrap_or_else(|| panic!("{} {key} has no reason: {row}", row["id"]));
                assert!(!text.is_empty() && text.len() <= 200, "{} {key}: {text}", row["id"]);
            }
        }
    }
}

/// The allowed-state policy, one FC phase at a time. The default follows the
/// contract: allowed everywhere, warning while the FC flies.
#[test]
fn fc_reset_policy_for_every_fc_phase() {
    use crate::types::FlightState as F;
    for phase in [F::Startup, F::Armed, F::Landed, F::Recovery, F::Aborted] {
        assert_eq!(fc_reset_any_state_policy(Some(phase), false), FcResetPolicy::Allowed { warning: None }, "{phase:?}");
    }
    for phase in [F::Launch, F::Ascent, F::Coast, F::Apogee, F::Descent, F::Reefing, F::InFlight] {
        let label = crate::mission_phase::detailed_label(phase);
        assert_eq!(
            fc_reset_any_state_policy(Some(phase), false),
            FcResetPolicy::Allowed { warning: Some(format!(
                "FC in {label}: after reset the FC will NOT deploy automatically; only manual Deploy/Expand.")) },
            "{phase:?}"
        );
    }
    assert_eq!(fc_reset_any_state_policy(None, false), FcResetPolicy::Allowed { warning: None });
    assert!(!FC_RESET_REFUSE_BEFORE_AUTOMATIC_DEPLOY, "the contract default allows every phase");
}
