//! After an operator Abort the controller fence (gse::queue_controller) drops
//! every controller packet except the abort-safe directions. A HITL flight
//! command pressed then must be refused with that reason, not answered 200
//! and dropped at send time. Whether it should be allowed is a separate policy
//! decision; this only makes the existing refusal visible.
use super::*;
use axum::http::StatusCode;

fn operator() -> crate::auth::AuthPrincipal {
    use crate::auth::*;
    AuthPrincipal {
        roles: vec![StreamRole::Admin],
        username: Some("administrator".into()),
        permissions: Permissions {
            view_data: true,
            send_commands: true,
            set_system_time: false,
            voice_transmit: false,
            camera_publish: false,
            sign_checklists: false,
        },
        expires_at_ms: None,
        anonymous: false,
        session_type: Some("session".into()),
        command_access: CommandAccess::default(),
        calibration_access: CalibrationAccess::default(),
    }
}

async fn press(state: &Arc<AppState>, cmd: TelemetryCommand) -> (StatusCode, String) {
    crate::web::dispatch_authorized_command(state.clone(), operator(), cmd, crate::gse::automatic_generation(state), None, None).await
}

#[tokio::test]
async fn hitl_deploy_after_an_operator_abort_is_refused_with_its_reason() {
    let _mode = crate::runtime_mode::test_support::thread_mode(crate::station_mode::Mode::Hitl);
    let (tx, mut rx) = mpsc::channel(8);
    let mut state = crate::state::tests::test_app_state().await;
    Arc::get_mut(&mut state).unwrap().cmd_tx = tx;
    state.action_policy.lock().unwrap().software_buttons_enabled = true;
    state.set_abort_indicator_latched(true);
    for cmd in [TelemetryCommand::DeployParachute, TelemetryCommand::ExpandParachute] {
        let (status, body) = press(&state, cmd.clone()).await;
        assert_eq!(status, StatusCode::CONFLICT, "{cmd:?}: {body}");
        assert!(body.contains("Abort"), "{body}");
        assert!(rx.try_recv().is_err(), "{cmd:?} must not be queued only to be dropped");
        assert!(state.notifications_snapshot().iter().any(|n| n.message == body), "refusal is shown: {body}");
    }
    // No flight-computer command is abort-safe any more (the Evaluation
    // Abort that was is retired): a sensor re-init is refused the same way.
    let (status, body) = press(&state, TelemetryCommand::ReinitSensors).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("Abort"), "{body}");
    assert!(rx.try_recv().is_err());
    // Without a latched Abort a Deploy is admitted as before.
    state.set_abort_indicator_latched(false);
    let (status, body) = press(&state, TelemetryCommand::DeployParachute).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// A board recovery returns the Valve Board to Idle but leaves the station
/// Abort latched (only Reset all boards clears it), so the fence still drops a
/// Vent close or Pilot open. Such a press must be refused with that reason,
/// in the policy and at /api/command, not answered 200 and dropped (fullnet
/// vbrec ts2: Vent pressed after the VB recovery never reached the board).
#[tokio::test]
async fn fill_outputs_the_latched_abort_would_drop_are_refused_with_the_reason() {
    let _mode = crate::runtime_mode::test_support::thread_mode(crate::station_mode::Mode::Hitl);
    let (tx, mut rx) = mpsc::channel(8);
    let mut state = crate::state::tests::test_app_state().await;
    Arc::get_mut(&mut state).unwrap().cmd_tx = tx;
    state.action_policy.lock().unwrap().software_buttons_enabled = true;
    let vent = ValveBoardCommands::NormallyOpenOpen as u8;
    state.set_umbilical_valve_state(vent, true);
    state.set_umbilical_valve_state(ValveBoardCommands::PilotOpen as u8, false);
    state.set_abort_indicator_latched(true);
    let control = |state: &AppState, cmd: &str| {
        let mut policy = state.action_policy.lock().unwrap().clone();
        crate::board_abort::block_policy(state, &mut policy);
        let c = policy.controls.into_iter().find(|c| c.cmd == cmd).expect("control");
        (c.enabled, c.disabled_reason.unwrap_or_default())
    };
    // Vent open: a press closes it, which the latched Abort drops.
    for cmd in [TelemetryCommand::NormallyOpen, TelemetryCommand::Pilot] {
        let name = crate::sequences::command_name(&cmd).to_string();
        let (enabled, reason) = control(&state, &name);
        assert!(!enabled && reason.contains("Reset all boards"), "{name}: {reason}");
        let (status, body) = press(&state, cmd).await;
        assert_eq!(status, StatusCode::CONFLICT, "{name}: {body}");
        assert!(body.contains("station Abort is still latched"), "{body}");
        assert!(rx.try_recv().is_err(), "{name} must not be queued only to be dropped");
    }
    // The safe directions still pass the fence and stay admitted.
    let (status, body) = press(&state, TelemetryCommand::NitrogenClose).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(rx.try_recv().is_ok());
    state.set_umbilical_valve_state(vent, false);
    assert!(!control(&state, "NormallyOpen").1.contains("Reset all boards"));
    let (status, body) = press(&state, TelemetryCommand::NormallyOpen).await;
    assert_eq!(status, StatusCode::OK, "Vent open is abort-safe: {body}");
    assert!(rx.try_recv().is_ok());
    // Reset all boards clears the latch: a Vent close is admitted again.
    state.set_umbilical_valve_state(vent, true);
    state.set_abort_indicator_latched(false);
    assert!(!control(&state, "NormallyOpen").1.contains("Reset all boards"));
    let (status, body) = press(&state, TelemetryCommand::NormallyOpen).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[test]
fn fill_output_packet_picks_the_dispatch_direction() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let state = rt.block_on(crate::state::tests::test_app_state());
    let valve = crate::telemetry_schema::data_type("VALVE_COMMAND");
    let actuator = crate::telemetry_schema::data_type("ACTUATOR_COMMAND");
    assert_eq!(fill_output_packet(&state, "NormallyOpen"), Some((valve, ValveBoardCommands::NormallyOpenOpen as u8)));
    state.set_umbilical_valve_state(ValveBoardCommands::NormallyOpenOpen as u8, true);
    assert_eq!(fill_output_packet(&state, "NormallyOpen"), Some((valve, ValveBoardCommands::NormallyOpenClose as u8)));
    state.set_umbilical_valve_state(ActuatorBoardCommands::IgniterOn as u8, true);
    assert_eq!(fill_output_packet(&state, "Igniter"), Some((actuator, ActuatorBoardCommands::IgniterOff as u8)));
    assert_eq!(fill_output_packet(&state, "Nitrous"), Some((actuator, ActuatorBoardCommands::NitrousOpen as u8)));
    assert_eq!(fill_output_packet(&state, "Launch"), None);
}
