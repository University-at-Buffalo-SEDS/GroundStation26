//! A Launch holds station mode changes from the moment it is admitted until
//! its countdown starts: through the command queue, the worker's send and the
//! wait for the Valve Board to start the T clock. Production command dispatch,
//! the production Launch handlers and an actual SDK router; no hardware.
use super::held_launch_tests::{immediate, rig_with};
use super::*;
use axum::http::StatusCode;

macro_rules! isolated {
    ($name:ident) => {
        if crate::operator_queue::isolated_native_case(module_path!(), stringify!($name)) {
            return;
        }
    };
}

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

fn mode_change_refusal(state: &AppState) -> Option<String> {
    crate::station_mode_switch::station_busy(state, false)
}

/// Whether the launch itself refuses a mode change. Outside HITL the armed
/// GSE sequence also refuses one; that reason is not under test here.
fn launch_holds_mode(state: &AppState) -> bool {
    mode_change_refusal(state).is_some_and(|reason| reason.contains("Launch") || reason.contains("countdown"))
}

/// What an operator's Launch press does before the worker sees it.
async fn press_launch(state: &Arc<AppState>) -> (StatusCode, String) {
    state.action_policy.lock().unwrap().software_buttons_enabled = true;
    let revision = crate::board_abort::capture_launch_revision_named(state, "Launch");
    crate::web::dispatch_authorized_command(
        state.clone(),
        operator(),
        TelemetryCommand::Launch,
        crate::gse::automatic_generation(state),
        None,
        revision,
    )
    .await
}

#[tokio::test]
async fn launch_holds_mode_changes_from_admission_until_the_countdown_starts() {
    isolated!(launch_holds_mode_changes_from_admission_until_the_countdown_starts);
    let (tx, mut rx) = mpsc::channel(4);
    let rig = rig_with(false, Some(tx)).await;
    let state = &rig.state;
    assert!(!launch_holds_mode(state), "{:?}", mode_change_refusal(state));

    let (status, body) = press_launch(state).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Launch answered 200 and sits in the command queue: no mode change now.
    let queued = rx.try_recv().expect("Launch is queued");
    assert!(launch_holds_mode(state), "a queued Launch holds mode changes: {:?}", mode_change_refusal(state));

    // The worker sends it. The Valve Board has not started its sequence yet.
    immediate(&rig).await;
    assert!(state.launch_indicator_latched(), "{:?}", state.notifications_snapshot());
    drop(queued);
    assert!(launch_holds_mode(state), "a sent Launch holds mode changes until the countdown: {:?}", mode_change_refusal(state));

    // The Valve Board starts the countdown: the countdown holds it from here.
    start_launch_clock_from_valve_sequence_status(state);
    assert_eq!(state.launch_clock_snapshot().kind, LaunchClockKind::TMinus);
    assert!(launch_holds_mode(state));

    // Past T-0 the launch no longer holds it.
    transition_launch_clock_to_t_plus_from_pilot_open(state);
    assert_eq!(state.launch_clock_snapshot().kind, LaunchClockKind::TPlus);
    assert!(!launch_holds_mode(state), "{:?}", mode_change_refusal(state));
    state.request_shutdown();
}

#[tokio::test]
async fn a_launch_that_never_reaches_the_worker_releases_the_hold() {
    isolated!(a_launch_that_never_reaches_the_worker_releases_the_hold);
    let (tx, mut rx) = mpsc::channel(4);
    let rig = rig_with(false, Some(tx)).await;
    let state = &rig.state;
    let (status, body) = press_launch(state).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(launch_holds_mode(state), "{:?}", mode_change_refusal(state));
    // A recovery drain discards queued commands without running them.
    drop(rx.try_recv().unwrap());
    assert!(!launch_holds_mode(state), "nothing was sent: {:?}", mode_change_refusal(state));
    state.request_shutdown();
}

#[tokio::test]
async fn a_reset_t_clock_releases_a_sent_launch_that_never_started_a_countdown() {
    isolated!(a_reset_t_clock_releases_a_sent_launch_that_never_started_a_countdown);
    let (tx, mut rx) = mpsc::channel(4);
    let rig = rig_with(false, Some(tx)).await;
    let state = &rig.state;
    let (status, body) = press_launch(state).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let queued = rx.try_recv().unwrap();
    immediate(&rig).await;
    drop(queued);
    assert!(launch_holds_mode(state), "{:?}", mode_change_refusal(state));
    state.try_reset_launch_clock().unwrap();
    assert!(!launch_holds_mode(state), "{:?}", mode_change_refusal(state));
    state.request_shutdown();
}
