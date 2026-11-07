//! Actual SDK Core contention. Normal-mode prerequisites use the maintained
//! armed fixture (modeled policy facts only); no hardware side is opened.
use super::*;
use std::sync::mpsc;
use std::time::Duration;
struct Fixture {
    state: Arc<AppState>,
    router: Arc<sedsnet::router::Router>,
    path: Option<std::path::PathBuf>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = std::fs::remove_file(path);
        }
    }
}
async fn fixture() -> Fixture {
    let state = crate::state::tests::test_app_state().await;
    let owner = Arc::as_ptr(&state) as usize;
    let router = Arc::new(sedsnet::router::Router::new_with_clock(
        sedsnet::router::RouterConfig::new([]).with_sender("GS"),
        Box::new(move || {
            nonwaiting_admission_tests::clock_pause(owner);
            100
        }),
    ));
    state.topology_router.set(router.clone()).unwrap();
    #[cfg(feature = "hitl_mode")]
    {
        state.set_local_flight_state(FlightState::Idle);
        for index in 0..4 {
            if state.hitl_safety_interlock_enabled(index) {
                state.toggle_hitl_safety_interlock(index);
            }
        }
    }
    #[cfg(feature = "hitl_mode")]
    let path = None;
    #[cfg(not(feature = "hitl_mode"))]
    let path = Some(armed_fixture_for_test(&state, &router));
    assert!(
        !state
            .launch_history
            .lock()
            .unwrap()
            .automatic_actions_inhibited()
    );
    Fixture {
        state,
        router,
        path,
    }
}
struct HeldCore {
    pause: nonwaiting_admission_tests::Pause,
    thread: Option<std::thread::JoinHandle<sedsnet::TelemetryResult<()>>>,
}
impl HeldCore {
    fn new(f: &Fixture) -> Self {
        let pause = nonwaiting_admission_tests::Pause::install(&f.state);
        let seed = sedsnet::packet::Packet::new(
            crate::telemetry_schema::data_type("AV_BAY_UNDERGLOW"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "GS",
            100,
            Arc::from([0u8]),
        )
        .unwrap();
        let router = f.router.clone();
        let thread = std::thread::Builder::new()
            .name("production-sdk-core-holder".into())
            .spawn(move || router.seed_managed_variable(seed))
            .unwrap();
        pause
            .core_entered
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert_eq!(
            f.router
                .try_has_pending_reliable_tx_type(crate::telemetry_schema::types::ABORT),
            Err(sedsnet::router::TryPendingTypeError::CoreBusy)
        );
        Self {
            pause,
            thread: Some(thread),
        }
    }
    fn release(&mut self) {
        self.pause.release_core();
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap().unwrap();
        }
    }
}
impl Drop for HeldCore {
    fn drop(&mut self) {
        self.release();
    }
}
fn prepare(state: &Arc<AppState>, revision: Option<u64>) -> Result<LaunchAdmission, String> {
    #[cfg(feature = "hitl_mode")]
    {
        inhibit_automatic_actions_for_launch_revision(state, revision)
    }
    #[cfg(not(feature = "hitl_mode"))]
    {
        commit_fire_intent_revision(state, revision)
    }
}
fn start(
    f: &Fixture,
) -> (
    std::thread::JoinHandle<()>,
    mpsc::Receiver<Result<LaunchAdmission, String>>,
    LaunchAdmission,
) {
    let revision = crate::board_abort::capture_launch_revision_named(&f.state, "Launch");
    let original = LaunchAdmission::capture_for_launch(&f.state, revision).unwrap();
    let state = f.state.clone();
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        let _ = tx.send(prepare(&state, revision));
    });
    (thread, rx, original)
}
#[tokio::test]
async fn held_sdk_core_returns_without_journal_mutation_or_dispatch_lock_retention() {
    if crate::operator_queue::isolated_native_case(
        module_path!(),
        "held_sdk_core_returns_without_journal_mutation_or_dispatch_lock_retention",
    ) {
        return;
    }
    let f = fixture().await;
    let mut held = HeldCore::new(&f);
    let before = f.state.launch_history.lock().unwrap().snapshot();
    let (thread, rx, _) = start(&f);
    held.pause
        .busy
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let result = rx
        .recv_timeout(Duration::from_millis(400))
        .expect("Core Busy cannot block preparation behind SDK owner");
    assert!(result.unwrap_err().contains("SDK Core"));
    thread.join().unwrap();
    assert!(
        DISPATCH_GATE.try_lock().is_ok(),
        "DISPATCH released even while actual SDK core remains held"
    );
    assert_eq!(
        f.router
            .try_has_pending_reliable_tx_type(crate::telemetry_schema::types::ABORT),
        Err(sedsnet::router::TryPendingTypeError::CoreBusy)
    );
    assert_eq!(f.state.launch_history.lock().unwrap().snapshot(), before);
    assert!(
        !f.state
            .launch_history
            .lock()
            .unwrap()
            .automatic_actions_inhibited()
    );
    held.release();
}
#[tokio::test]
async fn released_sdk_core_prepares_same_original_ticket_once_before_inhibition() {
    if crate::operator_queue::isolated_native_case(
        module_path!(),
        "released_sdk_core_prepares_same_original_ticket_once_before_inhibition",
    ) {
        return;
    }
    let f = fixture().await;
    let mut held = HeldCore::new(&f);
    let (thread, rx, original) = start(&f);
    held.pause
        .busy
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert!(
        !f.state
            .launch_history
            .lock()
            .unwrap()
            .automatic_actions_inhibited()
    );
    held.release();
    let accepted = rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
    thread.join().unwrap();
    assert_eq!(accepted.original_ownership(), original.original_ownership());
    assert_eq!(accepted.board_abort_revision, original.board_abort_revision);
    assert!(
        f.state
            .launch_history
            .lock()
            .unwrap()
            .automatic_actions_inhibited()
    );
}
#[tokio::test]
async fn genuine_pending_sdk_controller_owner_refuses_without_durable_inhibition() {
    if crate::operator_queue::isolated_native_case(
        module_path!(),
        "genuine_pending_sdk_controller_owner_refuses_without_durable_inhibition",
    ) {
        return;
    }
    let f = fixture().await;
    let kind = crate::telemetry_schema::data_type("VALVE_COMMAND");
    let packet = sedsnet::packet::Packet::new(
        kind,
        &[crate::telemetry_schema::endpoint("VALVE_BOARD")],
        "GS",
        100,
        Arc::from([ValveBoardCommands::PilotOpen as u8]),
    )
    .unwrap();
    f.router.tx_queue(packet).unwrap();
    assert_eq!(f.router.try_has_pending_reliable_tx_type(kind), Ok(true));
    let revision = crate::board_abort::capture_launch_revision_named(&f.state, "Launch");
    // HITL: a command retained for an absent board must not hold Launch.
    #[cfg(feature = "hitl_mode")]
    assert!(prepare(&f.state, revision).is_ok());
    #[cfg(not(feature = "hitl_mode"))]
    {
        assert!(prepare(&f.state, revision).unwrap_err().contains("pending"));
        assert!(
            !f.state
                .launch_history
                .lock()
                .unwrap()
                .automatic_actions_inhibited()
        );
    }
    assert_eq!(f.router.try_has_pending_reliable_tx_type(kind), Ok(true));
}
#[tokio::test]
async fn newer_stop_during_busy_retry_refuses_original_before_journal_mutation() {
    if crate::operator_queue::isolated_native_case(
        module_path!(),
        "newer_stop_during_busy_retry_refuses_original_before_journal_mutation",
    ) {
        return;
    }
    let f = fixture().await;
    let mut held = HeldCore::new(&f);
    let (thread, rx, _) = start(&f);
    held.pause
        .busy
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    f.state.manual_abort_intent.request_untimed();
    let result = rx.recv_timeout(Duration::from_millis(400)).unwrap();
    thread.join().unwrap();
    assert!(result.is_err());
    assert!(
        !f.state
            .launch_history
            .lock()
            .unwrap()
            .automatic_actions_inhibited()
    );
    assert!(f.state.manual_abort_intent.pending_epoch().is_some());
    held.release();
}
