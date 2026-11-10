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
    // EvaluationAbort is abort-safe: the fence passes it, so it is still admitted.
    let (status, body) = press(&state, TelemetryCommand::EvaluationAbort).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(rx.try_recv().is_ok());
    // Without a latched Abort a Deploy is admitted as before.
    state.set_abort_indicator_latched(false);
    let (status, body) = press(&state, TelemetryCommand::DeployParachute).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
