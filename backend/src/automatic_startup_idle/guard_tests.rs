// Fresh public-SDK guard controls; actual dequeued-event/roster coverage is
// rerun separately. These tests do not claim hardware or physical Launch.
use super::*;
async fn ready() -> (Arc<AppState>, Arc<sedsnet::router::Router>) {
    crate::automatic_flight_state::tests::ready().await
}
fn cached(router: &sedsnet::router::Router) -> Option<u64> {
    router
        .get_cached_network_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
        .unwrap()
        .map(|p| p.packet_id())
}
fn isolated(name: &str) -> bool {
    crate::operator_queue::isolated_case(module_path!(), name)
}
#[tokio::test]
async fn retained_launch_history_vetoes_original_tf_idle_before_visible_phase_changes() {
    if isolated("retained_launch_history_vetoes_original_tf_idle_before_visible_phase_changes") {
        return;
    }
    let (state, router) = ready().await;
    let original = Captured::capture(&state, true).unwrap();
    let owner = original.ticket.original_ownership();
    let before = crate::network_variables::automatic_idle_test_snapshot();
    let old = cached(&router);
    state
        .launch_history
        .lock()
        .unwrap()
        .inhibit_automatic_actions()
        .unwrap();
    assert_eq!(state.local_flight_state_snapshot(), FlightState::Startup);
    assert!(!state.launch_indicator_latched());
    publish(&state, original).await;
    assert_eq!(state.local_flight_state_snapshot(), FlightState::Startup);
    assert_eq!(cached(&router), old);
    assert_eq!(
        crate::network_variables::automatic_idle_test_snapshot(),
        before
    );
    assert_eq!(
        owner,
        (
            crate::gse::automatic_generation(&state),
            state.manual_abort_intent.epoch(),
            state.deferred_abort.manual_epoch()
        )
    );
    assert!(
        state
            .launch_history
            .lock()
            .unwrap()
            .automatic_actions_inhibited()
    );
}
#[tokio::test]
async fn actual_pending_launch_flag_vetoes_original_tf_idle_without_host_or_sdk_effects() {
    if isolated("actual_pending_launch_flag_vetoes_original_tf_idle_without_host_or_sdk_effects") {
        return;
    }
    let (state, router) = ready().await;
    let original = Captured::capture(&state, true).unwrap();
    let before = crate::network_variables::automatic_idle_test_snapshot();
    let old = cached(&router);
    assert!(state.try_begin_launch_sequence_command());
    publish(&state, original).await;
    assert_eq!(state.local_flight_state_snapshot(), FlightState::Startup);
    assert_eq!(cached(&router), old);
    assert_eq!(
        crate::network_variables::automatic_idle_test_snapshot(),
        before
    );
    assert!(
        state
            .launch_sequence_command_pending
            .load(std::sync::atomic::Ordering::Acquire)
    );
}
#[tokio::test]
async fn actual_known_optional_board_abort_retires_original_tf_sdk_publication() {
    if isolated("actual_known_optional_board_abort_retires_original_tf_sdk_publication") {
        return;
    }
    let (state, router) = ready().await;
    let original = Captured::capture(&state, true).unwrap();
    let before = crate::network_variables::automatic_idle_test_snapshot();
    let old = cached(&router);
    let packet = sedsnet::packet::Packet::new(
        crate::telemetry_schema::data_type("FLIGHT_STATE"),
        &[crate::telemetry_schema::endpoint("GROUND_STATION")],
        crate::types::Board::RFBoard.sender_id(),
        router.network_time_ms().unwrap(),
        Arc::<[u8]>::from([15]),
    )
    .unwrap();
    crate::board_abort::observe_raw(&state, &packet);
    assert_ne!(state.board_abort.masks().0, 0);
    assert!(Captured::capture(&state, true).is_none());
    publish(&state, original).await;
    assert_eq!(state.local_flight_state_snapshot(), FlightState::Startup);
    assert_eq!(cached(&router), old);
    assert_eq!(
        crate::network_variables::automatic_idle_test_snapshot(),
        before
    );
    assert_ne!(state.board_abort.masks().0, 0);
}
