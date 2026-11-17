//! The QD control's retraction progress (presentation only).
use super::*;
use crate::sequences::{ControlProgress, ControlProgressState as S};
use std::time::Duration;

const KEY: u8 = crate::rocket_commands::ActuatorBoardCommands::RetractPlumbing as u8;

/// An AUX-drive motion status: `lifecycle` 1 active, 2 completed, 3 cancelled.
fn status(lifecycle: u8, remaining_ms: u32) -> motion_wire::MotionStatus {
    let mut b = vec![0u8; motion_wire::BYTES];
    b[0] = 2;
    b[2] = Mode::Auxiliary as u8;
    b[3] = 5;
    b[4..12].copy_from_slice(&9u64.to_le_bytes());
    b[12..20].copy_from_slice(&42u64.to_le_bytes());
    b[20..24].copy_from_slice(&8u32.to_le_bytes());
    b[24..32].copy_from_slice(&77u64.to_le_bytes());
    if lifecycle == 1 {
        b[32] = 1;
        b[33] = 1;
        b[36..40].copy_from_slice(&remaining_ms.to_le_bytes());
    }
    b[34] = lifecycle;
    b[35] = 1;
    b[40..48].copy_from_slice(&10_000u64.to_le_bytes());
    if lifecycle == 1 {
        b[48..52].copy_from_slice(&remaining_ms.to_le_bytes());
    }
    b[52..56].copy_from_slice(&8u32.to_le_bytes());
    motion_wire::MotionStatus::decode(&b).expect("valid motion status")
}

fn ago(ms: u64) -> Instant {
    Instant::now() - Duration::from_millis(ms)
}

/// A motion status received `received_ago_ms` ago for a query sent
/// `asked_ago_ms` ago.
fn observe(state: &AppState, value: motion_wire::MotionStatus, asked_ago_ms: u64, received_ago_ms: u64) {
    let generation = crate::gse::automatic_generation(state);
    let mut inner = state.plumbing_drive.inner.lock().unwrap();
    let mode_revision = inner.desired.revision;
    inner.motion_observed = Some(MotionObservation {
        value,
        after: Moment { wall: ago(asked_ago_ms), model: None },
        received: ago(received_ago_ms),
        generation,
        mode_revision,
    });
}

fn sent(state: &AppState, mode: Mode, ago_ms: u64) {
    let generation = crate::gse::automatic_generation(state);
    let mut inner = state.plumbing_drive.inner.lock().unwrap();
    let mode_revision = inner.desired.revision;
    inner.retract_sent = Some(RetractSent { at: ago(ago_ms), mode: mode as u8, mode_revision, generation });
}

fn progress(state: &AppState) -> Option<ControlProgress> {
    retract_progress(state)
}

#[tokio::test]
async fn nothing_shows_before_any_retraction() {
    let state = crate::state::tests::test_app_state().await;
    assert_eq!(progress(&state), None);
}

#[tokio::test]
async fn actuator_reported_aux_retraction_runs_with_its_remaining_time() {
    let state = crate::state::tests::test_app_state().await;
    sent(&state, Mode::Auxiliary, 3_000);
    observe(&state, status(1, 30_000), 2_100, 2_000);
    let p = progress(&state).unwrap();
    assert_eq!(p.state, S::Running);
    assert!(p.confirmed);
    let remaining = p.remaining_ms.unwrap();
    assert!((27_000..=28_000).contains(&remaining), "{remaining}");
}

#[tokio::test]
async fn sent_retraction_without_motion_status_shows_running_unconfirmed_with_countdown() {
    // The fire's build had no MotionV2 reply: the station still shows the
    // 45 s AUX move it sent as running, as an estimate.
    let state = crate::state::tests::test_app_state().await;
    sent(&state, Mode::Auxiliary, 10_000);
    let p = progress(&state).unwrap();
    assert_eq!(p.state, S::Running);
    assert!(!p.confirmed);
    let remaining = p.remaining_ms.unwrap();
    assert!((34_000..=35_000).contains(&remaining), "{remaining}");
    assert!(p.message.unwrap().contains("not confirmed"));
    // The estimate never feeds the actuator-reported hold or completion.
    assert!(!retract_motion_active(&state));
    assert!(!retract_motion_completed(&state));
}

#[tokio::test]
async fn stepper_retraction_estimate_has_no_countdown() {
    let state = crate::state::tests::test_app_state().await;
    sent(&state, Mode::Stepper, 1_000);
    let p = progress(&state).unwrap();
    assert_eq!((p.state, p.remaining_ms), (S::Running, None));
}

