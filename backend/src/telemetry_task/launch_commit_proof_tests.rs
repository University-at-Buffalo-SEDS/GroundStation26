//! A Launch that SDK admission would refuse is refused before its intent is
//! saved: nothing is published and Launch stays unlocked. Production Launch
//! handlers and an actual SDK router with warm ignition-pair peers.
use super::held_launch_tests::{Rig, immediate, rig_with};
use super::*;
use crate::board_abort::PROOF_TTL;
use std::time::Instant;

macro_rules! isolated {
    ($name:ident) => {
        if crate::operator_queue::isolated_native_case(module_path!(), stringify!($name)) {
            return;
        }
    };
}

async fn rig() -> Rig {
    let rig = rig_with(false, None).await;
    if crate::runtime_mode::is_hitl() {
        // HITL's board-state proof is opt-in, from boards on the bench.
        rig.state.toggle_hitl_safety_interlock(3);
        for board in ["AB", "VB"] {
            rig.state.mark_board_seen(board, get_current_timestamp_ms());
        }
    }
    rig
}

/// The boards' latest non-Abort proofs, as if their query was sent `age` ago.
pub(super) fn prove(state: &AppState, boards: &[Board], age: Duration) {
    for &board in boards {
        let now = Instant::now();
        state.board_abort.observe_status(
            board, 42, 8, 1, Some(1), false, true, now - age, state.board_abort.query_revision(board), now, 1,
        );
    }
}
pub(super) const PAIR: [Board; 2] = [Board::ActuatorBoard, Board::ValveBoard];

fn assert_unsent_and_unlocked(rig: &Rig) {
    let history = rig.state.launch_history.lock().unwrap();
    assert!(!history.launch_locked() && !history.automatic_actions_inhibited(), "nothing was saved");
    drop(history);
    assert_nothing_sent(rig);
}
fn assert_nothing_sent(rig: &Rig) {
    assert!(!rig.state.launch_indicator_latched());
    assert_ne!(rig.state.local_flight_state_snapshot(), FlightState::Launch);
    for name in ["FLIGHT_COMMAND", "VALVE_COMMAND"] {
        assert!(!rig.router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type(name)), "{name}");
    }
}
fn notified(state: &AppState, text: &str) -> bool {
    state.notifications_snapshot().iter().any(|n| n.message.contains(text))
}

/// The intent is saved between the last up-front check and SDK admission. A
/// proof that is fresh at the first and would be stale at the second refuses
/// the press; a later press with a fresh proof is an ordinary Launch.
#[tokio::test]
async fn a_proof_that_would_lapse_before_admission_refuses_the_press_and_leaves_launch_unlocked() {
    isolated!(a_proof_that_would_lapse_before_admission_refuses_the_press_and_leaves_launch_unlocked);
    let rig = rig().await;
    let published = crate::network_variables::last_flight_state_wire_for_test();
    prove(&rig.state, &PAIR, PROOF_TTL - Duration::from_secs(1));
    immediate(&rig).await;
    assert_unsent_and_unlocked(&rig);
    assert_eq!(crate::network_variables::last_flight_state_wire_for_test(), published, "no FLIGHT_STATE Launch");
    assert!(notified(&rig.state, crate::ignition_pair::PAIR_PROOF_REFUSAL), "{:?}", rig.state.notifications_snapshot());
    prove(&rig.state, &PAIR, Duration::ZERO);
    immediate(&rig).await;
    assert!(rig.state.launch_indicator_latched(), "{:?}", rig.state.notifications_snapshot());
    rig.state.request_shutdown();
}

/// Also once the press is past the handler's own check: the check under the
/// dispatch gate, immediately before the save, is the same one.
#[tokio::test]
async fn saving_the_intent_checks_the_pair_proof_itself() {
    isolated!(saving_the_intent_checks_the_pair_proof_itself);
    let rig = rig().await;
    prove(&rig.state, &PAIR, PROOF_TTL - Duration::from_secs(1));
    let error = save_intent(&rig.state).unwrap_err();
    assert!(error.contains("Actuator Board") && error.contains("Valve Board"), "{error}");
    assert_unsent_and_unlocked(&rig);
    prove(&rig.state, &PAIR, Duration::ZERO);
    assert!(save_intent(&rig.state).is_ok());
    rig.state.request_shutdown();
}

fn save_intent(state: &Arc<AppState>) -> Result<crate::gse::LaunchAdmission, String> {
    let revision = crate::board_abort::capture_launch_revision_named(state, "Launch");
    if crate::runtime_mode::is_hitl() {
        crate::gse::inhibit_automatic_actions_for_pressed_launch(state, revision, None)
    } else {
        crate::gse::commit_fire_intent_for_pressed_launch(state, revision, None)
    }
}
fn sequence_packet(router: &Router) -> Packet {
    build_launch_packet(router, "VALVE_COMMAND", "VALVE_BOARD", &[ValveBoardCommands::Sequence as u8]).unwrap()
}

/// A proof that does lapse during the save is still refused at SDK admission,
/// and the saved attempt keeps Launch locked. It publishes nothing.
#[tokio::test]
async fn a_proof_that_lapsed_during_the_save_is_refused_and_keeps_the_lock() {
    isolated!(a_proof_that_lapsed_during_the_save_is_refused_and_keeps_the_lock);
    let rig = rig().await;
    let ticket = save_intent(&rig.state).unwrap();
    let published = crate::network_variables::last_flight_state_wire_for_test();
    prove(&rig.state, &PAIR, PROOF_TTL + Duration::from_millis(1));
    let error = crate::gse::queue_admitted_launch_packets(&rig.state, &rig.router, ticket, vec![sequence_packet(&rig.router)])
        .unwrap_err();
    assert_eq!(error, crate::ignition_pair::PAIR_PROOF_REFUSAL);
    assert_nothing_sent(&rig);
    assert_eq!(crate::network_variables::last_flight_state_wire_for_test(), published, "no FLIGHT_STATE Launch");
    assert!(rig.state.launch_history.lock().unwrap().launch_locked(), "a saved attempt is never released here");
    rig.state.request_shutdown();
}

/// Another reader holding the board-state register for an instant is not a
/// missing proof: the saved Launch waits for it and is judged on the proof.
#[tokio::test]
async fn a_briefly_held_register_does_not_refuse_a_saved_launch() {
    isolated!(a_briefly_held_register_does_not_refuse_a_saved_launch);
    let rig = rig().await;
    let ticket = save_intent(&rig.state).unwrap();
    let held = rig.state.board_abort.hold_register_for_test();
    let (state, router) = (rig.state.clone(), rig.router.clone());
    let mode = crate::runtime_mode::current();
    let launch = std::thread::spawn(move || {
        let _mode = crate::runtime_mode::test_support::thread_mode(mode);
        crate::gse::queue_admitted_launch_packets(&state, &router, ticket, vec![sequence_packet(&router)])
    });
    std::thread::sleep(Duration::from_millis(100));
    drop(held);
    assert!(launch.join().unwrap().is_ok());
    rig.state.request_shutdown();
}

/// HITL's preflight leaves a pair board that is not on the bench out of the
/// proof, but SDK admission (proof interlock on) requires both. Such a Launch
/// is refused before it locks, never sent.
#[tokio::test]
async fn hitl_launch_without_a_pair_board_is_refused_before_it_locks() {
    isolated!(hitl_launch_without_a_pair_board_is_refused_before_it_locks);
    let _mode = crate::runtime_mode::test_support::thread_mode(crate::station_mode::Mode::Hitl);
    let state = crate::state::tests::test_app_state().await;
    state.set_local_flight_state(FlightState::Idle);
    state.toggle_hitl_safety_interlock(3);
    let router = Arc::new(Router::new(
        sedsnet::router::RouterConfig::new([crate::ignition_pair::fixture::handler(&state, |_| Ok(()))]).with_sender("GS"),
    ));
    state.topology_router.set(router.clone()).unwrap();
    state.mark_board_seen("VB", get_current_timestamp_ms());
    prove(&state, &[Board::ValveBoard], Duration::ZERO);
    assert!(crate::ignition_pair::launch_readiness(&state).is_ok(), "the bench rule admits the press");
    let revision = crate::board_abort::capture_launch_revision_named(&state, "Launch");
    handle_flight_computer_launch_command(state.clone(), router.clone(), revision).await;
    assert!(!state.launch_history.lock().unwrap().launch_locked());
    assert!(!state.launch_indicator_latched());
    assert!(!router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type("VALVE_COMMAND")));
    assert!(notified(&state, "Fresh actual state required for Actuator Board"), "{:?}", state.notifications_snapshot());
    state.request_shutdown();
}