#[tokio::test]
async fn estimate_turns_unconfirmed_after_the_move_and_completed_on_a_retracted_report() {
    let state = crate::state::tests::test_app_state().await;
    sent(&state, Mode::Auxiliary, 46_000);
    let p = progress(&state).unwrap();
    assert_eq!(p.state, S::Unconfirmed);
    assert!(!p.confirmed);
    state.set_umbilical_valve_state(KEY, true);
    let p = progress(&state).unwrap();
    assert_eq!(p.state, S::Completed);
    assert!(p.confirmed);
}

#[tokio::test]
async fn completion_counts_only_when_asked_after_the_latest_send() {
    let state = crate::state::tests::test_app_state().await;
    sent(&state, Mode::Auxiliary, 5_000);
    // An earlier operation's completed status, asked before this send.
    observe(&state, status(2, 0), 6_000, 500);
    assert_eq!(progress(&state).unwrap().state, S::Running);
    assert!(!progress(&state).unwrap().confirmed);
    observe(&state, status(2, 0), 1_000, 500);
    let p = progress(&state).unwrap();
    assert_eq!((p.state, p.confirmed), (S::Completed, true));
}

#[tokio::test]
async fn actuator_stopped_retraction_shows_failed() {
    let state = crate::state::tests::test_app_state().await;
    sent(&state, Mode::Auxiliary, 5_000);
    observe(&state, status(3, 0), 1_000, 500);
    let p = progress(&state).unwrap();
    assert_eq!((p.state, p.confirmed), (S::Failed, true));
    assert!(p.message.unwrap().contains("stopped"));
}

#[tokio::test]
async fn a_retraction_that_was_never_sent_shows_failed_with_the_reason() {
    let state = crate::state::tests::test_app_state().await;
    state.plumbing_drive.inner.lock().unwrap().retract_failure =
        Some(RetractFailure { at: Instant::now(), message: "Retraction was not sent: deadline".into() });
    let p = progress(&state).unwrap();
    assert_eq!(p.state, S::Failed);
    assert!(p.message.unwrap().contains("not sent"));
}

#[tokio::test]
async fn a_pending_press_shows_starting() {
    let state = crate::state::tests::test_app_state().await;
    state.set_pending_umbilical_valve_state(KEY, true);
    assert_eq!(progress(&state).unwrap().state, S::Starting);
}

#[tokio::test]
async fn a_mode_change_drops_the_estimate() {
    let state = crate::state::tests::test_app_state().await;
    sent(&state, Mode::Auxiliary, 1_000);
    state.plumbing_drive.inner.lock().unwrap().desired.revision += 1;
    assert_eq!(progress(&state), None);
}

#[tokio::test]
async fn action_policy_carries_the_progress_on_the_qd_control_only() {
    let state = crate::state::tests::test_app_state().await;
    sent(&state, Mode::Auxiliary, 2_000);
    let policy = state.action_policy_snapshot();
    let json = serde_json::to_value(&policy).unwrap();
    let controls = json["controls"].as_array().unwrap();
    let qd = controls.iter().find(|c| c["cmd"] == "RetractPlumbing").expect("QD control");
    assert_eq!(qd["progress"]["state"], "running");
    assert_eq!(qd["progress"]["confirmed"], false);
    assert!(qd["progress"]["remaining_ms"].as_u64().is_some());
    assert!(controls.iter().filter(|c| c["cmd"] != "RetractPlumbing").all(|c| c.get("progress").is_none()));
}

#[tokio::test]
async fn publisher_repeats_while_running_and_once_when_it_ends() {
    let state = crate::state::tests::test_app_state().await;
    let mut rx = state.action_policy_tx.subscribe();
    let mut publisher = ProgressPublisher::default();
    assert!(!publisher.poll(&state));
    sent(&state, Mode::Auxiliary, 1_000);
    assert!(publisher.poll(&state));
    assert!(rx.try_recv().is_ok());
    assert!(!publisher.poll(&state), "at most once a second");
    state.plumbing_drive.inner.lock().unwrap().retract_sent = None;
    assert!(publisher.poll(&state), "the end is published once");
    assert!(!publisher.poll(&state));
}

#[tokio::test]
async fn an_unanswered_board_query_does_not_starve_the_motion_status_query() {
    let fixture = http_tests::Fixture::new().await;
    fixture.state.network_health.lock().unwrap().observe("AB", Instant::now(), 100);
    crate::board_recovery::hold_unanswered_query_for_test(&fixture.state, Board::ActuatorBoard);
    let mut refresh = crate::board_abort::ActuatorReadbackRefresh::default();
    assert!(refresh.poll(&fixture.state));
    let inner = fixture.state.plumbing_drive.inner.lock().unwrap();
    assert_eq!(inner.motion_queries.len(), 1, "motion status queried");
    assert_eq!(inner.requests.len(), 1, "drive selection queried");
}
